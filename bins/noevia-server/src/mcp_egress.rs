//! Everything the MCP credential broker sends to a stranger (sbstndalton/noevia#2202): the OAuth
//! metadata, registration and token endpoints, an MCP server's own address, and the one short MCP
//! session that proves a person's API key before it is kept. All of it comes from a directory
//! entry or from a server's metadata, so none of it is trusted.
//!
//! The rules are core's (`directory-mcp.cjs` `hostedUrlOk`, `mcp-wiring.cjs`
//! `createDirectoryUrlAllowed`, `public-fetch.cjs`), with the policy decided by the `ssrf-policy`
//! crate:
//!
//! - the address is `https://` with no placeholder or whitespace (a QA run may also name a
//!   synthetic server at `http://127.0.0.1:<port>/…` when `NOEVIA_QA_ALLOW_LOOPBACK_MCP=1`);
//! - it is a public address: an IP literal is judged, a name is resolved and EVERY answer must be
//!   public (checked before the request, and again at connect time by the client's own resolver,
//!   so a name that re-resolves between the two - DNS rebinding - cannot reach the home network);
//! - no redirect is ever followed (core passes `redirect: 'error'`), no environment proxy is used,
//!   every exchange has a deadline, and every reply a size cap. Nothing here logs a header value
//!   or a body.
use js_json::JValue;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

/// core mcp.cjs `PROTOCOL_VERSION`.
const PROTOCOL_VERSION: &str = "2025-06-18";
/// Core's `MAX_JSON_BYTES` for OAuth metadata, registrations and tokens.
pub const JSON_CAP: usize = 256 * 1024;
/// core mcp.cjs `MAX_RESPONSE_BYTES`.
const MCP_CAP: usize = 8 * 1024 * 1024;
/// How long a name may take to resolve.
const LOOKUP_DEADLINE: Duration = Duration::from_secs(5);
/// The most pages of `tools/list` a key check follows (core `MCP_LIST_TOOLS_MAX_PAGES`).
const MAX_PAGES: usize = 20;

/// `directory-mcp.cjs` `hostedUrlOk`.
pub fn hosted_url_ok(url: &str, qa_loopback: bool) -> bool {
    let plain = |s: &str| !s.chars().any(|c| c == '{' || c == '}' || c.is_whitespace());
    if let Some(rest) = url.strip_prefix("https://") {
        return !rest.is_empty() && plain(rest);
    }
    qa_loopback && loopback_url(url) && plain(url)
}

/// `^http://127.0.0.1:\d+/` (the QA stand-in) and nothing else.
fn loopback_url(url: &str) -> bool {
    let Some(rest) = url.strip_prefix("http://127.0.0.1:") else {
        return false;
    };
    let digits = rest.chars().take_while(char::is_ascii_digit).count();
    digits > 0 && rest.get(digits..).is_some_and(|r| r.starts_with('/'))
}

/// core `createDirectoryUrlAllowed`'s QA clause: `^https?://127.0.0.1:\d+/`.
fn qa_loopback_url(url: &str) -> bool {
    let rest = url
        .strip_prefix("http://127.0.0.1:")
        .or_else(|| url.strip_prefix("https://127.0.0.1:"));
    rest.is_some_and(|rest| {
        let digits = rest.chars().take_while(char::is_ascii_digit).count();
        digits > 0 && rest.get(digits..).is_some_and(|r| r.starts_with('/'))
    })
}

/// The connect-time half of core's public-fetch: resolves as usual and refuses the connection
/// unless every answer is a public address.
struct PublicOnly;

