//! The user's connected storage as a source sync and an organized upload use it (sbstndalton/noevia#2191
//! round 8): core storage-client.cjs `listFiles`, `readTextFile`, `readBinaryFile`, `createFolder`,
//! `writeFile` and `discardWritten`, behind a trait so this crate keeps no network code
//! (bins/noevia-server implements it on the owned child transport, after the endpoint policy and
//! admission; the tests use a synthetic one).
//!
//! - [`StorageError`] is core's thrown storage error (`status`, `message`, `code`, `upstream`) and
//!   [`StorageError::failure`] its `storageFailure(err)`: the kind a route words (`login`,
//!   `forbidden`, `throttled`, `timeout`, `unreachable`) or none.
//! - [`Storage`] is what the account's storage is for one operation: none (local; nothing browsable), or
//!   refused for now (no admission slot, the endpoint policy), or a [`SourceStorage`].
//! - [`allocate_folder`] is project-folders.cjs `allocateFolder`.
use crate::lifecycle;
use std::time::Duration;

/// storage-client.cjs's thrown error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StorageError {
    /// The route's answer (404, 413, 502, ...).
    pub status: u16,
    pub message: String,
    /// `timeout`, `unreachable`, `changed` (a 412 on a conditional write) or `unknown`.
    pub code: Option<String>,
    /// The status storage itself gave.
    pub upstream: Option<u16>,
    /// A 429's `Retry-After` in seconds, when storage said.
    pub retry_after: Option<u64>,
}

impl StorageError {
    pub fn new(status: u16, message: impl Into<String>) -> Self {
        StorageError {
            status,
            message: message.into(),
            code: None,
            upstream: None,
            retry_after: None,
        }
    }
    pub fn with_code(mut self, code: &str) -> Self {
        self.code = Some(code.to_owned());
        self
    }
    pub fn with_upstream(mut self, status: u16) -> Self {
        self.upstream = Some(status);
        self
    }
    /// `storageFailure(err)`.
    pub fn failure(&self) -> Option<Failure> {
        match self.code.as_deref() {
            Some("timeout") => {
                return Some(Failure {
                    kind: "timeout",
                    retry_after: None,
                })
            }
            Some("unreachable") => {
                return Some(Failure {
                    kind: "unreachable",
                    retry_after: None,
                })
            }
            _ => {}
        }
        let status = self.upstream.or_else(|| {
            self.message
                .strip_prefix("storage returned ")
                .filter(|s| s.len() == 3 && s.bytes().all(|b| b.is_ascii_digit()))
                .and_then(|s| s.parse().ok())
        });
        match status {
            Some(401) => Some(Failure {
                kind: "login",
                retry_after: None,
            }),
            Some(403) => Some(Failure {
                kind: "forbidden",
                retry_after: None,
            }),
            Some(429) => Some(Failure {
                kind: "throttled",
                retry_after: self.retry_after.filter(|n| *n > 0),
            }),
            _ => None,
        }
    }
    /// `pathTaken(e)`: a create-only write that met a 412.
    pub fn path_taken(&self) -> bool {
        self.code.as_deref() == Some("changed") && self.status == 409
    }
}

impl std::fmt::Display for StorageError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

/// `{ kind, retryAfter? }`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Failure {
    pub kind: &'static str,
    pub retry_after: Option<u64>,
}

/// A listed entry, as `listFiles` gives it.
#[derive(Debug, Clone, PartialEq)]
pub struct Entry {
    pub name: String,
    /// Connection-absolute.
    pub path: String,
    pub is_dir: bool,
    /// The listed size (`null` when storage gave none).
    pub size: Option<f64>,
    /// The lower-cased extension from the last dot (`.pdf`), `""` for a folder.
    pub ext: String,
}

/// `readTextFile`: the text up to 200 000 UTF-16 units.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextRead {
    pub content: String,
    pub truncated: bool,
}

