//! A project's own folder in the user's remote storage (sbstndalton/noevia#2191 round 3): what a
//! create and a delete do there, now that the front has a storage write client.
//!
//! - [`store_create_files`]: core projects.cjs `storeCreateFiles` (#687). With browsable storage,
//!   the text files that arrived with the create call are stored like an upload: the project's
//!   folder is allocated (project-folders.cjs `allocateFolder`: the root, then the reserved path,
//!   then `<key> (n)`, MKCOL deciding), attached as a source, and each storable file (a plain name of
//!   the Text group that upload-sniff `validate` accepts) is written create-only (`If-None-Match:
//!   *`) to `<folder>/Text/<name>`, its original kept in `project-uploads/<sha256 id>/<sha256>`
//!   (uploads.cjs `ingest`). A file storage refuses stays noevia's plain-named copy, as in core.
//! - [`sweep_pending`]: the `sweep` jobs a delete leaves in the outbox, run here (project-sweep.cjs
//!   `sweepRemote`): only a folder directly under the projects root, its type groups then itself,
//!   each removed only while empty (PROPFIND Depth 1, then DELETE with the collection's ETag).
//!
//! The storage I/O is behind [`ProjectStorage`] (noevia-server implements it on the owned child
//! transport of server-storage-reads, after its endpoint policy and admission), so this crate keeps
//! no network code.
use crate::js;
use crate::lifecycle;
use crate::outbox;
use crate::store;
use crate::Ctx;
use js_json::{wtf16, JValue};
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// uploads.cjs GROUPS.
pub const GROUPS: [&str; 4] = ["Documents", "Images", "Text", "Other"];

/// The storage writes a project's folder needs (storage-client.cjs semantics). Errors are already
/// secret-free text for the log.
pub trait ProjectStorage {
    /// `createFolder`: `Ok(true)` when it already existed (MKCOL 405).
    fn create_folder(&self, path: &str) -> Result<bool, String>;
    /// `writeFile(..., { ifNoneMatch: '*' })`.
    fn write_new(&self, path: &str, bytes: &[u8]) -> Result<(), String>;
    /// `removeEmptyFolder`: (removed, reason).
    fn remove_empty_folder(&self, path: &str) -> Result<(bool, &'static str), String>;
    /// `deleteFile`: `Ok(missing)`.
    fn delete_file(&self, path: &str) -> Result<bool, FileError>;
}

/// A storage write's failure as a route answers it: its status and message, and its
/// storage-client.cjs `storageFailure` kind (`login`, `forbidden`, `throttled`, `timeout`,
/// `unreachable`) when it has one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileError {
    pub status: u16,
    pub message: String,
    pub kind: Option<&'static str>,
}

/// One step run with the user's storage ([`OpenRemote`]).
pub type StorageStep<'b> = dyn FnMut(&Remote<'_>) + 'b;
/// Opens the user's storage and runs a step with it (noevia-server's opener).
pub type OpenRemote<'a> = dyn Fn(&mut StorageStep<'_>) + 'a;

/// What the user's storage is, for the steps that depend on it.
pub enum Remote<'a> {
    /// Not browsable (local storage) or no connection could be read: nothing to do.
    None,
    /// S3: no folders and no writes (core: the folder path is named, every write is refused).
    S3,
    Dav(&'a dyn ProjectStorage),
    /// Storage is there but cannot be used now (the endpoint policy, no write slot): a create keeps
    /// its files in noevia, a sweep job stays queued, a route answers (status, message).
    Refused(u16, &'static str),
}

fn sha_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// source-limits.cjs `limits().sources`: PROJECT_SOURCE_LIMIT when a positive safe integer, else
/// contracts/folder-import.json's 1000.
fn source_limit() -> usize {
    std::env::var("PROJECT_SOURCE_LIMIT")
        .ok()
        .and_then(|v| js::trim(&v).parse::<u64>().ok())
        .filter(|n| *n > 0 && *n < (1 << 53))
        .and_then(|n| usize::try_from(n).ok())
        .unwrap_or(1000)
}

/// uploads.cjs `ingest`'s original copy: `project-uploads/<sha256 id>/<sha256 bytes>`, 0600 in 0700,
/// written to a temp name and renamed.
pub(crate) fn save_original(
    dir: &Path,
    project_id: &str,
    id: &str,
    bytes: &[u8],
) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
    let folder = lifecycle::uploads_dir(dir, project_id);
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&folder)?;
    let tmp = folder.join(format!("{id}.{}.{}", std::process::id(), js::random36(8)));
    {
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp, folder.join(id))
}

