//! `POST /api/chat` (chat port S5, capability `rust-chat-turn`): the front owns the route as a
//! transparent relay. Core keeps the whole turn (the inference gate, the `llm:` budget, the body cap,
//! routes/chat.cjs and the loop) behind its loopback `POST /internal/chat/turn` (core
//! rust-chat-turn.cjs), and refuses a POST /api/chat that did not come through here.
//!
//! In order:
//! 1. Node's request gate, as identity.rs answers it: 401 signed out, 403 `invalid CSRF token`, 403
//!    `browser_session_required`. Node's inference gate answered before authentication, so a refused
//!    request is first offered to Core's `POST /internal/chat/gate`: when the gate is held its 503
//!    (`Retry-After: 3` and the retryable body) is the answer, as Node gave it; otherwise the refusal.
//! 2. The request, unread, to `/internal/chat/turn` with the per-start secret and the signed-in user
//!    (legacy_proxy.rs `relay`): the client's headers and credentials ride along, Core authenticates
//!    them again and must find the same user. At most `STORED_HISTORY_BYTES + 1` body bytes go on:
//!    Core's own cap then answers its 413 after its 429, in Node's order.
//! 3. Core's answer byte for byte and frame by frame: JSON, the 503 with `Retry-After`, or the event
//!    stream with its `: keep-alive` comments. The client leaving drops the response, which closes
//!    Core's socket (the turn stops and keeps its interrupted reply, #679).
//!
//! When Core fails before its headers the answer is the proxy's 502 JSON. When an event stream breaks
//! after them, the client gets the frame index.cjs writes for a handler that throws mid-stream, and
//! the end: `data: {"type":"error","text":"The request could not be completed. Please retry."}`.
use crate::{identity, reply, serve::Conn, App};
use axum::{
    body::Body,
    http::{header, HeaderValue, Request, Response, StatusCode},
    response::IntoResponse,
};
use bytes::Bytes;
use http_body::{Body as HttpBody, Frame};
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

pub const PATH: &str = "/api/chat";
/// Core rust-chat-turn.cjs `TURN_PATH` and `GATE_PATH`.
pub const TURN_PATH: &str = "/internal/chat/turn";
pub const GATE_PATH: &str = "/internal/chat/gate";
/// Core rust-chat-turn.cjs `USER_HEADER`: the user the front authenticated.
pub const USER_HEADER: &str = "x-noevia-chat-user";
/// core index.cjs `STORED_HISTORY_BYTES`: routes/chat.cjs refuses a longer body with its 413.
pub const STORED_HISTORY_BYTES: u64 = 32 * 1024 * 1024;
/// Core reads one byte past its cap before it answers that 413.
pub const RELAY_LIMIT: u64 = STORED_HISTORY_BYTES + 1;
/// index.cjs handleRequestScoped's answer once the headers are out.
pub const FAILED_FRAME: &str =
    "data: {\"type\":\"error\",\"text\":\"The request could not be completed. Please retry.\"}\n\n";
/// The proxy's 502 (legacy_proxy.rs) for a Core that does not answer.
const UNAVAILABLE: &str = "The server is not answering. Please retry.";

fn unavailable() -> Response<Body> {
    reply::error(StatusCode::BAD_GATEWAY, UNAVAILABLE, true)
}

fn sensitive(value: &str) -> Option<HeaderValue> {
    let mut v = HeaderValue::from_str(value).ok()?;
    v.set_sensitive(true);
    Some(v)
}

/// One POST to exactly [`PATH`] (WHATWG pathname `path`).
pub async fn serve(app: &Arc<App>, conn: Conn, req: Request<Body>, path: String) -> Response<Body> {
    // An assembled native backend has no Node to relay to: its own executor answers, as before.
    if app.proxy.is_in_process() {
        return app.proxy.forward(conn.peer, req).await;
    }
    let (parts, body) = req.into_parts();
    let creds = identity::creds_of(&parts);
    let layer = Arc::clone(&app.identity);
    let now = identity::now_ms();
    let gated = tokio::task::spawn_blocking(move || layer.gate_blocking(&creds, &path, now)).await;
    let token = app.config.research.front_token().and_then(sensitive);
    let authn = match gated {
        Ok(Ok(id)) => id,
        Ok(Err(rejection @ identity::Rejection::Refused(_))) => {
            return match gate_refusal(app, conn, &parts, token).await {
                Some(held) => held,
                None => rejection.into_response(),
            };
        }
        Ok(Err(rejection)) => return rejection.into_response(),
        Err(_) => return identity::Rejection::Unavailable.into_response(),
    };
    let (Some(token), Some(user)) = (token, sensitive(&authn.user.id)) else {
        return unavailable();
    };
    let req = Request::from_parts(parts, body);
    let extra = [
        (crate::project_store::TOKEN_HEADER, token),
        (USER_HEADER, user),
    ];
    match app
        .proxy
        .relay(conn.peer, req, TURN_PATH, &extra, RELAY_LIMIT)
        .await
    {
        Some(res) if is_event_stream(&res) => res.map(|b| Body::new(StreamTail::new(b))),
        Some(res) => res,
        None => unavailable(),
    }
}

