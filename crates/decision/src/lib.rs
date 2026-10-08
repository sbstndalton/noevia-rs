//! noevia-core's `server/decision/index.cjs` pure checks in Rust, exported from `dav-parse.wasm`
//! (DECISION_IMPL). The decision layer never carries authority: the caller passes the allowed
//! options or items and a fallback, and a backend's answer is used only if it stays inside them.
//! This ports the three functions that decide that, in the JS's order and with its messages:
//!
//! - [`invalid_request`]: `invalidRequest(r)`, the request's own shape.
//! - [`invalid_result`]: `invalidResult(r, result)`, whether a backend's answer stays inside what
//!   was offered (scores only for offered ids, finite numbers, a ranking of offered items without
//!   duplicates, a choice among the options).
//! - [`cause_of`]: `causeOf(error)`, the short text-free code for a failure (#682).
//!
//! The backend chain, deadlines, benching and logging stay in the JS.
//!
//! # Input
//!
//! The host projects the JS values onto what these functions read, in review-verdict's tagged
//! JSON form plus one tag ([`Js`]): `null`, booleans, finite numbers and strings as themselves;
//! `["u"]` undefined; `["n","-0"|"NaN"|"Infinity"|"-Infinity"]`; `["f"]` a function, symbol or
//! bigint; `["x"]` an object never looked inside; `["h",n]` an array of length `n` that is sparse
//! or longer than the host lists; `["a",[…]]` and `["o",[[k,v],…]]` as the host projected them
//! (an item becomes `{"id":…}`, `scores` its own enumerable keys and values).
//!
//! # Faults
//!
//! Where the JS throws (`.id` of null, `.map` of a non-array), compares object identities, or
//! coerces a value the port is not shown, the port refuses ([`Fault`]) instead of guessing; the
//! host treats a refused request or result as invalid (the caller's fallback answers) and a
//! refused cause as `exception`. Never weaker than the JS.

#![forbid(unsafe_code)]

use prompt_framing::js::{is_js_space, units};
use prompt_framing::json::{self, Value};
use std::collections::HashSet;

/// The largest request [`call`] accepts (the op byte and the JSON).
pub const MAX_INPUT_BYTES: usize = 4 * 1024 * 1024 + 1;
/// JSON nesting of the tagged form.
const MAX_TAG_DEPTH: usize = 16;

const KINDS: [&str; 5] = ["choice", "multi", "rank", "noul", "score"];

/// A JS value as the host projects it (see the crate docs).
#[derive(Clone, Debug, PartialEq)]
pub enum Js {
    Undefined,
    Null,
    Bool(bool),
    Num(f64),
    Str(Vec<u16>),
    /// A function, symbol or bigint.
    Func,
    Arr(Vec<Js>),
    /// An array of this length the host did not list.
    Holey(f64),
    Obj(Vec<(Vec<u16>, Js)>),
    /// An object the port does not look inside.
    Opaque,
}

/// The port cannot answer exactly what the JS would.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Fault {
    /// The JS would throw here.
    Throws,
    /// The JS would look inside, coerce or compare the identity of a value it was not shown.
    Opaque,
}

impl Js {
    fn get(&self, key: &str) -> Js {
        match self {
            Js::Obj(m) => m
                .iter()
                .find(|(k, _)| k.iter().copied().eq(key.encode_utf16()))
                .map_or(Js::Undefined, |(_, v)| v.clone()),
            _ => Js::Undefined,
        }
    }

    fn is_object(&self) -> bool {
        matches!(self, Js::Arr(_) | Js::Holey(_) | Js::Obj(_) | Js::Opaque)
    }

    /// `!!v`; a bigint (`["f"]`) may be `0n`.
    fn truthy(&self) -> Result<bool, Fault> {
        Ok(match self {
            Js::Undefined | Js::Null => false,
            Js::Bool(b) => *b,
            Js::Num(n) => *n != 0.0 && !n.is_nan(),
            Js::Str(s) => !s.is_empty(),
            Js::Func => return Err(Fault::Opaque),
            Js::Arr(_) | Js::Holey(_) | Js::Obj(_) | Js::Opaque => true,
        })
    }

