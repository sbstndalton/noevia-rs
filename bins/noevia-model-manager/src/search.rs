//! noevia#1318: native, read-only `GET /api/v1/search` and `GET /api/v1/search/repo`.
//!
//! Python (`model-manager/app/api.py`, `hf.py`, `discover.py`):
//!
//! ```python
//! found = await hf.search_models(q.strip(), sort=hub_sort, limit=max(1, min(int(limit or 30), 60)))
//! budget = max((float(b.get("vram_gb") or 0) for b in _backend_list()), default=0.0)
//! candidates = await asyncio.gather(*[detail(m) for m in found[:40]])   # tree listings, cached 1 h
//! judged = [discover.judge(c, budget_gb=budget_gb, trusted=_trusted_quantisers()) for c in candidates]
//! ranked = discover.rank(discover.apply_filters(judged, ...))
//! avatars = await hf.owner_avatars([r["owner"] for r in ranked])        # sqlite cache, 7 days
//! return {"budgetGb": ..., "hubUrl": ..., "counts": ..., "trusted": ..., "results": [...]}
//! ```
//!
//! and, for one repository, the tree listing grouped by shard set, the remote GGUF header
//! (`hf.gguf_header`, a ranged read of the first MiB) and the fit chips and context estimates.
//!
//! Native here: the Hub lookups (search, listings, header, avatars) with the token, timeouts,
//! admission and the `hub_page` bounds of the update check and the draft-head lookup; the
//! ranking and filter rules; the 1 h listing cache, the header cache and the avatar cache; the
//! query validation and its 422; and the JSON. Delegated, never re-implemented: live state that
//! only the Python child holds, asked once per request through two private calls
//! (`GET /api/v1/internal/search-runtime`: the GPU budget, the downloaded files and the trusted
//! publisher sets; `POST /api/v1/internal/search-repo-runtime`: the fit chips and the context
//! estimates of the groups, the autoconfig fit math). When the child cannot answer, the route
//! answers 502 as any forwarded route does, never a guess.
//!
//! Deliberate differences (see `docs/native-search.md`): conditions this side cannot follow
//! exactly answer the native 503 refusal instead (text outside printable ASCII where Python's
//! patterns are applied, hub fields of odd JSON types, numbers past 64 bits, a page past the
//! `hub_page` bounds); a repository id that is not Hub-shaped is not sent to the Hub.
mod discover;
mod net;
mod params;
mod py;
mod store;
mod tree;

use crate::check_updates::{hub_auth, HubAuth};
use crate::hub_page::PAGE_CAP;
use crate::model_updates::json_string;
use discover::{Candidate, File, Filters, Judged, Opt, Tag};
use model_files::json::Value;
use py::Unsupported;
use serde_json::value::RawValue;
use std::{
    collections::{BTreeSet, HashMap, HashSet},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};
use tree::TreeError;

/// Body of the native refusal (503) when this side cannot answer exactly.
pub const REFUSAL: &str = "Native search is unavailable";
/// The child's private calls.
pub const RUNTIME_PATH: &str = "/api/v1/internal/search-runtime";
pub const REPO_RUNTIME_PATH: &str = "/api/v1/internal/search-repo-runtime";
/// `hf.HF_API` and `hf.HF_RESOLVE`.
pub const HF_API: &str = "https://huggingface.co/api";
pub const HF_RESOLVE: &str = "https://huggingface.co";
/// The child's private-call answers are read up to this size.
const CHILD_CAP: usize = 16 * 1024 * 1024;
/// A Hub error body is read up to this size (Python shows 200 characters of it).
const ERROR_BODY_CAP: usize = 1024 * 1024;
/// File groups of one repository (the child's `SEARCH_REPO_MAX_GROUPS`).
const MAX_GROUPS: usize = 4096;
/// `api._REPO_FILES_TTL`, and the entries `_REPO_FILES` holds before it is cleared.
const FILES_TTL: f64 = 3600.0;
const FILES_MAX: usize = 400;
/// A bound Python does not have: the listings cached hold at most this many bytes of paths.
const FILES_CACHE_BYTES: usize = 32 * 1024 * 1024;
/// `hf._HEADER_BYTES`, `hf._HEADER_CACHE_MAX`.
const HEADER_BYTES: usize = 1024 * 1024;
const HEADER_CACHE_MAX: usize = 64;
/// A bound Python does not have: repositories remembered as gated.
const GATED_MAX: usize = 4096;
/// `hf.AVATAR_TTL_S`.
const AVATAR_TTL: f64 = 7.0 * 86_400.0;
/// Candidates whose listing is asked for (`found[:40]`).
const CANDIDATES: usize = 40;
/// A bound httpx does not have: the whole of one request.
const REQUEST_BUDGET: Duration = Duration::from_secs(120);
/// Concurrent Hub listings (noevia#1973): each may hold about 100 MiB at most (see
/// `draft_heads::CONCURRENT_LOOKUPS`); later requests wait.
pub const CONCURRENT_LOOKUPS: usize = 2;
const COULD_NOT_REACH: &str =
    "Could not reach Hugging Face. Check this server's connection and try again.";
const FILES_REFUSED: &str =
    "The repository file list could not be checked, so it was not used. See the model manager log.";
/// `hf.SEARCH_SORTS`.
const SORTS: [&str; 4] = ["downloads", "likes", "lastModified", "trendingScore"];

pub type Clock = fn() -> f64;

fn now() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0.0, |d| d.as_secs_f64())
}

/// The timeouts of the Hub lookups (httpx: connect and every read).
#[derive(Debug, Clone, Copy)]
pub struct Timeouts {
    /// `hf.SEARCH_TIMEOUT_S`.
    pub search: Duration,
    /// `repo_detail`'s `timeout=30.0`.
    pub tree: Duration,
    /// `gguf_header`'s `timeout=25.0`.
    pub header: Duration,
    /// `owner_avatars`' `timeout=8.0`.
    pub avatar: Duration,
    /// A bound httpx does not have: the listings of a search (all candidates together), and one
    /// repository's listing. A lookup still running at it is a failed listing (no files, not
    /// cached; in the repository route `Network error:` with no text, as a timeout has).
    pub listings: Duration,
}

impl Default for Timeouts {
    fn default() -> Self {
        Self {
            search: Duration::from_secs(45),
            tree: Duration::from_secs(30),
            header: Duration::from_secs(25),
            avatar: Duration::from_secs(8),
            listings: Duration::from_secs(90),
        }
    }
}

/// Where the Hub is (tests point it at a local fake).
#[derive(Debug, Clone)]
pub struct Hub {
    pub api: String,
    pub resolve: String,
}

