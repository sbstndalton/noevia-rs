//! core `mcp.cjs` as the chat's tool call uses it (`connect`, `callTool`, `disconnect`): MCP over
//! streamable HTTP, one session per call and closed whatever happened. A directory server's requests
//! go through [`crate::mcp_egress::Egress`] (public addresses only, checked before and at connect
//! time); the operator's own servers (a LAN Nextcloud server, noevia's loopback server) use an
//! ordinary client, as core's `fetch` did. No redirect is followed and no proxy is used.
//!
//! **Cancellation.** A call that is dropped (the chat's Stop, a client that left, research's cancel)
//! must still close the session it opened. The session id lives in a shared cell until the moment a
//! close is *started*; starting one takes the id out of the cell and hands it, in the same
//! synchronous step, to a task of its own ([`Transport::start_close`]) that is bounded by one
//! deadline covering the address check, the connection and the `DELETE`. Dropping the caller after
//! that point cannot lose the close, and the id can be taken only once, so two closes never
//! compete. [`CloseGuard`] starts that same close for a caller dropped before it got that far.
//!
//! **Errors.** A server's text (an HTTP body, a JSON-RPC message) can echo the credential the call
//! carried, so none of it is kept: an HTTP failure is its status, a JSON-RPC failure its code.
use crate::mcp_egress::{rpc_result, Egress};
use dispatch_permit::DispatchPermit;
use js_json::JValue;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// core mcp.cjs `PROTOCOL_VERSION`.
const PROTOCOL_VERSION: &str = "2025-06-18";
/// core mcp.cjs `MAX_RESPONSE_BYTES`.
const MAX_RESPONSE_BYTES: usize = 8 * 1024 * 1024;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
const CALL_TIMEOUT: Duration = Duration::from_secs(60);
pub(super) const DISCONNECT_TIMEOUT: Duration = Duration::from_secs(5);
/// core mcp.cjs `ABORTED_DISCONNECT_MS`.
const ABORTED_DISCONNECT: Duration = Duration::from_millis(1000);
const BODY_LIMIT: &str = "MCP: response body exceeded the 8 MB limit";

/// A lookup of whether an address is still public (`true`: allowed); see [`Transport::with_address_check`].
pub type AddressCheck = Arc<
    dyn Fn(String) -> std::pin::Pin<Box<dyn std::future::Future<Output = bool> + Send>>
        + Send
        + Sync,
>;

/// A failed exchange. `Http` carries core's `httpStatus` (and the `Retry-After` a 429 may bring); its
/// message is `MCP <status>` (core's added the server's body: dropped, it can echo a credential).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RpcError {
    Http {
        status: u16,
        retry_after: Option<String>,
        message: String,
    },
    Msg(String),
    /// The dispatch permit refused the send: the `tools/call` was never made.
    NotRun,
}

impl RpcError {
    pub fn message(&self) -> &str {
        match self {
            RpcError::Http { message, .. } | RpcError::Msg(message) => message,
            RpcError::NotRun => "the call was not sent",
        }
    }
    pub fn http_status(&self) -> Option<u16> {
        match self {
            RpcError::Http { status, .. } => Some(*status),
            RpcError::Msg(_) | RpcError::NotRun => None,
        }
    }
}

fn fetch_failed() -> RpcError {
    RpcError::Msg("fetch failed".to_owned())
}

/// The abort a deadline raises in core (`controller.abort()`): undici's message.
fn aborted() -> RpcError {
    RpcError::Msg("This operation was aborted".to_owned())
}

/// The session id a server hands out at `initialize`, shared with the guard that closes it.
#[derive(Clone, Default)]
pub struct SessionId(Arc<Mutex<Option<String>>>);

impl SessionId {
    fn lock(&self) -> std::sync::MutexGuard<'_, Option<String>> {
        self.0.lock().unwrap_or_else(|e| e.into_inner())
    }
    fn get(&self) -> Option<String> {
        self.lock().clone()
    }
    fn set(&self, id: &str) {
        *self.lock() = Some(id.to_owned());
    }
    fn take(&self) -> Option<String> {
        self.lock().take()
    }
}

/// core mcp.cjs `RESERVED_HEADERS`: a credential's own header names may not replace these.
fn reserved(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "content-type" | "accept" | "mcp-protocol-version" | "mcp-session-id"
    )
}

