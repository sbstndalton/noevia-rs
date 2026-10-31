//! `POST /api/projects/{id}/sources/sync` (sbstndalton/noevia#2191 round 8, group
//! `rust-project-source-work`): core routes/projects.cjs's source sync, answer for answer, run by the front
//! (a background sync is this function in a job; a synchronous one answers with its result).
//!
//! Re-reads every attached storage folder and refreshes the project's sources from it. Folder-derived
//! files carry `source`; uploaded ones do not, so a sync replaces what came from folders and never touches
//! an upload.
//!
//! In core's order, under the project's source lock:
//! 1. images held by noevia only (assets with no source) are moved into the project's storage folder
//!    (`uploads.ingest`, one save each);
//! 2. every attached folder is walked breadth-first ([`folder_import::list_tree`], #1805): text files are
//!    read as text, documents and DOCX and a managed type group's files as streamed binary reads
//!    ([`crate::staging`], the large-document lock taken from the declared size before the body, #1872),
//!    within the project's source and document limits and the sync's OCR allowance (#1871);
//! 3. a refused or throttled login stops every further storage call of the refresh (#1429): the rest keep
//!    what they had and say why. The latch is [`crate::storage_latch`];
//! 4. a read that failed keeps the previous source (a document keeps its last readable text, stale);
//!    a stored result is never extracted or reopened again from storage (there is no `reopenStored`);
//! 5. the list is merged against the project as it is now (uploads and other folders' sources are never
//!    dropped), saved, the stored originals and images pruned, and the RAG work left to core through the
//!    index outbox (`index` for what arrived or changed, `unindex` for what is gone).
//!
//! The documents are read by Docling alone: one whose worker is down is queued durably in
//! `extraction-jobs.db` (`document.extraction: "queued"`) and read when it is back; there is no in-process
//! PDF reader. A stored result is never extracted again.
use crate::extract::{self, Budgeted, Engine, Limits, OcrBudget};
use crate::lifecycle::Catalogue;
use crate::source_reply as reply;
use crate::source_storage::{BinaryRead, Entry, SourceStorage, Storage, StorageError};
use crate::uploads_ingest::{self as uploads, Env as IngestEnv, Fail, Options};
use crate::{
    document_ingest, document_pages, extract_queue, folder_import, outbox, project_documents,
    source_limits, staging, storage_latch, store, Ctx, Reply,
};
use js_json::{wtf16, JValue};
use std::collections::BTreeSet;
use std::path::Path;
use std::time::{Duration, Instant};

/// storage-client.cjs TEXT_EXTENSIONS.
const TEXT_EXTENSIONS: [&str; 15] = [
    ".txt",
    ".md",
    ".markdown",
    ".json",
    ".csv",
    ".yml",
    ".yaml",
    ".ts",
    ".tsx",
    ".js",
    ".jsx",
    ".py",
    ".sh",
    ".html",
    ".css",
];
const BUSY_CODE: &str = "large-document-busy";
const BUSY: &str = "Large-document processing is busy; refresh to retry.";
pub const QUEUE_FAILED: &str =
    "Document extraction could not be queued; nothing was changed. Try again shortly.";

/// What a sync needs.
pub struct SyncEnv<'a> {
    pub ctx: Ctx<'a>,
    pub project: &'a str,
    pub storage: &'a Storage<'a>,
    pub engine: &'a dyn Engine,
    pub limits: &'a Limits,
    pub catalogue: Option<&'a Catalogue>,
    /// index.cjs PROJECT_ROOT_FOLDER.
    pub project_root: &'a str,
    /// Stages progress for the job (`Saving original`, ...).
    pub progress: &'a dyn Fn(&str),
    /// How long to wait for the project's source lock when another operation holds it, before answering
    /// 429 (a request that waited without end would pile up threads and bodies).
    pub lock_wait: Duration,
}

/// The refusal an automatic sync meets before it starts (#1429): the latch holds this account's
/// automatic calls, unless the person pressed Retry (`X-Cowork-Storage-Retry: 1`). `None` when the
/// sync may run.
pub fn automatic_hold(ctx: Ctx<'_>, storage: &Storage<'_>, retry: bool) -> Option<Reply> {
    if retry {
        return None;
    }
    let remote = storage.remote()?;
    match storage_latch::blocked(
        ctx.data_dir,
        ctx.user_id,
        &remote.fingerprint(),
        crate::wall_ms(),
    ) {
        Ok(Some(hold)) => Some(reply::hold_reply(hold)),
        Ok(None) => None,
        // A latch that cannot be read counts as held, for a minute (a throttle's default): the latch
        // is what keeps a refused login from being fed to the storage server's brute-force protection
        // (#1429), so when it cannot say, an automatic call waits; an explicit press (`retry`) still runs.
        Err(e) => {
            eprintln!("noevia-server: the storage latch could not be read; holding: {e:?}");
            Some(reply::hold_reply(storage_latch::Hold {
                kind: "throttled",
                retry_after: Some(storage_latch::UNREADABLE_HOLD_SECONDS),
            }))
        }
    }
}

#[derive(Default)]
struct Stats {
    seen: u32,
    ingested: u32,
    extracted: u32,
    reopened: u32,
    unchanged: u32,
    errors: u32,
    skipped: usize,
}

fn ref_hash(id: &str) -> String {
    uploads::sha_hex(id.as_bytes())
        .get(..8)
        .unwrap_or("")
        .to_owned()
}

fn obj(fields: Vec<(&str, JValue)>) -> JValue {
    JValue::Obj(fields.into_iter().map(|(k, v)| (k.to_owned(), v)).collect())
}

fn s(v: &str) -> JValue {
    JValue::Str(v.to_owned())
}

fn strs(v: &JValue) -> Vec<String> {
    match v {
        JValue::Arr(a) => a
            .iter()
            .filter_map(|x| x.as_str().map(str::to_owned))
            .collect(),
        _ => Vec::new(),
    }
}

fn files_of(p: &JValue) -> Vec<JValue> {
    match p.get("files") {
        JValue::Arr(a) => a.clone(),
        _ => Vec::new(),
    }
}

fn is_pdf(path: &str) -> bool {
    path.to_lowercase().ends_with(".pdf")
}

fn is_docx(name: &str) -> bool {
    name.to_lowercase().ends_with(".docx")
}

fn problem_fields(file: &JValue) -> (String, Vec<(String, JValue)>) {
    let d = file.get("document");
    let text = document_pages::problem(d).unwrap_or_default();
    let notes = document_pages::notes_json(d).unwrap_or_default();
    let mut extra = Vec::new();
    if !notes.is_empty() {
        let base = document_pages::problem_with(d, false).unwrap_or_default();
        extra.push(("reasonBase".to_owned(), JValue::Str(base)));
        extra.push(("notes".to_owned(), JValue::Arr(notes)));
    }
    (text, extra)
}

/// document-sources.cjs `failed(previous, name, reason)`, on a record whose text is inline.
fn failed(previous: Option<&JValue>, name: &str, why: &str) -> JValue {
    let mut f = previous.cloned().unwrap_or(JValue::Obj(Vec::new()));
    let content = previous
        .and_then(|p| p.get("content").as_str())
        .unwrap_or("")
        .to_owned();
    let had = !content.is_empty();
    let indexing = previous
        .map(|p| p.get("document").get("indexing"))
        .filter(|i| i.truthy())
        .cloned()
        .unwrap_or_else(|| s("unavailable"));
    if let JValue::Obj(items) = &mut f {
        wtf16::set(items, "name", s(name));
        wtf16::set(items, "content", JValue::Str(content));
    }
    let mut d = previous
        .map(|p| p.get("document").clone())
        .filter(JValue::is_object)
        .unwrap_or(JValue::Obj(Vec::new()));
    if let JValue::Obj(items) = &mut d {
        wtf16::set(items, "state", s("failed"));
        wtf16::set(items, "stale", JValue::Bool(had));
        wtf16::set(items, "error", s(why));
        wtf16::set(items, "indexing", indexing);
    }
    if let JValue::Obj(items) = &mut f {
        wtf16::set(items, "document", d);
    }
    f
}

