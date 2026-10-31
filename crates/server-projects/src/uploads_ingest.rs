//! core uploads.cjs `ingest` (sbstndalton/noevia#2191 round 8): one file into a project, from an
//! organized upload (the file's bytes in the request) or a managed type group of the project's own
//! storage folder (a sync's read).
//!
//! In core's order: the filename/size/archive rules (upload-sniff), the project limits, the original
//! saved under `project-uploads/<sha256 id>/<sha256 bytes>`, then by kind
//!
//! - a Docling document (a PDF, and with Docling every format it reads): the Docling pipeline
//!   ([`crate::document_ingest`]); a document whose worker is down is queued ([`Ingested::queued`]), not
//!   read in this process;
//! - a DOCX: the OCR worker's `/extract-docx` (docx.cjs), the text with its scope note, reused while the
//!   stored original and reader version are the same;
//! - a Text file: strictly decoded (upload-sniff `decode_text`), up to 200 000 UTF-16 units;
//! - an image (8 MiB at most): state `vision`, and an image asset named by its source;
//! - anything else: `stored`.
//!
//! The remote write comes last among the steps that can fail (#1653): the type group's folder (and a
//! folder import's subfolders) made, then the file written (create-only, conditional, or plain), and a
//! create-only write that made a file records it for [`discard_created`] (#1664).
//!
//! A vision image's asset is returned to the caller ([`Ingested::asset`]) to be applied to the project
//! list under its lock ([`apply_assets`]): core mutated the project object it held; this crate re-reads
//! the list at the save, so nothing here writes projects.json.
use crate::document_ingest::{self, mark_indexing};
use crate::extract::{self, Engine, Limits};
use crate::extract_queue;
use crate::project_storage::save_original;
use crate::source_limits;
use crate::source_storage::{Condition, SourceStorage, StorageError};
use crate::{assets, lifecycle, store};
use js_json::{wtf16, JValue};
use sha2::{Digest, Sha256};
use std::path::Path;

/// uploads.cjs GROUPS.
pub const GROUPS: [&str; 4] = ["Documents", "Images", "Text", "Other"];
/// uploads.cjs CAP.
pub const CAP: usize = upload_sniff::CAP as usize;
const IMAGE_MIB: usize = 8 * 1024 * 1024;
const TEXT_UNITS: usize = 200_000;
/// docx.cjs VERSION.
pub const DOCX_VERSION: &str = "docx-main-body-v1";

/// A refusal of the file, or a failure underneath it.
#[derive(Debug)]
pub enum Fail {
    /// A 4xx the person is given: the status, the message, and a `code` when it has one.
    Refused {
        status: u16,
        message: String,
        code: Option<&'static str>,
    },
    Storage(StorageError),
    /// A local file could not be written.
    Io(std::io::Error),
}

impl From<std::io::Error> for Fail {
    fn from(e: std::io::Error) -> Self {
        // The folders of a file are made below `users/<id>` and never that folder (#1425): `NotFound`
        // there is the account having been removed meanwhile.
        if e.kind() == std::io::ErrorKind::NotFound {
            return Fail::Storage(StorageError::gone());
        }
        Fail::Io(e)
    }
}
impl From<StorageError> for Fail {
    fn from(e: StorageError) -> Self {
        Fail::Storage(e)
    }
}

/// upload-sniff `validate(name, bytes)` as upload-sniff.cjs words it.
pub fn validate(name: &str, len: u64, head: &[u8]) -> Result<(), Fail> {
    let message = |r: upload_sniff::Refusal| match r {
        upload_sniff::Refusal::Filename => "Use a plain filename of at most 200 characters.",
        upload_sniff::Refusal::Empty | upload_sniff::Refusal::TooBig => {
            "Files must be non-empty and no larger than 25 MB."
        }
        upload_sniff::Refusal::Archive => {
            "Archive bundles are not supported. Upload their individual files instead."
        }
    };
    if name.is_empty() {
        return Err(Fail::Refused {
            status: 400,
            message: message(upload_sniff::Refusal::Filename).into(),
            code: None,
        });
    }
    match upload_sniff::validate(name, len, head) {
        None => Ok(()),
        Some(r) => Err(Fail::Refused {
            status: r.status(),
            message: message(r).into(),
            code: None,
        }),
    }
}

