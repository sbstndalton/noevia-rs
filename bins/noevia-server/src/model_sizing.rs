//! The read-only model sizing reads (sbstndalton/noevia#2189, batch M2-A): `GET
//! /api/models/estimate?model=` (the "Will it fit?" inputs) and `GET
//! /api/models/preset/suggest?model=` (the size-based preset suggestion), permanent and with no
//! switch. Node refuses both GETs (core server/rust-model-sizing.cjs), so the web supervisor and the
//! release assembly pair `rust-model-sizing` with that Core. Every other method stays Node's.
//!
//! Ported from core `routes/models.cjs` (the handlers), `llamacpp-manager.cjs` (`estimateMemory`,
//! `suggestPreset`, `readModel`, `modelFiles`, `localFile`, `sizingBudgetGib`),
//! `llamacpp-presets.cjs` (`read`, `parse`, `get`, `files`), `gguf-meta.cjs` (`readSummaryWasm`)
//! and `inference-budget.cjs` (`get`, `cacheRamLimits`). The sizing itself is the same Rust Node
//! already ran through dav-parse.wasm: crates/llamacpp-autoconfig's strict operations 1 (suggest)
//! and 2 (inputs), and crates/gguf's header summary. Node's answer was the JS estimator's,
//! confirmed by that strict port: where the two texts agree (every recorded case,
//! tests/fixtures/model-sizing) the port's text is the answer; a port refusal is Node's
//! `impl_refused` answer.
//!
//! Read-only: this reads models.ini, the model mounts, the router's `/models` listing and the
//! saved inference budget (SQLite `settings`, key `inference:memory-budget`); it writes none of
//! them and keeps no cache a Node write would have to drop.
//!
//! Where it cannot be Node's answer (documented, each unreachable in a working deployment):
//! - The boot-time memory-limit fault (`LLAMACPP_MEMORY_LIMIT` unparseable) and the "no budget"
//!   501: core always has an inference budget of at least 2 GiB, which wins over both.
//! - The settings store cannot be read: 503 (Node holds the budget in memory).
//! - GGUF metadata holding NaN, Infinity or -0 in a raw field (only float-typed headers): the port
//!   is asked with `null`/`0` where the JS estimator saw the number.
//! - A router listing nested 64 deep at the fields read: treated as no listing.

use crate::reply;
use axum::body::Body;
use axum::http::{Response, StatusCode};
use prompt_framing::json::Value;
use server_stats::compose::Read;
use std::io::{Read as _, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::Arc;

pub const ESTIMATE: &str = "/api/models/estimate";
pub const SUGGEST: &str = "/api/models/preset/suggest";

const ADMIN_REQUIRED: &str = "Administrator required for shared model profiles";
const BUDGET_KEY: &str = "inference:memory-budget";
const IMPL_SUGGEST_ERROR: &str = "No settings suggested: the Rust sizing port did not confirm this suggestion and the JS one is not the smaller. Size the context manually and load-test it.";
const IMPL_INPUTS_ERROR: &str = "Estimate unavailable: the memory estimate (Will it fit?) could not be confirmed by the Rust sizing port, and the JS estimate is not the larger one.";
const NO_MODEL_FILE: &str = "The model file is not visible under noevia's read-only model mounts.";
const NO_PROJECTOR: &str =
    "The vision projector is not visible under noevia's read-only model mounts.";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Route {
    Estimate,
    Suggest,
}

impl Route {
    pub fn of(path: &str) -> Option<Route> {
        match path {
            ESTIMATE => Some(Route::Estimate),
            SUGGEST => Some(Route::Suggest),
            _ => None,
        }
    }
}

/// What the sizing reads answer from: the keys core index.cjs read at startup.
pub struct Sizing {
    kind: server_stats::Kind,
    engine: Arc<server_stats::Engine>,
    /// `LLAMACPP_PRESET_PATH` (set and non-empty: the llama.cpp adapter has a preset store).
    presets: Option<PathBuf>,
    /// `LLAMACPP_MODELS_PATH || ''` and `LLAMACPP_CACHE_PATH || ''`.
    models_path: String,
    cache_path: String,
    /// `Number(LLAMACPP_AUTOCONFIG_MEMORY_GIB) || 0`.
    explicit_gib: f64,
    /// inference-budget.cjs `cacheRamLimits(env).capMib`.
    cache_ram_max_mib: f64,
    /// `INFERENCE_MEMORY_BUDGET_GIB` as the environment holds it.
    default_budget: Option<String>,
    /// Where `/proc/meminfo` is read (tests point it elsewhere).
    meminfo: PathBuf,
}

impl Sizing {
    /// `LLAMACPP_PRESET_PATH` (set and non-empty): where models.ini, and beside it models.roles.json,
    /// are kept.
    pub fn presets_path(&self) -> Option<&Path> {
        self.presets.as_deref()
    }
}

impl std::fmt::Debug for Sizing {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Sizing")
            .field("kind", &self.kind)
            .field("presets", &self.presets)
            .field("models_path", &self.models_path)
            .finish_non_exhaustive()
    }
}

/// JS `Number(text)` (`undefined` is NaN).
fn env_number(v: Option<&str>) -> f64 {
    match v {
        None => f64::NAN,
        Some(s) => server_stats::js::string_to_number(&s.encode_utf16().collect::<Vec<_>>()),
    }
}

/// inference-budget.cjs `positive(value, fallback)`.
fn positive(v: Option<&str>, fallback: f64) -> f64 {
    let n = env_number(v);
    if n.is_finite() && n > 0.0 {
        n
    } else {
        fallback
    }
}

/// `n || 0`.
fn or_zero(n: f64) -> f64 {
    if n.is_nan() {
        0.0
    } else {
        n
    }
}

