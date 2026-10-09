//! noevia-core's `server/llamacpp-presets.cjs` validation in Rust, exported from `dav-parse.wasm`
//! (LLAMACPP_PRESETS_IMPL): which llama.cpp preset options the web may write and in which range.
//! Preset values end up on llama-server's command line, and an oversized one can livelock the
//! host (noevia#697, #1132), so the host only ever uses this port to refuse more.
//!
//! - [`canonical`]: `canonical(key)`, a field name or one of its aliases to the field name.
//! - [`valid`]: `fields[key].valid(value)`, the field's pattern and range.
//! - [`checked_value`]: one entry of `prepare()`'s option loop: the cache-ram clamp
//!   (inference-budget.cjs `clampCacheRam`), then `Object.hasOwn(fields, key)` and the value
//!   check; the value as it would be written, or `None` where the write is refused.
//! - [`canonical_option`]: one entry of `canonicalOptions()`: the trimmed, dash-stripped key's
//!   field and the trimmed value.
//! - [`micro_batch_exceeds`]: `Number(ubatch) > Number(batch)`, the micro-batch check, with
//!   ECMAScript `Number()` string conversion ([`to_number`]).
//! - [`model_name_ok`]: the model name pattern, `[\w./:-]` 1 to 200 times.
//!
//! Deliberately stricter than the JS (the host then refuses the option; see the fixture table's
//! `strict` rows): a cache-ram clamp whose hard maximum is not a safe non-negative integer, a
//! `0x`/`0o`/`0b` number wider than 53 bits in the micro-batch check (both unknown: the host
//! refuses), and a model name that is not a string (the host does not ask; the JS coerces it).
//!
//! `String.prototype.trim` and `Number()`'s white space are the ECMAScript WhiteSpace and
//! LineTerminator code points, a fixed list ([`is_js_space`]); everything else here is ASCII.
//! Linear: every loop is charged to a work budget proportional to the request size, no panics.

#![forbid(unsafe_code)]

use prompt_framing::json::{self, Value};
use std::cmp::Ordering;

/// The largest request [`call`] accepts (the op byte and the JSON).
pub const MAX_INPUT_BYTES: usize = 4 * 1024 * 1024 + 1;
/// Work units a request may use per byte of its size (plus [`WORK_FLOOR`]).
pub const WORK_PER_BYTE: u64 = 16;
/// Work units every request may use, whatever its size.
pub const WORK_FLOOR: u64 = 1 << 16;
/// `Number.MAX_SAFE_INTEGER`.
pub const MAX_SAFE_INTEGER: u64 = (1 << 53) - 1;

const JSON_CAP: usize = 4;

/// Why the port gives no answer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refusal {
    Input,
    TooLarge,
}

impl Refusal {
    /// The refusal reply.
    pub fn json(self) -> &'static str {
        match self {
            Refusal::Input => r#"{"error":"input"}"#,
            Refusal::TooLarge => r#"{"error":"too_large"}"#,
        }
    }
}

type R<T> = Result<T, Refusal>;

/// Work left for one request.
#[derive(Debug)]
pub struct Work {
    left: u64,
}

impl Work {
    /// A budget of `units`.
    pub fn new(units: u64) -> Self {
        Work { left: units }
    }

    /// The budget for a request of `bytes` bytes.
    pub fn for_request(bytes: usize) -> Self {
        let bytes = u64::try_from(bytes).unwrap_or(u64::MAX);
        Work::new(
            bytes
                .saturating_mul(WORK_PER_BYTE)
                .saturating_add(WORK_FLOOR),
        )
    }

    /// Charge `n` units (and one for the step itself).
    pub fn charge(&mut self, n: usize) -> R<()> {
        let n = u64::try_from(n).unwrap_or(u64::MAX).saturating_add(1);
        if n > self.left {
            self.left = 0;
            return Err(Refusal::TooLarge);
        }
        self.left -= n;
        Ok(())
    }
}