#[derive(Clone)]
pub struct Transport {
    client: reqwest::Client,
    public: Option<Arc<Egress>>,
    /// Replaces the public-address check (a test that holds the lookup open); the egress's own when unset.
    address_check: Option<AddressCheck>,
    /// Tests only: stands in for an address lookup that has not answered yet.
    #[cfg(test)]
    lookup_gate: Option<Arc<tokio::sync::Semaphore>>,
}

impl Transport {
    /// For the operator's own servers: an ordinary client.
    pub fn operator() -> Option<Transport> {
        let client = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .ok()?;
        Some(Transport {
            client,
            public: None,
            address_check: None,
            #[cfg(test)]
            lookup_gate: None,
        })
    }

    /// For directory servers: public addresses only.
    pub fn public(egress: &Arc<Egress>) -> Transport {
        Transport {
            client: egress.client().clone(),
            public: Some(Arc::clone(egress)),
            address_check: None,
            #[cfg(test)]
            lookup_gate: None,
        }
    }

    /// This transport with its own address check in place of the egress's (the lookup a request waits on before it is sent).
    pub fn with_address_check(mut self, check: AddressCheck) -> Transport {
        self.address_check = Some(check);
        self
    }

    /// Whether `url` still resolves only to public addresses: the one await between a request being built and its send.
    async fn address_allowed(&self, url: &str) -> bool {
        if let Some(check) = &self.address_check {
            return check(url.to_owned()).await;
        }
        match &self.public {
            Some(egress) => egress.url_allowed(url).await,
            None => true,
        }
    }

    fn with_headers(
        &self,
        mut request: reqwest::RequestBuilder,
        headers: &[(String, String)],
    ) -> reqwest::RequestBuilder {
        for (k, v) in headers.iter().filter(|(k, _)| !reserved(k)) {
            request = request.header(k.as_str(), v.as_str());
        }
        request
    }

    /// One JSON-RPC round trip (core mcp.cjs `rpc`): the deadline covers the exchange and the body.
    #[allow(clippy::too_many_arguments)]
    async fn rpc(
        &self,
        url: &str,
        session: &SessionId,
        headers: &[(String, String)],
        id: Option<i64>,
        method: &str,
        params: JValue,
        timeout: Duration,
        permit: Option<&dyn DispatchPermit>,
    ) -> Result<Option<JValue>, RpcError> {
        if self.public.is_some() || self.address_check.is_some() {
            if !self.address_allowed(url).await {
                return Err(fetch_failed());
            }
        }
        let mut pairs = vec![("jsonrpc", JValue::Str("2.0".into()))];
        if let Some(id) = id {
            pairs.push(("id", JValue::Num(id as f64)));
        }
        pairs.push(("method", JValue::Str(method.into())));
        if !matches!(params, JValue::Undefined) {
            pairs.push(("params", params));
        }
        let body = js_json::stringify(&JValue::obj(pairs)).unwrap_or_default();
        let exchange = async {
            let mut request = self.with_headers(self.client.post(url), headers);
            request = request
                .header("content-type", "application/json")
                .header("accept", "application/json, text/event-stream")
                .header("mcp-protocol-version", PROTOCOL_VERSION);
            if let Some(s) = session.get() {
                request = request.header("mcp-session-id", s);
            }
            // The outbound boundary: the permit is asked here, after every await that prepared this request (the
            // address lookup above was the last) and with nothing between its answer and the send. A refusal means
            // the request is never made.
            if let Some(permit) = permit {
                if !permit.authorize() {
                    return Err(RpcError::NotRun);
                }
            }
            let mut response = request
                .body(body)
                .send()
                .await
                .map_err(|_| fetch_failed())?;
            let status = response.status().as_u16();
            // undici's `redirect: 'error'` throws on a redirect answer.
            if matches!(status, 301 | 302 | 303 | 307 | 308) {
                return Err(fetch_failed());
            }
            if let Some(sid) = response
                .headers()
                .get("mcp-session-id")
                .and_then(|v| v.to_str().ok())
                .filter(|s| !s.is_empty())
            {
                session.set(sid);
            }
            let header = |name: &str| {
                response
                    .headers()
                    .get(name)
                    .and_then(|v| v.to_str().ok())
                    .map(str::to_owned)
            };
            let (content_type, retry_after) = (header("content-type"), header("retry-after"));
            if !(200..300).contains(&status) {
                // The body is never read into a message: it is the server's text.
                return Err(RpcError::Http {
                    status,
                    retry_after,
                    message: format!("MCP {status}"),
                });
            }
            // A notification has no id and the server answers 202 with an empty body.
            let Some(id) = id else { return Ok(None) };
            let text = read_capped(&mut response)
                .await
                .map_err(|e| e.unwrap_or_else(|| RpcError::Msg(BODY_LIMIT.to_owned())))?;
            let sse = content_type
                .as_deref()
                .unwrap_or("")
                .contains("text/event-stream");
            rpc_result(id, sse, &text).map_err(RpcError::Msg)
        };
        tokio::time::timeout(timeout, exchange)
            .await
            .unwrap_or_else(|_| Err(aborted()))
    }

