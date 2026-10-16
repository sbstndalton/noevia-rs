//! The project store Node writes through (sbstndalton/noevia#1264, option 2 of the lead's
//! 2026-10-16 decision): with NOEVIA_RUST_PROJECT_STORE=1 Node no longer writes `projects.json` or
//! `free-chats.json` itself, and no longer takes their flock with a blocking `spawnSync(flock)`.
//! It reads the file and its version here, merges its own changes into it as before (core
//! rust-projects.cjs `merge` / `mergeFreeChats`), and asks this module to write the result only if
//! the file is still at that version: a compare-and-swap. A conflict hands back the current file so
//! Node merges again (a bounded number of times). Rust's own routes write the same files under the
//! same lock ([`crate::lock`]), so Rust is the single writer of both.
//!
//! - The version is the SHA-256 of the file's bytes (hex), or `""` when the file does not exist.
//! - A write is refused unless its text is what Node's `atomicJson` would hold for that file
//!   (`{ "projects": [...] }`, or a JSON array), at most [`TEXT_LIMIT`] bytes.
//! - Under the lock: the file is read, its version compared, and the text written atomically
//!   (temp file, fsync, rename: `server_store::json::write_shared_projects`).
//! - The user's directory is never created ([`StoreError::Gone`]: the account was removed).

use crate::lock;
use crate::store::{self, StoreError};
use js_json::JValue;
use std::path::Path;

/// The largest file text accepted (Node's projects.json holds no source text since #1806).
pub const TEXT_LIMIT: usize = 32 * 1024 * 1024;

/// The two files Node writes through here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StoreFile {
    Projects,
    FreeChats,
}

impl StoreFile {
    pub fn parse(name: &str) -> Option<Self> {
        match name {
            store::PROJECTS_FILE => Some(StoreFile::Projects),
            crate::chats::FREE_CHATS_FILE => Some(StoreFile::FreeChats),
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            StoreFile::Projects => store::PROJECTS_FILE,
            StoreFile::FreeChats => crate::chats::FREE_CHATS_FILE,
        }
    }
}

/// The file as it is: its version and text (`None` when it does not exist).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Snapshot {
    pub version: String,
    pub text: Option<String>,
}

#[derive(Debug)]
pub enum WriteError {
    /// The file changed since `version`: the current one, to merge into.
    Conflict(Snapshot),
    /// The text is not a file of this kind, or too long.
    Invalid(&'static str),
    Store(StoreError),
}

impl From<StoreError> for WriteError {
    fn from(e: StoreError) -> Self {
        WriteError::Store(e)
    }
}

/// SHA-256 hex of `bytes`.
pub fn version_of(bytes: &[u8]) -> String {
    use sha2::Digest;
    let digest = sha2::Sha256::digest(bytes);
    let mut out = String::with_capacity(64);
    for b in digest {
        out.push_str(&format!("{b:02x}"));
    }
    out
}

fn snapshot(file: &Path) -> Result<Snapshot, StoreError> {
    match std::fs::read(file) {
        Ok(bytes) => Ok(Snapshot {
            version: version_of(&bytes),
            text: Some(String::from_utf8_lossy(&bytes).into_owned()),
        }),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Snapshot {
            version: String::new(),
            text: None,
        }),
        Err(e) => Err(StoreError::Io(e)),
    }
}

fn existing_dir(dir: &Path) -> Result<(), StoreError> {
    match std::fs::symlink_metadata(dir) {
        Ok(m) if m.is_dir() => Ok(()),
        Ok(_) => Err(StoreError::Corrupt),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Err(StoreError::Gone),
        Err(e) => Err(StoreError::Io(e)),
    }
}

/// The file now. No lock: every write is a rename, so a read sees one whole version.
pub fn read(data_dir: &Path, user_id: &str, which: StoreFile) -> Result<Snapshot, StoreError> {
    let dir = store::user_dir(data_dir, user_id)?;
    existing_dir(&dir)?;
    snapshot(&dir.join(which.name()))
}

fn valid(which: StoreFile, text: &str) -> Result<(), &'static str> {
    if text.len() > TEXT_LIMIT {
        return Err("text too long");
    }
    let parsed = js_json::wtf16::parse(text).map_err(|_| "text is not JSON")?;
    let ok = match which {
        StoreFile::Projects => matches!(parsed.get("projects"), JValue::Arr(_)),
        StoreFile::FreeChats => matches!(parsed, JValue::Arr(_)),
    };
    ok.then_some(()).ok_or(match which {
        StoreFile::Projects => "projects.json must be { \"projects\": [...] }",
        StoreFile::FreeChats => "free-chats.json must be a list",
    })
}