/// A field's value rule.
#[derive(Clone, Copy, Debug)]
enum Rule {
    /// `/^\d+$/` and `min <= Number(v) <= max`.
    Integer(u64, u64),
    /// [`Rule::Integer`] or one of the words.
    IntegerOr(u64, u64, &'static [&'static str]),
    /// `/^\d+(\.\d{1,3})?$/` and `min <= Number(v) <= max`.
    Decimal(u64, u64),
    /// One of the words.
    Choice(&'static [&'static str]),
    /// `/^(0(\.\d{1,3})?|1(\.0{1,3})?)$/`.
    Probability,
}

/// One allowlisted option: its canonical name, aliases and rule.
#[derive(Clone, Copy, Debug)]
pub struct Field {
    /// The canonical name (the key `prepare()` takes and writes).
    pub name: &'static str,
    /// Other spellings `canonical()` folds in.
    pub aliases: &'static [&'static str],
    rule: Rule,
}

const CACHE_TYPES: &[&str] = &[
    "f32", "f16", "bf16", "q8_0", "q4_0", "q4_1", "iq4_nl", "q5_0", "q5_1",
];

/// llamacpp-presets.cjs `fields`, in its order.
pub const FIELDS: &[Field] = &[
    Field {
        name: "ctx-size",
        aliases: &["c", "LLAMA_ARG_CTX_SIZE"],
        rule: Rule::Integer(2048, 1_048_576),
    },
    Field {
        name: "parallel",
        aliases: &["np", "LLAMA_ARG_N_PARALLEL"],
        rule: Rule::Integer(1, 16),
    },
    Field {
        name: "n-gpu-layers",
        aliases: &["ngl", "gpu-layers", "LLAMA_ARG_N_GPU_LAYERS"],
        rule: Rule::IntegerOr(0, 999, &["auto", "all"]),
    },
    Field {
        name: "cache-type-k",
        aliases: &["ctk", "LLAMA_ARG_CACHE_TYPE_K"],
        rule: Rule::Choice(CACHE_TYPES),
    },
    Field {
        name: "cache-type-v",
        aliases: &["ctv", "LLAMA_ARG_CACHE_TYPE_V"],
        rule: Rule::Choice(CACHE_TYPES),
    },
    Field {
        name: "flash-attn",
        aliases: &["fa", "LLAMA_ARG_FLASH_ATTN"],
        rule: Rule::Choice(&["on", "off", "auto"]),
    },
    Field {
        name: "batch-size",
        aliases: &["b", "LLAMA_ARG_BATCH"],
        rule: Rule::Integer(32, 8192),
    },
    Field {
        name: "ubatch-size",
        aliases: &["ub", "LLAMA_ARG_UBATCH"],
        rule: Rule::Integer(32, 8192),
    },
    Field {
        name: "cache-ram",
        aliases: &["cram", "LLAMA_ARG_CACHE_RAM"],
        rule: Rule::Integer(0, 1_048_576),
    },
    Field {
        name: "image-max-tokens",
        aliases: &["LLAMA_ARG_IMAGE_MAX_TOKENS"],
        rule: Rule::Integer(64, 16384),
    },
    Field {
        name: "spec-type",
        aliases: &["LLAMA_ARG_SPEC_TYPE"],
        rule: Rule::Choice(&[
            "none",
            "draft-mtp",
            "ngram-simple",
            "draft-mtp,ngram-simple",
        ]),
    },
    Field {
        name: "spec-draft-n-max",
        aliases: &["LLAMA_ARG_SPEC_DRAFT_N_MAX"],
        rule: Rule::Integer(1, 32),
    },
    Field {
        name: "spec-draft-p-min",
        aliases: &["LLAMA_ARG_SPEC_DRAFT_P_MIN"],
        rule: Rule::Probability,
    },
    Field {
        name: "temp",
        aliases: &[],
        rule: Rule::Decimal(0, 2),
    },
    Field {
        name: "top-p",
        aliases: &[],
        rule: Rule::Decimal(0, 1),
    },
    Field {
        name: "top-k",
        aliases: &["LLAMA_ARG_TOP_K"],
        rule: Rule::Integer(0, 100_000),
    },
    Field {
        name: "min-p",
        aliases: &[],
        rule: Rule::Decimal(0, 1),
    },
    Field {
        name: "repeat-penalty",
        aliases: &[],
        rule: Rule::Decimal(0, 3),
    },
];

