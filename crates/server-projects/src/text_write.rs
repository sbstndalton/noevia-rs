//! core projects.cjs `writeProjectTextFile(project, name, text, options)` (track R14 step 5): the
//! one write path of the internal MCP project tools (`project_create_file`, `project_append_file`,
//! `project_replace_text`), the same one a browser upload takes: the file lands in the project's
//! storage folder (or stays in noevia without storage), is recorded in the project's list and is
//! indexed again. Every refusal is core's text, word for word; nothing is written then.
//!
//! An in-place edit (#648) names the stored file it read (`expectName`), the text it read
//! (`expectContent`) and that file's attachment id (`expectAttachment`); it is refused unless the
//! destination is exactly that file, it still holds that text and that version, and, for a stored
//! file, storage still holds exactly those bytes (sha256) under an ETag the write is conditional on
//! (If-Match). An adopt (#687, `adoptTo`, `adoptAccount`) moves a plain-named file into the project
//! folder: the folder is made when there is none, the destination must be `adoptTo`, nothing may be
//! there yet (the write is create-only), the storage account must be the approved one, and the
//! plain copy's index entries go once the list without it is saved.
//!
//! Differences from core, each a refusal where core would go on or wait:
//! - The project's source lock is the front's ([`crate::project_documents::source_lock`], shared
//!   with the front's uploads and syncs); held past `lock_wait`, the write is refused with the
//!   upload's 429 text. Core waited without end on its own lock (not shared with the front).
//! - "The project changed while writing": core compared the object it held with the one its list
//!   holds; here the project is looked up by id under the lock and the write is refused only when
//!   it is gone.
//! - A create in a project whose storage folder cannot be made is refused (core wrote the file to
//!   `null/<group>/<name>` in storage, then recorded it under that name: a bug not copied).
//! - Storage the front cannot use now ([`Storage::Refused`]) refuses with its own text where core
//!   would have tried and failed.
//! - multimodal-rag's `sourceChanged` nudge is the outbox index job's (as for an upload).
//! - A create-only write (an adopt) whose record cannot be saved is removed from storage again
//!   (#1664, as an upload does); core left it there.
//! - `expectAttachment` arrives as JSON, so an attachment id core's caller held as `undefined` (an
//!   attachment record without an id, which core never writes) reads as `null`.
use crate::extract::{Engine, Limits};
use crate::source_storage::{BinaryRead, Condition, SourceStorage, Storage, StorageError};
use crate::uploads_ingest::{self as uploads, Env as IngestEnv, Fail, Options};
use crate::{document_ingest, lifecycle, outbox, project_documents, store, workspace_view, Ctx};
use js_json::{wtf16, JValue};
use sha2::{Digest, Sha256};
use std::path::Path;
use std::time::Duration;

/// What the caller hands the write; `JValue::Undefined` is an option core's caller left out.
#[derive(Debug, Clone)]
pub struct Edit {
    pub expect_name: JValue,
    pub expect_content: JValue,
    pub expect_attachment: JValue,
    pub adopt_to: JValue,
    pub adopt_account: JValue,
}

impl Default for Edit {
    fn default() -> Self {
        Edit {
            expect_name: JValue::Undefined,
            expect_content: JValue::Undefined,
            expect_attachment: JValue::Undefined,
            adopt_to: JValue::Undefined,
            adopt_account: JValue::Undefined,
        }
    }
}

pub struct Env<'a> {
    pub ctx: Ctx<'a>,
    pub storage: &'a Storage<'a>,
    pub engine: &'a dyn Engine,
    pub limits: &'a Limits,
    /// index.cjs PROJECT_ROOT_FOLDER.
    pub project_root: &'a str,
    pub lock_wait: Duration,
}

/// How long a project file read back for an edit may take (core's readBinaryFile default).
const READ_BACK: Duration = Duration::from_secs(60);

fn changed_in_storage(stored: &str) -> String {
    format!("\"{stored}\" changed in storage since noevia last read it (it was edited, moved or deleted there, or storage is now a different account). Nothing was saved. Sync this project's Sources first, then try again.")
}

