//! Admission to the model engine from the Rust front (sbstndalton/noevia#2218).
//!
//! While chat lives in Node, Node is where the rules about the shared engine are held: the maintenance gate
//! (a tune, a calibration, a reload holds the engine, `modelManager.enterInference`), the inference memory
//! budget and the memory-watchdog quarantine (`prepareLoad`, #697), and, when one is configured, the
//! inference gate's one-model queue (`acquireLease`, #2115). These counts and caches are Node's memory, so
//! a second copy here could only disagree. So a caller asks Node for an inference lease, over the loopback
//! connection, and Node takes the same steps it takes for its own work and holds them until the lease ends
//! (core `inference-admission-lease.cjs`). The sibling of the models round's maintenance lease
//! (`/internal/model-maintenance/lease`): that one holds the engine for maintenance and refuses while
//! inference runs; this one holds a place in inference and refuses while maintenance runs. Same transport,
//! same token, same renewal rule, and the same exclusion is what makes them correct together.
//!
//! - A lease is renewed every third of its TTL by a task of its own. When Node says it ended, or stops
//!   confirming it for a whole TTL, [`Lease::lost`] is cancelled, so the holder's model request stops: Node
//!   may start maintenance from then on. It is never taken again silently.
//! - [`Lease::release`] gives it back. A lease dropped without that (a panic, a dropped task) gives it back
//!   from `Drop`; if even that cannot run, Node ends it by itself after the TTL.
//! - No answer from Node is a refusal: admission fails closed, nothing reaches the engine.
//!
//! - [`Admission::enter_gate`] (chat port S8): the inference gate alone, as a chat turn or the Diary local exchange
//!   meets it (core rust-chat-admission.cjs). Node waits at most [`GATE_WAIT`] for a held gate and stops when
//!   this caller goes away; the call the front then sends with [`Lease::id`] is covered by the lease and enters
//!   nothing again, so the lease is that call's one gate count.
//!
//! [`machines`]: which machine serves a chat role, read from the settings row Node owns.
pub mod machines;

use model_stream::Cancel;
use reqwest::header::{HeaderName, HeaderValue};
use serde_json::{json, Value};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

const PATH: &str = "/internal/inference-admission/lease";
const TOKEN_HEADER: &str = "x-noevia-store-token";
const CALL_TIMEOUT: Duration = Duration::from_secs(5);
/// Node's own queue limit is under this (`max_wait_ms`), so a queued wait ends with Node's answer.
pub const ENTER_TIMEOUT: Duration = Duration::from_secs(150);

/// Core chat-admission.cjs `CHAT_GATE_WAIT_MS`: Node waits at most this for a held gate.
pub const GATE_WAIT: Duration = Duration::from_secs(3);
/// How long a gate ask may take in all: Node's wait and a margin for the loopback call.
pub const GATE_TIMEOUT: Duration = Duration::from_secs(5);

/// Which call the inference gate is taken for (core rust-chat-admission.cjs `GATE_ROUTES`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Gate {
    /// `POST /api/chat`: a chat turn or a Cowork start.
    Chat,
    /// `POST /api/diary/local-exchange`.
    Diary,
}

impl Gate {
    pub fn name(self) -> &'static str {
        match self {
            Gate::Chat => "chat",
            Gate::Diary => "diary",
        }
    }
}

/// Why [`Admission::enter_gate`] gave no lease.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GateRefusal {
    /// The gate's refusal, in Node's words: its status (503 while maintenance holds it), message and code.
    Refused(Refused),
    /// No answer from Node, or not one of this route's: the caller answers as for a Node that is down.
    NoAnswer,
}

/// Cancels its [`Cancel`] when dropped: the caller of [`Admission::enter_gate`] went away.
struct CancelOnDrop(Cancel);

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.0.cancel();
    }
}

/// What to hold.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ask<'a> {
    /// This machine's engine, for this model.
    Engine { model: &'a str },
    /// A chat role on another machine: nothing is admitted here, the machine counts a turn (#1880), and
    /// its health and served model are checked first (#1851).
    Machine {
        role: &'a str,
        machine: &'a str,
        model: &'a str,
    },
}

