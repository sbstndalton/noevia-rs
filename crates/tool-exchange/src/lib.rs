//! noevia-core's `server/tool-exchange.cjs` in Rust, exported from `dav-parse.wasm`
//! (TOOL_EXCHANGE_IMPL). The exchange itself (one instance per chat turn, the result cache, the
//! serial execution, the approval gate and audit around it) stays in the JS; this decides what the
//! JS decides before a tool runs, in the same order:
//!
//! 1. the turn was cancelled: `ERROR: exchange cancelled; tool was not run.`
//! 2. the tool is not enabled: `ERROR: tool "<name>" is not enabled for this project`
//! 3. the arguments are not JSON (`JSON.parse`, exactly): `ERROR: tool arguments were not valid
//!    JSON: <the first 200 code units>`; not a JSON object: `ERROR: tool arguments must be a JSON
//!    object.`; absent (the JS's falsy `call.args`): `{}`.
//! 4. otherwise the dedupe key `JSON.stringify([name, canonical(args)])`, where `canonical` writes
//!    arrays in order and object keys sorted by UTF-16 code units (`Array.prototype.sort`), a
//!    duplicate key keeping its last value, numbers as `JSON.stringify` writes them (`-0` as `0`,
//!    an overflowed literal as `null`) and strings escaped as `JSON.stringify` escapes them (lone
//!    surrogates as `\udxxx`).
//!
//! and the text a failed call leaves ([`call_error`]). Strings are UTF-16 code units throughout,
//! so lone surrogates and a cut through a surrogate pair come out as the JS's do.
//!
//! Stricter than the JS, as refusals (the host then answers with an error and never runs the
//! tool): arguments over [`MAX_ARGS_UNITS`] units, a name over [`MAX_NAME_UNITS`], and arguments
//! nested [`MAX_DEPTH`] or more containers deep (the JS's recursive `canonical` overflows V8's
//! stack somewhere past ~2,000 levels and rejects the call; under 1,024 both agree).

#![forbid(unsafe_code)]

use prompt_framing::json::{self, Value};

/// Code units of a tool call's arguments.
pub const MAX_ARGS_UNITS: usize = 4 * 1024 * 1024;
/// Code units of a tool name, and of the error text [`call_error`] reads.
pub const MAX_NAME_UNITS: usize = 65_536;
/// Containers this deep (the arguments object is depth 0) are refused.
pub const MAX_DEPTH: usize = 1024;
/// The largest request [`call`] accepts.
pub const MAX_INPUT_BYTES: usize = 1 + 2 + 4 + 2 * MAX_NAME_UNITS + 1 + 2 * MAX_ARGS_UNITS;

const CANCELLED: &str = "ERROR: exchange cancelled; tool was not run.";
const NOT_OBJECT: &str = "ERROR: tool arguments must be a JSON object.";

/// Why the port will not answer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refusal {
    /// The request is not in the expected shape.
    Input,
    /// Arguments, name or message over their caps.
    TooLarge,
    /// Arguments nested [`MAX_DEPTH`] deep or more.
    Depth,
}

impl Refusal {
    pub fn json(self) -> &'static str {
        match self {
            Refusal::Input => r#"{"error":"input"}"#,
            Refusal::TooLarge => r#"{"error":"too_large"}"#,
            Refusal::Depth => r#"{"error":"depth"}"#,
        }
    }
}

/// What the exchange does with a call.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Check {
    /// Run it (unless the key already has a result), deduplicated on this key.
    Run(Vec<u16>),
    /// Do not run it; this is the tool result.
    Answer(Vec<u16>),
}

fn units(s: &str) -> Vec<u16> {
    s.encode_utf16().collect()
}

/// Append `s` as `JSON.stringify` quotes a string.
pub fn quote(out: &mut Vec<u16>, s: &[u16]) {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let esc = |out: &mut Vec<u16>, u: u16| {
        out.extend(units("\\u"));
        for shift in [12u16, 8, 4, 0] {
            let d = HEX
                .get(usize::from((u >> shift) & 0xf))
                .copied()
                .unwrap_or(b'0');
            out.push(u16::from(d));
        }
    };
    out.push(0x22);
    let mut i = 0;
    while let Some(&u) = s.get(i) {
        match u {
            0x22 => out.extend(units("\\\"")),
            0x5c => out.extend(units("\\\\")),
            0x08 => out.extend(units("\\b")),
            0x0c => out.extend(units("\\f")),
            0x0a => out.extend(units("\\n")),
            0x0d => out.extend(units("\\r")),
            0x09 => out.extend(units("\\t")),
            0x00..=0x1f => esc(out, u),
            0xd800..=0xdbff => {
                if let Some(&lo) = s.get(i + 1).filter(|n| (0xdc00..=0xdfff).contains(*n)) {
                    out.push(u);
                    out.push(lo);
                    i += 1;
                } else {
                    esc(out, u);
                }
            }
            0xdc00..=0xdfff => esc(out, u),
            _ => out.push(u),
        }
        i += 1;
    }
    out.push(0x22);
}