fn eq(units: &[u16], s: &str) -> bool {
    units.iter().copied().eq(s.encode_utf16())
}

/// `Object.hasOwn(fields, key)`: the field named exactly `key` (aliases are not field names).
pub fn field(key: &[u16]) -> Option<&'static Field> {
    FIELDS.iter().find(|f| eq(key, f.name))
}

/// `canonical(key)`: the first field (in `fields` order) named `key` or with `key` as an alias.
pub fn canonical(key: &[u16]) -> Option<&'static Field> {
    FIELDS
        .iter()
        .find(|f| eq(key, f.name) || f.aliases.iter().any(|a| eq(key, a)))
}

const fn is_digit(c: u16) -> bool {
    matches!(c, 0x30..=0x39)
}

/// The value of an ASCII digit string compared with `n`, exactly (leading zeros allowed).
fn digits_cmp(d: &[u16], n: u64) -> Ordering {
    // All zeros compare as "0" (`d` is never empty here).
    let start = d
        .iter()
        .position(|&c| c != 0x30)
        .unwrap_or(d.len().saturating_sub(1));
    let d = d.get(start..).unwrap_or(&[]);
    let want = n.to_string();
    d.len()
        .cmp(&want.len())
        .then_with(|| d.iter().copied().cmp(want.encode_utf16()))
}

fn integer_ok(v: &[u16], min: u64, max: u64) -> bool {
    !v.is_empty()
        && v.iter().all(|&c| is_digit(c))
        && digits_cmp(v, min) != Ordering::Less
        && digits_cmp(v, max) != Ordering::Greater
}

/// `/^\d+(\.\d{1,3})?$/` split into the integer digits and the (0-3) fraction digits.
fn decimal_parts(v: &[u16]) -> Option<(&[u16], &[u16])> {
    let dot = v.iter().position(|&c| c == 0x2e);
    let (int, frac) = match dot {
        None => (v, &[][..]),
        Some(i) => {
            let frac = v.get(i + 1..)?;
            if !(1..=3).contains(&frac.len()) {
                return None;
            }
            (v.get(..i)?, frac)
        }
    };
    if int.is_empty() || !int.iter().all(|&c| is_digit(c)) || !frac.iter().all(|&c| is_digit(c)) {
        return None;
    }
    Some((int, frac))
}

/// `decimal(min, max)`: the bounds are integers and the fraction has at most three digits, so
/// `Number(v)` compares as the exact decimal does (every such value rounds on its own side of an
/// integer bound).
fn decimal_ok(v: &[u16], min: u64, max: u64) -> bool {
    let Some((int, frac)) = decimal_parts(v) else {
        return false;
    };
    let zero_frac = frac.iter().all(|&c| c == 0x30);
    digits_cmp(int, min) != Ordering::Less
        && match digits_cmp(int, max) {
            Ordering::Less => true,
            Ordering::Equal => zero_frac,
            Ordering::Greater => false,
        }
}

fn probability_ok(v: &[u16]) -> bool {
    let Some((&first, rest)) = v.split_first() else {
        return false;
    };
    let Some((&dot, frac)) = rest.split_first() else {
        return first == 0x30 || first == 0x31;
    };
    dot == 0x2e
        && (1..=3).contains(&frac.len())
        && match first {
            0x30 => frac.iter().all(|&c| is_digit(c)),
            0x31 => frac.iter().all(|&c| c == 0x30),
            _ => false,
        }
}

/// `fields[f].valid(value)`.
pub fn valid(f: &Field, v: &[u16]) -> bool {
    match f.rule {
        Rule::Integer(min, max) => integer_ok(v, min, max),
        Rule::IntegerOr(min, max, words) => {
            integer_ok(v, min, max) || words.iter().any(|w| eq(v, w))
        }
        Rule::Decimal(min, max) => decimal_ok(v, min, max),
        Rule::Choice(words) => words.iter().any(|w| eq(v, w)),
        Rule::Probability => probability_ok(v),
    }
}

