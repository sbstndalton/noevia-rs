//! `GET /api/models/mtp-artifact?repo=&variant=`: whether the GGUF files of one download choice carry
//! MTP/NextN tensors, read from the published tensor directories (core routes/models.cjs, its
//! `modelManager.variants`: llamacpp-variants.cjs or the other engine's `/api/v1/pull/variants`, and
//! mtp-artifact.cjs). Any signed-in user may ask, as in Node (only writes under /api/models/ are
//! administrator-only). Node refuses the GET (core rust-mtp-artifact.cjs).
//!
//! Behaviour kept from Core:
//! - the repository is `owner/name` of `[A-Za-z0-9_.-]`, else 400 `Invalid repository`;
//! - the variant list failing in any way is 502 `Could not resolve selected files`; a list that
//!   names no such variant is 404 `Variant no longer available`; what makes Node's route throw (a
//!   transport failure of the list read, a null sibling or variant, a variant list that is not an
//!   array) is 500 `Internal error`;
//! - the check itself always answers 200: `present`, `absent` or `unknown` with a reason. It pins
//!   the repository's immutable revision, reads at most 16 MiB of each of up to four files, keeps
//!   at most 100 answers for five minutes (failures are never kept), coalesces identical checks and
//!   runs at most two at once.
//!
//! Replies are read with [`super::v8_json`], one parser that accepts exactly what `JSON.parse`
//! accepts (strings kept as UTF-16, lone surrogates included), and answers are written with its
//! `JSON.stringify`.
//!
//! Lifetime: the variant list is read on the request's own future, so a caller that leaves cancels
//! it. A coalesced inspection runs on its own task with a count of the callers waiting for it.
//! Joining and execution are separate: `pending` holds the joinable generation of each key (Node's
//! admission of at most two), and a task executes only while it owns one of two permits, released
//! when its future completes or is dropped. When the last caller leaves, the generation is removed
//! from `pending` (a newcomer starts a new one, which waits for a permit) and its abort is requested.
//! An aborted task that is still inside a poll may finish it, so publication is fenced: under the
//! same lock waiter departure takes, a task caches its answer only while its generation is still the
//! current one for the key. No lock is held across an await.
//!
//! The source is `https://huggingface.co`, or the QA source (`NOEVIA_QA_MODEL_CARD_SOURCE`) in
//! synthetic runs.
use super::v8_json::{self, W};
use super::Maint;
use axum::body::Body;
use axum::http::{HeaderValue, Response, StatusCode};
use js_json::JValue;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::watch;

pub const PATH: &str = "/api/models/mtp-artifact";
/// mtp-artifact.cjs `LIMIT`: the bytes read of each file.
pub const LIMIT: usize = 16 * 1024 * 1024;
const META_CAP: usize = 4 * 1024 * 1024;
/// http.cjs `DEFAULT_MAX_RESPONSE_BYTES`, the variant list's cap.
const LIST_CAP: usize = 8 * 1024 * 1024;
const CACHE_FOR: Duration = Duration::from_secs(300);
const CACHE_MAX: usize = 100;
const RUNNING_MAX: usize = 2;
const INCOMPLETE: &str = "The selected variant does not enumerate every GGUF shard.";

/// A check's answer (the object mtp-artifact.cjs `check` returns).
pub struct Answer {
    status: &'static str,
    reason: String,
    files: Arc<W>,
    found: Option<(W, Vec<Inspection>)>,
    checked_at: String,
}

impl Answer {
    /// `JSON.stringify(answer)`: status, reason, files, [revision, evidence,] checkedAt.
    pub fn render(&self) -> String {
        let mut out = String::from("{\"status\":");
        v8_json::quote(&units(self.status), &mut out);
        out.push_str(",\"reason\":");
        v8_json::quote(&units(&self.reason), &mut out);
        out.push_str(",\"files\":");
        v8_json::stringify(&self.files, &mut out);
        if let Some((revision, evidence)) = &self.found {
            out.push_str(",\"revision\":");
            v8_json::stringify(revision, &mut out);
            out.push_str(",\"evidence\":");
            let list = JValue::Arr(evidence.iter().map(Inspection::to_json).collect());
            out.push_str(&js_json::stringify(&list).unwrap_or_else(|| "[]".into()));
        }
        out.push_str(",\"checkedAt\":");
        v8_json::quote(&units(&self.checked_at), &mut out);
        out.push('}');
        out
    }
}

struct Entry {
    gen: u64,
    rx: watch::Receiver<Option<Arc<Answer>>>,
    waiters: usize,
    abort: tokio::task::AbortHandle,
}

type Pending = HashMap<String, Entry>;

/// The check's state (mtp-artifact.cjs module scope) and the variant list's clients.
pub struct Mtp {
    kind: server_stats::Kind,
    /// The other engine's base (`MODEL_MANAGER_BASE`, trailing slashes trimmed) and its key header.
    lemonade_base: String,
    lemonade_auth: Option<Result<HeaderValue, ()>>,
    /// `fetchJson`: `redirect: 'error'`.
    list_client: reqwest::Client,
    /// `fetch`: redirects followed (a resolve URL redirects to the file store).
    file_client: reqwest::Client,
    cache: Mutex<Vec<(String, Instant, Arc<Answer>)>>,
    pending: Arc<Mutex<Pending>>,
    next_gen: AtomicU64,
    /// Execution permits: at most two inspections run, cancelled ones included.
    permits: Arc<tokio::sync::Semaphore>,
    executing: Arc<AtomicUsize>,
    peak: Arc<AtomicUsize>,
    /// Test hook: block the inspection of one repository synchronously just before publication.
    pause: Mutex<Option<Pause>>,
}