fn with_source(mut f: JValue, folder: &str) -> JValue {
    if let JValue::Obj(items) = &mut f {
        wtf16::set(items, "source", s(folder));
    }
    f
}

/// What a read of one file gives back, with the large-document hold it took (kept while the file is
/// used).
struct Read {
    bytes: Vec<u8>,
    _hold: Option<extract::LargeHold>,
}

impl Read {
    /// Whether the read holds the large-document lock (until it is dropped, after the ingest).
    #[cfg(test)]
    fn held(&self) -> bool {
        self._hold.is_some()
    }
}

fn busy() -> StorageError {
    StorageError::new(503, BUSY).with_code(BUSY_CODE)
}

/// large-document-lock.cjs `readThenUse`'s read half: the listed size decides whether the lock is
/// taken before the read; otherwise the response's own declared size does, before its body (#1872).
fn read_file(
    dir: &Path,
    storage: &dyn SourceStorage,
    engine: &dyn Engine,
    limits: &Limits,
    entry: &Entry,
) -> Result<Read, StorageError> {
    let lock_file = engine.large_lock_file();
    let pdf = is_pdf(&entry.path);
    let (cap, timeout, listed_large) = if pdf {
        // #1864: a PDF is read up to PROJECT_DOCUMENT_READ_BYTES (never past the 96 MiB the front's
        // streamed read takes), with a timeout that grows with the cap.
        let cap = (limits.read_bytes as u64).min(crate::SOURCE_FILE_CAP);
        let timeout = Duration::from_secs(30 + cap.div_ceil(1024 * 1024));
        let large = !entry
            .size
            .is_some_and(|n| n.is_finite() && n >= 0.0 && n <= extract::LARGE_BYTES as f64);
        (cap, timeout, large)
    } else {
        (25 * 1024 * 1024, Duration::from_secs(30), false)
    };
    let mut hold: Option<extract::LargeHold> = None;
    if listed_large {
        hold = Some(
            extract::large_lock(extract::LARGE_BYTES + 1, lock_file.as_deref())
                .map_err(|_| busy())?,
        );
    }
    let cell = std::cell::RefCell::new(hold);
    let mut before = |declared: Option<u64>| -> Result<(), StorageError> {
        if pdf
            && cell.borrow().is_none()
            && !declared.is_some_and(|n| n <= extract::LARGE_BYTES as u64)
        {
            let taken = extract::large_lock(extract::LARGE_BYTES + 1, lock_file.as_deref())
                .map_err(|_| busy())?;
            *cell.borrow_mut() = Some(taken);
        }
        Ok(())
    };
    let staged = staging::stage(
        dir,
        storage,
        &entry.path,
        BinaryRead {
            cap,
            timeout,
            before_body: &mut before,
        },
    )?;
    let bytes = staged.bytes().map_err(|e| {
        eprintln!("noevia-server: reading a staged file failed: {}", e.kind());
        StorageError::new(500, "Internal error")
    })?;
    drop(staged);
    Ok(Read {
        bytes,
        _hold: cell.into_inner(),
    })
}

/// A refused or throttled login halts the rest of a refresh; what it says.
type Halt = Option<(String, Option<&'static str>)>;

/// `haltFor(err)`: the latch is told; a refused or throttled login halts the rest of the refresh.
fn halt_for(
    env: &SyncEnv<'_>,
    storage: Option<&dyn SourceStorage>,
    err: &StorageError,
    halted: &mut Halt,
) {
    let Some(failure) = err.failure() else {
        return;
    };
    if let Some(storage) = storage {
        if let Err(e) = storage_latch::note(
            env.ctx.data_dir,
            env.ctx.user_id,
            &storage.fingerprint(),
            failure,
            // The moment it happened: the request's own time is hours old for a long sync, and a
            // hold written from it would be over before it began.
            crate::wall_ms(),
        ) {
            eprintln!("noevia-server: the storage latch could not be written: {e:?}");
        }
    }
    if failure.kind == "login" || failure.kind == "throttled" {
        *halted = Some((reply::reason(err, ""), reply::code(err)));
    }
}

/// The state of one run, owned by [`run_locked`].
struct Run<'a> {
    env: &'a SyncEnv<'a>,
    dir: &'a Path,
    engine: &'a dyn Engine,
    storage: Option<&'a dyn SourceStorage>,
    stats: Stats,
    from_folders: Vec<JValue>,
    skipped: Vec<JValue>,
    halted: Halt,
    /// Ingested images' assets, applied to the list at the save.
    assets: Vec<uploads::Ingested>,
    /// (name, byte hash, version, error) of the documents whose extraction is queued.
    queued: Vec<(String, String, String, Option<String>)>,
    /// Documents read now: a job they had is done.
    read_ok: Vec<String>,
}

impl Run<'_> {
    fn skip(&mut self, fields: Vec<(&str, JValue)>) {
        self.skipped.push(obj(fields));
    }

    fn halt_for(&mut self, err: &StorageError) {
        halt_for(self.env, self.storage, err, &mut self.halted);
    }

    /// A folder's previous sources, kept as they are (a document keeps its last readable text, stale)
    /// with the reason, when nothing of it could be read.
    fn keep_previous(
        &mut self,
        folder: &str,
        previous: &[JValue],
        why: &str,
        code: Option<&'static str>,
        docling: bool,
    ) {
        for f in previous {
            let name = f.get("name").as_str().unwrap_or("");
            let kept = if extract::is_document(name, docling) {
                let text = reply::with_text(self.dir, self.env.project, f);
                with_source(failed(Some(&text), name, why), folder)
            } else {
                f.clone()
            };
            self.from_folders.push(kept);
        }
        let retained = previous.iter().any(|f| reply::text_length(f) > 0);
        let mut fields = vec![("folder", s(folder)), ("reason", s(why))];
        push_opt(&mut fields, "code", code.map(s));
        fields.push(("retained", JValue::Bool(retained)));
        self.skip(fields);
    }

    /// A document that arrived: queued ones get their job written before the save.
    fn note_document(&mut self, file: &JValue, queued: bool, name: &str) {
        if !file.get("document").truthy() {
            return;
        }
        if queued {
            let d = file.get("document");
            self.queued.push((
                name.to_owned(),
                d.get("byteHash").as_str().unwrap_or("").to_owned(),
                d.get("version").as_str().unwrap_or("").to_owned(),
                d.get("error").as_str().map(str::to_owned),
            ));
        } else {
            self.read_ok.push(name.to_owned());
        }
    }
}

fn push_opt<'a>(fields: &mut Vec<(&'a str, JValue)>, key: &'a str, v: Option<JValue>) {
    if let Some(v) = v {
        fields.push((key, v));
    }
}

/// The sync. The project's existence and the automatic hold ([`automatic_hold`]) are the route's.
pub fn run(env: &SyncEnv<'_>) -> Reply {
    run_budgeted(env, source_limits::sync_ocr_pages())
}

/// [`run`] with the OCR allowance (`PROJECT_SYNC_OCR_PAGES`) given.
fn run_budgeted(env: &SyncEnv<'_>, ocr_pages: usize) -> Reply {
    let started = Instant::now();
    let budgeted = Budgeted {
        inner: env.engine,
        budget: OcrBudget::new(ocr_pages),
    };
    let lock = project_documents::source_lock(env.ctx.user_id, env.project);
    let Some((_held, ctx)) = project_documents::lock_restamped(&lock, env.lock_wait, env.ctx)
    else {
        return finish_log(
            env,
            &Stats::default(),
            "429".to_owned(),
            started,
            project_documents::busy_reply(),
        );
    };
    // What the sync writes is dated by when it runs, not by when a queued job was made.
    let env = &SyncEnv { ctx, ..*env };
    match run_locked(env, &budgeted) {
        Ok((reply, stats, status)) => finish_log(env, &stats, status.to_string(), started, reply),
        Err(r) => {
            let status = r.status;
            finish_log(env, &Stats::default(), status.to_string(), started, r)
        }
    }
}