/// ECMAScript WhiteSpace or LineTerminator (what `String.prototype.trim` and `Number()` strip):
/// TAB, VT, FF, SP, NBSP, ZWNBSP, the Unicode Zs code points, LF, CR, LS and PS.
pub const fn is_js_space(c: u16) -> bool {
    matches!(
        c,
        0x09 | 0x0a | 0x0b | 0x0c | 0x0d | 0x20 | 0xa0 | 0x1680 | 0x2000
            ..=0x200a | 0x2028 | 0x2029 | 0x202f | 0x205f | 0x3000 | 0xfeff
    )
}

/// `String.prototype.trim`.
pub fn trim(s: &[u16]) -> &[u16] {
    let start = s.iter().position(|&c| !is_js_space(c)).unwrap_or(s.len());
    let end = s
        .iter()
        .rposition(|&c| !is_js_space(c))
        .map_or(start, |i| i + 1);
    s.get(start..end).unwrap_or(&[])
}

/// inference-budget.cjs `clampCacheRam(value, { hardMaxMib })` for a hard maximum that is a safe
/// non-negative integer: an integer below zero or above the maximum becomes the maximum, another
/// integer its plain decimal form (`String(Number(text))`), anything else the trimmed text.
pub fn clamp_cache_ram(value: &[u16], hard_max: u64) -> Vec<u16> {
    let text = trim(value);
    let (negative, digits) = match text.split_first() {
        Some((0x2d, rest)) => (true, rest),
        _ => (false, text),
    };
    if digits.is_empty() || !digits.iter().all(|&c| is_digit(c)) {
        return text.to_vec();
    }
    let zero = digits.iter().all(|&c| c == 0x30);
    // Number('-0') is -0: neither below zero nor above the maximum, and String(-0) is '0'.
    if (negative && !zero) || digits_cmp(digits, hard_max) == Ordering::Greater {
        return hard_max.to_string().encode_utf16().collect();
    }
    let start = digits
        .iter()
        .position(|&c| c != 0x30)
        .unwrap_or(digits.len().saturating_sub(1));
    digits.get(start..).unwrap_or(&[0x30]).to_vec()
}

/// One entry of `prepare()`'s option loop: `value` as it would be written, or `None` where the JS
/// refuses it (or, for cache-ram without a usable hard maximum, the port cannot tell).
pub fn checked_value(key: &[u16], value: &[u16], hard_max: Option<u64>) -> Option<Vec<u16>> {
    let f = field(key)?;
    let v = if f.name == "cache-ram" && !value.is_empty() {
        clamp_cache_ram(value, hard_max?)
    } else {
        value.to_vec()
    };
    (v.is_empty() || valid(f, &v)).then_some(v)
}

fn strip_dashes(s: &[u16]) -> &[u16] {
    let start = s.iter().position(|&c| c != 0x2d).unwrap_or(s.len());
    s.get(start..).unwrap_or(&[])
}

/// One entry of `canonicalOptions()`: the field of `String(key).trim().replace(/^-+/, '')` and
/// `String(value).trim()`.
pub fn canonical_option(
    key: &[u16],
    value: Option<&[u16]>,
) -> (Option<&'static Field>, Option<Vec<u16>>) {
    (
        canonical(strip_dashes(trim(key))),
        value.map(|v| trim(v).to_vec()),
    )
}

/// `Number(text)` for a string, or `None` where the port does not decide it exactly (a
/// `0x`/`0o`/`0b` literal wider than 53 bits).
pub fn to_number(s: &[u16]) -> Option<f64> {
    let t = trim(s);
    if t.is_empty() {
        return Some(0.0);
    }
    if let [0x30, p, rest @ ..] = t {
        let radix = match p | 0x20 {
            0x78 => Some(16),
            0x6f => Some(8),
            0x62 => Some(2),
            _ => None,
        };
        if let Some(radix) = radix {
            return non_decimal(rest, radix);
        }
    }
    let (negative, body) = match t.split_first() {
        Some((0x2d, rest)) => (true, rest),
        Some((0x2b, rest)) => (false, rest),
        _ => (false, t),
    };
    let magnitude = if eq(body, "Infinity") {
        f64::INFINITY
    } else if decimal_literal(body) {
        // ASCII only (checked); Rust's parse is correctly rounded, as V8's StringToDouble is.
        let ascii: String = body.iter().map(|&c| char::from(c as u8)).collect();
        ascii.parse::<f64>().unwrap_or(f64::NAN)
    } else {
        f64::NAN
    };
    Some(if negative { -magnitude } else { magnitude })
}