impl Default for Hub {
    fn default() -> Self {
        Self {
            api: HF_API.to_owned(),
            resolve: HF_RESOLVE.to_owned(),
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    /// A finished answer: status and JSON body.
    Json(u16, Vec<u8>),
    /// Python's handler raised: FastAPI's plain 500.
    Failed,
    /// This side could not answer exactly: the native 503 refusal.
    Refused,
    /// The Python child did not answer its live state: 502, as a forwarded route.
    Unavailable,
}

/// Why a request stops before its answer is built.
#[derive(Debug)]
enum Stop {
    Failed,
    Refused,
    Unavailable,
    /// An answer of its own (an exception handler's JSON).
    Json(u16, Vec<u8>),
}

impl From<Unsupported> for Stop {
    fn from(_: Unsupported) -> Self {
        Stop::Refused
    }
}

impl From<store::Unreadable> for Stop {
    fn from(_: store::Unreadable) -> Self {
        Stop::Failed
    }
}

impl From<Stop> for Outcome {
    fn from(stop: Stop) -> Self {
        match stop {
            Stop::Failed => Outcome::Failed,
            Stop::Refused => Outcome::Refused,
            Stop::Unavailable => Outcome::Unavailable,
            Stop::Json(status, body) => Outcome::Json(status, body),
        }
    }
}

/// `hf.search_models` ends with an `HfSearchError` (a sentence for the user) or a failure.
#[derive(Debug)]
enum SearchError {
    Message(String),
    Stop(Stop),
}

impl From<Stop> for SearchError {
    fn from(stop: Stop) -> Self {
        SearchError::Stop(stop)
    }
}

impl From<Unsupported> for SearchError {
    fn from(_: Unsupported) -> Self {
        SearchError::Stop(Stop::Refused)
    }
}

/// `hf.HfModel`, as far as the route reads it.
#[derive(Debug, Clone)]
struct Model {
    id: Id,
    downloads: i64,
    likes: i64,
    last_modified: String,
    tags: Vec<Tag>,
}

/// `item.get("id") or item.get("modelId") or ""`.
#[derive(Debug, Clone)]
enum Id {
    Text(String),
    /// Not a str: the first use of it raises in Python.
    Other,
}

/// The token, as the requests carry it.
enum Auth {
    None,
    Header(reqwest::header::HeaderValue),
    /// httpx raises while encoding the header (a UnicodeEncodeError).
    NotAscii,
    /// h11 refuses the header value (`LocalProtocolError`, an `HTTPError`).
    Refused,
}

/// What the stored token amounts to, to tell one token from another. Every token the client
/// cannot send (not ASCII, or refused by h11) is one: no request carries it.
#[derive(PartialEq, Eq)]
enum Token {
    Absent,
    Value(Vec<u8>),
    Unusable,
}

/// The token the newest `auth()` read saw.
#[derive(Default)]
struct Ledger {
    /// The ticket of the newest read that was applied: an older one that ends later is not.
    applied: u64,
    last: Option<Token>,
}

/// The cache generation a request was made under, or none when its token read was superseded by
/// a newer one that saw another token: its results are not remembered.
#[derive(Clone, Copy)]
struct Epoch(Option<u64>);

/// What a stored header does to the gated repositories.
enum Gate<'a> {
    Keep,
    Lift(&'a str),
    Set(&'a str, &'static str),
}

/// `api._REPO_FILES`.
#[derive(Default)]
struct FilesCache {
    map: HashMap<String, (f64, Arc<Vec<File>>)>,
    bytes: usize,
}

/// `hf._HEADER_CACHE` and `hf._HEADER_GATED`.
#[derive(Default)]
struct HeaderCache {
    map: HashMap<String, Option<Arc<str>>>,
    gated: HashMap<String, &'static str>,
}

impl HeaderCache {
    /// `_HEADER_CACHE[key] = value`. Python clears a full cache only for a success and lets the
    /// failures grow; here every insert is bounded.
    fn put(&mut self, key: String, value: Option<Arc<str>>) {
        if self.map.len() >= HEADER_CACHE_MAX {
            self.map.clear();
        }
        self.map.insert(key, value);
    }
}

pub struct Search {
    database: PathBuf,
    hub: Hub,
    clock: Clock,
    legacy: reqwest::Client,
    legacy_base: String,
    token: String,
    search_client: reqwest::Client,
    tree_client: reqwest::Client,
    header_client: reqwest::Client,
    avatar_client: reqwest::Client,
    lookups: Arc<tokio::sync::Semaphore>,
    listing_deadline: Duration,
    /// The token the newest read saw: caches filled under another token are dropped, since a
    /// listing or a header may be private to a token.
    last_auth: Mutex<Ledger>,
    /// Numbers the token reads, before each one starts.
    tickets: AtomicU64,
    /// Bumped, before the caches are cleared, whenever the token changes: a request that read
    /// the token under an older generation does not store what it learnt.
    generation: AtomicU64,
    files: Mutex<FilesCache>,
    headers: Mutex<HeaderCache>,
    /// Lib tests: runs before the final parse of every listing.
    #[cfg(test)]
    parse_hook: Mutex<Option<tree::ParseHook>>,
}

fn client(timeout: Duration, redirects: bool) -> Result<reqwest::Client, &'static str> {
    reqwest::Client::builder()
        .connect_timeout(timeout)
        .read_timeout(timeout)
        .timeout(REQUEST_BUDGET.max(timeout))
        .redirect(if redirects {
            reqwest::redirect::Policy::limited(20)
        } else {
            reqwest::redirect::Policy::none()
        })
        .build()
        .map_err(|_| "Hugging Face client could not be built")
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

impl Search {
    /// `data` is Python's `DATA_DIR`; the child is at `legacy_base` and takes `token`.
    pub fn new(data: &Path, legacy_base: &str, token: &str) -> Result<Self, &'static str> {
        Self::with(
            data,
            Hub::default(),
            legacy_base,
            token,
            now,
            Timeouts::default(),
        )
    }

    /// Tests point the Hub at a local fake, fix the clock and shorten the timeouts.
    pub fn with(
        data: &Path,
        hub: Hub,
        legacy_base: &str,
        token: &str,
        clock: Clock,
        timeouts: Timeouts,
    ) -> Result<Self, &'static str> {
        Ok(Self {
            database: data.join("model_loader.db"),
            hub: Hub {
                api: hub.api.trim_end_matches('/').to_owned(),
                resolve: hub.resolve.trim_end_matches('/').to_owned(),
            },
            clock,
            legacy: crate::legacy_client().map_err(|_| "child client could not be built")?,
            legacy_base: legacy_base.to_owned(),
            token: token.to_owned(),
            search_client: client(timeouts.search, false)?,
            tree_client: client(timeouts.tree, true)?,
            header_client: client(timeouts.header, true)?,
            avatar_client: client(timeouts.avatar, true)?,
            lookups: Arc::new(tokio::sync::Semaphore::new(CONCURRENT_LOOKUPS)),
            listing_deadline: timeouts.listings,
            last_auth: Mutex::new(Ledger::default()),
            tickets: AtomicU64::new(0),
            generation: AtomicU64::new(0),
            files: Mutex::new(FilesCache::default()),
            headers: Mutex::new(HeaderCache::default()),
            #[cfg(test)]
            parse_hook: Mutex::new(None),
        })
    }

    /// `GET /api/v1/search?...` (`query` is the raw query string).
    pub async fn search(&self, query: &str) -> Outcome {
        let params = match params::search(query) {
            Ok(params) => params,
            Err(params::Rejection::Invalid(body)) => return Outcome::Json(422, body.into_bytes()),
            Err(params::Rejection::Unsupported) => return Outcome::Refused,
        };
        match self.run_search(&params).await {
            Ok(body) => Outcome::Json(200, body.into_bytes()),
            Err(stop) => stop.into(),
        }
    }

    /// `GET /api/v1/search/repo?repo=...`.
    pub async fn repo(&self, query: &str) -> Outcome {
        let repo = match params::repo(query) {
            Ok(repo) => repo,
            Err(body) => return Outcome::Json(422, body.into_bytes()),
        };
        match self.run_repo(&repo).await {
            Ok(body) => Outcome::Json(200, body.into_bytes()),
            Err(stop) => stop.into(),
        }
    }

    // ---- shared pieces ---------------------------------------------------------------------

    #[cfg(test)]
    fn hook(&self) -> Option<tree::ParseHook> {
        lock(&self.parse_hook).clone()
    }

    #[cfg(not(test))]
    #[allow(clippy::unused_self)]
    fn hook(&self) -> Option<tree::ParseHook> {
        None
    }

    /// Runs a blocking step under the request's admission.
    async fn blocking<T, F>(&self, work: F) -> Result<T, Stop>
    where
        F: FnOnce() -> T + Send + 'static,
        T: Send + 'static,
    {
        crate::docker::spawn_blocking_admitted(work)
            .await
            .map_err(|_| Stop::Failed)
    }

