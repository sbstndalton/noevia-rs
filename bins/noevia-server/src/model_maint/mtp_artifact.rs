//! `GET /api/models/mtp-artifact?repo=&variant=`: whether the GGUF files of one download choice carry
//! MTP/NextN tensors, read from the published tensor directories (core routes/models.cjs, its
//! `modelManager.variants`: llamacpp-variants.cjs or the other engine's `/api/v1/pull/variants`, and
//! mtp-artifact.cjs). Any signed-in user may ask, as in Node (only writes under /api/models/ are
//! administrator-only). Node refuses the GET (core rust-mtp-artifact.cjs).
//!
//! Behaviour kept from Core:
//! - the repository is `owner/name` of `[A-Za-z0-9_.-]`, else 400 `Invalid repository`;
//! - the variant list failing in any way is 502 `Could not resolve selected files`; a list that
//!   names no such variant is 404 `Variant no longer available`; a transport failure of the list
//!   read (Node's `fetchJson` throws) is 500 `Internal error`;
//! - the check itself always answers 200: `present`, `absent` or `unknown` with a reason. It pins
//!   the repository's immutable revision, reads at most 16 MiB of each of up to four files, keeps
//!   at most 100 answers for five minutes, coalesces identical checks and runs at most two at once.
//!
//! The source is `https://huggingface.co`, or the QA source (`NOEVIA_QA_MODEL_CARD_SOURCE`) in
//! synthetic runs. Every check runs on its own task, so a client that leaves strands nothing.
use super::Maint;
use axum::body::Body;
use axum::http::{HeaderValue, Response, StatusCode};
use js_json::{parse, stringify, JValue};
use std::collections::HashMap;
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

type Pending = HashMap<String, watch::Receiver<Option<JValue>>>;

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
    cache: Mutex<Vec<(String, Instant, JValue)>>,
    pending: Arc<Mutex<Pending>>,
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
        })
    }
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

fn reply(status: u16, body: &JValue) -> Response<Body> {
    crate::reply::json_text(
        StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
        stringify(body).unwrap_or_else(|| "null".into()),
        true,
    )
}

fn error(status: u16, message: &str) -> Response<Body> {
    reply(
        status,
        &JValue::obj([("error", JValue::Str(message.into()))]),
    )
}

pub async fn serve(maint: &Arc<Maint>, uri: &axum::http::Uri) -> Response<Body> {
    let repo = param(uri, "repo").unwrap_or_default();
    if !repo_ok(&repo) {
        return error(400, "Invalid repository");
    }
    let variant = param(uri, "variant");
    let m = Arc::clone(maint);
    let work = tokio::spawn(async move {
        let list = match m.mtp_variants(&repo).await {
            Err(Thrown) => return Err(Thrown),
            Ok(None) => return Ok(Err((502, "Could not resolve selected files"))),
            Ok(Some(list)) => list,
        };
        let items = match &list {
            JValue::Undefined | JValue::Null => Vec::new(),
            v if !v.truthy() => Vec::new(),
            JValue::Arr(items) => items.clone(),
            // `.find` of a string, number or object: a TypeError.
            _ => return Err(Thrown),
        };
        let mut chosen = None;
        for v in items {
            if v.is_nullish() {
                return Err(Thrown);
            }
            let name = v.get("name");
            let same = match (&variant, name) {
                (None, JValue::Null) => true,
                (Some(want), JValue::Str(s)) => s == want,
                _ => false,
            };
            if same {
                chosen = Some(v);
                break;
            }
        }
        let Some(v) = chosen else {
            return Ok(Err((404, "Variant no longer available")));
        };
        let files = if v.get("files").truthy() {
            v.get("files").clone()
        } else {
            JValue::Arr(vec![v.get("primary_file").clone()])
        };
        Ok(Ok(m.mtp_check(&repo, files).await))
    });
    match work.await {
        Ok(Ok(Ok(body))) => reply(200, &body),
        Ok(Ok(Err((status, message)))) => error(status, message),
        Ok(Err(Thrown)) | Err(_) => error(500, "Internal error"),
    }
}

