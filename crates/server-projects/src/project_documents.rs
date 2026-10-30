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
//! `document.indexing` set as `indexSource` sets it, the save (texts first), the document folder
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
    let _held = match lock.lock() {
        Ok(g) => g,
        Err(p) => p.into_inner(),
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
        Err(_) => return Reply::internal(),
    };
    let mut file = ingested.file;
    let queued = ingested.retryable || ingested.open;
    if queued {
        extract_queue::mark_queued(&mut file);
    }
    mark_indexing(&mut file);
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
    match saved {
        Ok(Ok(())) => {}
        Ok(Err(reply)) => return reply,
        Err(e) => return chats::store_failure(&e),
    }
    let document = file.get("document").clone();
    if queued {
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
        }
    } else if extract_queue::pending(&dir, &id, &name).is_some() {
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

fn finish(dir: &std::path::Path, p: &mut JValue, _file: &JValue) -> (Result<(), Reply>, bool) {
    if crate::lifecycle::externalize(dir, p).is_err() {
        return (Err(Reply::internal()), false);
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
