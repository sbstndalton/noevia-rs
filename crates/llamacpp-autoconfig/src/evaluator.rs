//! Stateless, Rust-directed evaluator for the historical ECMAScript estimator.
//!
//! The program is part of the Rust artifact. The host only performs individual ECMAScript
//! primitives over opaque references. Raw strings, objects, Maps and iterators never cross
//! the wire, so neither input length nor a processed array prefix enlarges a continuation.
//! Reentrant callbacks start independent invocations; there is no module-global session.
use super::{Out, Refusal, R};
use prompt_framing::json::Value;

#[derive(Clone, Copy)]
enum Instruction {
    Host(usize, &'static str),
    Env,
    UseEnv,
    Pop,
    Dup,
    Swap,
    Jump(usize),
    If(usize, &'static str),
    Unless(usize, &'static str),
    Catch(usize),
    Uncatch,
    BooleanPeriod(usize),
    KvSum(usize),
    Return,
}
include!("evaluator_program.rs");

// These bound the generated program's registers, not the size of any caller value.
const STACK_REGISTERS: usize = 256;
const CATCH_REGISTERS: usize = 8;
const BATCH_OPERATIONS: usize = 32;
const VIRTUAL_BASE: u64 = 8_000_000_000_000_000;

const BOOLEAN_CHUNK: usize = 4096;
const BOOLEAN_CACHE: usize = 2;
#[derive(Clone)]
struct Chunk {
    base: usize,
    nibbles: Vec<u8>,
}
struct Period {
    seq: u64,
    return_pc: usize,
    len: usize,
    p: usize,
    i: usize,
    need: usize,
    chunks: Vec<Chunk>,
}
impl Period {
    fn read(v: &Value) -> R<Self> {
        let Some(Value::Arr(chunks)) = v.get("chunks") else {
            return Err(Refusal::Input);
        };
        if chunks.len() > BOOLEAN_CACHE {
            return Err(Refusal::Input);
        }
        let chunks = chunks
            .iter()
            .map(|c| {
                let base = index(c.get("base"))?;
                let Some(text) = c.get("hex").and_then(Value::as_str) else {
                    return Err(Refusal::Input);
                };
                Ok(Chunk {
                    base,
                    nibbles: hex(text)?,
                })
            })
            .collect::<R<Vec<_>>>()?;
        let period = Self {
            seq: integer(v.get("seq"))?,
            return_pc: index(v.get("returnPc"))?,
            len: index(v.get("len"))?,
            p: index(v.get("p"))?,
            i: index(v.get("i"))?,
            need: index(v.get("need"))?,
            chunks,
        };
        if period.p == 0
            || period.len > u32::MAX as usize
            || period.p > period.len.max(1)
            || period.i > period.len
            || period.need > period.len
            || period.return_pc >= PROGRAM.len()
            || period
                .chunks
                .iter()
                .any(|c| c.base > period.len || c.base % BOOLEAN_CHUNK != 0)
        {
            return Err(Refusal::Input);
        }
        Ok(period)
    }
    fn write(&self, out: &mut Out) {
        out.raw("{")
            .key("seq")
            .num(self.seq as f64)
            .raw(",")
            .key("returnPc")
            .num(self.return_pc as f64)
            .raw(",")
            .key("len")
            .num(self.len as f64)
            .raw(",")
            .key("p")
            .num(self.p as f64)
            .raw(",")
            .key("i")
            .num(self.i as f64)
            .raw(",")
            .key("need")
            .num(self.need as f64)
            .raw(",")
            .key("chunks")
            .raw("[");
        for (i, chunk) in self.chunks.iter().enumerate() {
            if i != 0 {
                out.raw(",");
            }
            let text: Vec<u16> = chunk
                .nibbles
                .iter()
                .map(|n| {
                    u16::from(
                        b"0123456789abcdef"
                            .get(usize::from(*n))
                            .copied()
                            .unwrap_or(b'0'),
                    )
                })
                .collect();
            out.raw("{")
                .key("base")
                .num(chunk.base as f64)
                .raw(",")
                .key("hex")
                .str16(&text)
                .raw("}");
        }
        out.raw("]}");
    }
    fn bit(&mut self, position: usize) -> Option<bool> {
        let hit = self
            .chunks
            .iter()
            .position(|c| position >= c.base && (position - c.base) / 4 < c.nibbles.len())?;
        let chunk = self.chunks.get(hit)?;
        let offset = position - chunk.base;
        let bit = chunk.nibbles.get(offset / 4).copied()? & (1 << (offset % 4)) != 0;
        if hit == 0 && self.chunks.len() == BOOLEAN_CACHE {
            self.chunks.swap(0, 1);
        }
        Some(bit)
    }
    // The original p/i/equality algorithm. This local fixed buffer only removes
    // chunk lookup overhead when the two bounded cache windows cover the value.
    fn covered_period(&self) -> R<Option<usize>> {
        if self.len > BOOLEAN_CHUNK * BOOLEAN_CACHE {
            return Ok(None);
        }
        let mut packed = [0_u8; BOOLEAN_CHUNK * BOOLEAN_CACHE / 8 + 8];
        for base in (0..self.len).step_by(BOOLEAN_CHUNK) {
            let Some(chunk) = self.chunks.iter().find(|c| c.base == base) else {
                return Ok(None);
            };
            let count = self.len.saturating_sub(base).min(BOOLEAN_CHUNK).div_ceil(4);
            if chunk.nibbles.len() != count {
                return Err(Refusal::Input);
            }
            for (offset, nibble) in chunk.nibbles.iter().enumerate() {
                let position = base / 4 + offset;
                let destination = packed.get_mut(position / 2).ok_or(Refusal::Input)?;
                *destination |= nibble << ((position % 2) * 4);
            }
        }
        let mut p = self.p.max(1);
        let mut i = self.i;
        while p <= self.len {
            while i < self.len {
                let j = i % p;
                // The original every test over a proven immutable primitive array:
                // compare consecutive booleans together, stopping at modulo wrap.
                // No application callback/getter is skipped on the observable path.
                let width = (self.len - i).min(p - j).min(32);
                let word = |position: usize| -> R<u64> {
                    let start = position / 8;
                    let bytes: [u8; 8] = packed
                        .get(start..start + 8)
                        .ok_or(Refusal::Input)?
                        .try_into()
                        .map_err(|_| Refusal::Input)?;
                    Ok(u64::from_le_bytes(bytes) >> (position % 8))
                };
                let mask = (1_u64 << width) - 1;
                if (word(i)? ^ word(j)?) & mask != 0 {
                    break;
                }
                i += width;
            }
            if i == self.len {
                return Ok(Some(p));
            }
            p += 1;
            i = 0;
        }
        Ok(Some(self.len))
    }
}
fn hex(text: &[u16]) -> R<Vec<u8>> {
    if text.len() > BOOLEAN_CHUNK / 4 {
        return Err(Refusal::Input);
    }
    text.iter()
        .map(|c| match c {
            0x30..=0x39 => Ok((*c - 0x30) as u8),
            0x61..=0x66 => Ok((*c - 0x61 + 10) as u8),
            _ => Err(Refusal::Input),
        })
        .collect()
}

struct Sum {
    cache: Period,
    raw: Vec<u64>,
    numbers: [f64; 6],
    total: f64,
}
fn read_bits(v: Option<&Value>) -> R<f64> {
    let Some(text) = v.and_then(Value::as_str) else {
        return Err(Refusal::Input);
    };
    if text.len() != 16 {
        return Err(Refusal::Input);
    }
    let mut bits = 0_u64;
    for &c in text {
        let digit = match c {
            0x30..=0x39 => c - 0x30,
            0x61..=0x66 => c - 0x61 + 10,
            _ => return Err(Refusal::Input),
        };
        bits = (bits << 4) | u64::from(digit);
    }
    Ok(f64::from_bits(bits))
}
impl Sum {
    fn read(v: &Value) -> R<Self> {
        let cache = Period::read(v.get("cache").ok_or(Refusal::Input)?)?;
        let raw = refs(v.get("raw"))?;
        if !raw.is_empty() && raw.len() != 8 {
            return Err(Refusal::Input);
        }
        let Some(Value::Arr(numbers)) = v.get("numbers") else {
            return Err(Refusal::Input);
        };
        if numbers.len() != 6 {
            return Err(Refusal::Input);
        }
        let mut values = [0.0; 6];
        for (destination, source) in values.iter_mut().zip(numbers) {
            *destination = read_bits(Some(source))?;
        }
        Ok(Self {
            cache,
            raw,
            numbers: values,
            total: read_bits(v.get("total"))?,
        })
    }
    fn write(&self, out: &mut Out) {
        out.raw("{").key("cache");
        self.cache.write(out);
        out.raw(",").key("raw");
        write_refs(out, &self.raw);
        out.raw(",").key("numbers").raw("[");
        for (i, n) in self.numbers.iter().enumerate() {
            if i != 0 {
                out.raw(",");
            }
            out.str(&format!("{:016x}", n.to_bits()));
        }
        out.raw("],")
            .key("total")
            .str(&format!("{:016x}", self.total.to_bits()))
            .raw("}");
    }
}

struct State {
    pc: usize,
    env: u64,
    stack: Vec<u64>,
    catches: Vec<(usize, usize, u64)>,
    mode: u8,
    target: usize,
    period: Option<Period>,
    sum: Option<Sum>,
}
fn integer(v: Option<&Value>) -> R<u64> {
    let Some(Value::Num(n)) = v else {
        return Err(Refusal::Input);
    };
    if !n.is_finite() || *n < 0.0 || n.fract() != 0.0 || *n > 9_007_199_254_740_991.0 {
        return Err(Refusal::Input);
    }
    Ok(*n as u64)
}
fn index(v: Option<&Value>) -> R<usize> {
    usize::try_from(integer(v)?).map_err(|_| Refusal::Input)
}
fn refs(v: Option<&Value>) -> R<Vec<u64>> {
    let Some(Value::Arr(xs)) = v else {
        return Err(Refusal::Input);
    };
    if xs.len() > STACK_REGISTERS {
        return Err(Refusal::Input);
    }
    xs.iter().map(|x| integer(Some(x))).collect()
}
impl State {
    fn read(v: &Value) -> R<Self> {
        let Some(Value::Arr(catches)) = v.get("catches") else {
            return Err(Refusal::Input);
        };
        if catches.len() > CATCH_REGISTERS {
            return Err(Refusal::Input);
        }
        let catches = catches
            .iter()
            .map(|c| {
                let Some(Value::Arr(pair)) = Some(c) else {
                    return Err(Refusal::Input);
                };
                if pair.len() != 3 {
                    return Err(Refusal::Input);
                }
                Ok((
                    index(pair.first())?,
                    index(pair.get(1))?,
                    integer(pair.get(2))?,
                ))
            })
            .collect::<R<Vec<_>>>()?;
        let state = Self {
            pc: index(v.get("pc"))?,
            env: integer(v.get("env"))?,
            stack: refs(v.get("stack"))?,
            catches,
            mode: u8::try_from(integer(v.get("mode"))?).map_err(|_| Refusal::Input)?,
            target: index(v.get("target"))?,
            sum: match v.get("sum") {
                None | Some(Value::Null) => None,
                Some(v) => Some(Sum::read(v)?),
            },
            period: match v.get("period") {
                None | Some(Value::Null) => None,
                Some(v) => Some(Period::read(v)?),
            },
        };
        if state.pc >= PROGRAM.len() || state.mode > 7 {
            return Err(Refusal::Input);
        }
        Ok(state)
    }
    fn write(&self, out: &mut Out) {
        out.raw("{")
            .key("pc")
            .num(self.pc as f64)
            .raw(",")
            .key("env")
            .num(self.env as f64)
            .raw(",")
            .key("stack");
        write_refs(out, &self.stack);
        out.raw(",").key("catches").raw("[");
        for (i, (pc, depth, env)) in self.catches.iter().enumerate() {
            if i != 0 {
                out.raw(",");
            }
            out.raw("[")
                .num(*pc as f64)
                .raw(",")
                .num(*depth as f64)
                .raw(",")
                .num(*env as f64)
                .raw("]");
        }
        out.raw("],")
            .key("mode")
            .num(f64::from(self.mode))
            .raw(",")
            .key("target")
            .num(self.target as f64)
            .raw(",")
            .key("period");
        if let Some(period) = &self.period {
            period.write(out);
        } else {
            out.raw("null");
        }
        out.raw(",").key("sum");
        if let Some(sum) = &self.sum {
            sum.write(out);
        } else {
            out.raw("null");
        }
        out.raw("}");
    }
    fn pop(&mut self) -> R<u64> {
        self.stack.pop().ok_or(Refusal::Input)
    }
    fn push(&mut self, value: u64) -> R<()> {
        if self.stack.len() >= STACK_REGISTERS {
            return Err(Refusal::Input);
        }
        self.stack.push(value);
        Ok(())
    }
}
fn write_refs(out: &mut Out, refs: &[u64]) {
    out.raw("[");
    for (i, value) in refs.iter().enumerate() {
        if i != 0 {
            out.raw(",");
        }
        out.num(*value as f64);
    }
    out.raw("]");
}
fn request(state: &State, operation: &str, args: &[u64], result: u64) -> String {
    let mut out = Out(Vec::new());
    out.raw("{").key("state");
    state.write(&mut out);
    out.raw(",")
        .key("request")
        .raw(operation)
        .raw(",")
        .key("args");
    write_refs(&mut out, args);
    out.raw(",").key("ref").num(result as f64).raw("}");
    out.text()
}
fn batch(requests: &[String]) -> String {
    format!(r#"{{"batch":[{}]}}"#, requests.join(","))
}
fn end(key: &str, reference: u64) -> String {
    let mut out = Out(Vec::new());
    out.raw("{").key(key).num(reference as f64).raw("}");
    out.text()
}

pub(super) fn call(op: u8, args: &Value) -> R<String> {
    let mut state = if op == 11 {
        let function = index(args.get("function"))?;
        let pc = *ENTRIES.get(function).ok_or(Refusal::Input)?;
        State {
            pc,
            env: integer(args.get("env"))?,
            stack: Vec::new(),
            catches: Vec::new(),
            mode: 0,
            target: 0,
            period: None,
            sum: None,
        }
    } else {
        let mut state = State::read(args.get("state").ok_or(Refusal::Input)?)?;
        let frame = args.get("frame").ok_or(Refusal::Input)?;
        if let Some(thrown) = frame.get("thrown") {
            let thrown = integer(Some(thrown))?;
            if let Some((pc, depth, env)) = state.catches.pop() {
                state.pc = pc;
                state.env = env;
                state.stack.truncate(depth);
                state.push(thrown)?;
            } else {
                return Ok(end("throw", thrown));
            }
        } else if state.mode == 5 {
            let Some(Value::Arr(facts)) = frame.get("numbers") else {
                return Err(Refusal::Input);
            };
            if facts.len() != 8 {
                return Err(Refusal::Input);
            }
            let is_null = matches!(facts.first().and_then(|v| v.get("kind")).and_then(Value::as_str),Some(s) if s.iter().copied().eq("null".encode_utf16()));
            let all_numbers = facts.iter().skip(1).all(|v| matches!(v.get("kind").and_then(Value::as_str),Some(s) if s.iter().copied().eq("number".encode_utf16())));
            if !is_null || !all_numbers {
                state.sum = None;
                state.mode = 0;
            } else {
                let length = read_bits(facts.get(1).and_then(|v| v.get("bits")))?;
                if !length.is_finite()
                    || length < 0.0
                    || length.fract() != 0.0
                    || length > f64::from(u32::MAX)
                {
                    state.sum = None;
                    state.mode = 0;
                } else {
                    let sum = state.sum.as_mut().ok_or(Refusal::Input)?;
                    sum.cache.len = length as usize;
                    sum.raw.clear();
                    for (destination, fact) in sum.numbers.iter_mut().zip(facts.iter().skip(2)) {
                        *destination = read_bits(fact.get("bits"))?;
                    }
                    state.mode = 6;
                    let seq = sum.cache.seq;
                    return Ok(batch(&[request(
                        &state,
                        r#"{"op":"booleanArrayFacts","data":null}"#,
                        &[seq],
                        0,
                    )]));
                }
            }
        } else if state.mode == 6 {
            let facts = frame.get("facts").ok_or(Refusal::Input)?;
            let safe = matches!(facts.get("safe"), Some(Value::Bool(true)));
            if safe
                && index(facts.get("len"))? >= state.sum.as_ref().ok_or(Refusal::Input)?.cache.len
            {
                state.mode = 7;
            } else {
                state.sum = None;
                state.mode = 0;
            }
        } else if state.mode == 7 {
            let sum = state.sum.as_mut().ok_or(Refusal::Input)?;
            let text = frame
                .get("bits")
                .and_then(Value::as_str)
                .ok_or(Refusal::Input)?;
            let nibbles = hex(text)?;
            let count = sum
                .cache
                .len
                .saturating_sub(sum.cache.need)
                .min(BOOLEAN_CHUNK);
            if nibbles.len() != count.div_ceil(4) {
                return Err(Refusal::Input);
            }
            if sum.cache.chunks.len() == BOOLEAN_CACHE {
                sum.cache.chunks.remove(0);
            }
            sum.cache.chunks.push(Chunk {
                base: sum.cache.need,
                nibbles,
            });
        } else if state.mode == 3 {
            let facts = frame.get("facts").ok_or(Refusal::Input)?;
            let Some(Value::Bool(safe)) = facts.get("safe") else {
                return Err(Refusal::Input);
            };
            if *safe {
                let period = state.period.as_mut().ok_or(Refusal::Input)?;
                period.len = index(facts.get("len"))?;
                if period.len > u32::MAX as usize {
                    return Err(Refusal::Input);
                }
                period.p = 1;
                state.mode = 4;
            } else {
                state.period = None;
                state.mode = 0;
            }
        } else if state.mode == 4 {
            let period = state.period.as_mut().ok_or(Refusal::Input)?;
            let text = frame
                .get("bits")
                .and_then(Value::as_str)
                .ok_or(Refusal::Input)?;
            let nibbles = hex(text)?;
            let count = period.len.saturating_sub(period.need).min(BOOLEAN_CHUNK);
            if nibbles.len() != count.div_ceil(4) {
                return Err(Refusal::Input);
            }
            if period.chunks.len() == BOOLEAN_CACHE {
                period.chunks.remove(0);
            }
            period.chunks.push(Chunk {
                base: period.need,
                nibbles,
            });
        } else if state.mode == 0 {
            state.push(integer(frame.get("value"))?)?;
        } else {
            let Some(Value::Bool(test)) = frame.get("truth") else {
                return Err(Refusal::Input);
            };
            if *test == (state.mode == 1) {
                state.pc = state.target;
            }
        }
        state
    };
    let mut requests = Vec::new();
    loop {
        if state.mode == 7 && state.sum.is_some() {
            let sum = state.sum.as_mut().ok_or(Refusal::Input)?;
            let mut missing = None;
            while sum.cache.i < sum.cache.len {
                let Some(local) = sum.cache.bit(sum.cache.i) else {
                    missing = Some(sum.cache.i);
                    break;
                };
                let a = *sum.numbers.first().ok_or(Refusal::Input)?;
                let h = *sum.numbers.get(1).ok_or(Refusal::Input)?;
                let element = *sum
                    .numbers
                    .get(if local { 2 } else { 4 })
                    .ok_or(Refusal::Input)?;
                let context = *sum
                    .numbers
                    .get(if local { 3 } else { 5 })
                    .ok_or(Refusal::Input)?;
                let first = a * h;
                let second = first * element;
                let term = second * context;
                sum.total += term;
                sum.cache.i += 1;
            }
            if let Some(position) = missing {
                sum.cache.need = position / BOOLEAN_CHUNK * BOOLEAN_CHUNK;
                let count = sum
                    .cache
                    .len
                    .saturating_sub(sum.cache.need)
                    .min(BOOLEAN_CHUNK);
                let operation = format!(
                    r#"{{"op":"booleanChunk","data":{{"start":{},"count":{count}}}}}"#,
                    sum.cache.need
                );
                let seq = sum.cache.seq;
                return Ok(batch(&[request(&state, &operation, &[seq], 0)]));
            }
            let answer = sum.total;
            state.pc = sum.cache.return_pc;
            state.sum = None;
            state.mode = 0;
            let operation = format!(r#"{{"op":"floatBits","data":"{:016x}"}}"#, answer.to_bits());
            return Ok(batch(&[request(&state, &operation, &[], VIRTUAL_BASE + 1)]));
        }
        if state.mode == 4 && state.period.is_some() {
            let period = state.period.as_mut().ok_or(Refusal::Input)?;
            let mut answer = period.covered_period()?;
            let mut missing = None;
            while answer.is_none() {
                if period.p > period.len {
                    answer = Some(period.len);
                    break;
                }
                if period.i >= period.len {
                    answer = Some(period.p);
                    break;
                }
                let Some(a) = period.bit(period.i) else {
                    missing = Some(period.i);
                    break;
                };
                let Some(b) = period.bit(period.i % period.p) else {
                    missing = Some(period.i % period.p);
                    break;
                };
                if a == b {
                    period.i += 1;
                } else {
                    period.p += 1;
                    period.i = 0;
                }
            }
            if let Some(answer) = answer {
                state.pc = period.return_pc;
                state.period = None;
                state.mode = 0;
                let operation = format!(r#"{{"op":"literal","data":{answer}}}"#);
                return Ok(batch(&[request(&state, &operation, &[], VIRTUAL_BASE + 1)]));
            }
            let position = missing.ok_or(Refusal::Input)?;
            period.need = (position / BOOLEAN_CHUNK) * BOOLEAN_CHUNK;
            let count = period.len.saturating_sub(period.need).min(BOOLEAN_CHUNK);
            let operation = format!(
                r#"{{"op":"booleanChunk","data":{{"start":{},"count":{count}}}}}"#,
                period.need
            );
            let seq = period.seq;
            return Ok(batch(&[request(&state, &operation, &[seq], 0)]));
        }
        let instruction = *PROGRAM.get(state.pc).ok_or(Refusal::Input)?;
        state.pc += 1;
        match instruction {
            Instruction::Host(count, operation) => {
                if state.stack.len() < count {
                    return Err(Refusal::Input);
                }
                let arguments = state.stack.split_off(state.stack.len() - count);
                state.mode = 0;
                let result = VIRTUAL_BASE + requests.len() as u64 + 1;
                requests.push(request(&state, operation, &arguments, result));
                state.push(result)?;
                if requests.len() == BATCH_OPERATIONS {
                    return Ok(batch(&requests));
                }
            }
            Instruction::Env => {
                state.push(state.env)?;
            }
            Instruction::UseEnv => {
                state.env = state.pop()?;
            }
            Instruction::Pop => {
                state.pop()?;
            }
            Instruction::Dup => {
                let value = *state.stack.last().ok_or(Refusal::Input)?;
                state.push(value)?;
            }
            Instruction::Swap => {
                let b = state.pop()?;
                let a = state.pop()?;
                state.push(b)?;
                state.push(a)?;
            }
            Instruction::Jump(pc) => {
                state.pc = pc;
            }
            Instruction::If(pc, kind) | Instruction::Unless(pc, kind) => {
                let reference = state.pop()?;
                state.mode = if matches!(instruction, Instruction::If(..)) {
                    1
                } else {
                    2
                };
                state.target = pc;
                let operation = format!(r#"{{"op":"test","data":"{kind}"}}"#);
                requests.push(request(&state, &operation, &[reference], 0));
                return Ok(batch(&requests));
            }
            Instruction::Catch(pc) => {
                if state.catches.len() >= CATCH_REGISTERS {
                    return Err(Refusal::Input);
                }
                state.catches.push((pc, state.stack.len(), state.env));
            }
            Instruction::Uncatch => {
                state.catches.pop().ok_or(Refusal::Input)?;
            }
            Instruction::KvSum(return_pc) => {
                if !requests.is_empty() {
                    return Ok(batch(&requests));
                }
                if state.stack.len() < 9 {
                    return Err(Refusal::Input);
                }
                let mut arguments = state.stack.split_off(state.stack.len() - 9);
                let seq = arguments.remove(0);
                state.sum = Some(Sum {
                    cache: Period {
                        seq,
                        return_pc,
                        len: 0,
                        p: 1,
                        i: 0,
                        need: 0,
                        chunks: Vec::new(),
                    },
                    raw: arguments.clone(),
                    numbers: [0.0; 6],
                    total: 0.0,
                });
                state.mode = 5;
                return Ok(batch(&[request(
                    &state,
                    r#"{"op":"numberFacts","data":null}"#,
                    &arguments,
                    0,
                )]));
            }
            Instruction::BooleanPeriod(return_pc) => {
                if !requests.is_empty() {
                    return Ok(batch(&requests));
                }
                let seq = state.pop()?;
                state.period = Some(Period {
                    seq,
                    return_pc,
                    len: 0,
                    p: 1,
                    i: 0,
                    need: 0,
                    chunks: Vec::new(),
                });
                state.mode = 3;
                return Ok(batch(&[request(
                    &state,
                    r#"{"op":"booleanArrayFacts","data":null}"#,
                    &[seq],
                    0,
                )]));
            }
            Instruction::Return => {
                if !requests.is_empty() {
                    return Ok(batch(&requests));
                }
                return Ok(end("done", state.pop()?));
            }
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    #[test]
    fn generated_program_has_fixed_bounded_register_depth() {
        for &entry in ENTRIES {
            let mut work = vec![(entry, 0_usize)];
            let mut visited = HashMap::new();
            while let Some((pc, depth)) = work.pop() {
                if let Some(previous) = visited.insert(pc, depth) {
                    assert_eq!(previous, depth, "inconsistent stack at {pc}");
                    continue;
                }
                assert!(depth < STACK_REGISTERS, "register overflow at {pc}");
                let next = pc + 1;
                match PROGRAM[pc] {
                    Instruction::Host(count, _) => {
                        assert!(depth >= count, "host argument underflow at {pc}");
                        work.push((next, depth - count + 1));
                    }
                    Instruction::Env | Instruction::Dup => work.push((next, depth + 1)),
                    Instruction::Pop | Instruction::UseEnv => {
                        assert!(depth > 0, "register underflow at {pc}");
                        work.push((next, depth - 1));
                    }
                    Instruction::Swap => {
                        assert!(depth >= 2);
                        work.push((next, depth));
                    }
                    Instruction::Jump(target) => work.push((target, depth)),
                    Instruction::If(target, _) | Instruction::Unless(target, _) => {
                        assert!(depth > 0);
                        work.push((next, depth - 1));
                        work.push((target, depth - 1));
                    }
                    Instruction::Catch(target) => {
                        work.push((next, depth));
                        work.push((target, depth + 1));
                    }
                    Instruction::Uncatch => work.push((next, depth)),
                    Instruction::KvSum(target) => {
                        assert_eq!(depth, 9);
                        work.push((next, 0));
                        work.push((target, 1));
                    }
                    Instruction::BooleanPeriod(target) => {
                        assert_eq!(depth, 1);
                        work.push((next, 0));
                        work.push((target, 1));
                    }
                    Instruction::Return => assert_eq!(depth, 1, "return register at {pc}"),
                }
            }
        }
    }

    #[test]
    fn malformed_continuations_are_refused_without_sessions() {
        let parse = |s: &str| prompt_framing::json::parse_utf8(s.as_bytes(), 10).unwrap();
        for text in [
            "{}",
            r#"{"function":99999,"env":1}"#,
            r#"{"function":0,"env":-1}"#,
            r#"{"state":{"pc":99999,"env":1,"stack":[],"catches":[],"mode":0,"target":0},"frame":{"value":2}}"#,
        ] {
            assert!(call(if text.contains("state") { 12 } else { 11 }, &parse(text)).is_err());
        }
        let start = call(11, &parse(r#"{"function":0,"env":1}"#)).unwrap();
        assert!(start.contains(r#""request":{"op":"literal","data":"use strict"}"#));
    }
}
