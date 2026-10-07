//! MCP response framing (sbstndalton/noevia#980): a port of noevia-core's `server/mcp.cjs`
//! `parseRpcBody` (a JSON-RPC reply as plain JSON or as a `text/event-stream`, matched to the
//! request id; server-initiated requests and notifications are not replies) and
//! `resolveSchemaRefs`/`inlineRefs` (local `$ref` inlining under the same depth and size budget).
//! The input is a third-party MCP server's output: untrusted.
//!
//! Text crosses as UTF-16 code units, the JS string itself, so a raw lone surrogate means what it
//! means to JS. [`json`] accepts exactly what `JSON.parse` accepts. Nothing here builds JS values:
//! [`parse_rpc_body`] answers with the span of the selected message (the host turns that text
//! into a value with its own `JSON.parse`, which yields the value `JSON.parse` would have given
//! the original, by construction: same lexemes, same duplicate keys, same `__proto__` members),
//! and [`schema::resolve_schema_refs`] answers with an encoding of the resolved tree (see there).
//!
//! What stays in JS: the transport, sessions, headers and tool policy, the `String(contentType)`
//! conversion (it may run user code), every error message (they interpolate JS values with JS's
//! `ToString`), and, for a body `JSON.parse` rejects, the `SyntaxError` itself (V8's text names
//! the offending input and differs between versions; the host re-raises the runtime's own).

pub mod json;
pub mod schema;

use json::{Scalar, Sink};
use std::ops::Range;

/// `MAX_RESPONSE_BYTES` in mcp.cjs: `readBodyCapped` stops at 8 MiB of body, and a body decoded
/// from at most that many bytes is at most that many UTF-16 units. Longer text is refused.
pub const MAX_BODY_UNITS: usize = 8 * 1024 * 1024;

/// The request id the reply must carry, by JS type (`===` semantics).
#[derive(Clone, Debug, PartialEq)]
pub enum Expected {
    /// A value no parsed JSON value is `===` to (undefined, an object, a symbol, a bigint, ...).
    Never,
    /// `null`
    Null,
    /// A boolean.
    Bool(bool),
    /// A number (`NaN` never matches; `0 === -0`).
    Number(f64),
    /// A string, as code units.
    String(Vec<u16>),
}

/// What `parseRpcBody` does with a body.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// Returns the message at this span of the text.
    Reply(Range<usize>),
    /// Plain JSON whose own `id` is not the request's: throws `reply id … does not match`.
    Mismatch(Range<usize>),
    /// Event stream with no frame carrying an id: `no JSON-RPC message in event stream`.
    NoMessage,
    /// Event stream with ids, none a reply to this request: `no reply to request …`.
    OtherTraffic,
    /// Plain body that `JSON.parse` rejects: the host re-raises its own `SyntaxError`.
    InvalidJson,
}

/// The text is longer than [`MAX_BODY_UNITS`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TooLarge;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Null,
    True,
    False,
    Number,
    String,
    Container,
}

#[derive(Clone, Copy)]
struct Val {
    kind: Kind,
    start: usize,
    end: usize,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Field {
    Id,
    Method,
}

/// Records the top-level value's kind and, for an object, its last `id` and `method` members.
#[derive(Default)]
struct Top {
    depth: usize,
    object: bool,
    pending: Option<Field>,
    open: Option<(Field, usize)>,
    id: Option<Val>,
    method: Option<Val>,
}

impl Top {
    fn set(&mut self, f: Field, v: Val) {
        match f {
            Field::Id => self.id = Some(v),
            Field::Method => self.method = Some(v),
        }
    }
}

struct TopSink<'a> {
    s: &'a [u16],
    top: Top,
}

