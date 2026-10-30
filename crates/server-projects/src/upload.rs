//! `POST /api/projects/{id}/upload` and `POST /api/projects/{id}/folder-import/plan` (sbstndalton/noevia#2191
//! round 8, group `rust-project-source-work`): core routes/projects.cjs's upload, answer for answer.
//!
//! An **organized** upload (`organized: true`, what the browser sends) stores the file in its type
//! group in the project's storage folder ([`crate::uploads_ingest`]): the id decoded (a bad escape 500),
//! 404 `no such project`, the body read up to `ceil(60 MiB / 3) * 4 + 512 KiB` (413 `That file is larger
//! than the 25 MB limit.`; 400 `invalid JSON`), a name (400 `a filename is required`), a folder import's
//! `relativePath` and ticket ([`crate::folder_import::admit`]: only a path the plan accepted, within its
//! declared size and the import's byte cap), then, one operation per project at a time: the project as it
//! is (409), an existing source of the name without `replace: true` (409 `sourceExists`; a folder import
//! replaces, and a file unchanged since the last import is left alone), a PDF over 25 MB reduced by the OCR
//! worker (pdf-reduce.cjs, up to 60 MB), the project's storage folder made when it has none, the
//! ingest (a create-only write when not replacing, removed again if the record cannot be saved,
//! #1664), the record saved with the file last in the list, and 200 `{ name, path, bytes, document,
//! attachment }`.
//!
//! The older form (no `organized`) puts a text file or a document straight into the project's folder (or
//! keeps it in noevia without storage) and answers `{ name, path, bytes, document }`.
//!
//! A document whose Docling is down is queued durably (`document.extraction: "queued"`) before the
//! record that says so is saved, as `POST .../documents` does.
use crate::extract::{self, Engine, Limits};
use crate::lifecycle::Catalogue;
use crate::source_reply as reply;
use crate::source_storage::{Condition, Storage};
use crate::uploads_ingest::{self as uploads, Env as IngestEnv, Fail, Options};
use crate::{
    chats, document_ingest, extract_queue, folder_import, js, outbox, project_documents, store,
    Ctx, Reply,
};
use js_json::{wtf16, JValue};
use std::path::Path;

/// pdf-reduce.cjs INPUT_CAP and OUTPUT_CAP.
pub const INPUT_CAP: usize = 60 * 1024 * 1024;
pub const OUTPUT_CAP: usize = 25 * 1024 * 1024;
/// `Math.ceil(INPUT_CAP / 3) * 4 + 512 * 1024`.
pub const BODY_LIMIT: usize = INPUT_CAP.div_ceil(3) * 4 + 512 * 1024;
/// readBody's limit for a folder-import plan.
pub const PLAN_LIMIT: usize = 4 * 1024 * 1024;
const DOCUMENT_UPLOAD_CAP: usize = 25 * 1024 * 1024;
pub const QUEUE_FAILED: &str =
    "Document extraction could not be queued; nothing was changed. Try again shortly.";
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

pub struct Env<'a> {
    pub ctx: Ctx<'a>,
    pub storage: &'a Storage<'a>,
    pub engine: &'a dyn Engine,
    pub limits: &'a Limits,
    pub catalogue: Option<&'a Catalogue>,
    /// index.cjs PROJECT_ROOT_FOLDER.
    pub project_root: &'a str,
    pub progress: &'a dyn Fn(&str),
}

fn s(v: &str) -> JValue {
    JValue::Str(v.to_owned())
}

/// What an exception becomes in Node (http.cjs `errorResponse`): a 4xx keeps its message, anything else is
/// a generic text on its 5xx status (or 500).
fn thrown(status: u16, message: &str) -> Reply {
    if (400..500).contains(&status) {
        return Reply::error(status, message);
    }
    let status = if (500..600).contains(&status) {
        status
    } else {
        500
    };
    Reply::error(status, "Internal error")
}

fn fail_reply(f: Fail) -> Reply {
    match f {
        Fail::Refused {
            status,
            message,
            code,
        } => {
            if status == 400 && code.is_some() {
                reply::error_reply(400, &message, code)
            } else {
                thrown(status, &message)
            }
        }
        Fail::Storage(e) => thrown(e.status, &e.message),
        Fail::Io(e) => {
            eprintln!("noevia-server: an upload could not be stored: {}", e.kind());
            Reply::internal()
        }
    }
}

/// pdf-reduce.cjs `prepare(name, bytes)`: a file within 25 MB as it is; a larger PDF (up to 60 MB)
/// compressed, or reduced to its text, by the OCR worker's `/reduce-pdf`.
pub struct Reduced {
    pub name: String,
    pub bytes: Vec<u8>,
    pub reduction: Option<JValue>,
}

pub fn prepare(
    engine: &dyn Engine,
    name: &str,
    bytes: &[u8],
    progress: &dyn Fn(&str),
) -> Result<Reduced, Fail> {
    if bytes.len() <= OUTPUT_CAP {
        return Ok(Reduced {
            name: name.to_owned(),
            bytes: bytes.to_vec(),
            reduction: None,
        });
    }
    let refuse = |status: u16, message: &str| Fail::Refused {
        status,
        message: message.to_owned(),
        code: None,
    };
    if !name.to_lowercase().ends_with(".pdf") || bytes.len() > INPUT_CAP {
        return Err(refuse(
            413,
            "Files are limited to 25 MB; PDFs up to 60 MB can be reduced automatically.",
        ));
    }
    if !bytes.starts_with(b"%PDF-") {
        return Err(refuse(400, "This file is not a PDF."));
    }
    if !engine.ocr_configured() {
        return Err(refuse(
            503,
            "PDF compression is unavailable. Compress or split this PDF locally and retry.",
        ));
    }
    progress("Extracting PDF text and compressing images to fit 25 MB");
    let result = match engine.reduce_pdf(bytes) {
        None => {
            return Err(refuse(
                503,
                "PDF compression is unavailable. Compress or split this PDF locally and retry.",
            ))
        }
        Some(Ok(r)) => r,
        Some(Err(e)) => {
            return Err(match e.status {
                _ if e.permanent => refuse(502, &e.message),
                Some(503) => refuse(503, "Document processor is busy; retry shortly."),
                Some(_) => refuse(
                    422,
                    "PDF could not fit 25 MB after compression or text extraction. Split or compress it locally and retry.",
                ),
                None => refuse(500, &e.message),
            })
        }
    };
    let stem = wtf16::slice(name.get(..name.len().saturating_sub(4)).unwrap_or(""), 175);
    let kind = result.get("kind").as_str().unwrap_or("");
    let output: Option<(String, Vec<u8>)> = match kind {
        "pdf" => result
            .get("dataBase64")
            .as_str()
            .filter(|d| d.len() <= OUTPUT_CAP.div_ceil(3) * 4)
            .map(|d| (format!("{stem}.compressed.pdf"), js::base64_loose(d))),
        "text" => result
            .get("text")
            .as_str()
            .filter(|t| wtf16::len(t) <= 200_000)
            .map(|t| (format!("{stem}.extracted.txt"), t.as_bytes().to_vec())),
        _ => None,
    };
    if let Some((_, out)) = &output {
        if kind == "pdf" && !out.starts_with(b"%PDF-") {
            return Err(refuse(500, "Invalid reduced PDF"));
        }
    }
    let Some((out_name, out)) = output.filter(|(_, o)| !o.is_empty() && o.len() <= OUTPUT_CAP)
    else {
        return Err(refuse(500, "Invalid PDF reduction response"));
    };
    let note = if kind == "pdf" {
        "Compressed PDF; image quality or interactive features may change. Original remains on your computer."
    } else {
        "Text-only extraction; images, scanned-page text and layout omitted. Original remains on your computer."
    };
    Ok(Reduced {
        name: out_name,
        bytes: out,
        reduction: Some(JValue::obj([
            ("originalName", s(name)),
            ("originalBytes", JValue::Num(bytes.len() as f64)),
            ("kind", s(kind)),
            ("note", s(note)),
        ])),
    })
}