/// A JS exception escaping the route: index.cjs answers 500 `Internal error`.
#[derive(Debug)]
struct Thrown;

/// `fetchJson`'s answer: transport failure throws, an over-cap reply is `{ ok: false }`.
async fn fetch_json(
    client: &reqwest::Client,
    url: &str,
    auth: Option<&HeaderValue>,
    within: Duration,
) -> Result<Option<JValue>, Thrown> {
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
        let text = String::from_utf8_lossy(&bytes).into_owned();
        Ok(Some(parse(&text).unwrap_or(JValue::Str(text))))
    };
    tokio::time::timeout(within, read)
        .await
        .map_err(|_| Thrown)?
}

impl Maint {
    /// `modelManager.variants(repo)`: the `variants` of a successful answer (`Ok(None)` for any
    /// `{ ok: false }`), or a thrown error.
    async fn mtp_variants(&self, repo: &str) -> Result<Option<JValue>, Thrown> {
        let mtp = &self.mtp;
        match mtp.kind {
            server_stats::Kind::LlamaCpp => {
                let url = format!("{}/api/models/{repo}?blobs=true", self.card_base);
                let Some(body) =
                    fetch_json(&mtp.list_client, &url, None, Duration::from_secs(20)).await?
                else {
                    return Ok(None);
                };
                Ok(variants_of(&body))
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
                Ok(body.map(|b| match b {
                    JValue::Obj(_) => b.get("variants").clone(),
                    _ => JValue::Undefined,
                }))
            }
            // model-manager.cjs `requireEnabled` throws.
            server_stats::Kind::None => Err(Thrown),
        }
    }

    /// mtp-artifact.cjs `check(repo, files)`.
    async fn mtp_check(self: &Arc<Self>, repo: &str, files: JValue) -> JValue {
        if let Err(reason) = validate(repo, &files) {
            return unknown(reason, &files);
        }
        let key = stringify(&JValue::Arr(vec![JValue::Str(repo.into()), files.clone()]))
            .unwrap_or_default();
        if let Some(saved) = self
            .mtp
            .cache
            .lock()
            .ok()
            .and_then(|c| c.iter().find(|(k, _, _)| *k == key).cloned())
            .filter(|(_, at, _)| at.elapsed() < CACHE_FOR)
        {
            return saved.2;
        }
        let mut rx = {
            let Ok(mut pending) = self.mtp.pending.lock() else {
                return unknown("Internal error".into(), &files);
            };
            if let Some(rx) = pending.get(&key) {
                rx.clone()
            } else if pending.len() >= RUNNING_MAX {
                return unknown(
                    "Two artifact checks are already running. Try again shortly.".into(),
                    &files,
                );
            } else {
                let (tx, rx) = watch::channel(None);
                pending.insert(key.clone(), rx.clone());
                let me = Arc::clone(self);
                let (repo, files, key) = (repo.to_owned(), files.clone(), key.clone());
                tokio::spawn(async move {
                    let result = me.mtp_run(&repo, &files, &key).await;
                    if let Ok(mut p) = me.mtp.pending.lock() {
                        p.remove(&key);
                    }
                    let _ = tx.send(Some(result));
                });
                rx
            }
        };
        let done = rx.wait_for(Option::is_some).await.map(|v| v.clone());
        match done {
            Ok(v) => v.unwrap_or(JValue::Undefined),
            Err(_) => unknown("Internal error".into(), &files),
        }
    }

    async fn mtp_run(&self, repo: &str, files: &JValue, key: &str) -> JValue {
        let names: Vec<String> = match files {
            JValue::Arr(items) => items
                .iter()
                .filter_map(|f| f.as_str().map(str::to_owned))
                .collect(),
            _ => Vec::new(),
        };
        match self.mtp_evidence(repo, &names).await {
            Err(reason) => unknown(reason, files),
            Ok((revision, evidence)) => {
                let result = verdict(files, revision, &evidence);
                if let Ok(mut cache) = self.mtp.cache.lock() {
                    if cache.len() >= CACHE_MAX {
                        cache.remove(0);
                    }
                    if let Some(slot) = cache.iter_mut().find(|(k, _, _)| k == key) {
                        slot.1 = Instant::now();
                        slot.2 = result.clone();
                    } else {
                        cache.push((key.to_owned(), Instant::now(), result.clone()));
                    }
                }
                result
            }
        }
    }

