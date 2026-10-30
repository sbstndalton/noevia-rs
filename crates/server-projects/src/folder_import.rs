//! core folder-import.cjs (sbstndalton/noevia#2191 round 8): import a whole folder from this computer,
//! and read every subfolder of an attached storage folder. The rules live here so the upload, the
//! plan and the folder refresh agree.
//!
//! - [`relative_path`]: a folder import names each file by its path inside the chosen folder. NFC,
//!   backslashes become `/`, empty segments collapse, and anything that could leave the folder is
//!   refused, never rewritten.
//! - [`skip_reason`] / [`skipped_name`]: one shared list (contracts/folder-import.json, a copy in
//!   `data/` pinned by hash to noevia-core's) of system and tool folders and files, skipped at any
//!   depth, every hidden (dot) name, and any type a project cannot read.
//! - [`plan`] + tickets: the client sends the manifest (path, size, SHA-256) first; the plan says what
//!   will be uploaded, what is unchanged and what is skipped and why, and opens an import id that
//!   only the accepted paths can be uploaded under ([`admit`], [`consume`]). The tickets live in this
//!   process's memory, per person (4), at most 1024 overall, an hour each, as core's did.
//! - [`list_tree`]: a breadth-first walk of an attached folder, bounded by depth and folder count.
use crate::source_limits::{self, Kind};
use crate::source_storage::Entry;
use crate::{document_pages::string_to_number, js};
use js_json::{wtf16, JValue};
use sha2::{Digest, Sha256};
use std::collections::{BTreeSet, HashMap};
use std::sync::{Mutex, OnceLock};
use unicode_normalization::UnicodeNormalization;

/// The pinned copy of noevia-core's contracts/folder-import.json.
pub const CONTRACT_JSON: &str = include_str!("../data/folder-import.json");
/// Its SHA-256; noevia-core pins the same value for its own file (folder-import.test.cjs), so a
/// change on either side fails there until both move together.
pub const CONTRACT_SHA256: &str =
    "eb1ab98abd6fe6f2f881cfe3d09c1609f76af8feb21359ff48bfe7e338e84cb4";

pub struct Contract {
    skip_names: BTreeSet<String>,
    skip_hidden: bool,
    readable: BTreeSet<String>,
    /// sources, documents, images.
    pub project_limits: (usize, usize, usize),
    pub max_bytes: usize,
    pub max_depth: usize,
    pub max_folders: usize,
}

pub fn contract() -> &'static Contract {
    static C: OnceLock<Contract> = OnceLock::new();
    C.get_or_init(|| {
        let v: serde_json::Value = serde_json::from_str(CONTRACT_JSON).unwrap_or_default();
        let list = |key: &str| -> BTreeSet<String> {
            v.get(key)
                .and_then(serde_json::Value::as_array)
                .map(|a| {
                    a.iter()
                        .filter_map(|s| s.as_str().map(str::to_owned))
                        .collect()
                })
                .unwrap_or_default()
        };
        let num = |value: Option<&serde_json::Value>, fallback: usize| {
            value
                .and_then(serde_json::Value::as_u64)
                .and_then(|n| usize::try_from(n).ok())
                .unwrap_or(fallback)
        };
        let limit = |key: &str| v.get("projectLimits").and_then(|p| p.get(key));
        Contract {
            skip_names: list("skipNames")
                .into_iter()
                .map(|n| n.to_lowercase())
                .collect(),
            skip_hidden: v
                .get("skipHidden")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(true),
            readable: list("readableExtensions"),
            project_limits: (
                num(limit("sources"), 1000),
                num(limit("documents"), 200),
                num(limit("images"), 12),
            ),
            max_bytes: num(v.get("maxBytes"), 104_857_600),
            max_depth: num(v.get("maxDepth"), 16),
            max_folders: num(v.get("maxFolders"), 400),
        }
    })
}

const PATH_MAX: usize = 1000;
const SEGMENT_MAX: usize = 200;
const TICKET_TTL_MS: i64 = 60 * 60 * 1000;
const TICKETS_PER_USER: usize = 4;
const TICKETS_MAX: usize = 1024;
pub const MANIFEST_MAX: usize = 20000;

/// A refusal of a path or a plan: core's 400 with `code: 'invalidPath'` (or the status given).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refused {
    pub status: u16,
    pub message: String,
    pub code: Option<&'static str>,
}