pub fn classify(name: &str) -> &'static str {
    upload_sniff::classify(name)
}

fn image_mime(name: &str) -> Option<&'static str> {
    match source_limits::extname(name).to_lowercase().as_str() {
        ".png" => Some("image/png"),
        ".jpg" | ".jpeg" => Some("image/jpeg"),
        ".webp" => Some("image/webp"),
        ".gif" => Some("image/gif"),
        _ => None,
    }
}

pub fn sha_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// uploads.cjs `destinationFor(project, name, { connection, remotePath, folderPath })`: the name an
/// upload is stored under — its storage path with a connection, else the plain name.
pub fn destination_for(
    project_folder: &str,
    name: &str,
    connection: bool,
    remote_path: Option<&str>,
    folder_path: &str,
) -> String {
    let rel = if folder_path.is_empty() {
        name.to_owned()
    } else {
        format!("{folder_path}/{name}")
    };
    if let Some(r) = remote_path.filter(|r| !r.is_empty()) {
        return r.to_owned();
    }
    if connection {
        return format!("{project_folder}/{}/{rel}", classify(name));
    }
    rel
}

/// What the caller gives `ingest`.
#[derive(Default)]
pub struct Options<'a> {
    /// The account has browsable storage (core's `connection`).
    pub connection: bool,
    /// The folder a sync read the file from (`source`).
    pub source: Option<&'a str>,
    /// The storage path a sync read it at: nothing is written back.
    pub remote_path: Option<&'a str>,
    /// A folder import's subfolder inside the type group.
    pub folder_path: &'a str,
    /// `ifMatch` / `ifNoneMatch: '*'`.
    pub condition: Option<Condition>,
    /// A document whose completion pass was left open (the sync's OCR allowance ran out) is not queued:
    /// the next sync continues it, as core's did. Only a document whose worker is down (`retryable`)
    /// gets a job. An upload has no next sync, so it queues an open document too.
    pub defer_open: bool,
}

/// A create-only write's record (core `created`): what [`discard_created`] removes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Created {
    pub path: String,
    pub etag: String,
}

/// An ingested file.
pub struct Ingested {
    /// The source record: `{ name, content, attachment, document?, source? }`.
    pub file: JValue,
    /// The image asset to list (a vision image of 8 MiB at most).
    pub asset: Option<JValue>,
    /// A document whose extraction is queued: its job still has to be written.
    pub queued: bool,
    /// A create-only write made this file in storage.
    pub created: Option<Created>,
}

pub struct Env<'a> {
    /// `users/<id>`.
    pub dir: &'a Path,
    pub project: &'a str,
    pub project_folder: &'a str,
    /// The project as listed now (limits, the file it replaces).
    pub files: &'a [JValue],
    pub assets_held: usize,
    pub engine: &'a dyn Engine,
    pub limits: &'a Limits,
    pub storage: Option<&'a dyn SourceStorage>,
    pub progress: &'a dyn Fn(&str),
}

fn text_of(dir: &Path, project: &str, file: &JValue) -> JValue {
    let mut f = file.clone();
    if f.get("content").as_str().is_none() {
        let text = crate::sources::text(dir, project, file).unwrap_or_default();
        if let JValue::Obj(items) = &mut f {
            wtf16::set(items, "content", JValue::Str(text));
        }
    }
    f
}

fn set(v: &mut JValue, key: &str, value: JValue) {
    if let JValue::Obj(items) = v {
        wtf16::set(items, key, value);
    }
}