/// `0x`/`0o`/`0b` digits (no sign, no separators): exact up to 53 significant bits.
fn non_decimal(digits: &[u16], radix: u32) -> Option<f64> {
    if digits.is_empty() {
        return Some(f64::NAN);
    }
    let mut n: u64 = 0;
    for &c in digits {
        let Some(d) = char::from_u32(u32::from(c)).and_then(|ch| ch.to_digit(radix)) else {
            return Some(f64::NAN);
        };
        n = n.checked_mul(u64::from(radix))?.checked_add(u64::from(d))?;
        if n > MAX_SAFE_INTEGER {
            return None;
        }
    }
    // n <= 2^53 - 1: exact as a double.
    #[allow(clippy::cast_precision_loss)]
    Some(n as f64)
}

/// StrUnsignedDecimalLiteral without `Infinity`: `1`, `1.`, `1.5`, `.5`, each with an optional
/// `e`/`E`, sign and digits.
fn decimal_literal(s: &[u16]) -> bool {
    let int = s.iter().take_while(|&&c| is_digit(c)).count();
    let mut rest = s.get(int..).unwrap_or(&[]);
    let mut frac = 0;
    if let Some((0x2e, after)) = rest.split_first() {
        frac = after.iter().take_while(|&&c| is_digit(c)).count();
        rest = after.get(frac..).unwrap_or(&[]);
    }
    if int == 0 && frac == 0 {
        return false;
    }
    match rest.split_first() {
        None => true,
        Some((0x65 | 0x45, exp)) => {
            let exp = match exp.split_first() {
                Some((0x2b | 0x2d, e)) => e,
                _ => exp,
            };
            !exp.is_empty() && exp.iter().all(|&c| is_digit(c))
        }
        Some(_) => false,
    }
}

/// `Number(ubatch) > Number(batch)` (`None` stands for `undefined`), or `None` where the port
/// does not decide one of them.
pub fn micro_batch_exceeds(ubatch: Option<&[u16]>, batch: Option<&[u16]>) -> Option<bool> {
    let n = |v: Option<&[u16]>| v.map_or(Some(f64::NAN), to_number);
    Some(n(ubatch)? > n(batch)?)
}

/// `/^[\w./:-]{1,200}$/.test(model)` for a string model.
pub fn model_name_ok(model: &[u16]) -> bool {
    (1..=200).contains(&model.len())
        && model.iter().all(|&c| {
            matches!(c, 0x30..=0x39 | 0x41..=0x5a | 0x61..=0x7a | 0x5f | 0x2e | 0x2f | 0x3a | 0x2d)
        })
}

fn string(v: &Value) -> R<&[u16]> {
    v.as_str().ok_or(Refusal::Input)
}

fn opt_string(v: &Value) -> R<Option<&[u16]>> {
    match v {
        Value::Null => Ok(None),
        Value::Str(s) => Ok(Some(s)),
        _ => Err(Refusal::Input),
    }
}

fn pairs(v: Option<&Value>) -> R<&[Value]> {
    match v {
        Some(Value::Arr(items)) => Ok(items),
        _ => Err(Refusal::Input),
    }
}

fn pair(v: &Value) -> R<(&Value, &Value)> {
    match v {
        Value::Arr(p) => match p.as_slice() {
            [k, v] => Ok((k, v)),
            _ => Err(Refusal::Input),
        },
        _ => Err(Refusal::Input),
    }
}

/// A hard maximum the clamp is decided for: a safe non-negative integer.
fn hard_max(v: Option<&Value>) -> R<Option<u64>> {
    match v {
        Some(Value::Null) => Ok(None),
        Some(Value::Num(n)) => {
            #[allow(clippy::cast_precision_loss)]
            let safe =
                n.is_finite() && n.fract() == 0.0 && *n >= 0.0 && *n <= MAX_SAFE_INTEGER as f64;
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            Ok(safe.then_some(*n as u64))
        }
        _ => Err(Refusal::Input),
    }
}

