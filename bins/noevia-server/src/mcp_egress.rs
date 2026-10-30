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
/// One whole listing (connect, every page, close) may take this long. Each request has its own
/// deadline, but a server that answers each page a byte at a time could otherwise hold one for
/// twenty-two requests of thirty seconds (sbstndalton/noevia#2202 round 2 review, M2).
const LIST_DEADLINE: Duration = Duration::from_secs(60);
/// How many listings this front runs at once, for every member together: a listing may hold up to
/// [`LIST_MAX_BYTES`] of definitions, so the number in flight is what bounds the memory (M1).
const LIST_PERMITS: usize = 4;
/// How long a listing waits for one of those places before it says the front is busy.
const LIST_QUEUE_WAIT: Duration = Duration::from_secs(20);
/// What a listing that found no free place says (a person can try again in a moment).
pub const BUSY: &str = "noevia is checking other servers right now; try again in a moment";

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
    /// The places for listings that run at once ([`LIST_PERMITS`]).
    listings: tokio::sync::Semaphore,
    /// [`LIST_DEADLINE`] and [`LIST_QUEUE_WAIT`] (shortened by the unit tests).
    list_deadline: Duration,
    list_queue_wait: Duration,
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
            listings: tokio::sync::Semaphore::new(LIST_PERMITS),
            list_deadline: LIST_DEADLINE,
            list_queue_wait: LIST_QUEUE_WAIT,
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
    // Counted, not kept: a listing can be 16 MiB of definitions and a check only asks whether it works.
    list(egress, url, headers, next_id, false)
        .await
        .map(|listing| listing.count)
}

/// What a listing found: how many tools, and the tools themselves when they were asked for.
pub struct Listing {
    pub count: usize,
    pub tools: Vec<JValue>,
}

/// core mcp.cjs `listTools` budget: the bytes of tool definitions one listing may gather.
const LIST_MAX_BYTES: usize = 16 * 1024 * 1024;

/// The tools an MCP server offers, as it sent them (core `connect` + `listTools` + `disconnect`):
/// `initialize`, the `initialized` notification, `tools/list` following `nextCursor` (at most
/// [`MAX_PAGES`] pages and [`LIST_MAX_BYTES`] of definitions, keeping what was gathered), then the
/// session is closed whatever happened. `headers` are the server's key headers (never the reserved
/// ones). The failure is in core's words (`MCP 401: ...`, `MCP error -32000: ...`, `fetch failed`,
/// `its address no longer resolves to a public host`). Every request is checked against the public
/// address rules first, and again at connect time by the client's own resolver.
pub async fn list_tools(
    egress: &Egress,
    url: &str,
    headers: &[(String, String)],
    next_id: &std::sync::atomic::AtomicI64,
) -> Result<Vec<JValue>, String> {
    list(egress, url, headers, next_id, true)
        .await
        .map(|listing| listing.tools)
}