    /// `_auth_headers()`: the stored token as the `Authorization` value, and the generation the
    /// request's results may be stored under.
    async fn auth(&self) -> Result<(Auth, Epoch), Stop> {
        let ticket = self.tickets.fetch_add(1, Ordering::SeqCst) + 1;
        let database = self.database.clone();
        let read = self.blocking(move || hub_auth(&database)).await?;
        let seen = match &read {
            HubAuth::Absent => Token::Absent,
            HubAuth::Value(value) => Token::Value(value.as_bytes().to_vec()),
            HubAuth::NotAscii | HubAuth::Refused => Token::Unusable,
            HubAuth::Failed => return Err(Stop::Failed),
        };
        let epoch = {
            let mut ledger = lock(&self.last_auth);
            let same = ledger.last.as_ref() == Some(&seen);
            if ticket <= ledger.applied {
                // A newer read already ended: this one only moves nothing forward. Its token is
                // the current one or an older one, and an older one's results are not kept.
                Epoch(same.then(|| self.generation.load(Ordering::SeqCst)))
            } else {
                ledger.applied = ticket;
                if !same && ledger.last.is_some() {
                    // Another token: what was learnt under the old one may not hold. The
                    // generation moves first, so that a request still under the old one that
                    // stores afterwards (it checks it under the cache's lock) finds it changed.
                    self.generation.fetch_add(1, Ordering::SeqCst);
                    self.drop_learnt();
                }
                ledger.last = Some(seen);
                Epoch(Some(self.generation.load(Ordering::SeqCst)))
            }
        };
        let auth = match read {
            HubAuth::Absent => Auth::None,
            HubAuth::Value(value) => Auth::Header(value),
            HubAuth::NotAscii => Auth::NotAscii,
            HubAuth::Refused => Auth::Refused,
            HubAuth::Failed => return Err(Stop::Failed),
        };
        Ok((auth, epoch))
    }

    /// Everything learnt from the Hub that a token may have shaped.
    fn drop_learnt(&self) {
        let mut files = lock(&self.files);
        files.map.clear();
        files.bytes = 0;
        let mut headers = lock(&self.headers);
        headers.map.clear();
        headers.gated.clear();
    }

    /// Whether a request made under `epoch` may still store what it learnt. Called with the
    /// cache's lock held.
    fn current(&self, epoch: Epoch) -> bool {
        epoch.0 == Some(self.generation.load(Ordering::SeqCst))
    }

    /// `_HEADER_CACHE[key] = value` and the gate the answer set or lifted.
    fn store_header(&self, epoch: Epoch, key: String, value: Option<Arc<str>>, gate: Gate<'_>) {
        let mut cache = lock(&self.headers);
        if !self.current(epoch) {
            return;
        }
        match gate {
            Gate::Keep => {}
            Gate::Lift(repo) => {
                cache.gated.remove(repo);
            }
            Gate::Set(repo, reason) => {
                if cache.gated.len() >= GATED_MAX {
                    cache.gated.clear();
                }
                cache.gated.insert(repo.to_owned(), reason);
            }
        }
        cache.put(key, value);
    }

    /// One call of the child's private API.
    async fn child(&self, request: reqwest::RequestBuilder) -> Result<Vec<u8>, Stop> {
        let request = if self.token.is_empty() {
            request
        } else {
            request.header("x-model-loader-token", &self.token)
        };
        let response = request.send().await.map_err(|_| Stop::Unavailable)?;
        let status = response.status();
        let body = net::read_body(response, CHILD_CAP)
            .await
            .map_err(|_| Stop::Unavailable)?;
        match status.as_u16() {
            200 => Ok(body),
            // Its own unhandled error is Python's 500.
            500 => Err(Stop::Failed),
            _ => Err(Stop::Unavailable),
        }
    }

    // ---- GET /api/v1/search ----------------------------------------------------------------