impl reqwest::dns::Resolve for PublicOnly {
    fn resolve(&self, name: reqwest::dns::Name) -> reqwest::dns::Resolving {
        let host = name.as_str().to_string();
        Box::pin(async move {
            let answers: Vec<SocketAddr> =
                tokio::time::timeout(LOOKUP_DEADLINE, tokio::net::lookup_host((host.as_str(), 0)))
                    .await
                    .map_err(|_| std::io::Error::other("lookup timed out"))??
                    .collect();
            let ips: Vec<String> = answers.iter().map(|a| a.ip().to_string()).collect();
            if !ssrf_policy::addresses_public(&ips) {
                return Err(Box::new(std::io::Error::other(
                    "refused: the name does not resolve only to public addresses",
                ))
                    as Box<dyn std::error::Error + Send + Sync>);
            }
            let out: reqwest::dns::Addrs = Box::new(answers.into_iter());
            Ok(out)
        })
    }
}

/// The exchange did not happen: the address was refused, the request failed, a redirect came back
/// (never followed) or the deadline passed. Nothing about why leaves this module.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Refused;

/// One reply: the status, the few headers anyone reads, and the body (`None` when it was over the
/// cap or cut off mid-way, as core's capped readers reject).
pub struct Reply {
    pub status: u16,
    pub www_authenticate: String,
    pub content_type: String,
    /// The `mcp-session-id` header, if the server sent one.
    pub session: Option<String>,
    pub text: Option<String>,
}

impl Reply {
    pub fn ok(&self) -> bool {
        (200..300).contains(&self.status)
    }
    /// The body as JSON, or `None` (core `readJsonCapped(r).catch(() => null)`).
    pub fn json(&self) -> Option<JValue> {
        js_json::parse(self.text.as_deref()?).ok()
    }
}

/// The guarded client.
pub struct Egress {
    client: reqwest::Client,
    qa_loopback: bool,
}

impl Egress {
    pub fn new(qa_loopback: bool) -> Option<Arc<Egress>> {
        let client = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .dns_resolver(Arc::new(PublicOnly))
            .user_agent("noevia")
            .build()
            .ok()?;
        Some(Arc::new(Egress {
            client,
            qa_loopback,
        }))
    }

    pub fn qa_loopback(&self) -> bool {
        self.qa_loopback
    }

    /// `hostedUrlOk(url) && directoryUrlAllowed(url)`: an https public address (or the QA
    /// loopback stand-in). A name that does not resolve, or resolves to anything not public, is
    /// refused.
    pub async fn url_allowed(&self, url: &str) -> bool {
        if !hosted_url_ok(url, self.qa_loopback) {
            return false;
        }
        if self.qa_loopback && qa_loopback_url(url) {
            return true;
        }
        let Ok(accepted) = ssrf_policy::check_url(url, ssrf_policy::Mode::Fetch, false) else {
            return false;
        };
        match accepted.kind {
            ssrf_policy::Kind::Ip => true,
            ssrf_policy::Kind::Name => {
                let lookup = tokio::net::lookup_host((accepted.host.as_str(), 0));
                let Ok(Ok(found)) = tokio::time::timeout(LOOKUP_DEADLINE, lookup).await else {
                    return false;
                };
                let ips: Vec<String> = found.map(|a| a.ip().to_string()).collect();
                ssrf_policy::addresses_public(&ips)
            }
        }
    }

    /// One exchange: the request goes out only if the address passes [`Egress::url_allowed`] (so
    /// no caller can forget the check), under `deadline`, with the body read up to `cap`.
    pub async fn send(
        &self,
        request: reqwest::RequestBuilder,
        url: &str,
        deadline: Duration,
        cap: usize,
    ) -> Result<Reply, Refused> {
        if !self.url_allowed(url).await {
            return Err(Refused);
        }
        let exchange = async {
            let mut response = request.send().await.map_err(|_| Refused)?;
            let status = response.status().as_u16();
            // undici's `redirect: 'error'` throws on a redirect answer.
            if matches!(status, 301 | 302 | 303 | 307 | 308) {
                return Err(Refused);
            }
            let header = |name: &str| {
                response
                    .headers()
                    .get(name)
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or("")
                    .to_owned()
            };
            let (www_authenticate, content_type) =
                (header("www-authenticate"), header("content-type"));
            let session = Some(header("mcp-session-id")).filter(|s| !s.is_empty());
            let mut body = Vec::new();
            let mut over = false;
            loop {
                match response.chunk().await {
                    Ok(Some(chunk)) => {
                        if body.len() + chunk.len() > cap {
                            over = true;
                            break;
                        }
                        body.extend_from_slice(&chunk);
                    }
                    Ok(None) => break,
                    Err(_) => {
                        over = true;
                        break;
                    }
                }
            }
            Ok(Reply {
                status,
                www_authenticate,
                content_type,
                session,
                text: (!over).then(|| String::from_utf8_lossy(&body).into_owned()),
            })
        };
        match tokio::time::timeout(deadline, exchange).await {
            Ok(Ok(reply)) => Ok(reply),
            _ => Err(Refused),
        }
    }