impl Sink for TopSink<'_> {
    fn begin(&mut self, array: bool, at: usize) {
        let t = &mut self.top;
        if t.depth == 0 {
            t.object = !array;
        } else if t.depth == 1 && t.object {
            if let Some(f) = t.pending.take() {
                t.open = Some((f, at));
            }
        }
        t.depth += 1;
    }
    fn key(&mut self, start: usize, end: usize) {
        if self.top.depth == 1 {
            self.top.pending = if json::string_is(self.s, start, end, "id") {
                Some(Field::Id)
            } else if json::string_is(self.s, start, end, "method") {
                Some(Field::Method)
            } else {
                None
            };
        }
    }
    fn scalar(&mut self, kind: Scalar, start: usize, end: usize) {
        let t = &mut self.top;
        if t.depth == 1 && t.object {
            if let Some(f) = t.pending.take() {
                let kind = match kind {
                    Scalar::Null => Kind::Null,
                    Scalar::True => Kind::True,
                    Scalar::False => Kind::False,
                    Scalar::Number => Kind::Number,
                    Scalar::String => Kind::String,
                };
                t.set(f, Val { kind, start, end });
            }
        }
    }
    fn end(&mut self, end: usize) {
        let t = &mut self.top;
        t.depth = t.depth.saturating_sub(1);
        if t.depth == 1 {
            if let Some((f, start)) = t.open.take() {
                t.set(
                    f,
                    Val {
                        kind: Kind::Container,
                        start,
                        end,
                    },
                );
            }
        }
    }
}

/// Scan one JSON text; `None` when `JSON.parse` rejects it.
fn scan(s: &[u16]) -> Option<Top> {
    let mut sink = TopSink {
        s,
        top: Top::default(),
    };
    json::parse(s, &mut sink).then_some(sink.top)
}

/// `msg.id === expected` for the id member `v` of the scanned text `s`.
fn id_matches(s: &[u16], v: Val, expected: &Expected) -> bool {
    match (expected, v.kind) {
        (Expected::Null, Kind::Null) => true,
        (Expected::Bool(true), Kind::True) | (Expected::Bool(false), Kind::False) => true,
        (Expected::Number(x), Kind::Number) => json::number_value(s, v.start, v.end) == *x,
        (Expected::String(u), Kind::String) => json::decode_string(s, v.start, v.end) == *u,
        _ => false,
    }
}

/// JS truthiness of the member `v` (absent members are `undefined`: falsy).
fn truthy(s: &[u16], v: Option<Val>) -> bool {
    let Some(v) = v else { return false };
    match v.kind {
        Kind::Null | Kind::False => false,
        Kind::True | Kind::Container => true,
        Kind::String => v.end - v.start > 2,
        Kind::Number => {
            let x = json::number_value(s, v.start, v.end);
            x != 0.0 && !x.is_nan()
        }
    }
}

/// `String.prototype.trim`'s set: WhiteSpace and LineTerminator (ECMA-262 12.2, 12.3).
pub const fn is_js_space(c: u16) -> bool {
    matches!(
        c,
        0x09 | 0x0a | 0x0b | 0x0c | 0x0d | 0x20 | 0xa0 | 0x1680 | 0x2000
            ..=0x200a | 0x2028 | 0x2029 | 0x202f | 0x205f | 0x3000 | 0xfeff
    )
}

/// `text.split(/\r?\n/)`: the ranges of each line.
fn lines(text: &[u16]) -> impl Iterator<Item = Range<usize>> + '_ {
    let mut start = 0;
    let mut done = false;
    std::iter::from_fn(move || {
        if done {
            return None;
        }
        let rest = text.get(start..).unwrap_or(&[]);
        match rest.iter().position(|&c| c == 0x0a) {
            Some(p) => {
                let nl = start + p;
                let end = if nl > start && text.get(nl - 1) == Some(&0x0d) {
                    nl - 1
                } else {
                    nl
                };
                let r = start..end;
                start = nl + 1;
                Some(r)
            }
            None => {
                done = true;
                Some(start..text.len())
            }
        }
    })
}

const DATA: [u16; 5] = [0x64, 0x61, 0x74, 0x61, 0x3a]; // "data:"