/// uploads.cjs `ingest(workspace, project, name, bytes, options)`.
pub fn ingest(
    env: &Env<'_>,
    name: &str,
    bytes: &[u8],
    opts: &Options<'_>,
) -> Result<Ingested, Fail> {
    // #1864: a PDF synced from storage is not an upload: its size is bounded by
    // PROJECT_DOCUMENT_READ_BYTES when it is read; the name, type and content checks still run, on
    // its first 25 MB.
    let synced_pdf = opts.remote_path.is_some()
        && name.to_lowercase().ends_with(".pdf")
        && bytes.len() > CAP
        && bytes.len() <= env.limits.read_bytes;
    let checked = if synced_pdf {
        bytes.get(..CAP).unwrap_or(bytes)
    } else {
        bytes
    };
    validate(
        name,
        checked.len() as u64,
        checked.get(..upload_sniff::SNIFF_BYTES).unwrap_or(checked),
    )?;
    let group = classify(name);
    let mime = if bytes.len() <= IMAGE_MIB {
        image_mime(name)
    } else {
        None
    };
    let full_name = destination_for(
        env.project_folder,
        name,
        opts.connection,
        opts.remote_path,
        opts.folder_path,
    );
    let previous = env
        .files
        .iter()
        .find(|f| f.get("name").as_str() == Some(full_name.as_str()));
    // #1805: one place for the project limits.
    if previous.is_none() {
        if let Some(full) = source_limits::refusal(
            env.files,
            name,
            Some(if mime.is_some() { env.assets_held } else { 0 }),
        ) {
            return Err(Fail::Refused {
                status: 400,
                message: full.message,
                code: Some(full.code),
            });
        }
    }
    (env.progress)("Saving original");
    let id = sha_hex(bytes);
    save_original(env.dir, env.project, &id, bytes)?;
    let previous_text = previous.map(|p| text_of(env.dir, env.project, p));
    let docling = env.engine.docling_configured();
    let mut file = JValue::obj([
        ("name", JValue::Str(full_name.clone())),
        ("content", JValue::Str(String::new())),
    ]);
    let mut state = "stored".to_owned();
    let mut reason: Option<String> = None;
    let mut reason_id: Option<String> = None;
    let mut reason_params: Option<JValue> = None;
    let mut reader_version: Option<&str> = None;
    let mut queued = false;
    let lower = name.to_lowercase();
    if extract::is_document(name, docling) {
        (env.progress)("Extracting PDF text / OCR");
        let ingested = document_ingest::ingest(
            env.dir,
            env.project,
            &full_name,
            bytes,
            previous_text.as_ref(),
            env.engine,
            env.limits,
        )?;
        queued = ingested.retryable || (ingested.open && !opts.defer_open);
        file = ingested.file;
        if queued {
            extract_queue::mark_queued(&mut file, ingested.retryable);
        }
        state = file
            .get("document")
            .get("state")
            .as_str()
            .unwrap_or("failed")
            .to_owned();
    } else if lower.ends_with(".docx") {
        (env.progress)("Reading DOCX body text and tables");
        let reuse = previous_text.as_ref().filter(|p| {
            p.get("attachment").get("id").as_str() == Some(id.as_str())
                && p.get("attachment").get("readerVersion").as_str() == Some(DOCX_VERSION)
                && p.get("content").truthy()
        });
        if let Some(p) = reuse {
            set(&mut file, "content", p.get("content").clone());
            state = p
                .get("attachment")
                .get("state")
                .as_str()
                .unwrap_or("stored")
                .to_owned();
            reason = p
                .get("attachment")
                .get("reason")
                .as_str()
                .map(str::to_owned);
            reason_id = p
                .get("attachment")
                .get("reasonId")
                .as_str()
                .map(str::to_owned);
            reader_version = Some(DOCX_VERSION);
        } else {
            match docx(env.engine, bytes) {
                Ok((content, limited)) => {
                    let note = "DOCX body text and tables only; layout, images, headers, footers, comments and footnotes are not interpreted.";
                    set(&mut file, "content", JValue::Str(content));
                    state = "partial".into();
                    reader_version = Some(DOCX_VERSION);
                    if limited {
                        reason = Some(format!("{note} Text extraction limit reached."));
                        reason_id = Some("docxPartialLimit".into());
                    } else {
                        reason = Some(note.into());
                        reason_id = Some("docxPartial".into());
                    }
                }
                Err((message, id)) => {
                    reason = Some(wtf16::slice(&message, 300));
                    reason_id = id;
                }
            }
        }
    } else if group == "Text" {
        match upload_sniff::decode_text(bytes) {
            Ok(Some((text, encoding))) => {
                set(
                    &mut file,
                    "content",
                    JValue::Str(wtf16::slice(&text, TEXT_UNITS)),
                );
                let utf8 = encoding == upload_sniff::Encoding::Utf8;
                state = if bytes.len() > 200_000 || !utf8 {
                    "partial"
                } else {
                    "ready"
                }
                .into();
                if !utf8 {
                    reason_id = Some("encoding".into());
                    reason_params = Some(JValue::obj([(
                        "encoding",
                        JValue::Str(encoding.name().into()),
                    )]));
                    reason = Some(format!("Not valid UTF-8; read as {}. Characters outside that encoding may be wrong — re-save the file as UTF-8 if anything looks mangled.", encoding.name()));
                }
            }
            _ => {
                state = "stored".into();
                reason_id = Some("binaryText".into());
                reason = Some("This file is not readable as text — it looks like binary data despite its extension. The original is kept.".into());
            }
        }
    } else if mime.is_some() {
        state = "vision".into();
    }
    // The remote write comes LAST among the steps that can fail (#1653).
    let mut created = None;
    if opts.connection && opts.remote_path.is_none() {
        let storage = env.storage.ok_or_else(|| {
            StorageError::new(502, "the storage server could not be reached")
                .with_code("unreachable")
        })?;
        (env.progress)("Saving to Nextcloud / storage");
        storage.create_folder(&format!("{}/{group}", env.project_folder))?;
        // #1805: each level of a folder import's subfolder, outermost first (MKCOL needs the parent).
        if !opts.folder_path.is_empty() && !storage.s3() {
            let mut at = format!("{}/{group}", env.project_folder);
            for segment in opts.folder_path.split('/') {
                at.push('/');
                at.push_str(segment);
                storage.create_folder(&at)?;
            }
        }
        let condition = opts.condition.clone().unwrap_or(Condition::None);
        let written = storage.write(&full_name, bytes, &condition)?;
        // #1664: only a create-only write made a file that nothing else owns yet; an edit or replace
        // overwrote a recorded file, which a later failure must never delete.
        if condition == Condition::IfNoneMatch && written.created && !written.etag.is_empty() {
            created = Some(Created {
                path: full_name.clone(),
                etag: written.etag.clone(),
            });
        }
    }
    let finished = finish(
        env,
        name,
        bytes,
        opts,
        file,
        &full_name,
        &id,
        group,
        mime,
        &state,
        reason,
        reason_id,
        reason_params,
        reader_version,
        queued,
    );
    match finished {
        Ok(mut done) => {
            done.created = created;
            Ok(done)
        }
        Err(e) => {
            if let (Some(c), Some(storage)) = (&created, env.storage) {
                discard_created(storage, c);
            }
            Err(e)
        }
    }
}

