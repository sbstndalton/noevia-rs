//! `UI_DATA_DIR/users/<user id>/projects.json` as core workspace.cjs keeps it (`{ "projects": [...] }`,
//! written by `atomicJson`), read fresh for every request and changed only under the lock shared
//! with Node ([`crate::lock`]).

use crate::lock::{self, LockError};
use js_json::JValue;
use server_store::RustProjects;
use std::path::{Path, PathBuf};

pub const PROJECTS_FILE: &str = "projects.json";

#[derive(Debug)]
pub enum StoreError {
    /// Not a user id workspace.cjs `userDir` accepts (Node throws: a 500).
    InvalidUser,
    /// The file is not JSON, or its `projects` is not an array: never overwritten from here.
    Corrupt,
    /// The lock is held by a live writer past its wait.
    Busy,
    /// The user's directory is gone: the account was removed (core workspace.cjs `remove` deletes
    /// it) after the request passed the gate. Node's `assertActive` answer, a 410; nothing is
    /// written and the directory is not recreated (sbstndalton/noevia#1425).
    Gone,
    Io(std::io::Error),
    Write(server_store::json::JsonError),
}

impl From<std::io::Error> for StoreError {
    fn from(e: std::io::Error) -> Self {
        StoreError::Io(e)
    }
}

impl From<LockError> for StoreError {
    fn from(e: LockError) -> Self {
        match e {
            LockError::Busy => StoreError::Busy,
            LockError::Io(e) => StoreError::Io(e),
        }
    }
}

/// workspace.cjs `userDir`: `users/<id>` for an id matching `/^[0-9a-f-]{36}$/i` only, so a user id
/// can never name a path outside `users/`.
pub fn user_dir(data_dir: &Path, user_id: &str) -> Result<PathBuf, StoreError> {
    let ok = user_id.len() == 36 && user_id.bytes().all(|b| b.is_ascii_hexdigit() || b == b'-');
    if !ok {
        return Err(StoreError::InvalidUser);
    }
    Ok(data_dir.join("users").join(user_id))
}

/// workspace.cjs `assetDir(projectId)`: `project-assets/<id reduced to [A-Za-z0-9_-]>`.
pub fn asset_dir(user_dir: &Path, project_id: &str) -> PathBuf {
    let id = crate::js::id_chars(project_id);
    let base = user_dir.join("project-assets");
    if id.is_empty() {
        base // path.join(dir, 'project-assets', '')
    } else {
        base.join(id)
    }
}

/// The projects in `dir/projects.json`: none when the file is missing, or when it holds no
/// `projects` (Node's `readJson(...).projects || []`, and its reading in rust-projects.cjs).
pub fn read(dir: &Path) -> Result<Vec<JValue>, StoreError> {
    let text = match std::fs::read(dir.join(PROJECTS_FILE)) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(StoreError::Io(e)),
    };
    let text = String::from_utf8_lossy(&text);
    // Lossless: Node keeps lone surrogates (a chat preview cut mid-emoji) and writes them back.
    let parsed = js_json::wtf16::parse(&text).map_err(|_| StoreError::Corrupt)?;
    match parsed.get("projects") {
        JValue::Undefined | JValue::Null => Ok(Vec::new()),
        JValue::Arr(items) => {
            let mut projects = items.clone();
            drop_null_files(&mut projects);
            Ok(projects)
        }
        _ => Err(StoreError::Corrupt),
    }
}

/// workspace.cjs load (#1236): in each project whose `files` is an array, the entries that are not
/// objects (`f && typeof f === 'object'`: a `null` from a damaged projects.json, a number, a
/// string) are dropped, so no reader meets them. Node does it to the list it holds, so the next
/// save it makes writes the list without them; a write from here does the same.
fn drop_null_files(projects: &mut [JValue]) {
    for project in projects.iter_mut() {
        if let JValue::Obj(fields) = project {
            if let Some((_, JValue::Arr(files))) = fields.iter_mut().find(|(k, _)| k == "files") {
                files.retain(|f| matches!(f, JValue::Obj(_) | JValue::Arr(_)));
            }
        }
    }
}

/// The project with this id (`PROJECTS.find((p) => p.id === id)`).
pub fn find<'a>(projects: &'a [JValue], id: &str) -> Option<&'a JValue> {
    projects.iter().find(|p| p.get("id").as_str() == Some(id))
}

