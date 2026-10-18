//! noevia#1318: native, read-only `GET /api/v1/models/detail`.
//!
//! Python (`model-manager/app/api.py`):
//!
//! ```python
//! g = _find_entry(key)
//! summary = await asyncio.to_thread(gguf_meta.summarize_path, g.parts[0])   # 422 on a bad header
//! summary.pop("chat_template", None)
//! return {**_entry(g, await _loaded_map()), "path": str(g.parts[0]), "summary": _plain(summary)}
//! ```
//!
//! Native here: the key check, the models-folder snapshot (the walk of the native model update
//! read, with the ini aliases and `sections_by_file`), the entry fields, the GGUF summary (the
//! `gguf` crate, differential-tested against `gguf_meta.py`) and `services.model_shape`.
//!
//! Delegated, never re-implemented: `loadedOn` (Docker discovery plus llama-server probes) and
//! `fit` (the GPU sampler and host RAM) are live state that only the Python child computes. After
//! the file part has succeeded, the route asks the child for exactly `api._loaded_map()` and
//! `services.vram_fit_chips(bytes)` (`GET /api/v1/internal/model-detail-runtime`) and places
//! them where `_entry` does, so the answer is the one Python would give. When the child cannot
//! answer, the route answers 502 as any forwarded route does, never a guess.
//!
//! Deliberate differences (see `docs/native-model-detail.md`): conditions the kernel-contained
//! reader cannot follow exactly (I/O errors whose Python text names an errno, a link leaving the
//! models folder, a `model =` path with `..`, header reads past the reader's bound, numbers
//! Python would print beyond 128 bits) answer the native 503 refusal instead.
use crate::model_updates::{
    civil, exists, from_timestamp, group, imatrix, inside, json_string, listing, py_float_repr,
    read_ini, Found, Model, Stop,
};
use crate::sections::{access::Root, ini, Budget, Error as WalkError};
use std::{
    collections::HashMap,
    io::{Read, Seek, SeekFrom},
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};

/// Body of the native refusal (503) when the bounded reader cannot answer exactly.
pub const REFUSAL: &str = "Native model detail is unavailable";
/// The child's private call for the live fields.
pub const RUNTIME_PATH: &str = "/api/v1/internal/model-detail-runtime";
/// Largest runtime answer read from the child.
pub const RUNTIME_CAP: usize = 1024 * 1024;
/// FastAPI's answer to a request without `key`, byte for byte.
pub const MISSING_KEY: &str =
    r#"{"detail":[{"type":"missing","loc":["query","key"],"msg":"Field required","input":null}]}"#;
/// `autoconfig_core.HEAD_MAX_BYTES`: a draft-named file larger than this is a model, not a head.
const HEAD_MAX_BYTES: u64 = 2 * 1024 * 1024 * 1024;
const DEADLINE: Duration = Duration::from_millis(4500);

#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    /// A finished answer: status and JSON body.
    Json(u16, Vec<u8>),
    /// Python's handler raised: FastAPI's plain 500.
    Failed,
    /// The bounded native reader could not answer exactly: the native 503 refusal.
    Refused,
    /// The Python child did not answer the live fields: 502, as a forwarded route.
    Unavailable,
}

/// The file part of an answer, waiting for the live fields.
#[derive(Debug, PartialEq, Eq)]
pub struct Located {
    /// The JSON object text from `{"key":` through `"shape":…,` (trailing comma included).
    head: String,
    /// `,"badges":[],"path":…,"summary":…}`.
    tail: String,
    model_id: String,
    pub bytes: u128,
    /// `fit` is asked for only when the entry is not a companion, as `_entry` does.
    pub fit: bool,
}

pub enum Step {
    Done(Outcome),
    Located(Box<Located>),
}

/// Concurrent file parts (noevia#1889): each may hold a header of up to 256 MiB while parsing.
pub const CONCURRENT_READS: usize = 2;

