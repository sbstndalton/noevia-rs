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
        let id = parsed
            .get("lease")
            .and_then(Value::as_str)
            .filter(|l| valid_id(l))
            .ok_or_else(Refused::unavailable)?
            .to_owned();
        let ttl = parsed
            .get("ttlMs")
            .and_then(Value::as_u64)
            .filter(|t| *t > 0)
            .map(Duration::from_millis)
            .ok_or_else(Refused::unavailable)?;
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
        Ok(Lease {
            inner,
            headers,
            lost,
            renewing,
        })
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
}