/// project-folders.cjs `allocateFolder`, logged and `None` on any failure (ensureProjectFolder).
fn allocate(remote: &Remote<'_>, root: &str, id: &str, reserved: Option<&str>) -> Option<String> {
    let key = lifecycle::folder_key(id);
    let storage = match remote {
        Remote::None | Remote::Refused(..) => return None,
        Remote::S3 => return Some(reserved.map_or_else(|| format!("{root}/{key}"), str::to_owned)),
        Remote::Dav(s) => *s,
    };
    let fail = |e: String| {
        eprintln!("[projects] could not create folder for project {id}: {e}");
        None
    };
    if let Err(e) = storage.create_folder(root) {
        return fail(e);
    }
    if let Some(reserved) = reserved {
        match storage.create_folder(reserved) {
            Ok(false) => return Some(reserved.to_owned()),
            Ok(true) => {}
            Err(e) => return fail(e),
        }
    }
    for n in 1..=1000 {
        let folder = if n == 1 {
            format!("{root}/{key}")
        } else {
            format!("{root}/{key} ({n})")
        };
        match storage.create_folder(&folder) {
            Ok(false) => return Some(folder),
            Ok(true) => {}
            Err(e) => return fail(e),
        }
    }
    fail("Too many folders with this project name".into())
}

/// The storable create-time files: a plain name (no `/`) of the Text group that `validate` accepts.
fn storable(name: &str, content: &str) -> bool {
    !name.contains('/')
        && upload_sniff::classify(name) == "Text"
        && upload_sniff::validate(name, content.len() as u64, content.as_bytes()).is_none()
}

/// projects.cjs `storeCreateFiles(project)` after the create's save. `inline` are the create-time
/// files as the request sent them (name, text). Returns the project as saved last (`None` when
/// nothing changed or it is gone).
pub fn store_create_files(
    ctx: Ctx<'_>,
    dir: &Path,
    project_id: &str,
    inline: &[(String, String)],
    remote: &Remote<'_>,
    root: &str,
) -> Option<JValue> {
    if matches!(remote, Remote::None | Remote::Refused(..)) {
        return None;
    }
    let pending: Vec<&(String, String)> = inline.iter().filter(|(n, c)| storable(n, c)).collect();
    if pending.is_empty() {
        return None;
    }
    let current = store::read(dir).ok()?;
    let project = store::find(&current, project_id)?;
    let mut folder = project
        .get("projectFolder")
        .as_str()
        .filter(|f| !f.is_empty())
        .map(str::to_owned);
    let mut saved = None;
    if folder.is_none() {
        let reserved = project
            .get("reservedFolder")
            .as_str()
            .filter(|f| !f.is_empty())
            .map(str::to_owned);
        let made = allocate(remote, root, project_id, reserved.as_deref())?;
        let attached = store::update(dir, ctx.switch, |projects| {
            let Some(p) = store::find_mut(projects, project_id) else {
                return (None, false);
            };
            store::set(p, "projectFolder", JValue::Str(made.clone()));
            let mut sources: Vec<JValue> = match p.get("sourceFolders") {
                JValue::Arr(a) => a.clone(),
                _ => Vec::new(),
            };
            if !sources.iter().any(|s| s.as_str() == Some(made.as_str())) {
                sources.push(JValue::Str(made.clone()));
            }
            store::set(p, "sourceFolders", JValue::Arr(sources));
            (Some(p.clone()), true)
        });
        match attached {
            Ok(Some(p)) => saved = Some(p),
            Ok(None) => return None,
            Err(e) => {
                eprintln!("[projects] storing the files of new project {project_id} failed; they stay in noevia: {e:?}");
                return None;
            }
        }
        folder = Some(made);
    }
    let folder = folder?;
    let limit = source_limit();
    let mut count = inline.len();
    let mut stored: Vec<(String, JValue)> = Vec::new();
    for (name, content) in pending {
        let warn = |e: &str| {
            eprintln!("[projects] could not store a file of new project {project_id} in storage; it stays in noevia: {e}");
        };
        let full = format!("{folder}/Text/{name}");
        if count >= limit {
            warn(&format!("A project holds at most {limit} sources."));
            continue;
        }
        let bytes = content.as_bytes();
        let id = sha_hex(bytes);
        if let Err(e) = save_original(dir, project_id, &id, bytes) {
            warn(&format!("{}", e.kind()));
            continue;
        }
        let storage = match remote {
            Remote::Dav(s) => *s,
            _ => {
                warn("S3 has no folders — a prefix appears once a file is stored under it");
                continue;
            }
        };
        if let Err(e) = storage.create_folder(&format!("{folder}/Text")) {
            warn(&e);
            continue;
        }
        if let Err(e) = storage.write_new(&full, bytes) {
            warn(&e);
            continue;
        }
        let state = if bytes.len() > 200_000 {
            "partial"
        } else {
            "ready"
        };
        let entry = JValue::Obj(vec![
            ("name".into(), JValue::Str(full)),
            ("content".into(), JValue::Str(content.clone())),
            (
                "attachment".into(),
                JValue::Obj(vec![
                    ("id".into(), JValue::Str(id)),
                    ("bytes".into(), JValue::Num(bytes.len() as f64)),
                    ("group".into(), JValue::Str("Text".into())),
                    ("state".into(), JValue::Str(state.into())),
                ]),
            ),
            ("source".into(), JValue::Str(folder.clone())),
        ]);
        stored.push((name.clone(), entry));
        count += 1;
    }
    let done = store::update(dir, ctx.switch, |projects| {
        let Some(p) = store::find_mut(projects, project_id) else {
            return (None, false);
        };
        let mut files: Vec<JValue> = match p.get("files") {
            JValue::Arr(a) => a.clone(),
            _ => Vec::new(),
        };
        for (plain, entry) in &stored {
            let full = entry.get("name").as_str().unwrap_or("").to_owned();
            // The plain-named create-time entry (never one with a source) becomes the stored file;
            // any other entry already named like it goes.
            let is_inline = |f: &JValue| {
                f.get("name").as_str() == Some(plain.as_str()) && !f.get("source").truthy()
            };
            files = files
                .into_iter()
                .filter(|f| is_inline(f) || f.get("name").as_str() != Some(full.as_str()))
                .map(|f| if is_inline(&f) { entry.clone() } else { f })
                .collect();
        }
        let mut changed = JValue::Obj(vec![
            ("id".into(), JValue::Str(project_id.into())),
            ("files".into(), JValue::Arr(files)),
        ]);
        if lifecycle::externalize(dir, &mut changed).is_err() {
            return (None, false);
        }
        if let JValue::Arr(files) = changed.get("files").clone() {
            store::set(p, "files", JValue::Arr(files));
        }
        store::set(p, "updatedAt", JValue::Num(ctx.now_ms as f64));
        if !lifecycle::reconcile_all(projects) {
            return (None, false);
        }
        let saved = store::find(projects, project_id).cloned();
        (saved, true)
    });
    match done {
        Ok(Some(p)) => Some(p),
        Ok(None) => saved,
        Err(e) => {
            eprintln!("[projects] storing the files of new project {project_id} failed; they stay in noevia: {e:?}");
            saved
        }
    }
}