    async fn run_search(&self, p: &params::SearchParams) -> Result<String, Stop> {
        let query = py::strip(&p.q);
        let hub_sort = if p.sort == "fit" || p.sort == "trending" {
            "trendingScore"
        } else {
            p.sort.as_str()
        };
        let found = match self.search_models(query, hub_sort, p.limit).await {
            Ok(found) => found,
            Err(SearchError::Message(message)) => {
                // `return {"error": str(e), "results": []}`.
                return Ok(format!(
                    "{{\"error\":{},\"results\":[]}}",
                    json_string(&message)
                ));
            }
            Err(SearchError::Stop(stop)) => return Err(stop),
        };
        let runtime = self.search_runtime().await?;
        // `budget_gb = max(1.0, (budget * 1.073 - 2.5)) if budget else 1e6`.
        let budget_gb = if runtime.budget != 0.0 {
            f64::max(1.0, runtime.budget * 1.073 - 2.5)
        } else {
            1e6
        };

        // `detail(model)` for `found[:40]`: the owner, the licence, the listing.
        let top: Vec<&Model> = found.iter().take(CANDIDATES).collect();
        let mut ids: Vec<String> = Vec::with_capacity(top.len());
        for model in &top {
            match &model.id {
                Id::Text(id) => ids.push(id.clone()),
                // `model.id.split("/", 1)` raises.
                Id::Other => return Err(Stop::Failed),
            }
        }
        let listings = self.repo_files(&ids).await?;
        let mut candidates: Vec<Candidate> = Vec::with_capacity(top.len());
        for ((model, id), files) in top.iter().zip(&ids).zip(listings) {
            let owner = id.split_once('/').map_or(id.as_str(), |(owner, _)| owner);
            let license = model.tags.iter().find_map(|tag| match tag {
                Tag::Text(t) => t.strip_prefix("license:").map(str::to_owned),
                Tag::Opaque => None,
            });
            candidates.push(Candidate {
                id: id.clone(),
                owner: owner.to_owned(),
                downloads: i128::from(model.downloads),
                likes: i128::from(model.likes),
                last_modified: model.last_modified.clone(),
                tags: model.tags.clone(),
                files,
                license,
            });
        }

        let extra = self.trusted_extra().await?;
        let quantisers: HashSet<String> = runtime
            .quantisers
            .iter()
            .cloned()
            .chain(extra.iter().cloned())
            .collect();
        let labs: HashSet<String> = runtime.labs.iter().cloned().collect();
        let instant = py::instant_of_unix((self.clock)())?;
        let mut judged: Vec<Judged> = Vec::with_capacity(candidates.len());
        for candidate in &candidates {
            judged.push(
                discover::judge(candidate, budget_gb, &quantisers, &labs, instant).inspect_err(
                    |_| {
                        eprintln!(
                            "search: refusing the answer, {:?} cannot be judged exactly",
                            candidate.id
                        );
                    },
                )?,
            );
        }
        let found_count = judged.len();
        let hidden_untrusted = judged.iter().filter(|r| !r.trusted).count();
        let hidden_unsuitable = judged.iter().filter(|r| !r.suitable).count();
        let quants: Vec<&str> = p.quants.split(',').filter(|x| !x.is_empty()).collect();
        let mut ranked = discover::apply_filters(
            judged,
            &Filters {
                trusted_only: p.trusted_only,
                show_unsuitable: p.show_unsuitable,
                min_gb: p.min_gb,
                max_gb: p.max_gb,
                quants,
                min_params: p.min_params,
                max_params: p.max_params,
                moe: &p.moe,
                vision: p.vision,
                license_contains: &p.license,
                owner: &p.owner,
            },
        );
        if p.sort == "fit" || p.sort == "trending" {
            discover::rank(&mut ranked);
        }
        // Decoration: a failure here never fails the search.
        let owners: Vec<&str> = ranked.iter().map(|r| r.owner.as_str()).collect();
        let avatars = self.owner_avatars(&owners).await?;

        let mut out = String::with_capacity(4096);
        out.push_str("{\"budgetGb\":");
        float(&mut out, py::round_to(budget_gb, 1))?;
        out.push_str(",\"hubUrl\":");
        out.push_str(&json_string(&discover::hub_url(query, &p.owner)));
        out.push_str(&format!(
            ",\"counts\":{{\"found\":{found_count},\"shown\":{},\"hiddenUntrusted\":{},\"hiddenUnsuitable\":{}}}",
            ranked.len(),
            if p.trusted_only { hidden_untrusted } else { 0 },
            if p.show_unsuitable { 0 } else { hidden_unsuitable },
        ));
        let all: BTreeSet<&String> = quantisers.iter().chain(labs.iter()).collect();
        out.push_str(",\"trusted\":[");
        out.push_str(
            &all.iter()
                .map(|name| json_string(name))
                .collect::<Vec<_>>()
                .join(","),
        );
        out.push_str("],\"results\":[");
        for (i, r) in ranked.iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            let avatar = avatars.get(&r.owner).map_or("", String::as_str);
            let downloaded = runtime.downloaded.get(&r.id).map_or("[]", |raw| raw.get());
            write_judged(&mut out, r, avatar, downloaded)?;
        }
        out.push_str("]}");
        Ok(out)
    }

    /// `hf.search_models(query, sort, limit)`.
    async fn search_models(
        &self,
        query: &str,
        sort: &str,
        limit: i64,
    ) -> Result<Vec<Model>, SearchError> {
        let header = match self.auth().await?.0 {
            Auth::None => None,
            Auth::Header(value) => Some(value),
            // The client cannot be built: UnicodeEncodeError, not an `HTTPError`.
            Auth::NotAscii => return Err(Stop::Failed.into()),
            // The request cannot be sent: `LocalProtocolError`, an `HTTPError`.
            Auth::Refused => return Err(SearchError::Message(COULD_NOT_REACH.to_owned())),
        };
        let sort = if SORTS.contains(&sort) {
            sort
        } else {
            "downloads"
        };
        let mut base = format!("limit={limit}&sort={}&direction=-1", py::quote_plus(sort));
        if !query.is_empty() {
            base.push_str("&search=");
            base.push_str(&py::quote_plus(query));
        }
        let first = format!("{}/models?{base}&filter=gguf", self.hub.api);
        let items = self.get_models(&first, header.as_ref()).await?;
        if !items.is_empty() {
            return Ok(models(&items)?);
        }
        // Browse mode (no query) is left alone: an untagged "top models overall" is noise.
        if query.is_empty() {
            return Ok(Vec::new());
        }
        let second = format!("{}/models?{base}", self.hub.api);
        let items = self.get_models(&second, header.as_ref()).await?;
        let loose = models(&items)?;
        let mut kept: Vec<Model> = Vec::new();
        for model in &loose {
            if looks_like_gguf(model)? {
                kept.push(model.clone());
            }
        }
        Ok(if kept.is_empty() { loose } else { kept })
    }

    /// `hf._get_models`: the dicts of a JSON list.
    async fn get_models(
        &self,
        url: &str,
        header: Option<&reqwest::header::HeaderValue>,
    ) -> Result<Vec<Value>, SearchError> {
        let reach = || SearchError::Message(COULD_NOT_REACH.to_owned());
        let mut request = self.search_client.get(url);
        if let Some(value) = header {
            request = request.header(reqwest::header::AUTHORIZATION, value.clone());
        }
        let response = request.send().await.map_err(|error| {
            eprintln!("hub search transport error: {:?}", net::describe(&error));
            reach()
        })?;
        let status = response.status().as_u16();
        let content_type = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned);
        if status >= 400 {
            let body = match net::read_body(response, ERROR_BODY_CAP).await {
                Ok(body) => body,
                Err(net::BodyError::Transport(_)) => return Err(reach()),
                Err(net::BodyError::Cap) => return Err(Stop::Refused.into()),
            };
            let text = net::text(&body, content_type.as_deref())?;
            let detail = net::stripped_head(&text, 200);
            let hint = match status {
                401 | 403 => " Check the Hugging Face token.",
                429 => {
                    " Hugging Face is rate limiting this server; add a token to raise the limit."
                }
                _ => "",
            };
            let tail = if detail.is_empty() {
                String::new()
            } else {
                format!(" {detail}")
            };
            return Err(SearchError::Message(format!(
                "Hugging Face returned HTTP {status}.{hint}{tail}"
            )));
        }
        let body = match net::read_body(response, PAGE_CAP).await {
            Ok(body) => body,
            Err(net::BodyError::Transport(_)) => return Err(reach()),
            Err(net::BodyError::Cap) => return Err(Stop::Refused.into()),
        };
        match tree::parse_page(body) {
            Ok(Value::Arr(items)) => Ok(items
                .into_iter()
                .filter(|item| matches!(item, Value::Obj(_)))
                .collect()),
            // `data` of another type: `[]`.
            Ok(_) => Ok(Vec::new()),
            // `r.json()` raised: not an `HTTPError`, so Python's 500.
            Err(tree::Page::Syntax) => Err(Stop::Failed.into()),
            Err(tree::Page::Limit) => Err(Stop::Refused.into()),
        }
    }

    /// `api._repo_files_cached(repo)` for each repository: the cache is looked at once for all
    /// of them (as the concurrent coroutines do), the misses are listed, successes are cached.
    async fn repo_files(&self, repos: &[String]) -> Result<Vec<Arc<Vec<File>>>, Stop> {
        let at = (self.clock)();
        let mut found: Vec<Option<Arc<Vec<File>>>> = {
            let cache = lock(&self.files);
            repos
                .iter()
                .map(|repo| {
                    cache
                        .map
                        .get(repo)
                        .filter(|(stamp, _)| at - stamp < FILES_TTL)
                        .map(|(_, files)| files.clone())
                })
                .collect()
        };
        if found.iter().all(Option::is_some) {
            return Ok(found.into_iter().flatten().collect());
        }
        // Any failure to list is `files = []`, and is not cached.
        let (header, epoch) = match self.auth().await {
            Ok((Auth::None, epoch)) => (Some(None), epoch),
            Ok((Auth::Header(value), epoch)) => (Some(Some(value)), epoch),
            Ok((Auth::NotAscii | Auth::Refused, epoch)) => (None, epoch),
            Err(Stop::Failed) => (None, Epoch(None)),
            Err(other) => return Err(other),
        };
        let cut_off = std::sync::atomic::AtomicUsize::new(0);
        let cut_off = &cut_off;
        let deadline = tokio::time::Instant::now() + self.listing_deadline;
        let lookups = repos
            .iter()
            .zip(&found)
            .filter(|(_, hit)| hit.is_none())
            .map(|(repo, _)| repo.as_str())
            .map(|repo| {
                let header = header.clone();
                async move {
                    let Some(header) = header else {
                        return Ok(None);
                    };
                    let work = async {
                        let Ok(permit) = self.lookups.clone().acquire_owned().await else {
                            return Err(Stop::Refused);
                        };
                        let permit = Arc::new(permit);
                        let listed = tree::list(
                            &self.tree_client,
                            &self.hub.api,
                            header.as_ref(),
                            repo,
                            &permit,
                            self.hook(),
                        )
                        .await;
                        drop(permit);
                        match listed {
                            Ok(files) => gguf_files(files).map(Some),
                            Err(_) => Ok(None),
                        }
                    };
                    match tokio::time::timeout_at(deadline, work).await {
                        Ok(result) => result,
                        Err(_) => {
                            cut_off.fetch_add(1, Ordering::Relaxed);
                            Ok(None)
                        }
                    }
                }
            });
        let results = futures_util::future::join_all(lookups).await;
        let cut_off = cut_off.load(Ordering::Relaxed);
        if cut_off > 0 {
            // Not an empty repository: its listing did not finish.
            eprintln!(
                "search: {cut_off} of {} listings were cut off by the {:?} listing deadline",
                results.len(),
                self.listing_deadline
            );
        }
        let mut results = results.into_iter();
        for (repo, slot) in repos.iter().zip(found.iter_mut()) {
            if slot.is_some() {
                continue;
            }
            let files = match results.next() {
                Some(result) => result?,
                None => None,
            };
            match files {
                Some(files) => {
                    let files = Arc::new(files);
                    self.remember(epoch, repo, files.clone());
                    *slot = Some(files);
                }
                None => *slot = Some(Arc::new(Vec::new())),
            }
        }
        Ok(found.into_iter().flatten().collect())
    }

    /// `_REPO_FILES[repo] = (time.time(), files)`, clearing a cache past its bounds first.
    fn remember(&self, epoch: Epoch, repo: &str, files: Arc<Vec<File>>) {
        let weight: usize = files.iter().map(|f| f.path.len() + 48).sum::<usize>() + repo.len();
        if weight > FILES_CACHE_BYTES {
            return;
        }
        let mut cache = lock(&self.files);
        if !self.current(epoch) {
            return;
        }
        if cache.map.len() > FILES_MAX || cache.bytes + weight > FILES_CACHE_BYTES {
            cache.map.clear();
            cache.bytes = 0;
        }
        let stamp = (self.clock)();
        if let Some((_, old)) = cache.map.insert(repo.to_owned(), (stamp, files)) {
            let old_weight: usize =
                old.iter().map(|f| f.path.len() + 48).sum::<usize>() + repo.len();
            cache.bytes = cache.bytes.saturating_sub(old_weight);
        }
        cache.bytes += weight;
    }

    /// The `trusted_quantisers` setting, as `_trusted_quantisers()` splits it.
    async fn trusted_extra(&self) -> Result<Vec<String>, Stop> {
        let database = self.database.clone();
        let value = self
            .blocking(move || {
                let db = store::open(&database)?;
                store::setting(&db, "trusted_quantisers")
            })
            .await??;
        let text = match value {
            None | Some(store::Stored::Null) => String::new(),
            Some(store::Stored::Text(text)) => text,
            // `.split` of a number or of bytes raises.
            Some(_) => return Err(Stop::Failed),
        };
        Ok(text
            .split(',')
            .map(py::strip)
            .filter(|x| !x.is_empty())
            .map(str::to_lowercase)
            .collect())
    }

    /// `hf.owner_avatars(owners)`, which the route wraps in `except Exception: avatars = {}`.
    async fn owner_avatars(&self, owners: &[&str]) -> Result<HashMap<String, String>, Stop> {
        match self.avatars(owners).await {
            Ok(found) => Ok(found),
            Err(AvatarError::Raised) => Ok(HashMap::new()),
            Err(AvatarError::Stop(stop)) => Err(stop),
        }
    }

    async fn avatars(&self, owners: &[&str]) -> Result<HashMap<String, String>, AvatarError> {
        let at = (self.clock)();
        let unique: BTreeSet<&str> = owners.iter().copied().filter(|o| !o.is_empty()).collect();
        let names: Vec<String> = unique.iter().map(|o| (*o).to_owned()).collect();
        let database = self.database.clone();
        let cached = self
            .blocking(
                move || -> Result<(HashMap<String, String>, Vec<String>), AvatarError> {
                    let db = store::open(&database).map_err(|_| AvatarError::Raised)?;
                    let mut result = HashMap::new();
                    let mut missing = Vec::new();
                    for owner in names {
                        let row = store::avatar(&db, &owner).map_err(|_| AvatarError::Raised)?;
                        let Some((url, fetched)) = row else {
                            missing.push(owner);
                            continue;
                        };
                        // `now - cached["fetched_at"]` raises for anything but a number.
                        let fetched = match fetched {
                            store::Stored::Int(n) => n as f64,
                            store::Stored::Real(f) => f,
                            _ => return Err(AvatarError::Raised),
                        };
                        if at - fetched < AVATAR_TTL {
                            match url {
                                store::Stored::Text(url) if url.is_empty() => {}
                                store::Stored::Text(url) => {
                                    result.insert(owner, url);
                                }
                                // A cached url that is not a str reaches the JSON as it is.
                                _ => return Err(AvatarError::Stop(Stop::Refused)),
                            }
                            continue;
                        }
                        missing.push(owner);
                    }
                    Ok((result, missing))
                },
            )
            .await
            .map_err(AvatarError::Stop)??;
        let (mut result, to_fetch) = cached;
        if to_fetch.is_empty() {
            return Ok(result);
        }
        let header = match self.auth().await {
            Ok((Auth::None, _)) => Some(None),
            Ok((Auth::Header(value), _)) => Some(Some(value)),
            // Each request fails in h11: every owner caches "no avatar".
            Ok((Auth::Refused, _)) => None,
            Ok((Auth::NotAscii, _)) | Err(_) => return Err(AvatarError::Raised),
        };
        let fetches = to_fetch.iter().map(|owner| {
            let header = header.clone();
            async move {
                match header {
                    Some(header) => self.fetch_avatar(owner, header.as_ref()).await,
                    None => Ok(None),
                }
            }
        });
        let mut urls: Vec<Option<String>> = Vec::with_capacity(to_fetch.len());
        for fetched in futures_util::future::join_all(fetches).await {
            urls.push(fetched?);
        }
        let database = self.database.clone();
        let rows: Vec<(String, Option<String>)> = to_fetch.into_iter().zip(urls).collect();
        let written = self
            .blocking(
                move || -> Result<Vec<(String, Option<String>)>, AvatarError> {
                    let db = store::open(&database).map_err(|_| AvatarError::Raised)?;
                    for (owner, url) in &rows {
                        store::set_avatar(&db, owner, url.as_deref().unwrap_or(""), at)
                            .map_err(|_| AvatarError::Raised)?;
                    }
                    Ok(rows)
                },
            )
            .await
            .map_err(AvatarError::Stop)??;
        for (owner, url) in written {
            if let Some(url) = url {
                result.insert(owner, url);
            }
        }
        Ok(result)
    }

    /// `hf._fetch_owner_avatar`: the organisation's overview, then the user's.
    async fn fetch_avatar(
        &self,
        owner: &str,
        header: Option<&reqwest::header::HeaderValue>,
    ) -> Result<Option<String>, AvatarError> {
        // Only Hub-shaped names are asked about (as for a listing).
        if !crate::check_updates::hub_id(owner) || owner.contains('/') {
            return Ok(None);
        }
        for endpoint in [
            format!("{}/organizations/{owner}/overview", self.hub.api),
            format!("{}/users/{owner}/overview", self.hub.api),
        ] {
            let mut request = self.avatar_client.get(&endpoint);
            if let Some(value) = header {
                request = request.header(reqwest::header::AUTHORIZATION, value.clone());
            }
            let Ok(response) = request.send().await else {
                continue;
            };
            if response.status().as_u16() != 200 {
                continue;
            }
            // A body past the bound counts as one `r.json()` refuses.
            let Ok(body) = net::read_body(response, ERROR_BODY_CAP).await else {
                continue;
            };
            let Ok(data) = tree::parse_page(body) else {
                continue;
            };
            // `data.get(...)` raises for anything but a dict.
            let Value::Obj(_) = data else {
                return Err(AvatarError::Raised);
            };
            let url = match data.get("avatarUrl") {
                Some(v) if v.truthy() => v,
                _ => continue,
            };
            let Value::Str(url) = url else {
                // `url.startswith` raises for a number or a list.
                return Err(AvatarError::Raised);
            };
            return Ok(Some(if url.starts_with("//") {
                format!("https:{url}")
            } else if url.starts_with('/') {
                format!("https://huggingface.co{url}")
            } else {
                url.clone()
            }));
        }
        Ok(None)
    }

    /// The child's `{budget, downloaded, quantisers, labs}`.
    async fn search_runtime(&self) -> Result<Runtime, Stop> {
        let body = self
            .child(
                self.legacy
                    .get(format!("{}{RUNTIME_PATH}", self.legacy_base)),
            )
            .await?;
        parse_runtime(&body).ok_or(Stop::Unavailable)
    }

    // ---- GET /api/v1/search/repo -----------------------------------------------------------

    async fn run_repo(&self, repo: &str) -> Result<String, Stop> {
        let failure = |message: String| {
            Ok(format!(
                "{{\"repo\":{},\"error\":{},\"groups\":[]}}",
                json_string(repo),
                json_string(&message)
            ))
        };
        let header = match self.auth().await?.0 {
            Auth::None => None,
            Auth::Header(value) => Some(value),
            Auth::NotAscii => return Err(Stop::Failed),
            // h11's message carries the header value; it is not reproduced.
            Auth::Refused => return Err(Stop::Refused),
        };
        let lookup = Arc::new(
            self.lookups
                .clone()
                .acquire_owned()
                .await
                .map_err(|_| Stop::Refused)?,
        );
        let listing = tree::list(
            &self.tree_client,
            &self.hub.api,
            header.as_ref(),
            repo,
            &lookup,
            self.hook(),
        );
        let deadline = tokio::time::Instant::now() + self.listing_deadline;
        let listed = match tokio::time::timeout_at(deadline, listing).await {
            Ok(listed) => listed,
            // A timeout has no message in httpx.
            Err(_) => {
                eprintln!(
                    "search: the listing of {repo:?} was cut off by the {:?} listing deadline",
                    self.listing_deadline
                );
                Err(TreeError::Transport(String::new()))
            }
        };
        drop(lookup);
        let files = match listed {
            Ok(files) => files,
            // Not an id the Hub lists: the answer it gives for an unknown repository.
            Err(TreeError::Invalid) => return failure("Hugging Face returned HTTP 404".to_owned()),
            Err(TreeError::Status(status)) => {
                return failure(format!("Hugging Face returned HTTP {status}"))
            }
            Err(TreeError::Transport(message)) => {
                return failure(format!("Network error: {message}"))
            }
            Err(TreeError::BadBody) => return Err(Stop::Failed),
            Err(TreeError::Limit) => return Err(Stop::Refused),
            Err(TreeError::Files) => {
                return Err(Stop::Json(
                    502,
                    format!("{{\"detail\":{}}}", json_string(FILES_REFUSED)).into_bytes(),
                ))
            }
        };
        let groups = build_groups(files)?;
        if groups.len() > MAX_GROUPS {
            return Err(Stop::Refused);
        }
        let probe = groups
            .iter()
            .find(|g| g.is_gguf && !g.projector)
            .and_then(|g| g.files.first())
            .map(|f| f.path.clone());
        let mut summary: Option<Arc<str>> = None;
        let mut gated = "";
        if let Some(path) = probe {
            summary = self.gguf_header(repo, &path).await?;
            if summary.is_none() {
                gated = lock(&self.headers).gated.get(repo).copied().unwrap_or("");
            }
        }
        let live = if groups.is_empty() {
            Vec::new()
        } else {
            self.repo_runtime(&groups, summary.as_deref()).await?
        };
        let mut out = format!("{{\"repo\":{},\"groups\":[", json_string(repo));
        for (i, (group, extra)) in groups.iter().zip(live).enumerate() {
            if i > 0 {
                out.push(',');
            }
            write_group(&mut out, group, &extra, summary.is_some());
        }
        out.push_str(&format!("],\"gated\":{}}}", json_string(gated)));
        Ok(out)
    }

    /// `hf.gguf_header(repo, path)`: the summary of the first MiB of a file, cached.
    async fn gguf_header(&self, repo: &str, path: &str) -> Result<Option<Arc<str>>, Stop> {
        let key = format!("{repo}/{path}");
        if let Some(cached) = lock(&self.headers).map.get(&key) {
            return Ok(cached.clone());
        }
        let Some((target, query)) =
            net::httpx_target(&format!("{}/{repo}/resolve/main", self.hub.resolve), path)
        else {
            // `httpx.InvalidURL` is not an `HTTPError`.
            return Err(Stop::Failed);
        };
        let url = match query {
            Some(query) => format!("{}{target}?{query}", origin_prefix(&self.hub.resolve)),
            None => format!("{}{target}", origin_prefix(&self.hub.resolve)),
        };
        // The request must be the one httpx builds: refuse a URL this side would rewrite.
        let parsed = reqwest::Url::parse(&url).map_err(|_| Stop::Refused)?;
        let wanted = match url
            .split_once("://")
            .and_then(|(_, rest)| rest.find('/').map(|i| rest.get(i..)))
        {
            Some(Some(rest)) => rest,
            _ => return Err(Stop::Refused),
        };
        let have = match parsed.query() {
            Some(q) => format!("{}?{q}", parsed.path()),
            None => parsed.path().to_owned(),
        };
        if have != wanted {
            return Err(Stop::Refused);
        }
        let (auth, epoch) = self.auth().await?;
        let header = match auth {
            Auth::None => None,
            Auth::Header(value) => Some(value),
            Auth::NotAscii | Auth::Refused => {
                self.store_header(epoch, key, None, Gate::Keep);
                return Ok(None);
            }
        };
        let mut request = self.header_client.get(parsed).header(
            reqwest::header::RANGE,
            format!("bytes=0-{}", HEADER_BYTES - 1),
        );
        if let Some(value) = header {
            request = request.header(reqwest::header::AUTHORIZATION, value);
        }
        let failed = |this: &Self| {
            this.store_header(epoch, key.clone(), None, Gate::Keep);
        };
        let Ok(response) = request.send().await else {
            failed(self);
            return Ok(None);
        };
        let status = response.status().as_u16();
        let mut body = Vec::new();
        if status == 200 || status == 206 {
            match net::read_prefix(response, HEADER_BYTES).await {
                Ok(read) => body = read,
                Err(_) => {
                    failed(self);
                    return Ok(None);
                }
            }
        }
        if status == 401 || status == 403 {
            // A gated repository: the listing is public, the weights are not.
            let reason = if status == 401 {
                "needs a Hugging Face token (set one in Settings)"
            } else {
                "your token lacks access \u{2014} accept this model's licence on its HF page"
            };
            self.store_header(epoch, key, None, Gate::Set(repo, reason));
            return Ok(None);
        }
        if (status != 200 && status != 206) || body.is_empty() {
            failed(self);
            return Ok(None);
        }
        let parsed = self
            .blocking(move || -> Result<Option<String>, Stop> {
                // `GgufMetaError` and `ValueError` are caught by `gguf_header`; a summary
                // error's class is not known here.
                let Ok(raw) = gguf::read_raw_bytes(&body) else {
                    return Ok(None);
                };
                let summary = gguf::summarize(&raw).map_err(|_| Stop::Refused)?;
                if has_nonfinite(&summary) {
                    return Err(Stop::Refused);
                }
                Ok(Some(summary.to_string()))
            })
            .await??;
        let Some(summary) = parsed else {
            failed(self);
            return Ok(None);
        };
        let summary: Arc<str> = Arc::from(summary);
        self.store_header(epoch, key, Some(summary.clone()), Gate::Lift(repo));
        Ok(Some(summary))
    }

    /// The child's fit chips and estimates for the groups.
    async fn repo_runtime(
        &self,
        groups: &[Group],
        summary: Option<&str>,
    ) -> Result<Vec<Live>, Stop> {
        let mut body = String::from("{\"groups\":[");
        for (i, group) in groups.iter().enumerate() {
            if group.bytes < 0 {
                return Err(Stop::Refused);
            }
            if i > 0 {
                body.push(',');
            }
            let estimate = summary.is_some() && group.is_gguf && !group.projector;
            body.push_str(&format!(
                "{{\"bytes\":{},\"estimate\":{estimate}}}",
                group.bytes
            ));
        }
        // `min(mm)` of the projector groups, as an integer (the child divides).
        let projectors: Vec<i128> = groups
            .iter()
            .filter(|g| g.projector)
            .map(|g| g.bytes)
            .collect();
        body.push_str("],\"mmprojBytes\":");
        match projectors.iter().min() {
            Some(least) => body.push_str(&least.to_string()),
            None => body.push_str("null"),
        }
        body.push_str(",\"summary\":");
        body.push_str(summary.unwrap_or("null"));
        body.push('}');
        let answer = self
            .child(
                self.legacy
                    .post(format!("{}{REPO_RUNTIME_PATH}", self.legacy_base))
                    .header(reqwest::header::CONTENT_TYPE, "application/json")
                    .body(body),
            )
            .await?;
        parse_repo_runtime(&answer, groups.len()).ok_or(Stop::Unavailable)
    }
}

