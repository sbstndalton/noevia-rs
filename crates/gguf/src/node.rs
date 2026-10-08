//! noevia-core's `server/gguf-meta.cjs` (`summarize(readGguf(file))`) in Rust, decision for
//! decision. Not the model-manager reader above it in this crate: the Node reader has its own
//! caps (arrays over 1024 elements become `{array, count}`, strings over 256 KiB read as
//! `null`, a 128 MiB header limit) and throws on the first fault instead of recording `_error`.
//!
//! The host keeps the file I/O. It passes the file's size and a prefix of it (the *window*);
//! the reader either answers or names the end offset it needs (`need`), which the host reads
//! and asks again. Only bytes the JS `take`s must be in the window; skipped bytes need not be.
//!
//! Stricter than the JS, as fixed refusals (the JS would recurse or allocate without bound
//! first): arrays nested more than [`MAX_NEST`] deep, more than [`MAX_KEPT`] values kept for
//! the summary, and a window over [`MAX_WINDOW_BYTES`].

use std::collections::HashMap;

/// gguf-meta.cjs MAX_HEADER_BYTES: no read or skip may end past this offset.
pub const MAX_HEADER_BYTES: u64 = 128 * 1024 * 1024;
/// gguf-meta.cjs MAX_ARRAY_KEPT: longer arrays become `{array: true, count}`.
pub const MAX_ARRAY_KEPT: u64 = 1024;
/// gguf-meta.cjs MAX_STRING_KEPT: longer strings are skipped and read as `null`.
pub const MAX_STRING_KEPT: u64 = 256 * 1024;
/// The largest window (file prefix) one call accepts.
pub const MAX_WINDOW_BYTES: usize = 16 * 1024 * 1024;
/// The largest request: `u64le(size)` and the window.
pub const MAX_INPUT_BYTES: usize = MAX_WINDOW_BYTES + 8;
/// Arrays nested deeper than this are refused (the JS recurses until its stack runs out).
pub const MAX_NEST: u32 = 64;
/// Values kept for the summary (every list element counts) before the call is refused.
pub const MAX_KEPT: u64 = 262_144;

const MAX_SAFE_INTEGER: u64 = (1 << 53) - 1;
const T_BOOL: u32 = 7;
const T_STRING: u32 = 8;
const T_ARRAY: u32 = 9;

/// The suffixes summarize() reads under `${arch}.`, in output order with their JSON names.
/// `raw` fields are returned as stored (`?? null`), the others through `int()`.
const FIELDS: [(&str, &str, bool); 16] = [
    ("contextLength", "context_length", false),
    ("embeddingLength", "embedding_length", false),
    ("blockCount", "block_count", false),
    ("headCount", "attention.head_count", false),
    ("headCountKv", "attention.head_count_kv", true),
    ("keyLength", "attention.key_length", false),
    ("valueLength", "attention.value_length", false),
    ("keyLengthSwa", "attention.key_length_swa", false),
    ("valueLengthSwa", "attention.value_length_swa", false),
    ("slidingWindow", "attention.sliding_window", false),
    (
        "slidingWindowPattern",
        "attention.sliding_window_pattern",
        true,
    ),
    ("sharedKvLayers", "attention.shared_kv_layers", false),
    ("fullAttentionInterval", "full_attention_interval", false),
    ("ssmStateSize", "ssm.state_size", false),
    ("expertCount", "expert_count", false),
    ("nextnPredictLayers", "nextn_predict_layers", false),
];

/// One kv value as the JS holds it.
#[derive(Debug, Clone, PartialEq)]
pub enum Val {
    /// A JS number (every scalar but bool; 64-bit integers rounded as `Number(bigint)`).
    Num(f64),
    Bool(bool),
    Str(String),
    /// A string over [`MAX_STRING_KEPT`] bytes.
    Null,
    List(Vec<Val>),
    /// An array over [`MAX_ARRAY_KEPT`] elements: `{array: true, count}`.
    Big(u64),
}