pub struct ModelDetail {
    models: PathBuf,
    ini: PathBuf,
    reads: Arc<tokio::sync::Semaphore>,
}

impl ModelDetail {
    /// `models` and `ini` are Python's `MODELS_DIR` and `MODELS_INI_PATH`.
    pub fn new(models: &Path, ini: &Path) -> Self {
        Self {
            models: models.to_owned(),
            ini: ini.to_owned(),
            reads: Arc::new(tokio::sync::Semaphore::new(CONCURRENT_READS)),
        }
    }

    /// The file part, off the async runtime.
    pub async fn locate(&self, query: &str) -> Step {
        let (models, ini, query) = (self.models.clone(), self.ini.clone(), query.to_owned());
        // Waiting for a slot counts against the same budget; the permit lives as long as the
        // blocking read, also when this request is dropped.
        let deadline = Instant::now() + DEADLINE;
        let Ok(Ok(permit)) =
            tokio::time::timeout(DEADLINE, self.reads.clone().acquire_owned()).await
        else {
            return Step::Done(Outcome::Refused);
        };
        // noevia#2006: admitted: a dropped request keeps its admission permit (as its read
        // slot) until the read returns.
        crate::docker::spawn_blocking_admitted(move || {
            let _permit = permit;
            locate(&models, &ini, &query, deadline, Root::new)
        })
        .await
        .unwrap_or(Step::Done(Outcome::Failed))
    }
}

/// The query string of the child's runtime call for this entry.
pub fn runtime_query(located: &Located) -> String {
    format!(
        "bytes={}&fit={}",
        located.bytes,
        if located.fit { "1" } else { "0" }
    )
}

/// `{**_entry(g, loaded), "path": …, "summary": …}` with the child's `{"loaded": …, "fit": …}`.
pub fn splice(located: &Located, runtime: &[u8]) -> Outcome {
    use serde_json::value::RawValue;
    let Ok(fields) = serde_json::from_slice::<HashMap<String, Box<RawValue>>>(runtime) else {
        return Outcome::Unavailable;
    };
    let (Some(loaded), Some(fit)) = (fields.get("loaded"), fields.get("fit")) else {
        return Outcome::Unavailable;
    };
    let Ok(loaded) = serde_json::from_str::<HashMap<String, Vec<String>>>(loaded.get()) else {
        return Outcome::Unavailable;
    };
    // The chips are placed verbatim: the child rendered them with the response class FastAPI
    // uses, so every float keeps Python's text.
    if serde_json::from_str::<Vec<Box<RawValue>>>(fit.get()).is_err() {
        return Outcome::Unavailable;
    }
    if !located.fit && fit.get() != "[]" {
        return Outcome::Unavailable;
    }
    let names = loaded.get(&located.model_id).cloned().unwrap_or_default();
    let mut out = String::with_capacity(located.head.len() + located.tail.len() + 64);
    out.push_str(&located.head);
    out.push_str("\"loadedOn\":[");
    for (i, name) in names.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push_str(&json_string(name));
    }
    out.push_str("],\"fit\":");
    out.push_str(if located.fit { fit.get() } else { "[]" });
    out.push_str(&located.tail);
    Outcome::Json(200, out.into_bytes())
}

/// Starlette's `parse_qsl(keep_blank_values=True)`; FastAPI reads the last `key`.
fn key_of(query: &str) -> Option<String> {
    form_urlencoded::parse(query.as_bytes())
        .filter(|(name, _)| name == "key")
        .last()
        .map(|(_, value)| value.into_owned())
}

fn detail(status: u16, text: &str) -> Step {
    Step::Done(Outcome::Json(
        status,
        format!("{{\"detail\":{}}}", json_string(text)).into_bytes(),
    ))
}

fn stop(stop: Stop) -> Step {
    Step::Done(match stop {
        Stop::Failed => Outcome::Failed,
        Stop::Refused => Outcome::Refused,
    })
}