enum AvatarError {
    /// Python's `owner_avatars` raised: the route uses `{}`.
    Raised,
    Stop(Stop),
}

impl From<Stop> for AvatarError {
    fn from(stop: Stop) -> Self {
        AvatarError::Stop(stop)
    }
}

/// `http://host:port` of a base URL.
fn origin_prefix(base: &str) -> String {
    match base.split_once("://") {
        Some((scheme, rest)) => {
            let host = rest.split('/').next().unwrap_or(rest);
            format!("{scheme}://{host}")
        }
        None => base.to_owned(),
    }
}

fn has_nonfinite(value: &gguf::Json) -> bool {
    match value {
        gguf::Json::Float(f) => !f.is_finite(),
        gguf::Json::Array(items) => items.iter().any(has_nonfinite),
        gguf::Json::Object(fields) => fields.iter().any(|(_, v)| has_nonfinite(v)),
        _ => false,
    }
}

/// A float as the route writes it. The routes return `-> dict`, so FastAPI serialises the answer
/// through pydantic first, which turns a non-finite float into `null`.
fn float(out: &mut String, value: f64) -> Result<(), Stop> {
    if value.is_finite() {
        out.push_str(&gguf::py_float_repr(value));
    } else {
        out.push_str("null");
    }
    Ok(())
}