/// Core's inference gate for a request the front refuses: its 503 when the gate is held, else `None`
/// (a 204, or no answer: the front's refusal stands).
async fn gate_refusal(
    app: &Arc<App>,
    conn: Conn,
    parts: &axum::http::request::Parts,
    token: Option<HeaderValue>,
) -> Option<Response<Body>> {
    let token = token?;
    let mut probe = Request::post(GATE_PATH).body(Body::empty()).ok()?;
    if let Some(host) = parts.headers.get(header::HOST) {
        probe.headers_mut().insert(header::HOST, host.clone());
    }
    let extra = [(crate::project_store::TOKEN_HEADER, token)];
    let res = app
        .proxy
        .relay(conn.peer, probe, GATE_PATH, &extra, 0)
        .await?;
    (res.status() == StatusCode::SERVICE_UNAVAILABLE).then_some(res)
}

fn is_event_stream(res: &Response<Body>) -> bool {
    res.headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| {
            v.split(';')
                .next()
                .is_some_and(|t| t.trim().eq_ignore_ascii_case("text/event-stream"))
        })
}

/// Core's event stream as it comes; when it breaks before its end, [`FAILED_FRAME`] (on a frame
/// boundary of its own) and then the end, instead of a reset the client would read as "the model
/// connection ended".
struct StreamTail {
    inner: Body,
    /// The last two bytes sent, to know whether the stream stands between frames.
    last: [u8; 2],
    done: bool,
}

impl StreamTail {
    fn new(inner: Body) -> Self {
        StreamTail {
            inner,
            last: *b"\n\n",
            done: false,
        }
    }
}

/// What to send when the stream breaks after `last`: the failure frame, after a blank line when the
/// stream stopped inside a frame.
fn failed_tail(last: [u8; 2]) -> Bytes {
    if last == *b"\n\n" {
        Bytes::from_static(FAILED_FRAME.as_bytes())
    } else {
        Bytes::from(format!("\n\n{FAILED_FRAME}"))
    }
}

impl HttpBody for StreamTail {
    type Data = Bytes;
    type Error = axum::Error;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, axum::Error>>> {
        if self.done {
            return Poll::Ready(None);
        }
        match Pin::new(&mut self.inner).poll_frame(cx) {
            Poll::Ready(Some(Ok(frame))) => {
                if let Some(data) = frame.data_ref() {
                    let [_, prev] = self.last;
                    match data.as_ref() {
                        [] => {}
                        [only] => self.last = [prev, *only],
                        [.., a, b] => self.last = [*a, *b],
                    }
                }
                Poll::Ready(Some(Ok(frame)))
            }
            Poll::Ready(Some(Err(_))) => {
                self.done = true;
                Poll::Ready(Some(Ok(Frame::data(failed_tail(self.last)))))
            }
            Poll::Ready(None) => {
                self.done = true;
                Poll::Ready(None)
            }
            Poll::Pending => Poll::Pending,
        }
    }

    fn is_end_stream(&self) -> bool {
        self.done
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use http_body_util::BodyExt;

    #[test]
    fn the_failure_frame_is_index_cjs_s_and_starts_on_a_frame_boundary() {
        // JSON.stringify's key order: type, then text.
        let inner = FAILED_FRAME
            .strip_prefix("data: ")
            .and_then(|f| f.strip_suffix("\n\n"))
            .unwrap();
        assert!(inner.starts_with("{\"type\":\"error\",\"text\":"));
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(inner).unwrap(),
            serde_json::json!({"type":"error","text":"The request could not be completed. Please retry."})
        );
        assert_eq!(
            failed_tail(*b"\n\n"),
            Bytes::from_static(FAILED_FRAME.as_bytes())
        );
        assert_eq!(
            failed_tail(*b"}\n"),
            Bytes::from(format!("\n\n{FAILED_FRAME}"))
        );
        assert_eq!(RELAY_LIMIT, 32 * 1024 * 1024 + 1);
    }

    #[tokio::test]
    async fn a_stream_that_breaks_ends_with_the_failure_frame_and_a_whole_one_passes_unchanged() {
        type Chan = http_body_util::channel::Channel<Bytes, std::io::Error>;
        let (mut tx, body) = Chan::new(4);
        tx.send_data(Bytes::from_static(b"data: {\"type\":\"meta\"}\n"))
            .await
            .unwrap();
        tx.send_data(Bytes::from_static(b"\n: keep-alive\n\ndata: {\"ty"))
            .await
            .unwrap();
        tx.abort(std::io::Error::other("Node went away"));
        let out = Body::new(StreamTail::new(Body::new(body)))
            .collect()
            .await
            .unwrap()
            .to_bytes();
        assert_eq!(
            out,
            Bytes::from(format!(
                "data: {{\"type\":\"meta\"}}\n\n: keep-alive\n\ndata: {{\"ty\n\n{FAILED_FRAME}"
            ))
        );
        let (mut tx, body) = Chan::new(4);
        tx.send_data(Bytes::from_static(b"data: {\"type\":\"done\"}\n\n"))
            .await
            .unwrap();
        drop(tx);
        let out = Body::new(StreamTail::new(Body::new(body)))
            .collect()
            .await
            .unwrap()
            .to_bytes();
        assert_eq!(out, Bytes::from_static(b"data: {\"type\":\"done\"}\n\n"));
    }
}