/// [`list_tools`] with the choice of keeping the tools. At most [`LIST_PERMITS`] listings run at once
/// in this process (one that finds no place for [`LIST_QUEUE_WAIT`] answers [`BUSY`]), and one listing
/// is over after [`LIST_DEADLINE`] whatever its pages are doing; the session is closed either way.
async fn list(
    egress: &Egress,
    url: &str,
    headers: &[(String, String)],
    next_id: &std::sync::atomic::AtomicI64,
    retain: bool,
) -> Result<Listing, String> {
    if !egress.url_allowed(url).await {
        return Err("its address no longer resolves to a public host".into());
    }
    let _place = match tokio::time::timeout(egress.list_queue_wait, egress.listings.acquire()).await
    {
        Ok(Ok(place)) => place,
        _ => return Err(BUSY.into()),
    };
    let mut session: Option<String> = None;
    let outcome = tokio::time::timeout(egress.list_deadline, async {
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
        let mut all: Vec<JValue> = Vec::new();
        let mut count = 0usize;
        let mut bytes = 0usize;
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
                for tool in tools {
                    let size = js_json::stringify(tool).map_or(0, |t| t.len());
                    if bytes + size > LIST_MAX_BYTES {
                        return Ok(Listing { count, tools: all });
                    }
                    bytes += size;
                    count += 1;
                    if retain {
                        all.push(tool.clone());
                    }
                }
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
        Ok(Listing { count, tools: all })
    })
    .await
    .unwrap_or_else(|_| Err("MCP: listing the tools took too long".to_string()));
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
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
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

    // ── the listing's limits (sbstndalton/noevia#2202 round 2 review, M1 and M2) ──

    use std::sync::atomic::{AtomicI64, AtomicUsize, Ordering};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// What the synthetic server does with a `tools/list`.
    enum Out {
        /// A `result` object, answered at once.
        Result(String),
        /// A result held for this long first.
        Held(Duration, String),
        /// Headers promising a long body, then one byte every 20 ms for as long as the client listens.
        Drip,
    }

    #[derive(Default)]
    struct Stats {
        deleted: AtomicUsize,
        lists: AtomicUsize,
        now: AtomicUsize,
        most: AtomicUsize,
    }

    /// A synthetic MCP server on loopback (never anything else). `act(n)` answers the n-th `tools/list` (from 1).
    async fn serve(act: impl Fn(usize) -> Out + Send + Sync + 'static) -> (String, Arc<Stats>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/mcp", listener.local_addr().unwrap());
        let stats = Arc::new(Stats::default());
        let act = Arc::new(act);
        let seen = Arc::clone(&stats);
        tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    return;
                };
                let (act, stats) = (Arc::clone(&act), Arc::clone(&seen));
                tokio::spawn(async move {
                    let mut buf = Vec::new();
                    let mut chunk = [0u8; 8192];
                    let head_end = loop {
                        let Ok(n) = socket.read(&mut chunk).await else {
                            return;
                        };
                        if n == 0 {
                            return;
                        }
                        buf.extend_from_slice(&chunk[..n]);
                        if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                            break i + 4;
                        }
                    };
                    let head = String::from_utf8_lossy(&buf[..head_end]).to_ascii_lowercase();
                    let len = head
                        .lines()
                        .find_map(|l| l.strip_prefix("content-length:")?.trim().parse().ok())
                        .unwrap_or(0usize);
                    while buf.len() < head_end + len {
                        let Ok(n) = socket.read(&mut chunk).await else {
                            return;
                        };
                        if n == 0 {
                            break;
                        }
                        buf.extend_from_slice(&chunk[..n]);
                    }
                    let body = String::from_utf8_lossy(&buf[head_end..]).to_string();
                    let id = body
                        .split("\"id\":")
                        .nth(1)
                        .map(|r| {
                            r.chars()
                                .take_while(char::is_ascii_digit)
                                .collect::<String>()
                        })
                        .unwrap_or_default();
                    let json = |extra: &str, result: &str| {
                        let message = format!(r#"{{"jsonrpc":"2.0","id":{id},"result":{result}}}"#);
                        format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n{extra}Content-Length: {}\r\nConnection: close\r\n\r\n{message}", message.len())
                    };
                    let out = if head.starts_with("delete ") {
                        stats.deleted.fetch_add(1, Ordering::SeqCst);
                        "HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                            .to_string()
                    } else if body.contains("\"method\":\"initialize\"") {
                        json("mcp-session-id: s1\r\n", "{}")
                    } else if body.contains("\"method\":\"tools/list\"") {
                        let n = stats.lists.fetch_add(1, Ordering::SeqCst) + 1;
                        let running = stats.now.fetch_add(1, Ordering::SeqCst) + 1;
                        stats.most.fetch_max(running, Ordering::SeqCst);
                        let out = match act(n) {
                            Out::Result(r) => json("", &r),
                            Out::Held(d, r) => {
                                tokio::time::sleep(d).await;
                                json("", &r)
                            }
                            Out::Drip => {
                                let _ = socket
                                    .write_all(b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 100000000\r\n\r\n")
                                    .await;
                                while socket.write_all(b" ").await.is_ok() {
                                    tokio::time::sleep(Duration::from_millis(20)).await;
                                }
                                stats.now.fetch_sub(1, Ordering::SeqCst);
                                return;
                            }
                        };
                        stats.now.fetch_sub(1, Ordering::SeqCst);
                        out
                    } else {
                        "HTTP/1.1 202 Accepted\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                            .to_string()
                    };
                    let _ = socket.write_all(out.as_bytes()).await;
                    let _ = socket.shutdown().await;
                });
            }
        });
        (url, stats)
    }

    fn egress_with(deadline: Duration, queue_wait: Duration) -> Egress {
        Egress {
            client: reqwest::Client::builder()
                .no_proxy()
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .unwrap(),
            qa_loopback: true,
            listings: tokio::sync::Semaphore::new(LIST_PERMITS),
            list_deadline: deadline,
            list_queue_wait: queue_wait,
        }
    }

    fn tools_of(n: usize, kib: usize) -> String {
        let tools: Vec<String> = (0..n)
            .map(|i| {
                format!(
                    r#"{{"name":"t{i}","description":"{}"}}"#,
                    "a".repeat(kib * 1024)
                )
            })
            .collect();
        format!(r#"{{"tools":[{}]}}"#, tools.join(","))
    }

    #[tokio::test]
    async fn a_listing_is_over_at_its_deadline_however_slowly_the_server_drips_and_its_session_is_closed(
    ) {
        let (url, stats) = serve(|_| Out::Drip).await;
        let egress = egress_with(Duration::from_millis(800), Duration::from_secs(5));
        let counter = AtomicI64::new(0);
        let started = std::time::Instant::now();
        let why = tokio::time::timeout(
            Duration::from_secs(10),
            list_tools(&egress, &url, &[], &counter),
        )
        .await
        .expect("the listing is cut off by its own deadline")
        .unwrap_err();
        assert!(
            started.elapsed() < Duration::from_secs(6),
            "{:?}",
            started.elapsed()
        );
        assert_eq!(why, "MCP: listing the tools took too long");
        for _ in 0..50 {
            if stats.deleted.load(Ordering::SeqCst) == 1 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        assert_eq!(
            stats.deleted.load(Ordering::SeqCst),
            1,
            "the session is closed after the deadline"
        );
        // The place was given back: another listing runs.
        let (url2, _) = serve(|_| Out::Result(tools_of(1, 0))).await;
        assert_eq!(check_key(&egress, &url2, &[], &counter).await, Ok(1));
    }

    #[tokio::test]
    async fn a_reply_over_8_mib_fails_and_a_listing_stops_at_16_mib_of_definitions_keeping_what_fit(
    ) {
        let (url, _) = serve(|_| Out::Result(tools_of(9, 1024))).await;
        let egress = egress_with(Duration::from_secs(60), Duration::from_secs(5));
        let counter = AtomicI64::new(0);
        assert_eq!(
            list_tools(&egress, &url, &[], &counter).await.unwrap_err(),
            "MCP: response body exceeded the 8 MB limit"
        );
        // Three pages of six tools of about 1 MiB (each reply under 8 MiB): the 16 MiB budget ends the third.
        let (url, stats) = serve(|_| {
            Out::Result(
                tools_of(6, 1024).trim_end_matches('}').to_string() + r#","nextCursor":"more"}"#,
            )
        })
        .await;
        let kept = list(&egress, &url, &[], &counter, true).await.unwrap();
        assert_eq!(
            (kept.count, kept.tools.len()),
            (15, 15),
            "the 16th definition does not fit"
        );
        assert_eq!(
            stats.lists.load(Ordering::SeqCst),
            3,
            "no page after the budget"
        );
        // A key check counts the same tools without keeping them.
        let counted = list(&egress, &url, &[], &counter, false).await.unwrap();
        assert_eq!((counted.count, counted.tools.len()), (15, 0));
        assert_eq!(check_key(&egress, &url, &[], &counter).await, Ok(15));
    }

    #[tokio::test]
    async fn only_four_listings_run_at_once_and_one_that_finds_no_place_says_so_then_recovers() {
        let (url, stats) = serve(|_| Out::Held(Duration::from_millis(300), tools_of(1, 0))).await;
        let egress = Arc::new(egress_with(
            Duration::from_secs(60),
            Duration::from_secs(30),
        ));
        let counter = Arc::new(AtomicI64::new(0));
        let mut all = Vec::new();
        for _ in 0..10 {
            let (e, u, c) = (Arc::clone(&egress), url.clone(), Arc::clone(&counter));
            all.push(tokio::spawn(
                async move { check_key(&e, &u, &[], &c).await },
            ));
        }
        for one in all {
            assert_eq!(one.await.unwrap(), Ok(1));
        }
        assert_eq!(stats.lists.load(Ordering::SeqCst), 10);
        let most = stats.most.load(Ordering::SeqCst);
        assert!((2..=LIST_PERMITS).contains(&most), "{most} at once");
        // Every place taken for longer than a listing will wait: the answer is BUSY, quickly.
        let busy = egress_with(Duration::from_secs(60), Duration::from_millis(100));
        let held = busy
            .listings
            .acquire_many(LIST_PERMITS as u32)
            .await
            .unwrap();
        let started = std::time::Instant::now();
        assert_eq!(
            check_key(&busy, &url, &[], &counter).await,
            Err(BUSY.to_string())
        );
        assert!(started.elapsed() < Duration::from_secs(3));
        drop(held);
        assert_eq!(check_key(&busy, &url, &[], &counter).await, Ok(1));
    }
}