fn extension_of_raw(name: &str) -> String {
    // `(rawName.slice(rawName.lastIndexOf('.')) || '').toLowerCase()`: with no dot, the last character.
    match name.rfind('.') {
        Some(i) => name.get(i..).unwrap_or("").to_lowercase(),
        None => name
            .chars()
            .last()
            .map(String::from)
            .unwrap_or_default()
            .to_lowercase(),
    }
}

fn files_of(p: &JValue) -> Vec<JValue> {
    match p.get("files") {
        JValue::Arr(a) => a.clone(),
        _ => Vec::new(),
    }
}

/// The checks the background form makes before it accepts the request (core's worker, before its
/// `accepted` line): the id decoded, the project (404 `project not found`), the body within its cap.
/// Returns the parsed body.
pub fn precheck(ctx: Ctx<'_>, raw_id: &str, body: &[u8], over: bool) -> Result<(), Reply> {
    let Some(id) = js::decode_uri_component(raw_id) else {
        return Err(Reply::internal());
    };
    let (_, project) = crate::originals::project(ctx, &id)?;
    if project.is_none() {
        return Err(Reply::error(404, "project not found"));
    }
    if over {
        return Err(Reply::error(413, "Request exceeds size limit"));
    }
    // `readJson`: an empty body is `{}`; anything else must parse.
    let text = String::from_utf8_lossy(body);
    if !text.is_empty() && wtf16::parse(&text).is_err() {
        return Err(Reply::error(400, "invalid JSON"));
    }
    Ok(())
}

/// POST /api/projects/{id}/upload. `body` was read up to [`BODY_LIMIT`] (`over` past it).
pub fn post(env: &Env<'_>, raw_id: &str, body: &[u8], over: bool) -> Reply {
    let ctx = env.ctx;
    let Some(id) = js::decode_uri_component(raw_id) else {
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
        return Reply::error(413, "That file is larger than the 25 MB limit.");
    }
    let parsed = match wtf16::parse(&String::from_utf8_lossy(body)) {
        Ok(v) => v,
        Err(_) => return chats::invalid_json(),
    };
    // `body.name` of `null` throws: the generic 500.
    if matches!(parsed, JValue::Null) {
        return Reply::internal();
    }
    let raw_full = if parsed.get("name").truthy() {
        crate::files::js_string(parsed.get("name"))
    } else {
        String::new()
    };
    let raw_name = wtf16::slice(raw_full.rsplit('/').next().unwrap_or(""), 200);
    if raw_name.is_empty() {
        return Reply::error(400, "a filename is required");
    }
    if parsed.get("organized").as_bool() == Some(true) {
        return organized(env, &dir, &id, &parsed, &raw_full);
    }
    let is_text = TEXT_EXTENSIONS.contains(&extension_of_raw(&raw_name).as_str());
    if !is_text && !extract::is_document(&raw_name, env.engine.docling_configured()) {
        return Reply::error(
            400,
            &format!("{raw_name} is not a supported source (text file or PDF)."),
        );
    }
    let data = if parsed.get("dataBase64").truthy() {
        crate::files::js_string(parsed.get("dataBase64"))
    } else {
        String::new()
    };
    let bytes = js::base64_loose(&data);
    if bytes.is_empty() {
        return Reply::error(400, "file was empty");
    }
    if bytes.len() > DOCUMENT_UPLOAD_CAP {
        return Reply::error(413, "File exceeds the 25 MB limit.");
    }
    legacy(env, &dir, &id, &parsed, &raw_name, &bytes)
}

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
                    .filter(|(k, v)| {
                        k != "content"
                            && k != "textRef"
                            && k != "chars"
                            && !matches!(v, JValue::Undefined)
                    })
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

/// A queued document's job, written before the record that says so; the name's earlier job to put back
/// should the save fail.
fn queue_job(
    dir: &Path,
    project: &str,
    name: &str,
    file: &JValue,
    now: i64,
) -> Result<Option<extract_queue::Job>, Reply> {
    let earlier = extract_queue::pending(dir, project, name);
    let d = file.get("document");
    if let Err(e) = extract_queue::enqueue(
        dir,
        project,
        name,
        d.get("byteHash").as_str().unwrap_or(""),
        d.get("version").as_str().unwrap_or(""),
        d.get("error").as_str(),
        now,
    ) {
        eprintln!("noevia-server: extraction queue write failed: {e}");
        document_ingest::prune_now(dir, project);
        return Err(Reply::error(503, QUEUE_FAILED));
    }
    Ok(earlier)
}

fn restore_job(dir: &Path, project: &str, name: &str, earlier: Option<extract_queue::Job>) {
    let put = match earlier {
        Some(j) => extract_queue::put_back(dir, &j),
        None => extract_queue::clear(dir, project, name),
    };
    if let Err(e) = put {
        eprintln!("noevia-server: extraction queue restore failed: {e}");
    }
}

/// `withSourceLock`'s end: the stored originals and images no source points at go (#1914).
fn prune(dir: &Path, ctx: Ctx<'_>, project: &str) {
    let r = store::update(dir, ctx.switch, |projects| {
        let Some(p) = store::find_mut(projects, project) else {
            return ((), false);
        };
        crate::files::prune_documents(dir, p);
        crate::files::prune_uploads(dir, p, ctx.now_ms);
        ((), true)
    });
    if let Err(e) = r {
        eprintln!("noevia-server: an upload's cleanup could not be saved: {e:?}");
    }
}

fn organized(env: &Env<'_>, dir: &Path, id: &str, body: &JValue, input_name_raw: &str) -> Reply {
    let ctx = env.ctx;
    let mut input_name = input_name_raw.to_owned();
    let data = if body.get("dataBase64").truthy() {
        crate::files::js_string(body.get("dataBase64"))
    } else {
        String::new()
    };
    let input_bytes = js::base64_loose(&data);
    // #1805: a file of a folder import carries its path inside the folder and the import's id. Only a
    // path that import's plan accepted is taken (the caps hold here, not only in the browser).
    let importing = !matches!(body.get("relativePath"), JValue::Undefined);
    let (mut folder_path, mut import_rel) = (String::new(), String::new());
    if importing {
        import_rel = match folder_import::relative_path(body.get("relativePath")) {
            Ok(r) => r,
            Err(e) => return reply::error_reply(400, &e.message, Some("invalidPath")),
        };
        if let Some((code, _)) = folder_import::skip_reason(&import_rel) {
            return reply::error_reply(400, folder_import::reason(code), Some(code));
        }
        if let Some((status, code, error)) = folder_import::admit(
            body.get("importId"),
            ctx.user_id,
            id,
            &import_rel,
            input_bytes.len(),
            ctx.now_ms,
        ) {
            return reply::error_reply(status, &error, Some(code));
        }
        let slash = import_rel.rfind('/');
        input_name = import_rel
            .get(slash.map_or(0, |i| i + 1)..)
            .unwrap_or("")
            .to_owned();
        folder_path = slash
            .filter(|i| *i > 0)
            .and_then(|i| import_rel.get(..i))
            .unwrap_or("")
            .to_owned();
    }
    // Validate the filename before contacting the document processor.
    if let Err(f) = uploads::validate(
        &input_name,
        input_bytes.len().min(1) as u64,
        input_bytes.get(..1).unwrap_or(&[]),
    ) {
        return fail_reply(f);
    }
    let lock = project_documents::source_lock(ctx.user_id, id);
    let _held = match lock.lock() {
        Ok(g) => g,
        Err(p) => p.into_inner(),
    };
    let reply = organized_locked(
        env,
        dir,
        id,
        body,
        &input_name,
        &input_bytes,
        &folder_path,
        importing.then_some(import_rel.as_str()),
    );
    prune(dir, ctx, id);
    reply
}