/// One content-free line per sync (#1885): counts and a short tag of the project, never a file name,
/// folder or text.
fn finish_log(
    env: &SyncEnv<'_>,
    stats: &Stats,
    status: String,
    started: Instant,
    r: Reply,
) -> Reply {
    eprintln!(
        "[sources] sync project={} status={status} seen={} ingested={} extracted={} reopened={} unchanged={} errors={} skipped={} ms={}",
        ref_hash(env.project),
        stats.seen,
        stats.ingested,
        stats.extracted,
        stats.reopened,
        stats.unchanged,
        stats.errors,
        stats.skipped,
        started.elapsed().as_millis()
    );
    r
}

fn run_locked(env: &SyncEnv<'_>, engine: &Budgeted<'_>) -> Result<(Reply, Stats, u16), Reply> {
    let ctx = env.ctx;
    let changed = || reply::error_reply(409, "Project changed; retry.", None);
    let (dir, project) = crate::originals::project(ctx, env.project)?;
    let Some(project) = project else {
        return Err(changed());
    };
    staging::sweep(&dir);
    let storage_ref: Option<&dyn SourceStorage> = env.storage.remote();
    let had_uploads = reply::has_uploads(&project);
    let source_limit = source_limits::limits().sources;
    let limit_reason =
        format!("{source_limit}-source project limit reached; this file was not read.");
    let mut run = Run {
        env,
        dir: &dir,
        engine,
        storage: storage_ref,
        stats: Stats::default(),
        from_folders: Vec::new(),
        skipped: Vec::new(),
        halted: None,
        assets: Vec::new(),
        queued: Vec::new(),
        read_ok: Vec::new(),
    };
    if let Storage::Refused(status, message) = env.storage {
        // The endpoint policy or admission: nothing can be read or written now.
        if !strs(project.get("sourceFolders")).is_empty() {
            return Err(reply::error_reply(*status, message, None));
        }
    }
    // 1. Images held by noevia only move into storage.
    let loose: Vec<JValue> = match project.get("assets") {
        JValue::Arr(a) => a
            .iter()
            .filter(|a| !a.get("sourceName").truthy())
            .cloned()
            .collect(),
        _ => Vec::new(),
    };
    let mut allocated: Option<String> = None;
    if let (Some(storage), false) = (storage_ref, loose.is_empty()) {
        let mut folder = project
            .get("projectFolder")
            .as_str()
            .filter(|f| !f.is_empty())
            .map(str::to_owned);
        if folder.is_none() {
            folder = crate::source_storage::allocate_folder(
                storage,
                env.project_root,
                env.project,
                project.get("reservedFolder").as_str(),
            )
            .map_err(|e| {
                eprintln!(
                    "[projects] could not create folder for project {}: {}",
                    env.project, e.message
                );
            })
            .ok();
            allocated = folder.clone();
            // Saved at once, as an upload does: when every move below fails (storage refuses the
            // writes) the folder it made is still the project's, and a retry finds it instead of
            // allocating "Name (2)" and orphaning this one.
            if let Some(held) = &allocated {
                let held = held.clone();
                let _ = store::update(dir.as_path(), ctx.switch, |projects| {
                    let Some(p) = store::find_mut(projects, env.project) else {
                        return ((), false);
                    };
                    if p.get("projectFolder").as_str().is_none_or(str::is_empty) {
                        store::set(p, "projectFolder", s(&held));
                        return ((), true);
                    }
                    ((), false)
                });
            }
        }
        if let Some(folder) = folder {
            for asset in &loose {
                if let Err(why) = move_asset(&mut run, storage, &folder, asset) {
                    run.skip(vec![
                        ("folder", s(&folder)),
                        ("file", asset.get("name").clone()),
                        ("reason", s(&format!("Image remains in noevia: {why}"))),
                        ("retained", JValue::Bool(true)),
                    ]);
                }
            }
        }
    }
    // The list as it is now (the moves saved theirs). A folder allocated for them is the project's
    // even when none moved (core held it on the project it saved at the end).
    let (_, current) = crate::originals::project(ctx, env.project)?;
    let Some(mut project) = current else {
        return Err(changed());
    };
    if let Some(f) = &allocated {
        if project
            .get("projectFolder")
            .as_str()
            .is_none_or(str::is_empty)
        {
            store::set(&mut project, "projectFolder", s(f));
        }
    }
    let folders = strs(project.get("sourceFolders"));
    if !folders.is_empty() && storage_ref.is_none() {
        return Err(reply::error_reply(
            400,
            "no browsable storage connection is configured",
            None,
        ));
    }
    let files_now = files_of(&project);
    let project_folder = project
        .get("projectFolder")
        .as_str()
        .unwrap_or("")
        .to_owned();
    // Room left for the folders being re-read: every source that is NOT from one of them stays, so it
    // counts against the source limit (and documents against theirs).
    let kept: Vec<JValue> = files_now
        .iter()
        .filter(|f| {
            !f.get("source").truthy()
                || !folders.iter().any(|x| f.get("source").as_str() == Some(x))
        })
        .cloned()
        .collect();
    let source_room = source_limit.saturating_sub(kept.len());
    let doc_limit = source_limits::limits().documents;
    let document_room = doc_limit.saturating_sub(source_limits::counts(&kept).documents);
    let mut documents_read = 0usize;
    let docling = engine.docling_configured();
    let assets_held = match project.get("assets") {
        JValue::Arr(a) => a.len(),
        _ => 0,
    };
    for folder in &folders {
        let previous_of = |files: &[JValue]| -> Vec<JValue> {
            files
                .iter()
                .filter(|f| f.get("source").as_str() == Some(folder.as_str()))
                .cloned()
                .collect()
        };
        if let Some((why, code)) = run.halted.clone() {
            let previous = previous_of(&files_now);
            run.keep_previous(folder, &previous, &why, code, docling);
            continue;
        }
        let Some(storage) = storage_ref else {
            continue;
        };
        // #1805: every subfolder is read, not only the top level: breadth-first, skipping the shared
        // system/hidden names, bounded by depth and folder count, and leaving a subfolder that is itself
        // attached to its own entry so no file is synced twice.
        let exclude: BTreeSet<String> = folders
            .iter()
            .filter(|other| *other != folder && other.starts_with(&format!("{folder}/")))
            .cloned()
            .collect();
        let halted = std::cell::RefCell::new(run.halted.clone());
        // Each failed subfolder with the reason it is given when it fails: a refused login that a later
        // folder meets must not rewrite what an earlier one said (core built the entry on the spot).
        type DirError = (String, StorageError, Halt);
        let dir_errors: std::cell::RefCell<Vec<DirError>> = Default::default();
        let tree = {
            let mut list = |d: &str| -> Result<Vec<Entry>, StorageError> {
                if let Some((why, _)) = halted.borrow().as_ref() {
                    return Err(StorageError::new(502, why.clone()));
                }
                storage.list(d)
            };
            let mut on_err = |d: &str, e: &StorageError| {
                halt_for(env, storage_ref, e, &mut halted.borrow_mut());
                dir_errors
                    .borrow_mut()
                    .push((d.to_owned(), e.clone(), halted.borrow().clone()));
            };
            folder_import::list_tree(&mut list, folder, &exclude, &mut on_err)
        };
        // A listing that failed at the root halts through the same latch.
        run.halted = halted.into_inner();
        for (d, e, halted_then) in dir_errors.into_inner() {
            run.from_folders
                .extend(previous_of(&files_now).into_iter().filter(|f| {
                    f.get("name")
                        .as_str()
                        .is_some_and(|n| n.starts_with(&format!("{d}/")))
                }));
            let (why, code) = match &halted_then {
                Some((why, code)) => (why.clone(), *code),
                None => (
                    "Could not read this subfolder; previous sources retained.".to_owned(),
                    reply::code(&e),
                ),
            };
            let mut fields = vec![("folder", s(folder)), ("file", s(&d)), ("reason", s(&why))];
            push_opt(&mut fields, "code", code.map(s));
            fields.push(("retained", JValue::Bool(true)));
            run.skip(fields);
        }
        let tree = match tree {
            Ok(t) => t,
            Err(e) => {
                run.halt_for(&e);
                let why = reply::reason(&e, "could not list folder");
                let previous = previous_of(&files_now);
                run.keep_previous(folder, &previous, &why, reply::code(&e), docling);
                continue;
            }
        };
        for (cut, code) in &tree.limited {
            run.from_folders
                .extend(previous_of(&files_now).into_iter().filter(|f| {
                    f.get("name")
                        .as_str()
                        .is_some_and(|n| n.starts_with(&format!("{cut}/")))
                }));
            let c = folder_import::contract();
            let words = if *code == "depthLimit" {
                format!(
                    "Folders more than {} levels deep are not read.",
                    c.max_depth
                )
            } else {
                format!("Only the first {} subfolders are read.", c.max_folders)
            };
            run.skip(vec![
                ("folder", s(folder)),
                ("file", s(cut)),
                ("code", s(code)),
                ("retained", JValue::Bool(true)),
                ("reason", s(&words)),
            ]);
        }
        // A listing that worked proves the login. Not one in which a subfolder was refused the login (or
        // throttled): that is the latch just set, which clearing would undo (core cleared it here too,
        // which let the next automatic sync straight into the same refusal).
        if run.halted.is_none() {
            if let Err(e) = storage_latch::clear(ctx.data_dir, ctx.user_id) {
                eprintln!("noevia-server: the storage latch could not be cleared: {e:?}");
            }
        }
        for entry in &tree.files {
            let ext = entry.ext.to_lowercase();
            let is_text = TEXT_EXTENSIONS.contains(&ext.as_str());
            let is_doc = extract::is_document(&entry.name, docling);
            let docx = is_docx(&entry.name);
            let managed = folder == &project_folder
                && uploads::GROUPS
                    .iter()
                    .any(|g| entry.path.starts_with(&format!("{folder}/{g}/")));
            if !is_text && !is_doc && !docx && !managed {
                continue;
            }
            // A cap, so one big folder cannot blow up a project.
            if run.from_folders.len() >= source_room {
                run.skip(vec![
                    ("folder", s(folder)),
                    ("file", s(&entry.path)),
                    ("reason", s(&limit_reason)),
                    ("code", s("limit")),
                    ("retained", JValue::Bool(false)),
                ]);
                continue;
            }
            let document_like = is_doc
                || docx
                || (managed
                    && source_limits::kind_of_name(&entry.name) == source_limits::Kind::Document);
            if document_like && documents_read >= document_room {
                run.skip(vec![
                    ("folder", s(folder)),
                    ("file", s(&entry.path)),
                    (
                        "reason",
                        s(&format!(
                            "{doc_limit}-document project limit reached; this file was not read."
                        )),
                    ),
                    ("code", s("documentLimit")),
                    ("retained", JValue::Bool(false)),
                ]);
                continue;
            }
            if document_like {
                documents_read += 1;
            }
            let previous = previous_of(&files_now)
                .into_iter()
                .find(|f| f.get("name").as_str() == Some(entry.path.as_str()));
            run.stats.seen += 1;
            if let Some((why, code)) = run.halted.clone() {
                if is_doc {
                    let text = previous
                        .as_ref()
                        .map(|p| reply::with_text(&dir, env.project, p));
                    run.from_folders.push(with_source(
                        failed(text.as_ref(), &entry.path, &why),
                        folder,
                    ));
                } else if let Some(p) = &previous {
                    run.from_folders.push(p.clone());
                }
                let retained = previous.as_ref().is_some_and(|p| reply::text_length(p) > 0);
                let mut fields = vec![
                    ("folder", s(folder)),
                    ("file", s(&entry.path)),
                    ("reason", s(&why)),
                ];
                push_opt(&mut fields, "code", code.map(s));
                fields.push(("retained", JValue::Bool(retained)));
                run.skip(fields);
                continue;
            }
            let kind = Kind {
                managed,
                docx,
                is_doc,
            };
            let outcome = read_entry(
                &mut run,
                storage,
                entry,
                folder,
                previous.as_ref(),
                kind,
                &files_now,
                assets_held,
                &project_folder,
            );
            match outcome {
                Ok(()) => run.stats.ingested += 1,
                // The account was removed meanwhile: nothing more is read for it, and nothing is made.
                Err(e) if e.is_gone() => {
                    return Err(Reply::error(410, crate::source_storage::GONE))
                }
                Err(e) => {
                    run.stats.errors += 1;
                    on_read_failure(&mut run, folder, entry, previous.as_ref(), kind.is_doc, &e);
                }
            }
        }
    }
    finish(&mut run, &folders, source_limit, had_uploads)
}

