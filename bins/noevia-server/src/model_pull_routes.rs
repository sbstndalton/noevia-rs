//! Track R11 (S11): `GET /api/models/mtp-artifact` answered by the front (crates/model-pull,
//! recorded from core's routes/models.cjs, llamacpp-variants.cjs and mtp-artifact.cjs), behind the
//! model writes switch `NOEVIA_RUST_MODEL_WRITES=rust` (model_writes_routes.rs `Settings::answers`).
//! Unset, the GET goes to core exactly as before. Core, told the same switch, must refuse its own
//! GET (rust-model-writes.cjs OWNED, method GET), so a direct request on its port cannot run a
//! second check beside the front's.
//!
//! What the front does: the session gate; the variant's files (with llama.cpp, Hugging Face's
//! public file list, `https://huggingface.co/api/models/<repo>?blobs=true`, 20 s, no redirects,
//! 8 MiB; without a model manager, core's 500); then the check itself: the repository's revision
//! and each file's tensor directory, a byte prefix of at most 16 MiB
//! (`model_pull::artifact`), with one process-wide cache and running set ([`State`], locked only
//! to begin and to finish a check, never while it fetches). A request for a check that is
//! running waits for that check's answer (crates/model-pull `check_shared`), as core shares its
//! promise; if that run panics, the waiter gets core's answer to a rejection (500).
//!
//! # Named differences (fail closed; draft advisory GHSA-2pcf-gc4m-hr5f)
//!
//! - `admin-only`: core answers this GET for every signed-in user; the front answers it for
//!   administrators only. A member gets 403 `{"error":"administrator required"}`, the text core's
//!   models write prelude uses (routes/models.cjs:205), which the web client already shows for
//!   every administrator-only models route.
//! - `busy-before-variants`: core runs the variants lookup for any number of requests and limits
//!   only the checks to two at once. The front takes one of two permits before the lookup and
//!   holds it to the end. Without a free permit the answer is core's busy answer (200,
//!   `status: unknown`, "Two artifact checks are already running. Try again shortly.") with
//!   `files: []`, since the files are not looked up. That includes a request that would have
//!   joined a running check.
//! - `hub-redirects`: core's fetch follows any redirect (at most 20). The front follows at most
//!   20 too, but only to `https` URLs whose host is `huggingface.co` or `hf.co` or a subdomain of
//!   either (DNS label boundary), on the default port, without credentials ([`hub_host_allowed`]).
//!   Every hop, the first included, is looked up before it is contacted; all its addresses must be
//!   public (ssrf-policy `addresses_public`) and the request is pinned to them. Anything else
//!   fails the fetch (`fetch failed`, so the check answers `unknown`). The variants lookup follows
//!   no redirect at all, as core's `fetchJson` (`redirect: 'error'`), and is vetted the same way:
//!   one of the five statuses fetch follows fails it, another 3xx is a listing that is not ok.
//!   As fetch, only 301, 302, 303, 307 and 308 are followed ([`fetch_redirect`]); another 3xx is
//!   the hop's answer (so a non-ok status, and core's reason for it). A followed status without a
//!   Location is the answer as it is; one whose Location does not parse is `fetch failed`.
//!
//! `POST /api/models/pull` is wired in model_pull_post.rs under the same switch (core's download
//! tracker told over its `/internal/models/downloads/{requested,rejected}` bridge).
use crate::{identity, reply, App};
use axum::body::Body;
use axum::http::{Request, Response, StatusCode};
use axum::response::IntoResponse;
use js_json::{wtf16, JValue};
use model_pull::artifact::{self, Artifacts, Checked, FetchError, Fetched, Fetcher};
use model_pull::manager::{self, Reply, Target, Thrown};
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

pub const PATH: &str = "/api/models/mtp-artifact";
/// `POST /api/models/pull` (model_pull_post.rs), under the same switch.
pub const PULL: &str = "/api/models/pull";
/// The public hub every URL of the check and of llama.cpp's variants names.
const PUBLIC_HUB: &str = "https://huggingface.co";
/// fetchJson's answer cap (http.cjs `DEFAULT_MAX_RESPONSE_BYTES`).
const ANSWER_CAP: usize = 8 * 1024 * 1024;
const MAX_REDIRECTS: usize = 20;