impl Sizing {
    pub fn from_lookup(
        kind: server_stats::Kind,
        engine: Arc<server_stats::Engine>,
        get: &impl Fn(&str) -> Option<String>,
    ) -> Sizing {
        let set = |k: &str| get(k).filter(|v| !v.is_empty());
        let hard_max = llamacpp_autoconfig::js_round(positive(
            get("LLAMACPP_CACHE_RAM_HARD_MAX_MIB").as_deref(),
            2048.0,
        ));
        let cap = llamacpp_autoconfig::js_min(
            hard_max,
            llamacpp_autoconfig::js_round(positive(
                get("LLAMACPP_AUTOCONFIG_CACHE_RAM_MAX_MIB").as_deref(),
                1024.0,
            )),
        );
        Sizing {
            kind,
            engine,
            presets: set("LLAMACPP_PRESET_PATH").map(PathBuf::from),
            models_path: set("LLAMACPP_MODELS_PATH").unwrap_or_default(),
            cache_path: set("LLAMACPP_CACHE_PATH").unwrap_or_default(),
            explicit_gib: or_zero(env_number(get("LLAMACPP_AUTOCONFIG_MEMORY_GIB").as_deref())),
            cache_ram_max_mib: cap,
            default_budget: get("INFERENCE_MEMORY_BUDGET_GIB"),
            meminfo: PathBuf::from(
                get("NOEVIA_TEST_MEMINFO_PATH").unwrap_or_else(|| "/proc/meminfo".into()),
            ),
        }
    }

    /// inference-budget.cjs `get().budgetGib` for the saved setting `setting` (the column as
    /// SQLite holds it, `None` when there is no row), then llamacpp-manager.cjs `sizingBudgetGib`.
    pub fn sizing_budget_gib(&self, setting: Option<&SettingValue>) -> f64 {
        let host = host_ram_gib(std::fs::read_to_string(&self.meminfo).ok().as_deref());
        let max_gib = match host {
            Some(h) if h != 0.0 => js_max(2.0, (h - 4.0).floor()),
            _ => 1024.0,
        };
        let saved = setting.and_then(saved_budget);
        let raw = saved.unwrap_or_else(|| positive(self.default_budget.as_deref(), 16.0));
        let inference = llamacpp_autoconfig::js_min(max_gib, js_max(2.0, raw));
        let explicit = self.explicit_gib;
        if inference > 0.0 {
            if explicit > 0.0 {
                llamacpp_autoconfig::js_min(inference, explicit)
            } else {
                inference
            }
        } else {
            0.0
        }
    }
}

fn js_max(a: f64, b: f64) -> f64 {
    llamacpp_autoconfig::js_max(a, b)
}

/// The `settings.value` column as better-sqlite3 hands it to `store.get(KEY)`.
#[derive(Debug, Clone, PartialEq)]
pub enum SettingValue {
    Text(String),
    /// An INTEGER or REAL value: `JSON.parse` of a number has no `budgetGib`.
    Number,
    Null,
}

/// `Number(JSON.parse(raw || 'null')?.budgetGib)`, kept when above 0.
fn saved_budget(v: &SettingValue) -> Option<f64> {
    let SettingValue::Text(text) = v else {
        return None;
    };
    if text.is_empty() {
        return None;
    }
    let units: Vec<u16> = text.encode_utf16().collect();
    let parsed = prompt_framing::json::parse(&units, 64)?;
    let n = match parsed {
        Value::Obj(_) => js_to_number(parsed.get("budgetGib")),
        _ => f64::NAN,
    };
    (n > 0.0).then_some(n)
}

/// `Number(v)` of a `JSON.parse` value (`undefined` is NaN).
fn js_to_number(v: Option<&Value>) -> f64 {
    match v {
        None | Some(Value::Deep) | Some(Value::Obj(_)) => f64::NAN,
        Some(Value::Null) => 0.0,
        Some(Value::Bool(b)) => f64::from(u8::from(*b)),
        Some(Value::Num(n)) => *n,
        Some(Value::Str(s)) => server_stats::js::string_to_number(s),
        Some(Value::Arr(items)) => {
            let mut text = Vec::new();
            array_string(items, &mut text);
            server_stats::js::string_to_number(&text)
        }
    }
}

/// `String(array)`: the elements joined by `,` (null and undefined as empty).
fn array_string(items: &[Value], out: &mut Vec<u16>) {
    for (i, item) in items.iter().enumerate() {
        if i > 0 {
            out.push(u16::from(b','));
        }
        match item {
            Value::Null => {}
            Value::Bool(b) => out.extend(if *b { "true" } else { "false" }.encode_utf16()),
            Value::Num(n) => out.extend(llamacpp_autoconfig::number_string(*n).encode_utf16()),
            Value::Str(s) => out.extend_from_slice(s),
            Value::Arr(inner) => array_string(inner, out),
            Value::Obj(_) | Value::Deep => out.extend("[object Object]".encode_utf16()),
        }
    }
}

/// inference-budget.cjs `hostRamGib`: `MemTotal` in GiB to one decimal, or `None`.
fn host_ram_gib(meminfo: Option<&str>) -> Option<f64> {
    let text = meminfo?;
    for line in text.split('\n') {
        let Some(rest) = line.strip_prefix("MemTotal:") else {
            continue;
        };
        let digits_at = rest.find(|c: char| !c.is_ascii_whitespace())?;
        if digits_at == 0 {
            return None;
        }
        let rest = &rest[digits_at..];
        let end = rest
            .find(|c: char| !c.is_ascii_digit())
            .unwrap_or(rest.len());
        if end == 0 || !rest[end..].trim_start().starts_with("kB") {
            return None;
        }
        let kib: f64 = rest[..end].parse().ok()?;
        if kib.partial_cmp(&0.0) != Some(std::cmp::Ordering::Greater) {
            return None;
        }
        return Some(llamacpp_autoconfig::js_round(kib / 1_048_576.0 * 10.0) / 10.0);
    }
    None
}

// ── models.ini (llamacpp-presets.cjs, read only) ─────────────────────────────────────────────