    async fn mtp_evidence(
        &self,
        repo: &str,
        files: &[String],
    ) -> Result<(JValue, Vec<Inspection>), String> {
        let client = &self.mtp.file_client;
        let meta_url = format!("{}/api/models/{repo}", self.card_base);
        let meta = tokio::time::timeout(Duration::from_secs(10), async {
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
            let text = String::from_utf8_lossy(&bytes).into_owned();
            let units: Vec<u16> = text.encode_utf16().collect();
            // JSON.parse's own message (V8); a lone surrogate in it becomes U+FFFD here.
            if let Some(message) = super::v8_json::parse_error(&units) {
                return Err(String::from_utf16_lossy(&message));
            }
            // V8 accepts it. The reader keeps a lone surrogate escape as U+FFFD, which can only
            // make a revision fail the hex test, as it does in Node.
            Ok(parse(&text).unwrap_or(JValue::Undefined))
        })
        .await
        .map_err(|_| "Artifact check timed out.".to_string())??;
        // `meta.sha`, then `/^[a-f0-9]{40,64}$/.test(revision || '')`: the test coerces the value to
        // a string, and the URL and the answer use the value as it is (`${revision}`, `revision`).
        let sha = match &meta {
            JValue::Null => return Err("Cannot read properties of null (reading 'sha')".into()),
            JValue::Obj(_) => meta.get("sha").clone(),
            _ => JValue::Undefined,
        };
        let revision = if sha.truthy() {
            js_json::to_js_string(&sha).unwrap_or_default()
        } else {
            String::new()
        };
        if !((40..=64).contains(&revision.len())
            && revision
                .bytes()
                .all(|b| matches!(b, b'a'..=b'f' | b'0'..=b'9')))
        {
            return Err("Immutable repository revision unavailable.".to_string());
        }
        let mut evidence = Vec::new();
        for file in files {
            let path: Vec<String> = file.split('/').map(encode_component).collect();
            let url = format!(
                "{}/{repo}/resolve/{revision}/{}",
                self.card_base,
                path.join("/")
            );
            let bytes = prefix(client, &url).await?;
            let mut found = inspect(&bytes)?;
            found.file = file.clone();
            evidence.push(found);
        }
        Ok((sha, evidence))
    }
}

/// llamacpp-variants.cjs over the source's model info: the variant rows, or `None` (`{ ok: false }`).
fn variants_of(body: &JValue) -> Option<JValue> {
    let siblings = match body {
        JValue::Obj(_) if body.get("private").truthy() => return None,
        JValue::Obj(_) => match body.get("siblings") {
            JValue::Arr(s) => s,
            _ => return None,
        },
        _ => return None,
    };
    struct File {
        name: String,
        size: JValue,
        shard: Option<String>,
        count: Option<String>,
    }
    let mut groups: Vec<(String, Vec<File>)> = Vec::new();
    for file in siblings {
        let Some(name) = (if file.is_object() {
            file.get("rfilename").as_str()
        } else {
            None
        }) else {
            continue;
        };
        if name.to_ascii_lowercase().contains("mmproj") {
            continue;
        }
        let Some((quant, shard, count)) = quant_match(name) else {
            continue;
        };
        let quant = quant.to_ascii_uppercase();
        let lfs = file.get("lfs");
        let lfs_size = if lfs.is_nullish() {
            &JValue::Undefined
        } else {
            lfs.get("size")
        };
        let size = if lfs_size.is_nullish() {
            file.get("size").clone()
        } else {
            lfs_size.clone()
        };
        let entry = File {
            name: name.to_owned(),
            size,
            shard,
            count,
        };
        match groups.iter_mut().find(|(q, _)| *q == quant) {
            Some((_, g)) => g.push(entry),
            None => groups.push((quant, vec![entry])),
        }
    }
    let mut rows: Vec<(String, JValue)> = Vec::new();
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
        let sizes: Vec<Option<f64>> = files
            .iter()
            .map(|f| match f.size {
                JValue::Num(n) if n.is_finite() && n > 0.0 => Some(n),
                _ => None,
            })
            .collect();
        let size = if sizes.iter().all(Option::is_some) {
            JValue::Num(sizes.iter().flatten().sum())
        } else {
            JValue::Null
        };
        rows.push((
            name.clone(),
            JValue::obj([
                ("name", JValue::Str(name.clone())),
                ("primary_file", JValue::Str(first.name.clone())),
                (
                    "files",
                    JValue::Arr(files.iter().map(|f| JValue::Str(f.name.clone())).collect()),
                ),
                ("size_bytes", size),
            ]),
        ));
    }
    rows.sort_by_key(|r| collate(&r.0));
    Some(JValue::Arr(rows.into_iter().map(|(_, r)| r).collect()))
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