pub fn find_mut<'a>(projects: &'a mut [JValue], id: &str) -> Option<&'a mut JValue> {
    projects
        .iter_mut()
        .find(|p| p.get("id").as_str() == Some(id))
}

/// `project[key] = value`: replaced in place, or appended (JS property order).
pub fn set(project: &mut JValue, key: &str, value: JValue) {
    if let JValue::Obj(items) = project {
        if let Some(slot) = items.iter_mut().find(|(k, _)| k == key) {
            slot.1 = value;
        } else {
            items.push((key.to_string(), value));
        }
    }
}

/// The bytes `atomicJson(file, { projects })` writes.
pub fn serialise(projects: Vec<JValue>) -> String {
    js_json::wtf16::stringify_pretty(&JValue::obj([("projects", JValue::Arr(projects))]))
        .unwrap_or_default()
}

/// workspace.cjs `readJson(file, fallback)` for a file Node keeps beside projects.json (the free
/// chats, the chat tombstones): `None` where Node takes the fallback (missing, unreadable, not
/// JSON); [`StoreError::Corrupt`] where Node would read a value this port cannot hold, so a caller
/// never acts on less than Node would have seen.
pub fn read_json(file: &Path) -> Result<Option<JValue>, StoreError> {
    let Ok(bytes) = std::fs::read(file) else {
        return Ok(None);
    };
    match js_json::wtf16::parse(&String::from_utf8_lossy(&bytes)) {
        Ok(v) => Ok(Some(v)),
        Err(js_json::wtf16::Refusal::Invalid) => Ok(None),
        Err(js_json::wtf16::Refusal::Unsupported) => Err(StoreError::Corrupt),
    }
}

/// The user's directory, which a shared-file save never creates: [`StoreError::Gone`] when it is
/// missing (the account was removed while the request was in flight).
fn existing(dir: &Path) -> Result<(), StoreError> {
    match std::fs::symlink_metadata(dir) {
        Ok(m) if m.is_dir() => Ok(()),
        Ok(_) => Err(StoreError::Corrupt),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Err(StoreError::Gone),
        Err(e) => Err(StoreError::Io(e)),
    }
}

/// A lock or a write that found the user's directory gone meanwhile.
fn gone_if_missing(e: StoreError) -> StoreError {
    let missing = match &e {
        StoreError::Io(io) => io.kind() == std::io::ErrorKind::NotFound,
        StoreError::Write(server_store::json::JsonError::Io(io)) => {
            io.kind() == std::io::ErrorKind::NotFound
        }
        _ => false,
    };
    if missing {
        StoreError::Gone
    } else {
        e
    }
}

/// One read-modify-write of `dir/projects.json` under the shared lock: `change` gets the projects as
/// they are on disk now and returns its result and whether it changed them; only then is the
/// file written (Node's bytes, atomically). The user's directory must exist ([`StoreError::Gone`]).
pub fn update<T>(
    dir: &Path,
    switch: RustProjects,
    change: impl FnOnce(&mut Vec<JValue>) -> (T, bool),
) -> Result<T, StoreError> {
    existing(dir)?;
    let _held = lock::acquire(&dir.join(PROJECTS_FILE))
        .map_err(|e| gone_if_missing(StoreError::from(e)))?;
    let mut projects = read(dir)?;
    let (out, changed) = change(&mut projects);
    if changed {
        let text = serialise(projects);
        server_store::json::write_shared_projects(dir, PROJECTS_FILE, &text, switch)
            .map_err(|e| gone_if_missing(StoreError::Write(e)))?;
    }
    Ok(out)
}