type Pause = (
    String,
    std::sync::mpsc::Sender<()>,
    std::sync::mpsc::Receiver<()>,
);

/// One executing inspection (counted while the task owns a permit).
struct Executing {
    counter: Arc<AtomicUsize>,
}

impl Executing {
    fn start(counter: &Arc<AtomicUsize>, peak: &AtomicUsize) -> Executing {
        let now = counter.fetch_add(1, Ordering::AcqRel) + 1;
        peak.fetch_max(now, Ordering::AcqRel);
        Executing {
            counter: Arc::clone(counter),
        }
    }
}

impl Drop for Executing {
    fn drop(&mut self) {
        self.counter.fetch_sub(1, Ordering::AcqRel);
    }
}

impl std::fmt::Debug for Mtp {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Mtp")
            .field("kind", &self.kind)
            .finish_non_exhaustive()
    }
}

impl Mtp {
    pub fn new(kind: server_stats::Kind, raw_base: &str, key: Option<&str>) -> Result<Mtp, String> {
        let list_client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .no_proxy()
            .build()
            .map_err(|e| format!("artifact list client: {e}"))?;
        let file_client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::limited(20))
            .no_proxy()
            .build()
            .map_err(|e| format!("artifact client: {e}"))?;
        let lemonade_auth = key
            .filter(|k| !k.is_empty() && *k != "local")
            .map(|k| HeaderValue::from_str(&format!("Bearer {}", k.trim())).map_err(|_| ()));
        Ok(Mtp {
            kind,
            lemonade_base: raw_base.trim_end_matches('/').to_owned(),
            lemonade_auth,
            list_client,
            file_client,
            cache: Mutex::new(Vec::new()),
            pending: Arc::new(Mutex::new(HashMap::new())),
            next_gen: AtomicU64::new(1),
            permits: Arc::new(tokio::sync::Semaphore::new(RUNNING_MAX)),
            executing: Arc::new(AtomicUsize::new(0)),
            peak: Arc::new(AtomicUsize::new(0)),
            pause: Mutex::new(None),
        })
    }

    /// How many generations can be joined (Node's `pending.size`).
    pub fn running(&self) -> usize {
        self.pending.lock().map_or(0, |p| p.len())
    }
}

fn units(s: &str) -> Vec<u16> {
    s.encode_utf16().collect()
}

/// `url.searchParams.get(name)`.
fn param(uri: &axum::http::Uri, name: &str) -> Option<String> {
    let query = uri.query()?;
    url::form_urlencoded::parse(query.as_bytes())
        .find(|(k, _)| k == name)
        .map(|(_, v)| v.into_owned())
}