/// `name.replace(/-\d{5}-of-\d{5}\.gguf$/i, '')`.
fn stem(name: &str) -> &str {
    let b = name.as_bytes();
    if b.len() >= 20 {
        let at = b.len() - 20;
        if matches!(tail(b, at), Some((Some(_), _))) {
            return name.get(..at).unwrap_or(name);
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

/// mtp-artifact.cjs `validate`.
pub fn validate(repo: &str, files: &JValue) -> Result<(), String> {
    if !repo_ok(repo) || repo.split('/').any(|p| p == "." || p == "..") {
        return Err("Invalid public Hugging Face repository.".into());
    }
    let bad = || Err("Inspection supports up to four selected GGUF shards.".to_string());
    let JValue::Arr(items) = files else {
        return bad();
    };
    if items.is_empty() || items.len() > 4 {
        return bad();
    }
    let mut seen: Vec<String> = Vec::new();
    for f in items {
        let key = stringify(f).unwrap_or_else(|| "undefined".into());
        if seen.contains(&key) {
            return bad();
        }
        seen.push(key);
    }
    for f in items {
        let Some(f) = f.as_str() else { return bad() };
        if !f.ends_with(".gguf")
            || f.starts_with('/')
            || f.split('/').any(|p| p.is_empty() || p == "." || p == "..")
            || f.chars()
                .any(|c| matches!(c, '?' | '#' | '\\' | '\u{0}'..='\u{1f}'))
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

fn unknown(reason: String, files: &JValue) -> JValue {
    JValue::obj([
        ("status", JValue::Str("unknown".into())),
        ("reason", JValue::Str(reason)),
        ("files", files.clone()),
        ("checkedAt", JValue::Str(now_iso())),
    ])
}

/// JS `x > n` for a metadata value (a number, a string, `null`).
fn js_gt(x: &JValue, n: f64) -> bool {
    match x {
        JValue::Num(v) => *v > n,
        JValue::Str(s) => {
            let t = s.trim();
            let v = if t.is_empty() {
                0.0
            } else {
                t.parse::<f64>().unwrap_or(f64::NAN)
            };
            v > n
        }
        JValue::Bool(b) => f64::from(u8::from(*b)) > n,
        JValue::Null => 0.0 > n,
        _ => false,
    }
}

fn verdict(files: &JValue, revision: JValue, evidence: &[Inspection]) -> JValue {
    let count = match files {
        JValue::Arr(f) => f.len() as f64,
        _ => 0.0,
    };
    if evidence.iter().any(|e| js_gt(&e.split_count, count)) {
        return unknown(
            "The selected variant does not enumerate every GGUF shard.".into(),
            files,
        );
    }
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
    JValue::obj([
        ("status", JValue::Str(status.into())),
        ("reason", JValue::Str(reason.into())),
        ("files", files.clone()),
        ("revision", revision),
        (
            "evidence",
            JValue::Arr(evidence.iter().map(Inspection::to_json).collect()),
        ),
        ("checkedAt", JValue::Str(now_iso())),
    ])
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
        assert_eq!(stem("m-Q8_0-00001-of-00002.gguf"), "m-Q8_0");
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