#[allow(clippy::too_many_arguments)]
fn organized_locked(
    env: &Env<'_>,
    dir: &Path,
    id: &str,
    body: &JValue,
    input_name: &str,
    input_bytes: &[u8],
    folder_path: &str,
    import_rel: Option<&str>,
) -> Reply {
    let ctx = env.ctx;
    let (_, project) = match crate::originals::project(ctx, id) {
        Ok(v) => v,
        Err(r) => return r,
    };
    let Some(project) = project else {
        return reply::error_reply(409, "Project changed; retry.", None);
    };
    let had_uploads = reply::has_uploads(&project);
    let remote = env.storage.remote();
    if let (Storage::Refused(status, message), true) = (env.storage, remote.is_none()) {
        return reply::error_reply(*status, message, None);
    }
    let progress = env.progress;
    let importing = import_rel.is_some();
    // A folder import updates the file at its path: re-importing a folder never makes a "(2)" copy.
    let replace = body.get("replace").as_bool() == Some(true) || importing;
    let project_folder = project
        .get("projectFolder")
        .as_str()
        .unwrap_or("")
        .to_owned();
    let files = files_of(&project);
    // #1454: refuse before any work when the name is taken; checked again for a reduced file's new name.
    let destination_of = |n: &str| -> Option<String> {
        match (remote.is_some(), project_folder.is_empty()) {
            (true, false) => Some(uploads::destination_for(
                &project_folder,
                n,
                true,
                None,
                folder_path,
            )),
            (true, true) => None,
            (false, _) => Some(uploads::destination_for("", n, false, None, folder_path)),
        }
    };
    let exists = |n: &str| {
        destination_of(n).is_some_and(|d| {
            files
                .iter()
                .any(|f| f.get("name").as_str() == Some(d.as_str()))
        })
    };
    if !replace && exists(input_name) {
        return reply::source_exists(input_name);
    }
    if let Some(rel) = import_rel {
        if let Some(dest) = destination_of(input_name).filter(|d| !d.is_empty()) {
            let fold = folder_import::fold_key(&dest);
            if let Some(clash) = files.iter().find(|f| {
                f.get("name")
                    .as_str()
                    .is_some_and(|n| n != dest && folder_import::fold_key(n) == fold)
            }) {
                let body = JValue::obj([
                    ("error", s(folder_import::reason("caseCollision"))),
                    ("code", s("caseCollision")),
                    ("existing", clash.get("name").clone()),
                ]);
                return reply::ok(409, &body);
            }
            if let Some(previous) = files
                .iter()
                .find(|f| f.get("name").as_str() == Some(dest.as_str()))
            {
                let same = previous
                    .get("attachment")
                    .get("id")
                    .as_str()
                    .is_some_and(|a| !a.is_empty() && a == uploads::sha_hex(input_bytes));
                if same {
                    folder_import::consume(body.get("importId"), rel, input_bytes.len());
                    let mut fields = vec![
                        ("name", s(input_name)),
                        ("path", previous.get("name").clone()),
                        ("bytes", JValue::Num(input_bytes.len() as f64)),
                        ("unchanged", JValue::Bool(true)),
                    ];
                    if previous.get("attachment").truthy() {
                        fields.push(("attachment", previous.get("attachment").clone()));
                    }
                    if previous.get("document").truthy() {
                        fields.push(("document", previous.get("document").clone()));
                    }
                    return reply::ok(
                        200,
                        &JValue::Obj(fields.into_iter().map(|(k, v)| (k.to_owned(), v)).collect()),
                    );
                }
            }
        }
    }
    let reduced = match prepare(env.engine, input_name, input_bytes, progress) {
        Ok(r) => r,
        Err(f) => return fail_reply(f),
    };
    let (name, bytes, reduction) = (reduced.name, reduced.bytes, reduced.reduction);
    if let Err(f) = uploads::validate(
        &name,
        bytes.len() as u64,
        bytes.get(..upload_sniff::SNIFF_BYTES).unwrap_or(&bytes),
    ) {
        return fail_reply(f);
    }
    if !replace && name != input_name && exists(&name) {
        return reply::source_exists(&name);
    }
    let mut folder = project_folder.clone();
    if let (Some(storage), true) = (remote, project_folder.is_empty()) {
        progress("Creating upload folder");
        match crate::source_storage::allocate_folder(
            storage,
            env.project_root,
            id,
            project.get("reservedFolder").as_str(),
        ) {
            Ok(f) => {
                // Core held it on the project it saved later; a retry after a refused write must find it.
                let held = f.clone();
                let _ = store::update(dir, ctx.switch, |projects| {
                    let Some(p) = store::find_mut(projects, id) else {
                        return ((), false);
                    };
                    if p.get("projectFolder").as_str().is_none_or(str::is_empty) {
                        store::set(p, "projectFolder", s(&held));
                        return ((), true);
                    }
                    ((), false)
                });
                folder = f;
            }
            Err(e) => {
                eprintln!(
                    "[projects] could not create folder for project {id}: {}",
                    e.message
                );
                return reply::failure_reply(e.failure()).unwrap_or_else(|| {
                    reply::error_reply(502, "Could not create the storage folder; retry.", None)
                });
            }
        }
    }
    let ingest_env = IngestEnv {
        dir,
        project: id,
        project_folder: &folder,
        files: &files,
        assets_held: match project.get("assets") {
            JValue::Arr(a) => a.len(),
            _ => 0,
        },
        engine: env.engine,
        limits: env.limits,
        storage: remote,
        progress,
    };
    // #1664: filled by ingest only after a create-only write; any failure before the record is saved
    // removes that file again (conditional on its ETag), so a retry is not a 409 / "(2)".
    let opts = Options {
        connection: remote.is_some(),
        folder_path,
        condition: (remote.is_some() && !replace).then_some(Condition::IfNoneMatch),
        ..Default::default()
    };
    let mut ingested = match uploads::ingest(&ingest_env, &name, &bytes, &opts) {
        Ok(i) => i,
        Err(Fail::Storage(e)) => {
            if !replace && remote.is_some() && e.path_taken() {
                return reply::source_exists(&name);
            }
            if let Some(r) = reply::failure_reply(e.failure()) {
                return r;
            }
            return fail_reply(Fail::Storage(e));
        }
        Err(other) => return fail_reply(other),
    };
    let created = ingested.created.clone();
    let mut recorded = false;
    let result = (|| -> Reply {
        if let Some(r) = &reduction {
            let attachment = ingested.file.get("attachment").clone();
            let joined = [
                attachment.get("reason").as_str().unwrap_or(""),
                r.get("note").as_str().unwrap_or(""),
            ]
            .iter()
            .filter(|p| !p.is_empty())
            .copied()
            .collect::<Vec<_>>()
            .join(" ");
            if let JValue::Obj(items) = &mut ingested.file {
                if let Some((_, JValue::Obj(ai))) =
                    items.iter_mut().find(|(k, _)| k == "attachment")
                {
                    wtf16::set(ai, "reduction", r.clone());
                    wtf16::set(ai, "reason", JValue::Str(joined));
                }
            }
        }
        progress("Indexing extracted text");
        let mut file = ingested.file.clone();
        if file.get("document").truthy() {
            document_ingest::mark_indexing(&mut file);
        }
        // The job is written before the record that says "queued".
        let file_name = file.get("name").as_str().unwrap_or("").to_owned();
        let earlier = if ingested.queued {
            match queue_job(dir, id, &file_name, &file, ctx.now_ms) {
                Ok(e) => Some(e),
                Err(r) => return r,
            }
        } else {
            None
        };
        let remote_here = remote.is_some();
        let folder_now = folder.clone();
        let saved = store::update(dir, ctx.switch, |projects| {
            let Some(p) = store::find_mut(projects, id) else {
                return (
                    Err(reply::error_reply(
                        409,
                        "Project removed during upload.",
                        None,
                    )),
                    false,
                );
            };
            if remote_here {
                let mut sources: Vec<String> = match p.get("sourceFolders") {
                    JValue::Arr(a) => a
                        .iter()
                        .filter_map(|x| x.as_str().map(str::to_owned))
                        .collect(),
                    _ => Vec::new(),
                };
                if !sources.contains(&folder_now) {
                    sources.push(folder_now.clone());
                }
                store::set(
                    p,
                    "sourceFolders",
                    JValue::Arr(sources.iter().map(|x| s(x)).collect()),
                );
                if p.get("projectFolder").as_str().is_none_or(str::is_empty) {
                    store::set(p, "projectFolder", s(&folder_now));
                }
            }
            let mut next: Vec<JValue> = files_of(p)
                .into_iter()
                .filter(|f| f.get("name").as_str() != Some(file_name.as_str()))
                .collect();
            next.push(file.clone());
            store::set(p, "files", JValue::Arr(next));
            uploads::apply_assets(p, &ingested, ctx.now_ms);
            store::set(p, "updatedAt", JValue::Num(ctx.now_ms as f64));
            reply::apply_project_docs_default(p, env.catalogue, had_uploads);
            if crate::lifecycle::externalize(dir, p).is_err() {
                return (Err(Reply::internal()), false);
            }
            if !crate::lifecycle::reconcile_all(std::slice::from_mut(p)) {
                return (Err(Reply::internal()), false);
            }
            (Ok(()), true)
        });
        let failed = match saved {
            Ok(Ok(())) => None,
            Ok(Err(r)) => Some(r),
            Err(e) => Some(chats::store_failure(&e)),
        };
        if let Some(r) = failed {
            if let Some(earlier) = earlier {
                restore_job(dir, id, &file_name, earlier);
            }
            return r;
        }
        recorded = true;
        if !ingested.queued && extract_queue::pending(dir, id, &file_name).is_some() {
            let _ = extract_queue::clear(dir, id, &file_name);
        }
        let job = outbox::Job {
            kind: "index",
            project: id,
            file: Some(file_name.as_str()),
            payload: None,
        };
        if let Err(e) = outbox::enqueue(dir, &[job], ctx.now_ms) {
            eprintln!("noevia-server: index outbox write failed for an upload: {e:?}");
        }
        if let Some(rel) = import_rel {
            folder_import::consume(body.get("importId"), rel, input_bytes.len());
        }
        let mut fields = vec![
            ("name", s(&name)),
            ("path", s(&file_name)),
            ("bytes", JValue::Num(bytes.len() as f64)),
        ];
        if file.get("document").truthy() {
            fields.push(("document", file.get("document").clone()));
        }
        fields.push(("attachment", file.get("attachment").clone()));
        reply::ok(
            200,
            &JValue::Obj(fields.into_iter().map(|(k, v)| (k.to_owned(), v)).collect()),
        )
    })();
    // The unsaved record goes with the file, so the retry is not refused as a same-name source.
    if !recorded {
        if let (Some(c), Some(storage)) = (&created, remote) {
            uploads::discard_created(storage, c);
        }
    }
    result
}

