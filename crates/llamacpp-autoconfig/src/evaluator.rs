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

// Only evaluator control integers use this writer. Accepted refs/indices are
// nonnegative exact f64 integers <= 2^53-1; generated refs are VIRTUAL_BASE+1..32.
// Their ECMAScript JSON spelling is exactly unsigned decimal (all are < 1e21).
// Business numbers retain Out::num or their existing raw-bit representation.
impl Out {
    fn private_uint(&mut self, mut value: u64) -> &mut Self {
        let mut digits = [0_u8; 20];
        let mut start = digits.len();
        for digit in digits.iter_mut().rev() {
            *digit = b'0' + (value % 10) as u8;
            start -= 1;
            value /= 10;
            if value == 0 {
                break;
            }
        }
        self.0.extend(digits.into_iter().skip(start));
        self
    }
}

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
            .raw("\"seq\":")
            .private_uint(self.seq)
            .raw(",")
            .raw("\"returnPc\":")
            .private_uint(self.return_pc as u64)
            .raw(",")
            .raw("\"len\":")
            .private_uint(self.len as u64)
            .raw(",")
            .raw("\"p\":")
            .private_uint(self.p as u64)
            .raw(",")
            .raw("\"i\":")
            .private_uint(self.i as u64)
            .raw(",")
            .raw("\"need\":")
            .private_uint(self.need as u64)
            .raw(",")
            .raw("\"chunks\":")
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
                .raw("\"base\":")
                .private_uint(chunk.base as u64)
                .raw(",")
                .raw("\"hex\":")
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
        out.raw("{").raw("\"cache\":");
        self.cache.write(out);
        out.raw(",").raw("\"raw\":");
        write_refs(out, &self.raw);
        out.raw(",").raw("\"numbers\":").raw("[");
        for (i, n) in self.numbers.iter().enumerate() {
            if i != 0 {
                out.raw(",");
            }
            out.str(&format!("{:016x}", n.to_bits()));
        }
        out.raw("],")
            .raw("\"total\":")
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
        Self::read_fields([
            v.get("pc"),
            v.get("env"),
            v.get("stack"),
            v.get("catches"),
            v.get("mode"),
            v.get("target"),
            v.get("period"),
            v.get("sum"),
        ])
    }
    fn read_wire(v: &Value, compact: bool) -> R<Self> {
        if !compact {
            return Self::read(v);
        }
        let Value::Arr(values) = v else {
            return Err(Refusal::Input);
        };
        let [pc, env, stack, catches, mode, target, period, sum] = values.as_slice() else {
            return Err(Refusal::Input);
        };
        Self::read_fields([
            Some(pc),
            Some(env),
            Some(stack),
            Some(catches),
            Some(mode),
            Some(target),
            Some(period),
            Some(sum),
        ])
    }
    fn read_fields(fields: [Option<&Value>; 8]) -> R<Self> {
        let [pc, env, stack, catches, mode, target, period, sum] = fields;
        let Some(Value::Arr(catches)) = catches else {
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
            pc: index(pc)?,
            env: integer(env)?,
            stack: refs(stack)?,
            catches,
            mode: u8::try_from(integer(mode)?).map_err(|_| Refusal::Input)?,
            target: index(target)?,
            sum: match sum {
                None | Some(Value::Null) => None,
                Some(v) => Some(Sum::read(v)?),
            },
            period: match period {
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
            .raw("\"pc\":")
            .private_uint(self.pc as u64)
            .raw(",")
            .raw("\"env\":")
            .private_uint(self.env)
            .raw(",")
            .raw("\"stack\":");
        write_refs(out, &self.stack);
        out.raw(",").raw("\"catches\":").raw("[");
        for (i, (pc, depth, env)) in self.catches.iter().enumerate() {
            if i != 0 {
                out.raw(",");
            }
            out.raw("[")
                .private_uint(*pc as u64)
                .raw(",")
                .private_uint(*depth as u64)
                .raw(",")
                .private_uint(*env)
                .raw("]");
        }
        out.raw("],")
            .raw("\"mode\":")
            .private_uint(u64::from(self.mode))
            .raw(",")
            .raw("\"target\":")
            .private_uint(self.target as u64)
            .raw(",")
            .raw("\"period\":");
        if let Some(period) = &self.period {
            period.write(out);
        } else {
            out.raw("null");
        }
        out.raw(",").raw("\"sum\":");
        if let Some(sum) = &self.sum {
            sum.write(out);
        } else {
            out.raw("null");
        }
        out.raw("}");
    }
    fn write_wire(&self, out: &mut Out, compact: bool) {
        if !compact {
            self.write(out);
            return;
        }
        out.raw("[")
            .private_uint(self.pc as u64)
            .raw(",")
            .private_uint(self.env)
            .raw(",");
        write_refs(out, &self.stack);
        out.raw(",[");
        for (index, (pc, depth, env)) in self.catches.iter().enumerate() {
            if index != 0 {
                out.raw(",");
            }
            out.raw("[")
                .private_uint(*pc as u64)
                .raw(",")
                .private_uint(*depth as u64)
                .raw(",")
                .private_uint(*env)
                .raw("]");
        }
        out.raw("],")
            .private_uint(u64::from(self.mode))
            .raw(",")
            .private_uint(self.target as u64)
            .raw(",");
        if let Some(period) = &self.period {
            period.write(out);
        } else {
            out.raw("null");
        }
        out.raw(",");
        if let Some(sum) = &self.sum {
            sum.write(out);
        } else {
            out.raw("null");
        }
        out.raw("]");
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
        out.private_uint(*value);
    }
    out.raw("]");
}
fn request(state: &State, operation: &str, args: &[u64], result: u64, compact: bool) -> String {
    let mut out = Out(Vec::new());
    out.raw("{").raw("\"state\":");
    state.write_wire(&mut out, compact);
    out.raw(",")
        .raw("\"request\":")
        .raw(operation)
        .raw(",")
        .raw("\"args\":");
    write_refs(&mut out, args);
    out.raw(",").raw("\"ref\":").private_uint(result).raw("}");
    out.text()
}
fn batch(requests: &[String], compact: bool) -> String {
    if compact {
        format!(r#"{{"wire":"state-v1","batch":[{}]}}"#, requests.join(","))
    } else {
        format!(r#"{{"batch":[{}]}}"#, requests.join(","))
    }
}
fn end(key: &str, reference: u64, compact: bool) -> String {
    let mut out = Out(Vec::new());
    out.raw("{");
    if compact {
        out.raw(r#""wire":"state-v1","#);
    }
    out.key(key).private_uint(reference).raw("}");
    out.text()
}

pub(super) fn call(op: u8, args: &Value) -> R<String> {
    let compact = matches!(args.get("wire").and_then(Value::as_str),
        Some(tag) if tag.iter().copied().eq("state-v1".encode_utf16()));
    let request = |state: &State, operation: &str, args: &[u64], result: u64| {
        request(state, operation, args, result, compact)
    };
    let batch = |requests: &[String]| batch(requests, compact);
    let end = |key: &str, reference: u64| end(key, reference, compact);
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
        let mut state = State::read_wire(args.get("state").ok_or(Refusal::Input)?, compact)?;
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
                    let count = sum.cache.len;
                    let operation =
                        format!(r#"{{"op":"ownBooleanPrefixFacts","data":{{"count":{count}}}}}"#);
                    return Ok(batch(&[request(&state, &operation, &[seq], 0)]));
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

    fn assert_private_uint_bytes(value: u64) {
        assert!(value <= 9_007_199_254_740_991);
        let mut direct = Out(Vec::new());
        direct.private_uint(value);
        let mut legacy = Out(Vec::new());
        legacy.num(value as f64);
        assert_eq!(direct.0, legacy.0, "private integer {value}");
    }

    #[test]
    fn private_integer_decimal_bytes_match_legacy_boundaries_and_seeded_values() {
        const MAX_SAFE: u64 = 9_007_199_254_740_991;
        for value in [0, 1, u64::from(u32::MAX), MAX_SAFE - 1, MAX_SAFE] {
            assert_private_uint_bytes(value);
        }
        for power in 0..=53 {
            let center = 1_u64 << power;
            for value in [center - 1, center, center + 1] {
                if value <= MAX_SAFE {
                    assert_private_uint_bytes(value);
                }
            }
        }
        for power in 0..=15 {
            let center = 10_u64.pow(power);
            for value in [center - 1, center, center + 1] {
                assert_private_uint_bytes(value);
            }
        }
        for value in VIRTUAL_BASE - 32..=VIRTUAL_BASE + BATCH_OPERATIONS as u64 {
            assert_private_uint_bytes(value);
        }
        let mut seed = 0x1071_ee12_5384_aa99_u64;
        for _ in 0..16_384 {
            seed = seed.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1);
            assert_private_uint_bytes(seed >> 11);
        }
    }

    // Re-emit complete private frames through the unchanged legacy number
    // formatter, retaining the parser's original object order and scalar bits.
    fn legacy_frame_bytes(text: &str) -> Vec<u8> {
        fn write(out: &mut Out, value: &Value) {
            match value {
                Value::Null => {
                    out.raw("null");
                }
                Value::Bool(value) => {
                    out.bool(*value);
                }
                Value::Num(value) => {
                    out.num(*value);
                }
                Value::Str(value) => {
                    out.str16(value);
                }
                Value::Arr(values) => {
                    out.raw("[");
                    for (index, value) in values.iter().enumerate() {
                        if index != 0 {
                            out.raw(",");
                        }
                        write(out, value);
                    }
                    out.raw("]");
                }
                Value::Obj(values) => {
                    out.raw("{");
                    for (index, (key, value)) in values.iter().enumerate() {
                        if index != 0 {
                            out.raw(",");
                        }
                        out.str16(key).raw(":");
                        write(out, value);
                    }
                    out.raw("}");
                }
                Value::Deep => panic!("bounded test frame"),
            }
        }
        let parsed = prompt_framing::json::parse_utf8(text.as_bytes(), 32).unwrap();
        let mut legacy = Out(Vec::new());
        write(&mut legacy, &parsed);
        legacy.0
    }

    #[test]
    fn private_state_requests_and_terminal_frames_keep_legacy_bytes() {
        const MAX_SAFE: u64 = 9_007_199_254_740_991;
        let period = || Period {
            seq: MAX_SAFE,
            return_pc: PROGRAM.len() - 1,
            len: u32::MAX as usize,
            p: 4096,
            i: 8192,
            need: 4096,
            chunks: vec![Chunk {
                base: 4096,
                nibbles: vec![0, 15, 1, 10],
            }],
        };
        let mut state = State {
            pc: PROGRAM.len() - 1,
            env: MAX_SAFE,
            stack: vec![0, 1, MAX_SAFE, VIRTUAL_BASE + BATCH_OPERATIONS as u64],
            catches: vec![(PROGRAM.len() - 1, STACK_REGISTERS, MAX_SAFE); CATCH_REGISTERS],
            mode: 7,
            target: PROGRAM.len() - 1,
            period: Some(period()),
            sum: Some(Sum {
                cache: period(),
                raw: vec![MAX_SAFE; 8],
                numbers: [
                    0.0,
                    -0.0,
                    f64::NAN,
                    f64::INFINITY,
                    f64::NEG_INFINITY,
                    f64::from_bits(1),
                ],
                total: f64::from_bits(0x7ff8_0000_0000_0001),
            }),
        };
        for specialized in [true, false] {
            if !specialized {
                state.period = None;
                state.sum = None;
                state.mode = 0;
            }
            let text = request(
                &state,
                r#"{"op":"get","data":null}"#,
                &[0, MAX_SAFE, VIRTUAL_BASE + 1],
                MAX_SAFE,
                false,
            );
            assert_eq!(text.as_bytes(), legacy_frame_bytes(&text));
            let mut out = Out(Vec::new());
            state.write(&mut out);
            let text = out.text();
            assert_eq!(text.as_bytes(), legacy_frame_bytes(&text));
        }
        for key in ["done", "throw"] {
            let text = end(key, MAX_SAFE, false);
            assert_eq!(text.as_bytes(), legacy_frame_bytes(&text));
        }
        // A complete batch captured directly from the generated program, rather
        // than a mirrored expected-state constructor.
        let input =
            prompt_framing::json::parse_utf8(br#"{"function":0,"env":9007199254740991}"#, 8)
                .unwrap();
        let text = call(11, &input).unwrap();
        assert_eq!(text.as_bytes(), legacy_frame_bytes(&text));
    }

    #[test]
    fn private_integer_input_domain_remains_exact_and_safe() {
        for value in [-1.0, 0.5, 9_007_199_254_740_992.0, f64::NAN, f64::INFINITY] {
            assert!(integer(Some(&Value::Num(value))).is_err());
        }
        assert_eq!(integer(Some(&Value::Num(-0.0))).unwrap(), 0);
        assert_eq!(
            integer(Some(&Value::Num(9_007_199_254_740_991.0))).unwrap(),
            9_007_199_254_740_991
        );
    }

    fn transport_parse(text: &str) -> Value {
        prompt_framing::json::parse_utf8(text.as_bytes(), 32).unwrap()
    }
    fn transport_args(state: Value, frame: Value, compact: bool) -> Value {
        let mut fields = vec![
            ("state".encode_utf16().collect(), state),
            ("frame".encode_utf16().collect(), frame),
        ];
        if compact {
            fields.push((
                "wire".encode_utf16().collect(),
                Value::Str("state-v1".encode_utf16().collect()),
            ));
        }
        Value::Obj(fields)
    }

    #[test]
    fn state_v1_initial_batches_preserve_every_default_operation_snapshot() {
        let plain = call(11, &transport_parse(r#"{"function":0,"env":1}"#)).unwrap();
        let tagged = call(
            11,
            &transport_parse(r#"{"function":0,"env":1,"wire":"state-v1"}"#),
        )
        .unwrap();
        let plain = transport_parse(&plain);
        let tagged = transport_parse(&tagged);
        assert!(plain.get("wire").is_none());
        assert_eq!(
            tagged.get("wire").unwrap().as_str().unwrap(),
            "state-v1".encode_utf16().collect::<Vec<_>>()
        );
        let Value::Arr(a) = plain.get("batch").unwrap() else {
            panic!("batch")
        };
        let Value::Arr(b) = tagged.get("batch").unwrap() else {
            panic!("batch")
        };
        assert_eq!(a.len(), b.len());
        for (plain, tagged) in a.iter().zip(b) {
            for key in ["request", "args", "ref"] {
                assert_eq!(plain.get(key), tagged.get(key), "{key}");
            }
            let mut old = Out(Vec::new());
            State::read(plain.get("state").unwrap())
                .unwrap()
                .write(&mut old);
            let mut expanded = Out(Vec::new());
            State::read_wire(tagged.get("state").unwrap(), true)
                .unwrap()
                .write(&mut expanded);
            assert_eq!(old.0, expanded.0);
        }
    }

    #[test]
    fn state_v1_round_trips_all_modes_nested_registers_and_terminal_paths() {
        let period = || Period {
            seq: 9_007_199_254_740_991,
            return_pc: 1,
            len: 4096,
            p: 1,
            i: 0,
            need: 0,
            chunks: vec![Chunk {
                base: 0,
                nibbles: vec![0, 15],
            }],
        };
        for mode in 0..=7 {
            let state = State {
                pc: 1,
                env: 9_007_199_254_740_991,
                stack: vec![1, VIRTUAL_BASE + 32],
                catches: vec![(1, 1, 9_007_199_254_740_991); CATCH_REGISTERS],
                mode,
                target: 1,
                period: Some(period()),
                sum: Some(Sum {
                    cache: period(),
                    raw: vec![1; 8],
                    numbers: [
                        -0.0,
                        f64::NAN,
                        f64::INFINITY,
                        f64::NEG_INFINITY,
                        f64::from_bits(1),
                        1.0,
                    ],
                    total: f64::from_bits(0x7ff8_0000_0000_0001),
                }),
            };
            let mut object = Out(Vec::new());
            state.write(&mut object);
            let mut tuple = Out(Vec::new());
            state.write_wire(&mut tuple, true);
            let decoded = State::read_wire(&transport_parse(&tuple.text()), true).unwrap();
            let mut again = Out(Vec::new());
            decoded.write(&mut again);
            assert_eq!(object.0, again.0);
        }

        let return_pc = PROGRAM
            .iter()
            .position(|instruction| matches!(instruction, Instruction::Return))
            .unwrap();
        for thrown in [false, true] {
            let state = State {
                pc: return_pc,
                env: 1,
                stack: Vec::new(),
                catches: Vec::new(),
                mode: 0,
                target: 0,
                period: None,
                sum: None,
            };
            let mut object = Out(Vec::new());
            state.write(&mut object);
            let mut tuple = Out(Vec::new());
            state.write_wire(&mut tuple, true);
            let frame = transport_parse(if thrown {
                r#"{"thrown":42}"#
            } else {
                r#"{"value":42}"#
            });
            let plain = call(
                12,
                &transport_args(transport_parse(&object.text()), frame.clone(), false),
            )
            .unwrap();
            let tagged = call(
                12,
                &transport_args(transport_parse(&tuple.text()), frame, true),
            )
            .unwrap();
            let plain = transport_parse(&plain);
            let tagged = transport_parse(&tagged);
            let key = if thrown { "throw" } else { "done" };
            assert_eq!(plain.get(key), tagged.get(key));
            assert!(plain.get("wire").is_none());
            assert!(tagged.get("wire").is_some());
        }
    }

    #[test]
    fn state_v1_rejects_wrong_tuple_arity_and_object_state_without_legacy_retry() {
        let object = transport_parse(
            r#"{"pc":0,"env":1,"stack":[],"catches":[],"mode":0,"target":0,"period":null,"sum":null}"#,
        );
        let frame = transport_parse(r#"{"value":1}"#);
        assert!(call(12, &transport_args(object, frame.clone(), true)).is_err());
        for tuple in [
            "[0,1,[],[],0,0,null]",
            "[0,1,[],[],0,0,null,null,null]",
            "[0,1,[],[],8,0,null,null]",
            "[0,-1,[],[],0,0,null,null]",
        ] {
            assert!(call(
                12,
                &transport_args(transport_parse(tuple), frame.clone(), true)
            )
            .is_err());
        }
        // Unrecognized tags do not alter default raw Rust operation bytes.
        let plain = call(11, &transport_parse(r#"{"function":0,"env":1}"#)).unwrap();
        let unknown = call(
            11,
            &transport_parse(r#"{"function":0,"env":1,"wire":"state-v0"}"#),
        )
        .unwrap();
        assert_eq!(plain, unknown);
    }

    #[test]
    fn own_prefix_request_follows_existing_scalar_head_and_numeric_gates() {
        let return_pc = PROGRAM
            .iter()
            .position(|instruction| matches!(instruction, Instruction::Return))
            .unwrap();
        let frame = |head_null: bool, period: f64, nonnumber: bool| {
            let number = |value: f64| {
                transport_parse(&format!(
                    r#"{{"kind":"number","bits":"{:016x}"}}"#,
                    value.to_bits()
                ))
            };
            let mut facts = vec![
                if head_null {
                    transport_parse(r#"{"kind":"null"}"#)
                } else {
                    number(1.0)
                },
                number(period),
            ];
            facts.extend([1.0, 8.0, 256.0, 64.0, 256.0, 4096.0].map(number));
            if nonnumber {
                facts[4] = transport_parse(r#"{"kind":"other"}"#);
            }
            Value::Obj(vec![(
                "numbers".encode_utf16().collect(),
                Value::Arr(facts),
            )])
        };
        let run = |frame: Value| {
            let state = State {
                pc: return_pc,
                env: 1,
                stack: vec![99],
                catches: Vec::new(),
                mode: 5,
                target: 0,
                period: None,
                sum: Some(Sum {
                    cache: Period {
                        seq: 9,
                        return_pc,
                        len: 0,
                        p: 1,
                        i: 0,
                        need: 0,
                        chunks: Vec::new(),
                    },
                    raw: vec![1; 8],
                    numbers: [0.0; 6],
                    total: 0.0,
                }),
            };
            let mut out = Out(Vec::new());
            state.write(&mut out);
            call(
                12,
                &transport_args(transport_parse(&out.text()), frame, false),
            )
            .unwrap()
        };
        for count in [0.0, 3.0, 80.0, f64::from(u32::MAX)] {
            let reply = transport_parse(&run(frame(true, count, false)));
            let Value::Arr(batch) = reply.get("batch").unwrap() else {
                panic!("batch")
            };
            let request = batch[0].get("request").unwrap();
            assert_eq!(
                request.get("op").unwrap().as_str().unwrap(),
                "ownBooleanPrefixFacts".encode_utf16().collect::<Vec<_>>()
            );
            assert_eq!(
                request.get("data").unwrap().get("count"),
                Some(&Value::Num(count))
            );
        }
        for facts in [
            frame(false, 3.0, false),
            frame(true, 3.0, true),
            frame(true, 0.5, false),
            frame(true, f64::NAN, false),
            frame(true, f64::INFINITY, false),
        ] {
            assert_eq!(run(facts), r#"{"done":99}"#);
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