/// llamacpp-presets.cjs `fields`: canonical names and aliases, in their order.
const FIELDS: &[(&str, &[&str])] = &[
    ("ctx-size", &["c", "LLAMA_ARG_CTX_SIZE"]),
    ("parallel", &["np", "LLAMA_ARG_N_PARALLEL"]),
    (
        "n-gpu-layers",
        &["ngl", "gpu-layers", "LLAMA_ARG_N_GPU_LAYERS"],
    ),
    ("cache-type-k", &["ctk", "LLAMA_ARG_CACHE_TYPE_K"]),
    ("cache-type-v", &["ctv", "LLAMA_ARG_CACHE_TYPE_V"]),
    ("flash-attn", &["fa", "LLAMA_ARG_FLASH_ATTN"]),
    ("batch-size", &["b", "LLAMA_ARG_BATCH"]),
    ("ubatch-size", &["ub", "LLAMA_ARG_UBATCH"]),
    ("cache-ram", &["cram", "LLAMA_ARG_CACHE_RAM"]),
    ("image-max-tokens", &["LLAMA_ARG_IMAGE_MAX_TOKENS"]),
    ("spec-type", &["LLAMA_ARG_SPEC_TYPE"]),
    ("spec-draft-n-max", &["LLAMA_ARG_SPEC_DRAFT_N_MAX"]),
    ("spec-draft-p-min", &["LLAMA_ARG_SPEC_DRAFT_P_MIN"]),
    ("temp", &[]),
    ("top-p", &[]),
    ("top-k", &["LLAMA_ARG_TOP_K"]),
    ("min-p", &[]),
    ("repeat-penalty", &[]),
];

fn canonical(key: &str) -> Option<&'static str> {
    FIELDS
        .iter()
        .find(|(k, aliases)| *k == key || aliases.contains(&key))
        .map(|(k, _)| *k)
}

/// ECMAScript `\s`.
fn js_space(c: char) -> bool {
    u16::try_from(u32::from(c)).is_ok_and(server_stats::js::is_space)
}

/// A line terminator `.` does not match (`\n` never occurs: the text is split on it).
fn line_terminator(c: char) -> bool {
    matches!(c, '\n' | '\r' | '\u{2028}' | '\u{2029}')
}

/// `\s*(.*?)\s*$` over `rest`: the value, or `None` when a line terminator sits inside it.
fn trimmed_value(rest: &str) -> Option<&str> {
    let v = rest.trim_start_matches(js_space).trim_end_matches(js_space);
    (!v.contains(line_terminator)).then_some(v)
}

/// `/^\s*\[([^\]\r\n]+)\]\s*(?:[;#].*)?$/`: the section name.
fn header_of(line: &str) -> Option<&str> {
    let rest = line.trim_start_matches(js_space).strip_prefix('[')?;
    let end = rest.find([']', '\r', '\n'])?;
    if end == 0 || !rest[end..].starts_with(']') {
        return None;
    }
    let name = &rest[..end];
    let after = rest[end + 1..].trim_start_matches(js_space);
    if after.is_empty() {
        return Some(name);
    }
    let comment = after.strip_prefix([';', '#'])?;
    (!comment.contains(line_terminator)).then_some(name)
}

/// `/^\s*([^=\s]+)\s*=\s*(.*?)\s*$/`: the key and the value.
fn option_of(line: &str) -> Option<(&str, &str)> {
    let rest = line.trim_start_matches(js_space);
    let end = rest.find(|c: char| c == '=' || js_space(c))?;
    if end == 0 {
        return None;
    }
    let key = &rest[..end];
    let after = rest[end..].trim_start_matches(js_space).strip_prefix('=')?;
    Some((key, trimmed_value(after)?))
}

/// `/^\s*(model|mmproj|m)\s*=\s*(.*?)\s*$/`: the file kind and the value.
fn file_of(line: &str) -> Option<(&'static str, &str)> {
    let rest = line.trim_start_matches(js_space);
    for (name, kind) in [("model", "model"), ("mmproj", "mmproj"), ("m", "model")] {
        let Some(after) = rest.strip_prefix(name) else {
            continue;
        };
        let Some(after) = after.trim_start_matches(js_space).strip_prefix('=') else {
            continue;
        };
        if let Some(v) = trimmed_value(after) {
            return Some((kind, v));
        }
    }
    None
}

/// An error llamacpp-presets.cjs throws: its status (when it set one) and message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Thrown {
    pub status: Option<u16>,
    pub message: String,
}

impl Thrown {
    fn plain(message: impl Into<String>) -> Thrown {
        Thrown {
            status: None,
            message: message.into(),
        }
    }
}

