//! noevia-core's `server/stream-guard.cjs` in Rust (sbstndalton/noevia#516; the Executor guard
//! of #704 is its first caller): an incremental JSON validator over a restricted JSON-schema
//! subset that reports the first violation as soon as it is decidable, and the bounded
//! correction request built from it.
//!
//! Same decisions as the JS, chunk by chunk: the same accept or reject, the same first
//! violation (message and path, UTF-16 code unit for code unit), after the same chunk. Text is
//! read as UTF-16 code units, as the JS indexes strings, so lone surrogates, a surrogate pair
//! split across two chunks and `\u` escapes behave exactly as they do there; maxBytes counts
//! each chunk's `Buffer.byteLength` before reading it, as the JS does (so, like the JS, a pair
//! split across chunks counts six bytes rather than four).
//!
//! Stricter than the JS, as refusals ([`Error`]), never as different answers: schema shapes
//! outside the subset's plain JSON (see [`schema`]), `maxDepth` beyond ±[`MAX_DEPTH_CAP`],
//! `maxBytes` beyond ±[`MAX_GUARD_BYTES`] (the default; no caller asks for more), and options
//! that are not integers (the loader refuses NaN and Infinity).
//!
//! The state between chunks is bytes the caller holds ([`State::encode`]); see [`state`].
//! `runGuardedStream` (the async producer loop with abort and one retry) stays in JS.
//!
//! # Wire format (`call`)
//!
//! Every request starts with an op byte. Integers are little-endian; "units" are UTF-16LE code
//! units; a schema is UTF-8 JSON (`JSON.stringify` of the JS schema).
//!
//! - `0` new: `f64 maxDepth`, `f64 maxBytes`, `u32 n`, schema(n). Reply: the empty result and
//!   a fresh state.
//! - `1` feed: `u32 n`, schema(n), `u32 m`, state(m), `u8 oversize`, then the chunk's units.
//!   `oversize = 1` (with no units) stands for a chunk longer than [`MAX_GUARD_BYTES`] units.
//! - `2` end: `u32 n`, schema(n), `u32 m`, state(m).
//! - `3` check (`feed(text) || end()` on a fresh validator, what code-tool-schemas.cjs does):
//!   `f64 maxDepth`, `f64 maxBytes`, `u32 n`, schema(n), `u8 oversize`, then the text's units.
//!   Reply: the result, no state.
//! - `4` correction (`buildCorrectionRequest({ message: message.slice(0, clip), path })`):
//!   `u32 clip` (`u32::MAX`: none), `u8 hasPath`, `u32 k`, message units(k), then the path's
//!   units. Reply: the request as ASCII JSON, no frame.
//!
//! Reply to ops 0 to 3, status 0: `u32 j`, ASCII JSON(j)
//! `{"violation":null|{"message":…,"path":…,"reason":…},"done":bool}`, then the state bytes (none
//! for check). Every string is ASCII JSON whose `\uXXXX` escapes carry the exact units. A
//! refusal is status 1 with `{"error":"too_large"|"input_shape"|"schema"|"state"|"options"}`:
//! a fixed code, never any of the input.

mod json;
pub mod schema;
pub mod state;
mod validator;

pub use schema::{Schema, MAX_SCHEMA_BYTES, MAX_SCHEMA_NESTING};
pub use state::MAX_STATE_BYTES;
pub use validator::{
    is_ws as is_whitespace, utf8_len, Reason, State, Validator, Violation, DEFAULT_MAX_BYTES,
    DEFAULT_MAX_DEPTH, MAX_DEPTH_CAP, MAX_GUARD_BYTES,
};

/// Most units a message or path given to the correction op may have. A message longer than the
/// clip can be cut to the clip by the caller first without changing the reply.
pub const MAX_CORRECTION_UNITS: usize = 1024 * 1024;

/// Largest request [`call`] takes: a full state, a schema and the longest chunk, plus framing.
pub const MAX_INPUT_BYTES: usize =
    MAX_STATE_BYTES + MAX_SCHEMA_BYTES + 2 * (MAX_GUARD_BYTES as usize) + 64;

/// A refusal; its [`Error::code`] is all a caller learns.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    TooLarge,
    InputShape,
    Schema,
    State,
    Options,
}