    fn is_str(&self, s: &str) -> bool {
        matches!(self, Js::Str(u) if u.iter().copied().eq(s.encode_utf16()))
    }
}

/// A primitive under `SameValueZero`: NaN equals NaN, -0 equals 0.
#[derive(Clone, PartialEq, Eq, Hash)]
enum Key {
    Undefined,
    Null,
    Bool(bool),
    Num(u64),
    Str(Vec<u16>),
}

/// A primitive's key; `None` for an object, function, symbol or bigint (compared by identity or
/// by a value the port is not shown).
fn key(v: &Js) -> Option<Key> {
    Some(match v {
        Js::Undefined => Key::Undefined,
        Js::Null => Key::Null,
        Js::Bool(b) => Key::Bool(*b),
        Js::Num(n) if n.is_nan() => Key::Num(f64::NAN.to_bits()),
        Js::Num(n) => Key::Num((*n + 0.0).to_bits()),
        Js::Str(s) => Key::Str(s.clone()),
        _ => return None,
    })
}

/// A JS `Set` of ids: `has` is `SameValueZero`. A primitive never equals an object; an object
/// may equal another object, which the port cannot tell.
#[derive(Default)]
struct IdSet {
    prims: HashSet<Key>,
    objects: bool,
}

impl IdSet {
    fn new<'a>(ids: impl IntoIterator<Item = &'a Js>) -> Self {
        let mut set = IdSet::default();
        for id in ids {
            match key(id) {
                Some(k) => {
                    set.prims.insert(k);
                }
                None => set.objects = true,
            }
        }
        set
    }

    fn has(&self, v: &Js) -> Result<bool, Fault> {
        match key(v) {
            Some(k) => Ok(self.prims.contains(&k)),
            None if self.objects => Err(Fault::Opaque),
            None => Ok(false),
        }
    }
}

/// `StringToNumber(s)` (ECMAScript 7.1.4.1.1).
pub fn string_to_number(s: &[u16]) -> f64 {
    let start = s.iter().position(|&c| !is_js_space(c)).unwrap_or(s.len());
    let end = s
        .iter()
        .rposition(|&c| !is_js_space(c))
        .map_or(start, |i| i + 1);
    let t = s.get(start..end).unwrap_or(&[]);
    if t.is_empty() {
        return 0.0;
    }
    if !t.iter().all(|&c| c < 0x80) {
        return f64::NAN;
    }
    let t: String = t.iter().map(|&c| char::from(c as u8)).collect();
    for (prefix, radix) in [
        ("0x", 16),
        ("0X", 16),
        ("0o", 8),
        ("0O", 8),
        ("0b", 2),
        ("0B", 2),
    ] {
        if let Some(digits) = t.strip_prefix(prefix) {
            if digits.is_empty() || !digits.chars().all(|c| c.is_digit(radix)) {
                return f64::NAN;
            }
            // Only whether it is above zero matters to the callers here, but keep it a number.
            return digits.chars().fold(0.0f64, |n, c| {
                n * f64::from(radix) + f64::from(c.to_digit(radix).unwrap_or(0))
            });
        }
    }
    let body = t.strip_prefix(['+', '-']).unwrap_or(&t);
    if body == "Infinity" {
        return if t.starts_with('-') {
            f64::NEG_INFINITY
        } else {
            f64::INFINITY
        };
    }
    // StrUnsignedDecimalLiteral: digits [. digits] [e [+-] digits], at least one mantissa digit.
    let (mant, exp) = match body.find(['e', 'E']) {
        Some(i) => (
            body.get(..i).unwrap_or(""),
            Some(body.get(i + 1..).unwrap_or("")),
        ),
        None => (body, None),
    };
    let (int, frac) = mant.split_once('.').unwrap_or((mant, ""));
    let digits = |x: &str| x.chars().all(|c| c.is_ascii_digit());
    if (int.is_empty() && frac.is_empty()) || !digits(int) || !digits(frac) {
        return f64::NAN;
    }
    if let Some(e) = exp {
        let e = e.strip_prefix(['+', '-']).unwrap_or(e);
        if e.is_empty() || !digits(e) {
            return f64::NAN;
        }
    }
    t.parse::<f64>().unwrap_or(f64::NAN)
}

