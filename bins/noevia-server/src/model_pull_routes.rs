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
//! - `hub-redirects`: core's fetch follows any redirect. The front follows at most 20, and only
//!   to `https` URLs whose host is `huggingface.co` or `hf.co` or a subdomain of either (DNS label
//!   boundary), on the default port, without credentials ([`hub_host_allowed`]). Any other
//!   redirect fails the fetch (`fetch failed`, so the check answers `unknown`). The variants
//!   lookup follows no redirect at all, as core's `fetchJson` (`redirect: 'error'`).
//!
//! # Not here: `POST /api/models/pull` (next step)
//!
//! The pull stays core's under the switch. With llama.cpp it enters core's maintenance gate, tells
//! core's download tracker (`llamacpp-downloads.cjs`: the state file and the router's
//! `/models/sse` stream it follows for `download_finished` / `download_failed`), and posts to the
//! router. The tracker's state is what `GET /api/models/downloads` reads, so a front-answered pull
//! needs a core bridge:
//! `POST /internal/models/downloads/{requested,rejected}` (the front's project store token, the
//! checkpoint as JSON), which core applies to its tracker (`requested` also connects the event
//! stream). Plus the front's own maintenance admission in place of core's `enter()`. The crate
//! already has the request shape and host hooks (`Host::enter`, `downloads_requested`,
//! `downloads_rejected`); the bridge and its recorded replay are the next step.
use crate::{identity, reply, App};
use axum::body::Body;
use axum::http::{Request, Response, StatusCode};
use axum::response::IntoResponse;
use js_json::{wtf16, JValue};
use model_pull::artifact::{self, Artifacts, Checked, FetchError, Fetched, Fetcher};
use model_pull::manager::{self, Reply, Target, Thrown};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

pub const PATH: &str = "/api/models/mtp-artifact";
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

    fn allows(&self, url: &reqwest::Url) -> bool {
        if hub_host_allowed(url) {
            return true;
        }
        self.test
            && reqwest::Url::parse(&self.origin)
                .is_ok_and(|o| o.origin() == url.origin() && url.username().is_empty())
    }
}

/// The process-wide state of the check: core's module state (cache, running checks and the
/// answers their joiners wait for) and the two permits.
pub struct State {
    hub: Hub,
    artifacts: Mutex<Artifacts>,
    permits: Arc<tokio::sync::Semaphore>,
    hub_client: Option<reqwest::Client>,
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

impl State {
    pub fn new(hub: Hub) -> State {
        let rule = hub.clone();
        let hub_client = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::custom(move |attempt| {
                if attempt.previous().len() >= MAX_REDIRECTS {
                    attempt.error("too many redirects")
                } else if rule.allows(attempt.url()) {
                    attempt.follow()
                } else {
                    attempt.error("a redirect outside the public hub")
                }
            }))
            .build()
            .ok();
        let plain_client = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .ok();
        State {
            hub,
            artifacts: Mutex::new(Artifacts::new()),
            permits: Arc::new(tokio::sync::Semaphore::new(artifact::RUNNING)),
            hub_client,
            plain_client,
            scan_clears: AtomicU64::new(0),
        }
    }

    /// How often the models scan was cleared from here: a GET never clears it (tests assert 0).
    #[doc(hidden)]
    pub fn scan_clears(&self) -> u64 {
        self.scan_clears.load(Ordering::SeqCst)
    }

    /// One fetchJson-style request: the manager's, or (PublicHub) the hub's file list.
    async fn send(&self, r: &manager::Request) -> Result<Reply, Thrown> {
        let failed = || Thrown::internal("fetch failed");
        let url = match r.target {
            Target::PublicHub => self.hub.rewrite(&r.url),
            Target::Manager => r.url.clone(),
        };
        let url = reqwest::Url::parse(&url).map_err(|_| failed())?;
        if r.target == Target::PublicHub && !self.hub.allows(&url) {
            return Err(failed());
        }
        let client = self.plain_client.as_ref().ok_or_else(failed)?;
        let method = reqwest::Method::from_bytes(r.method.as_bytes()).map_err(|_| failed())?;
        let mut req = client
            .request(method, url)
            .timeout(Duration::from_millis(u64::from(r.timeout_ms)));
        // A PublicHub request carries no headers by construction; nothing is added to it here.
        if r.target == Target::Manager {
            for (k, v) in &r.headers {
                req = req.header(k.as_str(), v.as_str());
            }
        }
        if let Some(body) = &r.body {
            req = req.body(body.clone());
        }
        let mut res = req.send().await.map_err(|_| failed())?;
        // fetch with `redirect: 'error'` throws on a redirect.
        if res.status().is_redirection() {
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

    /// One public hub fetch of the check (redirects followed within the hub rule).
    async fn fetch(&self, f: &artifact::Fetch) -> Result<Fetched, FetchError> {
        let failed = || FetchError::Failed("fetch failed".into());
        let url = reqwest::Url::parse(&self.hub.rewrite(&f.url)).map_err(|_| failed())?;
        if !self.hub.allows(&url) {
            return Err(failed());
        }
        let client = self.hub_client.as_ref().ok_or_else(failed)?;
        let mut req = client
            .get(url)
            .timeout(Duration::from_millis(u64::from(f.timeout_ms)));
        if let Some(range) = f.range {
            req = req.header(reqwest::header::RANGE, range);
        }
        let mut res = req.send().await.map_err(|e| {
            if e.is_timeout() {
                FetchError::Timeout
            } else {
                failed()
            }
        })?;
        let status = res.status().as_u16();
        let content_range = res
            .headers()
            .get(reqwest::header::CONTENT_RANGE)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);
        let mut buf = Vec::new();
        let body = loop {
            match res.chunk().await {
                Ok(Some(chunk)) => {
                    buf.extend_from_slice(&chunk);
                    if buf.len() >= f.max_bytes {
                        buf.truncate(f.max_bytes);
                        break Ok(buf);
                    }
                }
                Ok(None) => break Ok(buf),
                Err(e) if e.is_timeout() => break Err(FetchError::Timeout),
                Err(_) => break Err(FetchError::Failed("terminated".into())),
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
    fn clear_model_scan(&mut self) {
        self.state.scan_clears.fetch_add(1, Ordering::SeqCst);
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
    fn downloads_requested(&mut self, _: &JValue) {}
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
}
