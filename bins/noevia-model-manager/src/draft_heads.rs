//! noevia#1318 slice 12: native, read-only `GET /api/v1/sections/{name}/draft-heads`.
//!
//! Python (`model-manager/app/api.py`):
//!
//! ```python
//! gguf_path, _model_rel, rel = _resolve_section_gguf(name)
//! if gguf_path is None or rel is None:
//!     raise HTTPException(404, "no model file for this section")
//! subdir = rel.rsplit("/", 1)[0] if "/" in rel else ""
//! local = autoconfig._find_mtp(settings.models_dir, name, subdir)
//! builtin = await asyncio.to_thread(_builtin_mtp_layers, gguf_path)
//! out = {"section": name, "local": local, "builtinLayers": builtin, "available": bool(local or builtin),
//!        "remote": [], "mtpBuild": None, "repo": None, "modes": autoconfig.MODE_SPEC_PROFILE}
//! repo = db.repo_for_file(Path(rel).name)
//! if not repo:
//!     return out
//! out["repo"] = repo
//! try:
//!     detail = await hf.repo_detail(repo)
//!     out["remote"] = sorted((... heads of the listing ...), key=lambda x: x["size"] or 0)
//!     if not out["remote"] and not builtin and <repo is a "-GGUF" repo, not "-MTP-GGUF">:
//!         try: await hf.repo_detail(repo[:-5] + "-MTP-GGUF"); out["mtpBuild"] = sibling
//!         except Exception: pass
//! except Exception:
//!     out["remoteError"] = "Could not reach Hugging Face to list draft models."
//! return out
//! ```
//!
//! Everything is native: the section's file (`helpers._resolve_section_gguf` with
//! `ini.section_file_rel`, `ini.get_section` and `ini._stems_present`, through the kernel-contained
//! reader and the ini parser of the native sections read), the heads beside it
//! (`autoconfig._find_mtp`: `_listing` here, the name rules in `model_autoconfig::files`), the
//! built-in MTP layers (the `gguf` crate), `db.repo_for_file` (the same SQL), and `hf.repo_detail`
//! (the tree listing with its `Link` paging, the token from the store, and the listing turned into
//! files by the `model-files` crate from the same projected JSON the Python child sends the binary).
//!
//! Deliberate differences (see `docs/native-draft-heads.md`): a repository id that is not
//! Hub-shaped, a `next` link to another origin, or a page over the read bound counts as a failed
//! lookup (Python would send them); conditions the bounded reader cannot follow exactly (a link
//! leaving the models folder, a symlinked folder, a `..` in a `model =` path, a header past the
//! reader's bound, a summary error whose Python class is unknown) answer the native 503 refusal.
use crate::check_updates::{hub_auth, hub_id, HubAuth};
use crate::hub_page::{parse_page, read_capped, PAGE_CAP, PAGE_NODES};
use crate::model_detail::{hit_read_bound, is_file, stems_present, Watched};
use crate::model_updates::{exists, inside, json_string, listing, read_ini, Stop};
use crate::sections::{access::Root, ini, Budget, Error as WalkError};
use model_files::json::{Limits, Value};
use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

/// Body of the native refusal (503) when the bounded reader cannot answer exactly.
pub const REFUSAL: &str = "Native draft-head read is unavailable";
/// `hf.HF_API`.
pub const HF_API: &str = "https://huggingface.co/api";
const NOT_FOUND: &str = "no model file for this section";
const REMOTE_ERROR: &str = "Could not reach Hugging Face to list draft models.";
/// `autoconfig_core.MODE_SPEC_PROFILE` as FastAPI writes it.
const MODES: &str = r#"{"chat":"balanced","code":"coding","agent":"coding","writing":"writing"}"#;
/// `autoconfig_core.HEAD_MAX_BYTES`.
const HEAD_MAX_BYTES: i128 = 2 * 1024 * 1024 * 1024;
/// `hf._TREE_MAX_PAGES`.
const TREE_MAX_PAGES: usize = 40;
/// httpx `timeout=30.0` (connect and each read).
const HTTP_TIMEOUT: Duration = Duration::from_secs(30);
/// A bound httpx does not have: the whole of one page request.
const REQUEST_BUDGET: Duration = Duration::from_secs(120);
/// sqlite3's default `timeout=5.0`.
const BUSY_WAIT: Duration = Duration::from_millis(5000);
/// Work units for the name rules of one listing (`model_autoconfig::Work`).
const RULE_WORK: u64 = 64 * 1024 * 1024;
/// Concurrent file parts: each may hold a GGUF header while parsing.
pub const CONCURRENT_READS: usize = 2;
/// Concurrent Hub lookups (noevia#1973): each may hold one tree page (up to [`PAGE_CAP`]) while
/// it is parsed into a tree (bounded by [`PAGE_NODES`] and `hub_page::PAGE_STRING_CHARS`), then
/// that tree plus the projected listing (up to `model_files::MAX_INPUT_BYTES`, kept from earlier pages
/// throughout), then the listing's own tree (bounded by [`PROJECTED_NODES`]) and its file list:
/// about 100 MiB per lookup at most (noevia#1995); later requests wait.
pub const CONCURRENT_LOOKUPS: usize = 2;
/// noevia#1995: most JSON values and keys in the projected listing (the payload the binary
/// parses), counted as `model_files::json` counts them. A projected file entry is at most 11
/// nodes, so this is about 23,800 such entries (a GGUF repository has tens); it keeps the parsed
/// payload within the same bound as one page. Python would read up to `MAX_ENTRIES` (100,000).
pub const PROJECTED_NODES: usize = PAGE_NODES;