/// `v > 0` (Abstract Relational Comparison against the number 0).
fn above_zero(v: &Js) -> Result<bool, Fault> {
    Ok(match v {
        Js::Undefined | Js::Null => false,
        Js::Bool(b) => *b,
        Js::Num(n) => *n > 0.0,
        Js::Str(s) => string_to_number(s) > 0.0,
        // ToPrimitive could run user code; a bigint compares, a symbol throws.
        _ => return Err(Fault::Opaque),
    })
}

/// `Array.isArray(v)` and `v.length`.
fn array_len(v: &Js) -> Option<f64> {
    match v {
        Js::Arr(xs) => Some(xs.len() as f64),
        Js::Holey(n) => Some(*n),
        _ => None,
    }
}

/// `invalidRequest(r)`: the JS's message, or `None` for a well-formed request.
pub fn invalid_request(r: &Js) -> Result<Option<&'static str>, Fault> {
    // `!r || typeof r !== 'object'`: null is not an object here, and a function is not one.
    if !r.is_object() {
        return Ok(Some("not an object"));
    }
    let kind = r.get("kind");
    if !KINDS.iter().any(|k| kind.is_str(k)) {
        return Ok(Some("unknown kind"));
    }
    match r.get("purpose") {
        Js::Str(s) if !s.is_empty() => {}
        _ => return Ok(Some("purpose required")),
    }
    if r.get("fallback") == Js::Undefined {
        return Ok(Some("fallback required"));
    }
    let constraints = r.get("constraints");
    // A truthy primitive has no deadlineMs (undefined); an object or function was projected.
    if !constraints.truthy()? || !above_zero(&constraints.get("deadlineMs"))? {
        return Ok(Some("deadlineMs required"));
    }
    let nonempty = |key: &str| array_len(&r.get(key)).is_some_and(|n| n != 0.0);
    if kind.is_str("rank") && !nonempty("items") {
        return Ok(Some("rank needs items"));
    }
    if (kind.is_str("choice") || kind.is_str("multi")) && !nonempty("options") {
        return Ok(Some("options required"));
    }
    Ok(None)
}

/// `(r.kind === 'rank' ? r.items : r.options || []).map((o) => o.id)`.
fn allowed_ids(r: &Js) -> Result<Vec<Js>, Fault> {
    if !r.is_object() {
        // `r.kind` of null/undefined throws; of another primitive is undefined, and so is its
        // `options`, which gives [].
        return match r {
            Js::Undefined | Js::Null => Err(Fault::Throws),
            Js::Func => Err(Fault::Opaque),
            _ => Ok(Vec::new()),
        };
    }
    let list = if r.get("kind").is_str("rank") {
        r.get("items")
    } else {
        let o = r.get("options");
        if o.truthy()? {
            o
        } else {
            Js::Arr(Vec::new())
        }
    };
    match list {
        Js::Arr(items) => items
            .iter()
            .map(|o| match o {
                Js::Undefined | Js::Null => Err(Fault::Throws),
                Js::Obj(_) => Ok(o.get("id")),
                Js::Opaque | Js::Func | Js::Arr(_) | Js::Holey(_) => Err(Fault::Opaque),
                // A primitive's `.id` is undefined.
                _ => Ok(Js::Undefined),
            })
            .collect(),
        // `.map` skips holes: the port does not guess.
        Js::Holey(_) => Err(Fault::Opaque),
        // No `.map` on a primitive or on what the host did not describe.
        _ => Err(Fault::Throws),
    }
}