/// Everything before the live fields. `open` opens the models root (the kernel-contained walker
/// in production).
pub(crate) fn locate(
    models: &Path,
    ini_path: &Path,
    query: &str,
    deadline: Instant,
    open: impl FnOnce(&Path) -> Result<Root, WalkError>,
) -> Step {
    let Some(key) = key_of(query) else {
        return Step::Done(Outcome::Json(422, MISSING_KEY.as_bytes().to_vec()));
    };
    // `_find_entry`: checked before anything is read.
    if key.contains("..") || key.starts_with('/') || key.matches('/').count() > 4 {
        return detail(400, "bad model key");
    }
    match find(models, ini_path, &key, deadline, open) {
        Ok(Some(step)) => step,
        Ok(None) => detail(404, "model not found"),
        Err(e) => stop(e),
    }
}

/// One snapshot row with its aliases, as `GgufEntry` holds it.
pub(crate) struct Entry {
    pub(crate) model: Model,
    pub(crate) aliases: Vec<String>,
    /// A section whose `model =` path could not be checked may configure this file, so its
    /// aliases cannot be known exactly.
    pub(crate) unknowable: bool,
}

impl Entry {
    pub(crate) fn stem(&self) -> &str {
        stem(&self.model.base)
    }
    pub(crate) fn route_key(&self) -> String {
        if self.model.subdir.is_empty() {
            self.model.base.clone()
        } else {
            format!("{}/{}", self.model.subdir, self.model.base)
        }
    }
    pub(crate) fn first_rel(&self) -> Result<String, Stop> {
        let first = self.model.parts.first().ok_or(Stop::Failed)?;
        Ok(if self.model.subdir.is_empty() {
            first.name.clone()
        } else {
            format!("{}/{}", self.model.subdir, first.name)
        })
    }
    pub(crate) fn model_id(&self) -> &str {
        let stem = self.stem();
        if self.aliases.iter().any(|a| a == stem) {
            return stem;
        }
        self.aliases.first().map_or(stem, String::as_str)
    }
}

/// `name[:-5] if name.lower().endswith(".gguf") else name`. The last five characters can only be
/// ASCII when the lowered name ends with `.gguf`.
pub(crate) fn stem(name: &str) -> &str {
    if name.to_lowercase().ends_with(".gguf") {
        name.get(..name.len().saturating_sub(5)).unwrap_or(name)
    } else {
        name
    }
}

/// `services.snapshot_models_dir()`: the opened models root, its canonical path and every row
/// with its aliases. `None` for a missing models folder (nothing is read) or one that is a file.
pub(crate) struct Snapshot {
    pub(crate) root: Root,
    pub(crate) base: PathBuf,
    pub(crate) entries: Vec<Entry>,
    /// noevia#1318: what the model list also needs from the same reads: every file of the walk
    /// (`utils.iter_gguf` order), the files some section configures (`ini.sections_by_file()`'s
    /// keys) and the section names (`ini.section_names()`). Only the list reads them.
    #[cfg_attr(not(feature = "native-model-list"), allow(dead_code))]
    pub(crate) found: Vec<Found>,
    #[cfg_attr(not(feature = "native-model-list"), allow(dead_code))]
    pub(crate) claimed: std::collections::HashSet<String>,
    #[cfg_attr(not(feature = "native-model-list"), allow(dead_code))]
    pub(crate) names: Vec<String>,
}