#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    /// A finished answer: status and JSON body.
    Json(u16, Vec<u8>),
    /// Python's handler raised: FastAPI's plain 500.
    Failed,
    /// The bounded native reader could not answer exactly: the native 503 refusal.
    Refused,
}

pub struct DraftHeads {
    models: PathBuf,
    ini: PathBuf,
    database: PathBuf,
    hf_api: String,
    client: reqwest::Client,
    reads: Arc<tokio::sync::Semaphore>,
    lookups: Arc<tokio::sync::Semaphore>,
    open: fn(&Path) -> Result<Root, WalkError>,
    /// Lib tests: runs in the listing parse's blocking task, before the parse.
    #[cfg(test)]
    during_parse: Option<Arc<dyn Fn() + Send + Sync>>,
}

/// The local part of an answer.
#[derive(Debug, PartialEq, Eq)]
struct Local {
    local: String,
    /// `builtinLayers` as Python prints the int.
    builtin: String,
    repo: Option<String>,
}

impl DraftHeads {
    /// `models`, `ini` and `data` are Python's `MODELS_DIR`, `MODELS_INI_PATH` and `DATA_DIR`.
    pub fn new(models: &Path, ini: &Path, data: &Path) -> Result<Self, &'static str> {
        Self::with(models, ini, data, HF_API, Root::new)
    }

    /// Tests point the Hub at a local fake (the models root opened as in production).
    pub fn with_hub(
        models: &Path,
        ini: &Path,
        data: &Path,
        hf_api: &str,
    ) -> Result<Self, &'static str> {
        Self::with(models, ini, data, hf_api, Root::new)
    }

    /// Lib tests also open the models root with the synthetic reader.
    pub(crate) fn with(
        models: &Path,
        ini: &Path,
        data: &Path,
        hf_api: &str,
        open: fn(&Path) -> Result<Root, WalkError>,
    ) -> Result<Self, &'static str> {
        let client = reqwest::Client::builder()
            .connect_timeout(HTTP_TIMEOUT)
            .read_timeout(HTTP_TIMEOUT)
            .timeout(REQUEST_BUDGET)
            .redirect(reqwest::redirect::Policy::limited(20))
            .build()
            .map_err(|_| "Hugging Face client could not be built")?;
        Ok(Self {
            models: models.to_owned(),
            ini: ini.to_owned(),
            database: data.join("model_loader.db"),
            hf_api: hf_api.trim_end_matches('/').to_owned(),
            client,
            reads: Arc::new(tokio::sync::Semaphore::new(CONCURRENT_READS)),
            lookups: Arc::new(tokio::sync::Semaphore::new(CONCURRENT_LOOKUPS)),
            open,
            #[cfg(test)]
            during_parse: None,
        })
    }

    /// The answer for one (percent-decoded) section name.
    pub async fn get(&self, name: &str) -> Outcome {
        let Ok(permit) = self.reads.clone().acquire_owned().await else {
            return Outcome::Refused;
        };
        let (models, ini_path, database, open, section) = (
            self.models.clone(),
            self.ini.clone(),
            self.database.clone(),
            self.open,
            name.to_owned(),
        );
        // noevia#1998: every blocking task here is admitted: a dropped request keeps its
        // admission permit until the task has returned.
        let located = crate::docker::spawn_blocking_admitted(move || {
            let _permit = permit;
            locate(&models, &ini_path, &database, &section, open)
        })
        .await;
        let local = match located {
            Ok(Ok(Some(local))) => local,
            Ok(Ok(None)) => {
                return Outcome::Json(
                    404,
                    format!("{{\"detail\":{}}}", json_string(NOT_FOUND)).into_bytes(),
                )
            }
            Ok(Err(Stop::Refused)) => return Outcome::Refused,
            Ok(Err(Stop::Failed)) | Err(_) => return Outcome::Failed,
        };
        let mut remote = String::from("[]");
        let mut mtp_build = "null".to_owned();
        let mut remote_error = false;
        if let Some(repo) = &local.repo {
            // Held for the listing and the sibling lookup; the semaphore is never closed.
            // noevia#1998: each listing parse keeps a share, so a dropped request gives the
            // lookup back only when its parse has returned.
            let Ok(lookup) = self.lookups.clone().acquire_owned().await else {
                return Outcome::Refused;
            };
            let lookup = Arc::new(lookup);
            match self.repo_detail(repo, &lookup).await {
                Ok(files) => {
                    let heads = heads(repo, files);
                    remote = heads.render;
                    if heads.empty && local.builtin == "0" {
                        if let Some(sibling) = sibling(repo) {
                            if self.repo_detail(&sibling, &lookup).await.is_ok() {
                                mtp_build = json_string(&sibling);
                            }
                        }
                    }
                }
                Err(Lookup::Failed) => {
                    eprintln!("draft lookup for {repo} failed");
                    remote_error = true;
                }
            }
        }
        let available = !local.local.is_empty() || local.builtin != "0";
        let mut out = format!(
            "{{\"section\":{},\"local\":{},\"builtinLayers\":{},\"available\":{available},\"remote\":{remote},\"mtpBuild\":{mtp_build},\"repo\":{},\"modes\":{MODES}",
            json_string(name),
            json_string(&local.local),
            local.builtin,
            local.repo.as_deref().map_or("null".to_owned(), json_string),
        );
        if remote_error {
            out.push_str(",\"remoteError\":");
            out.push_str(&json_string(REMOTE_ERROR));
        }
        out.push('}');
        Outcome::Json(200, out.into_bytes())
    }

    /// `hf.repo_detail(repo_id)`: every page of the tree listing, as files.
    async fn repo_detail(
        &self,
        repo: &str,
        lookup: &Arc<tokio::sync::OwnedSemaphorePermit>,
    ) -> Result<Vec<model_files::HfFile>, Lookup> {
        // `_auth_headers()` when the client is built: any failure raises inside the lookup.
        let database = self.database.clone();
        let auth = crate::docker::spawn_blocking_admitted(move || hub_auth(&database))
            .await
            .map_err(|_| Lookup::Failed)?;
        let header = match auth {
            HubAuth::Absent => None,
            HubAuth::Value(v) => Some(v),
            HubAuth::NotAscii | HubAuth::Refused | HubAuth::Failed => return Err(Lookup::Failed),
        };
        if !hub_id(repo) {
            return Err(Lookup::Failed);
        }
        let origin = origin_of(&self.hf_api).ok_or(Lookup::Failed)?;
        let mut url = format!(
            "{}/models/{repo}/tree/main?recursive=true&expand=true",
            self.hf_api
        );
        // Each page is projected as it arrives (noevia#1973): only the projected entries are
        // kept, with the running size of `json.dumps(projected)`, and the lookup fails as soon as
        // it passes the binary's input cap (Python's `len(payload) > MODEL_FILES_STDIN_CAP`
        // after the last page; the size only grows, so the answer is the same).
        let mut projected = Projected::default();
        for _ in 0..TREE_MAX_PAGES {
            let mut request = self.client.get(&url);
            if let Some(value) = &header {
                request = request.header(reqwest::header::AUTHORIZATION, value.clone());
            }
            let response = request.send().await.map_err(|_| Lookup::Failed)?;
            // `raise_for_status()`: anything but 2xx raises.
            if !response.status().is_success() {
                return Err(Lookup::Failed);
            }
            let link = link_header(response.headers());
            let body = read_capped(response.bytes_stream(), PAGE_CAP)
                .await
                .ok_or(Lookup::Failed)?;
            // The raw page is dropped by the parse (noevia#1985): only the tree is kept.
            let page = parse_page(body).ok_or(Lookup::Failed)?;
            let Value::Arr(items) = page else {
                break;
            };
            for item in &items {
                projected.push(item).ok_or(Lookup::Failed)?;
            }
            drop(items);
            let Some(next) = next_link(&link) else {
                break;
            };
            // Python follows any next link with the token; only the Hub's own origin is.
            if origin_of(&next).as_ref() != Some(&origin) {
                return Err(Lookup::Failed);
            }
            url = next;
        }
        let payload = projected.finish();
        let lookup = lookup.clone();
        #[cfg(test)]
        let during = self.during_parse.clone();
        let files = crate::docker::spawn_blocking_admitted(move || {
            let _lookup = lookup;
            #[cfg(test)]
            if let Some(during) = during {
                during();
            }
            files_of(payload)
        })
        .await
        .map_err(|_| Lookup::Failed)??;
        let mut files = files;
        // `files.sort(key=lambda f: (not f.path.lower().endswith(".gguf"), f.path.lower()))`.
        files.sort_by_cached_key(|f| {
            let lower = f.path.to_lowercase();
            (!lower.ends_with(".gguf"), lower)
        });
        Ok(files)
    }
}