/// `invalidResult(r, result)`: the JS's message, or `None` when the answer stays inside.
pub fn invalid_result(r: &Js, result: &Js) -> Result<Option<&'static str>, Fault> {
    if !result.is_object() {
        return Ok(Some("no scores"));
    }
    let scores = result.get("scores");
    if !scores.is_object() {
        return Ok(Some("no scores"));
    }
    let allowed = IdSet::new(&allowed_ids(r)?);
    let Js::Obj(entries) = &scores else {
        return Err(Fault::Opaque);
    };
    for (id, _) in entries {
        if !allowed.prims.contains(&Key::Str(id.clone())) {
            return Ok(Some("score for an id that was not offered"));
        }
    }
    if entries
        .iter()
        .any(|(_, v)| !matches!(v, Js::Num(n) if n.is_finite()))
    {
        return Ok(Some("non-numeric score"));
    }
    let selected = result.get("selected");
    let kind = r.get("kind");
    if kind.is_str("rank") {
        let items = match &selected {
            Js::Arr(items) => items,
            Js::Holey(_) => return Err(Fault::Opaque),
            _ => return Ok(Some("ranking outside the items")),
        };
        for id in items {
            if !allowed.has(id)? {
                return Ok(Some("ranking outside the items"));
            }
        }
        // Every id is now a primitive (an object would have faulted or been outside): `new Set`.
        let mut seen = HashSet::with_capacity(items.len());
        for id in items {
            if !seen.insert(key(id).ok_or(Fault::Opaque)?) {
                return Ok(Some("duplicate in ranking"));
            }
        }
    } else if kind.is_str("choice") && selected != Js::Null && !allowed.has(&selected)? {
        return Ok(Some("choice outside the options"));
    }
    Ok(None)
}

/// What the host reads off an error for [`cause_of`].
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ErrorFacts {
    /// `!!error?.deadline`
    pub deadline: bool,
    /// `error?.reason` when it is a string.
    pub reason: Option<Vec<u16>>,
    /// `error?.name` when it is a string.
    pub name: Option<Vec<u16>>,
    /// `error instanceof SyntaxError`
    pub syntax: bool,
    /// `error instanceof TypeError`
    pub type_error: bool,
    /// `String(error.message)` when it is a TypeError.
    pub message: Option<Vec<u16>>,
}

/// `CAUSE_RE`: `^[a-z][a-z0-9-]{0,39}$`.
pub fn is_cause(s: &[u16]) -> bool {
    let lower = |c: &u16| (0x61..=0x7a).contains(c);
    match s.split_first() {
        Some((first, rest)) => {
            lower(first)
                && rest.len() <= 39
                && rest
                    .iter()
                    .all(|c| lower(c) || (0x30..=0x39).contains(c) || *c == 0x2d)
        }
        None => false,
    }
}

/// `/fetch failed/i.test(s)`: without the `u` flag only ASCII letters fold.
fn mentions_fetch_failed(s: &[u16]) -> bool {
    let needle = units("fetch failed");
    let fold = |c: u16| {
        if (0x41..=0x5a).contains(&c) {
            c + 0x20
        } else {
            c
        }
    };
    s.windows(needle.len())
        .any(|w| w.iter().zip(&needle).all(|(&a, &b)| fold(a) == b))
}

/// `causeOf(error)`.
pub fn cause_of(e: &ErrorFacts) -> Vec<u16> {
    if e.deadline {
        return units("deadline");
    }
    if let Some(r) = e.reason.as_deref().filter(|r| is_cause(r)) {
        return r.to_vec();
    }
    if e.name.as_deref().is_some_and(|n| {
        n == units("AbortError").as_slice() || n == units("TimeoutError").as_slice()
    }) {
        return units("aborted");
    }
    if e.syntax {
        return units("parse");
    }
    if e.type_error && e.message.as_deref().is_some_and(mentions_fetch_failed) {
        return units("network");
    }
    units("exception")
}