    /// core `connect`: `initialize`, then the `initialized` notification. A failure closes whatever
    /// session the server already issued.
    pub async fn connect(
        &self,
        url: &str,
        headers: &[(String, String)],
        session: &SessionId,
        ids: &AtomicI64,
    ) -> Result<(), RpcError> {
        let init = ids.fetch_add(1, Ordering::Relaxed) + 1;
        let opened = async {
            self.rpc(
                url,
                session,
                headers,
                Some(init),
                "initialize",
                JValue::obj([
                    ("protocolVersion", JValue::Str(PROTOCOL_VERSION.into())),
                    ("capabilities", JValue::obj::<&str>([])),
                    (
                        "clientInfo",
                        JValue::obj([
                            ("name", JValue::Str("noevia".into())),
                            ("version", JValue::Str("1".into())),
                        ]),
                    ),
                ]),
                CONNECT_TIMEOUT,
                None,
            )
            .await?;
            self.rpc(
                url,
                session,
                headers,
                None,
                "notifications/initialized",
                JValue::Undefined,
                CONNECT_TIMEOUT,
                None,
            )
            .await
            .map(|_| ())
        }
        .await;
        if let Err(e) = opened {
            self.disconnect(url, headers, session, DISCONNECT_TIMEOUT)
                .await;
            return Err(e);
        }
        Ok(())
    }

    /// core `callTool`: `tools/call`, whose `result` is returned as the server sent it.
    pub async fn call_tool(
        &self,
        url: &str,
        headers: &[(String, String)],
        session: &SessionId,
        ids: &AtomicI64,
        name: &str,
        args: &JValue,
        permit: Option<&dyn DispatchPermit>,
    ) -> Result<JValue, RpcError> {
        let id = ids.fetch_add(1, Ordering::Relaxed) + 1;
        let arguments = if args.truthy() {
            args.clone()
        } else {
            JValue::obj::<&str>([])
        };
        self.rpc(
            url,
            session,
            headers,
            Some(id),
            "tools/call",
            JValue::obj([("name", JValue::Str(name.into())), ("arguments", arguments)]),
            CALL_TIMEOUT,
            permit,
        )
        .await
        .map(|r| r.unwrap_or(JValue::Undefined))
    }

    /// core `disconnect`: best effort by contract; a session that never got an id has nothing to close.
    /// The close runs in a task of its own (see [`Transport::start_close`]), so dropping this future
    /// while it waits cannot lose it, and the wait is `timeout` for the whole close.
    pub async fn disconnect(
        &self,
        url: &str,
        headers: &[(String, String)],
        session: &SessionId,
        timeout: Duration,
    ) -> bool {
        match self.start_close(url, headers, session, timeout) {
            Some(task) => task.await.unwrap_or(false),
            None => false,
        }
    }

    /// Takes the session id out of its cell and gives it, with the duty to close it, to a task that
    /// belongs to no caller: there is no await between the two steps. The one deadline covers the
    /// address check (a directory server's name is looked up again), the connection and the
    /// `DELETE`. `None`: no id, or no runtime to run the task on.
    fn start_close(
        &self,
        url: &str,
        headers: &[(String, String)],
        session: &SessionId,
        timeout: Duration,
    ) -> Option<tokio::task::JoinHandle<bool>> {
        let runtime = tokio::runtime::Handle::try_current().ok()?;
        let id = session.take()?;
        let (transport, url, headers) = (self.clone(), url.to_owned(), headers.to_vec());
        Some(runtime.spawn(async move {
            tokio::time::timeout(timeout, transport.delete_session(&url, &headers, &id))
                .await
                .unwrap_or(false)
        }))
    }

    async fn delete_session(&self, url: &str, headers: &[(String, String)], id: &str) -> bool {
        #[cfg(test)]
        if let Some(gate) = &self.lookup_gate {
            if let Ok(permit) = gate.acquire().await {
                permit.forget();
            }
        }
        if (self.public.is_some() || self.address_check.is_some())
            && !self.address_allowed(url).await
        {
            return false;
        }
        let request = self
            .with_headers(self.client.delete(url), headers)
            .header("mcp-protocol-version", PROTOCOL_VERSION)
            .header("mcp-session-id", id);
        matches!(request.send().await, Ok(r) if r.status().is_success())
    }
}