/// A finite number as `Number.prototype.toString()` writes it (`-0` as `0`); the same algorithm
/// as noevia-rs `gguf::node::js_number`.
pub fn js_number(v: f64) -> String {
    if v == 0.0 {
        return "0".into();
    }
    let sign = if v < 0.0 { "-" } else { "" };
    // `{:e}` gives the shortest digit count k that round-trips; ECMAScript then takes the k-digit
    // value closest to v, which is v correctly rounded to k digits.
    let shortest = format!("{:e}", v.abs());
    let k = shortest
        .split('e')
        .next()
        .map_or(1, |m| m.chars().filter(char::is_ascii_digit).count());
    let e_form = format!("{:.*e}", k.saturating_sub(1), v.abs());
    let (mant, exp) = e_form.split_once('e').unwrap_or((e_form.as_str(), "0"));
    let digits: String = mant.chars().filter(|c| *c != '.').collect();
    let exp: i64 = exp.parse().unwrap_or(0);
    let k = digits.len() as i64;
    let n = exp + 1;
    let body = if k <= n && n <= 21 {
        let mut s = digits.clone();
        s.extend(std::iter::repeat_n('0', (n - k) as usize));
        s
    } else if 0 < n && n <= 21 {
        let (a, b) = digits.split_at(n as usize);
        format!("{a}.{b}")
    } else if -6 < n && n <= 0 {
        format!("0.{}{}", "0".repeat((-n) as usize), digits)
    } else {
        let e = n - 1;
        let es = if e < 0 {
            format!("-{}", -e)
        } else {
            format!("+{e}")
        };
        let (first, rest) = digits.split_at(1);
        if rest.is_empty() {
            format!("{first}e{es}")
        } else {
            format!("{first}.{rest}e{es}")
        }
    };
    format!("{sign}{body}")
}

/// `canonical(value)` of tool-exchange.cjs, appended to `out`.
pub fn canonical(out: &mut Vec<u16>, v: &Value) -> Result<(), Refusal> {
    match v {
        Value::Null => out.extend(units("null")),
        Value::Bool(b) => out.extend(units(if *b { "true" } else { "false" })),
        Value::Num(n) if n.is_finite() => out.extend(units(&js_number(*n))),
        Value::Num(_) => out.extend(units("null")),
        Value::Str(s) => quote(out, s),
        Value::Arr(items) => {
            out.push(0x5b);
            for (i, x) in items.iter().enumerate() {
                if i > 0 {
                    out.push(0x2c);
                }
                canonical(out, x)?;
            }
            out.push(0x5d);
        }
        Value::Obj(members) => {
            let mut sorted: Vec<&(Vec<u16>, Value)> = members.iter().collect();
            // Code-unit order, as `Array.prototype.sort` compares strings. Keys are unique.
            sorted.sort_by(|a, b| a.0.cmp(&b.0));
            out.push(0x7b);
            for (i, (k, x)) in sorted.into_iter().enumerate() {
                if i > 0 {
                    out.push(0x2c);
                }
                quote(out, k);
                out.push(0x3a);
                canonical(out, x)?;
            }
            out.push(0x7d);
        }
        Value::Deep => return Err(Refusal::Depth),
    }
    Ok(())
}