/// The binary's own parse and caps for the projected listing (its size, entries and nodes were
/// checked entry by entry). noevia#1995: the parse is held to [`PROJECTED_NODES`] as well, and
/// the payload text is dropped before the files are built.
fn files_of(payload: String) -> Result<Vec<model_files::HfFile>, Lookup> {
    let limits = Limits {
        nodes: PROJECTED_NODES,
        ..Limits::default()
    };
    let value =
        model_files::json::parse_with_limits(&payload, limits).map_err(|_| Lookup::Failed)?;
    drop(payload);
    model_files::files_from_tree(&value).map_err(|_| Lookup::Failed)
}

/// Which cap an entry would take a [`Projected`] listing past (noevia#1318 search: the routes
/// answer them differently).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Exceeded {
    /// The payload would pass `model_files::MAX_INPUT_BYTES` (the binary refuses it).
    Bytes,
    /// More than `model_files::MAX_ENTRIES` entries (the binary refuses it).
    Entries,
    /// More than [`PROJECTED_NODES`] values and keys (a bound of this side).
    Nodes,
}

/// `json.dumps([_project(e) for e in entries])`, built entry by entry under the input cap.
#[derive(Default)]
pub(crate) struct Projected {
    out: String,
    /// noevia#1995: entries appended, at most `model_files::MAX_ENTRIES` (the binary refuses
    /// more, so the answer is the same).
    entries: usize,
    /// noevia#1995: values and keys appended (without the enclosing array), so the payload
    /// stays within [`PROJECTED_NODES`].
    nodes: usize,
}