fn refuse(message: impl Into<String>) -> Refused {
    Refused {
        status: 400,
        message: message.into(),
        code: Some("invalidPath"),
    }
}

/// `limits(env)`: the caps in force for one import.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    pub max_files: usize,
    pub max_bytes: usize,
    pub max_depth: usize,
    pub max_folders: usize,
}

fn positive_env(key: &str) -> Option<usize> {
    let raw = std::env::var(key).ok().filter(|r| !r.is_empty())?;
    let n = string_to_number(&raw);
    (n.is_finite() && n.fract() == 0.0 && n > 0.0 && n <= 9_007_199_254_740_991.0)
        .then_some(n as usize)
}

pub fn limits() -> Limits {
    let c = contract();
    Limits {
        max_files: positive_env("FOLDER_IMPORT_MAX_FILES")
            .unwrap_or_else(|| source_limits::limits().sources),
        max_bytes: positive_env("FOLDER_IMPORT_MAX_BYTES").unwrap_or(c.max_bytes),
        max_depth: c.max_depth,
        max_folders: c.max_folders,
    }
}

/// JS `String.prototype.length` of `s`.
fn len16(s: &str) -> usize {
    s.encode_utf16().count()
}

/// `relativePath(raw)`: the canonical relative path (NFC, `/`-separated), or a 400.
pub fn relative_path(raw: &JValue) -> Result<String, Refused> {
    let JValue::Str(raw) = raw else {
        return Err(refuse(
            "A folder import names each file by its path inside the folder.",
        ));
    };
    let value: String = raw.nfc().collect::<String>().replace('\\', "/");
    if value.is_empty() || len16(&value) > PATH_MAX {
        return Err(refuse("That path is empty or too long."));
    }
    let mut chars = value.chars();
    let (first, second) = (chars.next(), chars.next());
    if first == Some('/') || (first.is_some_and(|c| c.is_ascii_alphabetic()) && second == Some(':'))
    {
        return Err(refuse(
            "An absolute path cannot be imported; send the path inside the folder.",
        ));
    }
    if value.chars().any(|c| c <= '\u{1f}' || c == '\u{7f}') {
        return Err(refuse("That path contains a control character."));
    }
    let segments: Vec<&str> = value.split('/').filter(|s| !s.is_empty()).collect();
    if segments.is_empty() {
        return Err(refuse("That path is empty."));
    }
    if segments.iter().any(|s| *s == "." || *s == "..") {
        return Err(refuse("A path that leaves the folder cannot be imported."));
    }
    if segments
        .iter()
        .any(|s| len16(s) > SEGMENT_MAX || js::trim(s) != *s || js::trim(s).is_empty())
    {
        return Err(refuse(
            "A folder or file name in that path is too long or blank at its ends.",
        ));
    }
    let depth = contract().max_depth;
    if segments.len() > depth {
        return Err(refuse(format!(
            "That file is more than {depth} folders deep."
        )));
    }
    Ok(segments.join("/"))
}

fn extension_of(name: &str) -> String {
    match name.rfind('.') {
        Some(dot) if dot > 0 => name.get(dot..).unwrap_or("").to_lowercase(),
        _ => String::new(),
    }
}

/// The case-folded key two names collide under on a case-insensitive disk.
pub fn fold_key(name: &str) -> String {
    name.nfc().collect::<String>().to_lowercase()
}

/// `skippedName(name)`: `system`, `hidden` or none.
pub fn skipped_name(name: &str) -> Option<&'static str> {
    let c = contract();
    if c.skip_names.contains(&name.to_lowercase()) {
        return Some("system");
    }
    if c.skip_hidden && name.starts_with('.') {
        return Some("hidden");
    }
    None
}

/// `skipReason(rel)`: `(code, at)` or none.
pub fn skip_reason(rel: &str) -> Option<(&'static str, String)> {
    for s in rel.split('/') {
        if let Some(why) = skipped_name(s) {
            return Some((why, s.to_owned()));
        }
    }
    let name = rel.rsplit('/').next().unwrap_or(rel);
    let ext = extension_of(name);
    if !contract().readable.contains(&ext) {
        let at = if ext.is_empty() { name.to_owned() } else { ext };
        return Some(("unsupported", at));
    }
    None
}