/// core `editRefusal(stored, err)` (#655).
fn edit_refusal(stored: &str, e: &StorageError) -> String {
    match e.code.as_deref() {
        Some("unsupported") => return format!("\"{stored}\" cannot be edited in place: in-place edits are not supported on this storage type (only WebDAV / Nextcloud). Nothing was saved. Use project_create_file to write a new file instead."),
        Some("folder") => return format!("\"{stored}\" is a folder in storage, not a file, so it cannot be edited. Nothing was saved."),
        _ => {}
    }
    match e.upstream {
        Some(up @ (401 | 403)) => format!("Storage refused the sign-in while checking \"{stored}\" ({up}), so it was not edited. Nothing was saved. Reconnect storage in Settings → Storage and try again."),
        Some(404) => changed_in_storage(stored),
        _ if e.status == 404 => changed_in_storage(stored),
        _ => format!("Storage is unavailable right now, so \"{stored}\" could not be checked before editing. Nothing was saved. Try again in a moment."),
    }
}

/// `a === b` for JSON values (an object or list is never equal to another).
fn strict_eq(a: &JValue, b: &JValue) -> bool {
    match (a, b) {
        (JValue::Undefined, JValue::Undefined) | (JValue::Null, JValue::Null) => true,
        (JValue::Bool(x), JValue::Bool(y)) => x == y,
        (JValue::Num(x), JValue::Num(y)) => x == y,
        (JValue::Str(x), JValue::Str(y)) => x == y,
        _ => false,
    }
}

fn files_of(p: &JValue) -> Vec<JValue> {
    match p.get("files") {
        JValue::Arr(items) => items.clone(),
        _ => Vec::new(),
    }
}

/// `Buffer.from(text, 'utf8')`: a lone surrogate as U+FFFD.
fn utf8(text: &str) -> Vec<u8> {
    text.chars()
        .map(|c| if wtf16::lone_unit(c).is_some() { '\u{fffd}' } else { c })
        .collect::<String>()
        .into_bytes()
}

/// core `confirmStoredVersion(connection, stored, current)`: the ETag the write is conditional on.
fn confirm_stored_version(remote: &dyn SourceStorage, stored: &str, current: &JValue) -> Result<String, String> {
    let id = current.get("attachment").get("id");
    let id = match (current.get("attachment").truthy(), id) {
        (true, JValue::Str(s)) if s.len() == 64 && s.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)) => s.clone(),
        _ => return Err(changed_in_storage(stored)),
    };
    let etag = match remote.version(stored) {
        Ok(Some(etag)) => etag,
        Ok(None) => return Err(changed_in_storage(stored)),
        Err(e) => return Err(edit_refusal(stored, &e)),
    };
    let mut hash = Sha256::new();
    let mut before = |_: Option<u64>| Ok(());
    let read = remote.read_binary(
        stored,
        BinaryRead { cap: u64::MAX, timeout: READ_BACK, before_body: &mut before },
        &mut |chunk| {
            hash.update(chunk);
            Ok(())
        },
    );
    if let Err(e) = read {
        return Err(edit_refusal(stored, &e));
    }
    if format!("{:x}", hash.finalize()) != id {
        return Err(changed_in_storage(stored));
    }
    if etag.is_empty() {
        return Err(format!("\"{stored}\" cannot be edited in place: this storage server does not report file versions (ETags), so noevia cannot make sure a change made there meanwhile is not overwritten. Nothing was saved. Use project_create_file to write a new file instead."));
    }
    Ok(etag)
}

/// project-folders.cjs `createProjectFolder` through core's `ensureProjectFolder`: the folder, or
/// `None` when it could not be made (logged, as core logs it).
fn ensure_folder(env: &Env<'_>, remote: &dyn SourceStorage, id: &str, project: &JValue) -> Option<String> {
    match crate::source_storage::allocate_folder(remote, env.project_root, id, project.get("reservedFolder").as_str()) {
        Ok(f) => Some(f),
        Err(e) => {
            eprintln!("[projects] could not create folder for project {id}: {}", e.message);
            None
        }
    }
}