impl Projected {
    /// Adds one entry; `None` once the finished payload would pass `model_files::MAX_INPUT_BYTES`.
    ///
    /// noevia#1985: the entry's projected size is measured first and nothing is appended when it
    /// does not fit, so one entry (escaped non-ASCII text dumps to up to 3 times its page bytes)
    /// never takes the payload past the cap. The buffer grows by doubling but never beyond the cap.
    ///
    /// noevia#1995: likewise once the payload would hold more than `model_files::MAX_ENTRIES`
    /// entries or [`PROJECTED_NODES`] nodes (the enclosing array is one).
    fn push(&mut self, entry: &Value) -> Option<()> {
        self.try_push(entry).ok()
    }

    /// [`Projected::push`], telling which cap was reached.
    pub(crate) fn try_push(&mut self, entry: &Value) -> Result<(), Exceeded> {
        let separator = if self.out.is_empty() { "[" } else { ", " };
        let mut size = Count::default();
        dump_projected(entry, &mut size);
        let len = self
            .out
            .len()
            .checked_add(separator.len())
            .and_then(|len| len.checked_add(size.bytes))
            .ok_or(Exceeded::Bytes)?;
        // The closing `]` counts too.
        if len >= model_files::MAX_INPUT_BYTES {
            return Err(Exceeded::Bytes);
        }
        let entries = self.entries.checked_add(1).ok_or(Exceeded::Entries)?;
        let nodes = self.nodes.checked_add(size.nodes).ok_or(Exceeded::Nodes)?;
        if entries > model_files::MAX_ENTRIES {
            return Err(Exceeded::Entries);
        }
        if nodes.checked_add(1).ok_or(Exceeded::Nodes)? > PROJECTED_NODES {
            return Err(Exceeded::Nodes);
        }
        // Room for the closing `]` too, so `finish` never reallocates (len < the cap here).
        let room = len.checked_add(1).ok_or(Exceeded::Bytes)?;
        if self.out.capacity() < room {
            let grown = self
                .out
                .capacity()
                .saturating_mul(2)
                .max(room)
                .min(model_files::MAX_INPUT_BYTES);
            self.out
                .try_reserve_exact(grown - self.out.len())
                .map_err(|_| Exceeded::Bytes)?;
        }
        self.out.push_str(separator);
        dump_projected(entry, &mut self.out);
        self.entries = entries;
        self.nodes = nodes;
        Ok(())
    }

    pub(crate) fn finish(mut self) -> String {
        if self.out.is_empty() {
            self.out.push('[');
        }
        self.out.push(']');
        self.out
    }
}

#[derive(Debug)]
enum Lookup {
    /// Python's lookup raised (caught by the route).
    Failed,
}

/// The remote heads of a listing, rendered, and whether there were none.
struct Heads {
    render: String,
    empty: bool,
}

fn heads(repo: &str, files: Vec<model_files::HfFile>) -> Heads {
    let mut picked: Vec<(i128, model_files::HfFile)> = Vec::new();
    for f in files {
        let size: i128 = f.size.to_string().parse().unwrap_or(i128::MAX);
        let name = pathlib_name(&f.path);
        if f.path.to_lowercase().ends_with(".gguf")
            && !name.to_lowercase().contains("mmproj")
            && model_autoconfig::files::looks_like_draft(&name)
            && 0 < size
            && size <= HEAD_MAX_BYTES
        {
            picked.push((size, f));
        }
    }
    // `sorted(..., key=lambda x: x["size"] or 0)`: stable.
    picked.sort_by_key(|(size, _)| *size);
    let items: Vec<String> = picked
        .iter()
        .map(|(_, f)| {
            format!(
                "{{\"repo\":{},\"path\":{},\"size\":{}}}",
                json_string(repo),
                json_string(&f.path),
                f.size
            )
        })
        .collect();
    Heads {
        empty: items.is_empty(),
        render: format!("[{}]", items.join(",")),
    }
}