    /// core mcp-oauth.cjs `getJson`: `None` on any failure to get a JSON answer; an address that is
    /// not allowed is `Err(what)` (the caller words it).
    pub async fn get_json(&self, url: &str) -> Result<Option<JValue>, Refused> {
        if !self.url_allowed(url).await {
            return Err(Refused);
        }
        let request = self
            .client
            .get(url)
            .header("accept", "application/json")
            .header("user-agent", "noevia");
        match self
            .send(request, url, Duration::from_secs(8), JSON_CAP)
            .await
        {
            Ok(r) if r.ok() => Ok(r.json()),
            _ => Ok(None),
        }
    }

    /// A form or JSON POST (token, registration, revocation).
    pub async fn post(
        &self,
        url: &str,
        headers: &[(&str, String)],
        body: String,
        deadline: Duration,
    ) -> Result<Reply, Refused> {
        let mut request = self.client.post(url);
        for (k, v) in headers {
            request = request.header(*k, v.as_str());
        }
        self.send(request.body(body), url, deadline, JSON_CAP).await
    }

    /// core `probeMcpAuth`: an unauthenticated `initialize`, to learn whether a server wants OAuth
    /// (401 and `WWW-Authenticate`). `(status, challenge)`; `(0, "")` when it did not answer.
    pub async fn probe_auth(&self, url: &str) -> (u16, String) {
        let body = rpc_body(
            Some(1),
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
        );
        let request = self
            .client
            .post(url)
            .header("content-type", "application/json")
            .header("accept", "application/json, text/event-stream")
            .body(body);
        match self
            .send(request, url, Duration::from_secs(8), JSON_CAP)
            .await
        {
            Ok(r) => (r.status, r.www_authenticate),
            Err(Refused) => (0, String::new()),
        }
    }
}

fn rpc_body(id: Option<i64>, method: &str, params: JValue) -> String {
    let mut pairs = vec![("jsonrpc", JValue::Str("2.0".into()))];
    if let Some(id) = id {
        pairs.push(("id", JValue::Num(id as f64)));
    }
    pairs.push(("method", JValue::Str(method.into())));
    pairs.push(("params", params));
    js_json::stringify(&JValue::obj(pairs)).unwrap_or_default()
}

/// core mcp.cjs `RESERVED_HEADERS`: the key's own header names may not replace these.
fn reserved(name: &str) -> bool {
    [
        "content-type",
        "accept",
        "mcp-protocol-version",
        "mcp-session-id",
    ]
    .contains(&name.to_ascii_lowercase().as_str())
}