/// The older form: a text file or a document straight into the project's folder (or noevia).
fn legacy(
    env: &Env<'_>,
    dir: &Path,
    id: &str,
    body: &JValue,
    raw_name: &str,
    bytes: &[u8],
) -> Reply {
    let ctx = env.ctx;
    let lock = project_documents::source_lock(ctx.user_id, id);
    let _held = match lock.lock() {
        Ok(g) => g,
        Err(p) => p.into_inner(),
    };
    let reply = legacy_locked(env, dir, id, body, raw_name, bytes);
    prune(dir, ctx, id);
    reply
}

fn legacy_locked(
    env: &Env<'_>,
    dir: &Path,
    id: &str,
    body: &JValue,
    raw_name: &str,
    bytes: &[u8],
) -> Reply {
    let ctx = env.ctx;
    let (_, project) = match crate::originals::project(ctx, id) {
        Ok(v) => v,
        Err(r) => return r,
    };
    let Some(project) = project else {
        return reply::error_reply(409, "Project changed; retry.", None);
    };
    let had_uploads = reply::has_uploads(&project);
    let replace = body.get("replace").as_bool() == Some(true);
    let docling = env.engine.docling_configured();
    let is_doc = extract::is_document(raw_name, docling);
    let remote = env.storage.remote();
    let Some(storage) = remote else {
        if let Storage::Refused(status, message) = env.storage {
            return reply::error_reply(*status, message, None);
        }
        // Remote storage is optional. Keep local uploads as project sources, including document
        // extraction, for installations without a cloud account.
        let uploads_listed: Vec<JValue> = files_of(&project)
            .into_iter()
            .filter(|f| !f.get("source").truthy())
            .collect();
        if uploads_listed.len() >= 20
            && !uploads_listed
                .iter()
                .any(|f| f.get("name").as_str() == Some(raw_name))
        {
            return reply::error_reply(400, "A project holds at most 20 uploaded sources.", None);
        }
        let previous = uploads_listed
            .iter()
            .find(|f| f.get("name").as_str() == Some(raw_name))
            .cloned();
        if previous.is_some() && !replace {
            return reply::source_exists(raw_name);
        }
        let mut queued = false;
        let mut file = if is_doc {
            let prev = previous.as_ref().map(|p| reply::with_text(dir, id, p));
            match document_ingest::ingest(
                dir,
                id,
                raw_name,
                bytes,
                prev.as_ref(),
                env.engine,
                env.limits,
            ) {
                Ok(i) => {
                    queued = i.retryable || i.open;
                    let mut f = i.file;
                    if queued {
                        extract_queue::mark_queued(&mut f);
                    }
                    f
                }
                Err(_) => return Reply::internal(),
            }
        } else {
            JValue::obj([
                ("name", s(raw_name)),
                (
                    "content",
                    JValue::Str(wtf16::slice(&String::from_utf8_lossy(bytes), 200_000)),
                ),
            ])
        };
        if file.get("document").truthy() {
            document_ingest::mark_indexing(&mut file);
        }
        let earlier = if queued {
            match queue_job(dir, id, raw_name, &file, ctx.now_ms) {
                Ok(e) => Some(e),
                Err(r) => return r,
            }
        } else {
            None
        };
        let saved = store::update(dir, ctx.switch, |projects| {
            let Some(p) = store::find_mut(projects, id) else {
                return (
                    Err(reply::error_reply(409, "Source changed; retry.", None)),
                    false,
                );
            };
            let now = files_of(p)
                .into_iter()
                .find(|f| !f.get("source").truthy() && f.get("name").as_str() == Some(raw_name));
            if !same_source(now.as_ref(), previous.as_ref()) {
                return (
                    Err(reply::error_reply(409, "Source changed; retry.", None)),
                    false,
                );
            }
            let mut next: Vec<JValue> = files_of(p)
                .into_iter()
                .filter(|f| f.get("source").truthy() || f.get("name").as_str() != Some(raw_name))
                .collect();
            next.push(file.clone());
            store::set(p, "files", JValue::Arr(next));
            store::set(p, "updatedAt", JValue::Num(ctx.now_ms as f64));
            reply::apply_project_docs_default(p, env.catalogue, had_uploads);
            if crate::lifecycle::externalize(dir, p).is_err() {
                return (Err(Reply::internal()), false);
            }
            if !crate::lifecycle::reconcile_all(std::slice::from_mut(p)) {
                return (Err(Reply::internal()), false);
            }
            (Ok(()), true)
        });
        match saved {
            Ok(Ok(())) => {}
            Ok(Err(r)) => {
                if let Some(e) = earlier {
                    restore_job(dir, id, raw_name, e);
                }
                return r;
            }
            Err(e) => {
                if let Some(e2) = earlier {
                    restore_job(dir, id, raw_name, e2);
                }
                return chats::store_failure(&e);
            }
        }
        if !queued && extract_queue::pending(dir, id, raw_name).is_some() {
            let _ = extract_queue::clear(dir, id, raw_name);
        }
        let job = outbox::Job {
            kind: "index",
            project: id,
            file: Some(raw_name),
            payload: None,
        };
        if let Err(e) = outbox::enqueue(dir, &[job], ctx.now_ms) {
            eprintln!("noevia-server: index outbox write failed for an upload: {e:?}");
        }
        let mut fields = vec![
            ("name", s(raw_name)),
            ("path", s(raw_name)),
            ("bytes", JValue::Num(bytes.len() as f64)),
        ];
        if file.get("document").truthy() {
            fields.push(("document", file.get("document").clone()));
        }
        return reply::ok(
            200,
            &JValue::Obj(fields.into_iter().map(|(k, v)| (k.to_owned(), v)).collect()),
        );
    };
    // With storage: the file goes into the project's own folder; the sync that follows turns it into a
    // source, so there is one path from a file to a source however it arrived.
    let mut folder = project
        .get("projectFolder")
        .as_str()
        .unwrap_or("")
        .to_owned();
    if folder.is_empty() {
        match crate::source_storage::allocate_folder(
            storage,
            env.project_root,
            id,
            project.get("reservedFolder").as_str(),
        ) {
            Ok(f) => folder = f,
            Err(e) => {
                eprintln!(
                    "[projects] could not create folder for project {id}: {}",
                    e.message
                );
                return reply::failure_reply(e.failure()).unwrap_or_else(|| {
                    reply::error_reply(
                        502,
                        "Could not create the project storage folder. Check your storage connection and retry.",
                        None,
                    )
                });
            }
        }
    }
    let attach = |folder: &str| {
        store::update(dir, ctx.switch, |projects| {
            let Some(p) = store::find_mut(projects, id) else {
                return (false, false);
            };
            let mut sources: Vec<String> = match p.get("sourceFolders") {
                JValue::Arr(a) => a
                    .iter()
                    .filter_map(|x| x.as_str().map(str::to_owned))
                    .collect(),
                _ => Vec::new(),
            };
            if !sources.iter().any(|x| x == folder) {
                sources.push(folder.to_owned());
            }
            store::set(
                p,
                "sourceFolders",
                JValue::Arr(sources.iter().map(|x| s(x)).collect()),
            );
            if p.get("projectFolder").as_str().is_none_or(str::is_empty) {
                store::set(p, "projectFolder", s(folder));
            }
            (true, true)
        })
    };
    if let Err(e) = attach(&folder) {
        return chats::store_failure(&e);
    }
    let name = format!("{folder}/{raw_name}");
    if !replace
        && files_of(&project)
            .iter()
            .any(|f| f.get("name").as_str() == Some(name.as_str()))
    {
        return reply::source_exists(raw_name);
    }
    // #1653: a document is read BEFORE the write, so a failure while reading leaves nothing in storage.
    let mut file: Option<JValue> = None;
    let mut queued = false;
    if is_doc {
        let previous = files_of(&project)
            .into_iter()
            .find(|f| {
                f.get("name").as_str() == Some(name.as_str())
                    && f.get("source").as_str() == Some(folder.as_str())
            })
            .map(|p| reply::with_text(dir, id, &p));
        match document_ingest::ingest(
            dir,
            id,
            &name,
            bytes,
            previous.as_ref(),
            env.engine,
            env.limits,
        ) {
            Ok(i) => {
                queued = i.retryable || i.open;
                let mut f = i.file;
                if queued {
                    extract_queue::mark_queued(&mut f);
                }
                if let JValue::Obj(items) = &mut f {
                    wtf16::set(items, "source", s(&folder));
                }
                file = Some(f);
            }
            Err(_) => return Reply::internal(),
        }
    }
    let written = storage.write(
        &name,
        bytes,
        &if replace {
            Condition::None
        } else {
            Condition::IfNoneMatch
        },
    );
    let written = match written {
        Ok(w) => w,
        Err(e) => {
            if !replace && e.path_taken() {
                return reply::source_exists(raw_name);
            }
            if let Some(r) = reply::failure_reply(e.failure()) {
                return r;
            }
            let status = if e.status == 0 { 502 } else { e.status };
            let message = if e.message.is_empty() {
                "could not save the file"
            } else {
                &e.message
            };
            return reply::error_reply(status, message, None);
        }
    };
    if let Some(mut f) = file {
        // #1664: a document needs its record; if that fails after a create-only write, remove the file
        // again (If-Match on the write's ETag) so a retry is not a 409 / "(2)". A replace never deletes.
        document_ingest::mark_indexing(&mut f);
        let earlier = if queued {
            match queue_job(dir, id, &name, &f, ctx.now_ms) {
                Ok(e) => Some(e),
                Err(r) => {
                    discard_after(storage, replace, &name, &written);
                    return r;
                }
            }
        } else {
            None
        };
        let folder_now = folder.clone();
        let saved = store::update(dir, ctx.switch, |projects| {
            let Some(p) = store::find_mut(projects, id) else {
                return (
                    Err(reply::error_reply(
                        409,
                        "Source changed; refresh again.",
                        None,
                    )),
                    false,
                );
            };
            let attached = match p.get("sourceFolders") {
                JValue::Arr(a) => a.iter().any(|x| x.as_str() == Some(folder_now.as_str())),
                _ => false,
            };
            if !attached {
                return (
                    Err(reply::error_reply(
                        409,
                        "Source changed; refresh again.",
                        None,
                    )),
                    false,
                );
            }
            let mut next: Vec<JValue> = files_of(p)
                .into_iter()
                .filter(|x| {
                    !(x.get("name").as_str() == Some(name.as_str())
                        && x.get("source").as_str() == Some(folder_now.as_str()))
                })
                .collect();
            next.push(f.clone());
            store::set(p, "files", JValue::Arr(next));
            reply::apply_project_docs_default(p, env.catalogue, had_uploads);
            if crate::lifecycle::externalize(dir, p).is_err() {
                return (Err(Reply::internal()), false);
            }
            if !crate::lifecycle::reconcile_all(std::slice::from_mut(p)) {
                return (Err(Reply::internal()), false);
            }
            (Ok(()), true)
        });
        let failed = match saved {
            Ok(Ok(())) => None,
            Ok(Err(r)) => Some(r),
            Err(e) => Some(chats::store_failure(&e)),
        };
        if let Some(r) = failed {
            if let Some(e) = earlier {
                restore_job(dir, id, &name, e);
            }
            discard_after(storage, replace, &name, &written);
            return r;
        }
        if !queued && extract_queue::pending(dir, id, &name).is_some() {
            let _ = extract_queue::clear(dir, id, &name);
        }
        let job = outbox::Job {
            kind: "index",
            project: id,
            file: Some(name.as_str()),
            payload: None,
        };
        if let Err(e) = outbox::enqueue(dir, &[job], ctx.now_ms) {
            eprintln!("noevia-server: index outbox write failed for an upload: {e:?}");
        }
        file = Some(f);
    }
    let mut fields = vec![
        ("name", s(raw_name)),
        ("path", s(&name)),
        ("bytes", JValue::Num(bytes.len() as f64)),
    ];
    if let Some(f) = &file {
        if f.get("document").truthy() {
            fields.push(("document", f.get("document").clone()));
        }
    }
    reply::ok(
        200,
        &JValue::Obj(fields.into_iter().map(|(k, v)| (k.to_owned(), v)).collect()),
    )
}