fn optional_float(out: &mut String, value: Option<f64>) -> Result<(), Stop> {
    match value {
        Some(value) => float(out, value),
        None => {
            out.push_str("null");
            Ok(())
        }
    }
}

fn strings(out: &mut String, items: &[String]) {
    out.push('[');
    for (i, item) in items.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push_str(&json_string(item));
    }
    out.push(']');
}

fn write_option(out: &mut String, o: &Opt) -> Result<(), Stop> {
    out.push_str(&format!(
        "{{\"path\":{},\"bytes\":{},\"shards\":{},\"gb\":",
        json_string(&o.path),
        o.bytes,
        o.shards
    ));
    float(out, o.gb)?;
    out.push_str(",\"quant\":");
    out.push_str(&o.quant.as_deref().map_or("null".to_owned(), json_string));
    out.push_str(",\"params\":");
    optional_float(out, o.params)?;
    out.push_str(",\"activeParams\":");
    optional_float(out, o.active)?;
    out.push_str(&format!(",\"fits\":{},\"reasons\":", o.fits));
    strings(out, &o.reasons);
    out.push('}');
    Ok(())
}

/// One result: `{**judged, "avatar": ..., "downloaded": ...}`.
fn write_judged(out: &mut String, r: &Judged, avatar: &str, downloaded: &str) -> Result<(), Stop> {
    out.push_str(&format!(
        "{{\"id\":{},\"owner\":{},\"downloads\":{},\"likes\":{},\"lastModified\":{},\"ageDays\":{},\"license\":{},\"tags\":",
        json_string(&r.id),
        json_string(&r.owner),
        r.downloads,
        r.likes,
        json_string(&r.last_modified),
        r.age_days,
        r.license.as_deref().map_or("null".to_owned(), json_string),
    ));
    strings(out, &r.tags);
    out.push_str(",\"params\":");
    optional_float(out, r.params)?;
    out.push_str(",\"activeParams\":");
    optional_float(out, r.active)?;
    out.push_str(&format!(
        ",\"moe\":{},\"vision\":{},\"trusted\":{},\"options\":[",
        r.moe, r.vision, r.trusted
    ));
    for (i, o) in r.options.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        write_option(out, o)?;
    }
    out.push_str("],\"best\":");
    match &r.best {
        Some(best) => write_option(out, best)?,
        None => out.push_str("null"),
    }
    out.push_str(&format!(",\"suitable\":{},\"reasons\":", r.suitable));
    strings(out, &r.reasons);
    out.push_str(",\"score\":");
    float(out, r.score)?;
    out.push_str(",\"popularity\":");
    float(out, r.popularity)?;
    out.push_str(&format!(
        ",\"avatar\":{},\"downloaded\":{downloaded}}}",
        json_string(avatar)
    ));
    Ok(())
}