fn push_opt(out: &mut Vec<u8>, v: Option<&[u16]>) {
    match v {
        Some(s) => json::push_str(out, s),
        None => out.extend_from_slice(b"null"),
    }
}

fn run(op: u8, args: &[Value], work: &mut Work) -> R<Vec<u8>> {
    let mut out = Vec::new();
    match op {
        1 => {
            let [list, max] = args else {
                return Err(Refusal::Input);
            };
            let max = hard_max(Some(max))?;
            out.extend_from_slice(b"{\"values\":[");
            for (i, p) in pairs(Some(list))?.iter().enumerate() {
                let (k, v) = pair(p)?;
                let (k, v) = (string(k)?, string(v)?);
                work.charge(k.len().saturating_add(v.len()).saturating_mul(4))?;
                if i > 0 {
                    out.push(b',');
                }
                push_opt(&mut out, checked_value(k, v, max).as_deref());
            }
            out.extend_from_slice(b"]}");
        }
        2 => {
            let [list] = args else {
                return Err(Refusal::Input);
            };
            out.extend_from_slice(b"{\"options\":[");
            for (i, p) in pairs(Some(list))?.iter().enumerate() {
                let (k, v) = pair(p)?;
                let (k, v) = (string(k)?, opt_string(v)?);
                work.charge(
                    k.len()
                        .saturating_add(v.map_or(0, <[u16]>::len))
                        .saturating_mul(4),
                )?;
                if i > 0 {
                    out.push(b',');
                }
                let (f, v) = canonical_option(k, v);
                out.push(b'[');
                match f {
                    Some(f) => json::push_ascii(&mut out, f.name),
                    None => out.extend_from_slice(b"null"),
                }
                out.push(b',');
                push_opt(&mut out, v.as_deref());
                out.push(b']');
            }
            out.extend_from_slice(b"]}");
        }
        3 => {
            let [u, b] = args else {
                return Err(Refusal::Input);
            };
            let (u, b) = (opt_string(u)?, opt_string(b)?);
            work.charge(
                u.map_or(0, <[u16]>::len)
                    .saturating_add(b.map_or(0, <[u16]>::len))
                    .saturating_mul(4),
            )?;
            out.extend_from_slice(match micro_batch_exceeds(u, b) {
                Some(true) => b"{\"exceeds\":true}",
                Some(false) => b"{\"exceeds\":false}",
                None => b"{\"exceeds\":null}",
            });
        }
        4 => {
            let [m] = args else {
                return Err(Refusal::Input);
            };
            let m = string(m)?;
            work.charge(m.len())?;
            out.extend_from_slice(if model_name_ok(m) {
                b"{\"ok\":true}"
            } else {
                b"{\"ok\":false}"
            });
        }
        _ => return Err(Refusal::Input),
    }
    Ok(out)
}

