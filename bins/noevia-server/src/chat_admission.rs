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
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};

/// Core rust-chat-admission.cjs `LEASE_HEADER`: the lease a covered call carries.
pub const LEASE_HEADER: &str = "x-noevia-admission-lease";
/// chat-admission.cjs `refusalBody`'s code when the gate names none.
pub const MAINTENANCE: &str = "inferenceMaintenance";
/// chat-admission.cjs `log503('maintenance')`.
pub const LOG_LINE: &str = "[chat] 503 route=/api/chat reason=maintenance";
const UNAVAILABLE: &str = "The server is not answering. Please retry.";

/// Takes the gate for one `gate` call: the lease, or the answer to give instead.
pub async fn admit(app: &Arc<App>, gate: Gate) -> Result<Lease, Box<Response<Body>>> {
    match app.config.research.admission().enter_gate(gate).await {
        Ok(lease) => Ok(lease),
        Err(GateRefusal::Refused(refused)) => Err(Box::new(refusal(gate, &refused, &mut |l| {
            eprintln!("{l}");
        }))),
        Err(GateRefusal::NoAnswer) => Err(Box::new(reply::error(
            StatusCode::BAD_GATEWAY,
            UNAVAILABLE,
            true,
        ))),
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
        let code = refused
            .code
            .as_deref()
            .filter(|c| !c.is_empty())
            .unwrap_or(MAINTENANCE);
        let retry_after = refused
            .retry_after
            .as_deref()
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(3);
        format!(
            r#"{{"error":{},"code":{},"retryable":true,"retryAfter":{retry_after}}}"#,
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
    if let Some(wait) = refused
        .retry_after
        .as_deref()
        .and_then(|v| HeaderValue::from_str(v).ok())
    {
        h.insert(header::RETRY_AFTER, wait);
    } else if chat {
        h.insert(header::RETRY_AFTER, HeaderValue::from_static("3"));
    }
    h.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    res
}

/// Node's words for a gate it no longer holds for this call (inference-maintenance.cjs `enter`): the
/// lease was lost, so maintenance may already be running.
pub const LOST: &str = "Model configuration is being applied. Try again shortly.";

/// The answer for a call whose lease was lost before its headers went out: the gate's refusal, as
/// index.cjs answers a gate that is held (for chat with its `[chat] 503` line).
pub fn lost_answer(gate: Gate) -> Response<Body> {
    let refused = Refused {
        status: 503,
        message: LOST.into(),
        code: None,
        retry_after: None,
    };
    refusal(gate, &refused, &mut |l| {
        eprintln!("{l}");
    })
}

/// `work`, unless `lease` is lost first (Node ended it, or stopped confirming it): then `None`, and
/// `work` is dropped, which closes Core's socket and stops the call there (review P2-1).
pub async fn until_lost<T>(lease: &Lease, work: impl std::future::Future<Output = T>) -> Option<T> {
    let lost = lease.lost().clone();
    tokio::select! {
        biased;
        () = lost.cancelled() => None,
        done = work => Some(done),
    }
}

/// What the answer's body does when the lease is lost after the headers went out.
pub enum OnLoss {
    /// Writes this last frame (the chat turn's failure event) and ends.
    Frame(&'static str),
    /// As `Frame`, unless the flag says the turn is already complete: then the body just ends.
    FrameUnless(&'static str, Arc<std::sync::atomic::AtomicBool>),
    /// Ends the body with an error (the connection is aborted, as for a Core that went away).
    Abort,
}

/// `res` with `lease` held until its body has ended or been dropped (the client left), then given
/// back. When the lease is lost first, Core's body is dropped at once by a task of its own (which
/// closes Core's socket and stops the call there) whether or not the client is reading (S8
/// re-review P2-2: a client that stopped reading must not keep the call running), and the client
/// gets `on_loss` when it reads next.
pub fn hold_until_end(res: Response<Body>, lease: Lease, on_loss: OnLoss) -> Response<Body> {
    let (parts, body) = res.into_parts();
    let shared = Arc::new(Mutex::new(Shared {
        body: Some(body),
        lost: false,
        waker: None,
    }));
    let lost = lease.lost().clone();
    let watched = Arc::clone(&shared);
    let watch = tokio::spawn(async move {
        lost.cancelled().await;
        let (dropped, waker) = {
            let mut s = lock(&watched);
            s.lost = true;
            (s.body.take(), s.waker.take())
        };
        // Outside the lock: dropping Core's body closes its socket, even without another poll.
        drop(dropped);
        if let Some(waker) = waker {
            waker.wake();
        }
    });
    Response::from_parts(
        parts,
        Body::new(Held {
            shared,
            lease: Some(lease),
            on_loss: Some(on_loss),
            watch,
        }),
    )
}

struct Shared {
    body: Option<Body>,
    lost: bool,
    waker: Option<std::task::Waker>,
}

fn lock(m: &Mutex<Shared>) -> std::sync::MutexGuard<'_, Shared> {
    match m.lock() {
        Ok(g) => g,
        Err(p) => p.into_inner(),
    }
}

struct Held {
    shared: Arc<Mutex<Shared>>,
    lease: Option<Lease>,
    on_loss: Option<OnLoss>,
    watch: tokio::task::JoinHandle<()>,
}

impl Drop for Held {
    fn drop(&mut self) {
        self.watch.abort();
        // Release Core synchronously on client disconnect, without waiting for the watch to run.
        let body = lock(&self.shared).body.take();
        drop(body);
    }
}

impl http_body::Body for Held {
    type Data = Bytes;
    type Error = axum::Error;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<http_body::Frame<Bytes>, Self::Error>>> {
        let shared = Arc::clone(&self.shared);
        let mut s = lock(&shared);
        if s.lost || self.lease.as_ref().is_some_and(|l| l.lost().is_cancelled()) {
            // Node no longer counts this call: it is stopped there; tell the client once.
            let dropped = s.body.take();
            drop(s);
            drop(dropped);
            self.lease = None;
            return Poll::Ready(match self.on_loss.take() {
                Some(OnLoss::Frame(frame)) => Some(Ok(http_body::Frame::data(Bytes::from_static(
                    frame.as_bytes(),
                )))),
                // #2494: never a failure event after the turn's `done` (Node wrote none either).
                Some(OnLoss::FrameUnless(_, completed))
                    if completed.load(std::sync::atomic::Ordering::Acquire) =>
                {
                    None
                }
                Some(OnLoss::FrameUnless(frame, _)) => Some(Ok(http_body::Frame::data(
                    Bytes::from_static(frame.as_bytes()),
                ))),
                Some(OnLoss::Abort) => Some(Err(axum::Error::new(std::io::Error::other(
                    "the call's admission lease was lost",
                )))),
                None => None,
            });
        }
        s.waker = Some(cx.waker().clone());
        let Some(body) = s.body.as_mut() else {
            return Poll::Ready(None);
        };
        let polled = Pin::new(body).poll_frame(cx);
        if matches!(polled, Poll::Ready(None) | Poll::Ready(Some(Err(_)))) {
            // The answer has ended: the call's gate count goes with it (dropping gives it back).
            s.body = None;
            drop(s);
            self.watch.abort();
            self.lease = None;
        }
        polled
    }

    fn is_end_stream(&self) -> bool {
        let s = lock(&self.shared);
        !s.lost && s.body.as_ref().is_none_or(http_body::Body::is_end_stream)
    }

    fn size_hint(&self) -> http_body::SizeHint {
        lock(&self.shared)
            .body
            .as_ref()
            .map(http_body::Body::size_hint)
            .unwrap_or_default()
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
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
            retry_after: None,
        };
        let res = refusal(Gate::Chat, &refused, &mut |l| lines.push(l.to_owned()));
        assert_eq!(res.status(), 503);
        let h = res.headers().clone();
        assert_eq!(h["retry-after"], "3");
        assert_eq!(h["x-noevia-api"], "1");
        assert_eq!(h["content-type"], "application/json");
        assert_eq!(h["cache-control"], "no-store");
        assert_eq!(
            h.len(),
            4,
            "only what index.cjs had set before the gate: {h:?}"
        );
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
            retry_after: None,
        };
        let res = refusal(Gate::Diary, &refused, &mut |l| lines.push(l.to_owned()));
        assert_eq!(res.status(), 503);
        assert!(res.headers().get("retry-after").is_none());
        assert_eq!(text(res).await, r#"{"error":"A model is being tuned."}"#);
        assert!(lines.is_empty());
    }

    /// A body that never yields and says when it is dropped (Core's socket closing).
    struct Pending(Option<tokio::sync::oneshot::Sender<()>>);

    impl http_body::Body for Pending {
        type Data = Bytes;
        type Error = axum::Error;
        fn poll_frame(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
        ) -> Poll<Option<Result<http_body::Frame<Bytes>, Self::Error>>> {
            Poll::Pending
        }
    }

    impl Drop for Pending {
        fn drop(&mut self) {
            if let Some(tx) = self.0.take() {
                let _ = tx.send(());
            }
        }
    }

    /// A lease from a synthetic Node that grants with a short TTL and then says it ended.
    async fn losing_lease() -> Lease {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move {
            loop {
                let Ok((mut s, _)) = listener.accept().await else {
                    return;
                };
                tokio::spawn(async move {
                    let mut buf = vec![0u8; 4096];
                    let n = s.read(&mut buf).await.unwrap_or(0);
                    let first = String::from_utf8_lossy(&buf[..n])
                        .lines()
                        .next()
                        .unwrap_or("")
                        .to_owned();
                    let (status, body) = if first.contains("/renew") {
                        (404, "{}")
                    } else if first.starts_with("POST") {
                        (
                            201,
                            r#"{"lease":"0123456789abcdef0123456789abcdef","ttlMs":150}"#,
                        )
                    } else {
                        (204, "")
                    };
                    let reply = format!("HTTP/1.1 {status} X\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}", body.len());
                    let _ = s.write_all(reply.as_bytes()).await;
                });
            }
        });
        let admission = Arc::new(engine_admission::Admission::new(&base, Some("t")).unwrap());
        admission.enter_gate(Gate::Chat).await.unwrap()
    }

    #[tokio::test]
    async fn a_lease_lost_as_the_answer_commits_drops_cores_body_unread() {
        // The commit race: the lease is lost just as the answer is wrapped, and nobody ever reads it.
        let lease = losing_lease().await;
        tokio::time::timeout(std::time::Duration::from_secs(2), lease.lost().cancelled())
            .await
            .unwrap();
        let (tx, rx) = tokio::sync::oneshot::channel();
        let res = hold_until_end(
            Response::new(Body::new(Pending(Some(tx)))),
            lease,
            OnLoss::Frame("data: lost\n\n"),
        );
        tokio::time::timeout(std::time::Duration::from_secs(1), rx)
            .await
            .expect("Core's body was kept though the lease was lost and nobody read it")
            .unwrap();
        // A client that reads after all gets the loss frame, then the end.
        let mut body = res.into_body();
        let first = body.frame().await.unwrap().unwrap();
        assert_eq!(
            first.into_data().unwrap(),
            Bytes::from_static(b"data: lost\n\n")
        );
        assert!(body.frame().await.is_none());
    }

    /// #2494: a lease lost after the turn is complete (its `done` went out, Core still open for the
    /// title) ends the body cleanly instead of writing the failure frame after `done`.
    #[tokio::test]
    async fn a_lease_lost_after_the_turn_is_complete_ends_cleanly() {
        let lease = losing_lease().await;
        let completed = Arc::new(std::sync::atomic::AtomicBool::new(true));
        let (tx, rx) = tokio::sync::oneshot::channel();
        let res = hold_until_end(
            Response::new(Body::new(Pending(Some(tx)))),
            lease,
            OnLoss::FrameUnless("data: lost\n\n", completed),
        );
        tokio::time::timeout(std::time::Duration::from_secs(2), rx)
            .await
            .expect("Core's body was kept though the lease was lost")
            .unwrap();
        let mut body = res.into_body();
        assert!(body.frame().await.is_none(), "a frame followed done");
    }
}