/// Records the folder at once, as an upload does (`project.projectFolder` and `sourceFolders`).
fn record_folder(env: &Env<'_>, dir: &Path, id: &str, folder: &str, sources: bool) -> Result<(), String> {
    let saved = store::update(dir, env.ctx.switch, |projects| {
        let Some(p) = store::find_mut(projects, id) else {
            return (Err("the project was removed while writing; nothing was saved".to_owned()), false);
        };
        store::set(p, "projectFolder", JValue::Str(folder.to_owned()));
        if sources {
            add_source_folder(p, folder);
        }
        if !lifecycle::reconcile_all(std::slice::from_mut(p)) {
            return (Err("the project could not be saved; nothing was saved".to_owned()), false);
        }
        (Ok(()), true)
    });
    match saved {
        Ok(r) => r,
        Err(_) => Err("the project could not be saved; nothing was saved".to_owned()),
    }
}

/// `project.sourceFolders = [...new Set([...(project.sourceFolders || []), folder])]`.
fn add_source_folder(p: &mut JValue, folder: &str) {
    let mut list: Vec<JValue> = match p.get("sourceFolders") {
        JValue::Arr(a) => a.clone(),
        v if !v.truthy() => Vec::new(),
        other => vec![other.clone()],
    };
    let mut seen: Vec<JValue> = Vec::new();
    list.retain(|v| {
        if seen.iter().any(|s| strict_eq(s, v)) {
            false
        } else {
            seen.push(v.clone());
            true
        }
    });
    if !list.iter().any(|v| v.as_str() == Some(folder)) {
        list.push(JValue::Str(folder.to_owned()));
    }
    store::set(p, "sourceFolders", JValue::Arr(list));
}

/// `writeProjectTextFile(project, name, text, edit)`: the file as recorded, or core's refusal.
pub fn write_text(env: &Env<'_>, id: &str, name: &str, text: &str, edit: &Edit) -> Result<JValue, String> {
    let lock = project_documents::source_lock(env.ctx.user_id, id);
    let Some((_held, ctx)) = project_documents::lock_restamped(&lock, env.lock_wait, env.ctx) else {
        return Err("Another upload or sync of this project is still running; try again shortly.".to_owned());
    };
    let env = &Env { ctx, ..*env };
    let result = locked(env, id, name, text, edit);
    if let Ok(dir) = store::user_dir(ctx.data_dir, ctx.user_id) {
        crate::upload::prune(&dir, ctx, id);
    }
    result
}

