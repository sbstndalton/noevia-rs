//! Chat admission (chat port S8, capability `rust-chat-admission`): the front takes Node's inference gate
//! for a chat turn, a Cowork start and the Diary local exchange itself, through crates/engine-admission
//! ([`engine_admission::Admission::enter_gate`]; core rust-chat-admission.cjs and
//! inference-admission-lease.cjs `{gate}`).
//!
//! Node waits at most 3 s for a held gate (core chat-admission.cjs `CHAT_GATE_WAIT_MS`), and the wait ends
//! when the client leaves: the client leaving drops this request's future, which ends the ask, which
//! ends Node's wait. A refusal is answered here as index.cjs answered it before the gate let a call in:
//! - `POST /api/chat`: 503, `Retry-After: 3`, `{error, code, retryable: true, retryAfter: 3}` (code
//!   `inferenceMaintenance` unless the gate names its own; chat-admission.cjs `refusalBody`), and one
//!   content-free line, `[chat] 503 route=/api/chat reason=maintenance` (chat-admission.cjs `log503`).
//! - the Diary local exchange: 503 `{error}`.
//!
//! Both with only the headers index.cjs had set by then (`X-Noevia-API: 1`, and http.cjs json()'s).
//! A Node that does not answer is the proxy's 502.
//!
//! The lease is the call's one gate count. The call the front sends carries its id ([`LEASE_HEADER`])
//! and Core skips its own gate entry for it (index.cjs `inferenceAdmission.covers`): two counts would make
//! chat.cjs `engineBusy()` (`inferenceBusy(1)`) read busy and titles silently stop. So the front keeps the
//! lease, renewed, until the call's answer has ended or the client has gone ([`hold_until_end`]), and gives
//! it back then; a front that dies stops renewing it and Node ends it by itself.
//!
//! This replaces the S6 gate hold (`/internal/chat/gate?hold=1`) and the gate probe for a request the
//! front refuses (`/internal/chat/gate`): the order stays Node's, the gate first, then the `llm:`
//! budget, then the checks (chat_turn.rs, chat_edge.rs).
use crate::{reply, App};
use axum::{
    body::{Body, Bytes},
    http::{header, HeaderValue, Response, StatusCode},
};
pub use engine_admission::Gate;
use engine_admission::{GateRefusal, Lease, Refused};
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

/// Core rust-chat-admission.cjs `LEASE_HEADER`: the lease a covered call carries.
pub const LEASE_HEADER: &str = "x-noevia-admission-lease";
/// chat-admission.cjs `refusalBody`'s code when the gate names none.
pub const MAINTENANCE: &str = "inferenceMaintenance";
/// chat-admission.cjs `log503('maintenance')`.
pub const LOG_LINE: &str = "[chat] 503 route=/api/chat reason=maintenance";
const UNAVAILABLE: &str = "The server is not answering. Please retry.";

/// Takes the gate for one `gate` call: the lease, or the answer to give instead.
pub async fn admit(app: &Arc<App>, gate: Gate) -> Result<Lease, Response<Body>> {
    match app.config.research.admission().enter_gate(gate).await {
        Ok(lease) => Ok(lease),
        Err(GateRefusal::Refused(refused)) => Err(refusal(gate, &refused, &mut |l| {
            eprintln!("{l}");
        })),
        Err(GateRefusal::NoAnswer) => Err(reply::error(StatusCode::BAD_GATEWAY, UNAVAILABLE, true)),
    }
}

/// The lease's id as the header value a covered call carries.
pub fn header_value(lease: &Lease) -> Option<HeaderValue> {
    let mut v = HeaderValue::from_str(lease.id()).ok()?;
    v.set_sensitive(true);
    Some(v)
}

fn quote(s: &str) -> String {
    serde_json::Value::from(s).to_string()
}

