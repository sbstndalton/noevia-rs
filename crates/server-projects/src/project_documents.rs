//! `POST /api/projects/{id}/documents` (sbstndalton/noevia#2191 round 5, group
//! `rust-project-documents`): core routes/projects.cjs's document upload, answer for answer, on the
//! Rust extraction pipeline ([`crate::extract`], [`crate::document_ingest`]).
//!
//! In Node's order: the id percent-decoded (a bad escape is a 500); getProject (its self-heal
//! saved); 404 `no such project`; the body read up to `ceil(25 MiB / 3) * 4 + 512 KiB` (past it 413
//! `That document is larger than the 25 MB limit.`), `JSON.parse` of it (400 `invalid JSON`, a `null`
//! body the TypeError's 500); `String(body.name || '').slice(0, 200)` checked by `isDocument` (400);
//! the base64 data (empty 400, over 25 MiB 413 with its size); then, one request per project at a
//! time in this process: the 20-upload cap (400), an existing upload of the name without
//! `replace: true` (409 `sourceExists`), the extraction, the project and the replaced upload still as
//! they were (409 `Project changed; retry.`), the file listed last in place of any upload of its name,
//! `document.indexing` set as `indexSource` sets it, a queued document's extraction job written
//! (a queue that cannot be written is a 503 [`QUEUE_FAILED`] and nothing is saved), the save (texts
//! first; a failed save puts back the name's earlier job), the document folder
//! pruned, an `index` job queued in the index outbox, and 200 `{ name, pages, characters,
//! truncated, document }`.
//!
//! The behaviour change (contracts/http/README.md): when Docling is unavailable, busy or not
//! configured, the answer is what core answered for an unreachable worker (state `failed`, the
//! worker's error, the stored record retryable) plus `document.extraction: "queued"`, and the
//! document's job waits in the durable extraction queue ([`crate::extract_queue`]) until Docling
//! reads it; core's in-process pdf.js reader is gone. `?background=1` is answered the same way,
//! synchronously (no client sends it for this route; the browser uploads through `/upload`).
use crate::document_ingest::{self, mark_indexing};
use crate::extract::{self, Engine, Limits};
use crate::extract_queue;
use crate::files::js_string;
use crate::{chats, js, outbox, store, Ctx, Reply};
use js_json::{wtf16, JValue};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

pub const DOCUMENT_UPLOAD_CAP: usize = 25 * 1024 * 1024;
/// `Math.ceil(DOCUMENT_UPLOAD_CAP / 3) * 4 + 512 * 1024`.
pub const BODY_LIMIT: usize = DOCUMENT_UPLOAD_CAP.div_ceil(3) * 4 + 512 * 1024;
pub const UPLOAD_LIMIT: usize = 20;

type Locks = Mutex<HashMap<String, Arc<Mutex<()>>>>;
static LOCKS: std::sync::OnceLock<Locks> = std::sync::OnceLock::new();

/// projects.cjs `withSourceLock(project, fn)` within this process: one source operation per project.
pub fn source_lock(user: &str, project: &str) -> Arc<Mutex<()>> {
    let locks = LOCKS.get_or_init(|| Mutex::new(HashMap::new()));
    let mut map = match locks.lock() {
        Ok(m) => m,
        Err(p) => p.into_inner(),
    };
    Arc::clone(
        map.entry(format!("{user}\u{0}{project}"))
            .or_insert_with(|| Arc::new(Mutex::new(()))),
    )
}

/// Waits for `lock` at most `wait` (polling, 5 ms apart): the guard, or `None` when another source
/// operation of the project still holds it. A lock a panicked holder left is taken over.
pub fn lock_within(
    lock: &Mutex<()>,
    wait: std::time::Duration,
) -> Option<std::sync::MutexGuard<'_, ()>> {
    acquire(lock, wait).map(|(guard, _)| guard)
}