/// The body up to the cap. `Err(None)`: over the cap; `Err(Some)`: the stream failed (undici's "terminated").
async fn read_capped(response: &mut reqwest::Response) -> Result<String, Option<RpcError>> {
    let mut bytes: Vec<u8> = Vec::new();
    loop {
        match response.chunk().await {
            Ok(Some(chunk)) => {
                if bytes.len() + chunk.len() > MAX_RESPONSE_BYTES {
                    return Err(None);
                }
                bytes.extend_from_slice(&chunk);
            }
            Ok(None) => return Ok(String::from_utf8_lossy(&bytes).into_owned()),
            Err(_) => return Err(Some(RpcError::Msg("terminated".to_owned()))),
        }
    }
}

/// Closes a session its owner never got to close: the call was dropped between `connect` and `disconnect`.
pub struct CloseGuard {
    transport: Transport,
    url: String,
    headers: Vec<(String, String)>,
    session: SessionId,
}

impl CloseGuard {
    pub fn new(
        transport: &Transport,
        url: &str,
        headers: &[(String, String)],
        session: &SessionId,
    ) -> CloseGuard {
        CloseGuard {
            transport: transport.clone(),
            url: url.to_owned(),
            headers: headers.to_vec(),
            session: session.clone(),
        }
    }
}