/// The hub host rule: `https`, no credentials, the default port, and a host of `huggingface.co`
/// or `hf.co` or a subdomain of either (`cdn-lfs.huggingface.co`, `cas-bridge.xethub.hf.co`).
/// An IP address, another port or scheme, or a look-alike (`huggingface.co.evil.example`,
/// `evilhf.co`) is refused.
pub fn hub_host_allowed(url: &reqwest::Url) -> bool {
    if url.scheme() != "https" || !url.username().is_empty() || url.password().is_some() {
        return false;
    }
    if url.port().is_some() {
        return false;
    }
    let Some(host) = url.domain() else {
        return false;
    };
    let host = host.to_ascii_lowercase();
    ["huggingface.co", "hf.co"]
        .iter()
        .any(|zone| host == *zone || host.ends_with(&format!(".{zone}")))
}

/// fetch's redirect statuses (`redirectStatusSet`): only these are followed. Another 3xx (300,
/// 304, 305, ...) is a final answer, as any other status.
pub fn fetch_redirect(status: u16) -> bool {
    matches!(status, 301 | 302 | 303 | 307 | 308)
}

/// A Location header's text as fetch reads it: printable ASCII as it is; otherwise its bytes read
/// as UTF-8 (undici's `normalizeBinaryStringToUtf8` over the header's binary string; a byte that
/// is not UTF-8 becomes U+FFFD), which the URL parser then percent-encodes.
pub fn location_text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

/// Where the public hub is: `https://huggingface.co` in production. Tests point it at a local
/// stub with [`Hub::for_tests`]; no environment variable can.
#[derive(Clone, Debug)]
pub struct Hub {
    origin: String,
    test: bool,
}

impl Default for Hub {
    fn default() -> Hub {
        Hub {
            origin: PUBLIC_HUB.into(),
            test: false,
        }
    }
}

impl Hub {
    /// A scripted hub at `origin` (`http://127.0.0.1:<port>`): its URLs stand for
    /// `https://huggingface.co`, and its own origin is allowed besides the hub rule.
    #[doc(hidden)]
    pub fn for_tests(origin: &str) -> Hub {
        Hub {
            origin: origin.trim_end_matches('/').to_string(),
            test: true,
        }
    }

    fn rewrite(&self, url: &str) -> String {
        match url.strip_prefix(PUBLIC_HUB) {
            Some(rest) if rest.is_empty() || rest.starts_with('/') => {
                format!("{}{rest}", self.origin)
            }
            _ => url.to_string(),
        }
    }

    /// The test hub's own origin (loopback in tests): reached without a lookup. Never true in
    /// production.
    fn is_test_origin(&self, url: &reqwest::Url) -> bool {
        self.test
            && url.username().is_empty()
            && url.password().is_none()
            && reqwest::Url::parse(&self.origin).is_ok_and(|o| o.origin() == url.origin())
    }

    #[cfg(test)]
    fn allows(&self, url: &reqwest::Url) -> bool {
        hub_host_allowed(url) || self.is_test_origin(url)
    }
}

/// The process-wide state of the check: core's module state (cache, running checks and the
/// answers their joiners wait for) and the two permits.
pub struct State {
    hub: Hub,
    artifacts: Mutex<Artifacts>,
    permits: Arc<tokio::sync::Semaphore>,
    plain_client: Option<reqwest::Client>,
    scan_clears: AtomicU64,
}

impl std::fmt::Debug for State {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("State")
            .field("hub", &self.hub)
            .finish_non_exhaustive()
    }
}

/// Why a hub request did not get an answer.
enum HopError {
    /// Refused (outside the hub rule, a private or unresolved address, too many redirects) or
    /// failed: undici's `fetch failed`.
    Failed,
    Timeout,
}

/// One hop's target, vetted before it is contacted: `Ok(None)` for the test hub's own origin
/// (no lookup), `Ok(Some(addresses))` for a hub host whose looked-up addresses are all public
/// (ssrf-policy; the request is pinned to them), `Err` otherwise.
pub(crate) async fn vet_hop<F, Fut>(
    hub: &Hub,
    url: &reqwest::Url,
    lookup: F,
) -> Result<Option<Vec<SocketAddr>>, ()>
where
    F: FnOnce(String, u16) -> Fut,
    Fut: std::future::Future<Output = std::io::Result<Vec<SocketAddr>>>,
{
    if hub.is_test_origin(url) {
        return Ok(None);
    }
    if !hub_host_allowed(url) {
        return Err(());
    }
    let host = url.host_str().ok_or(())?.to_string();
    let found = lookup(host, 443).await.map_err(|_| ())?;
    let texts: Vec<String> = found.iter().map(|a| a.ip().to_string()).collect();
    if found.is_empty() || !ssrf_policy::addresses_public(&texts) {
        return Err(());
    }
    Ok(Some(found))
}