/// One request: `u8(op)` and a UTF-8 JSON array of arguments.
///
/// - op 1 `[[[key, value]...], hardMaxMib|null]` → `{"values":[value|null...]}` (prepare's loop:
///   each value as it would be written, `null` where refused);
/// - op 2 `[[[key, value|null]...]]` → `{"options":[[field|null, trimmed|null]...]}`
///   (canonicalOptions per entry; the host keeps the later of equal fields);
/// - op 3 `[ubatch|null, batch|null]` → `{"exceeds":true|false|null}` (`null`: undecided);
/// - op 4 `[model]` → `{"ok":bool}`.
///
/// Status 0 and the reply, or status 1 and `{"error":"input"|"too_large"}`.
pub fn call(input: &[u8]) -> (u32, Vec<u8>) {
    let refuse = |r: Refusal| (1, r.json().as_bytes().to_vec());
    if input.len() > MAX_INPUT_BYTES {
        return refuse(Refusal::TooLarge);
    }
    let Some((&op, body)) = input.split_first() else {
        return refuse(Refusal::Input);
    };
    let mut work = Work::for_request(input.len());
    let Some(Value::Arr(args)) = json::parse_utf8(body, JSON_CAP) else {
        return refuse(Refusal::Input);
    };
    match run(op, &args, &mut work) {
        Ok(out) => (0, out),
        Err(e) => refuse(e),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use prompt_framing::js::units;

    fn reply(op: u8, json: &str) -> String {
        let mut input = vec![op];
        input.extend(json.as_bytes());
        let (status, out) = call(&input);
        assert_eq!(status, 0, "{}", String::from_utf8_lossy(&out));
        String::from_utf8(out).unwrap()
    }

    #[test]
    fn names_and_aliases_are_unique() {
        let mut all: Vec<&str> = FIELDS
            .iter()
            .flat_map(|f| std::iter::once(f.name).chain(f.aliases.iter().copied()))
            .collect();
        let n = all.len();
        all.sort_unstable();
        all.dedup();
        assert_eq!(all.len(), n);
    }

    #[test]
    fn prepare_values() {
        assert_eq!(
            reply(
                1,
                r#"[[["ctx-size","32768"],["c","4096"],["cache-ram"," -1 "],["temp","2.000"],["temp","2.001"],["parallel",""]],2048]"#
            ),
            r#"{"values":["32768",null,"2048","2.000",null,""]}"#
        );
        assert_eq!(
            reply(1, r#"[[["cache-ram","0004"]],null]"#),
            r#"{"values":[null]}"#
        );
        assert_eq!(
            reply(1, r#"[[["cache-ram","-0"]],1.5]"#),
            r#"{"values":[null]}"#
        );
        assert_eq!(
            reply(1, r#"[[["cache-ram","-0"],["cache-ram","0004"]],2048]"#),
            r#"{"values":["0","4"]}"#
        );
    }

    #[test]
    fn numbers() {
        let n = |s: &str| to_number(&units(s));
        assert_eq!(n(""), Some(0.0));
        assert_eq!(n(" \u{a0}+12e1\u{2028}"), Some(120.0));
        assert_eq!(n("0x10"), Some(16.0));
        assert!(n("-0x10").unwrap().is_nan());
        assert!(n("1_0").unwrap().is_nan());
        assert!(n("infinity").unwrap().is_nan());
        assert_eq!(n("-Infinity"), Some(f64::NEG_INFINITY));
        assert_eq!(n(".5"), Some(0.5));
        assert_eq!(n("5."), Some(5.0));
        assert!(n(".").unwrap().is_nan());
        assert!(n("1e").unwrap().is_nan());
        assert_eq!(n("0x20000000000000"), None);
        assert_eq!(n("0x1fffffffffffff"), Some(9_007_199_254_740_991.0));
        assert!(n("0b").unwrap().is_nan());
        assert!(n("0x1g").unwrap().is_nan());
        assert_eq!(reply(3, r#"["64",null]"#), r#"{"exceeds":false}"#);
        assert_eq!(reply(3, r#"["0x41","64"]"#), r#"{"exceeds":true}"#);
        assert_eq!(
            reply(3, r#"["0x41ffffffffffffff","64"]"#),
            r#"{"exceeds":null}"#
        );
    }

    #[test]
    fn canonical_options() {
        assert_eq!(
            reply(2, r#"[[["  --cram ","　 512 "],["model","x"],["c",null]]]"#),
            r#"{"options":[["cache-ram","512"],[null,"x"],["ctx-size",null]]}"#
        );
    }

    #[test]
    fn model_names() {
        assert_eq!(reply(4, r#"["org/m-1.gguf:Q4"]"#), r#"{"ok":true}"#);
        assert_eq!(reply(4, r#"[""]"#), r#"{"ok":false}"#);
        assert_eq!(reply(4, r#"["a b"]"#), r#"{"ok":false}"#);
        let mut input = vec![4u8];
        input.extend(br#"[7]"#);
        assert_eq!(call(&input).0, 1);
    }

    #[test]
    fn refusals() {
        assert_eq!(call(&[]).0, 1);
        assert_eq!(call(&[9, b'[', b']']).0, 1);
        assert_eq!(call(&vec![1u8; MAX_INPUT_BYTES + 1]).0, 1);
    }
}
