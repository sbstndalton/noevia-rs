//! The stores behind Coworkers and Spaces (sbstndalton/noevia#2446, slice 1). Code, routes and
//! files say `specialist`; people see "Coworkers" and "Spaces".
//!
//! Three per-account stores under the account's directory, written atomically through
//! `server_store::json` and never touched by Node:
//!
//! - `specialists.json`: `{revision, specialists: [...]}`, at most [`MAX_SPECIALISTS`];
//! - `spaces.json`: `{revision, spaces: [...]}`, at most [`MAX_SPACES`];
//! - `spaces/<spaceId>/index.json` (`{pages: [row]}`) and `spaces/<spaceId>/<pageId>.md`, at most
//!   [`MAX_PAGES`] pages of at most [`MAX_PAGE_BYTES`] each.
//!
//! The revision rule is the one Diary files and chat history use. A page's `version` is the sha256
//! hex of its content; a write carries the version it was based on (`None` creates); a mismatch is
//! [`Error::Stale`] carrying the current version, and the caller keeps its draft. Writing the
//! content a page already has is a no-op that returns the current version. The two lists carry an
//! integer `revision` (0 for a file that does not exist yet, +1 per write) checked the same way
//! on update and delete.
//!
//! Every operation takes the account's lock for its whole read-check-write, so two writers on one
//! base cannot both win. Ids must be lowercase hyphenated UUIDs ([`is_id`]) before any path is
//! built from them. This crate has no HTTP; the front maps [`Error`] to statuses.

pub mod backing;
mod lists;
mod pages;
pub mod tools;

pub use backing::Backing;

pub use lists::{
    NewSpace, NewSpecialist, Space, SpaceList, SpacePatch, Specialist, SpecialistList,
    SpecialistPatch,
};
pub use pages::{PageList, PagePut, PageRow, PageWrite};

use serde::de::DeserializeOwned;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

pub const MAX_SPECIALISTS: usize = 24;
pub const MAX_SPACES: usize = 50;
pub const MAX_PAGES: usize = 2000;
pub const MAX_PAGE_BYTES: usize = 512 * 1024;
pub const MAX_NAME_CHARS: usize = 60;
pub const MAX_SPACE_NAME_CHARS: usize = 80;
pub const MAX_TITLE_CHARS: usize = 200;
pub const MAX_BRIEF_CHARS: usize = 8000;
pub const MAX_GLYPH_CHARS: usize = 16;
pub const MAX_TINT_CHARS: usize = 32;
pub const MAX_TOOLBOXES: usize = 64;
pub const MAX_QUERY_CHARS: usize = 200;
pub const MAX_LIMIT: usize = 100;
pub const DEFAULT_LIMIT: usize = 100;
/// Page content a search may read in one request; a search that would read more stops and reports
/// `truncated`.
pub const SEARCH_BYTE_BUDGET: u64 = 32 * 1024 * 1024;
/// `savedBy` for a page a person saved.
pub const SAVED_BY_YOU: &str = "you";

#[derive(Debug)]
pub enum Error {
    /// No such id for this account (also: a malformed id, and another account's).
    NotFound,
    /// The request is not valid; the text is for the person.
    Invalid(String),
    /// A count limit: `code` is `specialist_limit`, `space_limit` or `page_limit`.
    Limit {
        code: &'static str,
        message: String,
    },
    /// Page content over [`MAX_PAGE_BYTES`].
    PageTooLarge,
    /// The write was based on an old revision (lists) or version (pages). Exactly one is set.
    Stale {
        revision: Option<u64>,
        version: Option<String>,
    },
    /// A Space with pages, deleted without `withPages`.
    SpaceNotEmpty,
    /// A store file this code cannot read; it is never overwritten.
    Corrupt(String),
    Io(std::io::Error),
}

impl Error {
    /// The `code` of the `{error, code}` body.
    pub fn code(&self) -> &'static str {
        match self {
            Error::NotFound => "not_found",
            Error::Invalid(_) => "invalid_request",
            Error::Limit { code, .. } => code,
            Error::PageTooLarge => "page_too_large",
            Error::Stale { .. } => "stale_revision",
            Error::SpaceNotEmpty => "space_not_empty",
            Error::Corrupt(_) | Error::Io(_) => "internal",
        }
    }
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::NotFound => write!(f, "Not found"),
            Error::Invalid(m) | Error::Limit { message: m, .. } => write!(f, "{m}"),
            Error::PageTooLarge => write!(f, "Page is larger than {} KiB", MAX_PAGE_BYTES / 1024),
            Error::Stale { .. } => write!(f, "Changed since you loaded it"),
            Error::SpaceNotEmpty => write!(f, "Space has pages; pass withPages to delete them"),
            Error::Corrupt(name) => write!(f, "{name} is unreadable"),
            Error::Io(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for Error {}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error::Io(e)
    }
}