pub(crate) fn snapshot(
    models: &Path,
    ini_path: &Path,
    open: impl FnOnce(&Path) -> Result<Root, WalkError>,
) -> Result<Option<Snapshot>, Stop> {
    // services.snapshot_models_dir(): nothing is read from a missing folder.
    let Some(directory) = exists(models)? else {
        return Ok(None);
    };
    // read_ini_aliases() swallows parse errors, but sections_by_file() reads the same file
    // through read_ini() and raises: any unreadable, non-UTF-8 or unparseable file is a 500.
    let ini = read_ini(ini_path)?;
    if !directory {
        return Ok(None); // os.walk of a file yields nothing
    }
    // A symlinked MODELS_DIR is followed, as Python's os.walk(top) follows it.
    let base = std::fs::canonicalize(models).map_err(|_| Stop::Refused)?;
    let root = open(&base).map_err(|_| Stop::Refused)?;
    let found = listing(&root, &base)?;
    let (by_file, unknown) = match &ini {
        Some((_, parsed)) => sections_by_file(parsed, &root, &base, &found)?,
        None => (HashMap::new(), Vec::new()),
    };
    let aliases: Vec<String> = match &ini {
        Some((text, parsed)) if !has_duplicate_sections(text) => {
            parsed.names().map(str::to_owned).collect()
        }
        _ => Vec::new(),
    };
    let names: Vec<String> = ini
        .as_ref()
        .map(|(_, parsed)| parsed.names().map(str::to_owned).collect())
        .unwrap_or_default();
    let claimed = by_file.keys().cloned().collect();
    let mut entries = Vec::new();
    for model in group(found.clone())? {
        let rel = match model.parts.first() {
            Some(first) if model.subdir.is_empty() => first.name.clone(),
            Some(first) => format!("{}/{}", model.subdir, first.name),
            None => return Err(Stop::Failed),
        };
        let matched = match by_file.get(&rel) {
            Some(names) if !names.is_empty() => names.clone(),
            _ => aliases
                .iter()
                .filter(|a| a.as_str() == stem(&model.base))
                .cloned()
                .collect(),
        };
        entries.push(Entry {
            unknowable: unknown.contains(&rel),
            model,
            aliases: matched,
        });
    }
    Ok(Some(Snapshot {
        root,
        base,
        entries,
        found,
        claimed,
        names,
    }))
}

fn find(
    models: &Path,
    ini_path: &Path,
    key: &str,
    deadline: Instant,
    open: impl FnOnce(&Path) -> Result<Root, WalkError>,
) -> Result<Option<Step>, Stop> {
    let Some(snapshot) = snapshot(models, ini_path, open)? else {
        return Ok(None);
    };
    let Some(entry) = snapshot.entries.into_iter().find(|e| e.route_key() == key) else {
        return Ok(None);
    };
    if entry.unknowable {
        return Err(Stop::Refused);
    }
    if Instant::now() >= deadline {
        return Err(Stop::Refused);
    }
    Ok(Some(describe(
        &entry,
        models,
        &snapshot.root,
        &snapshot.base,
        deadline,
    )?))
}

/// `ini.duplicate_sections(text)` is non-empty: then `read_ini_aliases` answers `[]`.
fn has_duplicate_sections(text: &str) -> bool {
    static HEADER: std::sync::OnceLock<Result<regex::Regex, regex::Error>> =
        std::sync::OnceLock::new();
    let Ok(header) = HEADER
        .get_or_init(|| {
            regex::Regex::new(r"^[\s\x1c-\x1f]*\[([^\]\r\n]+)\][\s\x1c-\x1f]*(?:[;#].*)?$")
        })
        .as_ref()
    else {
        return false;
    };
    let mut seen = std::collections::HashSet::new();
    // str.splitlines() boundaries (the text already has universal newlines).
    for line in text.split([
        '\n', '\r', '\u{000b}', '\u{000c}', '\u{001c}', '\u{001d}', '\u{001e}', '\u{0085}',
        '\u{2028}', '\u{2029}',
    ]) {
        if let Some(name) = header.captures(line).and_then(|c| c.get(1)) {
            if !seen.insert(name.as_str().to_owned()) {
                return true;
            }
        }
    }
    false
}

/// `ini._is_companion(name, path)`: with a path, a draft-named file over two GiB is a model.
fn companion_file(name: &str, size: u64) -> bool {
    let low = name.to_lowercase();
    low.contains("mmproj")
        || imatrix(&low)
        || (model_autoconfig::files::looks_like_draft(name) && size <= HEAD_MAX_BYTES)
}