fn clean_slashes(s: &str) -> &str {
    s.trim_start_matches('/').trim_end_matches('/')
}

/// project-sweep.cjs `sweepRemote(connection, folder, { root, groups })`: (dir, removed, reason).
pub fn sweep_remote(
    storage: &dyn ProjectStorage,
    folder: &str,
    root: &str,
) -> Vec<(String, bool, &'static str)> {
    let clean = clean_slashes(folder);
    let clean_root = clean_slashes(root);
    let parent = clean.rsplit_once('/').map_or(".", |(p, _)| p);
    if folder.is_empty()
        || clean
            .split('/')
            .any(|s| s.is_empty() || s == "." || s == "..")
        || clean_root.is_empty()
        || parent != clean_root
    {
        return vec![(clean.to_owned(), false, "outside-projects-root")];
    }
    let mut out = Vec::new();
    for target in GROUPS
        .iter()
        .map(|g| format!("{clean}/{g}"))
        .chain(std::iter::once(clean.to_owned()))
    {
        match storage.remove_empty_folder(&target) {
            Ok((removed, reason)) => out.push((target, removed, reason)),
            Err(_) => out.push((target, false, "error")),
        }
    }
    out
}

/// The users whose `running` sweep jobs this process has taken back (once per process each).
static RECOVERED: Mutex<BTreeSet<PathBuf>> = Mutex::new(BTreeSet::new());