impl Drop for CloseGuard {
    fn drop(&mut self) {
        // A close already started took the id; there is then nothing left to start.
        let _ =
            self.transport
                .start_close(&self.url, &self.headers, &self.session, ABORTED_DISCONNECT);
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    /// A synthetic MCP server on loopback: counts the `DELETE`s it reads; `hold` makes it answer a
    /// `DELETE` late; `fail_initialized` answers the `initialized` notification with a 500.
    struct Server {
        url: String,
        deletes: Arc<AtomicUsize>,
    }

    async fn server(hold: Duration, fail_initialized: bool) -> Server {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!(
            "http://127.0.0.1:{}/mcp",
            listener.local_addr().unwrap().port()
        );
        let deletes = Arc::new(AtomicUsize::new(0));
        let count = Arc::clone(&deletes);
        tokio::spawn(async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                let count = Arc::clone(&count);
                tokio::spawn(async move {
                    let mut buf = Vec::new();
                    let mut chunk = [0u8; 4096];
                    loop {
                        let n = socket.read(&mut chunk).await.unwrap_or(0);
                        if n == 0 {
                            return;
                        }
                        buf.extend_from_slice(&chunk[..n]);
                        let text = String::from_utf8_lossy(&buf).into_owned();
                        if let Some((head, body)) = text.split_once("\r\n\r\n") {
                            let want = head
                                .lines()
                                .find_map(|l| {
                                    l.to_ascii_lowercase()
                                        .strip_prefix("content-length: ")
                                        .and_then(|v| v.trim().parse::<usize>().ok())
                                })
                                .unwrap_or(0);
                            if body.len() >= want {
                                break;
                            }
                        }
                    }
                    let text = String::from_utf8_lossy(&buf).into_owned();
                    let reply = if text.starts_with("DELETE") {
                        count.fetch_add(1, Ordering::SeqCst);
                        tokio::time::sleep(hold).await;
                        "HTTP/1.1 200 OK\r\ncontent-length: 0\r\nconnection: close\r\n\r\n"
                            .to_owned()
                    } else if text.contains("notifications/initialized") {
                        let status = if fail_initialized { "500 X" } else { "202 X" };
                        format!(
                            "HTTP/1.1 {status}\r\ncontent-length: 0\r\nconnection: close\r\n\r\n"
                        )
                    } else {
                        let id = text
                            .split("\"id\":")
                            .nth(1)
                            .map(|r| {
                                r.chars()
                                    .take_while(char::is_ascii_digit)
                                    .collect::<String>()
                            })
                            .unwrap_or_default();
                        let body = format!("{{\"jsonrpc\":\"2.0\",\"id\":{id},\"result\":{{}}}}");
                        format!(
                            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\nmcp-session-id: S-1\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                            body.len()
                        )
                    };
                    let _ = socket.write_all(reply.as_bytes()).await;
                });
            }
        });
        Server { url, deletes }
    }

    fn gated() -> (Transport, Arc<tokio::sync::Semaphore>) {
        let mut t = Transport::operator().unwrap();
        let gate = Arc::new(tokio::sync::Semaphore::new(0));
        t.lookup_gate = Some(Arc::clone(&gate));
        (t, gate)
    }

    async fn until(what: impl Fn() -> bool) -> bool {
        for _ in 0..200 {
            if what() {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        false
    }

    /// The task a call runs in: the guard `call_once` holds, then `disconnect` (what the abort lands in).
    fn closing(
        t: &Transport,
        url: &str,
        session: &SessionId,
        timeout: Duration,
    ) -> tokio::task::JoinHandle<bool> {
        let (t, url, session) = (t.clone(), url.to_owned(), session.clone());
        tokio::spawn(async move {
            let _guard = CloseGuard::new(&t, &url, &[], &session);
            t.disconnect(&url, &[], &session, timeout).await
        })
    }

    /// Review P2: cancelled while the address of the session's server is being looked up, no `DELETE`
    /// had been sent and the id was gone with the dropped future.
    #[tokio::test]
    async fn a_disconnect_dropped_during_the_address_lookup_still_closes_the_session_once() {
        let s = server(Duration::ZERO, false).await;
        let (t, gate) = gated();
        let session = SessionId::default();
        session.set("S-1");
        let task = closing(&t, &s.url, &session, DISCONNECT_TIMEOUT);
        tokio::time::sleep(Duration::from_millis(150)).await;
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        assert_eq!(
            s.deletes.load(Ordering::SeqCst),
            0,
            "the lookup has not answered yet"
        );
        gate.add_permits(1);
        assert!(
            until(|| s.deletes.load(Ordering::SeqCst) == 1).await,
            "the session was never closed"
        );
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(
            s.deletes.load(Ordering::SeqCst),
            1,
            "one close, no competing second"
        );
    }

    /// A lookup that never answers does not hold the close open for ever: the whole close, address
    /// check included, is bounded by the deadline it was given.
    #[tokio::test]
    async fn a_close_stalled_in_the_address_check_ends_at_its_deadline() {
        let s = server(Duration::ZERO, false).await;
        let (t, _gate) = gated();
        let session = SessionId::default();
        session.set("S-1");
        let started = std::time::Instant::now();
        let closed = tokio::time::timeout(
            Duration::from_secs(3),
            t.disconnect(&s.url, &[], &session, Duration::from_millis(300)),
        )
        .await
        .expect("the close outlived its deadline");
        assert!(!closed);
        assert!(started.elapsed() < Duration::from_secs(3));
        assert_eq!(s.deletes.load(Ordering::SeqCst), 0);
        assert!(
            session.get().is_none(),
            "the id is spent, not left for a second close"
        );
    }

    /// Cancelled while the `DELETE` is in flight: it is not sent a second time by the guard.
    #[tokio::test]
    async fn a_disconnect_dropped_during_the_delete_is_not_closed_twice() {
        let s = server(Duration::from_secs(30), false).await;
        let t = Transport::operator().unwrap();
        let session = SessionId::default();
        session.set("S-1");
        let task = closing(&t, &s.url, &session, DISCONNECT_TIMEOUT);
        assert!(until(|| s.deletes.load(Ordering::SeqCst) == 1).await);
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        tokio::time::sleep(Duration::from_millis(400)).await;
        assert_eq!(
            s.deletes.load(Ordering::SeqCst),
            1,
            "no duplicate competing close"
        );
    }

    /// The close after a failed `initialized` (connect's own disconnect) is covered the same way.
    #[tokio::test]
    async fn a_connect_dropped_while_it_closes_after_a_failed_initialized_still_closes() {
        let s = server(Duration::ZERO, true).await;
        let (t, gate) = gated();
        let session = SessionId::default();
        let ids = AtomicI64::new(0);
        let task = tokio::spawn({
            let (t, url, session) = (t.clone(), s.url.clone(), session.clone());
            async move {
                let _guard = CloseGuard::new(&t, &url, &[], &session);
                t.connect(&url, &[], &session, &ids).await
            }
        });
        // initialize answered (the session id was known) and `initialized` failed: connect is in its
        // disconnect, which has taken the id and waits on the lookup.
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert!(
            session.get().is_none(),
            "connect never reached its disconnect"
        );
        assert_eq!(s.deletes.load(Ordering::SeqCst), 0);
        task.abort();
        let _ = task.await;
        gate.add_permits(1);
        assert!(
            until(|| s.deletes.load(Ordering::SeqCst) == 1).await,
            "the session was never closed"
        );
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(s.deletes.load(Ordering::SeqCst), 1);
    }
}
