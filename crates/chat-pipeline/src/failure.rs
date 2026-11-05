//! What a failed engine reply shows the person (core chat.cjs `providerFailureText`, chat-context.cjs
//! `providerError`, chat-template-caps.cjs `failureText`; #918, #1002, #1547, noevia#2115).
use crate::text::as_str;
use js_json::{wtf16, JValue};

/// chat-context.cjs `CONTEXT_FULL_TEXT`.
pub const CONTEXT_FULL_TEXT: &str =
    "The model ran out of context space. Compact this chat or reduce its sources before retrying.";
/// chat-context.cjs `STREAM_FAILED_TEXT`.
pub const STREAM_FAILED_TEXT: &str = "The model stream failed. Check the backend before retrying.";
/// chat-context.cjs `STREAM_FAILED_PARTIAL_TEXT`.
pub const STREAM_FAILED_PARTIAL_TEXT: &str =
    "The model stream failed. Partial output was preserved; check the backend before retrying.";
/// chat.cjs `PROVIDER_ERROR_BODY_CAP`: how much of an error body is read.
pub const PROVIDER_ERROR_BODY_CAP: usize = 64 * 1024;
/// dav-parse-wasm.cjs `MAX_ERROR_UNITS`.
const MAX_ERROR_UNITS: usize = 256 * 1024;

/// http.cjs `readCappedText(response, cap).text` for a body already read whole: decoded as UTF-8
/// (`Buffer.toString`, a bad sequence becoming U+FFFD); past `cap` bytes cut there, one trailing
/// U+FFFD (a character the cut halved) dropped.
pub fn capped_text(body: &[u8], cap: usize) -> String {
    if body.len() <= cap {
        return String::from_utf8_lossy(body).into_owned();
    }
    let cut = String::from_utf8_lossy(body.get(..cap).unwrap_or(body)).into_owned();
    cut.strip_suffix('\u{FFFD}')
        .map_or(cut.clone(), str::to_owned)
}

/// `failureText(code, text, {partial})` with `text` as the module receives it.
fn failure_text(code: i64, text: &str, partial: bool) -> String {
    let fixed = if partial {
        STREAM_FAILED_PARTIAL_TEXT
    } else {
        STREAM_FAILED_TEXT
    };
    let Ok(status) = u32::try_from(code) else {
        // dav-parse-wasm.cjs refuses the input; classify answers null.
        return fixed.to_owned();
    };
    // The module's input: at most MAX_ERROR_UNITS units, made well formed, as UTF-8.
    let well: String = wtf16::slice(text, MAX_ERROR_UNITS)
        .chars()
        .map(|c| {
            if wtf16::lone_unit(c).is_some() {
                '\u{FFFD}'
            } else {
                c
            }
        })
        .collect();
    let c = provider_error::classify(status, &well);
    let r = c.reason;
    let kept = if partial {
        " Partial output was preserved."
    } else {
        ""
    };
    match c.kind {
        provider_error::Kind::ContextFull => CONTEXT_FULL_TEXT.to_owned(),
        provider_error::Kind::TemplateOrToolsUnsupported => format!(
            "The model's chat template cannot handle this request{}. Try another model, or ask without tools.",
            if r.is_empty() { String::new() } else { format!(": {r}") }
        ),
        provider_error::Kind::BackendDown => format!(
            "The model backend is not responding{}. Check that the engine is running, then retry.",
            if r.is_empty() { String::new() } else { format!(" ({r})") }
        ),
        provider_error::Kind::BadRequest => format!(
            "The model provider refused the request{}.{kept}",
            if r.is_empty() { String::new() } else { format!(": {r}") }
        ),
        provider_error::Kind::Other if !r.is_empty() => format!(
            "The model stream failed: {r}. {} the backend before retrying.",
            if partial { "Partial output was preserved; check" } else { "Check" }
        ),
        provider_error::Kind::Other => fixed.to_owned(),
    }
}

/// What a failed reply carried.
pub struct FailedReply<'a> {
    /// `response.status` when it is an integer.
    pub status: Option<i64>,
    /// The body as [`capped_text`] read it.
    pub detail: &'a str,
    /// The `x-noevia-gate-refusal` header is present and not empty.
    pub gate_refusal: bool,
    /// The `x-noevia-provider-message` header is exactly `1`.
    pub provider_message: bool,
}

fn parsed_error(detail: &str) -> Option<JValue> {
    wtf16::parse(detail).ok().map(|v| v.get("error").clone())
}

/// `String(v)` of a message, cut to 300 units; `None` when Node's `String()` would throw.
fn shown(v: &JValue) -> Option<String> {
    js_json::to_js_string(v).ok().map(|s| wtf16::slice(&s, 300))
}

/// core chat.cjs `providerFailureText(response, chatgptProvider, partial)`.
pub fn provider_failure_text(
    reply: &FailedReply<'_>,
    chatgpt_provider: bool,
    partial: bool,
) -> String {
    let code = reply.status.unwrap_or(500);
    let mut msg = failure_text(code, reply.detail, partial);
    if reply.gate_refusal {
        if let Some(e) = parsed_error(reply.detail) {
            if as_str(e.get("type")) == Some("gate_error") && e.get("message").truthy() {
                if let Some(m) = shown(e.get("message")) {
                    msg = m;
                }
            }
        }
    }
    if chatgpt_provider && reply.provider_message {
        // `String(JSON.parse(detail).error.message).slice(0, 300) || msg`; a throw keeps msg.
        if let Some(e) = parsed_error(reply.detail) {
            if !e.is_nullish() {
                if let Some(m) = shown(e.get("message")) {
                    if !m.is_empty() {
                        msg = m;
                    }
                }
            }
        }
    }
    msg
}