/// Writes `text` if the file is still at `version`; returns the new version.
pub fn write(
    data_dir: &Path,
    user_id: &str,
    which: StoreFile,
    version: &str,
    text: &str,
    switch: server_store::RustProjects,
) -> Result<String, WriteError> {
    valid(which, text).map_err(WriteError::Invalid)?;
    let dir = store::user_dir(data_dir, user_id)?;
    existing_dir(&dir)?;
    let file = dir.join(which.name());
    let _held = lock::acquire(&file).map_err(StoreError::from)?;
    let now = snapshot(&file)?;
    if now.version != version {
        return Err(WriteError::Conflict(now));
    }
    server_store::json::write_shared_projects(&dir, which.name(), text, switch)
        .map_err(|e| WriteError::Store(StoreError::Write(e)))?;
    Ok(version_of(text.as_bytes()))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;

    const USER: &str = "11111111-2222-4333-8444-555555555555";

    fn on() -> server_store::RustProjects {
        server_store::RustProjects::from_env_value(Some("1")).unwrap()
    }

    fn ws() -> tempfile::TempDir {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("users").join(USER)).unwrap();
        root
    }

    #[test]
    fn compare_and_swap_writes_only_from_the_current_version() {
        let root = ws();
        let p = StoreFile::Projects;
        let s = read(root.path(), USER, p).unwrap();
        assert_eq!(
            s,
            Snapshot {
                version: String::new(),
                text: None
            }
        );
        let v1 = write(root.path(), USER, p, "", "{\"projects\":[]}", on()).unwrap();
        assert_eq!(read(root.path(), USER, p).unwrap().version, v1);
        // A stale writer gets the current file back and writes nothing.
        match write(
            root.path(),
            USER,
            p,
            "",
            "{\"projects\":[{\"id\":\"x\"}]}",
            on(),
        ) {
            Err(WriteError::Conflict(now)) => {
                assert_eq!(now.version, v1);
                assert_eq!(now.text.as_deref(), Some("{\"projects\":[]}"));
            }
            other => panic!("{other:?}"),
        }
        let v2 = write(
            root.path(),
            USER,
            p,
            &v1,
            "{\"projects\":[{\"id\":\"x\"}]}",
            on(),
        )
        .unwrap();
        assert_ne!(v1, v2);
    }

    #[test]
    fn only_files_of_their_kind_are_written() {
        let root = ws();
        for (which, text) in [
            (StoreFile::Projects, "[]"),
            (StoreFile::Projects, "{\"projects\":{}}"),
            (StoreFile::Projects, "{oops"),
            (StoreFile::FreeChats, "{\"projects\":[]}"),
            (StoreFile::FreeChats, "null"),
        ] {
            assert!(
                matches!(
                    write(root.path(), USER, which, "", text, on()),
                    Err(WriteError::Invalid(_))
                ),
                "{which:?} {text}"
            );
        }
        assert!(write(root.path(), USER, StoreFile::FreeChats, "", "[]", on()).is_ok());
        assert_eq!(StoreFile::parse("../projects.json"), None);
        assert_eq!(StoreFile::parse("cowork.db"), None);
    }

    #[test]
    fn a_removed_account_or_a_bad_user_writes_nothing() {
        let root = tempfile::tempdir().unwrap();
        let w = write(
            root.path(),
            USER,
            StoreFile::Projects,
            "",
            "{\"projects\":[]}",
            on(),
        );
        assert!(matches!(w, Err(WriteError::Store(StoreError::Gone))));
        assert!(!root.path().join("users").exists());
        let w = write(
            root.path(),
            "../x",
            StoreFile::Projects,
            "",
            "{\"projects\":[]}",
            on(),
        );
        assert!(matches!(w, Err(WriteError::Store(StoreError::InvalidUser))));
    }

    /// Many writers, each retrying its read-modify-write on conflict: every append survives.
    #[test]
    fn concurrent_compare_and_swap_loses_no_update() {
        let root = ws();
        let p = StoreFile::Projects;
        write(root.path(), USER, p, "", "{\"projects\":[]}", on()).unwrap();
        std::thread::scope(|s| {
            for t in 0..8 {
                let root = root.path();
                s.spawn(move || {
                    for i in 0..10 {
                        loop {
                            let snap = read(root, USER, p).unwrap();
                            let mut v =
                                js_json::wtf16::parse(snap.text.as_deref().unwrap()).unwrap();
                            if let JValue::Obj(fields) = &mut v {
                                if let Some((_, JValue::Arr(list))) = fields.first_mut() {
                                    list.push(JValue::Str(format!("{t}-{i}")));
                                }
                            }
                            let text = js_json::wtf16::stringify(&v).unwrap();
                            match write(root, USER, p, &snap.version, &text, on()) {
                                Ok(_) => break,
                                Err(WriteError::Conflict(_)) => continue,
                                Err(e) => panic!("{e:?}"),
                            }
                        }
                    }
                });
            }
        });
        let text = read(root.path(), USER, p).unwrap().text.unwrap();
        let v = js_json::wtf16::parse(&text).unwrap();
        let JValue::Arr(list) = v.get("projects") else {
            panic!()
        };
        assert_eq!(list.len(), 80);
    }
}
