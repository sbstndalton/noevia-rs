//! noevia-core's `server/code-review-verdict.cjs` (#519) in Rust, exported from `dav-parse.wasm`
//! (CODE_REVIEW_VERDICT_IMPL):
//!
//! - [`read_verdict`]: `readVerdict(raw)`, the strict reading of what a reviewer model returned.
//!   Either the cleaned verdict or which of the JS's nine `ReviewVerdictError` reasons it throws.
//! - [`bound_review_event`]: `boundReviewEvent(type, data)`, what a `review.*` job event keeps on
//!   append and on replay.
//!
//! The verdict is advice: nothing here can name a capability, an approval or a decision, and any
//! field the schema does not have makes the verdict invalid (the person reviews it themselves).
//!
//! # Input
//!
//! The host hands over JS values in a tagged JSON form ([`Js`]) that keeps what `JSON.stringify`
//! loses (`undefined`, `-0`, `NaN`, functions, own keys whose value is `undefined`):
//!
//! - `null`, `true`, `false`, a finite number or a string: itself (`-0` excepted).
//! - `["u"]` undefined; `["n","-0"|"NaN"|"Infinity"|"-Infinity"]`; `["f"]` a function, symbol or
//!   bigint (not an object); `["a",[T,…]]` a dense array with `Array.prototype`; `["o",[[k,T],…]]`
//!   an object with `Object.prototype` or a null prototype, its own enumerable string keys in
//!   `Object.keys` order; `["x"]` any other object (a class instance, a sparse array, a container
//!   deeper than the host encodes), which is never inspected.
//!
//! Where the JS only asks "is this a string / an integer / `true` / `undefined`", `["x"]` answers
//! as the object it is. Where the JS would look inside it (`isPlain`, `Array.isArray`, its keys),
//! the port refuses instead ([`Fault::Opaque`]), which the host turns into the JS's own failure
//! path: `readVerdict` throws, `boundReviewEvent` keeps a failed review. Never weaker than the JS.

#![forbid(unsafe_code)]

use prompt_framing::js::trim;
use prompt_framing::json::{self, Value};

/// The largest request [`call`] accepts (the op byte and the tagged JSON).
pub const MAX_INPUT_BYTES: usize = 4 * 1024 * 1024 + 1;
/// JSON nesting of the tagged form the parser builds (the host encodes three JS levels).
const MAX_TAG_DEPTH: usize = 32;

pub const MAX_SUMMARY: usize = 600;
pub const MAX_FINDINGS: usize = 12;
pub const MAX_FINDING: usize = 600;
pub const MAX_FILE: usize = 240;
pub const MAX_REASON: usize = 300;
const MAX_COUNT: f64 = 100_000.0;

const VERDICTS: [&str; 2] = ["approve", "request_changes"];
const SEVERITIES: [&str; 4] = ["blocker", "major", "minor", "note"];

/// A JS value as the host describes it (see the crate docs).
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
    Obj(Vec<(Vec<u16>, Js)>),
    /// An object the port does not look inside.
    Opaque,
}

/// The port cannot answer exactly what the JS would (it never guesses).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Fault {
    /// The JS would inspect an object the host did not describe.
    Opaque,
}

/// Why `readVerdict` throws: one `ReviewVerdictError` message each.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Invalid {
    Fields,
    Verdict,
    Missing,
    TooMany,
    Malformed,
    NoMessage,
    NoSummary,
    Unspecified,
    Blocked,
}

impl Invalid {
    /// The code on the wire.
    pub fn code(self) -> &'static str {
        match self {
            Invalid::Fields => "fields",
            Invalid::Verdict => "verdict",
            Invalid::Missing => "missing",
            Invalid::TooMany => "too_many",
            Invalid::Malformed => "malformed",
            Invalid::NoMessage => "no_message",
            Invalid::NoSummary => "no_summary",
            Invalid::Unspecified => "unspecified",
            Invalid::Blocked => "blocked",
        }
    }

    /// The JS's message for it.
    pub fn message(self) -> &'static str {
        match self {
            Invalid::Fields => "The verdict had fields a review cannot have.",
            Invalid::Verdict => "The verdict was neither approve nor request changes.",
            Invalid::Missing => "The verdict was missing its summary or findings.",
            Invalid::TooMany => "The verdict listed too many findings.",
            Invalid::Malformed => "A finding was malformed.",
            Invalid::NoMessage => "A finding had no message.",
            Invalid::NoSummary => "The verdict had no summary.",
            Invalid::Unspecified => "Changes were requested without saying which.",
            Invalid::Blocked => "The verdict approved a change it also called blocked.",
        }
    }
}