async fn system_lookup(host: String, port: u16) -> std::io::Result<Vec<SocketAddr>> {
    Ok(tokio::net::lookup_host((host.as_str(), port))
        .await?
        .collect())
}

impl State {
    pub fn new(hub: Hub) -> State {
        let plain_client = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .ok();
        State {
            hub,
            artifacts: Mutex::new(Artifacts::new()),
            permits: Arc::new(tokio::sync::Semaphore::new(artifact::RUNNING)),
            plain_client,
            scan_clears: AtomicU64::new(0),
        }
    }

    /// How often the models scan was cleared from here: a GET never clears it (tests assert 0).
    #[doc(hidden)]
    pub fn scan_clears(&self) -> u64 {
        self.scan_clears.load(Ordering::SeqCst)
    }

    /// A GET to the public hub, following at most `redirects` redirects (fetch's 20; fetchJson's
    /// none, where a redirect is returned for the caller to refuse). Every hop, the first and each
    /// redirect target, is vetted ([`vet_hop`]) and pinned to its checked addresses; a refused
    /// hop is never contacted. No credentials, no cookies, no maintenance header: only `range`.
    async fn hub_get(
        &self,
        url: reqwest::Url,
        range: Option<&'static str>,
        redirects: usize,
    ) -> Result<reqwest::Response, HopError> {
        let mut url = url;
        let mut followed = 0;
        loop {
            let pinned = vet_hop(&self.hub, &url, system_lookup)
                .await
                .map_err(|()| HopError::Failed)?;
            let mut builder = reqwest::Client::builder()
                .no_proxy()
                .redirect(reqwest::redirect::Policy::none());
            if let (Some(found), Some(host)) = (pinned, url.host_str()) {
                builder = builder.resolve_to_addrs(host, &found);
            }
            let client = builder.build().map_err(|_| HopError::Failed)?;
            let mut req = client.get(url.clone());
            if let Some(range) = range {
                req = req.header(reqwest::header::RANGE, range);
            }
            let res = req.send().await.map_err(|e| {
                if e.is_timeout() {
                    HopError::Timeout
                } else {
                    HopError::Failed
                }
            })?;
            if redirects == 0 || !fetch_redirect(res.status().as_u16()) {
                return Ok(res);
            }
            // fetch's `responseLocationURL`: no Location, and the redirect is the answer as it is;
            // one that does not parse against this hop's URL is a network error (`fetch failed`).
            let Some(location) = res.headers().get(reqwest::header::LOCATION) else {
                return Ok(res);
            };
            let next = url
                .join(&location_text(location.as_bytes()))
                .map_err(|_| HopError::Failed)?;
            if followed >= redirects {
                return Err(HopError::Failed);
            }
            followed += 1;
            url = next;
        }
    }

    /// One fetchJson-style request: the manager's, or (PublicHub) the hub's file list.
    async fn send(&self, r: &manager::Request) -> Result<Reply, Thrown> {
        let deadline = Duration::from_millis(u64::from(r.timeout_ms));
        match tokio::time::timeout(deadline, self.send_within(r)).await {
            Ok(answer) => answer,
            Err(_) => Err(Thrown::internal("fetch failed")),
        }
    }