/// A write's precondition (core `writeFile`'s `ifMatch` / `ifNoneMatch: '*'`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Condition {
    None,
    IfMatch(String),
    IfNoneMatch,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Written {
    /// '' when the server sent none.
    pub etag: String,
    /// Only a 201.
    pub created: bool,
}

/// `readBinaryFile(conn, path, { cap, timeoutMs, beforeBody })`.
pub struct BinaryRead<'a> {
    pub cap: u64,
    pub timeout: Duration,
    /// Runs once with the declared size before the body is used (core's `beforeBody`).
    pub before_body: &'a mut dyn FnMut(Option<u64>) -> Result<(), StorageError>,
}

/// The account's storage client.
pub trait SourceStorage {
    /// Whether this is S3 (no folders and no writes).
    fn s3(&self) -> bool;
    /// storage-latch.cjs `fingerprintOf(connection)`: the digest of the connection's kind, server,
    /// bucket, username and secret.
    fn fingerprint(&self) -> String;
    /// `listFiles(conn, dir)`, sorted as core sorts.
    fn list(&self, dir: &str) -> Result<Vec<Entry>, StorageError>;
    /// `readTextFile(conn, path)`.
    fn read_text(&self, path: &str) -> Result<TextRead, StorageError>;
    /// `readBinaryFile`, streamed: every chunk goes to `sink` in order; the bytes read.
    fn read_binary(
        &self,
        path: &str,
        read: BinaryRead<'_>,
        sink: &mut dyn FnMut(&[u8]) -> Result<(), StorageError>,
    ) -> Result<u64, StorageError>;
    /// `createFolder`: `Ok(true)` when it already existed (MKCOL 405).
    fn create_folder(&self, path: &str) -> Result<bool, StorageError>;
    /// `writeFile(conn, path, bytes, condition)`.
    fn write(
        &self,
        path: &str,
        bytes: &[u8],
        condition: &Condition,
    ) -> Result<Written, StorageError>;
    /// `discardWritten(conn, path, etag)`: never fails; (deleted, reason).
    fn discard(&self, path: &str, etag: &str) -> (bool, &'static str);
}

/// What the account's storage is for one operation.
pub enum Storage<'a> {
    /// Not browsable (local storage), or no connection could be read.
    None,
    /// Storage is there but cannot be used now: the answer (status, message).
    Refused(u16, &'static str),
    Remote(&'a dyn SourceStorage),
}

impl Storage<'_> {
    pub fn remote(&self) -> Option<&dyn SourceStorage> {
        match self {
            Storage::Remote(s) => Some(*s),
            _ => None,
        }
    }
}

/// project-folders.cjs `allocateFolder(storage, connection, root, project)`.
pub fn allocate_folder(
    storage: &dyn SourceStorage,
    root: &str,
    id: &str,
    reserved: Option<&str>,
) -> Result<String, StorageError> {
    let key = lifecycle::folder_key(id);
    if storage.s3() {
        return Ok(reserved
            .filter(|r| !r.is_empty())
            .map_or_else(|| format!("{root}/{key}"), str::to_owned));
    }
    storage.create_folder(root)?;
    if let Some(reserved) = reserved.filter(|r| !r.is_empty()) {
        if !storage.create_folder(reserved)? {
            return Ok(reserved.to_owned());
        }
    }
    for n in 1..=1000 {
        let folder = if n == 1 {
            format!("{root}/{key}")
        } else {
            format!("{root}/{key} ({n})")
        };
        if !storage.create_folder(&folder)? {
            return Ok(folder);
        }
    }
    Err(StorageError::new(
        500,
        "Too many folders with this project name",
    ))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing, clippy::panic)]
pub(crate) mod fake {
    use super::*;
    use std::cell::RefCell;
    use std::collections::{BTreeMap, BTreeSet};