/// `repo[:-5] + "-MTP-GGUF"` when `repo.lower()` ends with `-gguf` and not with `-mtp-gguf`.
fn sibling(repo: &str) -> Option<String> {
    let lower = repo.to_lowercase();
    if lower.ends_with("-mtp-gguf") || !lower.ends_with("-gguf") {
        return None;
    }
    let chars: Vec<char> = repo.chars().collect();
    let keep = chars.len().checked_sub(5)?;
    Some(chars.get(..keep)?.iter().collect::<String>() + "-MTP-GGUF")
}

/// `PurePosixPath(path).name`.
fn pathlib_name(path: &str) -> String {
    path.split('/')
        .rfind(|p| !p.is_empty() && *p != ".")
        .unwrap_or_default()
        .to_owned()
}

/// `PurePosixPath(name).suffix` (Python 3.12).
fn pathlib_suffix(name: &str) -> &str {
    match name.rfind('.') {
        Some(i) if 0 < i && i + 1 < name.len() => name.get(i..).unwrap_or_default(),
        _ => "",
    }
}

/// (scheme, host, port) of a URL, as an origin.
pub(crate) fn origin_of(url: &str) -> Option<(String, String, u16)> {
    let parsed = reqwest::Url::parse(url).ok()?;
    Some((
        parsed.scheme().to_owned(),
        parsed.host_str()?.to_ascii_lowercase(),
        parsed.port_or_known_default()?,
    ))
}

/// `tree.headers.get("link", "")`: every `Link` value joined with `", "`, decoded as httpx
/// decodes header bytes (ASCII, else UTF-8, else Latin-1, decided over all headers).
pub(crate) fn link_header(headers: &reqwest::header::HeaderMap) -> String {
    let all_ascii = headers.iter().all(|(_, v)| v.as_bytes().is_ascii());
    let all_utf8 = headers
        .iter()
        .all(|(_, v)| std::str::from_utf8(v.as_bytes()).is_ok());
    let decode = |bytes: &[u8]| -> String {
        if all_ascii || all_utf8 {
            String::from_utf8_lossy(bytes).into_owned()
        } else {
            bytes.iter().map(|b| char::from(*b)).collect()
        }
    };
    headers
        .get_all(reqwest::header::LINK)
        .iter()
        .map(|v| decode(v.as_bytes()))
        .collect::<Vec<_>>()
        .join(", ")
}