impl Error {
    pub fn code(self) -> &'static str {
        match self {
            Error::TooLarge => "too_large",
            Error::InputShape => "input_shape",
            Error::Schema => "schema",
            Error::State => "state",
            Error::Options => "options",
        }
    }

    pub fn json(self) -> Vec<u8> {
        format!("{{\"error\":\"{}\"}}", self.code()).into_bytes()
    }
}

/// An option as the loader sends it: a finite integer of at most 2^53 in magnitude.
fn option(v: f64) -> Result<i64, Error> {
    if !v.is_finite() || v.fract() != 0.0 || v.abs() > 9_007_199_254_740_991.0 {
        return Err(Error::Options);
    }
    // Exact: a safe integer.
    Ok(v as i64)
}

struct In<'a> {
    b: &'a [u8],
}

impl<'a> In<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8], Error> {
        let (head, rest) = self.b.split_at_checked(n).ok_or(Error::InputShape)?;
        self.b = rest;
        Ok(head)
    }
    fn arr<const N: usize>(&mut self) -> Result<[u8; N], Error> {
        <[u8; N]>::try_from(self.take(N)?).map_err(|_| Error::InputShape)
    }
    fn u8(&mut self) -> Result<u8, Error> {
        Ok(self.arr::<1>()?[0])
    }
    fn u32(&mut self) -> Result<u32, Error> {
        Ok(u32::from_le_bytes(self.arr()?))
    }
    fn f64(&mut self) -> Result<f64, Error> {
        Ok(f64::from_le_bytes(self.arr()?))
    }
    fn block(&mut self) -> Result<&'a [u8], Error> {
        let n = self.u32()? as usize;
        self.take(n)
    }
    fn flag(&mut self) -> Result<bool, Error> {
        match self.u8()? {
            0 => Ok(false),
            1 => Ok(true),
            _ => Err(Error::InputShape),
        }
    }
}

fn units(b: &[u8]) -> Result<Vec<u16>, Error> {
    let (pairs, rest) = b.as_chunks::<2>();
    if !rest.is_empty() {
        return Err(Error::InputShape);
    }
    Ok(pairs.iter().map(|&p| u16::from_le_bytes(p)).collect())
}

/// The chunk after the oversize flag: oversize chunks carry no units, others at most the cap.
fn chunk(r: &mut In<'_>) -> Result<Option<Vec<u16>>, Error> {
    let oversize = r.flag()?;
    let rest = r.take(r.b.len())?;
    if oversize {
        return if rest.is_empty() {
            Ok(None)
        } else {
            Err(Error::InputShape)
        };
    }
    if rest.len() / 2 > MAX_GUARD_BYTES as usize {
        return Err(Error::TooLarge);
    }
    units(rest).map(Some)
}

fn result_json(state: &State) -> Vec<u8> {
    let mut out = b"{\"violation\":".to_vec();
    match state.violation() {
        None => out.extend_from_slice(b"null"),
        Some(v) => {
            out.extend_from_slice(b"{\"message\":");
            json::push_str(&mut out, &v.message);
            out.extend_from_slice(b",\"path\":");
            json::push_str(&mut out, &v.path);
            out.extend_from_slice(b",\"reason\":\"");
            out.extend_from_slice(v.reason.code().as_bytes());
            out.extend_from_slice(b"\"}");
        }
    }
    out.extend_from_slice(if state.is_done() {
        b",\"done\":true}"
    } else {
        b",\"done\":false}"
    });
    out
}

fn framed_reply(state: &State, with_state: bool) -> Vec<u8> {
    let j = result_json(state);
    let mut out = Vec::with_capacity(4 + j.len());
    out.extend_from_slice(&u32::try_from(j.len()).unwrap_or(u32::MAX).to_le_bytes());
    out.extend_from_slice(&j);
    if with_state {
        out.extend_from_slice(&state.encode());
    }
    out
}

fn feed_chunk(v: &mut Validator<'_>, c: Option<Vec<u16>>) {
    match c {
        Some(u) => v.feed(&u),
        None => v.feed_oversize(),
    };
}