/// docx.cjs `extract` through the OCR worker, worded as uploads.cjs words its outcome: the text
/// (with its scope note, at most 200 000 units) and whether a limit was reached; or the reason (and its
/// stable id).
fn docx(engine: &dyn Engine, bytes: &[u8]) -> Result<(String, bool), (String, Option<String>)> {
    let invalid = || ("Invalid DOCX reader response.".to_owned(), None);
    let result = engine.extract_docx(bytes).map_err(|e| (e.message, None))?;
    let text = result.get("text").as_str().ok_or_else(invalid)?;
    let truncated = result.get("truncated").as_bool().ok_or_else(invalid)?;
    if wtf16::len(text) > 200_000 {
        return Err(invalid());
    }
    if crate::js::trim(text).is_empty() {
        return Err((
            "No body text was recovered from this DOCX. The original is kept.".into(),
            Some("docxEmpty".into()),
        ));
    }
    let note = "DOCX body text and tables only; layout, images, headers, footers, comments and footnotes are not interpreted.";
    let combined = format!("[{note}]\n\n{text}");
    let content = wtf16::slice(&combined, 200_000);
    let limited = truncated || wtf16::len(text) + wtf16::len(note) + 4 > 200_000;
    Ok((content, limited))
}

#[allow(clippy::too_many_arguments)]
fn finish(
    env: &Env<'_>,
    name: &str,
    bytes: &[u8],
    opts: &Options<'_>,
    mut file: JValue,
    full_name: &str,
    id: &str,
    group: &str,
    mime: Option<&'static str>,
    state: &str,
    reason: Option<String>,
    reason_id: Option<String>,
    reason_params: Option<JValue>,
    reader_version: Option<&str>,
    queued: bool,
) -> Result<Ingested, Fail> {
    let mut attachment = JValue::obj([
        ("id", JValue::Str(id.to_owned())),
        ("bytes", JValue::Num(bytes.len() as f64)),
        ("group", JValue::Str(group.to_owned())),
        ("state", JValue::Str(state.to_owned())),
    ]);
    if let Some(r) = reason {
        set(&mut attachment, "reason", JValue::Str(r));
    }
    if let Some(r) = reason_id {
        set(&mut attachment, "reasonId", JValue::Str(r));
    }
    if let Some(p) = reason_params {
        set(&mut attachment, "reasonParams", p);
    }
    if let Some(v) = reader_version {
        set(&mut attachment, "readerVersion", JValue::Str(v.into()));
    }
    if group == "Images" && bytes.len() > IMAGE_MIB {
        set(
            &mut attachment,
            "reason",
            JValue::Str("Original stored; resize below 8 MB for model image input.".into()),
        );
        set(
            &mut attachment,
            "reasonId",
            JValue::Str("imageTooLarge".into()),
        );
        // `reasonParams: undefined`.
        if let JValue::Obj(items) = &mut attachment {
            items.retain(|(k, _)| k != "reasonParams");
        }
    }
    let source = opts
        .source
        .map(str::to_owned)
        .or_else(|| (opts.connection).then(|| env.project_folder.to_owned()));
    let mut asset = None;
    if let Some(mime) = mime {
        let asset_id = format!(
            "img-{}",
            sha_hex(format!("{full_name}:{id}").as_bytes())
                .get(..40)
                .unwrap_or("")
        );
        let asset_dir = store::asset_dir(env.dir, env.project);
        assets::write_asset_file(env.dir, &asset_dir, &asset_id, bytes)?;
        set(&mut attachment, "assetId", JValue::Str(asset_id.clone()));
        let mut a = JValue::obj([
            ("id", JValue::Str(asset_id)),
            ("name", JValue::Str(name.to_owned())),
            ("mime", JValue::Str(mime.into())),
            ("bytes", JValue::Num(bytes.len() as f64)),
            ("sourceName", JValue::Str(full_name.to_owned())),
        ]);
        if source.is_some() {
            set(&mut a, "storagePath", JValue::Str(full_name.to_owned()));
        }
        asset = Some(a);
    }
    set(&mut file, "attachment", attachment);
    if let Some(source) = source {
        set(&mut file, "source", JValue::Str(source));
    }
    Ok(Ingested {
        file,
        asset,
        queued,
        created: None,
    })
}