/// index.cjs's answer to the gate's refusal for `gate`; `log` gets the one 503 line of a chat refusal.
pub fn refusal(gate: Gate, refused: &Refused, log: &mut dyn FnMut(&str)) -> Response<Body> {
    let status = StatusCode::from_u16(refused.status)
        .ok()
        .filter(|s| s.is_client_error() || s.is_server_error())
        .unwrap_or(StatusCode::SERVICE_UNAVAILABLE);
    let chat = gate == Gate::Chat && status == StatusCode::SERVICE_UNAVAILABLE;
    let body = if chat {
        log(LOG_LINE);
        let code = refused.code.as_deref().filter(|c| !c.is_empty()).unwrap_or(MAINTENANCE);
        format!(
            r#"{{"error":{},"code":{},"retryable":true,"retryAfter":3}}"#,
            quote(&refused.message),
            quote(code)
        )
    } else {
        format!(r#"{{"error":{}}}"#, quote(&refused.message))
    };
    let mut res = Response::new(Body::from(body));
    *res.status_mut() = status;
    let h = res.headers_mut();
    h.insert("x-noevia-api", HeaderValue::from_static("1"));
    if chat {
        h.insert(header::RETRY_AFTER, HeaderValue::from_static("3"));
    }
    h.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    res
}

/// `res` with `lease` held until its body has ended or been dropped (the client left), then given back.
pub fn hold_until_end(res: Response<Body>, lease: Lease) -> Response<Body> {
    let (parts, body) = res.into_parts();
    Response::from_parts(
        parts,
        Body::new(Held {
            body,
            lease: Some(lease),
        }),
    )
}

struct Held {
    body: Body,
    lease: Option<Lease>,
}

impl http_body::Body for Held {
    type Data = Bytes;
    type Error = axum::Error;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<http_body::Frame<Bytes>, Self::Error>>> {
        let polled = Pin::new(&mut self.body).poll_frame(cx);
        if matches!(polled, Poll::Ready(None) | Poll::Ready(Some(Err(_)))) {
            // The answer has ended: the call's gate count goes with it (dropping gives it back).
            self.lease = None;
        }
        polled
    }

    fn is_end_stream(&self) -> bool {
        self.body.is_end_stream()
    }

    fn size_hint(&self) -> http_body::SizeHint {
        self.body.size_hint()
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use http_body_util::BodyExt;

    async fn text(res: Response<Body>) -> String {
        String::from_utf8(res.into_body().collect().await.unwrap().to_bytes().to_vec()).unwrap()
    }

    #[tokio::test]
    async fn a_chat_refusal_is_index_cjs_s_503_with_its_one_line() {
        let mut lines = Vec::new();
        let refused = Refused {
            status: 503,
            message: "Chat is paused while noevia calibrates m.".into(),
            code: Some("inferenceMaintenance".into()),
        };
        let res = refusal(Gate::Chat, &refused, &mut |l| lines.push(l.to_owned()));
        assert_eq!(res.status(), 503);
        let h = res.headers().clone();
        assert_eq!(h["retry-after"], "3");
        assert_eq!(h["x-noevia-api"], "1");
        assert_eq!(h["content-type"], "application/json");
        assert_eq!(h["cache-control"], "no-store");
        assert_eq!(h.len(), 4, "only what index.cjs had set before the gate: {h:?}");
        assert_eq!(
            text(res).await,
            r#"{"error":"Chat is paused while noevia calibrates m.","code":"inferenceMaintenance","retryable":true,"retryAfter":3}"#
        );
        assert_eq!(lines, ["[chat] 503 route=/api/chat reason=maintenance"]);
        // The gate's own code is kept; none is chat-admission.cjs's default.
        let own = Refused {
            code: Some("engine_held".into()),
            ..refused.clone()
        };
        let body = text(refusal(Gate::Chat, &own, &mut |_| {})).await;
        assert!(body.contains(r#""code":"engine_held""#), "{body}");
        let none = Refused {
            code: None,
            message: "Model \"x\" — paused\n".into(),
            ..refused.clone()
        };
        assert_eq!(
            text(refusal(Gate::Chat, &none, &mut |_| {})).await,
            r#"{"error":"Model \"x\" — paused\n","code":"inferenceMaintenance","retryable":true,"retryAfter":3}"#
        );
    }

    #[tokio::test]
    async fn a_diary_refusal_is_the_gates_503_and_error_with_no_line() {
        let mut lines = Vec::new();
        let refused = Refused {
            status: 503,
            message: "A model is being tuned.".into(),
            code: Some("inferenceMaintenance".into()),
        };
        let res = refusal(Gate::Diary, &refused, &mut |l| lines.push(l.to_owned()));
        assert_eq!(res.status(), 503);
        assert!(res.headers().get("retry-after").is_none());
        assert_eq!(text(res).await, r#"{"error":"A model is being tuned."}"#);
        assert!(lines.is_empty());
    }
}