/// The one MCP session a person's key is proven with before it is stored (core `discoverOneServer`
/// for a directory server with `pendingHeaders`): `initialize`, the `initialized` notification and
/// `tools/list` to its end, then the session is closed. `Ok(tool count)`, or the failure in core's
/// words (`MCP 401: …`, `MCP error -32000: …`, `its address no longer resolves to a public host`).
pub async fn check_key(
    egress: &Egress,
    url: &str,
    headers: &[(String, String)],
    next_id: &std::sync::atomic::AtomicI64,
) -> Result<usize, String> {
    if !egress.url_allowed(url).await {
        return Err("its address no longer resolves to a public host".into());
    }
    let mut session: Option<String> = None;
    let outcome = async {
        let init = next_id.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
        rpc(
            egress,
            url,
            headers,
            &mut session,
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
        )
        .await?;
        rpc(
            egress,
            url,
            headers,
            &mut session,
            None,
            "notifications/initialized",
            JValue::Undefined,
        )
        .await?;
        let mut count = 0usize;
        let mut cursor: Option<String> = None;
        for _ in 0..MAX_PAGES {
            let id = next_id.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
            let params = match &cursor {
                Some(c) => JValue::obj([("cursor", JValue::Str(c.clone()))]),
                None => JValue::obj::<&str>([]),
            };
            let result = rpc(
                egress,
                url,
                headers,
                &mut session,
                Some(id),
                "tools/list",
                params,
            )
            .await?
            .unwrap_or(JValue::Null);
            if let JValue::Arr(tools) = result.get("tools") {
                count += tools.len();
            }
            cursor = result
                .get("nextCursor")
                .as_str()
                .filter(|c| !c.is_empty())
                .map(str::to_owned);
            if cursor.is_none() {
                break;
            }
        }
        Ok(count)
    }
    .await;
    // Closed whatever happened (a session that never got an id has nothing to close).
    if let Some(id) = session.take() {
        let mut request = egress
            .client
            .delete(url)
            .header("mcp-protocol-version", PROTOCOL_VERSION)
            .header("mcp-session-id", id);
        for (k, v) in headers.iter().filter(|(k, _)| !reserved(k)) {
            request = request.header(k.as_str(), v.as_str());
        }
        let _ = egress
            .send(request, url, Duration::from_secs(5), 4096)
            .await;
    }
    outcome
}

