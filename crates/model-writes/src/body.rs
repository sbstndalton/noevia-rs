//! `http.cjs` `readJson(req)` as the models routes call it: the body read up to 1 MiB, decoded as
//! UTF-8 (invalid bytes as U+FFFD), `{}` when empty, else `JSON.parse`. The routes answer every
//! failure (too large included) with 400 `invalid JSON`.

use js_json::{wtf16, JValue};

/// `readBody`'s default limit.
pub const LIMIT: usize = 1024 * 1024;

/// A request body as `readJson` reads it.
#[derive(Debug, Clone, PartialEq)]
pub enum Read {
    Value(JValue),
    /// Node throws (the routes' 400).
    Invalid,
    /// Node reads it but the port cannot hold it (a number outside the `f64` range, nesting deeper
    /// than 128, text in U+10F800..U+10FFFF): the request goes to core unchanged.
    Unsupported,
}

/// The request body as read (`bytes` is `None` when it was larger than [`LIMIT`]).
pub fn read_json(bytes: Option<&[u8]>) -> Read {
    let Some(bytes) = bytes.filter(|b| b.len() <= LIMIT) else {
        return Read::Invalid;
    };
    if bytes.is_empty() {
        return Read::Value(JValue::Obj(Vec::new()));
    }
    match wtf16::parse(&String::from_utf8_lossy(bytes)) {
        Ok(v) => Read::Value(v),
        Err(wtf16::Refusal::Unsupported) => Read::Unsupported,
        Err(_) => Read::Invalid,
    }
}

/// The models routes' answer to a body that does not read.
pub fn invalid_json() -> JValue {
    JValue::obj([("error", JValue::from("invalid JSON"))])
}

/// The models write prelude's answer to a member (`routes/models.cjs`: any method but GET and HEAD
/// under `/api/models/`).
pub fn administrator_required() -> JValue {
    JValue::obj([("error", JValue::from("administrator required"))])
}