/// `buildCorrectionRequest({ message: message.slice(0, clip), path: path || null })` as ASCII
/// JSON: `{"type":"schema_violation_correction","violation":{"message":…,"path":…|null}}`.
pub fn correction(message: &[u16], path: Option<&[u16]>, clip: Option<usize>) -> Vec<u8> {
    let message = match clip {
        Some(n) => message.get(..n).unwrap_or(message),
        None => message,
    };
    let mut out = b"{\"type\":\"schema_violation_correction\",\"violation\":{\"message\":".to_vec();
    json::push_str(&mut out, message);
    out.extend_from_slice(b",\"path\":");
    match path {
        Some(p) if !p.is_empty() => json::push_str(&mut out, p),
        _ => out.extend_from_slice(b"null"),
    }
    out.extend_from_slice(b"}}");
    out
}

/// One request in the wire format above; `(status, reply)`.
pub fn call(input: &[u8]) -> (u32, Vec<u8>) {
    match call_inner(input) {
        Ok(reply) => (0, reply),
        Err(e) => (1, e.json()),
    }
}

fn call_inner(input: &[u8]) -> Result<Vec<u8>, Error> {
    if input.len() > MAX_INPUT_BYTES {
        return Err(Error::TooLarge);
    }
    let mut r = In { b: input };
    match r.u8()? {
        op @ (0 | 3) => {
            let max_depth = option(r.f64()?)?;
            let max_bytes = option(r.f64()?)?;
            let schema = Schema::parse(r.block()?)?;
            let state = State::new(max_depth, max_bytes)?;
            if op == 0 {
                if !r.b.is_empty() {
                    return Err(Error::InputShape);
                }
                return Ok(framed_reply(&state, true));
            }
            let c = chunk(&mut r)?;
            let mut v = Validator::new(&schema, state);
            feed_chunk(&mut v, c);
            v.end();
            Ok(framed_reply(v.state(), false))
        }
        op @ (1 | 2) => {
            let schema = Schema::parse(r.block()?)?;
            let state = State::decode(r.block()?, &schema)?;
            let mut v = Validator::new(&schema, state);
            if op == 1 {
                let c = chunk(&mut r)?;
                feed_chunk(&mut v, c);
            } else {
                if !r.b.is_empty() {
                    return Err(Error::InputShape);
                }
                v.end();
            }
            Ok(framed_reply(v.state(), true))
        }
        4 => {
            let clip = match r.u32()? {
                u32::MAX => None,
                n => Some(n as usize),
            };
            let has_path = r.flag()?;
            let k = r.u32()? as usize;
            let message = units(r.take(k.checked_mul(2).ok_or(Error::InputShape)?)?)?;
            let path = units(r.take(r.b.len())?)?;
            if message.len() > MAX_CORRECTION_UNITS || path.len() > MAX_CORRECTION_UNITS {
                return Err(Error::TooLarge);
            }
            if !has_path && !path.is_empty() {
                return Err(Error::InputShape);
            }
            Ok(correction(&message, has_path.then_some(&path[..]), clip))
        }
        _ => Err(Error::InputShape),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    fn le(units: &str) -> Vec<u8> {
        units.encode_utf16().flat_map(u16::to_le_bytes).collect()
    }

    fn new_req(schema: &str) -> Vec<u8> {
        let mut v = vec![0u8];
        v.extend_from_slice(&64f64.to_le_bytes());
        v.extend_from_slice(&2097152f64.to_le_bytes());
        v.extend_from_slice(&(schema.len() as u32).to_le_bytes());
        v.extend_from_slice(schema.as_bytes());
        v
    }

    fn split(reply: &[u8]) -> (String, Vec<u8>) {
        let j = u32::from_le_bytes(reply[..4].try_into().unwrap()) as usize;
        (
            String::from_utf8(reply[4..4 + j].to_vec()).unwrap(),
            reply[4 + j..].to_vec(),
        )
    }

    fn feed_req(schema: &str, state: &[u8], text: &str) -> Vec<u8> {
        let mut v = vec![1u8];
        v.extend_from_slice(&(schema.len() as u32).to_le_bytes());
        v.extend_from_slice(schema.as_bytes());
        v.extend_from_slice(&(state.len() as u32).to_le_bytes());
        v.extend_from_slice(state);
        v.push(0);
        v.extend_from_slice(&le(text));
        v
    }

    #[test]
    fn new_feed_end_round_trip() {
        let schema = r#"{"type":"object","properties":{"a":{"enum":["xé"]}}}"#;
        let (s, reply) = call(&new_req(schema));
        assert_eq!(s, 0);
        let (j, st) = split(&reply);
        assert_eq!(j, r#"{"violation":null,"done":false}"#);
        let (s, reply) = call(&feed_req(schema, &st, "{\"a\":\"x"));
        assert_eq!(s, 0);
        let (j, st) = split(&reply);
        assert_eq!(j, r#"{"violation":null,"done":false}"#);
        let (s, reply) = call(&feed_req(schema, &st, "\u{e9}\"}"));
        assert_eq!(s, 0);
        let (j, st) = split(&reply);
        assert_eq!(j, r#"{"violation":null,"done":true}"#);
        let mut end = vec![2u8];
        end.extend_from_slice(&(schema.len() as u32).to_le_bytes());
        end.extend_from_slice(schema.as_bytes());
        end.extend_from_slice(&(st.len() as u32).to_le_bytes());
        end.extend_from_slice(&st);
        assert_eq!(split(&call(&end).1).0, r#"{"violation":null,"done":true}"#);
        let (s, reply) = call(&feed_req(schema, &[], "{\"a\":\"y"));
        assert_eq!((s, reply), (1, br#"{"error":"state"}"#.to_vec()));
    }

    #[test]
    fn violation_reply_escapes_units() {
        let mut req = vec![3u8];
        req.extend_from_slice(&64f64.to_le_bytes());
        req.extend_from_slice(&2097152f64.to_le_bytes());
        let schema = br#"{"additionalProperties":false,"properties":{}}"#;
        req.extend_from_slice(&(schema.len() as u32).to_le_bytes());
        req.extend_from_slice(schema);
        req.push(0);
        for u in "{\""
            .encode_utf16()
            .chain([0xd800])
            .chain("\"".encode_utf16())
        {
            req.extend_from_slice(&u.to_le_bytes());
        }
        let (s, reply) = call(&req);
        assert_eq!(s, 0);
        assert_eq!(
            split(&reply),
            (
                r#"{"violation":{"message":"Unknown property '\ud800' at $","path":"$.\ud800","reason":"unknown_property"},"done":false}"#.to_owned(),
                vec![]
            )
        );
    }

    #[test]
    fn correction_clips_units() {
        let m: Vec<u16> = "ab\u{1f600}c".encode_utf16().collect();
        assert_eq!(
            String::from_utf8(correction(&m, Some(&[0x24]), Some(3))).unwrap(),
            r#"{"type":"schema_violation_correction","violation":{"message":"ab\ud83d","path":"$"}}"#
        );
        assert_eq!(
            String::from_utf8(correction(&m, Some(&[]), None)).unwrap(),
            r#"{"type":"schema_violation_correction","violation":{"message":"ab\ud83d\ude00c","path":null}}"#
        );
    }

    #[test]
    fn refusals_are_fixed_codes() {
        let code = |input: &[u8]| call(input);
        assert_eq!(code(&[]), (1, br#"{"error":"input_shape"}"#.to_vec()));
        assert_eq!(code(&[9]), (1, br#"{"error":"input_shape"}"#.to_vec()));
        let mut bad = new_req("{}");
        bad[1..9].copy_from_slice(&f64::NAN.to_le_bytes());
        assert_eq!(code(&bad), (1, br#"{"error":"options"}"#.to_vec()));
        let mut big = new_req("{}");
        big[9..17].copy_from_slice(&(MAX_GUARD_BYTES as f64 + 1.0).to_le_bytes());
        assert_eq!(code(&big), (1, br#"{"error":"options"}"#.to_vec()));
        assert_eq!(
            code(&new_req(r#"{"type":7}"#)),
            (1, br#"{"error":"schema"}"#.to_vec())
        );
        assert_eq!(
            code(&vec![0u8; MAX_INPUT_BYTES + 1]),
            (1, br#"{"error":"too_large"}"#.to_vec())
        );
    }
}