/// Decode the tagged form (see the crate docs); `None` when it is not one.
pub fn decode(v: &Value) -> Option<Js> {
    match v {
        Value::Null => Some(Js::Null),
        Value::Bool(b) => Some(Js::Bool(*b)),
        Value::Num(n) if n.is_finite() => Some(Js::Num(*n)),
        Value::Str(s) => Some(Js::Str(s.clone())),
        Value::Arr(items) => {
            let (tag, rest) = items.split_first()?;
            let tag = String::from_utf16(tag.as_str()?).ok()?;
            match (tag.as_str(), rest) {
                ("u", []) => Some(Js::Undefined),
                ("f", []) => Some(Js::Func),
                ("x", []) => Some(Js::Opaque),
                ("h", [Value::Num(n)]) if n.is_finite() && *n >= 0.0 && n.fract() == 0.0 => {
                    Some(Js::Holey(*n))
                }
                ("n", [Value::Str(s)]) => match String::from_utf16(s).ok()?.as_str() {
                    "-0" => Some(Js::Num(-0.0)),
                    "NaN" => Some(Js::Num(f64::NAN)),
                    "Infinity" => Some(Js::Num(f64::INFINITY)),
                    "-Infinity" => Some(Js::Num(f64::NEG_INFINITY)),
                    _ => None,
                },
                ("a", [Value::Arr(xs)]) => {
                    xs.iter().map(decode).collect::<Option<_>>().map(Js::Arr)
                }
                ("o", [Value::Arr(pairs)]) => {
                    let mut out: Vec<(Vec<u16>, Js)> = Vec::with_capacity(pairs.len());
                    for p in pairs {
                        let Value::Arr(kv) = p else { return None };
                        let [Value::Str(k), val] = kv.as_slice() else {
                            return None;
                        };
                        if out.iter().any(|(seen, _)| seen == k) {
                            return None;
                        }
                        out.push((k.clone(), decode(val)?));
                    }
                    Some(Js::Obj(out))
                }
                _ => None,
            }
        }
        _ => None,
    }
}

const TOO_LARGE: &str = r#"{"error":"too_large"}"#;
const BAD_INPUT: &str = r#"{"error":"input"}"#;

fn fault_json(f: Fault) -> &'static str {
    match f {
        Fault::Throws => r#"{"error":"throws"}"#,
        Fault::Opaque => r#"{"error":"opaque"}"#,
    }
}

fn invalid_json(v: Option<&str>) -> String {
    match v {
        None => r#"{"invalid":null}"#.to_owned(),
        Some(m) => format!("{{\"invalid\":\"{m}\"}}"),
    }
}

fn facts(v: &Value) -> Option<ErrorFacts> {
    let Value::Obj(m) = v else { return None };
    if m.len() != 6 {
        return None;
    }
    let flag = |k: &str| match v.get(k)? {
        Value::Bool(b) => Some(*b),
        _ => None,
    };
    let text = |k: &str| match v.get(k)? {
        Value::Null => Some(None),
        Value::Str(s) => Some(Some(s.clone())),
        _ => None,
    };
    Some(ErrorFacts {
        deadline: flag("deadline")?,
        reason: text("reason")?,
        name: text("name")?,
        syntax: flag("syntax")?,
        type_error: flag("typeError")?,
        message: text("message")?,
    })
}