/// `_NEXT_LINK_RE.search(link)`: `<([^>]+)>\s*;\s*rel="next"`.
pub(crate) fn next_link(link: &str) -> Option<String> {
    static NEXT: std::sync::OnceLock<Option<regex::Regex>> = std::sync::OnceLock::new();
    let re = NEXT
        .get_or_init(|| regex::Regex::new(r#"<([^>]+)>\s*;\s*rel="next""#).ok())
        .as_ref()?;
    re.captures(link)
        .and_then(|c| c.get(1))
        .map(|m| m.as_str().to_owned())
}

/// `model_files._project(e)`: only `type`, `path`, `size` and `lfs.size` reach the binary. The
/// payload is written by [`dump_projected`] without building this copy.
#[cfg(test)]
fn project(e: &Value) -> Value {
    let Value::Obj(_) = e else {
        return e.clone();
    };
    let mut out: Vec<(String, Value)> = Vec::new();
    for key in ["type", "path", "size"] {
        if let Some(v) = e.get(key) {
            out.push((key.to_owned(), v.clone()));
        }
    }
    if let Some(lfs) = e.get("lfs") {
        let projected = match lfs {
            Value::Obj(_) => match lfs.get("size") {
                Some(size) => Value::Obj(vec![("size".to_owned(), size.clone())]),
                None => Value::Obj(Vec::new()),
            },
            other => other.clone(),
        };
        out.push(("lfs".to_owned(), projected));
    }
    Value::Obj(out)
}

/// `json.dumps(value, ensure_ascii=True)` for what `json.loads` returned (a dict keeps the first
/// position of a repeated key and its last value).
#[cfg(test)]
fn py_dumps_ascii(value: &Value) -> String {
    let mut out = String::new();
    dump(value, &mut out);
    out
}

/// Where [`dump`] writes: the payload, or (noevia#1985) only its length, to size an entry before
/// it is appended.
trait Sink {
    fn push_str(&mut self, s: &str);
    fn push(&mut self, c: char);
    /// One JSON value or object key written, as `model_files::json` counts nodes (noevia#1995).
    fn node(&mut self) {}
}

impl Sink for String {
    fn push_str(&mut self, s: &str) {
        String::push_str(self, s);
    }
    fn push(&mut self, c: char) {
        String::push(self, c);
    }
}

/// The byte length and node count of what would be written.
#[derive(Default)]
struct Count {
    bytes: usize,
    nodes: usize,
}

impl Sink for Count {
    fn push_str(&mut self, s: &str) {
        self.bytes = self.bytes.saturating_add(s.len());
    }
    fn push(&mut self, c: char) {
        self.bytes = self.bytes.saturating_add(c.len_utf8());
    }
    fn node(&mut self) {
        self.nodes = self.nodes.saturating_add(1);
    }
}

/// `json.dumps(_project(e))` written straight from the entry (no projected copy is built).
fn dump_projected(e: &Value, out: &mut impl Sink) {
    let Value::Obj(_) = e else {
        return dump(e, out);
    };
    out.node();
    out.push('{');
    let mut first = true;
    let mut key = |out: &mut dyn Sink, name: &str| {
        out.node();
        if !first {
            out.push_str(", ");
        }
        first = false;
        out.push('"');
        out.push_str(name);
        out.push_str("\": ");
    };
    for name in ["type", "path", "size"] {
        if let Some(v) = e.get(name) {
            key(out, name);
            dump(v, out);
        }
    }
    if let Some(lfs) = e.get("lfs") {
        key(out, "lfs");
        match lfs {
            Value::Obj(_) => {
                out.node();
                match lfs.get("size") {
                    Some(size) => {
                        out.node();
                        out.push_str("{\"size\": ");
                        dump(size, out);
                        out.push('}');
                    }
                    None => out.push_str("{}"),
                }
            }
            other => dump(other, out),
        }
    }
    out.push('}');
}

fn dump(value: &Value, out: &mut (impl Sink + ?Sized)) {
    out.node();
    match value {
        Value::Null => out.push_str("null"),
        Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Value::Int(i) => out.push_str(&i.to_string()),
        Value::Float(f) if f.is_nan() => out.push_str("NaN"),
        Value::Float(f) if f.is_infinite() => {
            out.push_str(if *f > 0.0 { "Infinity" } else { "-Infinity" });
        }
        Value::Float(f) => out.push_str(&gguf::py_float_repr(*f)),
        Value::Str(s) => dump_str(s, out),
        Value::Arr(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                dump(item, out);
            }
            out.push(']');
        }
        Value::Obj(pairs) => {
            // Each key at its first position with its last value, in linear time (noevia#1985:
            // an entry is dumped twice, measured then written, and a page may hold an object of
            // `PAGE_NODES / 2` keys).
            let mut spans: std::collections::HashMap<&str, (usize, usize)> =
                std::collections::HashMap::with_capacity(pairs.len());
            for (i, (k, _)) in pairs.iter().enumerate() {
                spans.entry(k.as_str()).or_insert((i, i)).1 = i;
            }
            out.push('{');
            let mut written = 0usize;
            for (i, (key, _)) in pairs.iter().enumerate() {
                let Some(&(first, last)) = spans.get(key.as_str()) else {
                    continue;
                };
                if first != i {
                    continue;
                }
                if written > 0 {
                    out.push_str(", ");
                }
                written += 1;
                out.node();
                dump_str(key, out);
                out.push_str(": ");
                if let Some((_, v)) = pairs.get(last) {
                    dump(v, out);
                }
            }
            out.push('}');
        }
    }
}

fn dump_str(s: &str, out: &mut (impl Sink + ?Sized)) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            c if c.is_ascii() && !c.is_ascii_control() => out.push(c),
            c => {
                let mut units = [0u16; 2];
                for unit in c.encode_utf16(&mut units) {
                    out.push_str("\\u");
                    for shift in [12, 8, 4, 0] {
                        let digit = u32::from(*unit >> shift) & 0xf;
                        out.push(char::from_digit(digit, 16).unwrap_or('0'));
                    }
                }
            }
        }
    }
    out.push('"');
}