/// Runs the user's pending `sweep` jobs (a delete's storage sweep; after a restart, the ones it
/// left). With no browsable storage the job is done with nothing to remove, as core's afterDelete
/// with no connection. Every result but `missing` is logged as core logs it.
pub fn sweep_pending(dir: &Path, remote: &Remote<'_>, root: &str, now_ms: i64) {
    let recover = RECOVERED
        .lock()
        .map(|mut seen| seen.insert(dir.to_path_buf()))
        .unwrap_or(false);
    let result = outbox::drain_kind(dir, "sweep", recover, now_ms, |job| {
        let folder = job
            .payload
            .as_deref()
            .and_then(|p| wtf16::parse(p).ok())
            .and_then(|v| v.get("folder").as_str().map(str::to_owned))
            .unwrap_or_default();
        if let Remote::Dav(storage) = remote {
            if !folder.is_empty() {
                for (target, removed, reason) in sweep_remote(*storage, &folder, root) {
                    if reason == "missing" {
                        continue;
                    }
                    let mut line = String::from("{\"event\":\"project.sweep\",\"projectId\":");
                    js_json::quote(&job.project, &mut line);
                    line.push_str(",\"dir\":");
                    js_json::quote(&target, &mut line);
                    line.push_str(if removed {
                        ",\"removed\":true"
                    } else {
                        ",\"removed\":false"
                    });
                    if !reason.is_empty() {
                        line.push_str(",\"reason\":");
                        js_json::quote(reason, &mut line);
                    }
                    line.push('}');
                    eprintln!("[projects] {line}");
                }
            }
        }
        true
    });
    if let Err(e) = result {
        eprintln!("noevia-server: the storage sweep could not read the outbox: {e:?}");
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing, clippy::panic)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    #[derive(Default)]
    struct Fake {
        dirs: RefCell<BTreeSet<String>>,
        files: RefCell<Vec<(String, Vec<u8>)>>,
        refuse_writes: bool,
        calls: RefCell<Vec<String>>,
    }
    impl ProjectStorage for Fake {
        fn create_folder(&self, path: &str) -> Result<bool, String> {
            self.calls.borrow_mut().push(format!("MKCOL {path}"));
            Ok(!self.dirs.borrow_mut().insert(path.to_owned()))
        }
        fn write_new(&self, path: &str, bytes: &[u8]) -> Result<(), String> {
            self.calls.borrow_mut().push(format!("PUT {path}"));
            if self.refuse_writes {
                return Err("could not write (507)".into());
            }
            self.files
                .borrow_mut()
                .push((path.to_owned(), bytes.to_vec()));
            Ok(())
        }
        fn delete_file(&self, path: &str) -> Result<bool, FileError> {
            self.calls.borrow_mut().push(format!("DELETE {path}"));
            Ok(false)
        }
        fn remove_empty_folder(&self, path: &str) -> Result<(bool, &'static str), String> {
            self.calls.borrow_mut().push(format!("SWEEP {path}"));
            Ok(if path.ends_with("Text") {
                (false, "not-empty")
            } else {
                (false, "missing")
            })
        }
    }

    #[test]
    fn allocation_takes_the_reservation_then_the_next_free_name() {
        let f = Fake::default();
        let r = Remote::Dav(&f);
        assert_eq!(
            allocate(&r, "root", "proj-1", Some("root/proj-1")).as_deref(),
            Some("root/proj-1")
        );
        // Taken now: the reservation exists, so the numbered names follow.
        assert_eq!(
            allocate(&r, "root", "proj-1", Some("root/proj-1")).as_deref(),
            Some("root/proj-1 (2)")
        );
        assert_eq!(
            allocate(&Remote::S3, "root", "proj-1", None).as_deref(),
            Some("root/proj-1")
        );
        assert_eq!(allocate(&Remote::None, "root", "proj-1", None), None);
    }

    #[test]
    fn only_plain_text_files_are_storable() {
        assert!(storable("a.md", "x"));
        assert!(!storable("a.md", ""));
        assert!(!storable("d/a.md", "x"));
        assert!(!storable("a.pdf", "x"));
        assert!(!storable("a.zip", "x"));
        assert!(!storable("a.bin", "x"));
    }

    #[test]
    fn the_sweep_stays_inside_the_projects_root() {
        let f = Fake::default();
        for bad in ["", "root", "other/p", "root/a/b", "root/..", "root//p"] {
            let r = sweep_remote(&f, bad, "root");
            assert_eq!(r.len(), 1, "{bad}");
            assert_eq!(r[0].2, "outside-projects-root");
        }
        assert!(f.calls.borrow().is_empty());
        let r = sweep_remote(&f, "/root/p/", "/root/");
        assert_eq!(r.len(), 5);
        assert_eq!(
            f.calls.borrow().clone(),
            ["Documents", "Images", "Text", "Other"]
                .iter()
                .map(|g| format!("SWEEP root/p/{g}"))
                .chain(std::iter::once("SWEEP root/p".to_string()))
                .collect::<Vec<_>>()
        );
    }
}