// ---- hub items -----------------------------------------------------------------------------

/// `[_model_from_item(i) for i in items]`.
fn models(items: &[Value]) -> Result<Vec<Model>, Stop> {
    items.iter().map(model_from_item).collect()
}

/// Python's `int(value)` for `value or 0`, held to what an `i64` carries below 2**62.
fn int_of(value: Option<&Value>) -> Result<i64, Stop> {
    const BOUND: i64 = 1 << 62;
    let Some(value) = value.filter(|v| v.truthy()) else {
        return Ok(0);
    };
    let number = match value {
        Value::Int(i) => i.to_string().parse::<i64>().map_err(|_| Stop::Refused)?,
        Value::Bool(_) => 1,
        Value::Float(f) if f.is_finite() => {
            let t = f.trunc();
            if t.abs() >= BOUND as f64 {
                return Err(Stop::Refused);
            }
            t as i64
        }
        // `int(nan)` and `int(inf)` raise.
        Value::Float(_) => return Err(Stop::Failed),
        Value::Str(s) => match py::parse_int(s) {
            Ok(Ok(n)) => n,
            Ok(Err(())) => return Err(Stop::Failed),
            Err(Unsupported) => return Err(Stop::Refused),
        },
        // `int([...])` and `int({...})` raise.
        Value::Arr(_) | Value::Obj(_) | Value::Null => return Err(Stop::Failed),
    };
    if number.abs() >= BOUND {
        return Err(Stop::Refused);
    }
    Ok(number)
}

/// Python's `str(value)` of a scalar JSON value.
fn str_of(value: &Value) -> Result<String, Stop> {
    Ok(match value {
        Value::Str(s) => s.clone(),
        Value::Int(i) => {
            let text = i.to_string();
            // `str(int)` raises past 4300 digits.
            if text.len() > 4300 {
                return Err(Stop::Failed);
            }
            text
        }
        Value::Bool(true) => "True".to_owned(),
        Value::Bool(false) => "False".to_owned(),
        Value::Null => "None".to_owned(),
        Value::Float(f) => gguf::py_float_repr(*f),
        Value::Arr(_) | Value::Obj(_) => return Err(Stop::Refused),
    })
}

/// `hf._model_from_item(item)`.
fn model_from_item(item: &Value) -> Result<Model, Stop> {
    // `gguf_count`: iterating `siblings` and calling `.get` on each entry.
    if let Some(siblings) = item.get("siblings").filter(|v| v.truthy()) {
        match siblings {
            Value::Arr(entries) if entries.iter().all(|e| matches!(e, Value::Obj(_))) => {}
            _ => return Err(Stop::Failed),
        }
    }
    let id = match [item.get("id"), item.get("modelId")]
        .into_iter()
        .flatten()
        .find(|v| v.truthy())
    {
        None => Id::Text(String::new()),
        Some(Value::Str(s)) => Id::Text(s.clone()),
        Some(_) => Id::Other,
    };
    let downloads = int_of(item.get("downloads"))?;
    let likes = int_of(item.get("likes"))?;
    let last_modified = match item.get("lastModified").filter(|v| v.truthy()) {
        None => String::new(),
        Some(Value::Arr(_) | Value::Obj(_)) => return Err(Stop::Refused),
        Some(value) => str_of(value)?,
    };
    let tags = match item.get("tags").filter(|v| v.truthy()) {
        None => Vec::new(),
        Some(Value::Arr(entries)) => entries
            .iter()
            .map(|t| match t {
                Value::Arr(_) | Value::Obj(_) => Ok(Tag::Opaque),
                other => str_of(other).map(Tag::Text),
            })
            .collect::<Result<_, Stop>>()?,
        // Iterating a str gives its characters, a dict its keys.
        Some(Value::Str(s)) => s.chars().map(|c| Tag::Text(c.to_string())).collect(),
        Some(Value::Obj(pairs)) => {
            let mut seen: Vec<&str> = Vec::new();
            for (key, _) in pairs {
                if !seen.contains(&key.as_str()) {
                    seen.push(key);
                }
            }
            seen.into_iter().map(|k| Tag::Text(k.to_owned())).collect()
        }
        // A number or `True` is not iterable.
        Some(_) => return Err(Stop::Failed),
    };
    Ok(Model {
        id,
        downloads,
        likes,
        last_modified,
        tags,
    })
}