/// The file part: `None` for the 404.
fn locate(
    models: &Path,
    ini_path: &Path,
    database: &Path,
    name: &str,
    open: fn(&Path) -> Result<Root, WalkError>,
) -> Result<Option<Local>, Stop> {
    // `ini.get_section(name)` (read_ini() raises for a file it cannot parse).
    let parsed = read_ini(ini_path)?;
    let mut budget = Budget::default();
    let explicit = match &parsed {
        Some((_, ini)) if ini.names().any(|n| n == name) => ini
            .items(name, &mut budget)
            .map_err(|_| Stop::Refused)?
            .ok_or(Stop::Failed)?
            .iter()
            .rev()
            .find(|(k, _)| k == "model")
            .map(|(_, v)| ini::strip(v).to_owned())
            .unwrap_or_default(),
        _ => String::new(),
    };
    // Nothing is read from a missing models folder or one that is a file: every `is_file()` is
    // False and `_stems_present()` finds nothing, so there is no file for any section.
    let Some(true) = exists(models)? else {
        return Ok(None);
    };
    let base = std::fs::canonicalize(models).map_err(|_| Stop::Refused)?;
    let root = open(&base).map_err(|_| Stop::Refused)?;
    let checked =
        |rel: &str| -> Result<bool, Stop> { is_file(&root, &base, rel).ok_or(Stop::Refused) };
    // `ini.section_file_rel(name)`.
    let mut rel: Option<String> = None;
    if !explicit.is_empty() {
        let candidate = explicit.replacen("/models/", "", 1);
        let candidate = candidate.trim_start_matches('/');
        if checked(candidate)? {
            rel = Some(candidate.to_owned());
        }
    }
    if rel.is_none() {
        let found = listing(&root, &base)?;
        rel = stems_present(&found)?.get(name).cloned();
    }
    let Some(rel) = rel else {
        return Ok(None);
    };
    // `helpers._resolve_section_gguf(name)`.
    let gguf_rel = if !explicit.is_empty() && !rel.is_empty() && checked(&rel)? {
        rel.clone()
    } else if let Some((folder, _)) = rel.rsplit_once('/') {
        if !is_dir(&root, &base, folder)? {
            return Ok(None);
        }
        let entries = root
            .entries_unless_denied(&normal(folder)?, &mut budget)
            .map_err(|_| Stop::Refused)?
            .ok_or(Stop::Failed)?; // iterdir() raises PermissionError: unhandled
        let mut ggufs: Vec<String> = Vec::new();
        for entry in entries {
            let path = format!("{}/{}", normal(folder)?, entry.name);
            if pathlib_suffix(&entry.name).to_lowercase() == ".gguf"
                && stat_file(&root, &base, &path)?
            {
                ggufs.push(entry.name);
            }
        }
        ggufs.sort();
        let main: Vec<&String> = ggufs
            .iter()
            .filter(|n| !n.to_lowercase().contains("mmproj"))
            .collect();
        let first = main.first().copied().or(ggufs.first());
        match first {
            Some(file) => format!("{}/{file}", normal(folder)?),
            None => return Ok(None),
        }
    } else {
        rel.clone()
    };
    let subdir = rel.rsplit_once('/').map_or("", |(d, _)| d);
    let local = find_mtp(&root, &base, name, subdir)?;
    let builtin = builtin_layers(&root, &base, &gguf_rel)?;
    let repo = repo_for_file(database, &pathlib_name(&rel))?;
    Ok(Some(Local {
        local,
        builtin,
        repo,
    }))
}

/// A models-relative path as pathlib joins it (`""` and `.` segments dropped); `..` refuses.
pub(crate) fn normal(rel: &str) -> Result<String, Stop> {
    let mut segments = Vec::new();
    for segment in rel.split('/') {
        match segment {
            "" | "." => {}
            ".." => return Err(Stop::Refused),
            other => segments.push(other),
        }
    }
    Ok(if segments.is_empty() {
        ".".to_owned()
    } else {
        segments.join("/")
    })
}

/// `stat()` following links, inside the models folder; `None` when it does not exist.
pub(crate) fn stat(root: &Root, base: &Path, rel: &str) -> Result<Option<std::fs::Metadata>, Stop> {
    let rel = normal(rel)?;
    if rel == "." {
        return std::fs::metadata(base).map(Some).map_err(|_| Stop::Refused);
    }
    let mut budget = Budget::default();
    match root.stat(&rel, &mut budget) {
        Ok(meta) => Ok(meta),
        // An absolute link is followed only when its target stays inside the models folder.
        Err(_) => inside(base, &rel),
    }
}

pub(crate) fn is_dir(root: &Root, base: &Path, rel: &str) -> Result<bool, Stop> {
    Ok(stat(root, base, rel)?.is_some_and(|m| m.is_dir()))
}

pub(crate) fn stat_file(root: &Root, base: &Path, rel: &str) -> Result<bool, Stop> {
    Ok(stat(root, base, rel)?.is_some_and(|m| m.is_file()))
}

/// `autoconfig._listing(folder)` as the name rules read it; `Null` when it cannot be listed.
pub(crate) fn rule_listing(root: &Root, base: &Path, folder: &str) -> Result<Value, Stop> {
    let folder = normal(folder)?;
    let mut budget = Budget::default();
    let Some(mut entries) = root
        .entries_unless_denied(&folder, &mut budget)
        .map_err(|_| Stop::Refused)?
    else {
        return Ok(Value::Null);
    };
    entries.sort_by(|a, b| a.name.cmp(&b.name));
    let mut out = Vec::new();
    for entry in entries {
        let path = if folder == "." {
            entry.name.clone()
        } else {
            format!("{folder}/{}", entry.name)
        };
        let row = match stat(root, base, &path)? {
            Some(meta) if meta.is_file() => vec![
                Value::Str(entry.name),
                Value::Str("file".into()),
                Value::Int(model_files::BigInt::from_u64(meta.len())),
            ],
            _ => vec![
                Value::Str(entry.name),
                Value::Str("other".into()),
                Value::Null,
            ],
        };
        out.push(Value::Arr(row));
    }
    Ok(Value::Arr(out))
}