struct Section {
    name: String,
    start: usize,
    end: usize,
    /// Canonical key and value, in the object's insertion order (a later line keeps the place).
    options: Vec<(&'static str, String)>,
}

pub struct Presets {
    pub revision: String,
    /// The file's text as read (UTF-8, lossy), which `revision` hashes.
    pub text: String,
    lines: Vec<String>,
    sections: Vec<Section>,
}

fn put(options: &mut Vec<(&'static str, String)>, key: &'static str, value: String) {
    match options.iter_mut().find(|(k, _)| *k == key) {
        Some(slot) => slot.1 = value,
        None => options.push((key, value)),
    }
}

/// llamacpp-presets.cjs `read()` and `parse()`.
pub fn read_presets(file: &Path) -> Result<Presets, Thrown> {
    let meta = std::fs::symlink_metadata(file).map_err(|e| Thrown::plain(e.to_string()))?;
    if !meta.is_file() || meta.file_type().is_symlink() {
        return Err(Thrown {
            status: Some(409),
            message: "Preset path must be a regular file".into(),
        });
    }
    let bytes = std::fs::read(file).map_err(|e| Thrown::plain(e.to_string()))?;
    let text = String::from_utf8_lossy(&bytes).into_owned();
    if text.len() > 1024 * 1024 {
        return Err(Thrown {
            status: Some(413),
            message: "Preset file exceeds the editor limit".into(),
        });
    }
    let revision = hex(&text);
    let lines: Vec<String> = text.split('\n').map(str::to_owned).collect();
    let mut sections: Vec<Section> = Vec::new();
    for (index, line) in lines.iter().enumerate() {
        if let Some(name) = header_of(line) {
            if sections.iter().any(|s| s.name == name) {
                return Err(Thrown {
                    status: Some(409),
                    message: "Duplicate preset sections require operator repair".into(),
                });
            }
            if let Some(previous) = sections.last_mut() {
                previous.end = index;
            }
            sections.push(Section {
                name: name.to_owned(),
                start: index,
                end: lines.len(),
                options: Vec::new(),
            });
        } else if let Some(current) = sections.last_mut() {
            if let Some((key, value)) = option_of(line) {
                if let Some(key) = canonical(key) {
                    put(&mut current.options, key, value.to_owned());
                }
            }
        }
    }
    Ok(Presets {
        revision,
        text,
        lines,
        sections,
    })
}

fn hex(text: &str) -> String {
    use sha2::{Digest, Sha256};
    Sha256::digest(text.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

impl Presets {
    fn section(&self, name: &str) -> Option<&Section> {
        self.sections.iter().find(|s| s.name == name)
    }

    /// `{...profile.defaults, ...profile.options}` of `get(model)`.
    pub fn current(&self, model: &str) -> Vec<(&'static str, String)> {
        let mut out = self
            .section("*")
            .map(|s| s.options.clone())
            .unwrap_or_default();
        if let Some(s) = self.section(model) {
            for (k, v) in &s.options {
                put(&mut out, k, v.clone());
            }
        }
        out
    }

    /// `files(model)`: the container paths its section loads.
    pub fn files(&self, model: &str) -> (Option<String>, Option<String>) {
        let (mut m, mut mm) = (None, None);
        if let Some(s) = self.section(model) {
            for line in self.lines.get(s.start + 1..s.end).into_iter().flatten() {
                match file_of(line) {
                    Some(("model", v)) => m = Some(v.to_owned()),
                    Some((_, v)) => mm = Some(v.to_owned()),
                    None => {}
                }
            }
        }
        (m, mm)
    }

    /// The speculative draft model's file: not an editable option, so `parse` does not keep it
    /// (`draftFile` in core `llamacpp-presets.cjs`, sbstndalton/noevia#2215).
    pub fn draft_file(&self, model: &str) -> Option<String> {
        let s = self.section(model)?;
        let mut file = None;
        for line in self.lines.get(s.start + 1..s.end).into_iter().flatten() {
            if let Some((key, value)) = line.split_once('=') {
                let value = value.trim();
                if key.trim() == "spec-draft-model" && !value.is_empty() {
                    file = Some(value.to_owned());
                }
            }
        }
        file
    }
}

// ── The model's files (llamacpp-manager.cjs modelFiles, localFile) ───────────────────────────

/// A file reference as the JS holds it: a string, or a truthy value of another type.
#[derive(Debug, Clone, PartialEq, Eq)]
enum FileRef {
    Path(String),
    NotAString,
}

/// What the listing's `status.args` was for the model.
enum Args {
    List(Vec<Value>),
    /// A truthy non-array: `args.findIndex` throws outside the JS's try.
    Throws,
}

fn units_eq(a: &[u16], b: &str) -> bool {
    a.iter().copied().eq(b.encode_utf16())
}

/// `listing.body?.data?.find(m=>m.id===model)?.status?.args||[]`, inside the JS's try.
fn args_of(listing: &Read, model: &str) -> Args {
    let empty = Args::List(Vec::new());
    let Read::Done { body, .. } = listing else {
        return empty;
    };
    let Some(Value::Arr(rows)) = body.get("data") else {
        // Missing, null, or not an array (`find` is not a function: caught).
        return empty;
    };
    for row in rows {
        match row {
            // `m.id` of null throws inside the try.
            Value::Null | Value::Deep => return empty,
            Value::Obj(_) => {}
            _ => continue,
        }
        if !matches!(row.get("id"), Some(Value::Str(s)) if units_eq(s, model)) {
            continue;
        }
        let args = match row.get("status") {
            Some(status @ Value::Obj(_)) => status.get("args"),
            Some(Value::Deep) => return empty,
            _ => None,
        };
        return match args {
            Some(Value::Arr(items)) => Args::List(items.clone()),
            v if !server_stats::js::truthy(v) => empty,
            _ => Args::Throws,
        };
    }
    empty
}

/// `flag(names) || fallback`.
fn flag(args: &[Value], names: &[&str], fallback: Option<String>) -> Option<FileRef> {
    let i = args
        .iter()
        .position(|a| matches!(a, Value::Str(s) if names.iter().any(|n| units_eq(s, n))));
    let v = i.and_then(|i| args.get(i + 1));
    if server_stats::js::truthy(v) {
        return Some(match v {
            Some(Value::Str(s)) => FileRef::Path(String::from_utf16_lossy(s)),
            _ => FileRef::NotAString,
        });
    }
    fallback.map(FileRef::Path)
}

/// Node `path.posix.join(root, rest)` (normalised; a trailing slash kept).
fn join(root: &str, rest: &str) -> String {
    let joined = if rest.is_empty() {
        root.to_owned()
    } else {
        format!("{root}/{rest}")
    };
    let absolute = joined.starts_with('/');
    let trailing = joined.ends_with('/');
    let mut parts: Vec<&str> = Vec::new();
    for seg in joined.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                if parts.last().is_some_and(|p| *p != "..") {
                    parts.pop();
                } else if !absolute {
                    parts.push("..");
                }
            }
            s => parts.push(s),
        }
    }
    let mut out = parts.join("/");
    if absolute {
        out.insert(0, '/');
    }
    if out.is_empty() {
        out.push('.');
    }
    if trailing && !out.ends_with('/') {
        out.push('/');
    }
    out
}

impl Sizing {
    /// [`Sizing::local_file`] for a container path (the maintenance work, model_maint).
    pub(crate) fn local_file_path(&self, container: &str) -> Option<(PathBuf, u64)> {
        self.local_file(Some(&FileRef::Path(container.to_owned())))
    }