#[derive(Clone, Copy)]
struct Kind {
    managed: bool,
    docx: bool,
    is_doc: bool,
}

/// Step 1: one loose image into storage (`uploads.ingest` with the connection, saved at once).
fn move_asset(
    run: &mut Run<'_>,
    storage: &dyn SourceStorage,
    folder: &str,
    asset: &JValue,
) -> Result<(), String> {
    let env = run.env;
    let asset_id = asset.get("id").as_str().unwrap_or("").to_owned();
    let asset_name = asset.get("name").as_str().unwrap_or("");
    let bytes =
        std::fs::read(store::asset_dir(run.dir, env.project).join(crate::js::id_chars(&asset_id)))
            .map_err(|e| e.to_string())?;
    // A stable suffix prevents overwriting a file already placed there outside noevia.
    let name = match asset_name.rfind('.').filter(|d| *d > 0) {
        Some(dot) => format!(
            "{}-{asset_id}{}",
            asset_name.get(..dot).unwrap_or(""),
            asset_name.get(dot..).unwrap_or("")
        ),
        None => format!("{asset_name}-{asset_id}"),
    };
    let (_, now) = crate::originals::project(env.ctx, env.project)
        .map_err(|_| "Project changed; retry.".to_owned())?;
    let now = now.ok_or_else(|| "Project changed; retry.".to_owned())?;
    let files = files_of(&now);
    let held = match now.get("assets") {
        JValue::Arr(a) => a.len(),
        _ => 0,
    };
    let ingest_env = IngestEnv {
        dir: run.dir,
        project: env.project,
        project_folder: folder,
        files: &files,
        assets_held: held,
        engine: run.engine,
        limits: env.limits,
        storage: Some(storage),
        progress: env.progress,
    };
    let opts = Options {
        connection: true,
        ..Default::default()
    };
    let ingested = match uploads::ingest(&ingest_env, &name, &bytes, &opts) {
        Ok(i) => i,
        Err(Fail::Refused { message, .. }) => return Err(message),
        Err(Fail::Storage(e)) => return Err(e.message),
        Err(Fail::Io(e)) => return Err(e.to_string()),
    };
    let moved = ingested.file.clone();
    let folder = folder.to_owned();
    let saved = store::update(run.dir, env.ctx.switch, |projects| {
        let Some(p) = store::find_mut(projects, env.project) else {
            return (false, false);
        };
        let mut files: Vec<JValue> = files_of(p)
            .into_iter()
            .filter(|f| f.get("name") != moved.get("name"))
            .collect();
        files.push(moved.clone());
        store::set(p, "files", JValue::Arr(files));
        uploads::apply_assets(p, &ingested, env.ctx.now_ms);
        let kept: Vec<JValue> = match p.get("assets") {
            JValue::Arr(a) => a
                .iter()
                .filter(|a| a.get("id").as_str() != Some(asset_id.as_str()))
                .cloned()
                .collect(),
            _ => Vec::new(),
        };
        store::set(p, "assets", JValue::Arr(kept));
        let mut sources = strs(p.get("sourceFolders"));
        if !sources.contains(&folder) {
            sources.push(folder.clone());
        }
        store::set(
            p,
            "sourceFolders",
            JValue::Arr(sources.iter().map(|x| s(x)).collect()),
        );
        if p.get("projectFolder").as_str().is_none_or(str::is_empty) {
            store::set(p, "projectFolder", s(&folder));
        }
        if crate::lifecycle::externalize(run.dir, p).is_err() {
            return (false, false);
        }
        (true, true)
    });
    match saved {
        Ok(true) => Ok(()),
        Ok(false) => Err("Project changed; retry.".into()),
        Err(e) => Err(format!("{e:?}")),
    }
}

