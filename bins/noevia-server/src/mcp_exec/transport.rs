//! core `mcp.cjs` as the chat's tool call uses it (`connect`, `callTool`, `disconnect`): MCP over
//! streamable HTTP, one session per call and closed whatever happened. A directory server's requests
//! go through [`crate::mcp_egress::Egress`] (public addresses only, checked before and at connect
//! time); the operator's own servers (a LAN Nextcloud server, noevia's loopback server) use an
//! ordinary client, as core's `fetch` did. No redirect is followed and no proxy is used.
//!
//! **Cancellation.** A call that is dropped (the chat's Stop, a client that left, research's cancel)
//! must still close the session it opened: [`CloseGuard`] hands the session to a task of its own at
//! the moment it is dropped, with core's shortened wait for an aborted caller.
use crate::mcp_egress::{rpc_result, Egress};
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

/// A failed exchange. `Http` carries core's `httpStatus` (and the `Retry-After` a 429 may bring); its
/// message is core's `MCP <status>: <body>` and never reaches the model (a neutral line does).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RpcError {
    Http {
        status: u16,
        retry_after: Option<String>,
        message: String,
    },
    Msg(String),
}

impl RpcError {
    pub fn message(&self) -> &str {
        match self {
            RpcError::Http { message, .. } | RpcError::Msg(message) => message,
        }
    }
    pub fn http_status(&self) -> Option<u16> {
        match self {
            RpcError::Http { status, .. } => Some(*status),
            RpcError::Msg(_) => None,
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
            #[cfg(test)]
            lookup_gate: None,
        })
    }

    /// For directory servers: public addresses only.
    pub fn public(egress: &Arc<Egress>) -> Transport {
        Transport {
            client: egress.client().clone(),
            public: Some(Arc::clone(egress)),
            #[cfg(test)]
            lookup_gate: None,
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
    ) -> Result<Option<JValue>, RpcError> {
        if let Some(egress) = &self.public {
            if !egress.url_allowed(url).await {
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
                let detail = read_capped(&mut response).await.unwrap_or_default();
                return Err(RpcError::Http {
                    status,
                    retry_after,
                    message: format!("MCP {status}: {}", js_json::js_slice(&detail, 200)),
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
        )
        .await
        .map(|r| r.unwrap_or(JValue::Undefined))
    }

    /// core `disconnect`: best effort by contract; a session that never got an id has nothing to close.
    pub async fn disconnect(
        &self,
        url: &str,
        headers: &[(String, String)],
        session: &SessionId,
        timeout: Duration,
    ) -> bool {
        let Some(id) = session.take() else {
            return false;
        };
        #[cfg(test)]
        if let Some(gate) = &self.lookup_gate {
            if let Ok(permit) = gate.acquire().await {
                permit.forget();
            }
        }
        if let Some(egress) = &self.public {
            if !egress.url_allowed(url).await {
                return false;
            }
        }
        let request = self
            .with_headers(self.client.delete(url), headers)
            .header("mcp-protocol-version", PROTOCOL_VERSION)
            .header("mcp-session-id", id);
        match tokio::time::timeout(timeout, request.send()).await {
            Ok(Ok(r)) => r.status().is_success(),
            _ => false,
        }
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
        if self.session.get().is_none() {
            return;
        }
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let (transport, url, headers, session) = (
            self.transport.clone(),
            std::mem::take(&mut self.url),
            std::mem::take(&mut self.headers),
            self.session.clone(),
        );
        runtime.spawn(async move {
            transport
                .disconnect(&url, &headers, &session, ABORTED_DISCONNECT)
                .await;
        });
    }
}