/// `ini._stems_present()`: stem -> relative path of every non-companion file, the last one wins.
pub(crate) fn stems_present(found: &[Found]) -> Result<HashMap<String, String>, Stop> {
    let mut out = HashMap::new();
    for file in found {
        if companion_file(&file.name, file.size) {
            continue;
        }
        let base = shard_base(&file.name)?;
        let rel = if file.subdir.is_empty() {
            file.name.clone()
        } else {
            format!("{}/{}", file.subdir, file.name)
        };
        out.insert(stem(&base).to_owned(), rel);
    }
    Ok(out)
}

fn shard_base(name: &str) -> Result<String, Stop> {
    // Same rule as the snapshot's grouping, through the shared grouping helper.
    let grouped = group(vec![Found {
        subdir: String::new(),
        name: name.to_owned(),
        size: 0,
        mtime: 0.0,
    }])?;
    grouped
        .into_iter()
        .next()
        .map(|m| m.base)
        .ok_or(Stop::Failed)
}

/// `(settings.models_dir / rel).is_file()` for a section's `model =` path. pathlib drops empty
/// and `.` segments. `None` when the contained reader cannot tell: a `..` segment, or a link
/// leaving the folder (noevia#1889: that decides only this section, never the whole route).
pub(crate) fn is_file(root: &Root, base: &Path, rel: &str) -> Option<bool> {
    let mut segments = Vec::new();
    for segment in rel.split('/') {
        match segment {
            "" | "." => {}
            ".." => return None,
            other => segments.push(other),
        }
    }
    if segments.is_empty() {
        return Some(false); // the models folder itself
    }
    let normal = segments.join("/");
    let mut budget = Budget::default();
    let meta = match root.stat(&normal, &mut budget) {
        Ok(meta) => meta,
        // An absolute link is followed only when its target stays inside the models folder.
        Err(_) => inside(base, &normal).ok()?,
    };
    Some(meta.is_some_and(|m| m.is_file()))
}

/// `ini.sections_by_file()`: relative file -> the sections that configure it, in file order.
/// The second value lists the files a section might configure when its `model =` path could not
/// be checked: Python maps such a section either to that path (which names no listed file) or,
/// when it is not a file, to its stem's file. Only an entry among those is unknowable.
type ByFile = (HashMap<String, Vec<String>>, Vec<String>);
fn sections_by_file(
    parsed: &ini::Ini,
    root: &Root,
    base: &Path,
    found: &[Found],
) -> Result<ByFile, Stop> {
    let mut stems: Option<HashMap<String, String>> = None;
    let mut out: HashMap<String, Vec<String>> = HashMap::new();
    let mut unknown = Vec::new();
    let mut budget = Budget::default();
    for name in parsed.names() {
        let items = parsed
            .items(name, &mut budget)
            .map_err(|_| Stop::Refused)?
            .ok_or(Stop::Failed)?;
        let model = items
            .iter()
            .rev()
            .find(|(k, _)| k == "model")
            .map(|(_, v)| ini::strip(v))
            .unwrap_or_default();
        let mut rel = None;
        if !model.is_empty() {
            let candidate = model.replacen("/models/", "", 1);
            let candidate = candidate.trim_start_matches('/');
            match is_file(root, base, candidate) {
                Some(true) => rel = Some(candidate.to_owned()),
                Some(false) => {}
                None => {
                    if stems.is_none() {
                        stems = Some(stems_present(found)?);
                    }
                    if let Some(stem_rel) = stems.as_ref().and_then(|s| s.get(name)) {
                        unknown.push(stem_rel.clone());
                    }
                    continue;
                }
            }
        }
        if rel.is_none() {
            if stems.is_none() {
                stems = Some(stems_present(found)?);
            }
            rel = stems.as_ref().and_then(|s| s.get(name).cloned());
        }
        if let Some(rel) = rel.filter(|r| !r.is_empty()) {
            out.entry(rel).or_default().push(name.to_owned());
        }
    }
    Ok((out, unknown))
}