    /// A synthetic storage: files and folders in memory, calls recorded, failures scripted.
    #[derive(Default)]
    pub struct Fake {
        pub files: RefCell<BTreeMap<String, (Vec<u8>, u64)>>,
        pub dirs: RefCell<BTreeSet<String>>,
        pub calls: RefCell<Vec<String>>,
        /// Paths whose reads fail with this error.
        pub read_errors: RefCell<BTreeMap<String, StorageError>>,
        pub list_errors: RefCell<BTreeMap<String, StorageError>>,
        pub s3: bool,
        pub next_etag: RefCell<u64>,
        /// The declared size `read_binary` reports to its hook, when it should differ from the file's.
        pub declared: RefCell<Option<u64>>,
    }

    impl Fake {
        pub fn put(&self, path: &str, bytes: &[u8]) {
            let mut files = self.files.borrow_mut();
            let n = files.len() as u64 + 1;
            files.insert(path.to_owned(), (bytes.to_vec(), n));
            let mut parent = path;
            while let Some((p, _)) = parent.rsplit_once('/') {
                self.dirs.borrow_mut().insert(p.to_owned());
                parent = p;
            }
        }
        pub fn log(&self) -> Vec<String> {
            self.calls.borrow().clone()
        }
    }

    impl SourceStorage for Fake {
        fn s3(&self) -> bool {
            self.s3
        }
        fn fingerprint(&self) -> String {
            "f".repeat(64)
        }
        fn list(&self, dir: &str) -> Result<Vec<Entry>, StorageError> {
            self.calls.borrow_mut().push(format!("LIST {dir}"));
            if let Some(e) = self.list_errors.borrow().get(dir) {
                return Err(e.clone());
            }
            let prefix = format!("{dir}/");
            let mut out: BTreeMap<String, Entry> = BTreeMap::new();
            for (path, (bytes, _)) in self.files.borrow().iter() {
                let Some(rest) = path.strip_prefix(&prefix) else {
                    continue;
                };
                match rest.split_once('/') {
                    Some((name, _)) => {
                        out.entry(name.to_owned()).or_insert_with(|| Entry {
                            name: name.to_owned(),
                            path: format!("{dir}/{name}"),
                            is_dir: true,
                            size: None,
                            ext: String::new(),
                        });
                    }
                    None => {
                        let ext = rest
                            .rfind('.')
                            .map(|i| rest[i..].to_lowercase())
                            .unwrap_or_default();
                        out.insert(
                            rest.to_owned(),
                            Entry {
                                name: rest.to_owned(),
                                path: path.clone(),
                                is_dir: false,
                                size: Some(bytes.len() as f64),
                                ext,
                            },
                        );
                    }
                }
            }
            for d in self.dirs.borrow().iter() {
                if let Some(rest) = d.strip_prefix(&prefix) {
                    if !rest.contains('/') {
                        out.entry(rest.to_owned()).or_insert_with(|| Entry {
                            name: rest.to_owned(),
                            path: d.clone(),
                            is_dir: true,
                            size: None,
                            ext: String::new(),
                        });
                    }
                }
            }
            Ok(out.into_values().collect())
        }
        fn read_text(&self, path: &str) -> Result<TextRead, StorageError> {
            self.calls.borrow_mut().push(format!("READ {path}"));
            if let Some(e) = self.read_errors.borrow().get(path) {
                return Err(e.clone());
            }
            let files = self.files.borrow();
            let (bytes, _) = files
                .get(path)
                .ok_or_else(|| StorageError::new(404, "storage returned 404").with_upstream(404))?;
            Ok(TextRead {
                content: String::from_utf8_lossy(bytes).into_owned(),
                truncated: false,
            })
        }
        fn read_binary(
            &self,
            path: &str,
            read: BinaryRead<'_>,
            sink: &mut dyn FnMut(&[u8]) -> Result<(), StorageError>,
        ) -> Result<u64, StorageError> {
            self.calls.borrow_mut().push(format!("GET {path}"));
            if let Some(e) = self.read_errors.borrow().get(path) {
                return Err(e.clone());
            }
            let bytes = {
                let files = self.files.borrow();
                files
                    .get(path)
                    .ok_or_else(|| {
                        StorageError::new(404, "storage returned 404").with_upstream(404)
                    })?
                    .0
                    .clone()
            };
            let declared = self.declared.borrow().unwrap_or(bytes.len() as u64);
            if declared > read.cap {
                return Err(StorageError::new(
                    413,
                    format!("file exceeds the {} byte limit", read.cap),
                ));
            }
            (read.before_body)(Some(declared))?;
            for chunk in bytes.chunks(4 * 1024 * 1024) {
                sink(chunk)?;
            }
            Ok(bytes.len() as u64)
        }
        fn create_folder(&self, path: &str) -> Result<bool, StorageError> {
            self.calls.borrow_mut().push(format!("MKCOL {path}"));
            Ok(!self.dirs.borrow_mut().insert(path.to_owned()))
        }
        fn write(
            &self,
            path: &str,
            bytes: &[u8],
            condition: &Condition,
        ) -> Result<Written, StorageError> {
            self.calls.borrow_mut().push(format!("PUT {path}"));
            let existing = self.files.borrow().get(path).map(|(_, e)| *e);
            if *condition == Condition::IfNoneMatch && existing.is_some() {
                return Err(StorageError::new(
                    409,
                    format!(
                        "\"{path}\" already exists in storage, so it was not overwritten (412)"
                    ),
                )
                .with_code("changed"));
            }
            let mut next = self.next_etag.borrow_mut();
            *next += 1;
            self.files
                .borrow_mut()
                .insert(path.to_owned(), (bytes.to_vec(), *next));
            Ok(Written {
                etag: format!("\"v{}\"", *next),
                created: existing.is_none(),
            })
        }
        fn discard(&self, path: &str, etag: &str) -> (bool, &'static str) {
            self.calls.borrow_mut().push(format!("DISCARD {path}"));
            let mut files = self.files.borrow_mut();
            match files.get(path) {
                Some((_, n)) if format!("\"v{n}\"") == etag => {
                    files.remove(path);
                    (true, "")
                }
                Some(_) => (false, "changed"),
                None => (false, "missing"),
            }
        }
    }