/// Reads one entry and records its source; `Err` is the storage error to report.
#[allow(clippy::too_many_arguments)]
fn read_entry(
    run: &mut Run<'_>,
    storage: &dyn SourceStorage,
    entry: &Entry,
    folder: &str,
    previous: Option<&JValue>,
    kind: Kind,
    files_now: &[JValue],
    assets_held: usize,
    project_folder: &str,
) -> Result<(), StorageError> {
    let env = run.env;
    let dir = run.dir;
    let progress = env.progress;
    let Kind {
        managed,
        docx,
        is_doc,
    } = kind;
    if managed || docx {
        let read = read_file(dir, storage, run.engine, env.limits, entry)?;
        let ingest_env = IngestEnv {
            dir,
            project: env.project,
            project_folder,
            files: files_now,
            assets_held,
            engine: run.engine,
            limits: env.limits,
            storage: Some(storage),
            progress,
        };
        let opts = Options {
            connection: true,
            source: Some(folder),
            remote_path: Some(&entry.path),
            defer_open: true,
            ..Default::default()
        };
        let ingested = match uploads::ingest(&ingest_env, &entry.name, &read.bytes, &opts) {
            Ok(i) => i,
            Err(Fail::Storage(e)) => return Err(e),
            Err(Fail::Refused {
                status, message, ..
            }) => return Err(StorageError::new(status, message)),
            Err(Fail::Io(e)) => {
                eprintln!(
                    "noevia-server: a synced file could not be stored: {}",
                    e.kind()
                );
                return Err(StorageError::new(500, "Internal error"));
            }
        };
        let file = ingested.file.clone();
        run.note_document(&file, ingested.queued, &entry.path);
        if file.get("document").truthy()
            && file.get("document").get("state").as_str() != Some("ready")
        {
            let (text, extra) = problem_fields(&file);
            let mut fields = vec![
                ("folder", s(folder)),
                ("file", s(&entry.path)),
                ("reason", s(&text)),
            ];
            fields.extend(extra.iter().map(|(k, v)| (k.as_str(), v.clone())));
            fields.push(("retained", file.get("document").get("stale").clone()));
            run.skip(fields);
        }
        run.from_folders.push(file);
        // Every ingest, not only one that made an image: the save retires the vision image this name
        // used to have when it is replaced by a file too big to be one (uploads.cjs `ingest`'s finish).
        run.assets.push(ingested);
    } else if is_doc {
        let read = read_file(dir, storage, run.engine, env.limits, entry)?;
        let prev_text = previous.map(|p| reply::with_text(dir, env.project, p));
        let ingested = document_ingest::ingest(
            dir,
            env.project,
            &entry.path,
            &read.bytes,
            prev_text.as_ref(),
            run.engine,
            env.limits,
        )
        .map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                return StorageError::gone();
            }
            eprintln!(
                "noevia-server: a synced document could not be stored: {}",
                e.kind()
            );
            StorageError::new(500, "Internal error")
        })?;
        match ingested.outcome {
            "extracted" => run.stats.extracted += 1,
            "reopened" => run.stats.reopened += 1,
            _ => run.stats.unchanged += 1,
        }
        // Only a document whose worker is down gets a job. One the OCR allowance left open stays open with
        // none: the next sync continues it, within that sync's own allowance (an unbudgeted queue would
        // run every remaining page and hold the OCR slot for hours; source-limits.cjs).
        let queued = ingested.retryable;
        let mut file = with_source(ingested.file, folder);
        if queued {
            extract_queue::mark_queued(&mut file);
        }
        run.note_document(&file, queued, &entry.path);
        if file.get("document").get("state").as_str() != Some("ready") {
            let (text, extra) = problem_fields(&file);
            let mut fields = vec![
                ("folder", s(folder)),
                ("file", s(&entry.path)),
                ("reason", s(&text)),
            ];
            fields.extend(extra.iter().map(|(k, v)| (k.as_str(), v.clone())));
            fields.push(("retained", file.get("document").get("stale").clone()));
            run.skip(fields);
        }
        run.from_folders.push(file);
    } else {
        let text = storage.read_text(&entry.path)?;
        run.from_folders.push(obj(vec![
            ("name", s(&entry.path)),
            ("content", JValue::Str(text.content)),
            ("source", s(folder)),
        ]));
    }
    Ok(())
}

/// The read failed, so ingest never ran: the previous source stays (a document keeps its last readable text,
/// stale).
fn on_read_failure(
    run: &mut Run<'_>,
    folder: &str,
    entry: &Entry,
    previous: Option<&JValue>,
    is_doc: bool,
    e: &StorageError,
) {
    let env = run.env;
    run.halt_for(e);
    let why = reply::reason(e, "could not read");
    let code = reply::code(e);
    let prev_text = previous.map(|p| reply::with_text(run.dir, env.project, p));
    if is_doc {
        run.from_folders.push(with_source(
            failed(prev_text.as_ref(), &entry.path, &why),
            folder,
        ));
    } else if let Some(p) = previous {
        run.from_folders.push(p.clone());
    }
    let retained = previous.is_some_and(|p| reply::text_length(p) > 0);
    let mut fields = vec![
        ("folder", s(folder)),
        ("file", s(&entry.path)),
        ("reason", s(&why)),
    ];
    push_opt(&mut fields, "code", code.map(s));
    fields.push(("retained", JValue::Bool(retained)));
    run.skip(fields);
}