/// A reader that remembers whether the file itself failed (Python's `OSError`, whose text names
/// an errno this reader does not reproduce).
pub(crate) struct Watched<R> {
    pub(crate) inner: R,
    pub(crate) failed: bool,
}

impl<R: Read> Read for Watched<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        self.inner.read(buf).inspect_err(|_| self.failed = true)
    }
}

impl<R: Seek> Seek for Watched<R> {
    fn seek(&mut self, pos: SeekFrom) -> std::io::Result<u64> {
        self.inner.seek(pos).inspect_err(|_| self.failed = true)
    }
}

fn open_part(root: &Root, base: &Path, rel: &str) -> Result<std::fs::File, Stop> {
    // Vanished since the walk: Python's FileNotFoundError text is not reproduced.
    open_model_file(root, base, rel)?.ok_or(Stop::Refused)
}

/// Opens a model file the walk listed (`None`: it vanished since). An absolute link is followed
/// only while it stays inside the models folder, as the walk followed it.
pub(crate) fn open_model_file(
    root: &Root,
    base: &Path,
    rel: &str,
) -> Result<Option<std::fs::File>, Stop> {
    let mut budget = Budget::default();
    match root.file(rel, false, &mut budget) {
        Ok(Some(file)) => Ok(Some(file)),
        Ok(None) => Ok(None),
        Err(_) => {
            // An absolute link inside the models folder, as the walk followed it.
            if inside(base, rel)?.is_none() {
                return Err(Stop::Refused);
            }
            let target = std::fs::canonicalize(base.join(rel)).map_err(|_| Stop::Refused)?;
            if !target.starts_with(base) {
                return Err(Stop::Refused);
            }
            let file = std::fs::File::open(&target).map_err(|_| Stop::Refused)?;
            if !file.metadata().map_err(|_| Stop::Refused)?.is_file() {
                return Err(Stop::Refused);
            }
            Ok(Some(file))
        }
    }
}

/// The reader's own bound (no Python counterpart) shows up as this `_error` text.
pub(crate) fn hit_read_bound(raw: &gguf::Raw) -> bool {
    ["_error", "_tensor_params_error"].iter().any(|key| {
        matches!(raw.get(*key), Some(gguf::Value::Str(text)) if text.contains("header read limit of"))
    })
}

fn describe(
    entry: &Entry,
    models: &Path,
    root: &Root,
    base: &Path,
    deadline: Instant,
) -> Result<Step, Stop> {
    let rel = entry.first_rel()?;
    let file = open_part(root, base, &rel)?;
    let mut reader = Watched {
        inner: file,
        failed: false,
    };
    let parsed = gguf::read_raw_stream(&mut reader);
    if reader.failed {
        return Err(Stop::Refused);
    }
    let raw = match parsed {
        Ok(raw) => raw,
        Err(e) => {
            return Ok(detail(
                422,
                &format!("Could not read the model file: {}", e.0),
            ))
        }
    };
    if hit_read_bound(&raw) || Instant::now() >= deadline {
        return Err(Stop::Refused);
    }
    // A summarize() failure is a TypeError/ValueError in Python: unhandled, 500.
    let mut summary = gguf::summarize(&raw).map_err(|_| Stop::Failed)?;
    if let gguf::Json::Object(fields) = &mut summary {
        fields.retain(|(k, _)| k != "chat_template");
    }
    let model = &entry.model;
    let shape = if model.companion {
        "null".to_owned()
    } else {
        shape(&raw)?
    };
    let head = entry_head(entry, &shape)?;
    // str(g.parts[0]): the configured folder (as pathlib normalises it) joined with the file.
    let path = models.join(&rel);
    let path = path.to_str().ok_or(Stop::Refused)?;
    let tail = format!(
        ",\"badges\":[],\"path\":{},\"summary\":{}}}",
        json_string(path),
        summary
    );
    Ok(Step::Located(Box::new(Located {
        head,
        tail,
        model_id: entry.model_id().to_owned(),
        bytes: model.total,
        fit: !model.companion,
    })))
}

