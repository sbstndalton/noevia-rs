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
    let parsed = js_json::parse(&text).map_err(|_| StoreError::Corrupt)?;
    match parsed.get("projects") {
        JValue::Undefined | JValue::Null => Ok(Vec::new()),
        JValue::Arr(items) => Ok(items.clone()),
        _ => Err(StoreError::Corrupt),
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
    js_json::stringify_pretty(&JValue::obj([("projects", JValue::Arr(projects))]))
        .unwrap_or_default()
}

/// One read-modify-write of `dir/projects.json` under the shared lock: `change` gets the projects as
/// they are on disk now and returns its result and whether it changed them; only then is the
/// file written (Node's bytes, atomically).
pub fn update<T>(
    dir: &Path,
    switch: RustProjects,
    change: impl FnOnce(&mut Vec<JValue>) -> (T, bool),
) -> Result<T, StoreError> {
    let _held = lock::acquire(&dir.join(PROJECTS_FILE))?;
    let mut projects = read(dir)?;
    let (out, changed) = change(&mut projects);
    if changed {
        let text = serialise(projects);
        server_store::json::write_shared_projects(dir, PROJECTS_FILE, &text, switch)
            .map_err(StoreError::Write)?;
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
        assert!(!dir.path().join("projects.json.lock").exists());
        let names: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names.len(), 1, "no temp or lock file left: {names:?}");
    }
}