/// `REASONS[code]`.
pub fn reason(code: &str) -> &'static str {
    match code {
        "invalidPath" => "This path is not a safe path inside the folder.",
        "system" => "System or tool folder, skipped on purpose.",
        "hidden" => "Hidden file or folder, skipped on purpose.",
        "unsupported" => "This file type cannot be read as a project source.",
        "duplicate" => "Listed twice in this import.",
        "caseCollision" => {
            "Differs only in letter case from another file in this import or project."
        }
        "empty" => "The file is empty.",
        "tooLarge" => "Files are limited to 25 MB; PDFs up to 60 MB.",
        "fileLimit" => "This import reached its file limit.",
        "sizeLimit" => "This import reached its size limit.",
        "projectLimit" => "The project already holds its maximum number of sources.",
        "documentLimit" => "The project already holds its maximum number of documents.",
        "imageLimit" => "The project already holds its maximum number of images.",
        _ => "",
    }
}

// ── tickets ───────────────────────────────────────────────────────────────

struct Ticket {
    user: String,
    project: String,
    paths: HashMap<String, f64>,
    max_bytes: usize,
    used_bytes: f64,
    expires: i64,
    /// Opening order, so a person's oldest goes first.
    seq: u64,
}

struct Tickets {
    map: HashMap<String, Ticket>,
    next: u64,
}

fn tickets() -> &'static Mutex<Tickets> {
    static T: OnceLock<Mutex<Tickets>> = OnceLock::new();
    T.get_or_init(|| {
        Mutex::new(Tickets {
            map: HashMap::new(),
            next: 0,
        })
    })
}

fn lock() -> std::sync::MutexGuard<'static, Tickets> {
    match tickets().lock() {
        Ok(g) => g,
        Err(p) => p.into_inner(),
    }
}

fn prune(t: &mut Tickets, now: i64) {
    t.map.retain(|_, ticket| ticket.expires > now);
}