/// uploads.cjs `retire(project, assets)`: image assets that left `project.assets`. They are no longer
/// model input, but a chat transcript may still name one; such an asset keeps its bytes until no chat of
/// the project mentions it (#218).
fn retire(project: &mut JValue, leaving: Vec<JValue>, now_ms: i64) {
    if leaving.is_empty() {
        return;
    }
    let mut retired: Vec<JValue> = match project.get("retiredAssets") {
        JValue::Arr(a) => a.clone(),
        _ => Vec::new(),
    };
    for a in leaving {
        let Some(id) = a.get("id").as_str().filter(|s| !s.is_empty()) else {
            continue;
        };
        if retired.iter().any(|r| r.get("id").as_str() == Some(id)) {
            continue;
        }
        let mut copy = a.clone();
        set(&mut copy, "retiredAt", JValue::Num(now_ms as f64));
        retired.push(copy);
    }
    store::set(project, "retiredAssets", JValue::Arr(retired));
}

/// The rest of uploads.cjs `ingest`'s `finish()` for the project list: the assets named by this file's
/// source are retired and dropped, and the new image (if any) is listed.
pub fn apply_assets(project: &mut JValue, ingested: &Ingested, now_ms: i64) {
    let name = ingested.file.get("name").as_str().unwrap_or("").to_owned();
    let assets: Vec<JValue> = match project.get("assets") {
        JValue::Arr(a) => a.clone(),
        _ => Vec::new(),
    };
    let (leaving, kept): (Vec<JValue>, Vec<JValue>) = assets
        .into_iter()
        .partition(|a| a.get("sourceName").as_str() == Some(name.as_str()));
    retire(project, leaving, now_ms);
    let mut kept = kept;
    if let Some(a) = &ingested.asset {
        kept.push(a.clone());
    }
    store::set(project, "assets", JValue::Arr(kept));
}