/// `api._entry(g, …)` from `"key"` through `"shape": …,` (trailing comma included); the live
/// fields and the badges follow.
pub(crate) fn entry_head(entry: &Entry, shape: &str) -> Result<String, Stop> {
    let model = &entry.model;
    let rel = entry.first_rel()?;
    let projector = if model.companion_name.is_empty() {
        "null".to_owned()
    } else {
        format!(
            "{{\"name\":{},\"bytes\":{}}}",
            json_string(&model.companion_name),
            model.companion_bytes
        )
    };
    let sections = entry
        .aliases
        .iter()
        .map(|a| json_string(a))
        .collect::<Vec<_>>()
        .join(",");
    Ok(format!(
        concat!(
            "{{\"key\":{},\"name\":{},\"stem\":{},\"subdir\":{},\"bytes\":{},\"size\":{},",
            "\"modified\":{},\"mtime\":{},\"sharded\":{},\"parts\":{},\"companion\":{},",
            "\"projector\":{},\"sections\":[{}],\"modelId\":{},\"file\":{},\"shape\":{},"
        ),
        json_string(&entry.route_key()),
        json_string(&model.base),
        json_string(entry.stem()),
        json_string(&model.subdir),
        model.total,
        json_string(&human_bytes(model.total)),
        json_string(&modified(model.mtime).ok_or(Stop::Failed)?),
        py_float_repr(model.mtime),
        model.parts.len() > 1,
        model.parts.len(),
        model.companion,
        projector,
        sections,
        json_string(entry.model_id()),
        json_string(&rel),
        shape,
    ))
}

/// `utils.human_bytes(n)`.
pub(crate) fn human_bytes(total: u128) -> String {
    let mut n = total as f64;
    for unit in ["B", "KB", "MB", "GB", "TB", "PB"] {
        if n.abs() < 1024.0 {
            return if unit == "B" {
                format!("{} B", n.trunc())
            } else {
                format!("{n:.1} {unit}")
            };
        }
        n /= 1024.0;
    }
    format!("{n:.1} EB")
}

/// `datetime.fromtimestamp(mtime, tz=timezone.utc).astimezone().strftime("%Y-%m-%d %H:%M")`
/// with the process zone UTC (the native route requires `TZ=UTC`, as it sets for the child).
fn modified(mtime: f64) -> Option<String> {
    let micros = from_timestamp(mtime)?;
    let seconds = i64::try_from(micros.div_euclid(1_000_000)).ok()?;
    let (year, month, day) = civil(seconds.div_euclid(86_400));
    let of_day = seconds.rem_euclid(86_400);
    Some(format!(
        "{year}-{month:02}-{day:02} {:02}:{:02}",
        of_day / 3600,
        of_day % 3600 / 60
    ))
}

/// `int(v)` for one header value; `None` refuses (a Python result this port does not print).
fn py_int(value: Option<&gguf::Value>) -> Result<i128, Stop> {
    Ok(match value {
        Some(gguf::Value::Bool(b)) => i128::from(*b),
        Some(gguf::Value::Int(i)) => *i,
        Some(gguf::Value::Float(f)) if !f.is_finite() => 0, // ValueError / OverflowError
        Some(gguf::Value::Float(f)) => {
            let t = f.trunc();
            if t.abs() >= 1.7e38 {
                return Err(Stop::Refused);
            }
            t as i128
        }
        Some(gguf::Value::Str(text)) => int_of_str(text)?,
        // TypeError: None, a list, an array summary (a dict).
        _ => 0,
    })
}