/// The merge against the project as it is now, the save, the prune and the outbox.
fn finish(
    run: &mut Run<'_>,
    folders: &[String],
    source_limit: usize,
    had_uploads: bool,
) -> Result<(Reply, Stats, u16), Reply> {
    let env = run.env;
    let ctx = env.ctx;
    let dir = run.dir;
    let changed = || reply::error_reply(409, "Project changed; retry.", None);
    // A document whose job must be written goes first: a record that says "queued" is saved only once
    // its job is in the queue (review #2191 r4+5).
    let mut earlier: Vec<(String, Option<extract_queue::Job>)> = Vec::new();
    for (name, byte_hash, version, error) in &run.queued {
        let before = extract_queue::pending(dir, env.project, name);
        if let Err(e) = extract_queue::enqueue(
            dir,
            env.project,
            name,
            byte_hash,
            version,
            error.as_deref(),
            ctx.now_ms,
        ) {
            eprintln!("noevia-server: extraction queue write failed: {e}");
            restore_all(dir, env.project, &mut earlier);
            // The job this one did not get is that of an earlier name still in place.
            let _ = (name, before);
            document_ingest::prune_now(dir, env.project);
            return Err(reply::error_reply(503, QUEUE_FAILED, None));
        }
        earlier.push((name.clone(), before));
    }
    let skipped = std::cell::RefCell::new(std::mem::take(&mut run.skipped));
    let from_folders = std::mem::take(&mut run.from_folders);
    let assets = std::mem::take(&mut run.assets);
    let due: std::cell::RefCell<(Vec<JValue>, Vec<String>)> = Default::default();
    let saved = store::update(dir, ctx.switch, |projects| {
        let Some(p) = store::find_mut(projects, env.project) else {
            return (Err(changed()), false);
        };
        let prev = files_of(p);
        // An upload or folder edit may have completed while storage was being read. Preserve current
        // uploads and never reattach a detached folder.
        let current: BTreeSet<String> = strs(p.get("sourceFolders")).into_iter().collect();
        let uploaded: Vec<&JValue> = prev.iter().filter(|f| !f.get("source").truthy()).collect();
        let untouched: Vec<&JValue> = prev
            .iter()
            .filter(|f| {
                f.get("source")
                    .as_str()
                    .is_some_and(|x| current.contains(x))
                    && !folders.iter().any(|x| f.get("source").as_str() == Some(x))
            })
            .collect();
        let synced: Vec<&JValue> = from_folders
            .iter()
            .filter(|f| {
                f.get("source")
                    .as_str()
                    .is_some_and(|x| current.contains(x))
            })
            .collect();
        // Existing uploads and other folders' sources are never dropped here; only newly synced files
        // beyond the cap are, and each is reported.
        let room = source_limit.saturating_sub(uploaded.len() + untouched.len());
        for f in synced.iter().skip(room) {
            skipped.borrow_mut().push(obj(vec![
                ("folder", f.get("source").clone()),
                ("file", f.get("name").clone()),
                (
                    "reason",
                    s(&format!(
                        "{source_limit}-source project limit reached; this file was not read."
                    )),
                ),
                ("code", s("limit")),
                ("retained", JValue::Bool(false)),
            ]));
        }
        let mut next: Vec<JValue> = Vec::new();
        next.extend(uploaded.into_iter().cloned());
        next.extend(untouched.into_iter().cloned());
        next.extend(synced.into_iter().take(room).cloned());
        for a in &assets {
            uploads::apply_assets(p, a, ctx.now_ms);
        }
        // The same RAG bookkeeping the config patch does: files that arrived or changed are indexed
        // again (a document's `indexing` is set as `indexSource` sets it), the ones that are gone are
        // dropped from the index once the list without them is on disk.
        let mut index: Vec<String> = Vec::new();
        for f in &mut next {
            let name = f.get("name").as_str().unwrap_or("").to_owned();
            let before = prev
                .iter()
                .find(|b| b.get("name").as_str() == Some(name.as_str()));
            let due_now = match before {
                None => true,
                Some(b) => {
                    !reply::same_text(dir, env.project, b, f)
                        || matches!(
                            f.get("document").get("indexing").as_str(),
                            Some("pending" | "failed" | "unavailable" | "partial")
                        )
                }
            };
            if due_now {
                if f.get("document").truthy() {
                    document_ingest::mark_indexing(f);
                }
                index.push(name);
            }
        }
        let names: BTreeSet<&str> = next.iter().filter_map(|f| f.get("name").as_str()).collect();
        let gone: Vec<String> = prev
            .iter()
            .filter_map(|f| f.get("name").as_str())
            .filter(|n| !names.contains(n))
            .map(str::to_owned)
            .collect();
        store::set(p, "files", JValue::Arr(next));
        // A file dropped into the project's own upload folder elsewhere is an upload too (#659).
        reply::apply_project_docs_default(p, env.catalogue, had_uploads);
        if let Err(r) = crate::lifecycle::externalize_reply(dir, p) {
            return (Err(r), false);
        }
        if !crate::lifecycle::reconcile_all(std::slice::from_mut(p)) {
            return (Err(Reply::internal()), false);
        }
        *due.borrow_mut() = (gone.into_iter().map(|g| s(&g)).collect(), index);
        (Ok(()), true)
    });
    let failed_save = match saved {
        Ok(Ok(())) => None,
        Ok(Err(r)) => Some(r),
        Err(e) => Some(crate::chats::store_failure(&e)),
    };
    if let Some(r) = failed_save {
        restore_all(dir, env.project, &mut earlier);
        return Err(r);
    }
    // Stored originals and images that no source points at any more go, against the list as saved.
    let pruned = store::update(dir, ctx.switch, |projects| {
        let Some(p) = store::find_mut(projects, env.project) else {
            return ((), false);
        };
        crate::files::prune_documents(dir, p);
        crate::files::prune_uploads(dir, p, ctx.now_ms);
        ((), true)
    });
    if let Err(e) = pruned {
        eprintln!("noevia-server: a sync's cleanup could not be saved: {e:?}");
    }
    // A document read now has nothing left to do in the queue.
    for name in &run.read_ok {
        if extract_queue::pending(dir, env.project, name).is_some() {
            let _ = extract_queue::clear(dir, env.project, name);
        }
    }
    // The saved project: the answer, and the RAG work for core.
    let (_, listed) = crate::originals::project(ctx, env.project)?;
    let listed = listed.ok_or_else(changed)?;
    let after = files_of(&listed);
    let (gone, index) = due.into_inner();
    let gone: Vec<String> = gone
        .iter()
        .filter_map(|g| g.as_str().map(str::to_owned))
        // #1914: once the list without them is on disk, and not listed again meanwhile.
        .filter(|g| {
            !after
                .iter()
                .any(|f| f.get("name").as_str() == Some(g.as_str()))
        })
        .collect();
    let mut jobs: Vec<outbox::Job<'_>> = Vec::new();
    for g in &gone {
        jobs.push(outbox::Job {
            kind: "unindex",
            project: env.project,
            file: Some(g.as_str()),
            payload: None,
        });
    }
    for n in &index {
        jobs.push(outbox::Job {
            kind: "index",
            project: env.project,
            file: Some(n.as_str()),
            payload: None,
        });
    }
    if !jobs.is_empty() {
        if let Err(e) = outbox::enqueue(dir, &jobs, ctx.now_ms) {
            eprintln!("noevia-server: index outbox write failed for a sync: {e:?}");
        }
    }
    run.stats.skipped = skipped.borrow().len();
    let files_out = JValue::Arr(
        after
            .iter()
            .map(|f| {
                let mut row = vec![
                    ("name", f.get("name").clone()),
                    (
                        "source",
                        f.get("source")
                            .as_str()
                            .filter(|x| !x.is_empty())
                            .map_or(JValue::Null, s),
                    ),
                    ("bytes", JValue::Num(reply::text_length(f) as f64)),
                ];
                if f.get("document").truthy() {
                    row.push(("document", f.get("document").clone()));
                }
                obj(row)
            })
            .collect(),
    );
    let body = obj(vec![
        ("files", files_out),
        ("skipped", JValue::Arr(skipped.into_inner())),
    ]);
    let stats = std::mem::take(&mut run.stats);
    Ok((reply::ok(200, &body), stats, 200))
}

fn restore_all(dir: &Path, project: &str, earlier: &mut Vec<(String, Option<extract_queue::Job>)>) {
    for (name, job) in earlier.drain(..) {
        restore(dir, project, &name, job);
    }
}

fn restore(dir: &Path, project: &str, name: &str, earlier: Option<extract_queue::Job>) {
    let put = match earlier {
        Some(j) => extract_queue::put_back(dir, &j),
        None => extract_queue::clear(dir, project, name),
    };
    if let Err(e) = put {
        eprintln!("noevia-server: extraction queue restore failed: {e}");
    }
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]
mod tests {
    use super::*;
    use crate::extract::tests::Fake as FakeEngine;
    use crate::source_storage::fake::Fake as FakeStorage;
    use crate::{extract_queue, outbox, store};