/// mcp.cjs `parseRpcBody(contentType, text, expectedId)`, where `sse` is
/// `String(contentType || '').includes('text/event-stream')`.
pub fn parse_rpc_body(sse: bool, text: &[u16], expected: &Expected) -> Result<Outcome, TooLarge> {
    if text.len() > MAX_BODY_UNITS {
        return Err(TooLarge);
    }
    if !sse {
        let Some(top) = scan(text) else {
            return Ok(Outcome::InvalidJson);
        };
        let whole = 0..text.len();
        return Ok(match top.id {
            Some(id) if top.object && !id_matches(text, id, expected) => Outcome::Mismatch(whole),
            _ => Outcome::Reply(whole),
        });
    }
    let mut found = None;
    let mut saw_other = false;
    for line in lines(text) {
        let Some(l) = text.get(line.clone()) else {
            continue;
        };
        if !l.starts_with(&DATA) {
            continue;
        }
        let mut a = line.start + DATA.len();
        let mut b = line.end;
        while a < b && text.get(a).is_some_and(|&c| is_js_space(c)) {
            a += 1;
        }
        while b > a && text.get(b - 1).is_some_and(|&c| is_js_space(c)) {
            b -= 1;
        }
        if a == b {
            continue;
        }
        let payload = text.get(a..b).unwrap_or(&[]);
        // A partial or non-JSON frame, a non-object, or a notification (no id): keep looking.
        let Some(top) = scan(payload) else { continue };
        if !top.object {
            continue;
        }
        let Some(id) = top.id else { continue };
        if id_matches(payload, id, expected) && !truthy(payload, top.method) {
            found = Some(a..b);
        } else {
            saw_other = true;
        }
    }
    Ok(match found {
        Some(r) => Outcome::Reply(r),
        None if saw_other => Outcome::OtherTraffic,
        None => Outcome::NoMessage,
    })
}

/// The reply bytes for `outcome`: a tag (0 reply, 1 mismatch, 2 no message, 3 other traffic,
/// 4 invalid JSON), then for 0 and 1 the message text as UTF-8 (lone surrogates, which can only
/// sit inside string literals, as `\uXXXX`).
pub fn rpc_reply(text: &[u16], outcome: &Outcome) -> Vec<u8> {
    let (tag, span) = match outcome {
        Outcome::Reply(r) => (0u8, Some(r)),
        Outcome::Mismatch(r) => (1, Some(r)),
        Outcome::NoMessage => (2, None),
        Outcome::OtherTraffic => (3, None),
        Outcome::InvalidJson => (4, None),
    };
    let mut out = vec![tag];
    if let Some(r) = span {
        json::push_utf8_escaped(&mut out, text.get(r.clone()).unwrap_or(&[]));
    }
    out
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    fn u(s: &str) -> Vec<u16> {
        s.encode_utf16().collect()
    }

    fn run(sse: bool, t: &str, id: impl Into<f64>) -> (u8, String) {
        let text = u(t);
        let o = parse_rpc_body(sse, &text, &Expected::Number(id.into())).unwrap();
        let r = rpc_reply(&text, &o);
        (r[0], String::from_utf8(r[1..].to_vec()).unwrap())
    }

    #[test]
    fn json_body() {
        assert_eq!(run(false, r#"{"id":1,"result":2}"#, 1).0, 0);
        assert_eq!(run(false, r#"{"id":2}"#, 1).0, 1);
        assert_eq!(run(false, r#"{"id":1,"id":2}"#, 2).0, 0);
        assert_eq!(run(false, r#"{"id":1.0e0}"#, 1).0, 0);
        assert_eq!(run(false, r#"[{"id":2}]"#, 1).0, 0);
        assert_eq!(run(false, "5", 1).0, 0);
        assert_eq!(run(false, "", 1).0, 4);
        assert_eq!(run(false, "{\"id\":-0}", 0.0).0, 0);
    }

    #[test]
    fn event_stream() {
        let t = "event: message\r\ndata: {\"id\":7,\"method\":\"sampling\"}\n: comment\ndata:{\"id\":1,\"result\":{}}\ndata: {\"id\":1,\"result\":2}";
        assert_eq!(run(true, t, 1), (0, "{\"id\":1,\"result\":2}".into()));
        assert_eq!(run(true, "data: {\"id\":7,\"method\":\"x\"}\n", 7).0, 3);
        assert_eq!(run(true, "data: {\"id\":7,\"method\":\"\"}\n", 7).0, 0);
        assert_eq!(run(true, "data: {\"jsonrpc\":\"2.0\"}\n\n", 7).0, 2);
        assert_eq!(run(true, "data: {\"id\":1\ndata: }\n", 1).0, 2);
        assert_eq!(run(true, "DATA: {\"id\":1}", 1).0, 2);
        assert_eq!(run(true, "data: \u{a0}{\"id\":1}\u{feff}\r\r", 1).0, 0);
    }

    #[test]
    fn caps() {
        let big = vec![0x20u16; MAX_BODY_UNITS + 1];
        assert_eq!(parse_rpc_body(false, &big, &Expected::Never), Err(TooLarge));
    }
}