/// One read-modify-write of `dir/free-chats.json` under its lock shared with Node
/// (`free-chats.json.lock`, core rust-projects.cjs `createFreeChatsFile`): `change` gets the file as
/// [`read_json`] reads it now and returns its result and, when the list is to be saved, the list;
/// that is written as `atomicJson(file, list)` writes it. The user's directory is never created
/// here ([`StoreError::Gone`] when it is missing): Node's workspace load makes it for every
/// signed-in user, and its removal means the account was deleted (sbstndalton/noevia#1425).
pub fn update_free<T>(
    dir: &Path,
    switch: RustProjects,
    change: impl FnOnce(Option<JValue>) -> (T, Option<Vec<JValue>>),
) -> Result<T, StoreError> {
    existing(dir)?;
    let file = dir.join(crate::chats::FREE_CHATS_FILE);
    let _held = lock::acquire(&file).map_err(|e| gone_if_missing(StoreError::from(e)))?;
    let stored = read_json(&file)?;
    let (out, list) = change(stored);
    if let Some(list) = list {
        let text = js_json::wtf16::stringify_pretty(&JValue::Arr(list)).unwrap_or_default();
        server_store::json::write_shared_projects(
            dir,
            crate::chats::FREE_CHATS_FILE,
            &text,
            switch,
        )
        .map_err(|e| gone_if_missing(StoreError::Write(e)))?;
    }
    Ok(out)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    fn on() -> RustProjects {
        RustProjects::from_env_value(Some("1")).unwrap()
    }

    #[test]
    fn user_dirs_are_uuid_shaped_only() {
        let root = Path::new("/data");
        assert_eq!(
            user_dir(root, "11111111-2222-4333-8444-555555555555").unwrap(),
            Path::new("/data/users/11111111-2222-4333-8444-555555555555")
        );
        for bad in [
            "",
            "..",
            "../../etc/passwd/aaaaaaaaaaaaaaaaaaaaaaaaa",
            "11111111-2222-4333-8444-55555555555/",
            "g1111111-2222-4333-8444-555555555555",
        ] {
            assert!(
                matches!(user_dir(root, bad), Err(StoreError::InvalidUser)),
                "{bad}"
            );
        }
        assert_eq!(
            asset_dir(Path::new("/u"), "proj-1/../x"),
            Path::new("/u/project-assets/proj-1x")
        );
        assert_eq!(
            asset_dir(Path::new("/u"), "../.."),
            Path::new("/u/project-assets")
        );
    }

    #[test]
    fn reads_like_node_and_refuses_to_overwrite_what_it_cannot_read() {
        let dir = tempfile::tempdir().unwrap();
        assert!(read(dir.path()).unwrap().is_empty());
        for (text, n) in [
            ("{}", 0),
            ("{\"projects\":null}", 0),
            ("5", 0),
            ("[]", 0),
            ("{\"projects\":[{\"id\":\"a\"}]}", 1),
        ] {
            std::fs::write(dir.path().join(PROJECTS_FILE), text).unwrap();
            assert_eq!(read(dir.path()).unwrap().len(), n, "{text}");
        }
        for text in [
            "{\"projects\":[",
            "{\"projects\":{}}",
            "{\"projects\":\"x\"}",
        ] {
            std::fs::write(dir.path().join(PROJECTS_FILE), text).unwrap();
            assert!(
                matches!(read(dir.path()), Err(StoreError::Corrupt)),
                "{text}"
            );
            assert!(update(dir.path(), on(), |p| {
                p.clear();
                ((), true)
            })
            .is_err());
            assert_eq!(
                std::fs::read_to_string(dir.path().join(PROJECTS_FILE)).unwrap(),
                text
            );
        }
    }

    #[test]
    fn update_writes_nodes_bytes_keeps_order_and_releases_the_lock() {
        let dir = tempfile::tempdir().unwrap();
        // As Node wrote it (JSON.stringify(value, null, 2)), with keys out of alphabetical order.
        let node = "{\n  \"projects\": [\n    {\n      \"name\": \"Zed\",\n      \"id\": \"p1\",\n      \"createdAt\": 1791600933849,\n      \"chats\": []\n    }\n  ]\n}";
        std::fs::write(dir.path().join(PROJECTS_FILE), node).unwrap();
        // A no-op change writes nothing.
        update(dir.path(), on(), |_| ((), false)).unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.path().join(PROJECTS_FILE)).unwrap(),
            node
        );
        update(dir.path(), on(), |p| {
            let proj = find_mut(p, "p1").unwrap();
            set(proj, "name", JValue::Str("Zed".into()));
            ((), true)
        })
        .unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.path().join(PROJECTS_FILE)).unwrap(),
            node
        );
        update(dir.path(), on(), |p| {
            set(&mut p[0], "assets", JValue::Arr(vec![]));
            ((), true)
        })
        .unwrap();
        let text = std::fs::read_to_string(dir.path().join(PROJECTS_FILE)).unwrap();
        assert!(
            text.ends_with("      \"chats\": [],\n      \"assets\": []\n    }\n  ]\n}"),
            "{text}"
        );
        // The lock file is long-lived (flock on it; never unlinked).
        assert!(dir.path().join("projects.json.lock").exists());
        let names: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names.len(), 2, "no temp file left: {names:?}");
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod free_tests {
    use super::*;

    #[test]
    fn update_free_writes_atomic_json_and_only_when_asked() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("users").join("u");
        std::fs::create_dir_all(&dir).unwrap();
        let on = RustProjects::from_env_value(Some("1")).unwrap();
        let seen = update_free(&dir, on, |stored| (stored, None)).unwrap();
        assert_eq!(seen, None, "missing: readJson's fallback");
        assert!(!dir.join("free-chats.json").exists(), "nothing written");
        update_free(&dir, on, |_| ((), Some(Vec::new()))).unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.join("free-chats.json")).unwrap(),
            "[]"
        );
        let one = JValue::obj([
            ("id", JValue::Str("a".into())),
            ("updatedAt", JValue::Num(1.0)),
        ]);
        update_free(&dir, on, |_| ((), Some(vec![one]))).unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.join("free-chats.json")).unwrap(),
            "[\n  {\n    \"id\": \"a\",\n    \"updatedAt\": 1\n  }\n]"
        );
        assert!(dir.join("free-chats.json.lock").exists(), "long-lived lock");
        std::fs::write(dir.join("free-chats.json"), "{nope").unwrap();
        assert_eq!(update_free(&dir, on, |s| (s, None)).unwrap(), None);
    }

    /// sbstndalton/noevia#1236 / #1621: a damaged projects.json with non-object `files` entries
    /// reads without them, as core's workspace load holds it; the next write leaves them out.
    #[test]
    fn null_file_entries_are_dropped_on_read() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path();
        std::fs::write(
            dir.join(PROJECTS_FILE),
            r#"{"projects":[{"id":"p1","files":[null,{"name":"a.md","content":"x"},7,"s",false]},{"id":"p2","files":"abc"},null,{"id":"p3"}]}"#,
        )
        .unwrap();
        let projects = read(dir).unwrap();
        assert_eq!(
            js_json::wtf16::stringify(&JValue::Arr(projects)).unwrap(),
            r#"[{"id":"p1","files":[{"name":"a.md","content":"x"}]},{"id":"p2","files":"abc"},null,{"id":"p3"}]"#
        );
        let on = RustProjects::from_env_value(Some("1")).unwrap();
        update(dir, on, |_| ((), true)).unwrap();
        let text = std::fs::read_to_string(dir.join(PROJECTS_FILE)).unwrap();
        let saved = js_json::wtf16::parse(&text).unwrap();
        let saved = match saved.get("projects") {
            JValue::Arr(items) => items.clone(),
            _ => Vec::new(),
        };
        let files = saved.first().unwrap().get("files");
        assert_eq!(
            js_json::wtf16::stringify(files).unwrap(),
            r#"[{"name":"a.md","content":"x"}]"#,
            "{text}"
        );
    }

    /// sbstndalton/noevia#1425: a removed account's directory is never recreated, by either save.
    #[test]
    fn a_missing_user_dir_is_gone_and_stays_gone() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("users").join("u");
        let on = RustProjects::from_env_value(Some("1")).unwrap();
        let mut ran = false;
        let r = update_free(&dir, on, |_| {
            ran = true;
            ((), Some(Vec::new()))
        });
        assert!(matches!(r, Err(StoreError::Gone)), "{r:?}");
        let r = update(&dir, on, |_| ((), true));
        assert!(matches!(r, Err(StoreError::Gone)), "{r:?}");
        assert!(!ran, "nothing read or changed");
        assert!(!root.path().join("users").exists(), "not recreated");
        // Removed after the lock was taken: the write fails, nothing comes back.
        std::fs::create_dir_all(&dir).unwrap();
        let r = update_free(&dir, on, |_| {
            std::fs::remove_dir_all(&dir).unwrap();
            ((), Some(Vec::new()))
        });
        assert!(matches!(r, Err(StoreError::Gone)), "{r:?}");
        assert!(!dir.exists(), "not recreated");
        std::fs::create_dir_all(&dir).unwrap();
        let r = update(&dir, on, |_| {
            std::fs::remove_dir_all(&dir).unwrap();
            ((), true)
        });
        assert!(matches!(r, Err(StoreError::Gone)), "{r:?}");
        assert!(!dir.exists(), "not recreated");
    }
}