fn discard_after(
    storage: &dyn crate::source_storage::SourceStorage,
    replace: bool,
    name: &str,
    w: &crate::source_storage::Written,
) {
    if !replace && w.created && !w.etag.is_empty() {
        uploads::discard_created(
            storage,
            &uploads::Created {
                path: name.to_owned(),
                etag: w.etag.clone(),
            },
        );
    }
}

/// POST /api/projects/{id}/folder-import/plan (#1805): decides which files of a folder will be uploaded,
/// which are unchanged, which are skipped and why, and opens an import id that only the accepted paths
/// can be uploaded under. Nothing is written. `remote` is whether the account's storage is browsable.
pub fn plan(ctx: Ctx<'_>, raw_id: &str, body: &[u8], over: bool, remote: bool) -> Reply {
    let Some(id) = js::decode_uri_component(raw_id) else {
        return Reply::internal();
    };
    let (_, project) = match crate::originals::project(ctx, &id) {
        Ok(v) => v,
        Err(r) => return r,
    };
    let Some(project) = project else {
        return Reply::error(404, "no such project");
    };
    // Never the Diary or a free chat's attachment context: those are not projects a person imports into.
    if reply::internal_project(&project) {
        return Reply::error(400, "Folders can be imported into a project only.");
    }
    if over {
        return Reply::error(413, "That folder lists too many files to import at once.");
    }
    let text = String::from_utf8_lossy(body);
    let parsed = if text.is_empty() {
        JValue::Obj(Vec::new())
    } else {
        match wtf16::parse(&text) {
            Ok(v) => v,
            Err(_) => return chats::invalid_json(),
        }
    };
    if !parsed.is_object() {
        return Reply::error(400, "request body must be a JSON object");
    }
    let folder = project
        .get("projectFolder")
        .as_str()
        .unwrap_or("")
        .to_owned();
    let destination = |rel: &str| -> String {
        let slash = rel.rfind('/');
        let name = rel.get(slash.map_or(0, |i| i + 1)..).unwrap_or("");
        let folder_path = slash
            .filter(|i| *i > 0)
            .and_then(|i| rel.get(..i))
            .unwrap_or("");
        if remote && folder.is_empty() {
            return String::new();
        }
        uploads::destination_for(&folder, name, remote, None, folder_path)
    };
    let cap = |rel: &str| -> f64 {
        if rel.to_lowercase().ends_with(".pdf") {
            INPUT_CAP as f64
        } else {
            uploads::CAP as f64
        }
    };
    match folder_import::plan(&folder_import::PlanInput {
        user: ctx.user_id,
        project: &project,
        entries: parsed.get("entries"),
        destination: &destination,
        per_file_cap: &cap,
        now: ctx.now_ms,
    }) {
        Ok(v) => reply::ok(200, &v),
        Err(e) if e.status == 400 || e.status == 429 => Reply::error(e.status, &e.message),
        Err(_) => Reply::internal(),
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
    use crate::extract::tests::{Fake as FakeEngine, Wrap};
    use crate::source_storage::fake::Fake as FakeStorage;
    use crate::{outbox, Body};

    const USER: &str = "11111111-2222-4333-8444-555555555555";
    const LIMITS: Limits = Limits {
        read_bytes: 96 * 1024 * 1024,
        pages: 1000,
        text: 8_000_000,
        range_pages: 25,
    };
    const PROJECT: &str = r#"{"id":"proj-a","name":"A","chats":[],"instructionSkills":{},"projectFolder":"root/proj-a"}"#;

    struct Rig {
        root: tempfile::TempDir,
        engine: FakeEngine,
        storage: FakeStorage,
    }
    impl Rig {
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
                engine: FakeEngine::default(),
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
        fn post_with(
            &self,
            engine: &dyn Engine,
            remote: bool,
            body: &str,
            progress: &dyn Fn(&str),
        ) -> (u16, JValue) {
            let storage = if remote {
                Storage::Remote(&self.storage)
            } else {
                Storage::None
            };
            let env = Env {
                ctx: self.ctx(),
                storage: &storage,
                engine,
                limits: &LIMITS,
                catalogue: None,
                project_root: "root",
                progress,
            };
            let r = post(&env, "proj-a", body.as_bytes(), false);
            let Body::Json(text) = &r.body else { panic!() };
            (r.status, wtf16::parse(text).unwrap())
        }
        fn post(&self, remote: bool, body: &str) -> (u16, JValue) {
            self.post_with(&self.engine, remote, body, &|_| {})
        }
        fn files(&self) -> Vec<JValue> {
            let list = store::read(&self.dir()).unwrap();
            files_of(store::find(&list, "proj-a").unwrap())
        }
    }

    fn b64(bytes: &[u8]) -> String {
        const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let mut out = String::new();
        for c in bytes.chunks(3) {
            let n = (u32::from(c[0]) << 16)
                | (u32::from(*c.get(1).unwrap_or(&0)) << 8)
                | u32::from(*c.get(2).unwrap_or(&0));
            for i in 0..4 {
                if i <= c.len() {
                    out.push(T[((n >> (18 - 6 * i)) & 63) as usize] as char);
                } else {
                    out.push('=');
                }
            }
        }
        out
    }

    fn organized(name: &str, bytes: &[u8], extra: &str) -> String {
        format!(
            r#"{{"name":"{name}","organized":true,"dataBase64":"{}"{extra}}}"#,
            b64(bytes)
        )
    }

    #[test]
    fn an_organized_upload_lands_in_its_type_group_create_only_and_never_replaces_unasked() {
        let rig = Rig::new(PROJECT);
        let stages = std::cell::RefCell::new(Vec::new());
        let progress = |s: &str| stages.borrow_mut().push(s.to_owned());
        let (status, body) = rig.post_with(
            &rig.engine,
            true,
            &organized("notes.md", b"# one", ""),
            &progress,
        );
        assert_eq!(status, 200, "{body:?}");
        assert_eq!(body.get("name").as_str(), Some("notes.md"));
        assert_eq!(body.get("path").as_str(), Some("root/proj-a/Text/notes.md"));
        assert_eq!(body.get("bytes").as_num(), Some(5.0));
        assert_eq!(body.get("attachment").get("state").as_str(), Some("ready"));
        assert_eq!(
            *stages.borrow(),
            vec![
                "Saving original",
                "Saving to Nextcloud / storage",
                "Indexing extracted text"
            ]
        );
        assert_eq!(
            rig.storage
                .files
                .borrow()
                .get("root/proj-a/Text/notes.md")
                .unwrap()
                .0,
            b"# one"
        );
        let files = rig.files();
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].get("source").as_str(), Some("root/proj-a"));
        assert_eq!(
            crate::sources::text(&rig.dir(), "proj-a", &files[0]).unwrap(),
            "# one"
        );
        // The project's folder is attached as a source.
        let list = store::read(&rig.dir()).unwrap();
        let p = store::find(&list, "proj-a").unwrap();
        assert_eq!(p.get("sourceFolders").get("0").as_str(), None);
        assert!(matches!(p.get("sourceFolders"), JValue::Arr(a) if a.len() == 1));
        let jobs = outbox::jobs(&rig.dir()).unwrap();
        assert!(jobs
            .iter()
            .any(|j| j.kind == "index" && j.file.as_deref() == Some("root/proj-a/Text/notes.md")));
        // The same name again is refused, with its code, before any write.
        let before = rig.storage.log().len();
        let (status, body) = rig.post(true, &organized("notes.md", b"# two", ""));
        assert_eq!(
            (status, body.get("code").as_str()),
            (409, Some("sourceExists"))
        );
        assert_eq!(rig.storage.log().len(), before);
        // `replace: true` overwrites: a plain PUT, not create-only.
        let (status, _) = rig.post(true, &organized("notes.md", b"# two", r#","replace":true"#));
        assert_eq!(status, 200);
        assert_eq!(
            rig.storage
                .files
                .borrow()
                .get("root/proj-a/Text/notes.md")
                .unwrap()
                .0,
            b"# two"
        );
        assert_eq!(
            rig.files().len(),
            1,
            "the record was replaced, not duplicated"
        );
    }

    #[test]
    fn a_record_that_cannot_be_saved_takes_the_file_it_just_made_out_of_storage_again() {
        let rig = Rig::new(PROJECT);
        // The project is deleted while the upload is between its write and its save.
        let dir = rig.dir();
        let progress = |stage: &str| {
            if stage == "Indexing extracted text" {
                std::fs::write(dir.join(store::PROJECTS_FILE), r#"{"projects":[]}"#).unwrap();
            }
        };
        let (status, body) =
            rig.post_with(&rig.engine, true, &organized("a.md", b"x", ""), &progress);
        assert_eq!(status, 409, "{body:?}");
        assert_eq!(
            body.get("error").as_str(),
            Some("Project removed during upload.")
        );
        assert!(rig.storage.log().iter().any(|l| l.starts_with("DISCARD ")));
        assert!(
            !rig.storage
                .files
                .borrow()
                .contains_key("root/proj-a/Text/a.md"),
            "no unrecorded file is left for the retry to meet as a 409"
        );
    }

    #[test]
    fn a_folder_import_uploads_only_what_its_plan_accepted_under_its_ticket() {
        crate::folder_import::reset();
        let rig = Rig::new(PROJECT);
        let plan = |entries: &str| -> JValue {
            let r = plan(
                rig.ctx(),
                "proj-a",
                format!(r#"{{"entries":{entries}}}"#).as_bytes(),
                false,
                true,
            );
            assert_eq!(r.status, 200, "{r:?}");
            let Body::Json(t) = &r.body else { panic!() };
            wtf16::parse(t).unwrap()
        };
        let planned =
            plan(r#"[{"path":"docs/api/a.md","size":5},{"path":"node_modules/x.js","size":3}]"#);
        let id = planned.get("importId").as_str().unwrap().to_owned();
        assert_eq!(planned.get("accepted").get("0").as_str(), None);
        let upload = |rel: &str, bytes: &[u8]| {
            rig.post(
                true,
                &organized(
                    "ignored.md",
                    bytes,
                    &format!(r#","relativePath":"{rel}","importId":"{id}""#),
                ),
            )
        };
        // Not in the plan, or skipped by it: 409 with its code.
        let (status, body) = upload("docs/other.md", b"12345");
        assert_eq!(
            (status, body.get("code").as_str()),
            (409, Some("importNotPlanned"))
        );
        let (status, body) = upload("node_modules/x.js", b"123");
        assert_eq!((status, body.get("code").as_str()), (400, Some("system")));
        // Larger than the plan declared: refused, and the path stays open.
        let (status, body) = upload("docs/api/a.md", b"123456");
        assert_eq!(
            (status, body.get("code").as_str()),
            (413, Some("sizeMismatch"))
        );
        // Planned: stored under its path inside the type group, subfolders made outermost first.
        let (status, body) = upload("docs/api/a.md", b"12345");
        assert_eq!(status, 200, "{body:?}");
        assert_eq!(
            body.get("path").as_str(),
            Some("root/proj-a/Text/docs/api/a.md")
        );
        assert!(rig
            .storage
            .log()
            .contains(&"MKCOL root/proj-a/Text/docs/api".to_owned()));
        // A second time the path is done.
        let (status, body) = upload("docs/api/a.md", b"12345");
        assert_eq!(
            (status, body.get("code").as_str()),
            (409, Some("importNotPlanned"))
        );
        // Re-importing the same bytes: the plan sees it unchanged and a stray upload changes nothing.
        let hash = uploads::sha_hex(b"12345");
        let again = plan(&format!(
            r#"[{{"path":"docs/api/a.md","size":5,"sha256":"{hash}"}}]"#
        ));
        assert_eq!(again.get("unchanged").get("0").as_str(), None);
        assert!(matches!(again.get("unchanged"), JValue::Arr(a) if a.len() == 1));
        let id2 = again.get("importId").as_str().unwrap().to_owned();
        let writes = rig
            .storage
            .log()
            .iter()
            .filter(|l| l.starts_with("PUT"))
            .count();
        let (status, body) = rig.post(
            true,
            &organized(
                "a.md",
                b"12345",
                &format!(r#","relativePath":"docs/api/a.md","importId":"{id2}""#),
            ),
        );
        assert_eq!(status, 409, "{body:?}");
        let _ = writes;
        // A changed file is replaced in place: no "(2)" copy.
        let changed = plan(r#"[{"path":"docs/api/a.md","size":6}]"#);
        let id3 = changed.get("importId").as_str().unwrap().to_owned();
        let (status, body) = rig.post(
            true,
            &organized(
                "a.md",
                b"123456",
                &format!(r#","relativePath":"docs/api/a.md","importId":"{id3}""#),
            ),
        );
        assert_eq!(status, 200, "{body:?}");
        assert_eq!(rig.files().len(), 1);
        assert_eq!(
            rig.storage
                .files
                .borrow()
                .get("root/proj-a/Text/docs/api/a.md")
                .unwrap()
                .0,
            b"123456"
        );
    }

    #[test]
    fn a_pdf_over_25_mb_is_reduced_by_the_ocr_worker_and_a_docx_is_read_by_it() {
        let rig = Rig::new(PROJECT);
        let big: Vec<u8> = b"%PDF-1.4 "
            .iter()
            .copied()
            .chain(std::iter::repeat_n(b' ', 26 * 1024 * 1024))
            .collect();
        let wrap = Wrap::new(&rig.engine);
        *wrap.reduce.borrow_mut() = Some(Ok(wtf16::parse(
            r#"{"kind":"text","text":"only the text"}"#,
        )
        .unwrap()));
        let (status, body) = rig.post_with(&wrap, true, &organized("big.pdf", &big, ""), &|_| {});
        assert_eq!(status, 200, "{body:?}");
        assert_eq!(body.get("name").as_str(), Some("big.extracted.txt"));
        let reduction = body.get("attachment").get("reduction");
        assert_eq!(reduction.get("originalName").as_str(), Some("big.pdf"));
        assert_eq!(
            reduction.get("originalBytes").as_num(),
            Some(big.len() as f64)
        );
        assert_eq!(reduction.get("kind").as_str(), Some("text"));
        assert!(body
            .get("attachment")
            .get("reason")
            .as_str()
            .unwrap()
            .contains("Text-only extraction"));
        assert_eq!(
            rig.storage
                .files
                .borrow()
                .get("root/proj-a/Text/big.extracted.txt")
                .unwrap()
                .0,
            b"only the text"
        );
        // Without an OCR worker: 503 (a 5xx is `Internal error` to the person, as errorResponse words it).
        let (status, body) = rig.post(true, &organized("big2.pdf", &big, ""));
        assert_eq!(
            (status, body.get("error").as_str()),
            (503, Some("Internal error"))
        );
        // Over 60 MB, or not a PDF: refused with the words.
        let huge = vec![b'x'; 61 * 1024 * 1024];
        let (status, body) = rig.post(true, &organized("huge.pdf", &huge, ""));
        assert_eq!(status, 413);
        assert_eq!(
            body.get("error").as_str(),
            Some("Files are limited to 25 MB; PDFs up to 60 MB can be reduced automatically.")
        );
        // A DOCX goes through the worker's reader and keeps its scope note.
        *wrap.docx.borrow_mut() = Some(Ok(wtf16::parse(
            r#"{"text":"Body text","truncated":false}"#,
        )
        .unwrap()));
        let (status, body) = rig.post_with(
            &wrap,
            true,
            &organized("body.docx", b"PK\x03\x04docx", ""),
            &|_| {},
        );
        assert_eq!(status, 200, "{body:?}");
        let a = body.get("attachment");
        assert_eq!(
            (a.get("state").as_str(), a.get("reasonId").as_str()),
            (Some("partial"), Some("docxPartial"))
        );
        assert_eq!(a.get("readerVersion").as_str(), Some("docx-main-body-v1"));
    }

    #[test]
    fn a_document_whose_docling_is_down_is_queued_with_its_job_written_first() {
        let rig = Rig::new(PROJECT);
        let engine = FakeEngine {
            docling: true,
            ..Default::default()
        };
        let (status, body) = rig.post_with(
            &engine,
            true,
            &organized("r.pdf", b"%PDF-1.4 synthetic", ""),
            &|_| {},
        );
        assert_eq!(status, 200, "{body:?}");
        assert_eq!(
            body.get("document").get("extraction").as_str(),
            Some("queued")
        );
        let job = extract_queue::pending(&rig.dir(), "proj-a", "root/proj-a/Documents/r.pdf")
            .expect("job");
        assert_eq!(
            job.byte_hash,
            body.get("document").get("byteHash").as_str().unwrap()
        );
        // A queue that cannot be written saves nothing and removes the file again (503).
        let rig = Rig::new(PROJECT);
        std::fs::create_dir(extract_queue::path(&rig.dir())).unwrap();
        let (status, body) = rig.post_with(
            &engine,
            true,
            &organized("r.pdf", b"%PDF-1.4 synthetic", ""),
            &|_| {},
        );
        assert_eq!(status, 503, "{body:?}");
        assert_eq!(body.get("error").as_str(), Some(QUEUE_FAILED));
        assert!(rig.files().is_empty());
        assert!(
            !rig.storage
                .files
                .borrow()
                .contains_key("root/proj-a/Documents/r.pdf"),
            "the file went with the failure"
        );
    }

    #[test]
    fn the_older_upload_keeps_text_and_documents_in_noevia_without_storage_and_in_the_folder_with_it(
    ) {
        let rig = Rig::new(r#"{"id":"proj-a","name":"A","chats":[],"instructionSkills":{}}"#);
        let plain = |name: &str, bytes: &[u8], extra: &str| {
            format!(
                r#"{{"name":"{name}","dataBase64":"{}"{extra}}}"#,
                b64(bytes)
            )
        };
        let (status, body) = rig.post(false, &plain("notes.txt", b"hello", ""));
        assert_eq!(status, 200, "{body:?}");
        assert_eq!(body.get("path").as_str(), Some("notes.txt"));
        let (status, body) = rig.post(false, &plain("notes.txt", b"again", ""));
        assert_eq!(
            (status, body.get("code").as_str()),
            (409, Some("sourceExists"))
        );
        let (status, _) = rig.post(false, &plain("notes.txt", b"again", r#","replace":true"#));
        assert_eq!(status, 200);
        assert_eq!(
            crate::sources::text(&rig.dir(), "proj-a", &rig.files()[0]).unwrap(),
            "again"
        );
        // What it refuses.
        let (status, body) = rig.post(false, &plain("photo.png", b"x", ""));
        assert_eq!(
            (status, body.get("error").as_str()),
            (
                400,
                Some("photo.png is not a supported source (text file or PDF).")
            )
        );
        let (status, body) = rig.post(false, r#"{"name":"empty.txt","dataBase64":""}"#);
        assert_eq!(
            (status, body.get("error").as_str()),
            (400, Some("file was empty"))
        );
        let (status, body) = rig.post(false, r#"{"name":""}"#);
        assert_eq!(
            (status, body.get("error").as_str()),
            (400, Some("a filename is required"))
        );
        // With storage the file goes into the project's own folder (create-only); the sync makes it a source.
        let rig = Rig::new(PROJECT);
        let (status, body) = rig.post(true, &plain("notes.txt", b"hello", ""));
        assert_eq!(status, 200, "{body:?}");
        assert_eq!(body.get("path").as_str(), Some("root/proj-a/notes.txt"));
        assert_eq!(
            rig.storage
                .files
                .borrow()
                .get("root/proj-a/notes.txt")
                .unwrap()
                .0,
            b"hello"
        );
        let (status, body) = rig.post(true, &plain("notes.txt", b"hello", ""));
        assert_eq!(
            (status, body.get("code").as_str()),
            (409, Some("sourceExists"))
        );
    }

    #[test]
    fn what_is_checked_before_the_upload_starts() {
        let rig = Rig::new(PROJECT);
        let ctx = rig.ctx();
        assert_eq!(
            precheck(ctx, "ghost", b"{}", false).unwrap_err(),
            Reply::error(404, "project not found")
        );
        assert_eq!(
            precheck(ctx, "proj-a", b"{}", true).unwrap_err(),
            Reply::error(413, "Request exceeds size limit")
        );
        assert_eq!(
            precheck(ctx, "proj-a", b"{nope", false).unwrap_err(),
            Reply::error(400, "invalid JSON")
        );
        assert!(
            precheck(ctx, "proj-a", b"", false).is_ok(),
            "an empty body reads as an empty object"
        );
        assert_eq!(
            precheck(ctx, "%E0", b"{}", false).unwrap_err(),
            Reply::internal()
        );
        // The upload's own order: 404 `no such project`, then the cap's 413, then invalid JSON.
        let none = Storage::None;
        let engine = FakeEngine::default();
        let env = Env {
            ctx,
            storage: &none,
            engine: &engine,
            limits: &LIMITS,
            catalogue: None,
            project_root: "root",
            progress: &|_| {},
        };
        assert_eq!(
            post(&env, "ghost", b"{}", false),
            Reply::error(404, "no such project")
        );
        assert_eq!(
            post(&env, "proj-a", b"{}", true),
            Reply::error(413, "That file is larger than the 25 MB limit.")
        );
        assert_eq!(
            post(&env, "proj-a", b"{nope", false),
            Reply::error(400, "invalid JSON")
        );
        assert_eq!(post(&env, "proj-a", b"null", false), Reply::internal());
    }
}