    /// llamacpp-manager.cjs `localFile`: the real file and its size under a read-only mount.
    fn local_file(&self, r: Option<&FileRef>) -> Option<(PathBuf, u64)> {
        let Some(FileRef::Path(container)) = r else {
            return None;
        };
        for (prefix, root) in [
            ("/models/", &self.models_path),
            ("/cache/", &self.cache_path),
        ] {
            if root.is_empty() {
                continue;
            }
            let Some(rest) = container.strip_prefix(prefix) else {
                continue;
            };
            let base = std::fs::canonicalize(root).ok()?;
            let real = std::fs::canonicalize(join(root, rest)).ok()?;
            let mut within = base.into_os_string();
            within.push("/");
            if !real
                .as_os_str()
                .as_encoded_bytes()
                .starts_with(within.as_encoded_bytes())
            {
                return None;
            }
            let meta = std::fs::metadata(&real).ok()?;
            let size = meta.len();
            return meta.is_file().then_some((real, size));
        }
        None
    }
}

// ── GGUF header (gguf-meta.cjs readSummaryWasm) ──────────────────────────────────────────────

const READ_CHUNK: u64 = 4 * 1024;
const MAX_ROUNDS: usize = 64;
const MAX_SENT_WINDOWS: u64 = 8;

struct Held {
    off: u64,
    bytes: Vec<u8>,
}

fn read_range(f: &mut std::fs::File, from: u64, to: u64) -> Result<Vec<u8>, String> {
    let len = usize::try_from(to.saturating_sub(from))
        .map_err(|_| "GGUF length out of range".to_string())?;
    let mut out = vec![0u8; len];
    f.seek(SeekFrom::Start(from))
        .map_err(|_| "Unexpected end of GGUF header".to_string())?;
    let mut at = 0;
    while at < len {
        let Some(rest) = out.get_mut(at..) else {
            break;
        };
        let n = f.read(rest).map_err(|e| format!("{}: {e}", io_code(&e)))?;
        if n == 0 {
            return Err("Unexpected end of GGUF header".into());
        }
        at += n;
    }
    Ok(out)
}

fn io_code(e: &std::io::Error) -> &'static str {
    match e.kind() {
        std::io::ErrorKind::PermissionDenied => "EACCES",
        std::io::ErrorKind::NotFound => "ENOENT",
        _ => "EIO",
    }
}

/// The summary JSON text (crates/gguf `node::summary`, with the wasm wrapper's checks), or the
/// message the JS throws.
fn read_summary(file: &Path) -> Result<String, String> {
    let mut f = std::fs::File::open(file).map_err(|e| {
        let what = match e.kind() {
            std::io::ErrorKind::PermissionDenied => "permission denied",
            std::io::ErrorKind::NotFound => "no such file or directory",
            _ => "i/o error",
        };
        format!("{}: {what}, open '{}'", io_code(&e), file.display())
    })?;
    let size = f
        .metadata()
        .map_err(|e| format!("{}: {e}", io_code(&e)))?
        .len();
    let max_window = gguf::node::MAX_WINDOW_BYTES as u64;
    let too_large = || "GGUF header could not be checked (too_large)".to_string();
    let first = size.min(READ_CHUNK).min(max_window);
    let mut segs: Vec<Held> = if first > 0 {
        vec![Held {
            off: 0,
            bytes: read_range(&mut f, 0, first)?,
        }]
    } else {
        Vec::new()
    };
    let byte_budget = MAX_SENT_WINDOWS * max_window;
    let mut sent = 0u64;
    let len = |h: &Held| h.bytes.len() as u64;
    for round in 0.. {
        if round >= MAX_ROUNDS {
            return Err(too_large());
        }
        sent += segs.iter().map(len).sum::<u64>();
        if sent > byte_budget {
            return Err(too_large());
        }
        // dav-parse-wasm.cjs ggufSummary's own checks.
        if segs.len() > gguf::node::MAX_SEGMENTS || segs.iter().map(len).sum::<u64>() > max_window {
            return Err(too_large());
        }
        let view: Vec<gguf::node::Segment<'_>> = segs
            .iter()
            .map(|h| gguf::node::Segment {
                off: h.off,
                bytes: &h.bytes,
            })
            .collect();
        let (at, end) = match gguf::node::summary(size, &view) {
            Ok(text) => return Ok(text),
            Err(gguf::node::Fault::Need { at, end }) => {
                let held = segs.iter().any(|s| s.off <= at && end <= s.off + len(s));
                if held || at >= end || end > size {
                    return Err("GGUF header could not be checked (reply)".into());
                }
                (at, end)
            }
            Err(gguf::node::Fault::Depth) => {
                return Err("GGUF header could not be checked (depth)".into())
            }
            Err(gguf::node::Fault::Kept) => {
                return Err("GGUF header could not be checked (kept)".into())
            }
            Err(fault) => {
                return Err(fault
                    .js_message()
                    .unwrap_or_else(|| "GGUF header could not be checked (reply)".into()))
            }
        };
        let run = segs
            .iter()
            .find(|x| x.off <= at && at <= x.off + len(x) + READ_CHUNK);
        let start = run.map_or(at, |r| at.min(r.off + len(r)));
        let run_len = run.map_or(0, len);
        // Merge every held range [start, hi) touches into one: (lo, hi, kept indices, total).
        let merge = |hi0: u64| {
            let (mut lo, mut hi) = (start, hi0);
            let mut keep = Vec::new();
            for (i, x) in segs.iter().enumerate() {
                if x.off <= hi && x.off + len(x) >= lo {
                    lo = lo.min(x.off);
                    hi = hi.max(x.off + len(x));
                } else {
                    keep.push(i);
                }
            }
            let total = keep
                .iter()
                .filter_map(|&i| segs.get(i))
                .map(len)
                .sum::<u64>()
                + (hi - lo);
            (lo, hi, keep, total)
        };
        let generous = size.min(end.max(at + run_len.max(READ_CHUNK)));
        let least = merge(end);
        if least.3 > max_window || least.2.len() >= gguf::node::MAX_SEGMENTS {
            return Err(too_large());
        }
        let mut m = merge(generous.min(least.1 + (max_window - least.3)));
        if m.3 > max_window {
            m = least;
        }
        let bytes = read_range(&mut f, m.0, m.1)?;
        let mut next: Vec<Held> = Vec::with_capacity(m.2.len() + 1);
        let mut old: Vec<Option<Held>> = segs.into_iter().map(Some).collect();
        for i in m.2 {
            if let Some(h) = old.get_mut(i).and_then(Option::take) {
                next.push(h);
            }
        }
        next.push(Held { off: m.0, bytes });
        next.sort_by_key(|h| h.off);
        segs = next;
    }
    Err(too_large())
}