/// Why the JS would throw, or why this reader refuses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fault {
    /// `Not a GGUF file`
    NotGguf,
    /// `Unsupported GGUF version ${v}`
    Version(u32),
    /// `GGUF header exceeds the metadata limit`
    Limit,
    /// `Unexpected end of GGUF header`
    Eof,
    /// `GGUF length out of range`
    Range,
    /// `Unknown GGUF value type ${t}`
    Type(u32),
    /// `Unsupported nested GGUF array`
    Nested,
    /// Refusal: arrays nested deeper than [`MAX_NEST`].
    Depth,
    /// Refusal: more than [`MAX_KEPT`] values kept.
    Kept,
    /// Not a fault: the window must reach this end offset.
    Need(u64),
}

impl Fault {
    /// The JS error message, for faults the JS has; `None` for refusals and `Need`.
    pub fn js_message(&self) -> Option<String> {
        Some(match self {
            Fault::NotGguf => "Not a GGUF file".into(),
            Fault::Version(v) => format!("Unsupported GGUF version {v}"),
            Fault::Limit => "GGUF header exceeds the metadata limit".into(),
            Fault::Eof => "Unexpected end of GGUF header".into(),
            Fault::Range => "GGUF length out of range".into(),
            Fault::Type(t) => format!("Unknown GGUF value type {t}"),
            Fault::Nested => "Unsupported nested GGUF array".into(),
            Fault::Depth | Fault::Kept | Fault::Need(_) => return None,
        })
    }
}

fn scalar_size(t: u32) -> Option<u64> {
    match t {
        0 | 1 | 7 => Some(1),
        2 | 3 => Some(2),
        4..=6 => Some(4),
        10..=12 => Some(8),
        _ => None,
    }
}

struct Reader<'a, F: Fn(&str) -> bool> {
    win: &'a [u8],
    size: u64,
    pos: u64,
    kept: u64,
    keep_key: F,
}

