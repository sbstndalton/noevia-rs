//! The account sign-in provider's chat call in the front: `signin-transport`'s fetch over the
//! front's own sign-in broker ([`crate::chatgpt_oauth_routes::Broker`]) and a no-redirect HTTP
//! client, answered as the `/chat/completions` reply the chat loop reads.
//!
//! Inert: nothing calls it yet. A native turn on the sign-in row reaches it only once its caller
//! (the native turn's provider call path, which refuses the sign-in row today) checks that core
//! offered [`CAPS_TOKEN`]; no core sends that token yet.
//!
//! What it does, in core's terms (the sign-in provider module's `fetchFor`):
//! - the broker answers come straight from the broker, not over the internal store channel, but
//!   mapped exactly as core's store client maps that channel's answers (a session, a refusal with
//!   its status and code, "try again" when the broker is missing or slow);
//! - the change signal is core's file signal of the broker's change file (inode, size and mtime in
//!   nanoseconds; "" when the file does not exist yet; unreadable otherwise);
//! - the backend call refuses redirects (core: `redirect: 'error'`), uses no proxy, and is tied to
//!   the turn by ownership: dropping the answer (or the fetch future) closes the connection;
//! - failure texts never carry the backend's own error text (which can name the address): a failed
//!   send is "fetch failed" and a failed body read is "terminated", the texts core's fetch gives.
//!
//! Named differences from core: the connect step is bounded ([`CONNECT_TIMEOUT`]; core has no
//! bound), and a broker ask is bounded by [`BROKER_WAIT`] here as core's store channel bounds it.

use crate::chatgpt_oauth_routes::{generation_of, Broker, Fail};
use crate::App;
use axum::body::Body as HttpResponseBody;
use axum::http::{HeaderName, HeaderValue, Response, StatusCode};
use js_json::JValue;
use signin_transport::fetch::{self, Answer, Body, Call, Outgoing, Upstream, Wire};
use signin_transport::request::{Protocol, Session};
use signin_transport::session::{BrokerAnswer, SessionCache};
use signin_transport::texts::Texts;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::Duration;

/// The capability core sends once it hands sign-in turns to the front. No core sends it yet.
pub const CAPS_TOKEN: &str = "signin-transport-v1";
/// core's store channel wait for a sign-in ask (`CHATGPT_WAIT_MS`).
pub const BROKER_WAIT: Duration = Duration::from_secs(45);
/// The backend connect bound (core: none).
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
/// The message of a failed send (undici's `TypeError: fetch failed`).
pub const SEND_FAILED: &str = "fetch failed";
/// The message of a body read that failed part-way (undici's `TypeError: terminated`).
pub const READ_FAILED: &str = "terminated";

/// The process's access-token cache (core's `sessions`), shared by every call.
pub static SESSIONS: LazyLock<Mutex<SessionCache>> =
    LazyLock::new(|| Mutex::new(SessionCache::default()));

/// Whether core offered the sign-in transport (an exact token of its capability list).
pub fn offered(caps: &str) -> bool {
    caps.split(',').any(|c| c.trim() == CAPS_TOKEN)
}