// ── The answers ──────────────────────────────────────────────────────────────────────────────

/// A JSON string as `JSON.stringify` writes it.
fn js_string(s: &str) -> String {
    serde_json::to_string(s).unwrap_or_else(|_| "\"\"".into())
}

fn error_body(message: &str) -> String {
    format!(r#"{{"error":{}}}"#, js_string(message))
}

/// Where the read stopped before the sizing: an answer readModel gave, or a throw.
enum Stop {
    Answer(u16, String),
    Thrown(Thrown),
}

/// What readModel read: the summary text and the two sizes.
struct ModelRead {
    summary: String,
    model_bytes: u64,
    mmproj_bytes: u64,
}

impl Sizing {
    /// llamacpp-manager.cjs `readModel` after `modelFiles`' listing read.
    fn read_model(&self, file: &Path, model: &str, listing: &Read) -> Result<ModelRead, Stop> {
        // `fromPreset` (presets.files, which may throw) is read before `flag` runs, and only
        // `flag` finds out that a truthy `args` is not an array.
        let args = args_of(listing, model);
        let (preset_model, preset_mmproj) = read_presets(file).map_err(Stop::Thrown)?.files(model);
        let args = match args {
            Args::List(a) => a,
            Args::Throws => {
                return Err(Stop::Thrown(Thrown::plain(
                    "args.findIndex is not a function",
                )))
            }
        };
        let model_ref = flag(&args, &["--model", "-m"], preset_model);
        let mmproj_ref = flag(&args, &["--mmproj", "-mm"], preset_mmproj)
            .filter(|r| !matches!(r, FileRef::Path(p) if p.is_empty()));
        let Some((model_file, model_bytes)) = self.local_file(model_ref.as_ref()) else {
            return Err(Stop::Answer(404, error_body(NO_MODEL_FILE)));
        };
        let mmproj = match &mmproj_ref {
            Some(r) => match self.local_file(Some(r)) {
                Some(found) => Some(found),
                None => return Err(Stop::Answer(404, error_body(NO_PROJECTOR))),
            },
            None => None,
        };
        let summary = read_summary(&model_file).map_err(|m| {
            Stop::Answer(
                422,
                error_body(&format!("Could not read model metadata: {m}")),
            )
        })?;
        Ok(ModelRead {
            summary,
            model_bytes,
            mmproj_bytes: mmproj.map_or(0, |(_, n)| n),
        })
    }
}

/// The summary as `JSON.stringify` hands the revived object to the strict port: NaN and
/// ±Infinity become `null`, -0 becomes `0` (see the module documentation).
fn meta_json(summary: &str) -> String {
    if !summary.contains("{\"$num\":") {
        return summary.to_owned();
    }
    summary
        .replace(r#"{"$num":"-0"}"#, "0")
        .replace(r#"{"$num":"NaN"}"#, "null")
        .replace(r#"{"$num":"Infinity"}"#, "null")
        .replace(r#"{"$num":"-Infinity"}"#, "null")
}

fn current_json(current: &[(&'static str, String)]) -> String {
    let mut out = String::from("{");
    for (i, (k, v)) in current.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push_str(&js_string(k));
        out.push(':');
        out.push_str(&js_string(v));
    }
    out.push('}');
    out
}

fn num(n: f64) -> String {
    if n.is_finite() {
        llamacpp_autoconfig::number_string(n)
    } else {
        "null".into()
    }
}

/// One strict port operation: the answer text, or `None` when the port refused.
fn port(op: u8, args: &str) -> Option<String> {
    let mut input = Vec::with_capacity(args.len() + 1);
    input.push(op);
    input.extend_from_slice(args.as_bytes());
    let (status, text) = llamacpp_autoconfig::call(&input);
    (status == 0 && text.starts_with('{') && text.len() > 2).then_some(text)
}

/// `{"a":…,"b":…, ...rest}` where `rest` is an object's JSON text with none of the leading keys.
fn spread(lead: &str, rest: &str) -> String {
    format!("{lead},{}", rest.strip_prefix('{').unwrap_or(rest))
}

impl Sizing {
    /// The answer for `route` (status, JSON text) to a signed-in caller. `model` is
    /// `url.searchParams.get('model')`.
    pub async fn answer(
        self: &Arc<Self>,
        route: Route,
        admin: bool,
        model: Option<String>,
        budget: Arc<dyn Fn() -> Result<Option<SettingValue>, ()> + Send + Sync>,
    ) -> (u16, String) {
        if !admin {
            return (403, error_body(ADMIN_REQUIRED));
        }
        let model = model.unwrap_or_default();
        if self.kind != server_stats::Kind::LlamaCpp {
            return match route {
                Route::Estimate => (404, error_body("Memory estimates need the native engine")),
                Route::Suggest => (404, error_body("Native preset suggestions are unavailable")),
            };
        }
        if route == Route::Estimate && (model.is_empty() || model.encode_utf16().count() > 200) {
            return (400, error_body("Choose a model"));
        }
        let Some(file) = self.presets.clone() else {
            return (
                501,
                error_body(&format!(
                    "{}; configure LLAMACPP_PRESET_PATH is not exposed by this llama.cpp adapter",
                    match route {
                        Route::Estimate => "Memory estimates",
                        Route::Suggest => "Native preset suggestions",
                    }
                )),
            );
        };
        if self.models_path.is_empty() {
            return (
                501,
                error_body(match route {
                    Route::Estimate => "Estimates need the model directory mounted read-only; set LLAMACPP_MODELS_PATH.",
                    Route::Suggest => "Suggestions need the model directory mounted read-only; set LLAMACPP_MODELS_PATH.",
                }),
            );
        }
        // The budget (Node: sizingBudgetGib() for budgetFaultReason), then presets.get(model).
        let me = Arc::clone(self);
        let (m, f) = (model.clone(), file.clone());
        let first = tokio::task::spawn_blocking(move || {
            let setting = budget()?;
            let budget_gib = me.sizing_budget_gib(setting.as_ref());
            Ok::<_, ()>((
                budget_gib,
                read_presets(&f).map(|p| (p.revision.clone(), p.current(&m))),
            ))
        })
        .await;
        let Ok(Ok((budget_gib, profile))) = first else {
            return (
                503,
                error_body("The inference memory budget could not be read."),
            );
        };
        if route == Route::Suggest
            && budget_gib.partial_cmp(&0.0) != Some(std::cmp::Ordering::Greater)
        {
            return (501, error_body("Suggestions need an inference memory budget; set LLAMACPP_AUTOCONFIG_MEMORY_GIB or LLAMACPP_MEMORY_LIMIT."));
        }
        let thrown = |t: Thrown| -> (u16, String) {
            let public = t.status.is_some_and(|s| s < 500) && !t.message.is_empty();
            match route {
                // routes/models.cjs: 500 with clientMessage(e, …).
                Route::Estimate => (
                    500,
                    error_body(if public {
                        &t.message
                    } else {
                        "Could not estimate memory for this model."
                    }),
                ),
                // index.cjs errorResponse: a 4xx keeps its status and message.
                Route::Suggest => match t.status {
                    Some(s) if (400..500).contains(&s) => (s, error_body(&t.message)),
                    _ => (500, error_body("Internal error")),
                },
            }
        };
        let (revision, current) = match profile {
            Ok(p) => p,
            Err(t) => return thrown(t),
        };
        // modelFiles: the router's listing (a failure or a timeout is no listing).
        let listing = self.engine.listing().await;
        let me = Arc::clone(self);
        let m = model.clone();
        let read = tokio::task::spawn_blocking(move || me.read_model(&file, &m, &listing)).await;
        let read = match read {
            Ok(Ok(r)) => r,
            Ok(Err(Stop::Answer(status, body))) => return (status, body),
            Ok(Err(Stop::Thrown(t))) => return thrown(t),
            Err(_) => return thrown(Thrown::plain("")),
        };
        let meta = meta_json(&read.summary);
        let sizes = format!(
            r#""modelBytes":{},"mmprojBytes":{}"#,
            num(read.model_bytes as f64),
            num(read.mmproj_bytes as f64)
        );
        let current = current_json(&current);
        match route {
            Route::Estimate => {
                let args = format!(r#"{{"meta":{meta},{sizes},"current":{current}}}"#);
                let Some(inputs) = port(2, &args) else {
                    return (
                        503,
                        format!(
                            r#"{{"error":{},"code":"autoconfig_impl"}}"#,
                            js_string(IMPL_INPUTS_ERROR)
                        ),
                    );
                };
                let budget = if budget_gib > 0.0 {
                    num(budget_gib)
                } else {
                    "null".into()
                };
                (
                    200,
                    spread(
                        &format!(r#"{{"model":{},"budgetGib":{budget}"#, js_string(&model)),
                        &inputs,
                    ),
                )
            }
            Route::Suggest => {
                let args = format!(
                    r#"{{"meta":{meta},{sizes},"budgetGib":{},"current":{current},"cacheRamMaxMib":{}}}"#,
                    num(budget_gib),
                    num(self.cache_ram_max_mib)
                );
                let arch = serde_json::from_str::<serde_json::Value>(&read.summary)
                    .ok()
                    .and_then(|v| v.get("arch").and_then(|a| a.as_str()).map(js_string))
                    .unwrap_or_else(|| "\"\"".into());
                let lead = format!(
                    r#"{{"model":{},"revision":{},"arch":{arch}"#,
                    js_string(&model),
                    js_string(&revision)
                );
                let result = port(1, &args).unwrap_or_else(|| {
                    format!(
                        r#"{{"error":{},"code":"autoconfig_impl","unverified":"impl_refused"}}"#,
                        js_string(IMPL_SUGGEST_ERROR)
                    )
                });
                (200, spread(&lead, &result))
            }
        }
    }
}

/// `url.searchParams.get('model')` of the request target.
pub fn model_param(uri: &axum::http::Uri) -> Option<String> {
    let target = uri
        .path_and_query()
        .map_or_else(|| uri.path(), |pq| pq.as_str());
    let base = url::Url::parse("http://h").ok()?;
    let parsed = url::Url::options()
        .base_url(Some(&base))
        .parse(target)
        .ok()?;
    parsed
        .query_pairs()
        .find(|(k, _)| k == "model")
        .map(|(_, v)| v.into_owned())
}

/// The saved inference budget setting, through the front's read-only store.
#[allow(clippy::result_unit_err)]
pub fn budget_setting(
    identity: &crate::identity::IdentityLayer,
) -> Result<Option<SettingValue>, ()> {
    use server_store::rusqlite::types::ValueRef;
    let store = identity.store_blocking().map_err(|_| ())?;
    store
        .read(|reader| {
            reader.row(
                "SELECT value FROM settings WHERE key=?1",
                [BUDGET_KEY],
                |row| {
                    Ok(match row.get_ref(0)? {
                        ValueRef::Text(t) | ValueRef::Blob(t) => {
                            SettingValue::Text(String::from_utf8_lossy(t).into_owned())
                        }
                        ValueRef::Integer(_) | ValueRef::Real(_) => SettingValue::Number,
                        ValueRef::Null => SettingValue::Null,
                    })
                },
            )
        })
        .map_err(|_| ())
}

pub fn respond(status: u16, body: String) -> Response<Body> {
    reply::json_text(
        StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
        body,
        true,
    )
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    #[test]
    fn ini_lines_follow_the_javascript_regexes() {
        assert_eq!(header_of("[a]"), Some("a"));
        assert_eq!(header_of("  [a b] ; note"), Some("a b"));
        assert_eq!(header_of("[a]\r"), Some("a"), "\\s takes the CR");
        assert_eq!(header_of("[a] ;c\r"), None, ". does not take the CR");
        assert_eq!(header_of("[]"), None);
        assert_eq!(header_of("[a] x"), None);
        assert_eq!(header_of("\u{feff}[a]"), Some("a"), "the BOM is \\s");
        assert_eq!(option_of(" model   =  /m/x  "), Some(("model", "/m/x")));
        assert_eq!(option_of("c=\u{2028}8192"), Some(("c", "8192")));
        assert_eq!(option_of("c = 81\u{2028}92"), None);
        assert_eq!(option_of("k ="), Some(("k", "")));
        assert_eq!(option_of("= v"), None);
        assert_eq!(option_of("a b = c"), None);
        assert_eq!(file_of("mmproj = /p"), Some(("mmproj", "/p")));
        assert_eq!(file_of("m=/q"), Some(("model", "/q")));
        assert_eq!(file_of("models = /q"), None);
        assert_eq!(canonical("LLAMA_ARG_CACHE_RAM"), Some("cache-ram"));
        assert_eq!(canonical("unknown"), None);
    }

    #[test]
    fn join_is_node_path_join() {
        assert_eq!(join("/r/models", "a/../b.gguf"), "/r/models/b.gguf");
        assert_eq!(join("/r/models", "../outside"), "/r/outside");
        assert_eq!(join("/r/models", ""), "/r/models");
        assert_eq!(join("/r/models/", "sub/"), "/r/models/sub/");
        assert_eq!(join("rel", "../../x"), "../x");
        assert_eq!(join("/", "../../x"), "/x");
    }

    #[test]
    fn the_saved_budget_is_read_like_number_of_json_parse() {
        let t = |s: &str| saved_budget(&SettingValue::Text(s.into()));
        assert_eq!(t(r#"{"budgetGib":6}"#), Some(6.0));
        assert_eq!(t(r#"{"budgetGib":" 7 "}"#), Some(7.0));
        assert_eq!(t(r#"{"budgetGib":[3]}"#), Some(3.0));
        assert_eq!(t(r#"{"budgetGib":[1,2]}"#), None);
        assert_eq!(t(r#"{"budgetGib":true}"#), Some(1.0));
        assert_eq!(t(r#"{"budgetGib":0}"#), None);
        assert_eq!(t(r#"{"budgetGib":"0x10"}"#), Some(16.0));
        assert_eq!(t("[6]"), None);
        assert_eq!(t("not json"), None);
        assert_eq!(t(""), None);
        assert_eq!(saved_budget(&SettingValue::Number), None);
    }

    #[test]
    fn host_ram_is_memtotal_to_one_decimal() {
        assert_eq!(
            host_ram_gib(Some("MemFree: 1 kB\nMemTotal:       16303324 kB\n")),
            Some(15.5)
        );
        assert_eq!(host_ram_gib(Some("MemTotal: 0 kB")), None);
        assert_eq!(host_ram_gib(Some("nothing")), None);
        assert_eq!(host_ram_gib(None), None);
    }

    #[test]
    fn the_budget_is_clamped_to_the_host_and_lowered_only_by_the_explicit_figure() {
        let dir = tempfile::tempdir().unwrap();
        let meminfo = dir.path().join("meminfo");
        std::fs::write(&meminfo, "MemTotal: 16777216 kB\n").unwrap();
        let engine = Arc::new(
            server_stats::Engine::new(
                server_stats::Kind::LlamaCpp,
                "http://127.0.0.1:9/v1",
                None,
                server_stats::Timeouts::default(),
            )
            .unwrap(),
        );
        let make = |pairs: &[(&str, &str)]| {
            let mut env: std::collections::HashMap<String, String> = pairs
                .iter()
                .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
                .collect();
            env.insert(
                "NOEVIA_TEST_MEMINFO_PATH".into(),
                meminfo.display().to_string(),
            );
            Sizing::from_lookup(server_stats::Kind::LlamaCpp, Arc::clone(&engine), &|k| {
                env.get(k).cloned()
            })
        };
        let saved = |v: &str| SettingValue::Text(format!(r#"{{"budgetGib":{v}}}"#));
        // 16 GiB host: at most 12.
        assert_eq!(make(&[]).sizing_budget_gib(None), 12.0);
        assert_eq!(make(&[]).sizing_budget_gib(Some(&saved("40"))), 12.0);
        assert_eq!(make(&[]).sizing_budget_gib(Some(&saved("1"))), 2.0);
        let s = make(&[("INFERENCE_MEMORY_BUDGET_GIB", "5")]);
        assert_eq!(s.sizing_budget_gib(None), 5.0);
        assert_eq!(s.sizing_budget_gib(Some(&saved("7"))), 7.0);
        let s = make(&[("LLAMACPP_AUTOCONFIG_MEMORY_GIB", "3.5")]);
        assert_eq!(s.sizing_budget_gib(Some(&saved("8"))), 3.5);
        let s = make(&[("LLAMACPP_AUTOCONFIG_MEMORY_GIB", "-1")]);
        assert_eq!(s.sizing_budget_gib(Some(&saved("8"))), 8.0);
        let s = make(&[
            ("LLAMACPP_AUTOCONFIG_CACHE_RAM_MAX_MIB", "4000"),
            ("LLAMACPP_CACHE_RAM_HARD_MAX_MIB", "3000.4"),
        ]);
        assert_eq!(s.cache_ram_max_mib, 3000.0);
        assert_eq!(make(&[]).cache_ram_max_mib, 1024.0);
    }
}