/// Why no lease was given. `message` is the words to show; `code` is core's.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refused {
    pub status: u16,
    pub message: String,
    pub code: Option<String>,
}

impl Refused {
    fn unavailable() -> Refused {
        Refused {
            status: 503,
            message: "The model is busy. Try again shortly.".into(),
            code: None,
        }
    }
}

/// The client. Cheap to share.
pub struct Admission {
    client: reqwest::Client,
    base: String,
    token: Option<HeaderValue>,
}

impl std::fmt::Debug for Admission {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Admission")
            .field("base", &self.base)
            .field("token", &self.token.is_some())
            .finish_non_exhaustive()
    }
}

fn valid_id(s: &str) -> bool {
    s.len() == 32
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

impl Admission {
    /// `base`: Node's loopback address. `token`: the per-start secret both processes were handed.
    pub fn new(base: &str, token: Option<&str>) -> Result<Admission, String> {
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .no_proxy()
            .build()
            .map_err(|e| format!("inference admission client: {e}"))?;
        let token = token
            .filter(|t| !t.is_empty())
            .and_then(|t| HeaderValue::from_str(t).ok())
            .map(|mut t| {
                t.set_sensitive(true);
                t
            });
        Ok(Admission {
            client,
            base: base.trim_end_matches('/').to_owned(),
            token,
        })
    }

    fn request(&self, method: reqwest::Method, path: &str) -> Option<reqwest::RequestBuilder> {
        let token = self.token.clone()?;
        Some(
            self.client
                .request(method, format!("{}{path}", self.base))
                .header(HeaderName::from_static(TOKEN_HEADER), token),
        )
    }

    /// Takes a lease, waiting as long as Node's queue does, or until `cancel`.
    pub async fn enter(self: &Arc<Self>, ask: Ask<'_>, cancel: &Cancel) -> Result<Lease, Refused> {
        let body = match ask {
            Ask::Engine { model } => json!({ "model": model }),
            Ask::Machine {
                role,
                machine,
                model,
            } => json!({ "role": role, "machine": machine, "model": model }),
        };
        let Some(req) = self.request(reqwest::Method::POST, PATH) else {
            return Err(Refused::unavailable());
        };
        let send = req
            .header("content-type", "application/json")
            .timeout(ENTER_TIMEOUT)
            .body(body.to_string())
            .send();
        let answer = tokio::select! {
            () = cancel.cancelled() => return Err(cancelled()),
            r = send => r,
        };
        let response = answer.map_err(|_| Refused::unavailable())?;
        let status = response.status().as_u16();
        let reading = response.bytes();
        let bytes = tokio::select! {
            () = cancel.cancelled() => return Err(cancelled()),
            b = reading => b.map_err(|_| Refused::unavailable())?,
        };
        let parsed: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
        if status != 201 {
            let message = parsed.get("error").and_then(Value::as_str);
            // A 404 is Node without this route, or one that did not trust the call.
            return Err(match (status, message) {
                (404, _) | (_, None) => Refused::unavailable(),
                (_, Some(m)) => Refused {
                    status,
                    message: m.chars().take(500).collect(),
                    code: parsed
                        .get("code")
                        .and_then(Value::as_str)
                        .map(str::to_owned),
                },
            });
        }
        self.grant(&parsed).ok_or_else(Refused::unavailable)
    }

    /// The lease Node granted in `parsed` (its 201 body), renewed from now on; `None` when it is not one.
    fn grant(self: &Arc<Self>, parsed: &Value) -> Option<Lease> {
        let id = parsed
            .get("lease")
            .and_then(Value::as_str)
            .filter(|l| valid_id(l))?
            .to_owned();
        let ttl = parsed
            .get("ttlMs")
            .and_then(Value::as_u64)
            .filter(|t| *t > 0)
            .map(Duration::from_millis)?;
        // The gate's own lease header, when Node holds one for this: only that name, and a lease token.
        let headers = parsed
            .get("headers")
            .and_then(Value::as_object)
            .into_iter()
            .flatten()
            .filter(|(k, v)| {
                k.as_str() == "x-noevia-lease"
                    && v.as_str().is_some_and(|t| {
                        !t.is_empty()
                            && t.len() <= 128
                            && t.bytes().all(|b| b.is_ascii_alphanumeric())
                    })
            })
            .filter_map(|(k, v)| Some((k.clone(), v.as_str()?.to_owned())))
            .collect();
        let lost = Cancel::new();
        let inner = Arc::new(Held {
            admission: Arc::clone(self),
            id,
            released: AtomicBool::new(false),
        });
        let renewing = tokio::spawn(renew(Arc::clone(&inner), ttl, lost.clone()));
        Some(Lease {
            inner,
            headers,
            lost,
            renewing,
        })
    }

    /// Chat port S8: takes the inference gate for one `gate` call (core rust-chat-admission.cjs). Node waits at
    /// most [`GATE_WAIT`]; the whole ask at most [`GATE_TIMEOUT`]. Dropping this future (the client left) ends
    /// the ask, which ends Node's wait; a lease that was already on its way is given back, never kept.
    pub async fn enter_gate(self: &Arc<Self>, gate: Gate) -> Result<Lease, GateRefusal> {
        let gone = Cancel::new();
        let _guard = CancelOnDrop(gone.clone());
        let me = Arc::clone(self);
        // Its own task, so that a grant that arrives as the caller leaves is still read and given back.
        let asking = tokio::spawn(async move { me.gate_once(gate, &gone).await });
        asking.await.unwrap_or(Err(GateRefusal::NoAnswer))
    }

    async fn gate_once(self: &Arc<Self>, gate: Gate, gone: &Cancel) -> Result<Lease, GateRefusal> {
        let req = self
            .request(reqwest::Method::POST, PATH)
            .ok_or(GateRefusal::NoAnswer)?;
        let send = req
            .header("content-type", "application/json")
            .timeout(GATE_TIMEOUT)
            .body(json!({ "gate": gate.name() }).to_string())
            .send();
        // Before Node answers, leaving ends the ask (Node's wait ends with it). Once it has answered, the
        // answer is read to its end: a lease in it is then dropped, which gives it back.
        let response = tokio::select! {
            () = gone.cancelled() => return Err(GateRefusal::NoAnswer),
            r = send => r.map_err(|_| GateRefusal::NoAnswer)?,
        };
        let status = response.status().as_u16();
        let bytes = response.bytes().await.map_err(|_| GateRefusal::NoAnswer)?;
        let parsed: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
        if status == 201 {
            return self.grant(&parsed).ok_or(GateRefusal::NoAnswer);
        }
        // The gate refuses only with its 503; anything else is not this route's answer (a Core that is
        // not this front's pair answers an ask it does not know with 400 or 404).
        match (status, parsed.get("error").and_then(Value::as_str)) {
            // The gate's words whole, as index.cjs sends them (refusalBody keeps err.message).
            (503, Some(m)) => Err(GateRefusal::Refused(Refused {
                status,
                message: m.to_owned(),
                code: parsed
                    .get("code")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
            })),
            _ => Err(GateRefusal::NoAnswer),
        }
    }

    async fn renew_once(&self, id: &str) -> Option<bool> {
        let res = self
            .request(reqwest::Method::POST, &format!("{PATH}/{id}/renew"))?
            .timeout(CALL_TIMEOUT)
            .send()
            .await
            .ok()?;
        match res.status().as_u16() {
            200 => Some(true),
            404 => Some(false),
            _ => None,
        }
    }

    async fn release_once(&self, id: &str) {
        if let Some(req) = self.request(reqwest::Method::DELETE, &format!("{PATH}/{id}")) {
            let _ = req.timeout(CALL_TIMEOUT).send().await;
        }
    }
}

/// The words of a cancel seen while waiting for admission (the caller knows it cancelled).
fn cancelled() -> Refused {
    Refused {
        status: 499,
        message: "Cancelled.".into(),
        code: None,
    }
}

struct Held {
    admission: Arc<Admission>,
    id: String,
    released: AtomicBool,
}

async fn renew(held: Arc<Held>, ttl: Duration, lost: Cancel) {
    let every = (ttl / 3).max(Duration::from_millis(50));
    let mut unconfirmed = Duration::ZERO;
    loop {
        tokio::time::sleep(every).await;
        if held.released.load(Ordering::SeqCst) {
            return;
        }
        match held.admission.renew_once(&held.id).await {
            Some(true) => unconfirmed = Duration::ZERO,
            Some(false) => {
                lost.cancel();
                return;
            }
            None => {
                unconfirmed += every;
                // By now Node has ended a lease it heard nothing about. Stop before it can matter.
                if unconfirmed + every >= ttl {
                    lost.cancel();
                    return;
                }
            }
        }
    }
}

/// A place in inference, held until released.
pub struct Lease {
    inner: Arc<Held>,
    headers: Vec<(String, String)>,
    lost: Cancel,
    renewing: tokio::task::JoinHandle<()>,
}

impl Lease {
    /// Node's id of this lease: what a covered call carries (core rust-chat-admission.cjs `LEASE_HEADER`).
    pub fn id(&self) -> &str {
        &self.inner.id
    }
    /// Headers to send with the model request (the inference gate's lease, when there is a gate).
    pub fn headers(&self) -> &[(String, String)] {
        &self.headers
    }
    /// Cancelled when the lease ended without [`Lease::release`]: stop the model request.
    pub fn lost(&self) -> &Cancel {
        &self.lost
    }
    /// Gives it back, and waits for that to be asked.
    pub async fn release(self) {
        if !self.inner.released.swap(true, Ordering::SeqCst) {
            self.renewing.abort();
            self.inner.admission.release_once(&self.inner.id).await;
        }
    }
}

impl Drop for Lease {
    fn drop(&mut self) {
        self.renewing.abort();
        if self.inner.released.swap(true, Ordering::SeqCst) {
            return;
        }
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            let inner = Arc::clone(&self.inner);
            handle.spawn(async move {
                inner.admission.release_once(&inner.id).await;
            });
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing, clippy::panic)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    #[derive(Default)]
    struct Node {
        seen: Mutex<Vec<String>>,
        renew_status: Mutex<u16>,
    }

    /// A synthetic Node: `POST` answers `grant`; renewals answer `renew_status`; records each request.
    async fn node(grant: (u16, String)) -> (String, Arc<Node>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let state = Arc::new(Node {
            seen: Mutex::default(),
            renew_status: Mutex::new(200),
        });
        let st = Arc::clone(&state);
        tokio::spawn(async move {
            loop {
                let (mut s, _) = listener.accept().await.unwrap();
                let st = Arc::clone(&st);
                let grant = grant.clone();
                tokio::spawn(async move {
                    let mut buf = vec![0u8; 4096];
                    let n = s.read(&mut buf).await.unwrap_or(0);
                    let text = String::from_utf8_lossy(&buf[..n]).into_owned();
                    let first = text.lines().next().unwrap_or("").to_owned();
                    let token = text
                        .to_ascii_lowercase()
                        .contains("x-noevia-store-token: secret");
                    let line = format!("{first} {}", if token { "T" } else { "-" });
                    st.seen.lock().unwrap().push(line);
                    let (status, body) = if first.starts_with("POST") && first.contains("/renew") {
                        (*st.renew_status.lock().unwrap(), "{}".to_owned())
                    } else if first.starts_with("POST") {
                        grant
                    } else {
                        (204, String::new())
                    };
                    let reply = format!("HTTP/1.1 {status} X\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}", body.len());
                    let _ = s.write_all(reply.as_bytes()).await;
                });
            }
        });
        (base, state)
    }

    const GRANT: &str = r#"{"lease":"0123456789abcdef0123456789abcdef","ttlMs":300,"headers":{"x-noevia-lease":"abc123","x-evil":"no"}}"#;

    fn admission(base: &str) -> Arc<Admission> {
        Arc::new(Admission::new(base, Some("secret")).unwrap())
    }

    async fn settle() {
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    #[tokio::test]
    async fn a_lease_is_given_only_the_gate_header_passes_and_release_is_asked() {
        let (base, state) = node((201, GRANT.into())).await;
        let lease = admission(&base)
            .enter(Ask::Engine { model: "m" }, &Cancel::new())
            .await
            .unwrap();
        assert_eq!(
            lease.headers(),
            &[("x-noevia-lease".to_owned(), "abc123".to_owned())]
        );
        lease.release().await;
        let seen = state.seen.lock().unwrap().clone();
        assert!(
            seen[0].starts_with("POST /internal/inference-admission/lease")
                && seen[0].ends_with('T'),
            "{seen:?}"
        );
        assert!(
            seen.iter().any(|l| l.starts_with("DELETE")
                && l.contains("0123456789abcdef")
                && l.ends_with('T')),
            "{seen:?}"
        );
    }

    #[tokio::test]
    async fn a_refusal_keeps_nodes_words_and_no_answer_fails_closed() {
        let (base, _s) = node((409, r#"{"error":"over budget","code":"overBudget"}"#.into())).await;
        let refused = admission(&base)
            .enter(Ask::Engine { model: "m" }, &Cancel::new())
            .await
            .err()
            .unwrap();
        assert_eq!(
            (
                refused.status,
                refused.message.as_str(),
                refused.code.as_deref()
            ),
            (409, "over budget", Some("overBudget"))
        );
        for grant in [
            (404, "{}".to_owned()),
            (201, r#"{"lease":"short","ttlMs":1}"#.to_owned()),
            (500, "x".to_owned()),
        ] {
            let (base, _s) = node(grant).await;
            let refused = admission(&base)
                .enter(Ask::Engine { model: "m" }, &Cancel::new())
                .await
                .err()
                .unwrap();
            assert_eq!(refused, Refused::unavailable());
        }
        let refused = admission("http://127.0.0.1:9")
            .enter(Ask::Engine { model: "m" }, &Cancel::new())
            .await
            .err()
            .unwrap();
        assert_eq!(refused, Refused::unavailable());
        let no_token = Arc::new(Admission::new("http://127.0.0.1:9", None).unwrap());
        assert!(no_token
            .enter(Ask::Engine { model: "m" }, &Cancel::new())
            .await
            .is_err());
    }

    #[tokio::test]
    async fn dropping_a_lease_gives_it_back() {
        let (base, state) = node((201, GRANT.into())).await;
        let lease = admission(&base)
            .enter(Ask::Engine { model: "m" }, &Cancel::new())
            .await
            .unwrap();
        drop(lease);
        settle().await;
        assert!(state
            .seen
            .lock()
            .unwrap()
            .iter()
            .any(|l| l.starts_with("DELETE")));
    }

    #[tokio::test]
    async fn renewal_runs_and_a_lease_node_ended_is_lost_and_not_taken_again() {
        let (base, state) = node((201, GRANT.into())).await;
        let lease = admission(&base)
            .enter(Ask::Engine { model: "m" }, &Cancel::new())
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(250)).await;
        assert!(state
            .seen
            .lock()
            .unwrap()
            .iter()
            .any(|l| l.contains("/renew")));
        assert!(!lease.lost().is_cancelled());
        *state.renew_status.lock().unwrap() = 404;
        tokio::time::timeout(Duration::from_secs(5), lease.lost().cancelled())
            .await
            .unwrap();
        let posts = state
            .seen
            .lock()
            .unwrap()
            .iter()
            .filter(|l| l.starts_with("POST") && !l.contains("/renew"))
            .count();
        assert_eq!(posts, 1);
    }

    #[tokio::test]
    async fn a_node_that_stops_confirming_loses_the_lease_within_its_ttl() {
        let (base, state) = node((201, GRANT.into())).await;
        let lease = admission(&base)
            .enter(Ask::Engine { model: "m" }, &Cancel::new())
            .await
            .unwrap();
        *state.renew_status.lock().unwrap() = 500;
        tokio::time::timeout(Duration::from_millis(900), lease.lost().cancelled())
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn the_machine_ask_names_its_machine() {
        let (base, _s) = node((201, GRANT.into())).await;
        let lease = admission(&base)
            .enter(
                Ask::Machine {
                    role: "smart",
                    machine: "m1",
                    model: "x",
                },
                &Cancel::new(),
            )
            .await
            .unwrap();
        lease.release().await;
    }

    #[tokio::test]
    async fn a_cancel_while_waiting_ends_the_wait() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move {
            let (_s, _) = listener.accept().await.unwrap();
            tokio::time::sleep(Duration::from_secs(30)).await;
        });
        let cancel = Cancel::new();
        let trigger = cancel.clone();
        tokio::spawn(async move {
            settle().await;
            trigger.cancel();
        });
        let refused = tokio::time::timeout(
            Duration::from_secs(5),
            admission(&base).enter(Ask::Engine { model: "m" }, &cancel),
        )
        .await
        .unwrap()
        .err()
        .unwrap();
        assert_eq!(refused.status, 499);
    }

    // Chat port S8: the gate alone, for a chat call.

    #[tokio::test]
    async fn a_gate_lease_asks_for_its_route_and_carries_its_id() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let seen = Arc::new(Mutex::new(Vec::<String>::new()));
        let log = Arc::clone(&seen);
        tokio::spawn(async move {
            loop {
                let (mut s, _) = listener.accept().await.unwrap();
                let log = Arc::clone(&log);
                tokio::spawn(async move {
                    let mut buf = vec![0u8; 4096];
                    let n = s.read(&mut buf).await.unwrap_or(0);
                    let text = String::from_utf8_lossy(&buf[..n]).into_owned();
                    log.lock().unwrap().push(text.clone());
                    let body = if text.starts_with("POST /internal/inference-admission/lease ") {
                        r#"{"lease":"0123456789abcdef0123456789abcdef","ttlMs":30000}"#
                    } else {
                        ""
                    };
                    let status = if body.is_empty() { 204 } else { 201 };
                    let reply = format!("HTTP/1.1 {status} X\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}", body.len());
                    let _ = s.write_all(reply.as_bytes()).await;
                });
            }
        });
        for (gate, name) in [(Gate::Chat, "chat"), (Gate::Diary, "diary")] {
            let lease = admission(&base).enter_gate(gate).await.unwrap();
            assert_eq!(lease.id(), "0123456789abcdef0123456789abcdef");
            assert!(lease.headers().is_empty());
            lease.release().await;
            let seen = seen.lock().unwrap().clone();
            let ask = seen
                .iter()
                .rev()
                .find(|t| t.starts_with("POST /internal/inference-admission/lease "))
                .unwrap();
            assert!(
                ask.to_ascii_lowercase()
                    .contains("x-noevia-store-token: secret"),
                "{ask}"
            );
            assert!(ask.ends_with(&format!(r#"{{"gate":"{name}"}}"#)), "{ask}");
            assert!(seen
                .iter()
                .any(|t| t
                    .starts_with("DELETE /internal/inference-admission/lease/0123456789abcdef")));
        }
    }

    #[tokio::test]
    async fn a_gate_refusal_keeps_nodes_words_and_anything_else_is_no_answer() {
        let (base, _s) = node((503, r#"{"error":"Chat is paused while noevia calibrates m.","code":"inferenceMaintenance"}"#.into())).await;
        let refused = admission(&base).enter_gate(Gate::Chat).await.err().unwrap();
        assert_eq!(
            refused,
            GateRefusal::Refused(Refused {
                status: 503,
                message: "Chat is paused while noevia calibrates m.".into(),
                code: Some("inferenceMaintenance".into()),
            })
        );
        // Node's words whole, however long (index.cjs keeps err.message).
        let long = "x".repeat(700);
        let (base, _s) = node((
            503,
            format!(r#"{{"error":"{long}","code":"inferenceMaintenance"}}"#),
        ))
        .await;
        match admission(&base).enter_gate(Gate::Chat).await.err().unwrap() {
            GateRefusal::Refused(r) => assert_eq!(r.message, long),
            other => panic!("{other:?}"),
        }
        for grant in [
            (404, r#"{"error":"Not found"}"#.to_owned()),
            (400, r#"{"error":"A model is required."}"#.to_owned()),
            (503, "{}".to_owned()),
            (201, r#"{"lease":"short","ttlMs":1}"#.to_owned()),
            (500, "x".to_owned()),
        ] {
            let (base, _s) = node(grant).await;
            assert_eq!(
                admission(&base).enter_gate(Gate::Chat).await.err(),
                Some(GateRefusal::NoAnswer)
            );
        }
        assert_eq!(
            admission("http://127.0.0.1:9")
                .enter_gate(Gate::Chat)
                .await
                .err(),
            Some(GateRefusal::NoAnswer)
        );
        let no_token = Arc::new(Admission::new("http://127.0.0.1:9", None).unwrap());
        assert_eq!(
            no_token.enter_gate(Gate::Diary).await.err(),
            Some(GateRefusal::NoAnswer)
        );
    }

    #[tokio::test]
    async fn a_caller_that_leaves_during_the_wait_ends_the_ask_at_once() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let (closed_tx, closed_rx) = tokio::sync::oneshot::channel::<()>();
        tokio::spawn(async move {
            let (mut s, _) = listener.accept().await.unwrap();
            let mut buf = vec![0u8; 4096];
            let _ = s.read(&mut buf).await;
            // Node waits for its gate: it answers nothing until the asker goes away.
            let n = s.read(&mut buf).await.unwrap_or(0);
            assert_eq!(n, 0, "the asker closed its socket");
            let _ = closed_tx.send(());
        });
        let a = admission(&base);
        let asking = tokio::spawn(async move { a.enter_gate(Gate::Chat).await.is_ok() });
        settle().await;
        asking.abort();
        tokio::time::timeout(Duration::from_secs(1), closed_rx)
            .await
            .unwrap()
            .unwrap();
    }

    #[tokio::test]
    async fn a_grant_already_on_its_way_when_the_caller_leaves_is_given_back() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let deleted = Arc::new(Mutex::new(Vec::<String>::new()));
        let log = Arc::clone(&deleted);
        let (headers_tx, headers_rx) = tokio::sync::oneshot::channel::<()>();
        let headers_tx = Arc::new(Mutex::new(Some(headers_tx)));
        tokio::spawn(async move {
            loop {
                let (mut s, _) = listener.accept().await.unwrap();
                let log = Arc::clone(&log);
                let headers_tx = Arc::clone(&headers_tx);
                tokio::spawn(async move {
                    let mut buf = vec![0u8; 4096];
                    let n = s.read(&mut buf).await.unwrap_or(0);
                    let first = String::from_utf8_lossy(&buf[..n])
                        .lines()
                        .next()
                        .unwrap_or("")
                        .to_owned();
                    if first.starts_with("DELETE") {
                        log.lock().unwrap().push(first);
                        let _ = s
                            .write_all(
                                b"HTTP/1.1 204 X\r\ncontent-length: 0\r\nconnection: close\r\n\r\n",
                            )
                            .await;
                        return;
                    }
                    let body = r#"{"lease":"0123456789abcdef0123456789abcdef","ttlMs":30000}"#;
                    let head = format!(
                        "HTTP/1.1 201 X\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                        body.len()
                    );
                    let _ = s.write_all(head.as_bytes()).await;
                    let _ = s.flush().await;
                    if let Some(tx) = headers_tx.lock().unwrap().take() {
                        let _ = tx.send(());
                    }
                    tokio::time::sleep(Duration::from_millis(150)).await;
                    let _ = s.write_all(body.as_bytes()).await;
                });
            }
        });
        let a = admission(&base);
        let asking = tokio::spawn(async move { a.enter_gate(Gate::Chat).await.is_ok() });
        headers_rx.await.unwrap();
        settle().await;
        asking.abort();
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert_eq!(
            deleted.lock().unwrap().clone(),
            ["DELETE /internal/inference-admission/lease/0123456789abcdef0123456789abcdef HTTP/1.1"]
        );
    }
}
