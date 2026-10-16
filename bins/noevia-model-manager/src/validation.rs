//! FastAPI's 422 validation bodies, byte for byte (noevia#1908).
//!
//! FastAPI writes each error as `{"type","loc","msg","input"[,"ctx"]}` in that order, through
//! `json.dumps(ensure_ascii=False, separators=(",", ":"))`. A `serde_json::Value` sorts object
//! keys, so these bodies are assembled as text and handed out as a `RawValue` that serialises
//! verbatim (callers keep wrapping it in `Json(...)`).
use serde_json::value::RawValue;

/// A JSON string literal as Python's `json.dumps(ensure_ascii=False)` writes it (the same escapes
/// as serde_json: `\"`, `\\`, `\n`, `\r`, `\t`, `\b`, `\f`, other controls as `\u00XX`).
pub fn string(text: &str) -> String {
    serde_json::to_string(text).unwrap_or_else(|_| "\"\"".to_owned())
}

/// One error object. `loc` items and `input` are already JSON text; `ctx_error` is the
/// `ctx.error` text when present.
pub fn error(
    kind: &str,
    loc: &[String],
    msg: &str,
    input: &str,
    ctx_error: Option<&str>,
) -> String {
    let mut out = format!(
        "{{\"type\":{},\"loc\":[{}],\"msg\":{},\"input\":{}",
        string(kind),
        loc.join(","),
        string(msg),
        input
    );
    if let Some(ctx) = ctx_error {
        out.push_str(&format!(",\"ctx\":{{\"error\":{}}}", string(ctx)));
    }
    out.push('}');
    out
}

/// `{"detail":[errors...]}` as a raw value; `None` only if the text were not JSON.
pub fn body(errors: &[String]) -> Box<RawValue> {
    let text = format!("{{\"detail\":[{}]}}", errors.join(","));
    RawValue::from_string(text)
        .unwrap_or_else(|_| RawValue::from_string("{\"detail\":[]}".to_owned()).unwrap_or_default())
}

/// `loc` entry for a string key.
pub fn key(name: &str) -> String {
    string(name)
}

/// A required field that is absent: `{"type":"missing",...,"input":null}`.
pub fn missing(loc: &[String]) -> String {
    error("missing", loc, "Field required", "null", None)
}

/// A query or path value that does not parse (`int_parsing`, `bool_parsing`); the input echoes
/// the raw text.
pub fn parsing(kind: &str, loc: &[String], msg: &str, raw: &str) -> Box<RawValue> {
    body(&[error(kind, loc, msg, &string(raw), None)])
}