/// core's `fileSignal(file)`: `<inode>:<size>:<mtime ns>`, "" when the file does not exist,
/// `None` (unreadable: nothing is cached) on any other failure.
pub fn file_signal(path: &Path) -> Option<String> {
    use std::os::unix::fs::MetadataExt;
    match std::fs::metadata(path) {
        Ok(m) => {
            let ns = i128::from(m.mtime()) * 1_000_000_000 + i128::from(m.mtime_nsec());
            Some(format!("{}:{}:{ns}", m.ino(), m.size()))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Some(String::new()),
        Err(_) => None,
    }
}

/// The broker half of the call: the front's sign-in state, or a test's fake.
pub trait Accounts: Send + Sync {
    /// The broker's change signal.
    fn signal(&self) -> Option<String>;
    /// A usable session; on the second ask with the token the backend refused.
    fn session(
        &self,
        user: &str,
        rejected: Option<&str>,
    ) -> impl Future<Output = BrokerAnswer> + Send;
    /// Mark the sign-in named by `generation` as failed.
    fn reject(&self, user: &str, generation: &str) -> impl Future<Output = ()> + Send;
    /// The account's catalogue entries; `None` when the broker refused or was not reached.
    fn catalogue(&self, user: &str) -> impl Future<Output = Option<JValue>> + Send;
}

/// core's store client `chatgptRefusal(status, body)` over the broker's own failure.
pub fn refusal_of(e: &Fail) -> BrokerAnswer {
    BrokerAnswer::Refused {
        error: Some(e.message.clone()),
        status: Some(f64::from(e.status)),
        code: e.code.map(str::to_owned),
    }
}

/// core's store client `chatgptSession` over the broker's session answer.
pub fn session_of(access: &str, account_id: &str, fed_ramp: bool, expires_at: Option<f64>, raw: &str) -> Session {
    Session {
        access_token: access.to_owned(),
        account_id: account_id.to_owned(),
        restricted_region: fed_ramp,
        expires_at: expires_at.filter(|e| e.is_finite()),
        generation: JValue::Str(generation_of(raw)),
    }
}

/// The front's sign-in broker as [`Accounts`]. Every ask first runs the broker's start sweep, as
/// the internal store op does.
pub struct FrontAccounts {
    app: Arc<App>,
    broker: Arc<Broker>,
}

impl FrontAccounts {
    /// `None` while the front has no sign-in broker (core's channel then answers "unavailable").
    pub fn new(app: &Arc<App>) -> Option<FrontAccounts> {
        let broker = app.chatgpt_oauth.as_ref().map(Arc::clone)?;
        Some(FrontAccounts {
            app: Arc::clone(app),
            broker,
        })
    }

    fn signal_path(&self) -> PathBuf {
        self.broker.signal_path()
    }
}

impl Accounts for FrontAccounts {
    fn signal(&self) -> Option<String> {
        file_signal(&self.signal_path())
    }

    async fn session(&self, user: &str, rejected: Option<&str>) -> BrokerAnswer {
        let ask = async {
            self.broker.sweep_once(&self.app).await;
            self.broker.session(&self.app, user, rejected).await
        };
        match tokio::time::timeout(BROKER_WAIT, ask).await {
            Ok(Ok((t, raw))) => BrokerAnswer::Ok(session_of(
                t.access.as_str(),
                &t.account_id,
                t.fed_ramp,
                t.expires_at,
                &raw,
            )),
            Ok(Err(e)) => refusal_of(&e),
            // core: the channel call threw, read as authError('transient').
            Err(_) => BrokerAnswer::Unreachable,
        }
    }

    async fn reject(&self, user: &str, generation: &str) {
        let ask = async {
            self.broker.sweep_once(&self.app).await;
            self.broker.reject(&self.app, user, generation).await
        };
        // Its own failure is ignored, as in core.
        let _ = tokio::time::timeout(BROKER_WAIT, ask).await;
    }

    async fn catalogue(&self, user: &str) -> Option<JValue> {
        let ask = async {
            self.broker.sweep_once(&self.app).await;
            self.broker.catalogue(&self.app, user, false).await
        };
        match tokio::time::timeout(BROKER_WAIT, ask).await {
            Ok(Ok((models, ..))) => Some(JValue::Arr(models.iter().map(|m| m.to_js()).collect())),
            _ => None,
        }
    }
}

/// A backend answer's body over the HTTP client. `None` once it ended or failed.
pub struct HttpBody(Option<reqwest::Response>);

impl Body for HttpBody {
    async fn chunk(&mut self) -> Option<Result<Vec<u8>, String>> {
        let response = self.0.as_mut()?;
        match response.chunk().await {
            Ok(Some(b)) => Some(Ok(b.to_vec())),
            Ok(None) => {
                self.0 = None;
                None
            }
            Err(_) => {
                self.0 = None;
                Some(Err(READ_FAILED.to_owned()))
            }
        }
    }
}

/// The backend client: no proxy, no redirect followed, a bounded connect.
pub fn client() -> Option<reqwest::Client> {
    reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(CONNECT_TIMEOUT)
        .build()
        .ok()
}

/// The statuses fetch answers without a body (the Fetch standard's null-body statuses).
fn bodyless(status: u16) -> bool {
    matches!(status, 101 | 103 | 204 | 205 | 304)
}

/// The statuses fetch follows (and `redirect: 'error'` rejects on).
fn redirect(status: u16) -> bool {
    matches!(status, 301 | 302 | 303 | 307 | 308)
}

/// The broker plus the backend client: `signin-transport`'s [`Wire`].
pub struct SigninWire<A> {
    pub accounts: A,
    pub client: reqwest::Client,
}

impl<A: Accounts> Wire for SigninWire<A> {
    type Body = HttpBody;

    fn signal(&self) -> Option<String> {
        self.accounts.signal()
    }

    async fn catalogue(&mut self, user: &str) -> Option<JValue> {
        self.accounts.catalogue(user).await
    }

    async fn session(&mut self, user: &str, rejected_token: Option<&str>) -> BrokerAnswer {
        self.accounts.session(user, rejected_token).await
    }

    async fn reject(&mut self, user: &str, generation: &JValue) {
        // core's store client sends String(generation || ''); the front's generation is always a
        // string ([`session_of`]), anything else names no sign-in.
        let g = generation.as_str().unwrap_or("");
        self.accounts.reject(user, g).await;
    }

    async fn send(&mut self, request: Outgoing<'_>) -> Result<Upstream<HttpBody>, String> {
        let mut builder = self.client.post(request.url);
        for (k, v) in request.headers {
            let (Ok(name), Ok(value)) = (
                HeaderName::from_bytes(k.as_bytes()),
                HeaderValue::from_str(v),
            ) else {
                return Err(SEND_FAILED.to_owned());
            };
            builder = builder.header(name, value);
        }
        let response = builder
            .body(request.body.to_owned())
            .send()
            .await
            .map_err(|_| SEND_FAILED.to_owned())?;
        let status = response.status().as_u16();
        if redirect(status) {
            return Err(SEND_FAILED.to_owned());
        }
        let headers: Vec<(String, String)> = response
            .headers()
            .iter()
            .map(|(k, v)| {
                (
                    k.as_str().to_owned(),
                    String::from_utf8_lossy(v.as_bytes()).into_owned(),
                )
            })
            .collect();
        let labelled = response
            .headers()
            .get("x-noevia-provider-message")
            .is_some_and(|v| v.as_bytes() == b"1");
        Ok(Upstream {
            status,
            labelled,
            headers,
            body: (!bodyless(status)).then_some(HttpBody(Some(response))),
        })
    }
}

/// What one call needs besides the wire: the texts, the backend's header names and its address.
#[derive(Debug, Clone)]
pub struct Transport {
    pub texts: Texts,
    pub protocol: Protocol,
    pub base_url: String,
}

/// A random version-4 UUID (`crypto.randomUUID()`), for the reply id.
pub fn uuid_v4() -> String {
    use rand_core::{OsRng, RngCore};
    let mut b = [0u8; 16];
    OsRng.fill_bytes(&mut b);
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    let h: String = b.iter().map(|x| format!("{x:02x}")).collect();
    format!(
        "{}-{}-{}-{}-{}",
        &h[0..8],
        &h[8..12],
        &h[12..16],
        &h[16..20],
        &h[20..32]
    )
}

fn now_ms() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0.0, |d| d.as_millis() as f64)
}