/// `hf.looks_like_gguf(model)`.
fn looks_like_gguf(model: &Model) -> Result<bool, Stop> {
    let tagged = model.tags.iter().any(|t| match t {
        Tag::Text(t) => t.to_lowercase().starts_with("gguf"),
        Tag::Opaque => false,
    });
    if tagged {
        return Ok(true);
    }
    match &model.id {
        Id::Text(id) => Ok(id.to_lowercase().contains("gguf")),
        // `model.id.lower()` raises.
        Id::Other => Err(Stop::Failed),
    }
}

/// `[{"path": f.path, "size": f.size or 0} for f in detail.files if f.path.lower().endswith(".gguf")]`.
fn gguf_files(files: Vec<model_files::HfFile>) -> Result<Vec<File>, Stop> {
    const BOUND: i128 = 1 << 62;
    let mut out = Vec::new();
    for f in files {
        if !model_files::lower_ends_with(&f.path, ".gguf") {
            continue;
        }
        let size: i128 = f.size.to_string().parse().map_err(|_| Stop::Refused)?;
        if size.abs() >= BOUND {
            return Err(Stop::Refused);
        }
        out.push(File { path: f.path, size });
    }
    Ok(out)
}

// ---- the child's answers -------------------------------------------------------------------

struct Runtime {
    budget: f64,
    downloaded: HashMap<String, Box<RawValue>>,
    quantisers: Vec<String>,
    labs: Vec<String>,
}

fn parse_runtime(body: &[u8]) -> Option<Runtime> {
    let fields: HashMap<String, Box<RawValue>> = serde_json::from_slice(body).ok()?;
    let budget: String = serde_json::from_str(fields.get("budget")?.get()).ok()?;
    Some(Runtime {
        budget: budget.parse().ok()?,
        downloaded: serde_json::from_str(fields.get("downloaded")?.get()).ok()?,
        quantisers: serde_json::from_str(fields.get("quantisers")?.get()).ok()?,
        labs: serde_json::from_str(fields.get("labs")?.get()).ok()?,
    })
}

/// What the child adds to one group.
struct Live {
    fit: Box<RawValue>,
    estimates: Option<Box<RawValue>>,
    reason: String,
    native_ctx: Option<Box<RawValue>>,
}

fn parse_repo_runtime(body: &[u8], count: usize) -> Option<Vec<Live>> {
    let fields: HashMap<String, Box<RawValue>> = serde_json::from_slice(body).ok()?;
    let groups: Vec<HashMap<String, Box<RawValue>>> =
        serde_json::from_str(fields.get("groups")?.get()).ok()?;
    if groups.len() != count {
        return None;
    }
    groups
        .into_iter()
        .map(|mut g| {
            Some(Live {
                fit: g.remove("fit")?,
                estimates: g.remove("estimates"),
                reason: match g.remove("reason") {
                    Some(raw) => serde_json::from_str(raw.get()).ok()?,
                    None => String::new(),
                },
                native_ctx: g.remove("nativeCtx"),
            })
        })
        .collect()
}

// ---- GET /api/v1/search/repo: the file groups -----------------------------------------------

/// One entry of a group: `{"path", "bytes", "size", "quant"}`.
struct Member {
    path: String,
    bytes: i128,
    quant: Option<String>,
}

/// One `groups` entry before the live fields are added.
struct Group {
    shard_base: String,
    shards: Option<u32>,
    bytes: i128,
    quant: Option<String>,
    projector: bool,
    is_gguf: bool,
    files: Vec<Member>,
}

/// The groups of a listing, in `search_repo`'s order.
fn build_groups(files: Vec<model_files::HfFile>) -> Result<Vec<Group>, Stop> {
    const BOUND: i128 = 1 << 62;
    let mut order: Vec<String> = Vec::new();
    let mut by_base: HashMap<String, Vec<model_files::HfFile>> = HashMap::new();
    for f in files {
        if !by_base.contains_key(&f.shard_base) {
            order.push(f.shard_base.clone());
        }
        by_base.entry(f.shard_base.clone()).or_default().push(f);
    }
    let mut groups = Vec::new();
    for base in order {
        let Some(mut members) = by_base.remove(&base) else {
            continue;
        };
        // `files.sort(key=lambda x: (x.shard_index or 0, x.path))`, stable.
        members.sort_by(|a, b| {
            a.shard_index
                .unwrap_or(0)
                .cmp(&b.shard_index.unwrap_or(0))
                .then_with(|| a.path.cmp(&b.path))
        });
        let mut sized: Vec<(model_files::HfFile, i128)> = Vec::with_capacity(members.len());
        let mut total: i128 = 0;
        for m in members {
            let size: i128 = m.size.to_string().parse().map_err(|_| Stop::Refused)?;
            if size.abs() >= BOUND {
                return Err(Stop::Refused);
            }
            total += size;
            sized.push((m, size));
        }
        let Some((first, _)) = sized.first() else {
            continue;
        };
        let is_gguf = model_files::lower_ends_with(&first.path, ".gguf");
        let projector = first.path.to_lowercase().contains("mmproj");
        // Calibration data and other tiny strays are not models.
        if is_gguf && !projector && discover::is_stray_gguf(&base, total) {
            continue;
        }
        let shards = if first.shard_index.is_some_and(|i| i != 0) {
            first.shard_total
        } else {
            None
        };
        let quant = sized
            .iter()
            .find_map(|(m, _)| m.quant.clone().filter(|q| !q.is_empty()));
        groups.push(Group {
            shard_base: base,
            shards,
            bytes: total,
            quant,
            projector,
            is_gguf,
            files: sized
                .into_iter()
                .map(|(m, bytes)| Member {
                    path: m.path,
                    bytes,
                    quant: m.quant,
                })
                .collect(),
        });
    }
    // `(0 if gguf else 1, projector, shardBase.lower())`, stable.
    groups.sort_by_cached_key(|g| {
        (
            u8::from(!g.is_gguf),
            g.projector,
            g.shard_base.to_lowercase(),
        )
    });
    Ok(groups)
}

fn write_group(out: &mut String, g: &Group, live: &Live, with_summary: bool) {
    out.push_str(&format!(
        "{{\"shardBase\":{},\"shards\":{},\"bytes\":{},\"size\":{},\"quant\":{},\"fit\":{},\"projector\":{},\"files\":[",
        json_string(&g.shard_base),
        g.shards.map_or("null".to_owned(), |n| n.to_string()),
        g.bytes,
        json_string(&py::human_bytes(g.bytes)),
        g.quant.as_deref().map_or("null".to_owned(), json_string),
        live.fit.get(),
        g.projector,
    ));
    for (i, m) in g.files.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push_str(&format!(
            "{{\"path\":{},\"bytes\":{},\"size\":{},\"quant\":{}}}",
            json_string(&m.path),
            m.bytes,
            json_string(&py::human_bytes(m.bytes)),
            m.quant.as_deref().map_or("null".to_owned(), json_string),
        ));
    }
    out.push(']');
    if with_summary && g.is_gguf && !g.projector {
        if let Some(estimates) = &live.estimates {
            out.push_str(&format!(",\"estimates\":{}", estimates.get()));
        }
        if !live.reason.is_empty() {
            out.push_str(&format!(
                ",\"estimatesReason\":{}",
                json_string(&live.reason)
            ));
        }
        if let Some(ctx) = &live.native_ctx {
            out.push_str(&format!(",\"nativeCtx\":{}", ctx.get()));
        }
    }
    out.push('}');
}

#[cfg(test)]
mod bounds;
#[cfg(test)]
mod tests;