/// `autoconfig._find_mtp(settings.models_dir, section_name, subdir)`.
pub(crate) fn find_mtp(
    root: &Root,
    base: &Path,
    section: &str,
    subdir: &str,
) -> Result<String, Stop> {
    let pick = |call: Value| -> Result<String, Stop> {
        let mut work = model_autoconfig::Work::new(RULE_WORK);
        match model_autoconfig::files::pick(&call, &mut work) {
            Ok(model_autoconfig::files::Answer::Path(path)) => Ok(path),
            // min(..., key=stat) raised OSError: unhandled.
            Err(model_autoconfig::Error::Python(_)) => Err(Stop::Failed),
            _ => Err(Stop::Refused),
        }
    };
    let s = |v: &str| Value::Str(v.to_owned());
    if subdir.is_empty() {
        return pick(Value::Obj(vec![
            ("rule".into(), s("mtp_flat")),
            ("listing".into(), rule_listing(root, base, ".")?),
            ("section".into(), s(section)),
        ]));
    }
    for candidate in [
        format!("{subdir}/MTP"),
        format!("{subdir}/mtp"),
        subdir.to_owned(),
    ] {
        if !is_dir(root, base, &candidate)? {
            continue;
        }
        let rel = normal(&candidate)?;
        let found = pick(Value::Obj(vec![
            ("rule".into(), s("mtp_folder")),
            ("listing".into(), rule_listing(root, base, &candidate)?),
            ("prefix".into(), Value::Str(format!("/models/{rel}/"))),
            ("section".into(), s(section)),
        ]))?;
        if !found.is_empty() {
            return Ok(found);
        }
    }
    Ok(String::new())
}

/// `api._builtin_mtp_layers(gguf_path)`: the summary's `nextn_predict_layers`, 0 where Python
/// catches the error (`GgufMetaError`, `OSError`).
fn builtin_layers(root: &Root, base: &Path, rel: &str) -> Result<String, Stop> {
    let rel = normal(rel)?;
    let mut budget = Budget::default();
    let file = match root.file(&rel, false, &mut budget) {
        Ok(Some(file)) => file,
        Ok(None) => return Ok("0".into()), // FileNotFoundError
        Err(_) => {
            // An absolute link inside the models folder, as the walk followed it.
            if inside(base, &rel)?.is_none() {
                return Ok("0".into());
            }
            let target = std::fs::canonicalize(base.join(&rel)).map_err(|_| Stop::Refused)?;
            if !target.starts_with(base) {
                return Err(Stop::Refused);
            }
            match std::fs::File::open(&target) {
                Ok(file) if file.metadata().is_ok_and(|m| m.is_file()) => file,
                _ => return Ok("0".into()),
            }
        }
    };
    let mut reader = Watched {
        inner: file,
        failed: false,
    };
    let parsed = gguf::read_raw_stream(&mut reader);
    if reader.failed {
        return Ok("0".into()); // OSError while reading
    }
    let Ok(raw) = parsed else {
        return Ok("0".into()); // GgufMetaError
    };
    if hit_read_bound(&raw) {
        return Err(Stop::Refused);
    }
    // A summarize() failure is a ValueError (caught) or a TypeError (not): unknown here.
    let summary = gguf::summarize(&raw).map_err(|_| Stop::Refused)?;
    let layers = match &summary {
        gguf::Json::Object(fields) => {
            fields
                .iter()
                .rev()
                .find(|(k, _)| k == "model")
                .and_then(|(_, model)| match model {
                    gguf::Json::Object(m) => m
                        .iter()
                        .rev()
                        .find(|(k, _)| k == "nextn_predict_layers")
                        .map(|(_, v)| v.clone()),
                    _ => None,
                })
        }
        _ => None,
    };
    // `int(x or 0)`: the summary holds an int or None here.
    Ok(match layers {
        None | Some(gguf::Json::Null) => "0".into(),
        Some(gguf::Json::Int(n)) => n.to_string(),
        Some(_) => return Err(Stop::Refused),
    })
}

/// `db.repo_for_file(basename)`.
fn repo_for_file(database: &Path, basename: &str) -> Result<Option<String>, Stop> {
    use rusqlite::{types::ValueRef, OpenFlags, OptionalExtension};
    let db = rusqlite::Connection::open_with_flags(
        database,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(|e| logged(&e))?;
    db.busy_timeout(BUSY_WAIT).map_err(|e| logged(&e))?;
    let like = format!("%/{basename}");
    let row = db
        .query_row(
            "SELECT repo_id FROM download_history WHERE status = 'done' AND (filename = ? OR filename LIKE ?) \
             AND repo_id LIKE '%/%' ORDER BY id DESC LIMIT 1",
            [basename, like.as_str()],
            |row| {
                Ok(match row.get_ref(0)? {
                    // sqlite3 raises decoding TEXT that is not UTF-8: unhandled.
                    ValueRef::Text(t) => std::str::from_utf8(t)
                        .map(str::to_owned)
                        .map_err(|_| Stop::Failed),
                    // A blob holding a `/` is bytes in Python: not followed here.
                    _ => Err(Stop::Refused),
                })
            },
        )
        .optional()
        .map_err(|e| logged(&e))?;
    match row {
        None => Ok(None),
        Some(Ok(repo)) if repo.is_empty() => Ok(None),
        Some(Ok(repo)) => Ok(Some(repo)),
        Some(Err(stop)) => Err(stop),
    }
}

fn logged(error: &rusqlite::Error) -> Stop {
    crate::sqlite_read::log_once("draft_heads", error);
    Stop::Failed
}

#[cfg(test)]
mod tests;
