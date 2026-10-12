//! Native `GET /api/routing-default` (M6 groundwork, sbstndalton/noevia#1305): the signed-in
//! user's default routing, read from that user's `preferences.json` the way core
//! `workspace.cjs` loads it (`readJson(file, {}) || {}`, then `defaultRouting === 'manual'`).
//!
//! Read only. Node keeps the PUT and every write to `preferences.json`. Tenant access is
//! descriptor-relative from the data directory with `nofollow` on every component, and the file
//! must be a single-link regular file no larger than [`BODY_LIMIT`]. Missing or malformed data is
//! `auto`, as Node answers. Stricter (fail closed) boundaries: a symlink, non-regular file, a
//! file over the limit, nesting Node would accept but serde refuses, a bad tenant id, or a
//! cancelled read.

use server_routing_mode::input::Json;
use std::{
    fs::File,
    io::Read,
    os::fd::{AsFd, OwnedFd},
    path::Path,
    sync::atomic::{AtomicBool, Ordering},
};

use rustix::fs::{self, FileType, Mode, OFlags};

/// The per-user file Node's workspace loads as `preferences`.
pub const FILE: &str = "preferences.json";
/// Largest file read; a larger one fails closed rather than being truncated.
pub const BODY_LIMIT: usize = 1024 * 1024;
/// The JSON nesting depth serde_json accepts; deeper is a fail-closed boundary.
const MAX_DEPTH: usize = 128;

/// What `GET /api/routing-default` answers: `{"routing": "auto" | "manual"}`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Routing {
    Auto,
    Manual,
}

impl Routing {
    pub fn as_str(self) -> &'static str {
        match self {
            Routing::Auto => "auto",
            Routing::Manual => "manual",
        }
    }
}

/// workspace.cjs `preferences`: only an object with exactly the string `"manual"` under
/// `defaultRouting` selects manual. Duplicate keys: the last one wins, as in `JSON.parse`.
/// Anything else (absent, `null`, an array, a number, another string) is `auto`.
pub fn routing_of(preferences: Option<&Json>) -> Routing {
    let manual = preferences
        .and_then(|p| p.get("defaultRouting"))
        .and_then(|v| v.string())
        .as_deref()
        == Some("manual");
    if manual {
        Routing::Manual
    } else {
        Routing::Auto
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fault {
    /// Fail closed: the store cannot be read safely.
    Unavailable,
    /// The caller went away before the read finished.
    Cancelled,
}

impl From<rustix::io::Errno> for Fault {
    fn from(_: rustix::io::Errno) -> Self {
        Fault::Unavailable
    }
}

impl From<std::io::Error> for Fault {
    fn from(_: std::io::Error) -> Self {
        Fault::Unavailable
    }
}

/// The data directory, opened once; each read walks `users/<id>/` from it without following links.
#[derive(Debug)]
pub struct Root {
    fd: OwnedFd,
}

fn cancelled(c: &AtomicBool) -> Result<(), Fault> {
    if c.load(Ordering::Acquire) {
        Err(Fault::Cancelled)
    } else {
        Ok(())
    }
}

/// The status of an open descriptor, which must be a single-link regular file.
fn regular(fd: impl AsFd) -> Result<fs::Stat, Fault> {
    let stat = fs::fstat(fd)?;
    if FileType::from_raw_mode(stat.st_mode) != FileType::RegularFile || stat.st_nlink != 1 {
        return Err(Fault::Unavailable);
    }
    Ok(stat)
}

/// `users/<id>` under `parent`: `Ok(None)` when either level does not exist (no directory is
/// created: a GET has no side effects).
fn tenant_dir(parent: impl AsFd, name: &str) -> Result<Option<OwnedFd>, Fault> {
    let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC;
    match fs::openat(parent.as_fd(), name, flags, Mode::empty()) {
        Ok(fd) => Ok(Some(fd)),
        Err(rustix::io::Errno::NOENT) => Ok(None),
        Err(e) => Err(e.into()),
    }
}

/// Node's `userDir` accepts `/^[0-9a-f-]{36}$/i`; anything else never names a tenant directory.
fn valid_tenant(id: &str) -> bool {
    id.len() == 36 && id.bytes().all(|b| b.is_ascii_hexdigit() || b == b'-')
}

/// Whether `bytes` nests deeper than [`MAX_DEPTH`] outside strings. Node parses such a file;
/// serde refuses it, so the reader fails closed instead of answering from a refusal.
fn depth_excess(bytes: &[u8]) -> bool {
    let (mut depth, mut quote, mut escape) = (0usize, false, false);
    for &b in bytes {
        if quote {
            if escape {
                escape = false;
            } else if b == b'\\' {
                escape = true;
            } else if b == b'"' {
                quote = false;
            }
        } else if b == b'"' {
            quote = true;
        } else if b == b'[' || b == b'{' {
            depth += 1;
            if depth > MAX_DEPTH {
                return true;
            }
        } else if b == b']' || b == b'}' {
            depth = depth.saturating_sub(1);
        }
    }
    false
}

impl Root {
    /// Opens the data directory (`UI_DATA_DIR`) itself without following a link.
    pub fn open(path: &Path) -> Result<Self, Fault> {
        Ok(Self {
            fd: fs::open(
                path,
                OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                Mode::empty(),
            )?,
        })
    }

    /// The tenant's default routing. `user_id` must come from the authenticated identity; it
    /// selects only `users/<user_id>/`.
    pub fn read(&self, user_id: &str, cancel: &AtomicBool) -> Result<Routing, Fault> {
        if !valid_tenant(user_id) {
            return Err(Fault::Unavailable);
        }
        cancelled(cancel)?;
        let Some(users) = tenant_dir(&self.fd, "users")? else {
            return Ok(routing_of(None));
        };
        let Some(tenant) = tenant_dir(&users, user_id)? else {
            return Ok(routing_of(None));
        };
        let fd = match fs::openat(
            &tenant,
            FILE,
            OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
            Mode::empty(),
        ) {
            Ok(fd) => fd,
            Err(rustix::io::Errno::NOENT) => return Ok(routing_of(None)),
            Err(e) => return Err(e.into()),
        };
        let stat = regular(&fd)?;
        if stat.st_size < 0 || stat.st_size > BODY_LIMIT as i64 {
            return Err(Fault::Unavailable);
        }
        let mut bytes = Vec::new();
        bytes
            .try_reserve(BODY_LIMIT + 1)
            .map_err(|_| Fault::Unavailable)?;
        File::from(fd)
            .take((BODY_LIMIT + 1) as u64)
            .read_to_end(&mut bytes)?;
        cancelled(cancel)?;
        if bytes.len() > BODY_LIMIT {
            return Err(Fault::Unavailable);
        }
        match Json::parse(&bytes) {
            Ok(j) => Ok(routing_of(Some(&j))),
            // Malformed JSON is Node's readJson fallback ({}), i.e. auto. Nesting Node accepts
            // is not malformed: it is unanswerable here, so it fails closed.
            Err(_) if depth_excess(&bytes) => Err(Fault::Unavailable),
            Err(_) => Ok(routing_of(None)),
        }
    }
}