/// The wasm call: input `u8(op)` and UTF-8 JSON. Op 1 `T` (invalidRequest's argument); op 2
/// `[T r, T result]` (invalidResult); reply `{"invalid":null|"<message>"}`. Op 3
/// `{"deadline":bool,"reason":s|null,"name":s|null,"syntax":bool,"typeError":bool,
/// "message":s|null}` (causeOf); reply `{"cause":"<code>"}`. Status 1 refuses with
/// `{"error":"input"|"too_large"|"throws"|"opaque"}`.
pub fn call(input: &[u8]) -> (u32, String) {
    if input.len() > MAX_INPUT_BYTES {
        return (1, TOO_LARGE.to_owned());
    }
    let Some((&op, body)) = input.split_first() else {
        return (1, BAD_INPUT.to_owned());
    };
    let Some(tree) = json::parse_utf8(body, MAX_TAG_DEPTH) else {
        return (1, BAD_INPUT.to_owned());
    };
    let answer = match op {
        1 => match decode(&tree) {
            Some(r) => invalid_request(&r).map(invalid_json),
            None => return (1, BAD_INPUT.to_owned()),
        },
        2 => {
            let Value::Arr(pair) = &tree else {
                return (1, BAD_INPUT.to_owned());
            };
            let [r, result] = pair.as_slice() else {
                return (1, BAD_INPUT.to_owned());
            };
            match (decode(r), decode(result)) {
                (Some(r), Some(result)) => invalid_result(&r, &result).map(invalid_json),
                _ => return (1, BAD_INPUT.to_owned()),
            }
        }
        3 => match facts(&tree) {
            Some(f) => {
                let mut out = b"{\"cause\":".to_vec();
                json::push_str(&mut out, &cause_of(&f));
                out.push(b'}');
                Ok(String::from_utf8(out).unwrap_or_else(|_| BAD_INPUT.to_owned()))
            }
            None => return (1, BAD_INPUT.to_owned()),
        },
        _ => return (1, BAD_INPUT.to_owned()),
    };
    match answer {
        Ok(s) => (0, s),
        Err(f) => (1, fault_json(f).to_owned()),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    fn s(t: &str) -> Js {
        Js::Str(units(t))
    }
    fn obj(pairs: &[(&str, Js)]) -> Js {
        Js::Obj(pairs.iter().map(|(k, v)| (units(k), v.clone())).collect())
    }
    fn item(id: Js) -> Js {
        obj(&[("id", id)])
    }

    #[test]
    fn string_numbers() {
        for (t, want) in [
            ("", 0.0),
            (" 5 ", 5.0),
            ("0x10", 16.0),
            ("1e-400", 0.0),
            ("-Infinity", f64::NEG_INFINITY),
            (".5", 0.5),
            ("5.", 5.0),
            ("+1", 1.0),
        ] {
            assert_eq!(string_to_number(&units(t)), want, "{t}");
        }
        for t in [
            "inf", "nan", "1e", "0x", "-0x1", "1_0", "..5", "e5", "infinity", "0b2",
        ] {
            assert!(string_to_number(&units(t)).is_nan(), "{t}");
        }
    }

    #[test]
    fn requests() {
        let base = |deadline: Js| {
            obj(&[
                ("kind", s("score")),
                ("purpose", s("p")),
                ("fallback", Js::Opaque),
                ("constraints", obj(&[("deadlineMs", deadline)])),
            ])
        };
        assert_eq!(invalid_request(&base(Js::Num(5.0))), Ok(None));
        assert_eq!(invalid_request(&base(Js::Bool(true))), Ok(None));
        assert_eq!(invalid_request(&base(s(" 0x1 "))), Ok(None));
        assert_eq!(
            invalid_request(&base(s("1e-400"))),
            Ok(Some("deadlineMs required"))
        );
        assert_eq!(invalid_request(&base(Js::Opaque)), Err(Fault::Opaque));
        assert_eq!(invalid_request(&Js::Func), Ok(Some("not an object")));
        let rank = obj(&[
            ("kind", s("rank")),
            ("purpose", s("p")),
            ("fallback", Js::Null),
            ("constraints", obj(&[("deadlineMs", Js::Num(1.0))])),
            ("items", Js::Holey(3.0)),
        ]);
        assert_eq!(invalid_request(&rank), Ok(None));
    }

    #[test]
    fn results() {
        let r = obj(&[
            ("kind", s("rank")),
            ("items", Js::Arr(vec![item(s("a")), item(s("b"))])),
        ]);
        let res = |scores: Js, sel: Js| obj(&[("scores", scores), ("selected", sel)]);
        let sc = obj(&[("a", Js::Num(1.0))]);
        assert_eq!(
            invalid_result(&r, &res(sc.clone(), Js::Arr(vec![s("b"), s("a")]))),
            Ok(None)
        );
        assert_eq!(
            invalid_result(&r, &res(sc.clone(), Js::Arr(vec![s("a"), s("a")]))),
            Ok(Some("duplicate in ranking"))
        );
        assert_eq!(
            invalid_result(&r, &res(obj(&[("c", Js::Num(1.0))]), Js::Arr(vec![]))),
            Ok(Some("score for an id that was not offered"))
        );
        assert_eq!(
            invalid_result(&r, &res(obj(&[("a", Js::Num(f64::NAN))]), Js::Arr(vec![]))),
            Ok(Some("non-numeric score"))
        );
        assert_eq!(
            invalid_result(&r, &res(sc.clone(), Js::Holey(2.0))),
            Err(Fault::Opaque)
        );
        // Object ids: a string never equals one; an object might.
        let r = obj(&[
            ("kind", s("choice")),
            ("options", Js::Arr(vec![item(Js::Opaque)])),
        ]);
        assert_eq!(
            invalid_result(&r, &res(obj(&[]), s("x"))),
            Ok(Some("choice outside the options"))
        );
        assert_eq!(
            invalid_result(&r, &res(obj(&[]), Js::Opaque)),
            Err(Fault::Opaque)
        );
        // `.id` of a null item throws.
        let r = obj(&[("kind", s("choice")), ("options", Js::Arr(vec![Js::Null]))]);
        assert_eq!(
            invalid_result(&r, &res(obj(&[]), Js::Null)),
            Err(Fault::Throws)
        );
        // NaN ids match NaN; -0 matches 0.
        let r = obj(&[
            ("kind", s("choice")),
            (
                "options",
                Js::Arr(vec![item(Js::Num(f64::NAN)), item(Js::Num(0.0))]),
            ),
        ]);
        assert_eq!(
            invalid_result(&r, &res(obj(&[]), Js::Num(f64::NAN))),
            Ok(None)
        );
        assert_eq!(invalid_result(&r, &res(obj(&[]), Js::Num(-0.0))), Ok(None));
    }

    #[test]
    fn causes() {
        let mut f = ErrorFacts {
            reason: Some(units("http-503")),
            ..Default::default()
        };
        assert_eq!(cause_of(&f), units("http-503"));
        f.reason = Some(units("Has Spaces"));
        assert_eq!(cause_of(&f), units("exception"));
        f.type_error = true;
        f.message = Some(units("x FETCH Failed y"));
        assert_eq!(cause_of(&f), units("network"));
        f.message = Some(units("fetch fa\u{0131}led"));
        assert_eq!(cause_of(&f), units("exception"));
        assert!(is_cause(&units(&format!("a{}", "b".repeat(39)))));
        assert!(!is_cause(&units(&format!("a{}", "b".repeat(40)))));
    }

    #[test]
    fn large_rankings_are_hashed() {
        let n = 65_536;
        let ids: Vec<Js> = (0..n).map(|i| Js::Str(units(&format!("id{i}")))).collect();
        let r = obj(&[
            ("kind", s("rank")),
            ("items", Js::Arr(ids.iter().cloned().map(item).collect())),
        ]);
        let scores = Js::Obj(
            ids.iter()
                .map(|id| {
                    (
                        if let Js::Str(u) = id {
                            u.clone()
                        } else {
                            vec![]
                        },
                        Js::Num(0.5),
                    )
                })
                .collect(),
        );
        let mut sel = ids.clone();
        sel.reverse();
        let res = obj(&[("scores", scores.clone()), ("selected", Js::Arr(sel))]);
        assert_eq!(invalid_result(&r, &res), Ok(None));
        let mut dup = ids.clone();
        dup.push(ids[7].clone());
        let res = obj(&[("scores", scores), ("selected", Js::Arr(dup))]);
        assert_eq!(invalid_result(&r, &res), Ok(Some("duplicate in ranking")));
    }

    #[test]
    fn call_shapes() {
        assert_eq!(call(b""), (1, BAD_INPUT.to_owned()));
        assert_eq!(
            call(b"\x01null"),
            (0, r#"{"invalid":"not an object"}"#.to_owned())
        );
        assert_eq!(
            call(b"\x02[[\"x\"],null]"),
            (0, r#"{"invalid":"no scores"}"#.to_owned())
        );
        assert_eq!(
            call(b"\x02[null,[\"o\",[[\"scores\",[\"o\",[]]]]]]"),
            (1, r#"{"error":"throws"}"#.to_owned())
        );
        assert_eq!(
            call(br#"{"deadline":false,"reason":null,"name":"AbortError","syntax":false,"typeError":false,"message":null}"#.iter().copied().fold(vec![3u8], |mut v, b| { v.push(b); v }).as_slice()),
            (0, r#"{"cause":"aborted"}"#.to_owned())
        );
        assert_eq!(call(b"\x03{}"), (1, BAD_INPUT.to_owned()));
        assert_eq!(call(b"\x01[\"h\",-1]"), (1, BAD_INPUT.to_owned()));
        let big = vec![b' '; MAX_INPUT_BYTES + 1];
        assert_eq!(call(&big), (1, TOO_LARGE.to_owned()));
    }
}