    #[test]
    fn failure_kinds_are_cores() {
        let f = |e: StorageError| e.failure().map(|f| f.kind);
        assert_eq!(
            f(StorageError::new(502, "x").with_code("timeout")),
            Some("timeout")
        );
        assert_eq!(
            f(StorageError::new(502, "storage returned 401")),
            Some("login")
        );
        assert_eq!(
            f(StorageError::new(502, "y").with_upstream(403)),
            Some("forbidden")
        );
        assert_eq!(
            f(StorageError::new(502, "y").with_upstream(429)),
            Some("throttled")
        );
        assert_eq!(f(StorageError::new(404, "storage returned 404")), None);
        assert!(StorageError::new(409, "x")
            .with_code("changed")
            .path_taken());
    }

    #[test]
    fn a_project_folder_is_allocated_as_core_allocates_it() {
        let s = Fake::default();
        let root = "noevia projects";
        // The reserved path is taken when MKCOL makes it.
        assert_eq!(
            allocate_folder(&s, root, "proj-1", Some("noevia projects/proj-1")).unwrap(),
            "noevia projects/proj-1"
        );
        // Taken by someone else: the next free name.
        s.dirs.borrow_mut().insert("noevia projects/p2".into());
        assert_eq!(
            allocate_folder(&s, root, "p2", None).unwrap(),
            "noevia projects/p2 (2)"
        );
        // S3 has no folders: the reservation, else root/key, with no call.
        let s3 = Fake {
            s3: true,
            ..Default::default()
        };
        assert_eq!(
            allocate_folder(&s3, root, "p3", None).unwrap(),
            "noevia projects/p3"
        );
        assert!(s3.log().is_empty());
    }
}