fn word(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

/// `/^[\w.-]+\/[\w.-]+$/`.
pub fn repo_ok(repo: &str) -> bool {
    let seg = |s: &str| !s.is_empty() && s.bytes().all(|b| word(b) || b == b'.' || b == b'-');
    match repo.split_once('/') {
        Some((a, b)) => seg(a) && seg(b),
        None => false,
    }
}

fn reply_text(status: u16, body: String) -> Response<Body> {
    crate::reply::json_text(
        StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
        body,
        true,
    )
}

fn error(status: u16, message: &str) -> Response<Body> {
    let mut out = String::from("{\"error\":");
    v8_json::quote(&units(message), &mut out);
    out.push('}');
    reply_text(status, out)
}

pub async fn serve(maint: &Arc<Maint>, uri: &axum::http::Uri) -> Response<Body> {
    let repo = param(uri, "repo").unwrap_or_default();
    if !repo_ok(&repo) {
        return error(400, "Invalid repository");
    }
    let variant: Option<Vec<u16>> = param(uri, "variant").map(|v| units(&v));
    // On this future: a caller that leaves cancels the list read.
    let mut list = match maint.mtp_variants(&repo).await {
        Err(Thrown) => return error(500, "Internal error"),
        Ok(None) => return error(502, "Could not resolve selected files"),
        Ok(Some(list)) => list,
    };
    // `(result.body?.variants || []).find(v => v.name === url.searchParams.get('variant'))`.
    let chosen = if !list.truthy() {
        None
    } else if let W::Arr(items) = &list {
        let mut at = None;
        for (k, v) in items.iter().enumerate() {
            if v.is_nullish() {
                return error(500, "Internal error");
            }
            let same = match (&variant, v.get("name")) {
                (None, W::Null) => true,
                (Some(want), W::Str(s)) => s == want,
                _ => false,
            };
            if same {
                at = Some(k);
                break;
            }
        }
        at
    } else {
        // `.find` of a string, number, boolean or object: a TypeError.
        return error(500, "Internal error");
    };
    let Some(at) = chosen else {
        return error(404, "Variant no longer available");
    };
    let files = match &mut list {
        W::Arr(items) => match items.get_mut(at) {
            Some(v) if v.get("files").truthy() => v.take("files"),
            Some(v) => W::Arr(vec![v.take("primary_file")]),
            None => return error(500, "Internal error"),
        },
        _ => return error(500, "Internal error"),
    };
    drop(list);
    let answer = maint.mtp_check(&repo, Arc::new(files)).await;
    reply_text(200, answer.render())
}

/// A JS exception escaping the route: index.cjs answers 500 `Internal error`.
#[derive(Debug)]
struct Thrown;

/// `fetchJson`'s answer: transport failure throws, an over-cap or failed reply is `{ ok: false }`
/// (`None`), a body that is not JSON is its text.
async fn fetch_json(
    client: &reqwest::Client,
    url: &str,
    auth: Option<&HeaderValue>,
    within: Duration,
) -> Result<Option<W>, Thrown> {
    let read = async {
        let mut req = client.get(url);
        if let Some(a) = auth {
            req = req.header(reqwest::header::AUTHORIZATION, a.clone());
        }
        let mut res = req.send().await.map_err(|_| Thrown)?;
        // `redirect: 'error'`: a redirect is a failed fetch.
        if res.status().is_redirection() {
            return Err(Thrown);
        }
        let ok = res.status().is_success();
        let mut bytes: Vec<u8> = Vec::new();
        while let Some(chunk) = res.chunk().await.map_err(|_| Thrown)? {
            if bytes.len() + chunk.len() > LIST_CAP {
                return Ok(None);
            }
            bytes.extend_from_slice(&chunk);
        }
        if !ok {
            return Ok(None);
        }
        let text = units(&String::from_utf8_lossy(&bytes));
        Ok(Some(match v8_json::parse(&text) {
            Ok(v) => v,
            Err(_) => W::Str(text),
        }))
    };
    tokio::time::timeout(within, read)
        .await
        .map_err(|_| Thrown)?
}

/// One caller waiting for a coalesced inspection: leaving takes it off the count, and the last
/// one to leave aborts the work and frees its running slot.
struct Waiting {
    pending: Arc<Mutex<Pending>>,
    key: String,
    gen: u64,
}

impl Drop for Waiting {
    fn drop(&mut self) {
        let Ok(mut pending) = self.pending.lock() else {
            return;
        };
        let last = match pending.get_mut(&self.key) {
            Some(e) if e.gen == self.gen => {
                e.waiters = e.waiters.saturating_sub(1);
                e.waiters == 0
            }
            _ => false,
        };
        if last {
            if let Some(e) = pending.remove(&self.key) {
                e.abort.abort();
            }
        }
    }
}

/// Held by the inspection task: its slot is freed however the task ends.
struct Release {
    pending: Arc<Mutex<Pending>>,
    key: String,
    gen: u64,
}

impl Drop for Release {
    fn drop(&mut self) {
        if let Ok(mut pending) = self.pending.lock() {
            if pending.get(&self.key).is_some_and(|e| e.gen == self.gen) {
                pending.remove(&self.key);
            }
        }
    }
}

impl Maint {
    /// How many MTP artifact inspections can be joined (at most two).
    pub fn mtp_running(&self) -> usize {
        self.mtp.running()
    }

    /// How many inspections are executing now (permit holders, cancelled ones included).
    pub fn mtp_executing(&self) -> usize {
        self.mtp.executing.load(Ordering::Acquire)
    }

    /// The most inspections ever executing at once.
    pub fn mtp_peak_executing(&self) -> usize {
        self.mtp.peak.load(Ordering::Acquire)
    }

    /// Test hook: the next inspection of `repo` signals the first channel just before it publishes
    /// its answer and then blocks its worker thread (an executing poll an abort cannot stop) until
    /// the second channel is signalled.
    #[doc(hidden)]
    pub fn mtp_pause_before_publish(
        &self,
        repo: &str,
    ) -> (std::sync::mpsc::Receiver<()>, std::sync::mpsc::Sender<()>) {
        let (reached_tx, reached_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        if let Ok(mut p) = self.mtp.pause.lock() {
            *p = Some((repo.to_owned(), reached_tx, release_rx));
        }
        (reached_rx, release_tx)
    }

    /// `modelManager.variants(repo)`: the `variants` of a successful answer (`Ok(None)` for any
    /// `{ ok: false }`), or a thrown error.
    async fn mtp_variants(&self, repo: &str) -> Result<Option<W>, Thrown> {
        let mtp = &self.mtp;
        match mtp.kind {
            server_stats::Kind::LlamaCpp => {
                let url = format!("{}/api/models/{repo}?blobs=true", self.card_base);
                let Some(body) =
                    fetch_json(&mtp.list_client, &url, None, Duration::from_secs(20)).await?
                else {
                    return Ok(None);
                };
                variants_of(&body)
            }
            server_stats::Kind::Lemonade => {
                let auth = match &mtp.lemonade_auth {
                    Some(Err(())) => return Err(Thrown),
                    Some(Ok(h)) => Some(h),
                    None => None,
                };
                let url = format!(
                    "{}/api/v1/pull/variants?checkpoint={}",
                    mtp.lemonade_base,
                    encode_component(repo)
                );
                let body =
                    fetch_json(&mtp.list_client, &url, auth, Duration::from_secs(20)).await?;
                Ok(body.map(|mut b| b.take("variants")))
            }
            // model-manager.cjs `requireEnabled` throws.
            server_stats::Kind::None => Err(Thrown),
        }
    }

    /// mtp-artifact.cjs `check(repo, files)`.
    async fn mtp_check(self: &Arc<Self>, repo: &str, files: Arc<W>) -> Arc<Answer> {
        if let Err(reason) = validate(repo, &files) {
            return Arc::new(unknown(reason, files));
        }
        let key = {
            let mut k = String::from("[");
            v8_json::quote(&units(repo), &mut k);
            k.push(',');
            v8_json::stringify(&files, &mut k);
            k.push(']');
            k
        };
        if let Some(saved) = self.mtp.cache.lock().ok().and_then(|c| {
            c.iter()
                .find(|(k, at, _)| *k == key && at.elapsed() < CACHE_FOR)
                .map(|(_, _, a)| Arc::clone(a))
        }) {
            return saved;
        }
        let (mut rx, _waiting) = {
            let Ok(mut pending) = self.mtp.pending.lock() else {
                return Arc::new(unknown("Internal error".into(), files));
            };
            if let Some(e) = pending.get_mut(&key) {
                e.waiters += 1;
                let w = Waiting {
                    pending: Arc::clone(&self.mtp.pending),
                    key: key.clone(),
                    gen: e.gen,
                };
                (e.rx.clone(), w)
            } else if pending.len() >= RUNNING_MAX {
                return Arc::new(unknown(
                    "Two artifact checks are already running. Try again shortly.".into(),
                    files,
                ));
            } else {
                let gen = self.mtp.next_gen.fetch_add(1, Ordering::Relaxed);
                let (tx, rx) = watch::channel(None);
                let me = Arc::clone(self);
                let release = Release {
                    pending: Arc::clone(&self.mtp.pending),
                    key: key.clone(),
                    gen,
                };
                let (repo, run_files, run_key) = (repo.to_owned(), Arc::clone(&files), key.clone());
                let permits = Arc::clone(&self.mtp.permits);
                let task = tokio::spawn(async move {
                    let _release = release;
                    // Owned by this future: released when it completes or is dropped.
                    let Ok(_permit) = permits.acquire_owned().await else {
                        return;
                    };
                    let _executing = Executing::start(&me.mtp.executing, &me.mtp.peak);
                    let result = me.mtp_run(&repo, run_files, &run_key, gen).await;
                    let _ = tx.send(Some(result));
                });
                pending.insert(
                    key.clone(),
                    Entry {
                        gen,
                        rx: rx.clone(),
                        waiters: 1,
                        abort: task.abort_handle(),
                    },
                );
                let w = Waiting {
                    pending: Arc::clone(&self.mtp.pending),
                    key: key.clone(),
                    gen,
                };
                (rx, w)
            }
        };
        let done = rx.wait_for(Option::is_some).await.map(|v| v.clone());
        match done {
            Ok(Some(answer)) => answer,
            _ => Arc::new(unknown("Internal error".into(), files)),
        }
    }

    async fn mtp_run(&self, repo: &str, files: Arc<W>, key: &str, gen: u64) -> Arc<Answer> {
        let names: Vec<Vec<u16>> = match &*files {
            W::Arr(items) => items
                .iter()
                .filter_map(|f| f.units().map(<[u16]>::to_vec))
                .collect(),
            _ => Vec::new(),
        };
        match self.mtp_evidence(repo, &names).await {
            // A thrown failure answers unknown and is never kept (Node returns before the cache).
            Err(reason) => Arc::new(unknown(reason, files)),
            Ok((revision, evidence)) => {
                let result = Arc::new(verdict(files, revision, evidence));
                self.pause_before_publish(repo);
                // The fence: publish only while this generation is still the current one for the
                // key, decided under the lock waiter departure takes (a cancelled or superseded
                // generation drops its answer).
                if let Ok(pending) = self.mtp.pending.lock() {
                    if pending.get(key).is_some_and(|e| e.gen == gen) {
                        if let Ok(mut cache) = self.mtp.cache.lock() {
                            if cache.len() >= CACHE_MAX {
                                cache.remove(0);
                            }
                            if let Some(slot) = cache.iter_mut().find(|(k, _, _)| k == key) {
                                slot.1 = Instant::now();
                                slot.2 = Arc::clone(&result);
                            } else {
                                cache.push((key.to_owned(), Instant::now(), Arc::clone(&result)));
                            }
                        }
                    }
                }
                result
            }
        }
    }

    /// The test hook ([`Maint::mtp_pause_before_publish`]); a no-op unless a test armed it.
    fn pause_before_publish(&self, repo: &str) {
        let armed = self.mtp.pause.lock().ok().and_then(|mut p| {
            if p.as_ref().is_some_and(|(r, _, _)| r == repo) {
                p.take()
            } else {
                None
            }
        });
        if let Some((_, reached, release)) = armed {
            let _ = reached.send(());
            let _ = release.recv();
        }
    }

    async fn mtp_evidence(
        &self,
        repo: &str,
        files: &[Vec<u16>],
    ) -> Result<(W, Vec<Inspection>), String> {
        let client = &self.mtp.file_client;
        let meta_url = format!("{}/api/models/{repo}", self.card_base);
        let mut meta = tokio::time::timeout(Duration::from_secs(10), async {
            let mut res = client
                .get(&meta_url)
                .send()
                .await
                .map_err(|_| "fetch failed".to_string())?;
            if !res.status().is_success() {
                return Err("Public repository revision unavailable.".to_string());
            }
            let mut bytes: Vec<u8> = Vec::new();
            while let Some(chunk) = res.chunk().await.map_err(|_| "terminated".to_string())? {
                if bytes.len() + chunk.len() > META_CAP {
                    return Err(format!("reply exceeded the {} KB limit", META_CAP / 1024));
                }
                bytes.extend_from_slice(&chunk);
            }
            // JSON.parse; its own message on failure (a lone surrogate in it becomes U+FFFD here).
            v8_json::parse(&units(&String::from_utf8_lossy(&bytes)))
                .map_err(|m| String::from_utf16_lossy(&m))
        })
        .await
        .map_err(|_| "Artifact check timed out.".to_string())??;
        // `meta.sha`, then `/^[a-f0-9]{40,64}$/.test(revision || '')`: the test coerces the value to
        // a string, and the URL and the answer use the value as it is (`${revision}`, `revision`).
        let sha = match &meta {
            W::Null => return Err("Cannot read properties of null (reading 'sha')".into()),
            W::Obj(_) => meta.take("sha"),
            _ => W::Undefined,
        };
        drop(meta);
        let text = if sha.truthy() {
            v8_json::to_js_string(&sha)?
        } else {
            Vec::new()
        };
        if !((40..=64).contains(&text.len())
            && text.iter().all(|&u| matches!(u, 0x61..=0x66 | 0x30..=0x39)))
        {
            return Err("Immutable repository revision unavailable.".to_string());
        }
        let revision = String::from_utf16_lossy(&text);
        let mut evidence = Vec::new();
        for file in files {
            let mut path: Vec<String> = Vec::new();
            for seg in file.split(|&u| u == 0x2f) {
                path.push(encode_units(seg).ok_or_else(|| "URI malformed".to_string())?);
            }
            let url = format!(
                "{}/{repo}/resolve/{revision}/{}",
                self.card_base,
                path.join("/")
            );
            let bytes = prefix(client, &url).await?;
            let mut found = inspect(&bytes)?;
            found.file = String::from_utf16_lossy(file);
            evidence.push(found);
        }
        let count = files.len() as f64;
        if evidence.iter().any(|e| js_gt(&e.split_count, count)) {
            return Err(INCOMPLETE.into());
        }
        Ok((sha, evidence))
    }
}

/// llamacpp-variants.cjs over the source's model info: the variant rows, `None` (`{ ok: false }`),
/// or a thrown error (a nullish sibling: `file.rfilename` of null).
fn variants_of(body: &W) -> Result<Option<W>, Thrown> {
    if body.get("private").truthy() {
        return Ok(None);
    }
    let W::Arr(siblings) = body.get("siblings") else {
        return Ok(None);
    };
    struct File {
        name: Vec<u16>,
        size: Option<f64>,
        shard: Option<String>,
        count: Option<String>,
    }
    let mut groups: Vec<(String, Vec<File>)> = Vec::new();
    for file in siblings {
        if file.is_nullish() {
            return Err(Thrown);
        }
        let Some(name) = file.get("rfilename").units() else {
            continue;
        };
        // The pattern and the projector test are ASCII: a lone surrogate reads as any other
        // non-matching character.
        let text = String::from_utf16_lossy(name);
        if text.to_ascii_lowercase().contains("mmproj") {
            continue;
        }
        let Some((quant, shard, count)) = quant_match(&text) else {
            continue;
        };
        let quant = quant.to_ascii_uppercase();
        // `file.lfs?.size ?? file.size`
        let lfs = file.get("lfs");
        let lfs_size = if lfs.is_nullish() {
            &W::Undefined
        } else {
            lfs.get("size")
        };
        let size = if lfs_size.is_nullish() {
            file.get("size")
        } else {
            lfs_size
        };
        let size = match size {
            W::Num(n) if n.is_finite() && *n > 0.0 => Some(*n),
            _ => None,
        };
        let entry = File {
            name: name.to_vec(),
            size,
            shard,
            count,
        };
        match groups.iter_mut().find(|(q, _)| *q == quant) {
            Some((_, g)) => g.push(entry),
            None => groups.push((quant, vec![entry])),
        }
    }
    let mut rows: Vec<(String, W)> = Vec::new();
    for (name, files) in &groups {
        let Some(first) = files.first() else { continue };
        let count = first
            .count
            .as_deref()
            .map_or(f64::NAN, |c| c.parse::<f64>().unwrap_or(f64::NAN));
        let shard_no = |f: &File| {
            f.shard
                .as_deref()
                .map_or(f64::NAN, |s| s.parse::<f64>().unwrap_or(f64::NAN))
        };
        let distinct = {
            let mut seen: Vec<&Option<String>> = Vec::new();
            for f in files {
                if !seen.contains(&&f.shard) {
                    seen.push(&f.shard);
                }
            }
            seen.len()
        };
        let complete = first.shard.is_some()
            && count == files.len() as f64
            && distinct as f64 == count
            && files.iter().all(|f| {
                f.count == first.count
                    && stem(&f.name) == stem(&first.name)
                    && shard_no(f) >= 1.0
                    && shard_no(f) <= count
            });
        if (files.len() != 1 && !complete) || (first.shard.is_some() && !complete) {
            continue;
        }
        let size = if files.iter().all(|f| f.size.is_some()) {
            W::Num(files.iter().filter_map(|f| f.size).sum())
        } else {
            W::Null
        };
        rows.push((
            name.clone(),
            W::Obj(vec![
                (units("name"), W::str(name)),
                (units("primary_file"), W::Str(first.name.clone())),
                (
                    units("files"),
                    W::Arr(files.iter().map(|f| W::Str(f.name.clone())).collect()),
                ),
                (units("size_bytes"), size),
            ]),
        ));
    }
    rows.sort_by_key(|r| collate(&r.0));
    Ok(Some(W::Arr(rows.into_iter().map(|(_, r)| r).collect())))
}

/// `localeCompare` for quant names (`[A-Z0-9_]`, upper-cased): the root collation puts `_` before
/// digits before letters.
fn collate(s: &str) -> Vec<u16> {
    s.bytes()
        .map(|b| match b {
            b'_' => 1,
            b'0'..=b'9' => 10 + u16::from(b - b'0'),
            b'A'..=b'Z' => 100 + u16::from(b - b'A'),
            other => 1000 + u16::from(other),
        })
        .collect()
}

fn ci(b: &[u8], at: usize, lit: &str) -> bool {
    b.get(at..at + lit.len())
        .is_some_and(|s| s.eq_ignore_ascii_case(lit.as_bytes()))
}

/// `-(\d{5})-of-(\d{5})` then `\.gguf$`, or `\.gguf$`, from `at`.
fn tail(b: &[u8], at: usize) -> Option<(Option<String>, Option<String>)> {
    let digits = |from: usize| {
        b.get(from..from + 5)
            .filter(|s| s.iter().all(u8::is_ascii_digit))
            .map(|s| String::from_utf8_lossy(s).into_owned())
    };
    if b.get(at) == Some(&b'-') {
        if let (Some(shard), true, Some(count)) =
            (digits(at + 1), ci(b, at + 6, "-of-"), digits(at + 10))
        {
            if ci(b, at + 15, ".gguf") && at + 20 == b.len() {
                return Some((Some(shard), Some(count)));
            }
        }
    }
    (ci(b, at, ".gguf") && at + 5 == b.len()).then_some((None, None))
}

/// `/(?:^|[.\/_-])((?:IQ|Q)\d[\w]*|BF16|F16|F32)(?:-(\d{5})-of-(\d{5}))?\.gguf$/i`: the quant,
/// shard and count of the leftmost match.
pub fn quant_match(name: &str) -> Option<(String, Option<String>, Option<String>)> {
    let b = name.as_bytes();
    let at = |s: usize| -> Option<(String, Option<String>, Option<String>)> {
        let mut ends = Vec::new();
        let q = if ci(b, s, "iq") {
            Some(s + 2)
        } else if ci(b, s, "q") {
            Some(s + 1)
        } else {
            None
        };
        if let Some(i) = q.filter(|&i| b.get(i).is_some_and(u8::is_ascii_digit)) {
            let mut j = i + 1;
            while b.get(j).is_some_and(|&c| word(c)) {
                j += 1;
            }
            ends.push(j);
        }
        for lit in ["bf16", "f16", "f32"] {
            if ci(b, s, lit) {
                ends.push(s + lit.len());
            }
        }
        ends.into_iter().find_map(|e| {
            tail(b, e).map(|(shard, count)| {
                (
                    String::from_utf8_lossy(b.get(s..e).unwrap_or_default()).into_owned(),
                    shard,
                    count,
                )
            })
        })
    };
    for p in 0..=b.len() {
        if p == 0 {
            if let Some(m) = at(0) {
                return Some(m);
            }
        }
        if b.get(p)
            .is_some_and(|c| matches!(c, b'.' | b'/' | b'_' | b'-'))
        {
            if let Some(m) = at(p + 1) {
                return Some(m);
            }
        }
    }
    None
}

/// `name.replace(/-\d{5}-of-\d{5}\.gguf$/i, '')`, on UTF-16 units (the suffix is ASCII).
fn stem(name: &[u16]) -> &[u16] {
    if name.len() >= 20 {
        let at = name.len() - 20;
        let tail_bytes: Option<Vec<u8>> = name
            .get(at..)
            .unwrap_or_default()
            .iter()
            .map(|&u| u8::try_from(u).ok().filter(u8::is_ascii))
            .collect();
        if let Some(b) = tail_bytes {
            if matches!(tail(&b, 0), Some((Some(_), _))) {
                return name.get(..at).unwrap_or(name);
            }
        }
    }
    name
}

/// `encodeURIComponent`.
pub fn encode_component(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"-_.!~*'()".contains(&b) {
            out.push(char::from(b));
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// `encodeURIComponent` of UTF-16 units: `None` for a lone surrogate (JS throws `URIError: URI malformed`).
fn encode_units(s: &[u16]) -> Option<String> {
    let mut text = String::new();
    for c in char::decode_utf16(s.iter().copied()) {
        text.push(c.ok()?);
    }
    Some(encode_component(&text))
}

/// `new Set(files).size !== files.length`: SameValueZero, objects and arrays by identity.
fn same_value_zero(a: &W, b: &W) -> bool {
    match (a, b) {
        (W::Undefined, W::Undefined) | (W::Null, W::Null) => true,
        (W::Bool(x), W::Bool(y)) => x == y,
        (W::Num(x), W::Num(y)) => x == y || (x.is_nan() && y.is_nan()),
        (W::Str(x), W::Str(y)) => x == y,
        _ => false,
    }
}

/// mtp-artifact.cjs `validate`.
pub fn validate(repo: &str, files: &W) -> Result<(), String> {
    if !repo_ok(repo) || repo.split('/').any(|p| p == "." || p == "..") {
        return Err("Invalid public Hugging Face repository.".into());
    }
    let bad = || Err("Inspection supports up to four selected GGUF shards.".to_string());
    let W::Arr(items) = files else {
        return bad();
    };
    if items.is_empty() || items.len() > 4 {
        return bad();
    }
    for (k, a) in items.iter().enumerate() {
        if items.iter().skip(k + 1).any(|b| same_value_zero(a, b)) {
            return bad();
        }
    }
    let gguf = units(".gguf");
    for f in items {
        let Some(f) = f.units() else { return bad() };
        let segment_bad = |p: &[u16]| p.is_empty() || p == [0x2e] || p == [0x2e, 0x2e];
        if !f.ends_with(&gguf)
            || f.first() == Some(&0x2f)
            || f.split(|&u| u == 0x2f).any(segment_bad)
            || f.iter()
                .any(|&u| matches!(u, 0x3f | 0x23 | 0x5c | 0x00..=0x1f))
        {
            return bad();
        }
    }
    Ok(())
}

fn now_iso() -> String {
    let ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(0));
    crate::project_config_routes::iso(ms)
}

fn unknown(reason: String, files: Arc<W>) -> Answer {
    Answer {
        status: "unknown",
        reason,
        files,
        found: None,
        checked_at: now_iso(),
    }
}

/// JS `x > n` for a metadata value (a number, a string, `null`): a string by ECMAScript `ToNumber`.
fn js_gt(x: &JValue, n: f64) -> bool {
    match x {
        JValue::Num(v) => *v > n,
        JValue::Str(s) => v8_json::to_number(s) > n,
        JValue::Bool(b) => f64::from(u8::from(*b)) > n,
        JValue::Null => 0.0 > n,
        _ => false,
    }
}

/// The cacheable verdict over complete evidence (the incomplete-shard failure is raised before).
fn verdict(files: Arc<W>, revision: W, evidence: Vec<Inspection>) -> Answer {
    let found = evidence.iter().any(|e| !e.tensors.is_empty());
    let inconsistent = !found && evidence.iter().any(|e| js_gt(&e.declared_layers, 0.0));
    let (status, reason) = if found {
        ("present", "MTP/NextN tensors are present in the selected files. Backend compatibility and a working head still require a load test.")
    } else if inconsistent {
        (
            "unknown",
            "Metadata declares MTP layers, but no recognized MTP tensors were found.",
        )
    } else {
        ("absent", "No recognized MTP/NextN tensors in the complete selected tensor directories. Separate head files are not included in this check.")
    };
    Answer {
        status,
        reason: reason.into(),
        files,
        found: Some((revision, evidence)),
        checked_at: now_iso(),
    }
}

/// mtp-artifact.cjs `prefix(url)`: at most [`LIMIT`] bytes from the start of the file, in 20 s.
async fn prefix(client: &reqwest::Client, url: &str) -> Result<Vec<u8>, String> {
    let read = async {
        let mut res = client
            .get(url)
            .header(reqwest::header::RANGE, format!("bytes=0-{}", LIMIT - 1))
            .send()
            .await
            .map_err(|_| "fetch failed".to_string())?;
        let status = res.status().as_u16();
        if !res.status().is_success() || matches!(status, 204 | 205) {
            return Err(format!("Artifact metadata unavailable ({status})."));
        }
        if status == 206 {
            let range = res
                .headers()
                .get(reqwest::header::CONTENT_RANGE)
                .and_then(|v| v.to_str().ok())
                .unwrap_or("");
            if !range
                .get(..8)
                .is_some_and(|p| p.eq_ignore_ascii_case("bytes 0-"))
            {
                return Err("Unexpected artifact byte range.".into());
            }
        }
        let mut out: Vec<u8> = Vec::new();
        while out.len() < LIMIT {
            match res.chunk().await.map_err(|_| "terminated".to_string())? {
                None => break,
                Some(part) => {
                    let room = LIMIT - out.len();
                    out.extend_from_slice(part.get(..part.len().min(room)).unwrap_or_default());
                }
            }
        }
        Ok(out)
    };
    tokio::time::timeout(Duration::from_secs(20), read)
        .await
        .map_err(|_| "Artifact check timed out.".to_string())?
}

/// One file's tensor directory, as `inspect` returns it (and `file`).
#[derive(Debug, Clone, PartialEq)]
pub struct Inspection {
    pub file: String,
    pub architecture: JValue,
    pub declared_layers: JValue,
    pub split_count: JValue,
    pub tensor_count: f64,
    pub tensors: Vec<String>,
}

impl Inspection {
    /// `{ file, ...inspect(buffer) }`.
    pub fn to_json(&self) -> JValue {
        JValue::obj([
            ("file", JValue::Str(self.file.clone())),
            ("architecture", self.architecture.clone()),
            ("declaredLayers", self.declared_layers.clone()),
            ("splitCount", self.split_count.clone()),
            ("tensorCount", JValue::Num(self.tensor_count)),
            (
                "tensors",
                JValue::Arr(self.tensors.iter().cloned().map(JValue::Str).collect()),
            ),
        ])
    }

    /// `inspect(buffer)` alone (no `file`), for the differential test.
    pub fn inspect_json(&self) -> JValue {
        match self.to_json() {
            JValue::Obj(mut pairs) => {
                pairs.remove(0);
                JValue::Obj(pairs)
            }
            other => other,
        }
    }
}

struct Reader<'a> {
    b: &'a [u8],
    at: usize,
}

const TRUNCATED: &str = "GGUF header exceeds inspection limit or is truncated.";

impl Reader<'_> {
    fn need(&self, n: u64) -> Result<usize, String> {
        let n = usize::try_from(n).map_err(|_| TRUNCATED.to_string())?;
        if (n as u64) > (1u64 << 53) - 1 || self.at.checked_add(n).is_none_or(|e| e > self.b.len())
        {
            return Err(TRUNCATED.into());
        }
        Ok(n)
    }
    fn take(&mut self, n: u64) -> Result<&[u8], String> {
        let n = self.need(n)?;
        let s = self
            .b
            .get(self.at..self.at + n)
            .ok_or_else(|| TRUNCATED.to_string())?;
        self.at += n;
        Ok(s)
    }
    fn arr<const N: usize>(&mut self) -> Result<[u8; N], String> {
        let s = self.take(N as u64)?;
        <[u8; N]>::try_from(s).map_err(|_| TRUNCATED.to_string())
    }
    fn u32(&mut self) -> Result<u32, String> {
        Ok(u32::from_le_bytes(self.arr::<4>()?))
    }
    fn raw_u64(&mut self) -> Result<u64, String> {
        Ok(u64::from_le_bytes(self.arr::<8>()?))
    }
    fn u64(&mut self) -> Result<u64, String> {
        let v = self.raw_u64()?;
        if v > (1u64 << 53) - 1 {
            return Err("GGUF count exceeds safe limits.".into());
        }
        Ok(v)
    }
    fn str(&mut self, keep: bool) -> Result<Option<String>, String> {
        let n = self.u64()?;
        let s = self.take(n)?;
        Ok(keep.then(|| String::from_utf8_lossy(s).into_owned()))
    }
    fn value(&mut self, kind: u32, keep: bool, depth: u32) -> Result<JValue, String> {
        if depth > 2 {
            return Err("Unsupported nested GGUF metadata.".into());
        }
        if kind == 8 {
            return Ok(self.str(keep)?.map_or(JValue::Null, JValue::Str));
        }
        if kind == 9 {
            let t = self.u32()?;
            let n = self.u64()?;
            if n > 2_000_000 {
                return Err("GGUF array exceeds inspection limit.".into());
            }
            for _ in 0..n {
                self.value(t, false, depth + 1)?;
            }
            return Ok(JValue::Null);
        }
        let size = match kind {
            0 | 1 | 7 => 1,
            2 | 3 => 2,
            4..=6 => 4,
            10..=12 => 8,
            _ => return Err("Unsupported GGUF value type.".into()),
        };
        let s = self.take(size)?;
        let mut a = [0u8; 8];
        for (slot, b) in a.iter_mut().zip(s) {
            *slot = *b;
        }
        let [b0, b1, b2, b3, ..] = a;
        Ok(if !keep {
            JValue::Null
        } else {
            match kind {
                0 => JValue::Num(f64::from(b0)),
                2 => JValue::Num(f64::from(u16::from_le_bytes([b0, b1]))),
                4 => JValue::Num(f64::from(u32::from_le_bytes([b0, b1, b2, b3]))),
                5 => JValue::Num(f64::from(i32::from_le_bytes([b0, b1, b2, b3]))),
                10 => JValue::Num(u64::from_le_bytes(a) as f64),
                _ => JValue::Null,
            }
        })
    }
}

/// `/(?:^|\.)(?:nextn|mtp)(?:\.|_)/i`.
fn mtp_tensor(name: &str) -> bool {
    let b = name.as_bytes();
    (0..b.len()).any(|p| {
        let start_ok = p == 0 || b.get(p - 1) == Some(&b'.');
        start_ok
            && ["nextn", "mtp"].iter().any(|w| {
                ci(b, p, w) && b.get(p + w.len()).is_some_and(|c| matches!(c, b'.' | b'_'))
            })
    })
}

/// mtp-artifact.cjs `inspect(buffer)`: the GGUF v2/v3 key/value and tensor directories.
pub fn inspect(buffer: &[u8]) -> Result<Inspection, String> {
    let mut r = Reader { b: buffer, at: 0 };
    r.need(24)?;
    // `buffer.toString('ascii', 0, 4)` drops the high bit.
    let magic: Vec<u8> = buffer.iter().take(4).map(|b| b & 0x7f).collect();
    if magic != b"GGUF" {
        return Err("Not a supported little-endian GGUF file.".into());
    }
    r.at = 4;
    if !matches!(r.u32()?, 2 | 3) {
        return Err("Unsupported GGUF version.".into());
    }
    let count = r.u64()?;
    let kv = r.u64()?;
    if count > 200_000 || kv > 100_000 {
        return Err("GGUF directory exceeds inspection limit.".into());
    }
    let mut architecture = JValue::Str(String::new());
    let mut declared_layers = JValue::Null;
    let mut split_count = JValue::Num(1.0);
    for _ in 0..kv {
        let key = r.str(true)?.unwrap_or_default();
        let kind = r.u32()?;
        let keep = key == "general.architecture"
            || key.ends_with(".nextn_predict_layers")
            || key == "split.count";
        let v = r.value(kind, keep, 0)?;
        if key == "general.architecture" {
            architecture = v.clone();
        }
        if key.ends_with(".nextn_predict_layers") {
            declared_layers = v.clone();
        }
        if key == "split.count" {
            split_count = v;
        }
    }
    let mut tensors = Vec::new();
    for _ in 0..count {
        let name = r.str(true)?.unwrap_or_default();
        let dims = r.u32()?;
        if dims > 8 {
            return Err("Unsupported GGUF tensor dimensions.".into());
        }
        for _ in 0..dims {
            r.u64()?;
        }
        r.u32()?;
        r.u64()?;
        if mtp_tensor(&name) && tensors.len() < 8 {
            tensors.push(name);
        }
    }
    Ok(Inspection {
        file: String::new(),
        architecture,
        declared_layers,
        split_count,
        tensor_count: count as f64,
        tensors,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quant_names_match_like_the_js_pattern() {
        assert_eq!(
            quant_match("m-Q4_K_M.gguf").map(|m| m.0),
            Some("Q4_K_M".into())
        );
        assert_eq!(
            quant_match("IQ4_XS.gguf").map(|m| m.0),
            Some("IQ4_XS".into())
        );
        assert_eq!(
            quant_match("x/m.bf16.GGUF").map(|m| m.0),
            Some("bf16".into())
        );
        assert_eq!(
            quant_match("m-Q8_0-00001-of-00002.gguf"),
            Some(("Q8_0".into(), Some("00001".into()), Some("00002".into())))
        );
        assert_eq!(quant_match("modelQ4.gguf"), None);
        assert_eq!(quant_match("m-Q4.bin"), None);
        assert_eq!(
            stem(&units("m-Q8_0-00001-of-00002.gguf")),
            units("m-Q8_0").as_slice()
        );
    }

    #[test]
    fn collation_puts_underscore_before_digits_before_letters() {
        let mut v = vec!["Q40", "Q4_K", "BF16", "IQ4", "Q4A"];
        v.sort_by_key(|s| collate(s));
        assert_eq!(v, ["BF16", "IQ4", "Q4_K", "Q40", "Q4A"]);
    }

    #[test]
    fn tensor_names_match_like_the_js_pattern() {
        assert!(mtp_tensor("blk.32.nextn.eh_proj.weight"));
        assert!(mtp_tensor("mtp_head.weight"));
        assert!(mtp_tensor("x.MTP.y"));
        assert!(!mtp_tensor("blk.1.amtp.x"));
        assert!(!mtp_tensor("token_embd.weight"));
    }

    #[test]
    fn encode_component_is_js() {
        assert_eq!(encode_component("a b/c~!*'()é"), "a%20b%2Fc~!*'()%C3%A9");
    }
}