    const USER: &str = "11111111-2222-4333-8444-555555555555";
    const LIMITS: Limits = Limits {
        read_bytes: 96 * 1024 * 1024,
        pages: 1000,
        text: 8_000_000,
        range_pages: 25,
    };
    const GOOD: &str =
        r#"{"total":1,"pages":[{"number":1,"text":"hello","status":"native","method":"docling"}]}"#;

    struct Rig {
        root: tempfile::TempDir,
        engine: FakeEngine,
        storage: FakeStorage,
    }

    impl Rig {
        /// A project "proj-a" attaching the folder "docs" (its own folder is "root/proj-a").
        fn new(project: &str) -> Self {
            let root = tempfile::tempdir().unwrap();
            let dir = root.path().join("users").join(USER);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(
                dir.join(store::PROJECTS_FILE),
                format!("{{\"projects\":[{project}]}}"),
            )
            .unwrap();
            Rig {
                root,
                engine: FakeEngine {
                    docling: true,
                    ..Default::default()
                },
                storage: FakeStorage::default(),
            }
        }
        fn dir(&self) -> std::path::PathBuf {
            self.root.path().join("users").join(USER)
        }
        fn ctx(&self) -> Ctx<'_> {
            Ctx {
                data_dir: self.root.path(),
                user_id: USER,
                switch: server_store::RustProjects::owned(),
                now_ms: 1_700_000_000_000,
            }
        }
        fn sync(&self) -> (Reply, JValue) {
            self.sync_with(&Storage::Remote(&self.storage))
        }
        fn sync_with(&self, storage: &Storage<'_>) -> (Reply, JValue) {
            let progress = |_: &str| {};
            let reply = run(&SyncEnv {
                ctx: self.ctx(),
                project: "proj-a",
                storage,
                engine: &self.engine,
                limits: &LIMITS,
                catalogue: None,
                project_root: "root",
                progress: &progress,
                lock_wait: Duration::from_secs(5),
            });
            let Body::Json(text) = &reply.body else {
                panic!()
            };
            let body = wtf16::parse(text).unwrap();
            (reply, body)
        }
        fn files(&self) -> Vec<JValue> {
            let list = store::read(&self.dir()).unwrap();
            files_of(store::find(&list, "proj-a").unwrap())
        }
        fn names(&self) -> Vec<String> {
            self.files()
                .iter()
                .map(|f| f.get("name").as_str().unwrap().to_owned())
                .collect()
        }
    }

    use crate::Body;

    const PROJECT: &str = r#"{"id":"proj-a","name":"A","chats":[],"instructionSkills":{},"projectFolder":"root/proj-a","sourceFolders":["docs"]}"#;

    fn skipped(body: &JValue) -> Vec<JValue> {
        match body.get("skipped") {
            JValue::Arr(a) => a.clone(),
            _ => panic!(),
        }
    }

    #[test]
    fn an_attached_folder_is_walked_and_its_sources_replace_what_came_from_it() {
        let rig = Rig::new(PROJECT);
        rig.storage.put("docs/a.md", b"alpha");
        rig.storage.put("docs/sub/b.txt", b"beta");
        rig.storage.put("docs/.git/c.md", b"skipped on purpose");
        rig.storage.put("docs/ignored.bin", b"x");
        rig.storage.put("docs/r.pdf", b"%PDF-1.4 synthetic");
        rig.engine
            .replies
            .borrow_mut()
            .push(Ok(wtf16::parse(GOOD).unwrap()));
        let (reply, body) = rig.sync();
        assert_eq!(reply.status, 200, "{body:?}");
        let names: Vec<_> = match body.get("files") {
            JValue::Arr(f) => f
                .iter()
                .map(|f| f.get("name").as_str().unwrap().to_owned())
                .collect(),
            _ => panic!(),
        };
        assert_eq!(names, vec!["docs/a.md", "docs/r.pdf", "docs/sub/b.txt"]);
        assert!(skipped(&body).is_empty(), "{:?}", skipped(&body));
        let first = match body.get("files") {
            JValue::Arr(f) => f[0].clone(),
            _ => panic!(),
        };
        assert_eq!(first.get("source").as_str(), Some("docs"));
        assert_eq!(first.get("bytes").as_num(), Some(5.0));
        // The record is saved; its text is in the project's text files, read back.
        let saved = rig.files();
        assert_eq!(saved.len(), 3);
        let text = crate::sources::text(&rig.dir(), "proj-a", &saved[0]).unwrap();
        assert_eq!(text, "alpha");
        let pdf = saved
            .iter()
            .find(|f| f.get("name").as_str() == Some("docs/r.pdf"))
            .unwrap();
        assert_eq!(pdf.get("document").get("state").as_str(), Some("ready"));
        assert_eq!(
            pdf.get("document").get("indexing").as_str(),
            Some("pending")
        );
        // The index jobs for core: one per source that arrived.
        let jobs = outbox::jobs(&rig.dir()).unwrap();
        assert_eq!(
            jobs.iter().filter(|j| j.kind == "index").count(),
            3,
            "{jobs:?}"
        );
        // Streamed binary read, not a text read, for the document.
        assert!(rig.storage.log().contains(&"GET docs/r.pdf".to_owned()));
        assert!(rig.storage.log().contains(&"READ docs/a.md".to_owned()));
    }

    #[test]
    fn uploads_and_other_folders_stay_and_a_file_that_is_gone_is_unindexed() {
        let project = r#"{"id":"proj-a","name":"A","chats":[],"instructionSkills":{},"projectFolder":"root/proj-a","sourceFolders":["docs","other"],"files":[
            {"name":"mine.md","content":"an upload"},
            {"name":"docs/old.md","content":"gone from storage","source":"docs"},
            {"name":"other/keep.md","content":"another folder","source":"other"}]}"#;
        let rig = Rig::new(&project.replace('\n', ""));
        rig.storage.put("docs/new.md", b"fresh");
        // `other` is attached too: a refresh of the project re-reads both.
        rig.storage.put("other/keep.md", b"another folder");
        let (reply, _) = rig.sync();
        assert_eq!(reply.status, 200);
        let mut names = rig.names();
        names.sort();
        assert_eq!(names, vec!["docs/new.md", "mine.md", "other/keep.md"]);
        let jobs = outbox::jobs(&rig.dir()).unwrap();
        assert!(
            jobs.iter()
                .any(|j| j.kind == "unindex" && j.file.as_deref() == Some("docs/old.md")),
            "{jobs:?}"
        );
        assert!(jobs
            .iter()
            .any(|j| j.kind == "index" && j.file.as_deref() == Some("docs/new.md")));
        assert!(
            !jobs.iter().any(|j| j.file.as_deref() == Some("mine.md")),
            "an untouched upload is not indexed again"
        );
    }

    #[test]
    fn a_refused_login_stops_the_refresh_keeps_what_it_had_and_holds_the_automatic_ones() {
        let project = r#"{"id":"proj-a","name":"A","chats":[],"instructionSkills":{},"projectFolder":"root/proj-a","sourceFolders":["docs","more"],"files":[
            {"name":"docs/a.md","content":"kept text","source":"docs"},
            {"name":"more/b.md","content":"also kept","source":"more"}]}"#;
        let rig = Rig::new(&project.replace('\n', ""));
        rig.storage.list_errors.borrow_mut().insert(
            "docs".into(),
            StorageError::new(502, "storage returned 401").with_upstream(401),
        );
        let (reply, body) = rig.sync();
        assert_eq!(reply.status, 200);
        let skips = skipped(&body);
        assert_eq!(skips.len(), 2, "{skips:?}");
        assert_eq!(skips[0].get("code").as_str(), Some("storageLoginRejected"));
        assert_eq!(skips[0].get("retained").as_bool(), Some(true));
        assert_eq!(
            skips[1].get("folder").as_str(),
            Some("more"),
            "the next folder is not tried"
        );
        assert_eq!(skips[1].get("code").as_str(), Some("storageLoginRejected"));
        assert_eq!(rig.names().len(), 2, "both sources are still listed");
        assert_eq!(
            rig.storage.log(),
            vec!["LIST docs"],
            "no further storage call was made"
        );
        // The next automatic refresh is held; an explicit press runs.
        let held = automatic_hold(rig.ctx(), &Storage::Remote(&rig.storage), false).unwrap();
        assert_eq!(held.status, 502);
        assert!(matches!(&held.body, Body::Json(t) if t.contains("storageLoginRejected")));
        assert!(automatic_hold(rig.ctx(), &Storage::Remote(&rig.storage), true).is_none());
        // A listing that works proves the login again.
        rig.storage.list_errors.borrow_mut().clear();
        rig.storage.put("docs/a.md", b"kept text");
        rig.storage.put("more/b.md", b"also kept");
        let (reply, body) = rig.sync();
        assert_eq!(reply.status, 200);
        assert!(skipped(&body).is_empty());
        assert!(automatic_hold(rig.ctx(), &Storage::Remote(&rig.storage), false).is_none());
    }

    #[test]
    fn a_read_that_fails_keeps_the_previous_text_and_a_documents_last_readable_version() {
        let project = r#"{"id":"proj-a","name":"A","chats":[],"instructionSkills":{},"projectFolder":"root/proj-a","sourceFolders":["docs"],"files":[
            {"name":"docs/a.md","content":"previous text","source":"docs"},
            {"name":"docs/r.pdf","content":"last readable","source":"docs","document":{"version":"v","byteHash":"h","state":"ready","stale":false,"indexing":"ready"}}]}"#;
        let rig = Rig::new(&project.replace('\n', ""));
        rig.storage.put("docs/a.md", b"new text");
        rig.storage.put("docs/r.pdf", b"%PDF-1");
        let timeout = StorageError::new(504, "the storage server did not answer in time")
            .with_code("timeout");
        rig.storage
            .read_errors
            .borrow_mut()
            .insert("docs/a.md".into(), timeout.clone());
        rig.storage
            .read_errors
            .borrow_mut()
            .insert("docs/r.pdf".into(), timeout);
        let (_, body) = rig.sync();
        let skips = skipped(&body);
        assert_eq!(skips.len(), 2, "{skips:?}");
        assert_eq!(skips[0].get("code").as_str(), Some("storageTimeout"));
        assert_eq!(skips[0].get("retained").as_bool(), Some(true));
        let files = rig.files();
        let text = |f: &JValue| crate::sources::text(&rig.dir(), "proj-a", f).unwrap();
        assert_eq!(text(&files[0]), "previous text");
        assert_eq!(text(&files[1]), "last readable");
        let d = files[1].get("document");
        assert_eq!(d.get("state").as_str(), Some("failed"));
        assert_eq!(d.get("stale").as_bool(), Some(true));
        assert_eq!(
            d.get("error").as_str(),
            Some("Storage did not answer in time. Try again shortly.")
        );
        assert_eq!(
            rig.engine.asked.borrow().len(),
            0,
            "a failed read extracts nothing"
        );
        assert!(!skips[1].get("reason").as_str().unwrap().is_empty());
    }

    #[test]
    fn a_document_whose_docling_is_down_is_queued_before_the_record_says_so() {
        let rig = Rig::new(PROJECT);
        rig.storage.put("docs/r.pdf", b"%PDF-1.4 synthetic");
        // No reply scripted: Docling is unreachable.
        let (reply, body) = rig.sync();
        assert_eq!(reply.status, 200);
        let files = rig.files();
        let d = files[0].get("document");
        assert_eq!(d.get("extraction").as_str(), Some("queued"));
        // #2237: queued is the one state; "failed" and queued at once contradicted the record.
        assert_eq!(d.get("state").as_str(), Some("queued"));
        assert_eq!(d.get("error").as_str(), Some(extract_queue::WAITING));
        let job =
            extract_queue::pending(&rig.dir(), "proj-a", "docs/r.pdf").expect("the job is written");
        assert_eq!(job.byte_hash, d.get("byteHash").as_str().unwrap());
        assert_eq!(
            skipped(&body).len(),
            1,
            "the document is reported as not read"
        );
        // A later sync that reads it clears the job.
        rig.engine
            .replies
            .borrow_mut()
            .push(Ok(wtf16::parse(GOOD).unwrap()));
        rig.sync();
        let after = rig.files();
        assert_eq!(
            after[0].get("document").get("state").as_str(),
            Some("ready")
        );
        assert!(
            after[0].get("document").get("extraction").is_nullish(),
            "no longer queued"
        );
        assert!(
            extract_queue::pending(&rig.dir(), "proj-a", "docs/r.pdf").is_none(),
            "its job is done"
        );
    }

    #[test]
    fn the_sources_beyond_the_projects_limit_are_reported_not_read() {
        let rig = Rig::new(PROJECT);
        for n in 0..1005 {
            rig.storage.put(&format!("docs/f{n:04}.md"), b"x");
        }
        let (_, body) = rig.sync();
        assert_eq!(rig.names().len(), 1000);
        let skips = skipped(&body);
        assert_eq!(skips.len(), 5);
        assert_eq!(skips[0].get("code").as_str(), Some("limit"));
        assert_eq!(
            skips[0].get("reason").as_str(),
            Some("1000-source project limit reached; this file was not read.")
        );
        assert_eq!(skips[0].get("retained").as_bool(), Some(false));
    }

    /// The large-document lock is taken from the size the response declares, before the body (#1872), and
    /// held until the read's holder lets go; a small file never takes it.
    #[test]
    fn a_document_declared_large_takes_the_host_lock_before_its_body() {
        let _one = crate::extract::tests::SERIAL
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        let rig = Rig::new(PROJECT);
        rig.storage.put("docs/big.pdf", b"%PDF-1");
        let entry = |size: Option<f64>| Entry {
            name: "big.pdf".into(),
            path: "docs/big.pdf".into(),
            is_dir: false,
            size,
            ext: ".pdf".into(),
        };
        let read = |size: Option<f64>, declared: Option<u64>| {
            *rig.storage.declared.borrow_mut() = declared;
            read_file(&rig.dir(), &rig.storage, &rig.engine, &LIMITS, &entry(size)).unwrap()
        };
        let mib = 1024.0 * 1024.0;
        assert!(
            !read(Some(mib), None).held(),
            "small listed, small declared"
        );
        assert!(
            read(Some(mib), Some(30 * 1024 * 1024)).held(),
            "listed small, declared 30 MiB: the response decides"
        );
        assert!(
            read(None, None).held(),
            "an unknown listed size takes it first"
        );
        assert!(read(Some(40.0 * mib), None).held());
        // And a declared size past the 96 MiB cap is refused before the lock.
        *rig.storage.declared.borrow_mut() = Some(96 * 1024 * 1024 + 1);
        let e = read_file(
            &rig.dir(),
            &rig.storage,
            &rig.engine,
            &LIMITS,
            &entry(Some(mib)),
        )
        .err()
        .unwrap();
        assert_eq!(e.status, 413);
        // A non-PDF is read under the 25 MiB cap with no lock.
        *rig.storage.declared.borrow_mut() = None;
        let docx = Entry {
            name: "b.docx".into(),
            path: "docs/b.docx".into(),
            ..entry(None)
        };
        rig.storage.put("docs/b.docx", b"PK");
        assert!(
            !read_file(&rig.dir(), &rig.storage, &rig.engine, &LIMITS, &docx)
                .unwrap()
                .held()
        );
    }

    include!("sync/parity_tests.rs");
}