/// The pre-run checks of `run(call, execute)`. `args` is `None` when the JS's `call.args` is
/// falsy, else `String(call.args)`.
pub fn check(
    aborted: bool,
    allowed: bool,
    name: &[u16],
    args: Option<&[u16]>,
) -> Result<Check, Refusal> {
    if name.len() > MAX_NAME_UNITS || args.is_some_and(|a| a.len() > MAX_ARGS_UNITS) {
        return Err(Refusal::TooLarge);
    }
    if aborted {
        return Ok(Check::Answer(units(CANCELLED)));
    }
    if !allowed {
        let mut msg = units("ERROR: tool \"");
        msg.extend_from_slice(name);
        msg.extend(units("\" is not enabled for this project"));
        return Ok(Check::Answer(msg));
    }
    let mut canon = Vec::new();
    match args {
        None => canon.extend(units("{}")),
        Some(text) => {
            let Some(value) = json::parse(text, MAX_DEPTH) else {
                let mut msg = units("ERROR: tool arguments were not valid JSON: ");
                msg.extend_from_slice(text.get(..200).unwrap_or(text));
                return Ok(Check::Answer(msg));
            };
            if !matches!(value, Value::Obj(_) | Value::Deep) {
                return Ok(Check::Answer(units(NOT_OBJECT)));
            }
            canonical(&mut canon, &value)?;
        }
    }
    let mut key = vec![0x5b];
    quote(&mut key, name);
    key.push(0x2c);
    quote(&mut key, &canon);
    key.push(0x5d);
    Ok(Check::Run(key))
}

/// `` `ERROR calling ${call.name}: ${String(err?.message || err).slice(0, 300)}` ``; `message` is
/// the coerced text (a host may pass only its first [`MAX_NAME_UNITS`] units).
pub fn call_error(name: &[u16], message: &[u16]) -> Result<Vec<u16>, Refusal> {
    if name.len() > MAX_NAME_UNITS || message.len() > MAX_NAME_UNITS {
        return Err(Refusal::TooLarge);
    }
    let mut out = units("ERROR calling ");
    out.extend_from_slice(name);
    out.extend(units(": "));
    out.extend_from_slice(message.get(..300).unwrap_or(message));
    Ok(out)
}

fn le_units(b: &[u8]) -> Option<Vec<u16>> {
    if !b.len().is_multiple_of(2) {
        return None;
    }
    Some(
        b.as_chunks::<2>()
            .0
            .iter()
            .map(|c| u16::from_le_bytes(*c))
            .collect(),
    )
}

/// `u32le(n)` and n UTF-16LE units at the front of `b`; the rest. Refuses `n` over `cap`.
fn framed(b: &[u8], cap: usize) -> Result<(Vec<u16>, &[u8]), Refusal> {
    let head: [u8; 4] = b
        .get(..4)
        .and_then(|h| h.try_into().ok())
        .ok_or(Refusal::Input)?;
    let n = u32::from_le_bytes(head) as usize;
    if n > cap {
        return Err(Refusal::TooLarge);
    }
    let end = 4 + 2 * n;
    let text = le_units(b.get(4..end).ok_or(Refusal::Input)?).ok_or(Refusal::Input)?;
    Ok((text, b.get(end..).unwrap_or(&[])))
}

fn flag(b: Option<&u8>) -> Result<bool, Refusal> {
    match b {
        Some(0) => Ok(false),
        Some(1) => Ok(true),
        _ => Err(Refusal::Input),
    }
}

fn reply(tag: u8, text: &[u16]) -> Vec<u8> {
    let mut out = Vec::with_capacity(1 + 2 * text.len());
    out.push(tag);
    for u in text {
        out.extend_from_slice(&u.to_le_bytes());
    }
    out
}

fn run(input: &[u8]) -> Result<Vec<u8>, Refusal> {
    if input.len() > MAX_INPUT_BYTES {
        return Err(Refusal::TooLarge);
    }
    let (&op, rest) = input.split_first().ok_or(Refusal::Input)?;
    match op {
        1 => {
            let aborted = flag(rest.first())?;
            let allowed = flag(rest.get(1))?;
            let (name, rest) = framed(rest.get(2..).ok_or(Refusal::Input)?, MAX_NAME_UNITS)?;
            let has_args = flag(rest.first())?;
            let body = rest.get(1..).unwrap_or(&[]);
            let args = if has_args {
                Some(le_units(body).ok_or(Refusal::Input)?)
            } else if body.is_empty() {
                None
            } else {
                return Err(Refusal::Input);
            };
            Ok(match check(aborted, allowed, &name, args.as_deref())? {
                Check::Run(key) => reply(0, &key),
                Check::Answer(text) => reply(1, &text),
            })
        }
        2 => {
            let (name, rest) = framed(rest, MAX_NAME_UNITS)?;
            let (message, rest) = framed(rest, MAX_NAME_UNITS)?;
            if !rest.is_empty() {
                return Err(Refusal::Input);
            }
            Ok(reply(1, &call_error(&name, &message)?))
        }
        _ => Err(Refusal::Input),
    }
}

