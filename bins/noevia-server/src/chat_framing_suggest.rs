//! `/api/chat-framing/suggest` (chat port S4), ported from core routes/chat-framing.cjs's suggest
//! branch at the core in tests/fixtures/framing-suggest-oracle.core-ref. The front owns the public
//! route; core keeps only what it alone has, the user's lists as its workspace load gives them and the
//! decision service, behind `POST /internal/chat-framing/suggest {user, message, chatId}`
//! (core rust-chat-framing-suggest.cjs, on loopback with the per-start secret).
//!
//! In Node's order, every method of exactly this path:
//! 1. Node's request gate: 401 signed out, 403 `invalid CSRF token` for a write without it, 403
//!    `browser_session_required` (identity.rs).
//! 2. 405 `{"error":"method not allowed"}` for anything but POST.
//! 3. The `chat-frame:<user>` budget, 10 a minute (core `FRAMING_RATE_LIMIT` on `llmRateLimiter`,
//!    `LLM_RATE_WINDOW_MS`), counted before the body is read: 429 `{"frame":null,"reason":"rate-limited"}`.
//! 4. The body as http.cjs `readJson` reads it (1 MiB, UTF-8 with replacement, empty is `{}`):
//!    anything that fails is 400 `Invalid request`; `typeof body?.message !== 'string'` is 400
//!    `message required`.
//! 5. `message.slice(0, 4000)` in UTF-16 units (a halved pair keeps its high surrogate), and `chatId`
//!    when it is a string, else null.
//! 6. The bridge's 200 answer, byte for byte (core's `json(res, 200, result)`). Any other answer, or
//!    none, is the route's fail-open 200 `{"frame":null,"reason":"error"}`.
//!
//! The body is read with js_json `raw` (V8's `JSON.parse` acceptance with no tree): any nesting, any
//! number (`1e400` included) and every character, U+10F800..=U+10FFFF and lone surrogates included,
//! are accepted as Node accepts them; the strings sent on are written as `JSON.stringify` writes
//! them. No number is written, so V8's number digits do not arise here.
use crate::{identity, reply, App};
use axum::{
    body::Body,
    http::{Method, Request, Response, StatusCode},
    response::IntoResponse,
};
use js_json::raw;
use std::{sync::Arc, time::Duration};

pub const PATH: &str = "/api/chat-framing/suggest";
/// Core rust-chat-framing-suggest.cjs `BRIDGE_PATH`.
pub const BRIDGE_PATH: &str = "/internal/chat-framing/suggest";
/// http.cjs `readBody`'s default limit.
const BODY_LIMIT: usize = 1024 * 1024;
/// `FRAMING_RATE_LIMIT` and `LLM_RATE_WINDOW_MS`.
const RATE_LIMIT: u64 = 10;
const RATE_WINDOW_MS: i64 = 60_000;
/// `body.message.slice(0, 4000)`.
pub const MESSAGE_CAP: usize = 4000;
/// Core's suggestion stops at its decision deadline (1.5 s by default); this only bounds a bridge
/// that never answers.
const BRIDGE_TIMEOUT: Duration = Duration::from_secs(30);
const RATE_LIMITED: &str = r#"{"frame":null,"reason":"rate-limited"}"#;
const FAILED_OPEN: &str = r#"{"frame":null,"reason":"error"}"#;

fn answer(status: StatusCode, text: &str) -> Response<Body> {
    reply::json_text(status, text.to_owned(), true)
}

/// What the bridge is sent for a request body: `{"user":…,"message":…,"chatId":…}` as
/// `JSON.stringify` writes it, or the route's 400 text (`Invalid request`, `message required`).
pub fn bridge_body(user: &str, bytes: &[u8]) -> Result<String, &'static str> {
    // http.cjs readJson: `raw ? JSON.parse(raw) : {}` over Buffer#toString('utf8').
    if bytes.is_empty() {
        return Err("message required");
    }
    let fields = match raw::read(&String::from_utf8_lossy(bytes)) {
        None => return Err("Invalid request"),
        Some(raw::Top::Other(..)) => return Err("message required"),
        Some(raw::Top::Object(fields)) => fields,
    };
    let field = |name: &str| fields.iter().find(|f| raw::is_key(&f.key, name));
    let message = field("message")
        .and_then(|f| raw::slice_string(&f.value, MESSAGE_CAP))
        .ok_or("message required")?;
    let chat_id = field("chatId")
        .map(|f| f.value.as_str())
        .filter(|v| v.starts_with('"'))
        .unwrap_or("null");
    Ok(format!(
        "{{\"user\":{},\"message\":{message},\"chatId\":{chat_id}}}",
        raw::quote(user)
    ))
}

/// The bridge's answer as the route sends it: its 200 body when that is a JSON object.
fn relayed(reply: Option<(u16, Vec<u8>)>) -> Option<String> {
    let (status, bytes) = reply?;
    if status != 200 {
        return None;
    }
    let text = String::from_utf8(bytes).ok()?;
    matches!(raw::read(&text), Some(raw::Top::Object(_))).then_some(text)
}