    async fn send_within(&self, r: &manager::Request) -> Result<Reply, Thrown> {
        let failed = || Thrown::internal("fetch failed");
        let mut res = match r.target {
            // No headers by construction, no redirect (fetchJson's `redirect: 'error'`).
            Target::PublicHub => {
                let url = reqwest::Url::parse(&self.hub.rewrite(&r.url)).map_err(|_| failed())?;
                self.hub_get(url, None, 0).await.map_err(|_| failed())?
            }
            Target::Manager => {
                let url = reqwest::Url::parse(&r.url).map_err(|_| failed())?;
                let client = self.plain_client.as_ref().ok_or_else(failed)?;
                let method =
                    reqwest::Method::from_bytes(r.method.as_bytes()).map_err(|_| failed())?;
                let mut req = client.request(method, url);
                for (k, v) in &r.headers {
                    req = req.header(k.as_str(), v.as_str());
                }
                if let Some(body) = &r.body {
                    req = req.body(body.clone());
                }
                req.send().await.map_err(|_| failed())?
            }
        };
        // fetch with `redirect: 'error'` throws on a redirect status (301, 302, 303, 307, 308);
        // another 3xx (a 300, a 304) is an ordinary answer that is not ok (#2661).
        if fetch_redirect(res.status().as_u16()) {
            return Err(failed());
        }
        let (status, ok) = (res.status().as_u16(), res.status().is_success());
        let mut buf = Vec::new();
        while let Some(chunk) = res.chunk().await.map_err(|_| failed())? {
            buf.extend_from_slice(&chunk);
            if buf.len() > ANSWER_CAP {
                return Ok(Reply {
                    ok: false,
                    status,
                    body: JValue::Undefined,
                });
            }
        }
        let text = String::from_utf8_lossy(&buf).into_owned();
        let body = wtf16::parse(&text).unwrap_or(JValue::Str(text));
        Ok(Reply { ok, status, body })
    }

    /// One public hub fetch of the check (fetch's 20 redirects, each hop vetted and pinned), the
    /// deadline over the hops and the body read.
    async fn fetch(&self, f: &artifact::Fetch) -> Result<Fetched, FetchError> {
        let deadline = tokio::time::Instant::now() + Duration::from_millis(u64::from(f.timeout_ms));
        let failed = || FetchError::Failed("fetch failed".into());
        let url = reqwest::Url::parse(&self.hub.rewrite(&f.url)).map_err(|_| failed())?;
        let mut res = match tokio::time::timeout_at(
            deadline,
            self.hub_get(url, f.range, MAX_REDIRECTS),
        )
        .await
        {
            Err(_) | Ok(Err(HopError::Timeout)) => return Err(FetchError::Timeout),
            Ok(Err(HopError::Failed)) => return Err(failed()),
            Ok(Ok(res)) => res,
        };
        let status = res.status().as_u16();
        let content_range = res
            .headers()
            .get(reqwest::header::CONTENT_RANGE)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);
        let mut buf = Vec::new();
        let body = loop {
            match tokio::time::timeout_at(deadline, res.chunk()).await {
                Ok(Ok(Some(chunk))) => {
                    buf.extend_from_slice(&chunk);
                    if buf.len() >= f.max_bytes {
                        buf.truncate(f.max_bytes);
                        break Ok(buf);
                    }
                }
                Ok(Ok(None)) => break Ok(buf),
                Err(_) => break Err(FetchError::Timeout),
                Ok(Err(e)) if e.is_timeout() => break Err(FetchError::Timeout),
                Ok(Err(_)) => break Err(FetchError::Failed("terminated".into())),
            }
        };
        Ok(Fetched {
            status,
            content_range,
            body,
        })
    }
}

/// The crate's host: each HTTP step run to its end on the runtime (inside `spawn_blocking`).
struct FrontHub {
    state: Arc<State>,
    rt: tokio::runtime::Handle,
}

impl Fetcher for FrontHub {
    fn fetch(&mut self, request: &artifact::Fetch) -> Result<Fetched, FetchError> {
        let state = Arc::clone(&self.state);
        self.rt.block_on(state.fetch(request))
    }
    fn now_ms(&self) -> i64 {
        identity::now_ms()
    }
}

