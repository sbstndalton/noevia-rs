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
const PRIVATE_DECIMAL_PAIRS: &[u8; 200] = b"00010203040506070809101112131415161718192021222324252627282930313233343536373839404142434445464748495051525354555657585960616263646566676869707172737475767778798081828384858687888990919293949596979899";

impl Out {
    fn private_uint(&mut self, mut value: u64) -> &mut Self {
        // Batch-local virtual references have the same sixteen decimal digits on
        // every call. This is only a spelling shortcut; all other u64 values keep
        // the general writer below, and no output reference is renumbered.
        if let Some(alias) = value
            .checked_sub(VIRTUAL_BASE)
            .filter(|n| (1..=32).contains(n))
        {
            let pair = (alias * 2) as usize;
            if let Some(bytes) = PRIVATE_DECIMAL_PAIRS.get(pair..pair + 2) {
                self.0.extend_from_slice(b"80000000000000");
                self.0.extend_from_slice(bytes);
                return self;
            }
        }
        const CHUNK: u64 = 100_000_000;
        let mut digits = [0_u8; 20];
        let start = {
            let mut cursor = digits.iter_mut().rev();
            // A u64 has at most twenty digits: at most two eight-digit chunks,
            // followed by an unpadded leading chunk. With two lower chunks the
            // remaining value is at most 1844 (four digits); otherwise it is <10^8.
            while value >= CHUNK {
                let quotient = value / CHUNK;
                // quotient * CHUNK <= value; the remainder is below 10^8.
                let mut chunk = (value - quotient * CHUNK) as u32;
                value = quotient;
                for _ in 0..4 {
                    let quotient = chunk / 100;
                    let pair = ((chunk - quotient * 100) * 2) as usize;
                    // Every pair is 0..=198, so both table bytes always exist.
                    if let Some(bytes) = PRIVATE_DECIMAL_PAIRS.get(pair..pair + 2) {
                        for (output, byte) in cursor.by_ref().take(2).zip(bytes.iter().rev()) {
                            *output = *byte;
                        }
                    }
                    chunk = quotient;
                }
            }
            let mut leading = value as u32;
            while leading >= 100 {
                let quotient = leading / 100;
                let pair = ((leading - quotient * 100) * 2) as usize;
                if let Some(bytes) = PRIVATE_DECIMAL_PAIRS.get(pair..pair + 2) {
                    for (output, byte) in cursor.by_ref().take(2).zip(bytes.iter().rev()) {
                        *output = *byte;
                    }
                }
                leading = quotient;
            }
            if leading < 10 {
                // The total decimal length is <=20, so this slot always exists.
                if let Some(output) = cursor.next() {
                    *output = b'0' + leading as u8;
                }
            } else {
                let pair = leading as usize * 2;
                if let Some(bytes) = PRIVATE_DECIMAL_PAIRS.get(pair..pair + 2) {
                    for (output, byte) in cursor.by_ref().take(2).zip(bytes.iter().rev()) {
                        *output = *byte;
                    }
                }
            }
            cursor.len()
        };
        // start is the unconsumed prefix length, necessarily within 0..=20.
        self.0
            .extend_from_slice(digits.get(start..).unwrap_or_default());
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
        state.checked()
    }
    fn checked(self) -> R<Self> {
        if self.pc >= PROGRAM.len() || self.mode > 7 {
            return Err(Refusal::Input);
        }
        Ok(self)
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
// One call owns one lazily-started output buffer. Each append serializes the
// complete snapshot immediately; later VM mutations cannot change queued bytes.
fn write_request(
    out: &mut Out,
    state: &State,
    operation: &str,
    args: &[u64],
    result: u64,
    compact: bool,
) {
    out.raw("{").raw("\"state\":");
    state.write_wire(out, compact);
    out.raw(",")
        .raw("\"request\":")
        .raw(operation)
        .raw(",")
        .raw("\"args\":");
    write_refs(out, args);
    out.raw(",").raw("\"ref\":").private_uint(result).raw("}");
}

// Snapshot helpers are used only by the delta producer. They own metadata,
// never VM state or host values, and reuse existing vector capacity on updates.
impl Period {
    fn delta_eq(&self, other: &Self) -> bool {
        self.seq == other.seq
            && self.return_pc == other.return_pc
            && self.len == other.len
            && self.p == other.p
            && self.i == other.i
            && self.need == other.need
            && self.chunks.len() == other.chunks.len()
            && self
                .chunks
                .iter()
                .zip(&other.chunks)
                .all(|(a, b)| a.base == b.base && a.nibbles == b.nibbles)
    }
    fn delta_snapshot(&self) -> Self {
        Self {
            seq: self.seq,
            return_pc: self.return_pc,
            len: self.len,
            p: self.p,
            i: self.i,
            need: self.need,
            chunks: self.chunks.clone(),
        }
    }
    fn delta_copy_from(&mut self, source: &Self) {
        self.seq = source.seq;
        self.return_pc = source.return_pc;
        self.len = source.len;
        self.p = source.p;
        self.i = source.i;
        self.need = source.need;
        for (destination, source) in self.chunks.iter_mut().zip(&source.chunks) {
            destination.base = source.base;
            if destination.nibbles != source.nibbles {
                destination.nibbles.clone_from(&source.nibbles);
            }
        }
        let old_len = self.chunks.len();
        self.chunks
            .extend(source.chunks.iter().skip(old_len).cloned());
        self.chunks.truncate(source.chunks.len());
    }
}
impl Sum {
    fn delta_eq(&self, other: &Self) -> bool {
        self.cache.delta_eq(&other.cache)
            && self.raw == other.raw
            && self
                .numbers
                .iter()
                .zip(&other.numbers)
                .all(|(a, b)| a.to_bits() == b.to_bits())
            && self.total.to_bits() == other.total.to_bits()
    }
    fn delta_snapshot(&self) -> Self {
        Self {
            cache: self.cache.delta_snapshot(),
            raw: self.raw.clone(),
            numbers: self.numbers,
            total: self.total,
        }
    }
    fn delta_copy_from(&mut self, source: &Self) {
        if !self.cache.delta_eq(&source.cache) {
            self.cache.delta_copy_from(&source.cache);
        }
        if self.raw != source.raw {
            self.raw.clone_from(&source.raw);
        }
        self.numbers = source.numbers;
        self.total = source.total;
    }
}
fn same_period(a: &Option<Period>, b: &Option<Period>) -> bool {
    match (a, b) {
        (None, None) => true,
        (Some(a), Some(b)) => a.delta_eq(b),
        _ => false,
    }
}
fn same_sum(a: &Option<Sum>, b: &Option<Sum>) -> bool {
    match (a, b) {
        (None, None) => true,
        (Some(a), Some(b)) => a.delta_eq(b),
        _ => false,
    }
}
impl State {
    fn delta_mask(&self, previous: &Self) -> u8 {
        u8::from(self.pc != previous.pc)
            | (u8::from(self.env != previous.env) << 1)
            | (u8::from(self.stack != previous.stack) << 2)
            | (u8::from(self.catches != previous.catches) << 3)
            | (u8::from(self.mode != previous.mode) << 4)
            | (u8::from(self.target != previous.target) << 5)
            | (u8::from(!same_period(&self.period, &previous.period)) << 6)
            | (u8::from(!same_sum(&self.sum, &previous.sum)) << 7)
    }
    fn delta_snapshot(&self) -> Self {
        Self {
            pc: self.pc,
            env: self.env,
            stack: self.stack.clone(),
            catches: self.catches.clone(),
            mode: self.mode,
            target: self.target,
            period: self.period.as_ref().map(Period::delta_snapshot),
            sum: self.sum.as_ref().map(Sum::delta_snapshot),
        }
    }
    fn delta_copy_fields(&mut self, source: &Self, mask: u8) {
        if mask & 1 != 0 {
            self.pc = source.pc;
        }
        if mask & 2 != 0 {
            self.env = source.env;
        }
        if mask & 4 != 0 {
            self.stack.clone_from(&source.stack);
        }
        if mask & 8 != 0 {
            self.catches.clone_from(&source.catches);
        }
        if mask & 16 != 0 {
            self.mode = source.mode;
        }
        if mask & 32 != 0 {
            self.target = source.target;
        }
        if mask & 64 != 0 {
            match &source.period {
                None => self.period = None,
                Some(source) => match &mut self.period {
                    Some(destination) => destination.delta_copy_from(source),
                    None => self.period = Some(source.delta_snapshot()),
                },
            }
        }
        if mask & 128 != 0 {
            match &source.sum {
                None => self.sum = None,
                Some(source) => match &mut self.sum {
                    Some(destination) => destination.delta_copy_from(source),
                    None => self.sum = Some(source.delta_snapshot()),
                },
            }
        }
    }
    fn write_delta(&self, out: &mut Out, mask: u8) {
        out.raw("[").private_uint(u64::from(mask));
        if mask & 1 != 0 {
            out.raw(",").private_uint(self.pc as u64);
        }
        if mask & 2 != 0 {
            out.raw(",").private_uint(self.env);
        }
        if mask & 4 != 0 {
            out.raw(",");
            write_refs(out, &self.stack);
        }
        if mask & 8 != 0 {
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
            out.raw("]");
        }
        if mask & 16 != 0 {
            out.raw(",").private_uint(u64::from(self.mode));
        }
        if mask & 32 != 0 {
            out.raw(",").private_uint(self.target as u64);
        }
        if mask & 64 != 0 {
            out.raw(",");
            if let Some(period) = &self.period {
                period.write(out);
            } else {
                out.raw("null");
            }
        }
        if mask & 128 != 0 {
            out.raw(",");
            if let Some(sum) = &self.sum {
                sum.write(out);
            } else {
                out.raw("null");
            }
        }
        out.raw("]");
    }
}
#[derive(Clone, Copy)]
enum Wire {
    Object,
    V1,
    Delta,
}
impl Wire {
    #[cfg(test)]
    fn legacy(compact: bool) -> Self {
        if compact {
            Self::V1
        } else {
            Self::Object
        }
    }
    fn compact(self) -> bool {
        !matches!(self, Self::Object)
    }
}

struct RequestBatch {
    out: Out,
    count: usize,
    compact: bool,
    delta: bool,
    previous: Option<State>,
}
impl RequestBatch {
    #[cfg(test)]
    fn new(compact: bool) -> Self {
        Self::new_wire(Wire::legacy(compact))
    }
    fn new_wire(wire: Wire) -> Self {
        Self {
            out: Out(Vec::new()),
            count: 0,
            compact: wire.compact(),
            delta: matches!(wire, Wire::Delta),
            previous: None,
        }
    }
    fn len(&self) -> usize {
        self.count
    }
    fn is_empty(&self) -> bool {
        self.count == 0
    }
    fn prefix(&mut self) {
        if self.delta {
            self.out.raw(r#"{"wire":"state-v2d","batch":["#);
        } else if self.compact {
            self.out.raw(r#"{"wire":"state-v1","batch":["#);
        } else {
            self.out.raw(r#"{"batch":["#);
        }
    }
    fn append(&mut self, state: &State, operation: &str, args: &[u64], result: u64) {
        if self.count == 0 {
            self.prefix();
        } else {
            self.out.raw(",");
        }
        if self.delta {
            let mask = self
                .previous
                .as_ref()
                .map_or(255, |previous| state.delta_mask(previous));
            self.out.raw("[");
            state.write_delta(&mut self.out, mask);
            self.out.raw(",").raw(operation).raw(",");
            write_refs(&mut self.out, args);
            self.out.raw(",").private_uint(result).raw("]");
            if let Some(previous) = &mut self.previous {
                previous.delta_copy_fields(state, mask);
            } else {
                self.previous = Some(state.delta_snapshot());
            }
        } else {
            write_request(&mut self.out, state, operation, args, result, self.compact);
        }
        self.count += 1;
    }
    fn finish(mut self) -> String {
        if self.count == 0 {
            self.prefix();
        }
        self.out.raw("]}");
        self.out.text()
    }
}

// Byte oracles frozen from noevia-rs 17140326 evaluator.rs529-547. Only the
// cfg(test) attributes were added; both original writer bodies remain unchanged.
#[cfg(test)]
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
#[cfg(test)]
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

// The generic path still reads the same Value members in the same order. The
// canonical path supplies only already-decoded private scalars, never host values.
enum ResumeFrame<'a> {
    Generic(&'a Value),
    Value(u64),
    Truth(bool),
    Thrown(u64),
}
impl ResumeFrame<'_> {
    fn get(&self, key: &str) -> Option<&Value> {
        match self {
            Self::Generic(value) => value.get(key),
            _ => None,
        }
    }
    fn thrown(&self) -> R<Option<u64>> {
        match self {
            Self::Thrown(value) => Ok(Some(*value)),
            Self::Generic(value) => value.get("thrown").map(|v| integer(Some(v))).transpose(),
            _ => Ok(None),
        }
    }
    fn value(&self) -> R<u64> {
        match self {
            Self::Value(value) => Ok(*value),
            _ => integer(self.get("value")),
        }
    }
    fn truth(&self) -> R<bool> {
        match self {
            Self::Truth(value) => Ok(*value),
            _ => match self.get("truth") {
                Some(Value::Bool(value)) => Ok(*value),
                _ => Err(Refusal::Input),
            },
        }
    }
}

// Recognizes a complete, fixed ASCII subset of the existing wire format. A
// mismatch is never a refusal: the caller runs the unchanged general JSON parser.
// Its fixed containers are shallower than WIRE_DEPTH + 4. No strings or unknown
// fields are skipped, so successful recognition also proves UTF-8/JSON syntax.
struct CanonicalReader<'a> {
    bytes: &'a [u8],
    at: usize,
}
impl CanonicalReader<'_> {
    fn take(&mut self, token: &[u8]) -> Option<()> {
        let end = self.at.checked_add(token.len())?;
        if self.bytes.get(self.at..end)? != token {
            return None;
        }
        self.at = end;
        Some(())
    }
    fn uint(&mut self) -> Option<u64> {
        let first = *self.bytes.get(self.at)?;
        if !first.is_ascii_digit() {
            return None;
        }
        if first == b'0' {
            self.at = self.at.checked_add(1)?;
            if self.bytes.get(self.at).is_some_and(u8::is_ascii_digit) {
                return None;
            }
            return Some(0);
        }
        let mut value = 0_u64;
        while let Some(digit) = self.bytes.get(self.at).filter(|b| b.is_ascii_digit()) {
            value = value
                .checked_mul(10)?
                .checked_add(u64::from(*digit - b'0'))?;
            if value > 9_007_199_254_740_991 {
                return None;
            }
            self.at = self.at.checked_add(1)?;
        }
        Some(value)
    }
    fn index(&mut self) -> Option<usize> {
        usize::try_from(self.uint()?).ok()
    }
    fn refs(&mut self) -> Option<Vec<u64>> {
        self.take(b"[")?;
        let mut values = Vec::new();
        if self.take(b"]").is_some() {
            return Some(values);
        }
        loop {
            if values.len() == STACK_REGISTERS {
                return None;
            }
            values.push(self.uint()?);
            if self.take(b"]").is_some() {
                return Some(values);
            }
            self.take(b",")?;
        }
    }
    fn catches(&mut self) -> Option<Vec<(usize, usize, u64)>> {
        self.take(b"[")?;
        let mut values = Vec::new();
        if self.take(b"]").is_some() {
            return Some(values);
        }
        loop {
            if values.len() == CATCH_REGISTERS {
                return None;
            }
            self.take(b"[")?;
            let pc = self.index()?;
            self.take(b",")?;
            let depth = self.index()?;
            self.take(b",")?;
            let env = self.uint()?;
            self.take(b"]")?;
            values.push((pc, depth, env));
            if self.take(b"]").is_some() {
                return Some(values);
            }
            self.take(b",")?;
        }
    }
    fn resume(mut self) -> Option<(State, ResumeFrame<'static>, Wire)> {
        self.take(b"{\"state\":[")?;
        let pc = self.index()?;
        self.take(b",")?;
        let env = self.uint()?;
        self.take(b",")?;
        let stack = self.refs()?;
        self.take(b",")?;
        let catches = self.catches()?;
        self.take(b",")?;
        let mode = u8::try_from(self.uint()?).ok().filter(|m| *m <= 2)?;
        self.take(b",")?;
        let target = self.index()?;
        self.take(b",null,null],\"frame\":{")?;
        let frame = if self.take(b"\"thrown\":").is_some() {
            ResumeFrame::Thrown(self.uint()?)
        } else if self.take(b"\"value\":").is_some() && mode == 0 {
            ResumeFrame::Value(self.uint()?)
        } else if self.take(b"\"truth\":").is_some() && mode != 0 {
            if self.take(b"true").is_some() {
                ResumeFrame::Truth(true)
            } else {
                self.take(b"false")?;
                ResumeFrame::Truth(false)
            }
        } else {
            return None;
        };
        let wire = if self.take(b"},\"wire\":\"state-v1\"}").is_some() {
            Wire::V1
        } else {
            self.take(b"},\"wire\":\"state-v2d\"}")?;
            Wire::Delta
        };
        if self.at != self.bytes.len() {
            return None;
        }
        let state = State {
            pc,
            env,
            stack,
            catches,
            mode,
            target,
            period: None,
            sum: None,
        }
        .checked()
        .ok()?;
        Some((state, frame, wire))
    }
}

pub(super) fn try_canonical_resume(body: &[u8]) -> Option<R<String>> {
    // EOF and all eligibility checks precede the sole execution entrypoint.
    let (state, frame, wire) = CanonicalReader { bytes: body, at: 0 }.resume()?;
    Some(execute_wire(state, Some(frame), wire))
}

pub(super) fn call(op: u8, args: &Value) -> R<String> {
    let wire = match args.get("wire").and_then(Value::as_str) {
        Some(tag) if tag.iter().copied().eq("state-v1".encode_utf16()) => Wire::V1,
        Some(tag) if tag.iter().copied().eq("state-v2d".encode_utf16()) => Wire::Delta,
        _ => Wire::Object,
    };
    let compact = wire.compact();
    if op == 11 {
        let function = index(args.get("function"))?;
        let pc = *ENTRIES.get(function).ok_or(Refusal::Input)?;
        let state = State {
            pc,
            env: integer(args.get("env"))?,
            stack: Vec::new(),
            catches: Vec::new(),
            mode: 0,
            target: 0,
            period: None,
            sum: None,
        };
        execute_wire(state, None, wire)
    } else {
        let state = State::read_wire(args.get("state").ok_or(Refusal::Input)?, compact)?;
        let frame = args.get("frame").ok_or(Refusal::Input)?;
        execute_wire(state, Some(ResumeFrame::Generic(frame)), wire)
    }
}

#[cfg(test)]
fn execute(state: State, frame: Option<ResumeFrame<'_>>, compact: bool) -> R<String> {
    execute_wire(state, frame, Wire::legacy(compact))
}
fn execute_wire(mut state: State, frame: Option<ResumeFrame<'_>>, wire: Wire) -> R<String> {
    let mut requests = RequestBatch::new_wire(wire);
    let end = |key: &str, reference: u64| end(key, reference, wire.compact());
    if let Some(frame) = frame {
        if let Some(thrown) = frame.thrown()? {
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
                    requests.append(&state, &operation, &[seq], 0);
                    return Ok(requests.finish());
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
            state.push(frame.value()?)?;
        } else {
            let test = frame.truth()?;
            if test == (state.mode == 1) {
                state.pc = state.target;
            }
        }
    }
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
                requests.append(&state, &operation, &[seq], 0);
                return Ok(requests.finish());
            }
            let answer = sum.total;
            state.pc = sum.cache.return_pc;
            state.sum = None;
            state.mode = 0;
            let operation = format!(r#"{{"op":"floatBits","data":"{:016x}"}}"#, answer.to_bits());
            requests.append(&state, &operation, &[], VIRTUAL_BASE + 1);
            return Ok(requests.finish());
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
                requests.append(&state, &operation, &[], VIRTUAL_BASE + 1);
                return Ok(requests.finish());
            }
            let position = missing.ok_or(Refusal::Input)?;
            period.need = (position / BOOLEAN_CHUNK) * BOOLEAN_CHUNK;
            let count = period.len.saturating_sub(period.need).min(BOOLEAN_CHUNK);
            let operation = format!(
                r#"{{"op":"booleanChunk","data":{{"start":{},"count":{count}}}}}"#,
                period.need
            );
            let seq = period.seq;
            requests.append(&state, &operation, &[seq], 0);
            return Ok(requests.finish());
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
                requests.append(&state, operation, &arguments, result);
                state.push(result)?;
                if requests.len() == BATCH_OPERATIONS {
                    return Ok(requests.finish());
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
                requests.append(&state, &operation, &[reference], 0);
                return Ok(requests.finish());
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
                    return Ok(requests.finish());
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
                requests.append(&state, r#"{"op":"numberFacts","data":null}"#, &arguments, 0);
                return Ok(requests.finish());
            }
            Instruction::BooleanPeriod(return_pc) => {
                if !requests.is_empty() {
                    return Ok(requests.finish());
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
                requests.append(
                    &state,
                    r#"{"op":"booleanArrayFacts","data":null}"#,
                    &[seq],
                    0,
                );
                return Ok(requests.finish());
            }
            Instruction::Return => {
                if !requests.is_empty() {
                    return Ok(requests.finish());
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

    // Frozen verbatim from preserved evaluator 10c7b7fe, with method name only
    // changed. This test-only oracle retains its original div/rem10 and copy path.
    impl Out {
        fn private_uint_10c(&mut self, mut value: u64) -> &mut Self {
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

    fn assert_private_uint_chunk_bytes(value: u64) {
        let mut actual = Out(b"prefix:".to_vec());
        actual.private_uint(value).raw(":suffix");
        let mut frozen = Out(b"prefix:".to_vec());
        frozen.private_uint_10c(value).raw(":suffix");
        assert_eq!(actual.0, frozen.0, "frozen writer: {value}");
        assert_eq!(
            actual.0,
            format!("prefix:{value}:suffix").as_bytes(),
            "u64 Display: {value}"
        );
    }

    #[test]
    fn private_uint_chunks_match_both_oracles_at_full_u64_boundaries() {
        for value in [
            0,
            1,
            9,
            10,
            99,
            100,
            u64::from(u32::MAX),
            u64::MAX - 1,
            u64::MAX,
        ] {
            assert_private_uint_chunk_bytes(value);
        }
        for power in 0..=63 {
            let center = 1_u64 << power;
            for value in [center.checked_sub(1), Some(center), center.checked_add(1)]
                .into_iter()
                .flatten()
            {
                assert_private_uint_chunk_bytes(value);
            }
        }
        for power in 0..=19 {
            let center = 10_u64.pow(power);
            for value in [center - 1, center, center + 1] {
                assert_private_uint_chunk_bytes(value);
            }
        }
        for center in [100_000_000_u64, 10_000_000_000_000_000] {
            for value in [center - 1, center, center + 1] {
                assert_private_uint_chunk_bytes(value);
            }
        }
    }

    #[test]
    fn private_uint_chunks_preserve_zero_padding_and_every_decimal_pair() {
        const CHUNK: u64 = 100_000_000;
        for prefix in [1, 9, 99, 10_000_000, CHUNK - 1] {
            for low in [0, 1, 9, 10, 99, 100, 10_000, 1_000_000, CHUNK - 1] {
                assert_private_uint_chunk_bytes(prefix * CHUNK + low);
            }
        }
        for prefix in [1_u64, 999, 1844] {
            for middle in [0, 1, 99, CHUNK - 1] {
                for low in [0, 1, 9, 99, 100, CHUNK - 1] {
                    if let Some(value) = prefix
                        .checked_mul(CHUNK)
                        .and_then(|n| n.checked_add(middle))
                        .and_then(|n| n.checked_mul(CHUNK))
                        .and_then(|n| n.checked_add(low))
                    {
                        assert_private_uint_chunk_bytes(value);
                    }
                }
            }
        }
        for prefix in [
            0,
            1,
            99,
            CHUNK / 100 - 1,
            CHUNK / 100,
            CHUNK / 100 + 1,
            99_999_999_999_999,
            u64::MAX / 100,
        ] {
            for remainder in 0..100 {
                if let Some(value) = prefix
                    .checked_mul(100)
                    .and_then(|n| n.checked_add(remainder))
                {
                    assert_private_uint_chunk_bytes(value);
                }
            }
        }
    }

    #[test]
    fn private_uint_chunks_match_both_oracles_for_full_width_seeded_values() {
        let mut seed = 0x1071_deca_1c8f_9327_u64;
        for _ in 0..16_384 {
            seed = seed.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1);
            assert_private_uint_chunk_bytes(seed);
        }
    }

    fn output_buffer_state(mode: u8) -> State {
        let period = || Period {
            seq: 9_007_199_254_740_991,
            return_pc: PROGRAM.len() - 1,
            len: 4096,
            p: 16,
            i: 7,
            need: 0,
            chunks: vec![Chunk {
                base: 0,
                nibbles: vec![0, 1, 10, 15],
            }],
        };
        State {
            pc: 1,
            env: 9_007_199_254_740_991,
            stack: vec![1, VIRTUAL_BASE + 1],
            catches: vec![(PROGRAM.len() - 1, 1, 17)],
            mode,
            target: PROGRAM.len() - 1,
            period: matches!(mode, 3 | 4).then(period),
            sum: matches!(mode, 5..=7).then(|| Sum {
                cache: period(),
                raw: vec![1, 9_007_199_254_740_991],
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
        }
    }

    #[test]
    fn output_buffer_batches_match_frozen_171_writers_in_both_wire_forms() {
        let operations = [
            r#"{"op":"get","data":null}"#,
            r#"{"op":"literal","data":"quote\" slash\\ newline\n nul\u0000 lone\ud800"}"#,
            r#"{"op":"floatBits","data":"8000000000000000"}"#,
        ];
        for compact in [false, true] {
            for count in [0, 1, 2, 31, 32] {
                for mode in 0..=7 {
                    let mut state = output_buffer_state(mode);
                    let mut actual = RequestBatch::new(compact);
                    // An empty/done-only call has not started or allocated a batch.
                    assert!(actual.out.0.is_empty());
                    assert_eq!(actual.out.0.capacity(), 0);
                    let mut expected = Vec::new();
                    for index in 0..count {
                        let operation = operations[index % operations.len()];
                        let args = [1, VIRTUAL_BASE + index as u64, 9_007_199_254_740_991];
                        let result = VIRTUAL_BASE + actual.len() as u64 + 1;
                        expected.push(request(&state, operation, &args, result, compact));
                        actual.append(&state, operation, &args, result);
                        assert_eq!(actual.len(), index + 1);
                        // Queue snapshots must remain independent of subsequent steps.
                        state.pc += 1;
                        state.env = result;
                        state.stack.push(result);
                        if let Some(period) = state.period.as_mut() {
                            period.i += 1;
                            period.chunks[0].nibbles[0] ^= 1;
                        }
                        if let Some(sum) = state.sum.as_mut() {
                            sum.raw.push(result);
                            sum.total = f64::from_bits(sum.total.to_bits() ^ 1);
                        }
                    }
                    assert_eq!(
                        actual.finish(),
                        batch(&expected, compact),
                        "count {count}, mode {mode}, compact {compact}"
                    );
                }
            }
        }
    }

    #[test]
    fn output_buffer_append_freezes_before_result_push_and_borrowed_data_mutation() {
        for compact in [false, true] {
            let mut state = output_buffer_state(0);
            state.stack.clear();
            state.catches.clear();
            let mut operation = String::from(r#"{"op":"literal","data":"before\n"}"#);
            let mut arguments = vec![7];
            let mut actual = RequestBatch::new(compact);
            let first = VIRTUAL_BASE + actual.len() as u64 + 1;
            let mut expected = vec![request(&state, &operation, &arguments, first, compact)];
            actual.append(&state, &operation, &arguments, first);
            state.push(first).unwrap();
            state.pc += 1;
            let second = VIRTUAL_BASE + actual.len() as u64 + 1;
            expected.push(request(&state, &operation, &arguments, second, compact));
            actual.append(&state, &operation, &arguments, second);
            state.stack.clear();
            state.env = 41;
            operation.clear();
            arguments.clear();
            let text = actual.finish();
            assert_eq!(text, batch(&expected, compact));
            let reply = prompt_framing::json::parse_utf8(text.as_bytes(), 32).unwrap();
            let Value::Arr(operations) = reply.get("batch").unwrap() else {
                panic!("batch");
            };
            let before = State::read_wire(operations[0].get("state").unwrap(), compact).unwrap();
            let after = State::read_wire(operations[1].get("state").unwrap(), compact).unwrap();
            assert!(before.stack.is_empty());
            assert_eq!(after.stack, vec![first]);
            assert!(!after.stack.contains(&second));
        }
    }

    fn output_buffer_resume(state: &State, compact: bool) -> Value {
        let mut out = Out(Vec::new());
        out.raw(r#"{"state":"#);
        state.write_wire(&mut out, compact);
        out.raw(r#","frame":{"value":41}"#);
        if compact {
            out.raw(r#","wire":"state-v1""#);
        }
        out.raw("}");
        prompt_framing::json::parse_utf8(out.text().as_bytes(), 32).unwrap()
    }

    #[test]
    fn output_buffer_keeps_pending_host_snapshots_at_control_boundaries() {
        for compact in [false, true] {
            for boundary in 0..5 {
                let (pc, count) = PROGRAM
                    .windows(2)
                    .enumerate()
                    .find_map(|(pc, pair)| {
                        let Instruction::Host(count, _) = pair[0] else {
                            return None;
                        };
                        let matches = matches!(
                            (boundary, pair[1]),
                            (0, Instruction::Return)
                                | (1, Instruction::KvSum(_))
                                | (2, Instruction::BooleanPeriod(_))
                                | (3, Instruction::If(_, _))
                                | (4, Instruction::Unless(_, _))
                        );
                        matches.then_some((pc, count))
                    })
                    .unwrap();
                let mut state = output_buffer_state(0);
                state.pc = pc;
                state.env = 1;
                state.catches.clear();
                state.stack = vec![2; count.saturating_sub(1)];
                let text = call(12, &output_buffer_resume(&state, compact)).unwrap();
                let reply = prompt_framing::json::parse_utf8(text.as_bytes(), 32).unwrap();
                let Value::Arr(operations) = reply.get("batch").unwrap() else {
                    panic!("batch");
                };
                assert_eq!(operations.len(), if boundary >= 3 { 2 } else { 1 });
                let first = State::read_wire(operations[0].get("state").unwrap(), compact).unwrap();
                assert_eq!(first.pc, pc + 1);
                assert!(first.period.is_none() && first.sum.is_none());
                assert!(!first.stack.contains(&(VIRTUAL_BASE + 1)));
                assert_eq!(integer(operations[0].get("ref")).unwrap(), VIRTUAL_BASE + 1);
                if boundary >= 3 {
                    let last =
                        State::read_wire(operations[1].get("state").unwrap(), compact).unwrap();
                    assert_eq!(last.mode, if boundary == 3 { 1 } else { 2 });
                    assert_eq!(integer(operations[1].get("ref")).unwrap(), 0);
                }
            }
        }
    }

    #[test]
    fn output_buffer_generated_entries_keep_32_bound_and_preappend_virtual_refs() {
        for compact in [false, true] {
            let mut saw_full = false;
            for function in 0..ENTRIES.len() {
                let text = if compact {
                    format!(r#"{{"function":{function},"env":1,"wire":"state-v1"}}"#)
                } else {
                    format!(r#"{{"function":{function},"env":1}}"#)
                };
                let input = prompt_framing::json::parse_utf8(text.as_bytes(), 8).unwrap();
                let text = call(11, &input).unwrap();
                let reply = prompt_framing::json::parse_utf8(text.as_bytes(), 32).unwrap();
                let Some(Value::Arr(operations)) = reply.get("batch") else {
                    continue;
                };
                assert!(!operations.is_empty() && operations.len() <= BATCH_OPERATIONS);
                saw_full |= operations.len() == BATCH_OPERATIONS;
                for (index, operation) in operations.iter().enumerate() {
                    let result = integer(operation.get("ref")).unwrap();
                    if result != 0 {
                        assert_eq!(result, VIRTUAL_BASE + index as u64 + 1);
                        let state =
                            State::read_wire(operation.get("state").unwrap(), compact).unwrap();
                        assert!(!state.stack.contains(&result));
                        assert!(state
                            .stack
                            .iter()
                            .all(|value| *value < VIRTUAL_BASE || *value < result));
                    }
                }
            }
            assert!(
                saw_full,
                "actual generated entry must exercise the 32-operation flush"
            );
        }
    }

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
    fn canonical_test_body(state: &State, frame: &str) -> String {
        let mut out = Out(Vec::new());
        out.raw(r#"{"state":"#);
        state.write_wire(&mut out, true);
        out.raw(",\"frame\":")
            .raw(frame)
            .raw(",\"wire\":\"state-v1\"}");
        out.text()
    }
    fn canonical_test_state() -> State {
        State {
            pc: PROGRAM
                .iter()
                .position(|op| matches!(op, Instruction::Return))
                .unwrap(),
            env: 0,
            stack: vec![7],
            catches: Vec::new(),
            mode: 0,
            target: 0,
            period: None,
            sum: None,
        }
    }
    fn canonical_status(result: R<String>) -> (u32, String) {
        match result {
            Ok(text) => (0, text),
            Err(error) => (1, error.json().to_owned()),
        }
    }
    fn canonical_compare(body: &[u8]) -> bool {
        let generic = canonical_status(super::super::call_generic(12, body));
        let recognized = if let Some(result) = try_canonical_resume(body) {
            assert_eq!(canonical_status(result), generic);
            true
        } else {
            false
        };
        let mut input = vec![12];
        input.extend_from_slice(body);
        assert_eq!(super::super::call(&input), generic);
        recognized
    }

    #[test]
    fn canonical_virtual_uint_spelling_matches_existing_oracles_and_boundaries() {
        let mut values = vec![
            0,
            1,
            99,
            100,
            VIRTUAL_BASE - 1,
            VIRTUAL_BASE,
            VIRTUAL_BASE + 33,
            9_007_199_254_740_991,
            u64::MAX,
        ];
        values.extend((1..=32).map(|n| VIRTUAL_BASE + n));
        for value in values {
            assert_private_uint_chunk_bytes(value);
        }
    }

    #[test]
    fn canonical_frames_preserve_zero_truth_targets_and_throw_priority() {
        let mut state = canonical_test_state();
        for mode in 0..=2 {
            state.mode = mode;
            state.target = PROGRAM.len() + 1;
            let frames: &[&str] = if mode == 0 {
                &[r#"{"value":0}"#]
            } else {
                &[r#"{"truth":false}"#, r#"{"truth":true}"#]
            };
            for &frame in frames {
                let body = canonical_test_body(&state, frame);
                assert!(canonical_compare(body.as_bytes()));
                let answer = canonical_status(try_canonical_resume(body.as_bytes()).unwrap());
                let takes_branch = (mode == 1 && frame == r#"{"truth":true}"#)
                    || (mode == 2 && frame == r#"{"truth":false}"#);
                if !takes_branch {
                    assert_eq!(answer.0, 0);
                } else {
                    // An unused out-of-program target is permitted by State::read.
                    // Only the taken branch fails in the shared interpreter.
                    assert_eq!(answer, (1, Refusal::Input.json().to_owned()));
                }
            }
            let body = canonical_test_body(&state, r#"{"thrown":0}"#);
            assert!(canonical_compare(body.as_bytes()));
            assert_eq!(
                canonical_status(try_canonical_resume(body.as_bytes()).unwrap()),
                (0, r#"{"wire":"state-v1","throw":0}"#.to_owned())
            );
            state.catches = vec![(state.pc, STACK_REGISTERS + 17, 0)];
            let body = canonical_test_body(&state, r#"{"thrown":0}"#);
            assert!(canonical_compare(body.as_bytes()));
            assert_eq!(
                canonical_status(try_canonical_resume(body.as_bytes()).unwrap()),
                (0, r#"{"wire":"state-v1","done":0}"#.to_owned())
            );
            state.catches.clear();
        }
        state.mode = 0;
        let body = canonical_test_body(&state, r#"{"value":0,"thrown":9}"#);
        assert!(!canonical_compare(body.as_bytes()));
        assert_eq!(
            canonical_status(super::super::call_generic(12, body.as_bytes())),
            (0, r#"{"wire":"state-v1","throw":9}"#.to_owned())
        );
    }

    #[test]
    fn canonical_reader_falls_back_for_noncanonical_syntax_and_full_eof() {
        let state = canonical_test_state();
        let body = canonical_test_body(&state, r#"{"value":0}"#);
        let mut alternatives = vec![
            format!(" {body}"),
            format!("{body} "),
            format!("{body}null"),
            body.replace("\"state\":", "\"\\u0073tate\":"),
            body.replace("state-v1", "state-\\u00761"),
            body.replace("\"value\":0", "\"value\":-0"),
            body.replace("\"value\":0", "\"value\":1.0"),
            body.replace("\"value\":0", "\"value\":1e0"),
            body.replace("\"value\":0", "\"value\":1e309"),
            body.replace("\"value\":0", "\"value\":9007199254740992"),
            body.replace("\"value\":0", "\"value\":184467440737095516160"),
            body.replace("\"value\":0", "\"value\":00"),
            body.replace("\"value\":0", "\"value\":null,\"value\":0"),
            body.replace("\"value\":0", "\"truth\":false"),
            body.replace("\"wire\":\"state-v1\"", "\"wire\":\"state-v0\""),
            body.replace(
                "\"wire\":\"state-v1\"",
                "\"wire\":\"bad\",\"wire\":\"state-v1\"",
            ),
        ];
        let mut extra = body.trim_end_matches('}').to_owned();
        extra.push_str(",\"unused\":");
        extra.push_str(&"[".repeat(32));
        extra.push('0');
        extra.push_str(&"]".repeat(32));
        extra.push('}');
        alternatives.push(extra);
        for text in alternatives {
            assert!(!canonical_compare(text.as_bytes()), "{text}");
        }
        for end in 0..body.len() {
            assert!(!canonical_compare(body.get(..end).unwrap().as_bytes()));
        }
        let mut invalid = body.into_bytes();
        invalid.push(0xff);
        assert!(!canonical_compare(&invalid));
    }

    #[test]
    fn canonical_reader_keeps_existing_vector_and_scalar_domain_edges() {
        let mut state = canonical_test_state();
        for count in [0, STACK_REGISTERS, STACK_REGISTERS + 1] {
            state.stack = vec![0; count];
            let body = canonical_test_body(&state, r#"{"value":0}"#);
            assert_eq!(canonical_compare(body.as_bytes()), count <= STACK_REGISTERS);
        }
        state.stack = vec![
            0,
            VIRTUAL_BASE + 1,
            VIRTUAL_BASE + 32,
            9_007_199_254_740_991,
        ];
        for count in [0, CATCH_REGISTERS, CATCH_REGISTERS + 1] {
            state.catches = vec![(u32::MAX as usize, u32::MAX as usize, 0); count];
            let body = canonical_test_body(&state, r#"{"thrown":0}"#);
            assert_eq!(canonical_compare(body.as_bytes()), count <= CATCH_REGISTERS);
        }
        state.catches.clear();
        state.period = Some(Period {
            seq: 0,
            return_pc: state.pc,
            len: 0,
            p: 0,
            i: 0,
            need: 0,
            chunks: Vec::new(),
        });
        let body = canonical_test_body(&state, r#"{"value":0}"#);
        assert!(!canonical_compare(body.as_bytes()));
        state.period = None;
        for mode in 3..=7 {
            state.mode = mode;
            let body = canonical_test_body(&state, r#"{"value":0}"#);
            assert!(!canonical_compare(body.as_bytes()));
        }
    }

    #[test]
    fn canonical_reader_seeded_scalar_vectors_and_byte_mutations_match_general() {
        let mut seed = 0x41ab_8e90_1223_9955_u64;
        for step in 0..2048 {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            let mut state = canonical_test_state();
            state.mode = (seed % 3) as u8;
            state.env = seed & 9_007_199_254_740_991;
            state.target = if seed & 1 == 0 {
                state.pc
            } else {
                PROGRAM.len() + 1
            };
            state.stack = (0..seed % 10)
                .map(|n| {
                    if n % 2 == 0 {
                        VIRTUAL_BASE + 1 + n
                    } else {
                        seed & 9_007_199_254_740_991
                    }
                })
                .collect();
            if seed & 4 != 0 {
                state
                    .catches
                    .push((state.pc, (seed % 300) as usize, state.env));
            }
            let frame = if seed & 8 != 0 {
                format!(r#"{{"thrown":{}}}"#, seed & 9_007_199_254_740_991)
            } else if state.mode == 0 {
                format!(r#"{{"value":{}}}"#, seed & 9_007_199_254_740_991)
            } else {
                format!(r#"{{"truth":{}}}"#, seed & 16 != 0)
            };
            let body = canonical_test_body(&state, &frame);
            assert!(canonical_compare(body.as_bytes()), "{step}");
            let mut mutated = body.into_bytes();
            let at = seed as usize % mutated.len();
            *mutated.get_mut(at).unwrap() = *[b'0', b'9', b'-', b' ', b'"', b',', 0xff]
                .get(step % 7)
                .unwrap();
            canonical_compare(&mutated);
        }
    }

    // Independent evidence decoder: expand only the new delta metadata, then
    // compare whole rows with the frozen full-state writer, never a VM mirror.
    fn state_delta_replay(text: &str) -> (Value, Vec<u8>) {
        let value = transport_parse(text);
        let Some(Value::Arr(rows)) = value.get("batch") else {
            return (value, Vec::new());
        };
        assert_eq!(
            value.get("wire").unwrap().as_str().unwrap(),
            "state-v2d".encode_utf16().collect::<Vec<_>>()
        );
        let mut previous = vec![Value::Null; 8];
        let mut expanded = Vec::new();
        let mut masks = Vec::new();
        for (row_index, row) in rows.iter().enumerate() {
            let Value::Arr(row) = row else {
                panic!("delta row")
            };
            assert_eq!(row.len(), 4);
            let Value::Arr(delta) = row.first().unwrap() else {
                panic!("state delta")
            };
            let mask = u8::try_from(integer(delta.first()).unwrap()).unwrap();
            if row_index == 0 {
                assert_eq!(mask, 255);
            }
            assert_eq!(delta.len(), 1 + mask.count_ones() as usize);
            let mut changed = delta.iter().skip(1);
            for (bit, destination) in previous.iter_mut().enumerate() {
                if mask & (1 << bit) != 0 {
                    *destination = changed.next().unwrap().clone();
                }
            }
            assert!(changed.next().is_none());
            expanded.push(Value::Obj(vec![
                (
                    "state".encode_utf16().collect(),
                    Value::Arr(previous.clone()),
                ),
                (
                    "request".encode_utf16().collect(),
                    row.get(1).unwrap().clone(),
                ),
                ("args".encode_utf16().collect(), row.get(2).unwrap().clone()),
                ("ref".encode_utf16().collect(), row.get(3).unwrap().clone()),
            ]));
            masks.push(mask);
        }
        (
            Value::Obj(vec![
                (
                    "wire".encode_utf16().collect(),
                    Value::Str("state-v1".encode_utf16().collect()),
                ),
                ("batch".encode_utf16().collect(), Value::Arr(expanded)),
            ]),
            masks,
        )
    }
    fn state_delta_period() -> Period {
        Period {
            seq: 0,
            return_pc: 1,
            len: 8192,
            p: 1,
            i: 0,
            need: 0,
            chunks: vec![Chunk {
                base: 0,
                nibbles: vec![0, 1, 10, 15],
            }],
        }
    }
    fn state_delta_fixture() -> State {
        State {
            pc: 1,
            env: 9_007_199_254_740_991,
            stack: vec![0, VIRTUAL_BASE + 1, 9_007_199_254_740_991],
            catches: vec![(0, 0, 0), (PROGRAM.len() - 1, 256, 9_007_199_254_740_991)],
            mode: 7,
            target: PROGRAM.len() - 1,
            period: Some(state_delta_period()),
            sum: Some(Sum {
                cache: state_delta_period(),
                raw: vec![0; 8],
                numbers: [
                    0.0,
                    -0.0,
                    f64::from_bits(0x7ff8_0000_0000_0001),
                    f64::INFINITY,
                    f64::NEG_INFINITY,
                    f64::from_bits(1),
                ],
                total: f64::from_bits(0x7ff8_0000_0000_0002),
            }),
        }
    }
    fn state_delta_record(batch: &mut RequestBatch, state: &State, expected: &mut Vec<String>) {
        let operation = r#"{"op":"literal","data":"quote\" slash\\ nul\u0000 lone\ud800"}"#;
        let args = [0, VIRTUAL_BASE + 32, 9_007_199_254_740_991];
        expected.push(request(state, operation, &args, VIRTUAL_BASE + 1, true));
        batch.append(state, operation, &args, VIRTUAL_BASE + 1);
    }
    fn state_delta_check(text: &str, expected: &[String]) -> Vec<u8> {
        let (expanded, masks) = state_delta_replay(text);
        assert_eq!(expanded, transport_parse(&batch(expected, true)));
        masks
    }

    #[test]
    fn state_delta_each_mask_matches_frozen_complete_rows() {
        let mut batch = RequestBatch::new_wire(Wire::Delta);
        let mut state = state_delta_fixture();
        let mut expected = Vec::new();
        state_delta_record(&mut batch, &state, &mut expected);
        state_delta_record(&mut batch, &state, &mut expected);
        for field in 0..8 {
            match field {
                0 => state.pc += 1,
                1 => state.env -= 1,
                2 => state.stack.push(17),
                3 => {
                    state.catches.pop();
                }
                4 => state.mode = 6,
                5 => state.target -= 1,
                6 => state.period = None,
                _ => state.sum = None,
            }
            state_delta_record(&mut batch, &state, &mut expected);
        }
        state = state_delta_fixture();
        state_delta_record(&mut batch, &state, &mut expected);
        assert_eq!(
            state_delta_check(&batch.finish(), &expected),
            vec![255, 0, 1, 2, 4, 8, 16, 32, 64, 128, 255]
        );
    }

    #[test]
    fn state_delta_batch_counts_reset_and_legacy_snapshot_absence() {
        for count in [0, 1, 32] {
            let mut current = RequestBatch::new_wire(Wire::Delta);
            assert!(current.previous.is_none());
            assert_eq!(current.out.0.capacity(), 0);
            let state = state_delta_fixture();
            let mut expected = Vec::new();
            for _ in 0..count {
                state_delta_record(&mut current, &state, &mut expected);
            }
            let masks = state_delta_check(&current.finish(), &expected);
            assert_eq!(masks.len(), count);
            if count > 0 {
                assert_eq!(masks.first(), Some(&255));
            }
        }
        for compact in [false, true] {
            let mut current = RequestBatch::new(compact);
            let state = state_delta_fixture();
            let expected = request(
                &state,
                r#"{"op":"test","data":"nullish"}"#,
                &[0],
                0,
                compact,
            );
            current.append(&state, r#"{"op":"test","data":"nullish"}"#, &[0], 0);
            assert!(current.previous.is_none());
            assert_eq!(current.finish(), batch(&[expected], compact));
        }
        let empty = RequestBatch::new_wire(Wire::Delta);
        assert!(empty.previous.is_none());
        assert_eq!(empty.finish(), r#"{"wire":"state-v2d","batch":[]}"#);
    }

    #[test]
    fn state_delta_snapshot_owns_metadata_and_reuses_capacity() {
        let mut state = state_delta_fixture();
        state.stack = vec![9; STACK_REGISTERS];
        state.catches = vec![(1, 1, 9); CATCH_REGISTERS];
        for period in [
            state.period.as_mut().unwrap(),
            &mut state.sum.as_mut().unwrap().cache,
        ] {
            period.chunks = vec![
                Chunk {
                    base: 0,
                    nibbles: vec![0; BOOLEAN_CHUNK / 4],
                },
                Chunk {
                    base: BOOLEAN_CHUNK,
                    nibbles: vec![15; BOOLEAN_CHUNK / 4],
                },
            ];
        }
        let mut actual = RequestBatch::new_wire(Wire::Delta);
        let mut expected = Vec::new();
        state_delta_record(&mut actual, &state, &mut expected);
        let previous = actual.previous.as_ref().unwrap();
        let stack_ptr = previous.stack.as_ptr();
        let catches_ptr = previous.catches.as_ptr();
        let period_ptr = previous
            .period
            .as_ref()
            .unwrap()
            .chunks
            .first()
            .unwrap()
            .nibbles
            .as_ptr();
        let sum_ptr = previous
            .sum
            .as_ref()
            .unwrap()
            .cache
            .chunks
            .first()
            .unwrap()
            .nibbles
            .as_ptr();
        let raw_ptr = previous.sum.as_ref().unwrap().raw.as_ptr();
        *state.stack.first_mut().unwrap() = 17;
        *state.catches.first_mut().unwrap() = (0, 0, 0);
        *state
            .period
            .as_mut()
            .unwrap()
            .chunks
            .first_mut()
            .unwrap()
            .nibbles
            .first_mut()
            .unwrap() = 3;
        *state
            .sum
            .as_mut()
            .unwrap()
            .cache
            .chunks
            .first_mut()
            .unwrap()
            .nibbles
            .first_mut()
            .unwrap() = 4;
        *state.sum.as_mut().unwrap().raw.first_mut().unwrap() = 19;
        state_delta_record(&mut actual, &state, &mut expected);
        let previous = actual.previous.as_ref().unwrap();
        assert_eq!(previous.stack.as_ptr(), stack_ptr);
        assert_eq!(previous.catches.as_ptr(), catches_ptr);
        assert_eq!(
            previous
                .period
                .as_ref()
                .unwrap()
                .chunks
                .first()
                .unwrap()
                .nibbles
                .as_ptr(),
            period_ptr
        );
        assert_eq!(
            previous
                .sum
                .as_ref()
                .unwrap()
                .cache
                .chunks
                .first()
                .unwrap()
                .nibbles
                .as_ptr(),
            sum_ptr
        );
        assert_eq!(previous.sum.as_ref().unwrap().raw.as_ptr(), raw_ptr);
        // Changing only Sum.total must not replace its unchanged large cache.
        state.sum.as_mut().unwrap().total = -0.0;
        state_delta_record(&mut actual, &state, &mut expected);
        assert_eq!(
            actual
                .previous
                .as_ref()
                .unwrap()
                .sum
                .as_ref()
                .unwrap()
                .cache
                .chunks
                .first()
                .unwrap()
                .nibbles
                .as_ptr(),
            sum_ptr
        );
        // Shrink each vector, then mutate the live VM state after the final append.
        state.stack.truncate(1);
        state.catches.truncate(1);
        state.period.as_mut().unwrap().chunks.truncate(1);
        state.sum.as_mut().unwrap().cache.chunks.truncate(1);
        state.sum.as_mut().unwrap().raw.clear();
        state_delta_record(&mut actual, &state, &mut expected);
        state.stack.clear();
        state.catches.clear();
        state.period = None;
        state.sum = None;
        assert_eq!(
            state_delta_check(&actual.finish(), &expected),
            vec![255, 204, 128, 204]
        );
    }

    #[test]
    fn state_delta_period_cache_changes_preserve_every_field() {
        for cache in [false, true] {
            for field in 0..11 {
                let mut state = state_delta_fixture();
                let mut actual = RequestBatch::new_wire(Wire::Delta);
                let mut expected = Vec::new();
                state_delta_record(&mut actual, &state, &mut expected);
                let period = if cache {
                    &mut state.sum.as_mut().unwrap().cache
                } else {
                    state.period.as_mut().unwrap()
                };
                match field {
                    0 => period.seq = VIRTUAL_BASE + 32,
                    1 => period.return_pc += 1,
                    2 => period.len -= 1,
                    3 => period.p += 1,
                    4 => period.i += 1,
                    5 => period.need = BOOLEAN_CHUNK,
                    6 => period.chunks.first_mut().unwrap().base = BOOLEAN_CHUNK,
                    7 => {
                        *period
                            .chunks
                            .first_mut()
                            .unwrap()
                            .nibbles
                            .first_mut()
                            .unwrap() = 15
                    }
                    8 => period.chunks.push(Chunk {
                        base: BOOLEAN_CHUNK,
                        nibbles: vec![1],
                    }),
                    9 => period.chunks.clear(),
                    _ => period.chunks.first_mut().unwrap().nibbles.push(2),
                }
                state_delta_record(&mut actual, &state, &mut expected);
                assert_eq!(
                    state_delta_check(&actual.finish(), &expected),
                    vec![255, if cache { 128 } else { 64 }]
                );
            }
        }
    }

    #[test]
    fn state_delta_sum_bit_patterns_are_exact() {
        for bits in [
            0_u64,
            0x8000_0000_0000_0000,
            0x7ff8_0000_0000_0001,
            0x7ff8_0000_0000_0002,
            0x7ff0_0000_0000_0000,
            0xfff0_0000_0000_0000,
            1,
        ] {
            for field in 0..7 {
                let mut state = state_delta_fixture();
                let mut actual = RequestBatch::new_wire(Wire::Delta);
                let mut expected = Vec::new();
                state_delta_record(&mut actual, &state, &mut expected);
                let sum = state.sum.as_mut().unwrap();
                let slot = if field == 6 {
                    &mut sum.total
                } else {
                    sum.numbers.get_mut(field).unwrap()
                };
                let changed = slot.to_bits() != bits;
                *slot = f64::from_bits(bits);
                state_delta_record(&mut actual, &state, &mut expected);
                // Identical NaN payloads must emit mask0, not compare unequal.
                state_delta_record(&mut actual, &state, &mut expected);
                assert_eq!(
                    state_delta_check(&actual.finish(), &expected),
                    vec![255, if changed { 128 } else { 0 }, 0]
                );
            }
        }
    }

    #[test]
    fn state_delta_numeric_input_markers_and_legacy_terminal_bytes() {
        for env in [0, 1, VIRTUAL_BASE + 32, 9_007_199_254_740_991] {
            let old = call(
                11,
                &transport_parse(&format!(
                    r#"{{"function":0,"env":{env},"wire":"state-v1"}}"#
                )),
            )
            .unwrap();
            let delta = call(
                11,
                &transport_parse(&format!(
                    r#"{{"function":0,"env":{env},"wire":"state-v2d"}}"#
                )),
            )
            .unwrap();
            assert_eq!(state_delta_replay(&delta).0, transport_parse(&old));
        }
        for frame in [
            r#"{"value":0}"#,
            r#"{"value":-0}"#,
            r#"{"thrown":0}"#,
            r#"{"thrown":-0}"#,
        ] {
            let state = canonical_test_state();
            let old = canonical_test_body(&state, frame);
            let body = old.replace("state-v1", "state-v2d");
            assert_eq!(
                call(12, &transport_parse(&body)).unwrap(),
                call(12, &transport_parse(&old)).unwrap()
            );
            canonical_compare(body.as_bytes());
            if !frame.contains("-0") {
                assert!(try_canonical_resume(body.as_bytes()).is_some());
            }
            let escaped = body.replace("state-v2d", r"state-v\u0032d");
            assert!(try_canonical_resume(escaped.as_bytes()).is_none());
            assert_eq!(
                canonical_status(call(12, &transport_parse(&escaped))),
                canonical_status(call(12, &transport_parse(&body)))
            );
        }
        let invalid = r#"{"function":0,"env":"h1","wire":"state-v2d"}"#;
        assert!(matches!(
            call(11, &transport_parse(invalid)),
            Err(Refusal::Input)
        ));
        assert_eq!(end("done", 0, true), r#"{"wire":"state-v1","done":0}"#);
    }

    #[test]
    fn state_delta_special_modes_match_legacy_operations() {
        for mode in 3..=7 {
            let setup = || {
                let mut state = state_delta_fixture();
                state.pc = canonical_test_state().pc;
                state.mode = mode;
                state.stack = vec![1];
                state.catches.clear();
                state.period.as_mut().unwrap().chunks.clear();
                let cache = &mut state.sum.as_mut().unwrap().cache;
                cache.len = 4;
                cache.chunks.clear();
                state
            };
            let frame = match mode {
                3 | 6 => Some(transport_parse(r#"{"facts":{"safe":true,"len":4}}"#)),
                5 => Some(transport_parse(
                    r#"{"numbers":[{"kind":"null"},{"kind":"number","bits":"4010000000000000"},{"kind":"number","bits":"3ff0000000000000"},{"kind":"number","bits":"3ff0000000000000"},{"kind":"number","bits":"3ff0000000000000"},{"kind":"number","bits":"3ff0000000000000"},{"kind":"number","bits":"3ff0000000000000"},{"kind":"number","bits":"3ff0000000000000"}]}"#,
                )),
                _ => None,
            };
            let old = execute(setup(), frame.as_ref().map(ResumeFrame::Generic), true).unwrap();
            let delta = execute_wire(
                setup(),
                frame.as_ref().map(ResumeFrame::Generic),
                Wire::Delta,
            )
            .unwrap();
            assert_eq!(
                state_delta_replay(&delta).0,
                transport_parse(&old),
                "mode{mode}"
            );
            assert_eq!(
                state_delta_replay(&delta).1,
                vec![255],
                "single special request{mode}"
            );
        }
    }

    #[test]
    fn state_delta_order_and_shrink_regrow_match_frozen_rows() {
        let mut state = state_delta_fixture();
        for period in [
            state.period.as_mut().unwrap(),
            &mut state.sum.as_mut().unwrap().cache,
        ] {
            period.chunks.push(Chunk {
                base: BOOLEAN_CHUNK,
                nibbles: vec![4, 5, 6, 7],
            });
        }
        let mut actual = RequestBatch::new_wire(Wire::Delta);
        let mut expected = Vec::new();
        state_delta_record(&mut actual, &state, &mut expected);
        state.stack.swap(0, 2);
        state.catches.swap(0, 1);
        state_delta_record(&mut actual, &state, &mut expected);
        state.stack.truncate(1);
        state.catches.truncate(1);
        state_delta_record(&mut actual, &state, &mut expected);
        state.stack.extend_from_slice(&[1, VIRTUAL_BASE + 32]);
        state.catches.push((1, 1, 17));
        state_delta_record(&mut actual, &state, &mut expected);
        for period in [
            state.period.as_mut().unwrap(),
            &mut state.sum.as_mut().unwrap().cache,
        ] {
            period.chunks.reverse();
        }
        state_delta_record(&mut actual, &state, &mut expected);
        for period in [
            state.period.as_mut().unwrap(),
            &mut state.sum.as_mut().unwrap().cache,
        ] {
            period.chunks.first_mut().unwrap().nibbles.truncate(1);
        }
        state_delta_record(&mut actual, &state, &mut expected);
        for period in [
            state.period.as_mut().unwrap(),
            &mut state.sum.as_mut().unwrap().cache,
        ] {
            period
                .chunks
                .first_mut()
                .unwrap()
                .nibbles
                .extend_from_slice(&[8, 9, 10]);
        }
        state_delta_record(&mut actual, &state, &mut expected);
        for period in [
            state.period.as_mut().unwrap(),
            &mut state.sum.as_mut().unwrap().cache,
        ] {
            period.chunks.pop();
        }
        state_delta_record(&mut actual, &state, &mut expected);
        for period in [
            state.period.as_mut().unwrap(),
            &mut state.sum.as_mut().unwrap().cache,
        ] {
            period.chunks.push(Chunk {
                base: 0,
                nibbles: vec![1, 2, 3, 4],
            });
        }
        state_delta_record(&mut actual, &state, &mut expected);
        assert_eq!(
            state_delta_check(&actual.finish(), &expected),
            vec![255, 12, 12, 12, 192, 192, 192, 192, 192]
        );
    }
}