/// One cleaned finding.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Finding {
    pub severity: &'static str,
    pub file: Option<Vec<u16>>,
    pub message: Vec<u16>,
}

/// A verdict as `readVerdict` returns it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Verdict {
    pub verdict: &'static str,
    pub summary: Vec<u16>,
    pub findings: Vec<Finding>,
}

/// What a `review.*` event keeps.
#[derive(Clone, Debug, PartialEq)]
pub enum Event {
    Requested {
        base: Base,
        files: Option<f64>,
    },
    Failed {
        base: Base,
        code: Vec<u16>,
        reason: Vec<u16>,
    },
    Completed {
        base: Base,
        verdict: Verdict,
        corrected: bool,
    },
}

/// `baseSha` / `headSha`, each kept only when it looks like a commit.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Base {
    pub base_sha: Option<Vec<u16>>,
    pub head_sha: Option<Vec<u16>>,
}

const UNDEFINED: Js = Js::Undefined;

impl Js {
    /// `obj[key]` for an own key (anything else is `undefined`).
    fn get(&self, key: &str) -> &Js {
        let Js::Obj(m) = self else { return &UNDEFINED };
        m.iter()
            .find(|(k, _)| k.iter().copied().eq(key.encode_utf16()))
            .map_or(&UNDEFINED, |(_, v)| v)
    }

    fn is_str(&self, lit: &str) -> bool {
        matches!(self, Js::Str(s) if s.iter().copied().eq(lit.encode_utf16()))
    }
}

/// `isPlain(v)`: a non-null, non-array object.
fn is_plain(v: &Js) -> Result<bool, Fault> {
    match v {
        Js::Obj(_) => Ok(true),
        Js::Opaque => Err(Fault::Opaque),
        _ => Ok(false),
    }
}

/// `own(object, allowed)`: every own enumerable key is allowed (`object` is plain).
fn own(v: &Js, allowed: &[&str]) -> bool {
    let Js::Obj(m) = v else { return false };
    m.iter().all(|(k, _)| {
        allowed
            .iter()
            .any(|a| k.iter().copied().eq(a.encode_utf16()))
    })
}