/// uploads.cjs `discardCreated(record)`: remove a file a create-only write just made, after a later
/// step failed, so the retry does not meet a 409 and save a "(2)" copy. Conditional on the write's
/// ETag, so a file edited since, or written by someone else, is kept. Never fails.
pub fn discard_created(storage: &dyn SourceStorage, record: &Created) -> bool {
    if record.etag.is_empty() || record.path.is_empty() {
        return false;
    }
    let (deleted, reason) = storage.discard(&record.path, &record.etag);
    if !deleted {
        eprintln!(
            "[uploads] could not remove unrecorded upload \"{}\" ({})",
            record.path,
            if reason.is_empty() { "unknown" } else { reason }
        );
    }
    deleted
}

/// The record's `document.indexing`, as `indexSource` sets it, for a file that carries a document.
pub fn mark(file: &mut JValue) {
    if file.get("document").truthy() {
        mark_indexing(file);
    }
}

/// The folder-sync form of `destination_for` for a managed type group: whether `path` is in one.
pub fn managed(folder: &str, project_folder: &str, path: &str) -> bool {
    folder == project_folder
        && GROUPS
            .iter()
            .any(|g| path.starts_with(&format!("{folder}/{g}/")))
}

/// For the caller: the uploads dir of a project.
pub fn uploads_dir(dir: &Path, project: &str) -> std::path::PathBuf {
    lifecycle::uploads_dir(dir, project)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing, clippy::panic)]
mod tests {
    use super::*;
    use crate::extract::tests::Fake as FakeEngine;
    use crate::source_storage::fake::Fake as FakeStorage;

    const LIMITS: Limits = Limits {
        read_bytes: 96 * 1024 * 1024,
        pages: 1000,
        text: 8_000_000,
        range_pages: 25,
    };