/// One request for exactly [`PATH`]; `path` is the WHATWG pathname.
pub async fn serve(app: &Arc<App>, req: Request<Body>, path: String) -> Response<Body> {
    let (parts, body) = req.into_parts();
    let creds = identity::creds_of(&parts);
    let layer = Arc::clone(&app.identity);
    let now = identity::now_ms();
    let gate_path = path.clone();
    let authn =
        match tokio::task::spawn_blocking(move || layer.gate_blocking(&creds, &gate_path, now))
            .await
        {
            Ok(Ok(id)) => id,
            Ok(Err(rejection)) => return rejection.into_response(),
            Err(_) => return identity::Rejection::Unavailable.into_response(),
        };
    if parts.method != Method::POST {
        return reply::error(StatusCode::METHOD_NOT_ALLOWED, "method not allowed", true);
    }
    let user = authn.user.id.clone();
    let limited = app
        .framing_suggest_rate
        .lock()
        .map(|mut r| {
            r.limited(
                &format!("chat-frame:{user}"),
                RATE_LIMIT,
                RATE_WINDOW_MS,
                identity::now_ms(),
            )
        })
        // A poisoned limiter cannot count; core's limiter cannot fail, so let the request through.
        .unwrap_or(false);
    if limited {
        return answer(StatusCode::TOO_MANY_REQUESTS, RATE_LIMITED);
    }
    let bytes = match tokio::time::timeout(
        Duration::from_secs(30),
        crate::account_routes::read_body(body, BODY_LIMIT),
    )
    .await
    {
        Ok(Ok(b)) if !b.over => b.bytes,
        _ => return reply::error(StatusCode::BAD_REQUEST, "Invalid request", true),
    };
    let outgoing = match bridge_body(&user, &bytes) {
        Ok(text) => text,
        Err(message) => return reply::error(StatusCode::BAD_REQUEST, message, true),
    };
    let reply = app
        .config
        .research
        .node_call_text(BRIDGE_PATH, outgoing, BRIDGE_TIMEOUT)
        .await;
    match relayed(reply) {
        Some(text) => reply::json_text(StatusCode::OK, text, true),
        None => answer(StatusCode::OK, FAILED_OPEN),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn sent(body: &str) -> Result<String, &'static str> {
        bridge_body("u1", body.as_bytes())
    }

    #[test]
    fn the_body_checks_and_the_cut_are_nodes() {
        for refused in ["{", "\u{feff}{}", "{\"message\":\"a\"} x", "'x'"] {
            assert_eq!(sent(refused), Err("Invalid request"), "{refused}");
        }
        for (body, why) in [
            ("", "message required"),
            ("null", "message required"),
            ("[]", "message required"),
            ("\"hi\"", "message required"),
            ("1e400", "message required"),
            ("{\"message\":5}", "message required"),
            ("{\"message\":null}", "message required"),
            ("{\"message\":1e400}", "message required"),
        ] {
            assert_eq!(sent(body), Err(why), "{body}");
        }
        assert_eq!(
            sent(r#"{"message":"a","message":"b","chatId":7}"#).unwrap(),
            r#"{"user":"u1","message":"b","chatId":null}"#
        );
        assert_eq!(
            sent(r#"{"chatId":"","message":""}"#).unwrap(),
            r#"{"user":"u1","message":"","chatId":""}"#
        );
        // 3,999 units then an emoji: the cut keeps its high surrogate, as `slice` does.
        let long = format!("{{\"message\":\"{}\u{1F600}z\"}}", "m".repeat(3999));
        let out = sent(&long).unwrap();
        assert!(
            out.ends_with(&format!("{}\\ud83d\",\"chatId\":null}}", "m".repeat(3999))),
            "{out}"
        );
        // A lone surrogate the client sent is kept.
        assert_eq!(
            sent(r#"{"message":"\udc00x"}"#).unwrap(),
            r#"{"user":"u1","message":"\udc00x","chatId":null}"#
        );
        // What a tree could not hold is accepted, as JSON.parse accepts it.
        let deep = format!(
            "{{\"x\":{}1{},\"message\":\"m\"}}",
            "[".repeat(500),
            "]".repeat(500)
        );
        assert_eq!(
            sent(&deep).unwrap(),
            r#"{"user":"u1","message":"m","chatId":null}"#
        );
        assert_eq!(
            sent("{\"n\":1e400,\"message\":\"\u{10F800}\u{10FFFF}\",\"chatId\":\"\u{10FFFF}\"}")
                .unwrap(),
            "{\"user\":\"u1\",\"message\":\"\u{10F800}\u{10FFFF}\",\"chatId\":\"\u{10FFFF}\"}"
        );
        // Invalid UTF-8 reads as U+FFFD, as Buffer#toString does.
        assert_eq!(
            bridge_body("u1", b"{\"message\":\"a\xffb\"}").unwrap(),
            "{\"user\":\"u1\",\"message\":\"a\u{fffd}b\",\"chatId\":null}"
        );
    }

    #[test]
    fn only_a_json_object_from_the_bridge_is_relayed() {
        let ok = br#"{"frame":null,"reason":"disabled"}"#.to_vec();
        assert_eq!(
            relayed(Some((200, ok.clone()))).as_deref(),
            Some(r#"{"frame":null,"reason":"disabled"}"#)
        );
        assert_eq!(relayed(Some((502, ok))), None);
        assert_eq!(relayed(Some((200, b"[]".to_vec()))), None);
        assert_eq!(relayed(Some((200, b"{".to_vec()))), None);
        assert_eq!(relayed(Some((200, vec![0xff]))), None);
        assert_eq!(relayed(None), None);
        // A reply echoing what a tree cannot hold is still an object.
        let echo = "{\"frame\":{\"echo\":\"\u{10FFFF}\"},\"reason\":null}"
            .as_bytes()
            .to_vec();
        assert!(relayed(Some((200, echo))).is_some());
    }
}