/// `int(text)` in base 10: surrounding whitespace, a sign, digits with single underscores.
fn int_of_str(text: &str) -> Result<i128, Stop> {
    let body = ini::strip(text);
    if !body.is_ascii() {
        // Python also reads other scripts' decimal digits; not reproduced here.
        return if body.chars().any(char::is_numeric) {
            Err(Stop::Refused)
        } else {
            Ok(0)
        };
    }
    let (negative, digits) = match body.as_bytes().first() {
        Some(b'-') => (true, body.get(1..).unwrap_or_default()),
        Some(b'+') => (false, body.get(1..).unwrap_or_default()),
        _ => (false, body),
    };
    let bytes = digits.as_bytes();
    let valid = !bytes.is_empty()
        && bytes.first().is_some_and(u8::is_ascii_digit)
        && bytes.last().is_some_and(u8::is_ascii_digit)
        && bytes.iter().all(|b| b.is_ascii_digit() || *b == b'_')
        && !digits.contains("__");
    if !valid {
        return Ok(0); // ValueError
    }
    let clean: String = digits.chars().filter(char::is_ascii_digit).collect();
    if clean.len() > 4300 {
        return Ok(0); // ValueError: past sys.get_int_max_str_digits()
    }
    let magnitude: i128 = clean.parse().map_err(|_| Stop::Refused)?;
    Ok(if negative { -magnitude } else { magnitude })
}

/// `str(v)` of a truthy architecture value.
fn py_text(value: &gguf::Value) -> Result<String, Stop> {
    Ok(match value {
        gguf::Value::Str(text) => text.clone(),
        gguf::Value::Float(f) if f.is_nan() => "nan".to_owned(),
        gguf::Value::Float(f) if f.is_infinite() => {
            if *f > 0.0 { "inf" } else { "-inf" }.to_owned()
        }
        gguf::Value::Float(f) => py_float_repr(*f),
        gguf::Value::Int(i) => i.to_string(),
        gguf::Value::Bool(b) => if *b { "True" } else { "False" }.to_owned(),
        other => gguf::metadata_text_with_limit(other, 65536).map_err(|_| Stop::Refused)?,
    })
}

fn truthy(value: &gguf::Value) -> bool {
    match value {
        gguf::Value::Str(text) => !text.is_empty(),
        gguf::Value::Int(i) => *i != 0,
        gguf::Value::Float(f) => *f != 0.0,
        gguf::Value::Bool(b) => *b,
        gguf::Value::List(items) => !items.is_empty(),
        gguf::Value::ArraySummary { .. } => true,
        gguf::Value::None => false,
    }
}

/// `services.model_shape(path)` as JSON (`null` when the architecture is unknown).
pub(crate) fn shape(raw: &gguf::Raw) -> Result<String, Stop> {
    // `kv.get("kv")` is a dict only when a header key named `kv` held a summarised array; the
    // summary dict then has no architecture.
    if matches!(raw.get("kv"), Some(gguf::Value::ArraySummary { .. })) {
        return Ok("null".to_owned());
    }
    let arch = match raw.get("general.architecture") {
        Some(value) if truthy(value) => py_text(value)?,
        _ => return Ok("null".to_owned()),
    };
    if arch.is_empty() {
        return Ok("null".to_owned());
    }
    let experts = py_int(raw.get(&format!("{arch}.expert_count")))?;
    let active = py_int(raw.get(&format!("{arch}.expert_used_count")))?;
    let label = if experts <= 0 {
        "dense".to_owned()
    } else if active != 0 {
        format!("MoE {experts}x{active}")
    } else {
        format!("MoE {experts}")
    };
    Ok(format!(
        "{{\"arch\":{},\"moe\":{},\"experts\":{experts},\"active\":{active},\"label\":{}}}",
        json_string(&arch),
        experts > 0,
        json_string(&label)
    ))
}

#[cfg(test)]
pub(crate) mod tests;