/// One JSON-RPC round trip (core mcp.cjs `rpc`). A notification has no id and expects no body.
async fn rpc(
    egress: &Egress,
    url: &str,
    headers: &[(String, String)],
    session: &mut Option<String>,
    id: Option<i64>,
    method: &str,
    params: JValue,
) -> Result<Option<JValue>, String> {
    let body = rpc_body(
        id,
        method,
        if matches!(params, JValue::Undefined) {
            JValue::obj::<&str>([])
        } else {
            params
        },
    );
    let mut request = egress.client.post(url);
    for (k, v) in headers.iter().filter(|(k, _)| !reserved(k)) {
        request = request.header(k.as_str(), v.as_str());
    }
    request = request
        .header("content-type", "application/json")
        .header("accept", "application/json, text/event-stream")
        .header("mcp-protocol-version", PROTOCOL_VERSION);
    if let Some(s) = session.as_deref() {
        request = request.header("mcp-session-id", s);
    }
    let reply = egress
        .send(request.body(body), url, Duration::from_secs(30), MCP_CAP)
        .await
        .map_err(|Refused| "fetch failed".to_string())?;
    if let Some(sid) = reply.session.clone().filter(|s| !s.is_empty()) {
        *session = Some(sid);
    }
    if !reply.ok() {
        let detail = reply.text.as_deref().unwrap_or("");
        let snippet: String = detail.chars().take(200).collect();
        return Err(format!("MCP {}: {snippet}", reply.status));
    }
    let Some(id) = id else { return Ok(None) };
    let text = reply
        .text
        .ok_or_else(|| "MCP: response body exceeded the 8 MB limit".to_string())?;
    let units: Vec<u16> = text.encode_utf16().collect();
    let sse = reply.content_type.contains("text/event-stream");
    let outcome = mcp_frame::parse_rpc_body(sse, &units, &mcp_frame::Expected::Number(id as f64))
        .map_err(|_| "MCP: response body exceeded the 8 MB limit".to_string())?;
    let span = match outcome {
        mcp_frame::Outcome::Reply(r) => r,
        mcp_frame::Outcome::Mismatch(_) => {
            return Err(format!("MCP: reply id does not match request {id}"))
        }
        mcp_frame::Outcome::NoMessage => {
            return Err("MCP: no JSON-RPC message in event stream".into())
        }
        mcp_frame::Outcome::OtherTraffic => {
            return Err(format!("MCP: no reply to request {id} in event stream (the server sent other traffic; noevia does not implement server-initiated requests)"))
        }
        mcp_frame::Outcome::InvalidJson => return Err("MCP response could not be checked".into()),
    };
    let slice = units.get(span).unwrap_or(&[]);
    let message = js_json::parse(&String::from_utf16_lossy(slice))
        .map_err(|_| "MCP response could not be checked".to_string())?;
    if message.get("error").truthy() {
        let e = message.get("error");
        let code = js_json::to_js_string(e.get("code")).unwrap_or_default();
        let text = js_json::to_js_string(e.get("message")).unwrap_or_default();
        return Err(format!("MCP error {code}: {text}"));
    }
    Ok(Some(message.get("result").clone()))
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use reqwest::dns::Resolve;

    #[test]
    fn a_hosted_address_is_https_with_no_placeholder_and_only_qa_admits_loopback() {
        assert!(hosted_url_ok("https://mcp.example.com/mcp", false));
        for bad in [
            "http://mcp.example.com/mcp",
            "https://",
            "https://{tenant}.example.com/mcp",
            "https://mcp.example.com/m cp",
            "https://mcp.example.com/mcp\n",
            "ftp://mcp.example.com/mcp",
            "http://127.0.0.1:9000/mcp",
        ] {
            assert!(!hosted_url_ok(bad, false), "{bad}");
        }
        // The QA stand-in: http://127.0.0.1:<port>/…, and only that.
        assert!(hosted_url_ok("http://127.0.0.1:9000/mcp", true));
        for bad in [
            "http://127.0.0.1/mcp",
            "http://127.0.0.1:9000",
            "http://localhost:9000/mcp",
            "http://127.0.0.2:9000/mcp",
            "http://10.0.0.1:9000/mcp",
        ] {
            assert!(!hosted_url_ok(bad, true), "{bad}");
        }
        assert!(qa_loopback_url("https://127.0.0.1:9000/x"));
        assert!(!qa_loopback_url("https://127.0.0.1/x"));
    }

    #[tokio::test]
    async fn the_connect_time_lookup_refuses_a_name_that_is_not_public() {
        // The pre-check and the connection resolve separately (DNS rebinding): the resolver the client
        // connects with refuses any answer set that holds a private address.
        let refused = PublicOnly.resolve("localhost".parse().unwrap()).await;
        assert!(refused.is_err(), "localhost resolves to loopback");
        let literal = PublicOnly.resolve("127.0.0.1".parse().unwrap()).await;
        assert!(literal.is_err());
    }

    #[tokio::test]
    async fn an_address_is_judged_before_any_request_leaves() {
        let egress = Egress::new(false).unwrap();
        for url in [
            "https://10.0.0.5/x",
            "https://192.168.1.1/x",
            "https://169.254.169.254/latest",
            "https://localhost/x",
            "https://[::1]/x",
            "https://metadata.google.internal/x",
            "http://example.com/x",
            "https://user:pw@example.com/x",
        ] {
            assert!(!egress.url_allowed(url).await, "{url}");
        }
        assert!(egress.get_json("https://10.0.0.5/x").await.is_err());
        assert_eq!(
            egress.probe_auth("https://10.0.0.5/x").await,
            (0, String::new())
        );
        let counter = std::sync::atomic::AtomicI64::new(0);
        let why = check_key(&egress, "https://10.0.0.5/mcp", &[], &counter)
            .await
            .unwrap_err();
        assert_eq!(why, "its address no longer resolves to a public host");
    }
}