impl<F: Fn(&str) -> bool> Reader<'_, F> {
    /// gguf-meta.cjs `ensure` + `take`: the header limit first, then the end of the file.
    fn take(&mut self, n: u64) -> Result<&[u8], Fault> {
        let end = u128::from(self.pos) + u128::from(n);
        if end > u128::from(MAX_HEADER_BYTES) {
            return Err(Fault::Limit);
        }
        // end <= 128 MiB, so these conversions cannot fail.
        let end = u64::try_from(end).map_err(|_| Fault::Limit)?;
        if end > self.size {
            return Err(Fault::Eof);
        }
        let (Ok(from), Ok(to)) = (usize::try_from(self.pos), usize::try_from(end)) else {
            return Err(Fault::Limit);
        };
        let Some(bytes) = self.win.get(from..to) else {
            return Err(Fault::Need(end));
        };
        self.pos = end;
        Ok(bytes)
    }

    fn skip(&mut self, n: u128) -> Result<(), Fault> {
        let end = u128::from(self.pos) + n;
        if end > u128::from(MAX_HEADER_BYTES) {
            return Err(Fault::Limit);
        }
        self.pos = u64::try_from(end).map_err(|_| Fault::Limit)?;
        Ok(())
    }

    fn fixed<const N: usize>(&mut self) -> Result<[u8; N], Fault> {
        let mut out = [0u8; N];
        out.copy_from_slice(self.take(N as u64)?);
        Ok(out)
    }

    fn u32(&mut self) -> Result<u32, Fault> {
        Ok(u32::from_le_bytes(self.fixed::<4>()?))
    }

    /// `u64()`: a length or count, refused over Number.MAX_SAFE_INTEGER.
    fn u64(&mut self) -> Result<u64, Fault> {
        let v = u64::from_le_bytes(self.fixed::<8>()?);
        if v > MAX_SAFE_INTEGER {
            return Err(Fault::Range);
        }
        Ok(v)
    }

    fn string(&mut self) -> Result<Option<String>, Fault> {
        let n = self.u64()?;
        if n > MAX_STRING_KEPT {
            self.skip(u128::from(n))?;
            return Ok(None);
        }
        Ok(Some(String::from_utf8_lossy(self.take(n)?).into_owned()))
    }

    fn count_kept(&mut self, keep: bool) -> Result<(), Fault> {
        if keep {
            self.kept += 1;
            if self.kept > MAX_KEPT {
                return Err(Fault::Kept);
            }
        }
        Ok(())
    }

    fn scalar(&mut self, t: u32, size: u64) -> Result<Val, Fault> {
        let mut a8 = [0u8; 8];
        for (slot, b) in a8.iter_mut().zip(self.take(size)?) {
            *slot = *b;
        }
        let a4 = [a8[0], a8[1], a8[2], a8[3]];
        let a2 = [a8[0], a8[1]];
        Ok(match t {
            0 => Val::Num(f64::from(a8[0])),
            1 => Val::Num(f64::from(a8[0] as i8)),
            2 => Val::Num(f64::from(u16::from_le_bytes(a2))),
            3 => Val::Num(f64::from(i16::from_le_bytes(a2))),
            4 => Val::Num(f64::from(u32::from_le_bytes(a4))),
            5 => Val::Num(f64::from(i32::from_le_bytes(a4))),
            6 => Val::Num(f64::from(f32::from_le_bytes(a4))),
            T_BOOL => Val::Bool(a8[0] != 0),
            // Number(bigint): round to nearest, ties to even, as `as f64` does.
            10 => Val::Num(u64::from_le_bytes(a8) as f64),
            11 => Val::Num(i64::from_le_bytes(a8) as f64),
            _ => Val::Num(f64::from_le_bytes(a8)),
        })
    }

    /// `value(type)`. `nest` is the number of arrays around this value; `keep` whether the
    /// value is kept (otherwise it is only walked, for its faults).
    fn value(&mut self, t: u32, nest: u32, keep: bool) -> Result<Option<Val>, Fault> {
        self.count_kept(keep)?;
        if let Some(size) = scalar_size(t) {
            let v = self.scalar(t, size)?;
            return Ok(keep.then_some(v));
        }
        if t == T_STRING {
            let s = self.string()?;
            return Ok(keep.then(|| s.map_or(Val::Null, Val::Str)));
        }
        if t != T_ARRAY {
            return Err(Fault::Type(t));
        }
        if nest >= MAX_NEST {
            return Err(Fault::Depth);
        }
        let sub = self.u32()?;
        let count = self.u64()?;
        if count > MAX_ARRAY_KEPT {
            if sub == T_STRING {
                for _ in 0..count {
                    let n = self.u64()?;
                    self.skip(u128::from(n))?;
                }
            } else if let Some(size) = scalar_size(sub) {
                self.skip(u128::from(size) * u128::from(count))?;
            } else {
                return Err(Fault::Nested);
            }
            return Ok(keep.then_some(Val::Big(count)));
        }
        let mut out = Vec::new();
        for _ in 0..count {
            if let Some(v) = self.value(sub, nest + 1, keep)? {
                out.push(v);
            }
        }
        Ok(keep.then_some(Val::List(out)))
    }

    /// `readGguf`: every kv pair; `keep_key` names the ones kept (later duplicates win).
    fn read(&mut self) -> Result<HashMap<String, Val>, Fault> {
        if self.take(4)? != b"GGUF" {
            return Err(Fault::NotGguf);
        }
        let version = self.u32()?;
        if !(2..=3).contains(&version) {
            return Err(Fault::Version(version));
        }
        self.u64()?; // tensor count
        let kv_count = self.u64()?;
        let mut kv = HashMap::new();
        for _ in 0..kv_count {
            // A skipped key is `kv[null]`, i.e. the key "null".
            let key = self.string()?.unwrap_or_else(|| "null".to_owned());
            let t = self.u32()?;
            let keep = (self.keep_key)(&key);
            if let Some(v) = self.value(t, 0, keep)? {
                kv.insert(key, v);
            }
        }
        Ok(kv)
    }
}