/// The wasm call. Input `u8(op)`, then for op 1 (the checks) `u8(aborted) u8(allowed)
/// u32le(n) name u8(hasArgs) [args]`, for op 2 (a failed call's text) `u32le(n) name u32le(m)
/// message`; every string UTF-16LE code units, the arguments running to the end. Status 0 replies
/// `u8(tag)` and UTF-16LE units: tag 0 the dedupe key (run the tool), tag 1 the tool result
/// (do not run it). Status 1 refuses with `{"error":"input"|"too_large"|"depth"}`.
pub fn call(input: &[u8]) -> (u32, Vec<u8>) {
    match run(input) {
        Ok(r) => (0, r),
        Err(e) => (1, e.json().as_bytes().to_vec()),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing, clippy::panic)]
mod tests {
    use super::*;

    fn key(args: &str) -> String {
        match check(false, true, &units("t"), Some(&units(args))).unwrap() {
            Check::Run(k) => String::from_utf16(&k).unwrap(),
            Check::Answer(a) => panic!("{}", String::from_utf16_lossy(&a)),
        }
    }

    #[test]
    fn canonical_keys() {
        assert_eq!(
            key(r#"{"b":1,"a":[2,{"d":1,"c":-0}]}"#),
            r#"["t","{\"a\":[2,{\"c\":0,\"d\":1}],\"b\":1}"]"#
        );
        assert_eq!(key(r#"{"a":1,"a":2}"#), r#"["t","{\"a\":2}"]"#);
        assert_eq!(
            key(r#"{"n":1e400,"m":1.50,"k":1e21,"j":1e-7}"#),
            r#"["t","{\"j\":1e-7,\"k\":1e+21,\"m\":1.5,\"n\":null}"]"#
        );
        assert_eq!(key(r#"{"__proto__":1}"#), r#"["t","{\"__proto__\":1}"]"#);
        // U+FF5E sorts after U+1F600 (high surrogate 0xD83D) in code-unit order.
        assert_eq!(
            key("{\"\u{ff5e}\":1,\"\u{1f600}\":2}"),
            "[\"t\",\"{\\\"\u{1f600}\\\":2,\\\"\u{ff5e}\\\":1}\"]"
        );
    }

    #[test]
    fn answers() {
        let a = |aborted, allowed, args: Option<&str>| match check(
            aborted,
            allowed,
            &units("x\"y"),
            args.map(units).as_deref(),
        )
        .unwrap()
        {
            Check::Answer(t) => String::from_utf16(&t).unwrap(),
            Check::Run(_) => "run".into(),
        };
        assert_eq!(a(true, false, None), CANCELLED);
        assert_eq!(
            a(false, false, None),
            "ERROR: tool \"x\"y\" is not enabled for this project"
        );
        assert_eq!(a(false, true, None), "run");
        assert_eq!(a(false, true, Some("[1]")), NOT_OBJECT);
        assert_eq!(
            a(false, true, Some("nul")),
            "ERROR: tool arguments were not valid JSON: nul"
        );
    }

    #[test]
    fn depth_and_size() {
        let deep = |d: usize| format!("{}1{}", "{\"a\":".repeat(d), "}".repeat(d));
        assert!(matches!(
            check(false, true, &[], Some(&units(&deep(MAX_DEPTH)))),
            Ok(Check::Run(_))
        ));
        assert_eq!(
            check(false, true, &[], Some(&units(&deep(MAX_DEPTH + 1)))),
            Err(Refusal::Depth)
        );
        let big = vec![0x20u16; MAX_ARGS_UNITS + 1];
        assert_eq!(check(false, true, &[], Some(&big)), Err(Refusal::TooLarge));
    }

    #[test]
    fn call_shapes() {
        assert_eq!(call(&[]).0, 1);
        assert_eq!(
            call(&[1, 0, 1, 1, 0, 0, 0, 0x74, 0, 0]),
            (0, reply(0, &units("[\"t\",\"{}\"]")))
        );
        assert_eq!(call(&[1, 0, 1, 1, 0, 0, 0, 0x74, 0, 0, 1]).0, 1);
        assert_eq!(call(&[1, 2, 1, 0, 0, 0, 0, 0]).0, 1);
        assert_eq!(
            call(&[2, 1, 0, 0, 0, 0x74, 0, 1, 0, 0, 0, 0x61, 0]),
            (0, reply(1, &units("ERROR calling t: a")))
        );
        assert_eq!(js_number(123456789012345680000.0), "123456789012345680000");
        assert_eq!(js_number(5e-324), "5e-324");
        assert_eq!(js_number(0.000001), "0.000001");
    }
}