    struct Rig {
        root: tempfile::TempDir,
        engine: FakeEngine,
        storage: FakeStorage,
    }
    impl Rig {
        fn new() -> Self {
            let root = tempfile::tempdir().unwrap();
            std::fs::create_dir_all(root.path().join("p")).unwrap();
            Rig {
                root,
                engine: FakeEngine::default(),
                storage: FakeStorage::default(),
            }
        }
        fn env<'a>(&'a self, files: &'a [JValue], progress: &'a dyn Fn(&str)) -> Env<'a> {
            Env {
                dir: self.root.path(),
                project: "proj-a",
                project_folder: "noevia projects/proj-a",
                files,
                assets_held: 0,
                engine: &self.engine,
                limits: &LIMITS,
                storage: Some(&self.storage),
                progress,
            }
        }
    }

    fn remote(folder_path: &'static str) -> Options<'static> {
        Options {
            connection: true,
            folder_path,
            condition: Some(Condition::IfNoneMatch),
            ..Default::default()
        }
    }

    #[test]
    fn a_text_file_is_decoded_saved_and_written_create_only_into_its_type_group() {
        let rig = Rig::new();
        let seen = std::cell::RefCell::new(Vec::new());
        let progress = |s: &str| seen.borrow_mut().push(s.to_owned());
        let env = rig.env(&[], &progress);
        let out = ingest(&env, "notes.md", b"# hello", &remote("")).unwrap();
        let f = &out.file;
        assert_eq!(
            f.get("name").as_str(),
            Some("noevia projects/proj-a/Text/notes.md")
        );
        assert_eq!(f.get("content").as_str(), Some("# hello"));
        assert_eq!(f.get("source").as_str(), Some("noevia projects/proj-a"));
        let a = f.get("attachment");
        assert_eq!(a.get("group").as_str(), Some("Text"));
        assert_eq!(a.get("state").as_str(), Some("ready"));
        assert_eq!(a.get("bytes").as_num(), Some(7.0));
        assert_eq!(a.get("id").as_str(), Some(sha_hex(b"# hello").as_str()));
        assert_eq!(
            rig.storage.log(),
            vec![
                "MKCOL noevia projects/proj-a/Text",
                "PUT noevia projects/proj-a/Text/notes.md"
            ]
        );
        assert_eq!(
            out.created.as_ref().unwrap().path,
            "noevia projects/proj-a/Text/notes.md"
        );
        assert_eq!(
            *seen.borrow(),
            vec!["Saving original", "Saving to Nextcloud / storage"]
        );
        // The original is kept in the project's uploads folder.
        let kept = lifecycle::uploads_dir(rig.root.path(), "proj-a").join(sha_hex(b"# hello"));
        assert_eq!(std::fs::read(kept).unwrap(), b"# hello");
    }

    #[test]
    fn a_folder_imports_subfolders_are_made_outermost_first_and_a_taken_path_is_a_409() {
        let rig = Rig::new();
        let none = |_: &str| {};
        let env = rig.env(&[], &none);
        ingest(&env, "a.md", b"x", &remote("docs/api")).unwrap();
        assert_eq!(
            rig.storage.log(),
            vec![
                "MKCOL noevia projects/proj-a/Text",
                "MKCOL noevia projects/proj-a/Text/docs",
                "MKCOL noevia projects/proj-a/Text/docs/api",
                "PUT noevia projects/proj-a/Text/docs/api/a.md",
            ]
        );
        let again = match ingest(&env, "a.md", b"y", &remote("docs/api")) {
            Err(Fail::Storage(e)) => e,
            other => panic!("{:?}", other.map(|_| ())),
        };
        assert!(again.path_taken());
    }

    #[test]
    fn a_later_failure_removes_the_file_a_create_only_write_made() {
        let rig = Rig::new();
        let none = |_: &str| {};
        let env = rig.env(&[], &none);
        // An image whose asset cannot be written: the user directory has an asset folder that is a file.
        let asset_dir = store::asset_dir(rig.root.path(), "proj-a");
        std::fs::create_dir_all(asset_dir.parent().unwrap()).unwrap();
        std::fs::write(&asset_dir, b"in the way").unwrap();
        let out = ingest(&env, "pic.gif", b"GIF89a", &remote(""));
        assert!(matches!(out, Err(Fail::Io(_))));
        assert!(
            !rig.storage
                .files
                .borrow()
                .contains_key("noevia projects/proj-a/Images/pic.gif"),
            "the unrecorded remote file went with the failure: {:?}",
            rig.storage.log()
        );
        assert!(rig.storage.log().iter().any(|l| l.starts_with("DISCARD ")));
    }

    #[test]
    fn a_synced_file_writes_nothing_back_and_a_vision_image_gets_its_asset() {
        let rig = Rig::new();
        let none = |_: &str| {};
        let env = rig.env(&[], &none);
        let opts = Options {
            connection: true,
            source: Some("noevia projects/proj-a"),
            remote_path: Some("noevia projects/proj-a/Images/pic.webp"),
            ..Default::default()
        };
        let out = ingest(&env, "pic.webp", b"RIFF....WEBPVP8 ", &opts).unwrap();
        assert!(rig.storage.log().is_empty(), "nothing is written back");
        assert_eq!(
            out.file.get("attachment").get("state").as_str(),
            Some("vision")
        );
        let asset = out.asset.as_ref().unwrap();
        assert_eq!(asset.get("mime").as_str(), Some("image/webp"));
        assert_eq!(
            asset.get("sourceName").as_str(),
            Some("noevia projects/proj-a/Images/pic.webp")
        );
        assert_eq!(out.file.get("attachment").get("assetId"), asset.get("id"));
        // Applied to a project: the earlier asset of that source is retired, the new one listed.
        let mut project = JValue::obj([(
            "assets",
            JValue::Arr(vec![JValue::obj([
                ("id", JValue::Str("img-old".into())),
                (
                    "sourceName",
                    JValue::Str("noevia projects/proj-a/Images/pic.webp".into()),
                ),
            ])]),
        )]);
        apply_assets(&mut project, &out, 55);
        let JValue::Arr(listed) = project.get("assets").clone() else {
            panic!()
        };
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].get("id"), asset.get("id"));
        assert_eq!(project.get("retiredAssets").get("0").as_str(), None);
        let JValue::Arr(retired) = project.get("retiredAssets").clone() else {
            panic!()
        };
        assert_eq!(retired[0].get("id").as_str(), Some("img-old"));
        assert_eq!(retired[0].get("retiredAt").as_num(), Some(55.0));
    }

    #[test]
    fn rules_limits_and_binary_text_are_core_s() {
        let rig = Rig::new();
        let none = |_: &str| {};
        let env = rig.env(&[], &none);
        let local = Options::default();
        // Archives and bad names are refused with core's words.
        for (name, bytes, status) in [
            ("a.zip", &b"PK\x03\x04"[..], 400),
            ("bad/name.md", b"x", 400),
        ] {
            match ingest(&env, name, bytes, &local) {
                Err(Fail::Refused { status: s, .. }) => assert_eq!(s, status, "{name}"),
                other => panic!("{name}: {:?}", other.map(|_| ())),
            }
        }
        // A text file with NULs is binary data despite its extension: stored, never read.
        let out = ingest(&env, "bin.txt", b"a\0b", &local).unwrap();
        let a = out.file.get("attachment");
        assert_eq!(a.get("state").as_str(), Some("stored"));
        assert_eq!(a.get("reasonId").as_str(), Some("binaryText"));
        assert_eq!(out.file.get("name").as_str(), Some("bin.txt"));
        // Not UTF-8: read as windows-1252, partial, and says so.
        let out = ingest(&env, "old.txt", b"caf\xe9", &local).unwrap();
        let a = out.file.get("attachment");
        assert_eq!(a.get("state").as_str(), Some("partial"));
        assert_eq!(a.get("reasonId").as_str(), Some("encoding"));
        assert_eq!(
            a.get("reasonParams").get("encoding").as_str(),
            Some("windows-1252")
        );
        assert_eq!(out.file.get("content").as_str(), Some("caf\u{e9}"));
        // The project limit is a 400 with its code.
        let many: Vec<JValue> = (0..1000)
            .map(|i| JValue::obj([("name", JValue::Str(format!("f{i}.md")))]))
            .collect();
        let full = rig.env(&many, &none);
        match ingest(&full, "one-more.md", b"x", &local) {
            Err(Fail::Refused {
                status: 400,
                code: Some("projectLimit"),
                ..
            }) => {}
            other => panic!("{:?}", other.map(|_| ())),
        }
    }
}