impl model_pull::Host for FrontHub {
    // A GET reaches neither the write prelude nor the scan clear; these are counted, not acted on.
    fn invalidate_model_reads(&mut self) {}
    fn clear_model_scan(&mut self) -> Result<(), model_pull::Answered> {
        self.state.scan_clears.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
    fn send(&mut self, request: &manager::Request) -> Result<Reply, Thrown> {
        let state = Arc::clone(&self.state);
        self.rt.block_on(state.send(request))
    }
    // The pull's hooks: the pull stays core's (module documentation).
    fn enter(&mut self) -> Result<(), Thrown> {
        Err(Thrown::internal("the pull is core's"))
    }
    fn leave(&mut self) {}
    fn downloads_requested(&mut self, _: &JValue) -> Result<(), model_pull::Answered> {
        Ok(())
    }
    fn downloads_rejected(&mut self, _: &JValue) {}
}

fn answer(status: u16, body: &JValue) -> Response<Body> {
    reply::json_text(
        StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
        wtf16::stringify(body).unwrap_or_default(),
        true,
    )
}

fn internal() -> (u16, JValue) {
    (
        500,
        JValue::obj([("error", JValue::from("Internal error"))]),
    )
}

fn busy(files: JValue) -> JValue {
    JValue::obj([
        ("status", JValue::from("unknown")),
        ("reason", JValue::from(artifact::BUSY)),
        ("files", files),
        (
            "checkedAt",
            JValue::from(model_pull::js::iso_string(identity::now_ms())),
        ),
    ])
}

fn manager_config(kind: server_stats::Kind) -> manager::Config {
    manager::Config {
        kind: match kind {
            server_stats::Kind::None => manager::Kind::None,
            server_stats::Kind::Lemonade => manager::Kind::Lemonade,
            server_stats::Kind::LlamaCpp => manager::Kind::Llamacpp,
        },
        // Only a Lemonade manager is asked for the variants, and the front never runs with one
        // (config.rs); llama.cpp's go to the public hub.
        base_url: String::new(),
        api_key: None,
    }
}

/// The variants lookup and the check, on a blocking thread (a joiner waits here for the running
/// check's answer).
fn check(
    state: Arc<State>,
    rt: tokio::runtime::Handle,
    config: &manager::Config,
    query: &str,
) -> (u16, JValue) {
    let mut host = FrontHub {
        state: Arc::clone(&state),
        rt,
    };
    let (repo, files) = match model_pull::mtp_selection(config, query, &mut host) {
        Ok(selected) => selected,
        Err(model_pull::Outcome::Answer { status, body }) => return (status, body),
        Err(_) => return internal(),
    };
    match artifact::check_shared(&state.artifacts, &repo, &files, &mut host) {
        Checked::Answer(body) => (200, body),
        // The joined run panicked or did not end in time: core's answer to a rejected promise.
        Checked::Failed(_) => internal(),
    }
}

pub async fn serve(
    app: &Arc<App>,
    _conn: crate::serve::Conn,
    req: Request<Body>,
    path: String,
) -> Response<Body> {
    let (parts, _body) = req.into_parts();
    let creds = identity::creds_of(&parts);
    let layer = Arc::clone(&app.identity);
    let now = identity::now_ms();
    let id =
        match tokio::task::spawn_blocking(move || layer.gate_blocking(&creds, &path, now)).await {
            Ok(Ok(id)) => id,
            Ok(Err(rejection)) => return rejection.into_response(),
            Err(_) => return identity::Rejection::Unavailable.into_response(),
        };
    // Named difference admin-only.
    if id.user.role != server_auth::Role::Admin {
        return answer(
            403,
            &JValue::obj([("error", JValue::from("administrator required"))]),
        );
    }
    let state = Arc::clone(&app.mtp_artifact);
    // Named difference busy-before-variants.
    let Ok(permit) = Arc::clone(&state.permits).try_acquire_owned() else {
        return answer(200, &busy(JValue::Arr(Vec::new())));
    };
    let query = parts.uri.query().unwrap_or("").to_string();
    let config = manager_config(app.config.maint.kind);
    let rt = tokio::runtime::Handle::current();
    let work = tokio::task::spawn_blocking(move || {
        let _permit = permit;
        check(state, rt, &config, &query)
    })
    .await;
    match work {
        Ok((status, body)) => answer(status, &body),
        // A panic in the check (its run abandoned as it unwinds): core's answer to a throw.
        Err(_) => {
            let (status, body) = internal();
            answer(status, &body)
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn allowed(url: &str) -> bool {
        hub_host_allowed(&reqwest::Url::parse(url).unwrap())
    }

    #[test]
    fn the_hub_rule_is_exact() {
        for ok in [
            "https://huggingface.co/api/models/qa/x",
            "https://HuggingFace.co/qa",
            "https://cdn-lfs.huggingface.co/x",
            "https://cdn-lfs-us-1.hf.co/x",
            "https://cas-bridge.xethub.hf.co/x?X-Amz=1",
            "https://hf.co/x",
            "https://huggingface.co:443/x",
        ] {
            assert!(allowed(ok), "{ok}");
        }
        for refused in [
            "http://huggingface.co/x",
            "https://huggingface.co:8443/x",
            "https://user:pw@huggingface.co/x",
            "https://huggingface.co.evil.example/x",
            "https://evilhuggingface.co/x",
            "https://evilhf.co/x",
            "https://hf.com/x",
            "https://13.33.0.1/x",
            "https://[::1]/x",
            "https://localhost/x",
            "ftp://huggingface.co/x",
        ] {
            assert!(!allowed(refused), "{refused}");
        }
    }

    #[test]
    fn only_the_hub_prefix_is_rewritten_for_tests() {
        let hub = Hub::for_tests("http://127.0.0.1:9");
        assert_eq!(
            hub.rewrite("https://huggingface.co/api/x"),
            "http://127.0.0.1:9/api/x"
        );
        assert_eq!(
            hub.rewrite("https://huggingface.co.evil.example/x"),
            "https://huggingface.co.evil.example/x"
        );
        assert_eq!(
            Hub::default().rewrite("https://huggingface.co/api/x"),
            "https://huggingface.co/api/x"
        );
        let local = reqwest::Url::parse("http://127.0.0.1:9/x").unwrap();
        assert!(hub.allows(&local));
        assert!(!Hub::default().allows(&local));
        assert!(!hub.allows(&reqwest::Url::parse("http://127.0.0.1:10/x").unwrap()));
    }

    async fn vet(
        hub: &Hub,
        url: &str,
        answer: Result<Vec<&str>, ()>,
    ) -> (Result<Option<Vec<SocketAddr>>, ()>, bool) {
        let asked = std::sync::atomic::AtomicBool::new(false);
        let url = reqwest::Url::parse(url).unwrap();
        let out = vet_hop(hub, &url, |host, port| {
            asked.store(true, Ordering::SeqCst);
            assert_eq!(port, 443);
            assert!(hub_host_allowed(
                &reqwest::Url::parse(&format!("https://{host}/")).unwrap()
            ));
            let answer = answer
                .map(|ips| {
                    ips.iter()
                        .map(|ip| SocketAddr::new(ip.parse().unwrap(), 443))
                        .collect()
                })
                .map_err(|()| std::io::Error::other("qa: no answer"));
            async move { answer }
        })
        .await;
        (out, asked.load(Ordering::SeqCst))
    }

    #[tokio::test]
    async fn every_hop_is_looked_up_and_must_be_public() {
        let prod = Hub::default();
        let ok = vet(
            &prod,
            "https://cdn-lfs.hf.co/x",
            Ok(vec!["18.239.1.2", "2600:9000::1"]),
        )
        .await;
        assert_eq!(
            ok.0.unwrap().unwrap().len(),
            2,
            "pinned to the checked addresses"
        );
        for private in [
            vec!["10.0.0.5"],
            vec!["18.239.1.2", "127.0.0.1"],
            vec!["169.254.169.254"],
            vec!["::1"],
            vec!["fd00::1"],
            vec![],
        ] {
            let (out, asked) = vet(&prod, "https://huggingface.co/x", Ok(private.clone())).await;
            assert!(out.is_err() && asked, "{private:?}");
        }
        assert!(vet(&prod, "https://huggingface.co/x", Err(()))
            .await
            .0
            .is_err());
        // Outside the hub rule: refused without a lookup.
        let (out, asked) = vet(&prod, "https://evil.example/x", Ok(vec!["18.239.1.2"])).await;
        assert!(out.is_err() && !asked);
        let (out, asked) = vet(&prod, "http://127.0.0.1:9/x", Ok(vec!["127.0.0.1"])).await;
        assert!(out.is_err() && !asked);
        // The test hub's own origin only: no lookup, no pin.
        let test = Hub::for_tests("http://127.0.0.1:9");
        let (out, asked) = vet(&test, "http://127.0.0.1:9/x", Err(())).await;
        assert_eq!((out, asked), (Ok(None), false));
        let (out, asked) = vet(&test, "http://127.0.0.1:10/x", Ok(vec!["127.0.0.1"])).await;
        assert!(out.is_err() && !asked);
    }
}