/// One chat call on the sign-in row: core's `fetchFor(user, { conversation })(url, { body })`.
pub async fn fetch_for<A: Accounts>(
    wire: &mut SigninWire<A>,
    cache: &Mutex<SessionCache>,
    transport: &Transport,
    user: &str,
    conversation: Option<&str>,
    url: &str,
    body: &str,
) -> Answer<HttpBody> {
    let reply_uuid = uuid_v4();
    let call = Call {
        texts: &transport.texts,
        protocol: &transport.protocol,
        base_url: &transport.base_url,
        user,
        conversation,
        url,
        body,
        now_ms: now_ms(),
        reply_uuid: &reply_uuid,
    };
    fetch::fetch(wire, cache, &call).await
}

/// Headers a pass-through answer does not carry on (the connection's own, re-framed by the server).
fn hop_by_hop(name: &str) -> bool {
    matches!(
        name,
        "connection" | "keep-alive" | "transfer-encoding" | "te" | "trailer" | "upgrade" | "content-length"
    )
}

fn set(res: &mut Response<HttpResponseBody>, name: &'static str, value: &'static str) {
    res.headers_mut()
        .insert(name, HeaderValue::from_static(value));
}

fn read_error(message: String) -> std::io::Error {
    std::io::Error::other(message)
}

/// The answer as the HTTP reply the chat loop reads. `Err(message)`: core's fetch rejected (the
/// chat loop's failed fetch).
pub fn into_response(answer: Answer<HttpBody>) -> Result<Response<HttpResponseBody>, String> {
    let with_status = |res: &mut Response<HttpResponseBody>, status: u16| {
        *res.status_mut() = StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_GATEWAY);
    };
    match answer {
        Answer::Thrown(m) => Err(m),
        Answer::Error(r) => {
            let mut res = Response::new(HttpResponseBody::from(signin_transport::reply::error_body(&r)));
            with_status(&mut res, r.status);
            set(&mut res, "content-type", "application/json");
            set(&mut res, "x-noevia-provider-message", "1");
            Ok(res)
        }
        Answer::Completion(text) => {
            let mut res = Response::new(HttpResponseBody::from(text));
            set(&mut res, "content-type", "application/json");
            Ok(res)
        }
        Answer::Passthrough(u) => {
            let stream = futures_util::stream::unfold(u.body, |body| async move {
                let mut body = body?;
                match body.chunk().await {
                    Some(Ok(c)) => Some((Ok(bytes::Bytes::from(c)), Some(body))),
                    Some(Err(e)) => Some((Err(read_error(e)), None)),
                    None => None,
                }
            });
            let mut res = Response::new(HttpResponseBody::from_stream(stream));
            with_status(&mut res, u.status);
            for (k, v) in &u.headers {
                if hop_by_hop(k) {
                    continue;
                }
                if let (Ok(name), Ok(value)) =
                    (HeaderName::from_bytes(k.as_bytes()), HeaderValue::from_str(v))
                {
                    res.headers_mut().append(name, value);
                }
            }
            Ok(res)
        }
        Answer::Stream(st) => {
            let stream = futures_util::stream::unfold(Some(st), |st| async move {
                let mut st = st?;
                loop {
                    match st.next().await {
                        Some(Ok(frames)) if frames.is_empty() => continue,
                        Some(Ok(frames)) => {
                            return Some((Ok(bytes::Bytes::from(frames.concat())), Some(st)))
                        }
                        Some(Err(e)) => return Some((Err(read_error(e)), None)),
                        None => return None,
                    }
                }
            });
            let mut res = Response::new(HttpResponseBody::from_stream(stream));
            set(&mut res, "content-type", "text/event-stream");
            Ok(res)
        }
    }
}

#[cfg(test)]
#[path = "signin_wire_tests.rs"]
mod tests;