fn read_kept<F: Fn(&str) -> bool>(
    size: u64,
    win: &[u8],
    keep_key: F,
) -> Result<HashMap<String, Val>, Fault> {
    Reader {
        win,
        size,
        pos: 0,
        kept: 0,
        keep_key,
    }
    .read()
}

/// gguf-meta.cjs `int()`.
fn int(v: Option<&Val>) -> Option<f64> {
    match v? {
        Val::List(values) => mode(values),
        Val::Num(n) if n.is_finite() => Some(n.trunc()),
        Val::Bool(b) => Some(if *b { 1.0 } else { 0.0 }),
        _ => None,
    }
}

/// gguf-meta.cjs `mode()`: the most frequent number, first seen on a tie; keys compare as a
/// Map's (SameValueZero: NaN equals NaN, -0 is stored as +0).
fn mode(values: &[Val]) -> Option<f64> {
    let same = |a: f64, b: f64| (a.is_nan() && b.is_nan()) || a == b;
    let mut counts: Vec<(f64, u64)> = Vec::new();
    for v in values {
        if let Val::Num(n) = v {
            let key = if *n == 0.0 { 0.0 } else { *n };
            match counts.iter_mut().find(|(k, _)| same(*k, key)) {
                Some((_, c)) => *c += 1,
                None => counts.push((key, 1)),
            }
        }
    }
    let mut best = None;
    let mut most = 0;
    for (k, c) in counts {
        if c > most {
            best = Some(k);
            most = c;
        }
    }
    best
}