fn locked(env: &Env<'_>, id: &str, name: &str, text: &str, edit: &Edit) -> Result<JValue, String> {
    let ctx = env.ctx;
    let gone = || "the project changed while writing; nothing was saved".to_owned();
    let (dir, project) = crate::originals::project(ctx, id).map_err(|_| gone())?;
    let mut project = project.ok_or_else(gone)?;
    let bytes = utf8(text);
    if let Err(f) = uploads::validate(name, bytes.len() as u64, bytes.get(..upload_sniff::SNIFF_BYTES).unwrap_or(&bytes)) {
        return Err(fail_text(f));
    }
    let files = files_of(&project);
    let is_edit = !matches!(edit.expect_name, JValue::Undefined);
    let mut current: Option<(usize, JValue)> = None;
    if is_edit {
        let expect = match &edit.expect_name {
            JValue::Str(s) if !s.is_empty() => s.clone(),
            _ => return Err("an edit must name the stored file it changes; nothing was saved".to_owned()),
        };
        let Some((at, found)) = files
            .iter()
            .enumerate()
            .find(|(_, f)| f.truthy() && f.get("name").as_str() == Some(expect.as_str()))
        else {
            return Err(format!("\"{expect}\" is no longer in this project; nothing was saved"));
        };
        if !matches!(edit.expect_content, JValue::Undefined) {
            let now = workspace_view::source_text(&dir, id, found).ok_or_else(|| format!("\"{expect}\" changed while this edit was being made; nothing was saved. Read it again and retry."))?;
            if !strict_eq(&JValue::Str(now), &edit.expect_content) {
                return Err(format!("\"{expect}\" changed while this edit was being made; nothing was saved. Read it again and retry."));
            }
        }
        let attachment = found.get("attachment");
        let current_id = if attachment.truthy() { attachment.get("id").clone() } else { JValue::Null };
        let expected = if matches!(edit.expect_attachment, JValue::Undefined) { JValue::Null } else { edit.expect_attachment.clone() };
        if !strict_eq(&current_id, &expected) || (attachment.truthy() && attachment.get("state").as_str() != Some("ready")) {
            return Err(format!("\"{expect}\" changed since this edit was prepared; nothing was saved. Sync this project's Sources, read it again and retry."));
        }
        current = Some((at, found.clone()));
    }
    let expect_name = edit.expect_name.as_str().unwrap_or_default().to_owned();
    let remote = env.storage.remote();
    let refused = match env.storage {
        Storage::Refused(_, message) => Some(*message),
        _ => None,
    };
    let folder_of = |p: &JValue| p.get("projectFolder").as_str().filter(|f| !f.is_empty()).map(str::to_owned);
    let mut condition = Condition::None;
    let adopt = is_edit && !matches!(edit.adopt_to, JValue::Undefined);
    if let (true, Some((_, cur))) = (adopt, &current) {
        let adopt_to = match &edit.adopt_to {
            JValue::Str(s) if !s.is_empty() => s.clone(),
            _ => String::new(),
        };
        let cur_name = cur.get("name").as_str().unwrap_or_default();
        if adopt_to.is_empty() || cur.get("source").truthy() || cur_name.contains('/') || cur_name != name {
            return Err(format!("\"{expect_name}\" cannot be moved into the project folder; nothing was saved"));
        }
        let Some(remote) = remote else {
            if let Some(m) = refused {
                return Err(m.to_owned());
            }
            return Err(format!("\"{expect_name}\" was to be saved as \"{adopt_to}\" in this project's storage, which is not connected right now, so nothing was saved. Reconnect storage and try again."));
        };
        let approved = match &edit.adopt_account {
            JValue::Str(s) if !s.is_empty() => s.as_str(),
            _ => "",
        };
        if approved.is_empty() || remote.account() != approved {
            return Err(format!("\"{expect_name}\" was approved to be saved as \"{adopt_to}\" in a different storage account than the one connected now, so nothing was saved. Ask again to save it in this one."));
        }
        if folder_of(&project).is_none() {
            let Some(folder) = ensure_folder(env, remote, id, &project) else {
                return Err(format!("Could not create this project's storage folder, so \"{expect_name}\" was not edited. Nothing was saved. Check the storage connection and try again."));
            };
            record_folder(env, &dir, id, &folder, true)?;
            store::set(&mut project, "projectFolder", JValue::Str(folder.clone()));
            add_source_folder(&mut project, &folder);
        }
        let destination = uploads::destination_for(&folder_of(&project).unwrap_or_default(), name, true, None, "");
        if destination != adopt_to {
            return Err(format!("saving now would write \"{destination}\" instead of \"{adopt_to}\", the file that was approved; nothing was saved. Ask again to edit it there."));
        }
        match remote.version(&adopt_to) {
            Err(e) => return Err(edit_refusal(&adopt_to, &e)),
            Ok(Some(_)) => return Err(format!("\"{adopt_to}\" already exists in storage and noevia has not read it, so it was not overwritten. Nothing was saved. Sync this project's Sources, then edit that file.")),
            Ok(None) => {}
        }
        condition = Condition::IfNoneMatch;
    } else if let Some((_, cur)) = &current {
        let destination = match (remote, folder_of(&project)) {
            (Some(_), Some(folder)) => Some(uploads::destination_for(&folder, name, true, None, "")),
            (Some(_), None) => None,
            (None, _) => Some(name.to_owned()),
        };
        if destination.as_deref() != Some(expect_name.as_str()) {
            if remote.is_none() {
                if let Some(m) = refused {
                    return Err(m.to_owned());
                }
            }
            return Err(if remote.is_some() {
                format!("saving now would write \"{}\" instead of editing \"{expect_name}\" in place; nothing was saved", destination.as_deref().unwrap_or(name))
            } else {
                format!("\"{expect_name}\" is kept in this project's storage, which is not connected right now, so it cannot be edited in place; nothing was saved. Reconnect storage and try again.")
            });
        }
        if let Some(remote) = remote {
            condition = Condition::IfMatch(confirm_stored_version(remote, &expect_name, cur)?);
        }
    } else if let (Some(remote), None) = (remote, folder_of(&project)) {
        let Some(folder) = ensure_folder(env, remote, id, &project) else {
            return Err(format!("Could not create this project's storage folder, so \"{name}\" was not created. Nothing was saved. Check the storage connection and try again."));
        };
        store::set(&mut project, "projectFolder", JValue::Str(folder));
    }
    let project_folder = folder_of(&project).unwrap_or_default();
    let ingest_env = IngestEnv {
        dir: &dir,
        project: id,
        project_folder: &project_folder,
        files: &files,
        assets_held: match project.get("assets") {
            JValue::Arr(a) => a.len(),
            _ => 0,
        },
        engine: env.engine,
        limits: env.limits,
        storage: remote,
        progress: &|_| {},
    };
    let condition = (!matches!(condition, Condition::None)).then_some(condition);
    let opts = Options { connection: remote.is_some(), condition, ..Default::default() };
    let ingested = match uploads::ingest(&ingest_env, name, &bytes, &opts) {
        Ok(i) => i,
        Err(Fail::Storage(e)) if e.code.as_deref() == Some("changed") => {
            return Err(if adopt {
                let to = edit.adopt_to.as_str().unwrap_or_default();
                format!("\"{to}\" appeared in storage while this edit was being saved, so it was not overwritten. Nothing was saved. Sync this project's Sources, then edit that file.")
            } else {
                changed_in_storage(&expect_name)
            });
        }
        Err(Fail::Storage(e)) if is_edit && e.code.as_deref() == Some("unknown") => {
            let what = if adopt { edit.adopt_to.as_str().unwrap_or_default() } else { expect_name.as_str() };
            return Err(format!("The connection to storage failed while saving \"{what}\", so noevia cannot tell whether the edit landed. Outcome unknown: sync this project's Sources to check whether it was saved before trying again."));
        }
        Err(f) => return Err(fail_text(f)),
    };
    let mut file = ingested.file.clone();
    let file_name = file.get("name").as_str().unwrap_or_default().to_owned();
    let stored = if adopt { edit.adopt_to.as_str().unwrap_or_default().to_owned() } else { expect_name.clone() };
    // A create-only write whose record is not saved is removed again (#1664, as an upload does;
    // core left it in storage).
    let discard = || {
        if let (Some(c), Some(storage)) = (&ingested.created, remote) {
            uploads::discard_created(storage, c);
        }
    };
    if is_edit && file_name != stored {
        discard();
        return Err(format!("the edit was stored as \"{file_name}\", not \"{stored}\"; the project list was not changed"));
    }
    if file.get("document").truthy() {
        document_ingest::mark_indexing(&mut file);
    }
    let folder_now = project_folder.clone();
    let remote_here = remote.is_some();
    let adopted = current.as_ref().filter(|_| adopt).map(|(at, f)| (*at, f.clone()));
    let saved = store::update(&dir, ctx.switch, |projects| {
        let Some(p) = store::find_mut(projects, id) else {
            return (Err("the project was removed while writing; the project list was not changed".to_owned()), false);
        };
        if remote_here && !folder_now.is_empty() {
            if p.get("projectFolder").as_str().is_none_or(str::is_empty) {
                store::set(p, "projectFolder", JValue::Str(folder_now.clone()));
            }
            add_source_folder(p, &folder_now);
        }
        // A moved file (#687) leaves its plain-named copy behind in neither the list nor the index.
        let next: Vec<JValue> = files_of(p)
            .into_iter()
            .filter(|f| {
                f.truthy()
                    && f.get("name").as_str() != Some(file_name.as_str())
                    && !adopted.as_ref().is_some_and(|(_, cur)| f == cur)
            })
            .chain(std::iter::once(file.clone()))
            .collect();
        store::set(p, "files", JValue::Arr(next));
        store::set(p, "updatedAt", JValue::Num(ctx.now_ms as f64));
        if lifecycle::externalize_reply(&dir, p).is_err() || !lifecycle::reconcile_all(std::slice::from_mut(p)) {
            return (Err("the project could not be saved; the project list was not changed".to_owned()), false);
        }
        (Ok(()), true)
    });
    match saved {
        Ok(Ok(())) => {}
        Ok(Err(m)) => {
            discard();
            return Err(m);
        }
        Err(_) => {
            discard();
            return Err("the project could not be saved; the project list was not changed".to_owned());
        }
    }
    let mut jobs = vec![outbox::Job { kind: "index", project: id, file: Some(file_name.as_str()), payload: None }];
    // #1914/#1919: the plain copy's index entries go once the list without it is on disk, and only
    // if no source of that name is listed again by then.
    let relisted = adopt
        && crate::originals::project(ctx, id)
            .ok()
            .and_then(|(_, p)| p)
            .is_some_and(|p| files_of(&p).iter().any(|f| f.get("name").as_str() == Some(expect_name.as_str())));
    if adopt && !relisted {
        jobs.push(outbox::Job { kind: "unindex", project: id, file: Some(expect_name.as_str()), payload: None });
    }
    if let Err(e) = outbox::enqueue(&dir, &jobs, ctx.now_ms) {
        eprintln!("noevia-server: index outbox write failed for a project text write: {e:?}");
    }
    Ok(file)
}