impl From<server_store::json::JsonError> for Error {
    fn from(e: server_store::json::JsonError) -> Self {
        match e {
            server_store::json::JsonError::Io(e) => Error::Io(e),
            other => Error::Io(std::io::Error::other(other.to_string())),
        }
    }
}

/// A lowercase hyphenated UUID, the only shape an id may have before a path is built from it.
pub fn is_id(s: &str) -> bool {
    s.len() == 36
        && s.bytes().enumerate().all(|(i, b)| match i {
            8 | 13 | 18 | 23 => b == b'-',
            _ => b.is_ascii_digit() || (b'a'..=b'f').contains(&b),
        })
}

pub(crate) fn new_id() -> String {
    uuid::Uuid::new_v4().to_string()
}

pub(crate) fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
        .unwrap_or(0)
}

/// The version of page content: sha256 hex.
pub fn version_of(content: &str) -> String {
    Sha256::digest(content.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// `s` trimmed, when it is `1..=max` characters without control characters.
pub(crate) fn clean_line(what: &str, s: &str, max: usize) -> Result<String, Error> {
    let t = s.trim();
    let n = t.chars().count();
    if n == 0 || n > max || t.chars().any(char::is_control) {
        return Err(Error::Invalid(format!(
            "{what} must be 1 to {max} characters on one line"
        )));
    }
    Ok(t.to_owned())
}

static LOCKS: Mutex<Option<HashMap<PathBuf, Arc<Mutex<()>>>>> = Mutex::new(None);

/// One account's stores. Cheap to make; the lock is shared by every `Store` of the same directory.
#[derive(Debug, Clone)]
pub struct Store {
    dir: PathBuf,
    backing: Option<Backing>,
    /// The ids a coworker's `toolboxes` may name; `None` when the caller could not say.
    known_toolboxes: Option<Vec<String>>,
}

impl Store {
    /// `user_dir` is the account's directory (`users/<id>`), already validated by the caller.
    pub fn new(user_dir: impl Into<PathBuf>) -> Self {
        Store {
            dir: user_dir.into(),
            backing: None,
            known_toolboxes: None,
        }
    }

    /// A store whose coworkers have a hidden project and conversation in the account's project
    /// store ([`backing`]). Without it ids stay `null` and nothing is mirrored (the stores alone).
    pub fn with_backing(mut self, backing: Backing) -> Self {
        self.backing = Some(backing);
        self
    }

    /// The toolbox ids this account may give a coworker. A non-empty `toolboxes` is refused
    /// without it, so an id is never accepted unchecked.
    pub fn with_known_toolboxes(mut self, ids: Vec<String>) -> Self {
        self.known_toolboxes = Some(ids);
        self
    }

    pub(crate) fn backing(&self) -> Option<&Backing> {
        self.backing.as_ref()
    }

    pub(crate) fn known_toolboxes(&self) -> Option<&[String]> {
        self.known_toolboxes.as_deref()
    }

    pub(crate) fn dir(&self) -> &Path {
        &self.dir
    }

    /// Runs `f` holding the account's lock.
    pub(crate) fn locked<T>(&self, f: impl FnOnce() -> Result<T, Error>) -> Result<T, Error> {
        let lock = {
            let mut all = LOCKS.lock().unwrap_or_else(|p| p.into_inner());
            Arc::clone(
                all.get_or_insert_with(HashMap::new)
                    .entry(self.dir.clone())
                    .or_default(),
            )
        };
        let _guard = lock.lock().unwrap_or_else(|p| p.into_inner());
        f()
    }

    /// A store file, `default` when it does not exist, [`Error::Corrupt`] when it is not JSON of
    /// the expected shape.
    pub(crate) fn read<T: DeserializeOwned + Default>(
        &self,
        dir: &Path,
        name: &str,
    ) -> Result<T, Error> {
        match std::fs::read_to_string(dir.join(name)) {
            Ok(text) => serde_json::from_str(&text).map_err(|_| Error::Corrupt(name.to_owned())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(T::default()),
            Err(e) => Err(Error::Io(e)),
        }
    }

    pub(crate) fn write<T: serde::Serialize>(
        &self,
        dir: &Path,
        name: &str,
        value: &T,
    ) -> Result<(), Error> {
        let text =
            serde_json::to_string_pretty(value).map_err(|_| Error::Corrupt(name.to_owned()))?;
        server_store::json::write_owned_text(dir, name, &text, server_store::RustAuth::owned())?;
        Ok(())
    }

    pub(crate) fn space_dir(&self, space: &str) -> PathBuf {
        self.dir.join("spaces").join(space)
    }
}

#[cfg(test)]
mod backing_tests;
#[cfg(test)]
mod tests;
#[cfg(test)]
mod tools_tests;