/// A JS number as `Number.prototype.toString()` writes it (finite, not -0).
pub fn js_number(v: f64) -> String {
    if v == 0.0 {
        return "0".into();
    }
    let sign = if v < 0.0 { "-" } else { "" };
    // `{:e}` gives the shortest digit count k that round-trips; ECMAScript then takes the k-digit
    // value closest to v (ties to even), which is v correctly rounded to k digits.
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

/// One number in the reply: finite numbers as JS writes them; NaN, ±Infinity and -0 (which
/// JSON cannot carry) as `{"$num":"NaN"|"Infinity"|"-Infinity"|"-0"}`.
pub fn write_num(out: &mut String, v: f64) {
    if v.is_nan() {
        out.push_str(r#"{"$num":"NaN"}"#);
    } else if v.is_infinite() {
        out.push_str(if v > 0.0 {
            r#"{"$num":"Infinity"}"#
        } else {
            r#"{"$num":"-Infinity"}"#
        });
    } else if v == 0.0 && v.is_sign_negative() {
        out.push_str(r#"{"$num":"-0"}"#);
    } else {
        out.push_str(&js_number(v));
    }
}

/// An ASCII JSON string: `"` `\` and \b \f \n \r \t escaped short, every other unit below 0x20
/// or above 0x7e as `\uXXXX` (lowercase hex).
pub fn write_str(out: &mut String, s: &str) {
    out.push('"');
    for u in s.encode_utf16() {
        match u {
            0x22 => out.push_str("\\\""),
            0x5c => out.push_str("\\\\"),
            0x08 => out.push_str("\\b"),
            0x0c => out.push_str("\\f"),
            0x0a => out.push_str("\\n"),
            0x0d => out.push_str("\\r"),
            0x09 => out.push_str("\\t"),
            0x20..=0x7e => out.push(char::from(u as u8)),
            _ => out.push_str(&format!("\\u{u:04x}")),
        }
    }
    out.push('"');
}

fn write_val(out: &mut String, v: &Val) {
    match v {
        Val::Num(n) => write_num(out, *n),
        Val::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Val::Str(s) => write_str(out, s),
        Val::Null => out.push_str("null"),
        Val::Big(c) => {
            out.push_str(r#"{"array":true,"count":"#);
            out.push_str(&js_number(*c as f64));
            out.push('}');
        }
        Val::List(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_val(out, item);
            }
            out.push(']');
        }
    }
}

/// `summarize(readGguf(file))` over `win`, the first `win.len()` bytes of a file of `size`
/// bytes: the summary as ASCII JSON (keys in the JS order), or the fault.
pub fn summary(size: u64, win: &[u8]) -> Result<String, Fault> {
    // Pass 1 finds the architecture (and every fault); pass 2 keeps only the keys it names.
    let first = read_kept(size, win, |k| k == "general.architecture")?;
    let arch = match first.get("general.architecture") {
        Some(Val::Str(s)) => s.clone(),
        _ => String::new(),
    };
    let names: Vec<String> = FIELDS.iter().map(|f| format!("{arch}.{}", f.1)).collect();
    let kv = read_kept(size, win, |k| {
        k == "general.name" || k == "tokenizer.chat_template" || names.iter().any(|n| n == k)
    })?;
    let mut out = String::from("{\"arch\":");
    write_str(&mut out, &arch);
    out.push_str(",\"name\":");
    match kv.get("general.name") {
        Some(Val::Str(s)) => write_str(&mut out, s),
        _ => write_str(&mut out, ""),
    }
    for ((json, _, raw), key) in FIELDS.iter().zip(names.iter()) {
        out.push_str(",\"");
        out.push_str(json);
        out.push_str("\":");
        let v = kv.get(key);
        if *raw {
            match v {
                Some(v) => write_val(&mut out, v),
                None => out.push_str("null"),
            }
        } else {
            match int(v) {
                Some(n) => write_num(&mut out, n),
                None => out.push_str("null"),
            }
        }
    }
    let template = matches!(kv.get("tokenizer.chat_template"), Some(Val::Str(s)) if !s.is_empty());
    out.push_str(",\"hasChatTemplate\":");
    out.push_str(if template { "true" } else { "false" });
    out.push('}');
    Ok(out)
}

/// The wire call: input `u64le(size)` and the window (at most [`MAX_WINDOW_BYTES`], no longer
/// than `size`). Status 0 replies `{"summary":{…}}`, `{"need":N}` (N past the window, at most
/// `size`) or `{"fail":"not_gguf"|"limit"|"eof"|"range"|"nested"}` /
/// `{"fail":"version"|"type","value":N}` (the JS's errors); status 1 refuses with
/// `{"error":"input"|"too_large"|"depth"|"kept"}`.
pub fn call(input: &[u8]) -> (u32, String) {
    if input.len() > MAX_INPUT_BYTES {
        return (1, r#"{"error":"too_large"}"#.into());
    }
    let (Some(head), Some(win)) = (input.get(..8), input.get(8..)) else {
        return (1, r#"{"error":"input"}"#.into());
    };
    let mut size = [0u8; 8];
    size.copy_from_slice(head);
    let size = u64::from_le_bytes(size);
    if win.len() as u64 > size {
        return (1, r#"{"error":"input"}"#.into());
    }
    match summary(size, win) {
        Ok(s) => (0, format!("{{\"summary\":{s}}}")),
        Err(f) => fault_reply(f),
    }
}

fn fault_reply(f: Fault) -> (u32, String) {
    let fixed = |c: &str| (0, format!("{{\"fail\":\"{c}\"}}"));
    match f {
        Fault::NotGguf => fixed("not_gguf"),
        Fault::Limit => fixed("limit"),
        Fault::Eof => fixed("eof"),
        Fault::Range => fixed("range"),
        Fault::Nested => fixed("nested"),
        Fault::Version(v) => (0, format!("{{\"fail\":\"version\",\"value\":{v}}}")),
        Fault::Type(t) => (0, format!("{{\"fail\":\"type\",\"value\":{t}}}")),
        Fault::Need(n) => (0, format!("{{\"need\":{n}}}")),
        Fault::Depth => (1, r#"{"error":"depth"}"#.into()),
        Fault::Kept => (1, r#"{"error":"kept"}"#.into()),
    }
}