/// What an ingest failure says to the model: core threw its message.
fn fail_text(f: Fail) -> String {
    match f {
        Fail::Refused { message, .. } => message,
        Fail::Storage(e) => e.message,
        Fail::Io(e) => {
            eprintln!("noevia-server: a project text write could not be stored: {}", e.kind());
            "the file could not be stored; nothing was saved".to_owned()
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]
mod tests {
    //! Every case of tests/fixtures/node-text-write.v1.json (tools/gen-text-write-fixtures.cjs: core's
    //! own writeProjectTextFile on a scripted storage) against [`write_text`] on the same script: the
    //! answer, the storage calls in order, the project as saved and the index work.
    use super::*;
    use crate::extract::{EngineError, OcrCaps};
    use crate::source_storage::{Entry, TextRead, Written};
    use std::cell::RefCell;
    use std::collections::{BTreeMap, BTreeSet};

    fn fixture() -> JValue {
        wtf16::parse(include_str!("../tests/fixtures/node-text-write.v1.json")).unwrap()
    }

    fn arr(v: &JValue) -> &[JValue] {
        match v {
            JValue::Arr(items) => items,
            other => panic!("not a list: {other:?}"),
        }
    }

    fn obj(v: &JValue) -> &[(String, JValue)] {
        match v {
            JValue::Obj(items) => items,
            JValue::Undefined | JValue::Null => &[],
            other => panic!("not an object: {other:?}"),
        }
    }

    fn json(v: &JValue) -> String {
        wtf16::stringify(v).unwrap()
    }

    struct NoEngine;
    impl Engine for NoEngine {
        fn docling_configured(&self) -> bool {
            false
        }
        fn docling(&self, _: &str, _: &[u8], _: bool, _: Option<(u32, u32)>) -> Result<JValue, EngineError> {
            panic!("a text write never asks Docling")
        }
        fn docling_ranges(&self, _: usize) -> Option<bool> {
            None
        }
        fn ocr_configured(&self) -> bool {
            false
        }
        fn ocr_pages(&self, _: &[u8], _: &[u32], _: Option<u32>, _: Option<u64>) -> Result<Vec<JValue>, EngineError> {
            panic!("a text write never asks OCR")
        }
        fn ocr_caps(&self) -> Option<OcrCaps> {
            None
        }
        fn multimodal(&self) -> bool {
            false
        }
    }

    /// The recorder's scripted storage.
    struct Script {
        files: RefCell<BTreeMap<String, (Vec<u8>, String)>>,
        dirs: RefCell<BTreeSet<String>>,
        fail: BTreeMap<(String, String), StorageError>,
        calls: RefCell<Vec<Vec<String>>>,
        n: RefCell<u64>,
        account: String,
    }

    fn error_of(v: &JValue) -> StorageError {
        let mut e = StorageError::new(v.get("status").as_num().unwrap() as u16, v.get("message").as_str().unwrap());
        if let Some(c) = v.get("code").as_str() {
            e = e.with_code(c);
        }
        if let Some(u) = v.get("upstream").as_num() {
            e = e.with_upstream(u as u16);
        }
        e
    }

    impl Script {
        fn new(spec: &JValue, account: &str) -> Script {
            let mut fail = BTreeMap::new();
            for (op, paths) in obj(spec.get("fail")) {
                for (p, e) in obj(paths) {
                    fail.insert((op.clone(), p.clone()), error_of(e));
                }
            }
            Script {
                files: RefCell::new(
                    obj(spec.get("files"))
                        .iter()
                        .map(|(p, f)| (p.clone(), (f.get("text").as_str().unwrap().as_bytes().to_vec(), f.get("etag").as_str().unwrap().to_owned())))
                        .collect(),
                ),
                dirs: RefCell::new(arr(spec.get("dirs")).iter().map(|d| d.as_str().unwrap().to_owned()).collect()),
                fail,
                calls: RefCell::new(Vec::new()),
                n: RefCell::new(0),
                account: account.to_owned(),
            }
        }
        fn call(&self, parts: &[&str]) {
            self.calls.borrow_mut().push(parts.iter().map(|s| (*s).to_owned()).collect());
        }
        fn failed(&self, op: &str, path: &str) -> Option<StorageError> {
            self.fail.get(&(op.to_owned(), path.to_owned())).cloned()
        }
    }

    impl SourceStorage for Script {
        fn s3(&self) -> bool {
            false
        }
        fn fingerprint(&self) -> String {
            String::new()
        }
        fn list(&self, _: &str) -> Result<Vec<Entry>, StorageError> {
            panic!("a text write never lists")
        }
        fn read_text(&self, _: &str) -> Result<TextRead, StorageError> {
            panic!("a text write never reads text")
        }
        fn read_binary(
            &self,
            path: &str,
            _: BinaryRead<'_>,
            sink: &mut dyn FnMut(&[u8]) -> Result<(), StorageError>,
        ) -> Result<u64, StorageError> {
            self.call(&["read", path]);
            if let Some(e) = self.failed("read", path) {
                return Err(e);
            }
            let Some((bytes, _)) = self.files.borrow().get(path).cloned() else {
                return Err(StorageError::new(404, "not found").with_upstream(404));
            };
            sink(&bytes)?;
            Ok(bytes.len() as u64)
        }
        fn create_folder(&self, path: &str) -> Result<bool, StorageError> {
            self.call(&["mkdir", path]);
            if let Some(e) = self.failed("mkdir", path) {
                return Err(e);
            }
            Ok(!self.dirs.borrow_mut().insert(path.to_owned()))
        }
        fn write(&self, path: &str, bytes: &[u8], condition: &Condition) -> Result<Written, StorageError> {
            let cond = match condition {
                Condition::None => String::new(),
                Condition::IfMatch(e) => format!("if-match {e}"),
                Condition::IfNoneMatch => "if-none-match".to_owned(),
            };
            self.call(&["write", path, &cond]);
            if let Some(e) = self.failed("write", path) {
                return Err(e);
            }
            let existing = self.files.borrow().get(path).cloned();
            let conflict = match condition {
                Condition::IfMatch(e) => existing.as_ref().is_none_or(|(_, etag)| etag != e),
                Condition::IfNoneMatch => existing.is_some(),
                Condition::None => false,
            };
            if conflict {
                return Err(StorageError::new(412, "changed").with_code("changed").with_upstream(412));
            }
            *self.n.borrow_mut() += 1;
            let etag = format!("\"qa-etag-{}\"", self.n.borrow());
            self.files.borrow_mut().insert(path.to_owned(), (bytes.to_vec(), etag.clone()));
            Ok(Written { etag, created: existing.is_none() })
        }
        fn discard(&self, path: &str, etag: &str) -> (bool, &'static str) {
            self.call(&["discard", path]);
            let same = self.files.borrow().get(path).is_some_and(|(_, e)| e == etag);
            if same {
                self.files.borrow_mut().remove(path);
                (true, "deleted")
            } else {
                (false, "changed")
            }
        }
        fn account(&self) -> String {
            self.account.clone()
        }
        fn version(&self, path: &str) -> Result<Option<String>, StorageError> {
            self.call(&["version", path]);
            if let Some(e) = self.failed("version", path) {
                return Err(e);
            }
            Ok(self.files.borrow().get(path).map(|(_, e)| e.clone()))
        }
    }

    fn calls_of(v: &JValue) -> Vec<Vec<String>> {
        arr(v)
            .iter()
            .map(|c| {
                let mut parts: Vec<String> = arr(c).iter().map(|p| p.as_str().unwrap().to_owned()).collect();
                // The recorder writes the condition only for a write.
                if parts.first().is_some_and(|op| op == "write") && parts.len() == 2 {
                    parts.push(String::new());
                }
                parts
            })
            .collect()
    }

    /// Lone surrogates as `fs.writeFileSync` writes them.
    fn utf8_text(text: &str) -> String {
        String::from_utf8(utf8(text)).unwrap()
    }

    #[test]
    fn every_write_answers_as_core() {
        let doc = fixture();
        let user = doc.get("user").as_str().unwrap();
        let root_folder = doc.get("root").as_str().unwrap();
        let now = doc.get("now").as_num().unwrap() as i64;
        let mut checked = 0;
        for c in arr(doc.get("cases")) {
            let name = c.get("name").as_str().unwrap();
            let root = tempfile::tempdir().unwrap();
            let dir = root.path().join("users").join(user);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("projects.json"), c.get("projects").as_str().unwrap()).unwrap();
            for (id, texts) in obj(doc.get("texts")) {
                let folder = dir.join("project-sources").join(id);
                std::fs::create_dir_all(&folder).unwrap();
                for t in arr(texts) {
                    let t = utf8_text(t.as_str().unwrap());
                    std::fs::write(folder.join(format!("{:x}.txt", Sha256::digest(t.as_bytes()))), t).unwrap();
                }
            }
            let before = std::fs::read(dir.join("projects.json")).unwrap();
            let account = obj(c.get("accounts")).first().and_then(|(_, a)| a.as_str()).unwrap_or_default();
            let script = Script::new(c.get("storage"), account);
            let storage = if matches!(c.get("connection"), JValue::Null) {
                Storage::None
            } else {
                Storage::Remote(&script)
            };
            let call = c.get("call");
            let options = call.get("options");
            let edit = Edit {
                expect_name: options.get("expectName").clone(),
                expect_content: options.get("expectContent").clone(),
                expect_attachment: options.get("expectAttachment").clone(),
                adopt_to: options.get("adoptTo").clone(),
                adopt_account: options.get("adoptAccount").clone(),
            };
            let env = Env {
                ctx: Ctx { data_dir: root.path(), user_id: user, switch: server_store::RustProjects::owned(), now_ms: now },
                storage: &storage,
                engine: &NoEngine,
                limits: &Limits::from_env(),
                project_root: root_folder,
                lock_wait: Duration::from_secs(5),
            };
            let got = write_text(&env, call.get("project").as_str().unwrap(), call.get("name").as_str().unwrap(), call.get("text").as_str().unwrap(), &edit);
            let node = c.get("node");
            match (node.get("error").as_str(), &got) {
                (Some(want), Err(m)) => assert_eq!(m, want, "{name}: error"),
                (None, Ok(file)) => assert_eq!(json(file), node.get("file").as_str().unwrap(), "{name}: file"),
                (want, got) => panic!("{name}: core {want:?}, port {got:?}"),
            }
            assert_eq!(*script.calls.borrow(), calls_of(node.get("calls")), "{name}: storage calls");
            let after = std::fs::read(dir.join("projects.json")).unwrap();
            match node.get("project").as_str() {
                None => assert_eq!(after, before, "{name}: projects.json changed"),
                Some(want) => {
                    let saved = wtf16::parse(&String::from_utf8(after).unwrap()).unwrap();
                    let p = arr(saved.get("projects"))
                        .iter()
                        .find(|p| p.get("id") == call.get("project"))
                        .unwrap();
                    assert_eq!(json(p), want, "{name}: project saved");
                }
            }
            let jobs: Vec<(String, String)> = outbox::jobs(&dir)
                .unwrap()
                .into_iter()
                .map(|r| (r.kind, r.file.unwrap_or_default()))
                .collect();
            let want: Vec<(String, String)> = arr(node.get("index"))
                .iter()
                .map(|j| (arr(j)[0].as_str().unwrap().to_owned(), arr(j)[1].as_str().unwrap().to_owned()))
                .collect();
            assert_eq!(jobs, want, "{name}: index work");
            checked += 1;
        }
        assert!(checked >= 30, "{checked}");
    }
}