/// `list.includes(v)` over string literals; the matched literal.
fn one_of(v: &Js, list: &[&'static str]) -> Option<&'static str> {
    list.iter().copied().find(|lit| v.is_str(lit))
}

/// The code units `clean()` drops: C0 controls but tab and newline, DEL, and the bidi embeddings,
/// overrides (U+202A-U+202E) and isolates (U+2066-U+2069).
fn dropped(u: u16) -> bool {
    matches!(u, 0x00..=0x08 | 0x0b..=0x1f | 0x7f | 0x202a..=0x202e | 0x2066..=0x2069)
}

/// `clean(value, max)`: a string's units with the controls dropped, trimmed as
/// `String.prototype.trim`, cut to `max` code points (`Array.from`: a lone surrogate counts as
/// one). Anything but a string is empty.
pub fn clean(v: &Js, max: usize) -> Vec<u16> {
    let Js::Str(s) = v else { return Vec::new() };
    let kept: Vec<u16> = s.iter().copied().filter(|&u| !dropped(u)).collect();
    let text = trim(&kept);
    let mut end = 0;
    let mut points = 0;
    while points < max {
        let Some(&u) = text.get(end) else { break };
        let pair = (0xd800..=0xdbff).contains(&u)
            && text
                .get(end + 1)
                .is_some_and(|n| (0xdc00..=0xdfff).contains(n));
        end += if pair { 2 } else { 1 };
        points += 1;
    }
    text.get(..end).unwrap_or(&[]).to_vec()
}

/// `readVerdict(raw)`: the verdict, or the reason the JS throws.
pub fn read_verdict(raw: &Js) -> Result<Result<Verdict, Invalid>, Fault> {
    if !is_plain(raw)? || !own(raw, &["verdict", "summary", "findings"]) {
        return Ok(Err(Invalid::Fields));
    }
    let Some(verdict) = one_of(raw.get("verdict"), &VERDICTS) else {
        return Ok(Err(Invalid::Verdict));
    };
    let summary_raw = raw.get("summary");
    let list = match raw.get("findings") {
        Js::Arr(items) => Some(items),
        Js::Opaque => return Err(Fault::Opaque),
        _ => None,
    };
    let (Js::Str(_), Some(items)) = (summary_raw, list) else {
        return Ok(Err(Invalid::Missing));
    };
    if items.len() > MAX_FINDINGS {
        return Ok(Err(Invalid::TooMany));
    }
    let mut findings = Vec::with_capacity(items.len());
    for f in items {
        if !is_plain(f)? || !own(f, &["severity", "file", "message"]) {
            return Ok(Err(Invalid::Malformed));
        }
        let Some(severity) = one_of(f.get("severity"), &SEVERITIES) else {
            return Ok(Err(Invalid::Malformed));
        };
        let file_raw = f.get("file");
        if !matches!(file_raw, Js::Undefined | Js::Str(_)) {
            return Ok(Err(Invalid::Malformed));
        }
        let message = clean(f.get("message"), MAX_FINDING);
        if message.is_empty() {
            return Ok(Err(Invalid::NoMessage));
        }
        let file = clean(file_raw, MAX_FILE);
        findings.push(Finding {
            severity,
            file: (!file.is_empty()).then_some(file),
            message,
        });
    }
    let summary = clean(summary_raw, MAX_SUMMARY);
    if summary.is_empty() {
        return Ok(Err(Invalid::NoSummary));
    }
    if verdict == "request_changes" && findings.is_empty() {
        return Ok(Err(Invalid::Unspecified));
    }
    if verdict == "approve" && findings.iter().any(|f| f.severity == "blocker") {
        return Ok(Err(Invalid::Blocked));
    }
    Ok(Ok(Verdict {
        verdict,
        summary,
        findings,
    }))
}

/// `sha(v)`: `/^[0-9a-f]{7,64}$/`.
fn sha(v: &Js) -> Option<Vec<u16>> {
    let Js::Str(s) = v else { return None };
    let ok = (7..=64).contains(&s.len())
        && s.iter()
            .all(|&u| (0x30..=0x39).contains(&u) || (0x61..=0x66).contains(&u));
    ok.then(|| s.clone())
}

/// `count(v)`: a non-negative integer (`-0` included) up to 100,000.
fn count(v: &Js) -> Option<f64> {
    let Js::Num(n) = v else { return None };
    (n.is_finite() && n.trunc() == *n && *n >= 0.0).then_some(if *n > MAX_COUNT {
        MAX_COUNT
    } else {
        *n
    })
}

fn units(s: &str) -> Vec<u16> {
    s.encode_utf16().collect()
}

/// `boundReviewEvent(type, data)`; `data` is what the host passes after its `= {}` default.
pub fn bound_review_event(kind: &Js, data: &Js) -> Result<Event, Fault> {
    let empty = Js::Obj(Vec::new());
    let d = if is_plain(data)? { data } else { &empty };
    let base = Base {
        base_sha: sha(d.get("baseSha")),
        head_sha: sha(d.get("headSha")),
    };
    if kind.is_str("review.requested") {
        return Ok(Event::Requested {
            base,
            files: count(d.get("files")),
        });
    }
    if kind.is_str("review.failed") {
        let code = clean(d.get("code"), 40);
        let reason = clean(d.get("reason"), MAX_REASON);
        return Ok(Event::Failed {
            base,
            code: if code.is_empty() {
                units("failed")
            } else {
                code
            },
            reason: if reason.is_empty() {
                units("The review did not finish.")
            } else {
                reason
            },
        });
    }
    // review.completed: the same strict reader; anything it would throw (or that the port cannot
    // decide) is the JS's catch: a failed review.
    let raw = Js::Obj(vec![
        (units("verdict"), d.get("verdict").clone()),
        (units("summary"), d.get("summary").clone()),
        (units("findings"), d.get("findings").clone()),
    ]);
    match read_verdict(&raw) {
        Ok(Ok(verdict)) => Ok(Event::Completed {
            base,
            verdict,
            corrected: matches!(d.get("corrected"), Js::Bool(true)),
        }),
        _ => Ok(Event::Failed {
            base,
            code: units("invalid"),
            reason: units("The recorded verdict could not be read."),
        }),
    }
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

fn push(out: &mut Vec<u8>, s: &str) {
    out.extend_from_slice(s.as_bytes());
}

fn push_units(out: &mut Vec<u8>, u: &[u16]) {
    json::push_str(out, u);
}

fn push_opt(out: &mut Vec<u8>, u: Option<&[u16]>) {
    match u {
        Some(u) => push_units(out, u),
        None => push(out, "null"),
    }
}

/// The findings array: `[{"severity":…[,"file":…],"message":…},…]`.
fn findings_json(out: &mut Vec<u8>, findings: &[Finding]) {
    push(out, "[");
    for (i, f) in findings.iter().enumerate() {
        if i > 0 {
            push(out, ",");
        }
        push(out, "{\"severity\":");
        push_units(out, &units(f.severity));
        if let Some(file) = &f.file {
            push(out, ",\"file\":");
            push_units(out, file);
        }
        push(out, ",\"message\":");
        push_units(out, &f.message);
        push(out, "}");
    }
    push(out, "]");
}

/// The verdict as the JS object: `{"verdict":…,"summary":…,"findings":[…]}`.
pub fn verdict_json(out: &mut Vec<u8>, v: &Verdict) {
    push(out, "{\"verdict\":");
    push_units(out, &units(v.verdict));
    push(out, ",\"summary\":");
    push_units(out, &v.summary);
    push(out, ",\"findings\":");
    findings_json(out, &v.findings);
    push(out, "}");
}

fn base_json(out: &mut Vec<u8>, status: &str, b: &Base) {
    push(out, "{\"status\":\"");
    push(out, status);
    push(out, "\",\"reviewer\":\"planner\",\"baseSha\":");
    push_opt(out, b.base_sha.as_deref());
    push(out, ",\"headSha\":");
    push_opt(out, b.head_sha.as_deref());
}

/// The event as the JS object, in its key order. `files` is an integer (`-0` written as `-0`,
/// which `JSON.parse` reads back as -0) or null.
pub fn event_json(out: &mut Vec<u8>, e: &Event) {
    match e {
        Event::Requested { base, files } => {
            base_json(out, "pending", base);
            push(out, ",\"files\":");
            match files {
                None => push(out, "null"),
                Some(n) if *n == 0.0 && n.is_sign_negative() => push(out, "-0"),
                // An integer in 0..=100000: exact in i64.
                Some(n) => push(out, &format!("{}", *n as i64)),
            }
            push(out, "}");
        }
        Event::Failed { base, code, reason } => {
            base_json(out, "failed", base);
            push(out, ",\"code\":");
            push_units(out, code);
            push(out, ",\"reason\":");
            push_units(out, reason);
            push(out, "}");
        }
        Event::Completed {
            base,
            verdict,
            corrected,
        } => {
            base_json(out, "completed", base);
            push(out, ",\"verdict\":");
            push_units(out, &units(verdict.verdict));
            push(out, ",\"summary\":");
            push_units(out, &verdict.summary);
            push(out, ",\"findings\":");
            findings_json(out, &verdict.findings);
            push(out, ",\"corrected\":");
            push(out, if *corrected { "true" } else { "false" });
            push(out, "}");
        }
    }
}

const TOO_LARGE: &str = r#"{"error":"too_large"}"#;
const BAD_INPUT: &str = r#"{"error":"input"}"#;
const OPAQUE: &str = r#"{"error":"opaque"}"#;

/// The wasm call: input `u8(op)` and UTF-8 tagged JSON. Op 1: `T` (readVerdict's argument);
/// reply `{"verdict":{…}}` or `{"invalid":"<code>"}`. Op 2: `[T type, T data]`
/// (boundReviewEvent); reply `{"event":{…}}`. Status 1 refuses with
/// `{"error":"input"|"too_large"|"opaque"}`.
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
    let mut out = Vec::new();
    match op {
        1 => {
            let Some(raw) = decode(&tree) else {
                return (1, BAD_INPUT.to_owned());
            };
            match read_verdict(&raw) {
                Err(Fault::Opaque) => return (1, OPAQUE.to_owned()),
                Ok(Err(why)) => {
                    push(&mut out, "{\"invalid\":\"");
                    push(&mut out, why.code());
                    push(&mut out, "\"}");
                }
                Ok(Ok(v)) => {
                    push(&mut out, "{\"verdict\":");
                    verdict_json(&mut out, &v);
                    push(&mut out, "}");
                }
            }
        }
        2 => {
            let Value::Arr(pair) = &tree else {
                return (1, BAD_INPUT.to_owned());
            };
            let [kind, data] = pair.as_slice() else {
                return (1, BAD_INPUT.to_owned());
            };
            let (Some(kind), Some(data)) = (decode(kind), decode(data)) else {
                return (1, BAD_INPUT.to_owned());
            };
            match bound_review_event(&kind, &data) {
                Err(Fault::Opaque) => return (1, OPAQUE.to_owned()),
                Ok(e) => {
                    push(&mut out, "{\"event\":");
                    event_json(&mut out, &e);
                    push(&mut out, "}");
                }
            }
        }
        _ => return (1, BAD_INPUT.to_owned()),
    }
    // Every string went through JSON escaping (lone surrogates as \u escapes), so this is UTF-8.
    match String::from_utf8(out) {
        Ok(s) => (0, s),
        Err(_) => (1, BAD_INPUT.to_owned()),
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

    #[test]
    fn reads_a_verdict() {
        let raw = obj(&[
            ("verdict", s("request_changes")),
            ("summary", s("  ok\u{202e}  ")),
            (
                "findings",
                Js::Arr(vec![obj(&[("severity", s("minor")), ("message", s("m"))])]),
            ),
        ]);
        let v = read_verdict(&raw).unwrap().unwrap();
        assert_eq!(v.summary, units("ok"));
        assert_eq!(v.findings[0].file, None);
    }

    #[test]
    fn opaque_where_the_js_would_look_inside() {
        assert_eq!(read_verdict(&Js::Opaque), Err(Fault::Opaque));
        assert_eq!(read_verdict(&Js::Func), Ok(Err(Invalid::Fields)));
        let raw = obj(&[
            ("verdict", s("approve")),
            ("summary", s("x")),
            ("findings", Js::Arr(vec![Js::Opaque])),
        ]);
        assert_eq!(read_verdict(&raw), Err(Fault::Opaque));
        // As a leaf it is just an object.
        let raw = obj(&[
            ("verdict", Js::Opaque),
            ("summary", s("x")),
            ("findings", Js::Arr(vec![])),
        ]);
        assert_eq!(read_verdict(&raw), Ok(Err(Invalid::Verdict)));
        // boundReviewEvent keeps a failed review for an opaque finding, as for any throw.
        let data = obj(&[
            ("verdict", s("approve")),
            ("summary", s("x")),
            ("findings", Js::Arr(vec![Js::Opaque])),
        ]);
        assert!(matches!(
            bound_review_event(&s("review.completed"), &data),
            Ok(Event::Failed { .. })
        ));
        assert_eq!(
            bound_review_event(&s("review.completed"), &Js::Opaque),
            Err(Fault::Opaque)
        );
    }

    #[test]
    fn clean_counts_code_points() {
        let mut t = units("a");
        t.extend([0xd83d, 0xde00, 0xd800, 0x62]);
        assert_eq!(clean(&Js::Str(t.clone()), 2), vec![0x61, 0xd83d, 0xde00]);
        assert_eq!(clean(&Js::Str(t), 3), vec![0x61, 0xd83d, 0xde00, 0xd800]);
        assert!(clean(&Js::Num(1.0), 5).is_empty());
    }

    #[test]
    fn negative_zero_files() {
        let e = bound_review_event(&s("review.requested"), &obj(&[("files", Js::Num(-0.0))]));
        let mut out = Vec::new();
        event_json(&mut out, &e.unwrap());
        assert!(String::from_utf8(out).unwrap().ends_with(",\"files\":-0}"));
    }

    #[test]
    fn call_shapes() {
        assert_eq!(call(b""), (1, BAD_INPUT.to_owned()));
        assert_eq!(call(b"\x01[\"x\"]"), (1, OPAQUE.to_owned()));
        assert_eq!(call(b"\x01{}"), (1, BAD_INPUT.to_owned()));
        assert_eq!(call(b"\x01null"), (0, r#"{"invalid":"fields"}"#.to_owned()));
        assert_eq!(
            call(b"\x02[\"review.failed\",[\"u\"]]"),
            (
                0,
                r#"{"event":{"status":"failed","reviewer":"planner","baseSha":null,"headSha":null,"code":"failed","reason":"The review did not finish."}}"#
                    .to_owned()
            )
        );
        assert_eq!(
            call(b"\x01[\"o\",[[\"a\",1],[\"a\",2]]]"),
            (1, BAD_INPUT.to_owned())
        );
        let big = vec![b' '; MAX_INPUT_BYTES + 1];
        assert_eq!(call(&big), (1, TOO_LARGE.to_owned()));
    }
}