/// [`lock_within`] for an operation that dates what it writes by its context: the guard, and the context
/// with its clock brought to now when the lock had to be waited for (a job queued behind another
/// operation of the project can start hours after its context was made).
pub fn lock_restamped<'l, 'c>(
    lock: &'l Mutex<()>,
    wait: std::time::Duration,
    ctx: Ctx<'c>,
) -> Option<(std::sync::MutexGuard<'l, ()>, Ctx<'c>)> {
    let (guard, waited) = acquire(lock, wait)?;
    let ctx = if waited {
        Ctx {
            now_ms: ctx.now_ms.max(crate::wall_ms()),
            ..ctx
        }
    } else {
        ctx
    };
    Some((guard, ctx))
}

/// The guard, and whether the first attempt found the lock held.
fn acquire(
    lock: &Mutex<()>,
    wait: std::time::Duration,
) -> Option<(std::sync::MutexGuard<'_, ()>, bool)> {
    let end = std::time::Instant::now() + wait;
    let mut waited = false;
    loop {
        match lock.try_lock() {
            Ok(g) => return Some((g, waited)),
            Err(std::sync::TryLockError::Poisoned(p)) => return Some((p.into_inner(), waited)),
            Err(std::sync::TryLockError::WouldBlock) => waited = true,
        }
        if std::time::Instant::now() >= end {
            return None;
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
}

/// How long a foreground upload or sync waits for the project's source lock.
pub const FOREGROUND_LOCK_WAIT: std::time::Duration = std::time::Duration::from_secs(30);

/// The answer of a source operation that found the project's source lock held past its wait.
pub fn busy_reply() -> Reply {
    Reply::error(
        429,
        "Another upload or sync of this project is still running; try again shortly.",
    )
}

fn source_exists(name: &str) -> Reply {
    let body = JValue::obj([
        ("code", JValue::Str("sourceExists".into())),
        ("name", JValue::Str(name.to_string())),
        (
            "error",
            JValue::Str(format!(
                "A source named \"{name}\" already exists in this project, so it was not replaced. Upload it under another name, or choose to replace it."
            )),
        ),
    ]);
    Reply::json(409, wtf16::stringify(&body).unwrap_or_default())
}

fn uploads(project: &JValue) -> Vec<JValue> {
    match project.get("files") {
        JValue::Arr(files) => files
            .iter()
            .filter(|f| !f.get("source").truthy())
            .cloned()
            .collect(),
        _ => Vec::new(),
    }
}

/// routes/projects.cjs `sameSource(a, b)`: the same metadata (`content` left out).
fn same_source(a: Option<&JValue>, b: Option<&JValue>) -> bool {
    match (a, b) {
        (None, None) => true,
        (Some(a), Some(b)) => {
            let meta = |f: &JValue| -> Vec<(String, String)> {
                let JValue::Obj(items) = f else {
                    return Vec::new();
                };
                let mut m: Vec<(String, String)> = items
                    .iter()
                    .filter(|(k, v)| k != "content" && !matches!(v, JValue::Undefined))
                    .map(|(k, v)| (k.clone(), wtf16::stringify(v).unwrap_or_default()))
                    .collect();
                m.sort();
                m
            };
            meta(a) == meta(b)
        }
        _ => false,
    }
}

/// `Math.round(bytes / 1024 / 1024)`.
fn megabytes(n: usize) -> u64 {
    (n as f64 / 1024.0 / 1024.0 + 0.5).floor() as u64
}

/// POST /api/projects/{id}/documents. `body` was read up to [`BODY_LIMIT`] (`over` past it).
pub fn post(
    ctx: Ctx<'_>,
    raw: &str,
    body: &[u8],
    over: bool,
    engine: &dyn Engine,
    limits: &Limits,
) -> Reply {
    post_waiting(ctx, raw, body, over, engine, limits, FOREGROUND_LOCK_WAIT)
}

/// [`post`] that waits at most `lock_wait` for the project's source lock (429 past it).
fn post_waiting(
    ctx: Ctx<'_>,
    raw: &str,
    body: &[u8],
    over: bool,
    engine: &dyn Engine,
    limits: &Limits,
    lock_wait: std::time::Duration,
) -> Reply {
    let Some(id) = js::decode_uri_component(raw) else {
        return Reply::internal();
    };
    let (dir, project) = match crate::originals::project(ctx, &id) {
        Ok(v) => v,
        Err(r) => return r,
    };
    if project.is_none() {
        return Reply::error(404, "no such project");
    }
    if over {
        return Reply::error(413, "That document is larger than the 25 MB limit.");
    }
    let Ok(parsed) = wtf16::parse(&String::from_utf8_lossy(body)) else {
        return chats::invalid_json();
    };
    // `body.name` of `null` throws: the generic 500.
    if matches!(parsed, JValue::Null) {
        return Reply::internal();
    }
    let raw_name = if parsed.get("name").truthy() {
        js_string(parsed.get("name"))
    } else {
        String::new()
    };
    let name = wtf16::slice(&raw_name, 200);
    if !extract::is_document(&name, engine.docling_configured()) {
        let shown = if name.is_empty() { "that file" } else { &name };
        return Reply::error(400, &format!("{shown} is not a supported document (PDF)."));
    }
    let data = if parsed.get("dataBase64").truthy() {
        js_string(parsed.get("dataBase64"))
    } else {
        String::new()
    };
    let bytes = js::base64_loose(&data);
    if bytes.is_empty() {
        return Reply::error(400, "document data was empty");
    }
    if bytes.len() > DOCUMENT_UPLOAD_CAP {
        return Reply::error(
            413,
            &format!(
                "That document is {} MB, over the 25 MB limit.",
                megabytes(bytes.len())
            ),
        );
    }
    let replace = parsed.get("replace").as_bool() == Some(true);
    let lock = source_lock(ctx.user_id, &id);
    let Some(_held) = lock_within(&lock, lock_wait) else {
        return busy_reply();
    };
    // Under the lock: the project as it is now.
    let (_, project) = match crate::originals::project(ctx, &id) {
        Ok(v) => v,
        Err(r) => return r,
    };
    let Some(project) = project else {
        return Reply::error(409, "Project changed; retry.");
    };
    let listed = uploads(&project);
    if listed.len() >= UPLOAD_LIMIT && !listed.iter().any(|f| f.get("name").as_str() == Some(&name))
    {
        return Reply::error(400, "A project holds at most 20 uploaded sources.");
    }
    let previous = listed
        .iter()
        .find(|f| f.get("name").as_str() == Some(&name))
        .cloned();
    if previous.is_some() && !replace {
        return source_exists(&name);
    }
    let previous_text = previous.as_ref().map(|f| with_text(&dir, &id, f));
    let ingested = match document_ingest::ingest(
        &dir,
        &id,
        &name,
        &bytes,
        previous_text.as_ref(),
        engine,
        limits,
    ) {
        Ok(i) => i,
        Err(e) => return crate::source_reply::io_failure(&e),
    };
    let mut file = ingested.file;
    let queued = ingested.retryable || ingested.open;
    if queued {
        extract_queue::mark_queued(&mut file);
    }
    mark_indexing(&mut file);
    // The job is written before the record that says "queued" (review #2191 r4+5): a record
    // saved first and a queue write that failed after it would leave a queued document no
    // worker ever reads. A job whose record is never saved is dropped by the worker
    // (`find_source` matches the byte hash), and an earlier job of the name is put back.
    let earlier = if queued {
        let earlier = extract_queue::pending(&dir, &id, &name);
        let document = file.get("document");
        if let Err(e) = extract_queue::enqueue(
            &dir,
            &id,
            &name,
            document.get("byteHash").as_str().unwrap_or(""),
            document.get("version").as_str().unwrap_or(""),
            document.get("error").as_str(),
            ctx.now_ms,
        ) {
            eprintln!("noevia-server: extraction queue write failed: {e}");
            document_ingest::prune_now(&dir, &id);
            return Reply::error(503, QUEUE_FAILED);
        }
        Some(earlier)
    } else {
        None
    };
    let saved = store::update(&dir, ctx.switch, |projects| {
        let Some(p) = store::find_mut(projects, &id) else {
            return (Err(Reply::error(409, "Project changed; retry.")), false);
        };
        let JValue::Arr(files) = p.get("files").clone() else {
            if previous.is_some() {
                return (Err(Reply::error(409, "Project changed; retry.")), false);
            }
            store::set(p, "files", JValue::Arr(vec![file.clone()]));
            return finish(&dir, p, &file);
        };
        let now = files
            .iter()
            .find(|f| !f.get("source").truthy() && f.get("name").as_str() == Some(&name));
        if !same_source(now, previous.as_ref()) {
            return (Err(Reply::error(409, "Project changed; retry.")), false);
        }
        let mut kept: Vec<JValue> = files
            .iter()
            .filter(|f| f.get("source").truthy() || f.get("name").as_str() != Some(&name))
            .cloned()
            .collect();
        kept.push(file.clone());
        store::set(p, "files", JValue::Arr(kept));
        finish(&dir, p, &file)
    });
    let failed = match saved {
        Ok(Ok(())) => None,
        Ok(Err(reply)) => Some(reply),
        Err(e) => Some(chats::store_failure(&e)),
    };
    if let Some(reply) = failed {
        if let Some(earlier) = earlier {
            restore(&dir, &id, &name, earlier);
        }
        return reply;
    }
    let document = file.get("document").clone();
    if !queued && extract_queue::pending(&dir, &id, &name).is_some() {
        // A newer upload of the name was read now: the older queued job has nothing left to do.
        let _ = extract_queue::clear(&dir, &id, &name);
    }
    document_ingest::prune_now(&dir, &id);
    let index = outbox::Job {
        kind: "index",
        project: &id,
        file: Some(&name),
        payload: None,
    };
    if let Err(e) = outbox::enqueue(&dir, &[index], ctx.now_ms) {
        eprintln!("noevia-server: index outbox write failed for a document: {e:?}");
    }
    let characters = wtf16::len(file.get("content").as_str().unwrap_or(""));
    let body = JValue::obj([
        ("name", JValue::Str(name.clone())),
        ("pages", document.get("pages").clone()),
        ("characters", JValue::Num(characters as f64)),
        ("truncated", document.get("truncated").clone()),
        ("document", document),
    ]);
    Reply::json(200, wtf16::stringify(&body).unwrap_or_default())
}

/// The answer when the extraction job could not be written: nothing was saved.
pub const QUEUE_FAILED: &str =
    "Document extraction could not be queued; nothing was changed. Try again shortly.";

/// Puts back the job the name had before this upload's job replaced it (the save failed).
fn restore(dir: &std::path::Path, project: &str, name: &str, earlier: Option<extract_queue::Job>) {
    let put = match earlier {
        Some(j) => extract_queue::put_back(dir, &j),
        None => extract_queue::clear(dir, project, name),
    };
    if let Err(e) = put {
        eprintln!("noevia-server: extraction queue restore failed: {e}");
    }
}

fn finish(dir: &std::path::Path, p: &mut JValue, _file: &JValue) -> (Result<(), Reply>, bool) {
    if let Err(r) = crate::lifecycle::externalize_reply(dir, p) {
        return (Err(r), false);
    }
    if !crate::lifecycle::reconcile_all(std::slice::from_mut(p)) {
        return (Err(Reply::internal()), false);
    }
    (Ok(()), true)
}

/// The source with its text (inline, or read from its text file).
fn with_text(dir: &std::path::Path, project: &str, file: &JValue) -> JValue {
    let mut f = file.clone();
    if f.get("content").as_str().is_none() {
        let text = crate::sources::text(dir, project, file).unwrap_or_default();
        if let JValue::Obj(items) = &mut f {
            wtf16::set(items, "content", JValue::Str(text));
        }
    }
    f
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::indexing_slicing,
    clippy::panic,
    clippy::expect_used
)]
mod tests {
    use super::*;
    use crate::extract::tests::Fake;

    const USER: &str = "11111111-2222-4333-8444-555555555555";
    const PROJECTS: &str =
        "{\n  \"projects\": [\n    {\n      \"id\": \"proj-a\",\n      \"name\": \"A\",\n      \"chats\": [],\n      \"instructionSkills\": {}\n    }\n  ]\n}";
    const LIMITS: Limits = Limits {
        read_bytes: 1 << 30,
        pages: 1000,
        text: 8_000_000,
        range_pages: 25,
    };

    fn ctx(root: &std::path::Path) -> Ctx<'_> {
        Ctx {
            data_dir: root,
            user_id: USER,
            switch: server_store::RustProjects::owned(),
            now_ms: 1_700_000_000_000,
        }
    }

    fn body() -> Vec<u8> {
        // "%PDF-1.4 synthetic" in base64.
        br#"{"name":"a.pdf","dataBase64":"JVBERi0xLjQgc3ludGhldGlj"}"#.to_vec()
    }

    fn listed(dir: &std::path::Path) -> usize {
        let projects = store::read(dir).unwrap();
        match store::find(&projects, "proj-a").unwrap().get("files") {
            JValue::Arr(f) => f.len(),
            _ => 0,
        }
    }

    /// Review #2191 r4+5: the record said "queued" before the job was written, and a queue write
    /// that failed only logged and answered 200, so no worker ever read the document.
    #[test]
    fn an_unwritable_queue_is_a_503_and_saves_nothing_then_a_retry_queues_the_job() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("users").join(USER);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(store::PROJECTS_FILE), PROJECTS).unwrap();
        // A directory where the queue file should be: SQLite cannot open it.
        std::fs::create_dir(extract_queue::path(&dir)).unwrap();
        let fake = Fake::default();
        let r = post(ctx(root.path()), "proj-a", &body(), false, &fake, &LIMITS);
        assert_eq!(r.status, 503, "{r:?}");
        assert_eq!(listed(&dir), 0, "nothing saved without its job");

        std::fs::remove_dir(extract_queue::path(&dir)).unwrap();
        let r = post(ctx(root.path()), "proj-a", &body(), false, &fake, &LIMITS);
        assert_eq!(r.status, 200, "{r:?}");
        assert_eq!(listed(&dir), 1);
        let job = extract_queue::pending(&dir, "proj-a", "a.pdf").expect("the job is queued");
        let projects = store::read(&dir).unwrap();
        let JValue::Arr(files) = store::find(&projects, "proj-a")
            .unwrap()
            .get("files")
            .clone()
        else {
            panic!()
        };
        let doc = files[0].get("document");
        assert_eq!(doc.get("extraction").as_str(), Some("queued"));
        assert_eq!(doc.get("byteHash").as_str(), Some(job.byte_hash.as_str()));
    }

    /// #2191 round 8 re-review N4: the document route waits a bounded time for the project's source lock
    /// and then says 429, as the other source routes do; a hung holder cannot pile up its requests.
    #[test]
    fn a_document_waits_a_bounded_time_for_the_project_lock_and_then_says_429() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("users").join(USER);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(store::PROJECTS_FILE), PROJECTS).unwrap();
        let fake = Fake::default();
        let lock = source_lock(USER, "proj-a");
        let held = lock.lock().unwrap();
        let started = std::time::Instant::now();
        let wait = std::time::Duration::from_millis(100);
        let r = post_waiting(
            ctx(root.path()),
            "proj-a",
            &body(),
            false,
            &fake,
            &LIMITS,
            wait,
        );
        assert_eq!(r.status, 429, "{r:?}");
        assert!(started.elapsed() >= wait && started.elapsed() < std::time::Duration::from_secs(5));
        assert_eq!(listed(&dir), 0, "nothing was saved");
        drop(held);
        let r = post_waiting(
            ctx(root.path()),
            "proj-a",
            &body(),
            false,
            &fake,
            &LIMITS,
            wait,
        );
        assert_eq!(r.status, 200, "{r:?}");
    }

    /// An engine that, when the ingest first asks whether Docling is configured (the upload asked
    /// once before reading the project), saves another upload of the same name into the project:
    /// a second request landing between this one's read and its save.
    struct Racing {
        inner: Fake,
        dir: std::path::PathBuf,
        asked: std::cell::Cell<u32>,
        fired: std::cell::Cell<bool>,
    }
    impl Racing {
        fn new(dir: &std::path::Path) -> Self {
            Racing {
                inner: Fake::default(),
                dir: dir.to_path_buf(),
                asked: std::cell::Cell::new(0),
                fired: std::cell::Cell::new(false),
            }
        }
        fn race(&self) {
            self.fired.set(true);
            std::fs::write(
                self.dir.join(store::PROJECTS_FILE),
                r#"{"projects":[{"id":"proj-a","name":"A","chats":[],"instructionSkills":{},"files":[{"name":"a.pdf","content":"another upload"}]}]}"#,
            )
            .unwrap();
        }
    }
    impl Engine for Racing {
        fn docling_configured(&self) -> bool {
            self.asked.set(self.asked.get() + 1);
            if self.asked.get() == 2 {
                self.race();
            }
            self.inner.docling_configured()
        }
        fn docling(
            &self,
            name: &str,
            bytes: &[u8],
            figures: bool,
            pages: Option<(u32, u32)>,
        ) -> Result<JValue, crate::extract::EngineError> {
            self.inner.docling(name, bytes, figures, pages)
        }
        fn docling_ranges(&self, bytes: usize) -> Option<bool> {
            self.inner.docling_ranges(bytes)
        }
        fn ocr_configured(&self) -> bool {
            self.inner.ocr_configured()
        }
        fn ocr_pages(
            &self,
            bytes: &[u8],
            pages: &[u32],
            dpi: Option<u32>,
            timeout_ms: Option<u64>,
        ) -> Result<Vec<JValue>, crate::extract::EngineError> {
            self.inner.ocr_pages(bytes, pages, dpi, timeout_ms)
        }
        fn ocr_caps(&self) -> Option<crate::extract::OcrCaps> {
            self.inner.ocr_caps()
        }
        fn multimodal(&self) -> bool {
            self.inner.multimodal()
        }
        fn busy_waits(&self) -> Vec<std::time::Duration> {
            self.inner.busy_waits()
        }
    }

    /// Review #2191 r6+7: no test drove `post`'s own failed-save branch, so deleting its restore
    /// failed nothing. The save is forced to a 409 (another upload of the name landed first) after
    /// this upload's job replaced the name's earlier one: the earlier job must be back.
    #[test]
    fn a_save_that_loses_a_race_through_post_is_a_409_and_restores_the_earlier_job() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("users").join(USER);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(store::PROJECTS_FILE), PROJECTS).unwrap();
        extract_queue::enqueue(&dir, "proj-a", "a.pdf", "old", "v0", Some("busy"), 5).unwrap();
        let earlier = extract_queue::pending(&dir, "proj-a", "a.pdf").unwrap();
        let engine = Racing::new(&dir);
        let r = post(ctx(root.path()), "proj-a", &body(), false, &engine, &LIMITS);
        assert!(engine.fired.get(), "the race happened during the upload");
        assert_eq!(r.status, 409, "{r:?}");
        assert_eq!(
            extract_queue::pending(&dir, "proj-a", "a.pdf"),
            Some(earlier),
            "the failed save put the earlier job back"
        );
        // And with no earlier job there is none left over.
        extract_queue::clear(&dir, "proj-a", "a.pdf").unwrap();
        std::fs::write(dir.join(store::PROJECTS_FILE), PROJECTS).unwrap();
        let engine = Racing::new(&dir);
        let r = post(ctx(root.path()), "proj-a", &body(), false, &engine, &LIMITS);
        assert!(engine.fired.get());
        assert_eq!(r.status, 409, "{r:?}");
        assert_eq!(extract_queue::pending(&dir, "proj-a", "a.pdf"), None);
    }

    /// A save that fails after the job was written puts the name's earlier job back.
    #[test]
    fn a_failed_save_puts_the_earlier_job_back() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("users").join(USER);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(store::PROJECTS_FILE), PROJECTS).unwrap();
        extract_queue::enqueue(&dir, "proj-a", "a.pdf", "old", "v0", Some("busy"), 5).unwrap();
        let before = extract_queue::pending(&dir, "proj-a", "a.pdf").unwrap();
        restore(&dir, "proj-a", "a.pdf", Some(before.clone()));
        extract_queue::enqueue(&dir, "proj-a", "a.pdf", "new", "v1", None, 9).unwrap();
        restore(&dir, "proj-a", "a.pdf", Some(before.clone()));
        assert_eq!(
            extract_queue::pending(&dir, "proj-a", "a.pdf"),
            Some(before)
        );
        restore(&dir, "proj-a", "a.pdf", None);
        assert_eq!(extract_queue::pending(&dir, "proj-a", "a.pdf"), None);
    }
}