fn random_id() -> String {
    use rand_core::RngCore;
    let mut bytes = [0u8; 16];
    rand_core::OsRng.fill_bytes(&mut bytes);
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Opens a ticket for one person, retiring only that person's oldest beyond four.
fn open_ticket(
    user: &str,
    project: &str,
    paths: HashMap<String, f64>,
    max_bytes: usize,
    now: i64,
) -> Result<String, Refused> {
    let mut t = lock();
    prune(&mut t, now);
    let mut mine: Vec<(u64, String)> = t
        .map
        .iter()
        .filter(|(_, ticket)| ticket.user == user)
        .map(|(id, ticket)| (ticket.seq, id.clone()))
        .collect();
    mine.sort();
    while mine.len() >= TICKETS_PER_USER {
        let (_, oldest) = mine.remove(0);
        t.map.remove(&oldest);
    }
    if t.map.len() >= TICKETS_MAX {
        return Err(Refused {
            status: 429,
            message: "Too many folder imports are in progress on this server. Try again in a few minutes.".into(),
            code: None,
        });
    }
    let id = random_id();
    t.next += 1;
    let seq = t.next;
    t.map.insert(
        id.clone(),
        Ticket {
            user: user.to_owned(),
            project: project.to_owned(),
            paths,
            max_bytes,
            used_bytes: 0.0,
            expires: now + TICKET_TTL_MS,
            seq,
        },
    );
    Ok(id)
}

/// `admit(importId, userId, projectId, rel, size)`: whether these bytes may be uploaded at this
/// path under this import; `None` when yes, otherwise `(status, code, error)`.
pub fn admit(
    import_id: &JValue,
    user: &str,
    project: &str,
    rel: &str,
    size: usize,
    now: i64,
) -> Option<(u16, &'static str, String)> {
    let mut t = lock();
    prune(&mut t, now);
    let ticket = import_id.as_str().and_then(|id| t.map.get(id));
    let Some(ticket) = ticket.filter(|t| t.user == user && t.project == project) else {
        return Some((409, "importNotPlanned", "This file is not part of the current folder import, or the import expired. Import the folder again.".into()));
    };
    let Some(declared) = ticket.paths.get(rel) else {
        return Some((409, "importNotPlanned", "This file is not part of the current folder import, or the import expired. Import the folder again.".into()));
    };
    if size as f64 > *declared {
        return Some((
            413,
            "sizeMismatch",
            "This file is larger than the folder import declared for it. Import the folder again."
                .into(),
        ));
    }
    if ticket.used_bytes + size as f64 > ticket.max_bytes as f64 {
        return Some((413, "sizeLimit", reason("sizeLimit").to_owned()));
    }
    None
}

/// `consume(importId, rel, size)`: one accepted path done, its bytes counted against the cap.
pub fn consume(import_id: &JValue, rel: &str, size: usize) {
    let mut t = lock();
    if let Some(ticket) = import_id.as_str().and_then(|id| t.map.get_mut(id)) {
        if ticket.paths.remove(rel).is_some() {
            ticket.used_bytes += size as f64;
        }
    }
}

/// Tests: forget every ticket.
#[cfg(test)]
pub fn reset() {
    lock().map.clear();
}

/// `Number(value)` for a manifest row's `size`.
fn to_number(v: &JValue) -> f64 {
    match v {
        JValue::Num(n) => *n,
        JValue::Str(s) => string_to_number(s),
        JValue::Bool(true) => 1.0,
        JValue::Bool(false) | JValue::Null => 0.0,
        JValue::Arr(items) if items.is_empty() => 0.0,
        _ => f64::NAN,
    }
}

fn hex64(s: &str) -> bool {
    s.len() == 64
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// What the plan needs of the project and the caller.
pub struct PlanInput<'a> {
    pub user: &'a str,
    pub project: &'a JValue,
    pub entries: &'a JValue,
    /// The source name a relative path is stored under (`""` when the project has no folder yet).
    pub destination: &'a dyn Fn(&str) -> String,
    pub per_file_cap: &'a dyn Fn(&str) -> f64,
    pub now: i64,
}

fn utf16_cmp(a: &str, b: &str) -> std::cmp::Ordering {
    a.encode_utf16().cmp(b.encode_utf16())
}

fn str_of(v: &JValue) -> String {
    // `String(e.path ?? '')`.
    if v.is_nullish() {
        String::new()
    } else {
        js_json::to_js_string(v).unwrap_or_default()
    }
}

/// `plan(...)`: decides what an import does and opens a ticket for the accepted paths.
pub fn plan(input: &PlanInput<'_>) -> Result<JValue, Refused> {
    let JValue::Arr(entries) = input.entries else {
        return Err(Refused {
            status: 400,
            message: "entries must be a list".into(),
            code: None,
        });
    };
    if entries.len() > MANIFEST_MAX {
        return Err(Refused {
            status: 400,
            message: format!("A folder import lists at most {MANIFEST_MAX} files."),
            code: None,
        });
    }
    let caps = limits();
    let pl = source_limits::limits();
    let files: Vec<JValue> = match input.project.get("files") {
        JValue::Arr(a) => a.clone(),
        _ => Vec::new(),
    };
    let by_name: HashMap<String, &JValue> = files
        .iter()
        .filter_map(|f| f.get("name").as_str().map(|n| (n.to_owned(), f)))
        .collect();
    let by_fold: HashMap<String, String> = files
        .iter()
        .filter_map(|f| f.get("name").as_str().map(|n| (fold_key(n), n.to_owned())))
        .collect();
    let held = source_limits::counts(&files);
    let assets = match input.project.get("assets") {
        JValue::Arr(a) => a.len(),
        _ => 0,
    };
    let mut room = (
        pl.sources as i64 - held.sources as i64,
        pl.documents as i64 - held.documents as i64,
        pl.images as i64 - assets as i64,
    );
    let mut accepted: Vec<JValue> = Vec::new();
    let mut accepted_paths: Vec<(String, f64)> = Vec::new();
    let mut unchanged: Vec<JValue> = Vec::new();
    let mut skipped: Vec<JValue> = Vec::new();
    let mut seen: BTreeSet<String> = BTreeSet::new();
    let mut seen_fold: BTreeSet<String> = BTreeSet::new();
    let mut bytes = 0.0f64;
    let skip = |skipped: &mut Vec<JValue>, p: &str, code: &str, extra: Vec<(&str, JValue)>| {
        let mut row = vec![
            ("path".to_owned(), JValue::Str(p.to_owned())),
            ("code".to_owned(), JValue::Str(code.to_owned())),
            ("reason".to_owned(), JValue::Str(reason(code).to_owned())),
        ];
        row.extend(extra.into_iter().map(|(k, v)| (k.to_owned(), v)));
        skipped.push(JValue::Obj(row));
    };
    struct Row {
        raw: String,
        size: f64,
        sha256: String,
    }
    let mut rows: Vec<Row> = entries
        .iter()
        .map(|e| {
            let obj = if e.is_object() { e } else { &JValue::Undefined };
            Row {
                raw: str_of(obj.get("path")),
                size: to_number(obj.get("size")),
                sha256: obj
                    .get("sha256")
                    .as_str()
                    .map(str::to_lowercase)
                    .unwrap_or_default(),
            }
        })
        .collect();
    rows.sort_by(|a, b| utf16_cmp(&a.raw, &b.raw));
    for row in &rows {
        let rel = match relative_path(&JValue::Str(row.raw.clone())) {
            Ok(r) => r,
            Err(_) => {
                skip(
                    &mut skipped,
                    &wtf16::slice(&row.raw, PATH_MAX),
                    "invalidPath",
                    vec![],
                );
                continue;
            }
        };
        if let Some((code, at)) = skip_reason(&rel) {
            skip(&mut skipped, &rel, code, vec![("at", JValue::Str(at))]);
            continue;
        }
        if seen.contains(&rel) {
            skip(&mut skipped, &rel, "duplicate", vec![]);
            continue;
        }
        if seen_fold.contains(&fold_key(&rel)) {
            skip(&mut skipped, &rel, "caseCollision", vec![]);
            continue;
        }
        seen.insert(rel.clone());
        seen_fold.insert(fold_key(&rel));
        if !row.size.is_finite() || row.size <= 0.0 {
            skip(&mut skipped, &rel, "empty", vec![]);
            continue;
        }
        if row.size > (input.per_file_cap)(&rel) {
            skip(&mut skipped, &rel, "tooLarge", vec![]);
            continue;
        }
        let dest = (input.destination)(&rel);
        let previous = by_name.get(&dest).copied();
        if previous.is_none() && !dest.is_empty() {
            if let Some(existing) = by_fold.get(&fold_key(&dest)) {
                skip(
                    &mut skipped,
                    &rel,
                    "caseCollision",
                    vec![("existing", JValue::Str(existing.clone()))],
                );
                continue;
            }
        }
        if let Some(prev) = previous {
            if hex64(&row.sha256) && prev.get("attachment").get("id").as_str() == Some(&row.sha256)
            {
                unchanged.push(JValue::obj([("path", JValue::Str(rel))]));
                continue;
            }
        }
        if accepted.len() >= caps.max_files {
            skip(&mut skipped, &rel, "fileLimit", vec![]);
            continue;
        }
        if bytes + row.size > caps.max_bytes as f64 {
            skip(&mut skipped, &rel, "sizeLimit", vec![]);
            continue;
        }
        if previous.is_none() {
            let kind = source_limits::kind_of_name(&rel);
            let small_image = row.size <= 8.0 * 1024.0 * 1024.0;
            if room.0 <= 0 {
                skip(
                    &mut skipped,
                    &rel,
                    "projectLimit",
                    vec![("max", JValue::Num(pl.sources as f64))],
                );
                continue;
            }
            if kind == Kind::Document && room.1 <= 0 {
                skip(
                    &mut skipped,
                    &rel,
                    "documentLimit",
                    vec![("max", JValue::Num(pl.documents as f64))],
                );
                continue;
            }
            // Only an image small enough to be model input (8 MB) takes an image slot.
            if kind == Kind::Image && small_image && room.2 <= 0 {
                skip(
                    &mut skipped,
                    &rel,
                    "imageLimit",
                    vec![("max", JValue::Num(pl.images as f64))],
                );
                continue;
            }
            room.0 -= 1;
            if kind == Kind::Document {
                room.1 -= 1;
            }
            if kind == Kind::Image && small_image {
                room.2 -= 1;
            }
        }
        bytes += row.size;
        accepted_paths.push((rel.clone(), row.size));
        accepted.push(JValue::obj([
            ("path", JValue::Str(rel)),
            ("size", JValue::Num(row.size)),
            (
                "change",
                JValue::Str(if previous.is_some() { "changed" } else { "new" }.into()),
            ),
        ]));
    }
    let import_id = open_ticket(
        input.user,
        input.project.get("id").as_str().unwrap_or(""),
        accepted_paths.into_iter().collect(),
        caps.max_bytes,
        input.now,
    )?;
    Ok(JValue::obj([
        ("importId", JValue::Str(import_id)),
        (
            "limits",
            JValue::obj([
                ("maxFiles", JValue::Num(caps.max_files as f64)),
                ("maxBytes", JValue::Num(caps.max_bytes as f64)),
                ("projectSources", JValue::Num(pl.sources as f64)),
                ("projectDocuments", JValue::Num(pl.documents as f64)),
                ("projectImages", JValue::Num(pl.images as f64)),
            ]),
        ),
        ("accepted", JValue::Arr(accepted)),
        ("unchanged", JValue::Arr(unchanged)),
        ("skipped", JValue::Arr(skipped)),
    ]))
}

// ── the breadth-first walk ───────────────────────────────────────────────

/// `listTree`'s answer: every file, and the folders cut by a limit (`depthLimit` or `folderLimit`).
#[derive(Debug, Default)]
pub struct Tree {
    pub files: Vec<Entry>,
    pub limited: Vec<(String, &'static str)>,
}

/// Every file under `root`, breadth-first, through `list(dir)`. Skipped names are not entered. A
/// folder in `exclude` (another attached folder) is left to its own entry. `on_dir_error(dir, err)`
/// is told about a subfolder whose listing failed; the walk goes on without it. The root's own
/// failure is returned.
pub fn list_tree<E>(
    list: &mut dyn FnMut(&str) -> Result<Vec<Entry>, E>,
    root: &str,
    exclude: &BTreeSet<String>,
    on_dir_error: &mut dyn FnMut(&str, &E),
) -> Result<Tree, E> {
    let c = contract();
    let (max_depth, max_folders) = (c.max_depth, c.max_folders);
    let mut tree = Tree::default();
    let mut queue: Vec<(String, usize)> = vec![(root.to_owned(), 0)];
    let mut listed = 0usize;
    while !queue.is_empty() {
        let mut next: Vec<(String, usize)> = Vec::new();
        for (dir, depth) in queue {
            if listed >= max_folders {
                tree.limited.push((dir, "folderLimit"));
                continue;
            }
            listed += 1;
            let entries = match list(&dir) {
                Ok(e) => e,
                Err(err) => {
                    if dir == root {
                        return Err(err);
                    }
                    on_dir_error(&dir, &err);
                    continue;
                }
            };
            for entry in entries {
                if skipped_name(&entry.name).is_some() {
                    continue;
                }
                if !entry.is_dir {
                    tree.files.push(entry);
                    continue;
                }
                if exclude.contains(&entry.path) {
                    continue;
                }
                if depth + 1 >= max_depth {
                    tree.limited.push((entry.path.clone(), "depthLimit"));
                    continue;
                }
                next.push((entry.path.clone(), depth + 1));
            }
        }
        queue = next;
    }
    Ok(tree)
}

/// SHA-256 hex of the contract copy (for the pin test).
pub fn contract_digest() -> String {
    Sha256::digest(CONTRACT_JSON.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing, clippy::panic)]
mod tests {
    use super::*;

    /// The ticket store is the process's: tests that count tickets take turns.
    static SERIAL: Mutex<()> = Mutex::new(());
    fn serial() -> std::sync::MutexGuard<'static, ()> {
        SERIAL.lock().unwrap_or_else(|p| p.into_inner())
    }

    fn s(v: &str) -> JValue {
        JValue::Str(v.into())
    }

    #[test]
    fn the_contract_copy_is_the_pinned_one() {
        assert_eq!(contract_digest(), CONTRACT_SHA256);
        let c = contract();
        assert_eq!((c.max_depth, c.max_folders), (16, 400));
        assert_eq!(c.project_limits, (1000, 200, 12));
    }

    #[test]
    fn paths_are_canonical_or_refused_never_rewritten() {
        let ok = |raw: &str| relative_path(&s(raw)).unwrap();
        assert_eq!(ok("docs\\api//auth.md"), "docs/api/auth.md");
        // NFC: a decomposed e-acute arrives once.
        assert_eq!(ok("cafe\u{301}.md"), "caf\u{e9}.md");
        for bad in [
            "",
            "/etc/passwd",
            "C:/x.md",
            "a/../b.md",
            "./a.md",
            "a\u{1}b.md",
            " lead.md",
            "dir /a.md",
            "a/b/c/d/e/f/g/h/i/j/k/l/m/n/o/p/q.md",
        ] {
            let e = relative_path(&s(bad)).unwrap_err();
            assert_eq!((e.status, e.code), (400, Some("invalidPath")), "{bad:?}");
        }
        assert!(relative_path(&JValue::Num(1.0)).is_err());
        let long = format!("{}.md", "a".repeat(201));
        assert!(relative_path(&s(&long)).is_err());
    }

    #[test]
    fn skip_reasons_follow_the_shared_list() {
        assert_eq!(skip_reason("src/main.py"), None);
        assert_eq!(skip_reason("node_modules/a.js").unwrap().0, "system");
        assert_eq!(skip_reason("docs/.hidden/a.md").unwrap().0, "hidden");
        assert_eq!(skip_reason("a/Build/x.md").unwrap().0, "system");
        assert_eq!(
            skip_reason("pic.TIFF"),
            Some(("unsupported", ".tiff".into()))
        );
        assert_eq!(
            skip_reason("Makefile"),
            Some(("unsupported", "Makefile".into()))
        );
    }

    fn project(files: Vec<JValue>) -> JValue {
        JValue::obj([
            ("id", s("proj-a")),
            ("files", JValue::Arr(files)),
            ("assets", JValue::Arr(vec![])),
        ])
    }

    fn manifest(rows: &[(&str, f64, &str)]) -> JValue {
        JValue::Arr(
            rows.iter()
                .map(|(p, size, sha)| {
                    JValue::obj([
                        ("path", s(p)),
                        ("size", JValue::Num(*size)),
                        ("sha256", s(sha)),
                    ])
                })
                .collect(),
        )
    }

    fn run_plan(p: &JValue, entries: &JValue, user: &str) -> JValue {
        let destination = |rel: &str| rel.to_owned();
        let cap = |rel: &str| {
            if rel.ends_with(".pdf") {
                60.0 * 1048576.0
            } else {
                25.0 * 1048576.0
            }
        };
        plan(&PlanInput {
            user,
            project: p,
            entries,
            destination: &destination,
            per_file_cap: &cap,
            now: 1_000,
        })
        .unwrap()
    }

    fn strings(v: &JValue, key: &str) -> Vec<String> {
        match v {
            JValue::Arr(a) => a
                .iter()
                .map(|r| r.get(key).as_str().unwrap_or("").to_owned())
                .collect(),
            _ => panic!(),
        }
    }

    #[test]
    fn a_plan_accepts_skips_and_recognises_what_is_unchanged() {
        let _one = serial();
        reset();
        let hash = "a".repeat(64);
        let held = JValue::obj([
            ("name", s("kept.md")),
            ("attachment", JValue::obj([("id", s(&hash))])),
        ]);
        let p = project(vec![held]);
        let m = manifest(&[
            ("z.md", 5.0, ""),
            ("a.md", 5.0, ""),
            ("a.md", 5.0, ""),
            ("A.MD", 5.0, ""),
            ("dup.md", 5.0, ""),
            ("dup.md", 5.0, ""),
            ("kept.md", 3.0, &hash),
            ("node_modules/x.js", 4.0, ""),
            ("big.txt", 26.0 * 1048576.0, ""),
            ("zero.txt", 0.0, ""),
            ("../up.md", 5.0, ""),
            ("sheet.xlsx", 5.0, ""),
        ]);
        let out = run_plan(&p, &m, "user-1");
        // Sorted by path as JS compares strings (UTF-16 order): `.` then uppercase then lowercase.
        // "A.MD" is taken first, so both "a.md" rows differ from it only in case.
        assert_eq!(
            strings(out.get("accepted"), "path"),
            vec!["A.MD", "dup.md", "z.md"]
        );
        assert_eq!(strings(out.get("unchanged"), "path"), vec!["kept.md"]);
        assert_eq!(
            strings(out.get("skipped"), "code"),
            vec![
                "invalidPath",
                "caseCollision",
                "caseCollision",
                "tooLarge",
                "duplicate",
                "system",
                "unsupported",
                "empty"
            ]
        );
        assert_eq!(out.get("limits").get("maxFiles").as_num(), Some(1000.0));
    }

    #[test]
    fn a_ticket_admits_only_the_planned_paths_within_their_sizes() {
        let _one = serial();
        reset();
        let p = project(vec![]);
        let out = run_plan(
            &p,
            &manifest(&[("a.md", 10.0, ""), ("b.md", 20.0, "")]),
            "u1",
        );
        let id = JValue::Str(out.get("importId").as_str().unwrap().to_owned());
        assert!(admit(&id, "u1", "proj-a", "a.md", 10, 1_000).is_none());
        // Another person, another project, an unplanned path, an unknown id.
        assert_eq!(
            admit(&id, "u2", "proj-a", "a.md", 1, 1_000).unwrap().1,
            "importNotPlanned"
        );
        assert_eq!(
            admit(&id, "u1", "proj-b", "a.md", 1, 1_000).unwrap().1,
            "importNotPlanned"
        );
        assert_eq!(
            admit(&id, "u1", "proj-a", "c.md", 1, 1_000).unwrap().1,
            "importNotPlanned"
        );
        assert_eq!(
            admit(&s("nope"), "u1", "proj-a", "a.md", 1, 1_000)
                .unwrap()
                .1,
            "importNotPlanned"
        );
        // Larger than the plan declared: 413 sizeMismatch, and the path stays open.
        assert_eq!(
            admit(&id, "u1", "proj-a", "a.md", 11, 1_000).unwrap().1,
            "sizeMismatch"
        );
        consume(&id, "a.md", 10);
        assert_eq!(
            admit(&id, "u1", "proj-a", "a.md", 1, 1_000).unwrap().1,
            "importNotPlanned",
            "a consumed path is done"
        );
        // An hour later the ticket is gone.
        assert_eq!(
            admit(&id, "u1", "proj-a", "b.md", 1, 1_000 + TICKET_TTL_MS)
                .unwrap()
                .1,
            "importNotPlanned"
        );
    }

    #[test]
    fn a_person_keeps_four_tickets_and_the_oldest_goes_first() {
        let _one = serial();
        reset();
        let p = project(vec![]);
        let m = manifest(&[("a.md", 1.0, "")]);
        let ids: Vec<JValue> = (0..5)
            .map(|_| {
                JValue::Str(
                    run_plan(&p, &m, "u1")
                        .get("importId")
                        .as_str()
                        .unwrap()
                        .to_owned(),
                )
            })
            .collect();
        assert_eq!(
            admit(&ids[0], "u1", "proj-a", "a.md", 1, 1_000).unwrap().1,
            "importNotPlanned"
        );
        for id in &ids[1..] {
            assert!(admit(id, "u1", "proj-a", "a.md", 1, 1_000).is_none());
        }
        // Another person's tickets are untouched by this person's.
        let other = run_plan(&p, &m, "u2");
        assert!(admit(
            &JValue::Str(other.get("importId").as_str().unwrap().into()),
            "u2",
            "proj-a",
            "a.md",
            1,
            1_000
        )
        .is_none());
    }

    fn entry(path: &str, dir: bool) -> Entry {
        Entry {
            name: path.rsplit('/').next().unwrap().to_owned(),
            path: path.to_owned(),
            is_dir: dir,
            size: if dir { None } else { Some(1.0) },
            ext: String::new(),
        }
    }

    #[test]
    fn the_walk_is_breadth_first_skips_names_and_reports_limits_and_failures() {
        let mut calls = Vec::new();
        let mut list = |dir: &str| -> Result<Vec<Entry>, String> {
            calls.push(dir.to_owned());
            Ok(match dir {
                "root" => vec![
                    entry("root/a.md", false),
                    entry("root/sub", true),
                    entry("root/.git", true),
                    entry("root/other", true),
                    entry("root/bad", true),
                ],
                "root/sub" => vec![entry("root/sub/b.md", false), entry("root/sub/deep", true)],
                "root/sub/deep" => vec![entry("root/sub/deep/c.md", false)],
                "root/bad" => return Err("boom".into()),
                _ => vec![],
            })
        };
        let exclude: BTreeSet<String> = ["root/other".to_owned()].into();
        let mut failed = Vec::new();
        let tree = list_tree(&mut list, "root", &exclude, &mut |d, e| {
            failed.push((d.to_owned(), e.clone()))
        })
        .unwrap();
        let names: Vec<&str> = tree.files.iter().map(|f| f.path.as_str()).collect();
        assert_eq!(
            names,
            vec!["root/a.md", "root/sub/b.md", "root/sub/deep/c.md"]
        );
        assert_eq!(failed, vec![("root/bad".to_owned(), "boom".to_owned())]);
        assert_eq!(calls, vec!["root", "root/sub", "root/bad", "root/sub/deep"]);
        // The root's own failure is the walk's.
        let mut fail = |_: &str| -> Result<Vec<Entry>, String> { Err("down".into()) };
        assert_eq!(
            list_tree(&mut fail, "root", &BTreeSet::new(), &mut |_, _| {}).unwrap_err(),
            "down"
        );
    }
}
