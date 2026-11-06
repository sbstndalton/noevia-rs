//! The two files the issuer owns.
//!
//! - The **published proxy key** (`CODE_EGRESS_CONTROL_KEY_FILE`): exactly the contract the Node
//!   publisher had, so the proxy reads it unchanged. A regular, single-link file in a trusted
//!   0750-class directory, owner `uid`/`gid`, mode 0440, content `64 lowercase hex digits + LF`,
//!   replaced by rename (or linked when it was lost), never created into an unprepared directory,
//!   no permission repair, no path or value in any failure.
//! - The **private issuer state** (default `<UI_DATA_DIR>/code-egress-issuer.json`): mode 0600,
//!   owned by the issuer. It keeps the web key across restarts (a restart is not a rotation) and the
//!   last session, so the next start can replace its own session instead of waiting for it to die.
use crate::wire::{hex, unhex};
use crate::{err, ControlError, Reason};
use std::ffi::{OsStr, OsString};
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::os::fd::OwnedFd;
use std::os::unix::fs::{fchown, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

fn random_name() -> String {
    use rand_core::{OsRng, RngCore};
    let mut nonce = [0u8; 16];
    OsRng.fill_bytes(&mut nonce);
    hex(&nonce)
}

/// Where the files live and who owns the published one.
#[derive(Clone)]
pub struct IssuerFiles {
    pub key_file: PathBuf,
    pub uid: u32,
    pub gid: u32,
    pub state_file: PathBuf,
}

impl std::fmt::Debug for IssuerFiles {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("IssuerFiles(<configured>)")
    }
}

/// What the published key file holds relative to a key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyState {
    Own,
    Foreign,
    Missing,
    Unknown,
}

fn nofollow_nonblock() -> i32 {
    (rustix::fs::OFlags::NOFOLLOW | rustix::fs::OFlags::NONBLOCK).bits() as i32
}

fn publication<T>() -> Result<T, ControlError> {
    err(Reason::Publication)
}

fn canonical_absolute(file: &Path) -> bool {
    if !file.is_absolute() || file.as_os_str().as_encoded_bytes().contains(&0) {
        return false;
    }
    let mut rebuilt = PathBuf::from("/");
    for component in file.components() {
        match component {
            Component::RootDir => {}
            Component::Normal(part) => rebuilt.push(part),
            _ => return false,
        }
    }
    rebuilt == file
}

fn process_uid() -> u32 {
    rustix::process::getuid().as_raw()
}

/// A crash between link and unlink in [`write_key_file`] leaves the key with a second name. Only the
/// writer's own temporary pattern with this exact inode is removed; any other extra link refuses.
fn clear_interrupted_link(dir: &Path, file: &Path, uid: u32, gid: u32) {
    let Ok(key) = std::fs::symlink_metadata(file) else {
        return;
    };
    if !key.is_file() || key.nlink() != 2 || key.uid() != uid || key.gid() != gid {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        let tmp = name
            .strip_prefix(".egress-key-")
            .and_then(|n| n.strip_suffix(".tmp"));
        if !tmp.is_some_and(|t| {
            t.len() == 32 && t.bytes().all(|c| matches!(c, b'0'..=b'9' | b'a'..=b'f'))
        }) {
            continue;
        }
        if let Ok(other) = std::fs::symlink_metadata(entry.path()) {
            if other.is_file() && other.dev() == key.dev() && other.ino() == key.ino() {
                let _ = std::fs::remove_file(entry.path());
                return;
            }
        }
    }
}

/// `writeKeyFile`: publishes `make()`'s key. `replace` renames over the file (after `before_replace`);
/// otherwise the file must be missing and is linked into place.
fn write_key_file(
    files: &IssuerFiles,
    replace: bool,
    make: &mut dyn FnMut() -> Result<[u8; 32], ControlError>,
    before_replace: &mut dyn FnMut() -> Result<(), ControlError>,
) -> Result<[u8; 32], ControlError> {
    let file = files.key_file.as_path();
    if !canonical_absolute(file)
        || files.gid == 0
        || files.uid == u32::MAX
        || files.gid == u32::MAX
        || process_uid() != files.uid
    {
        return publication();
    }
    let Some(dir) = file.parent() else {
        return publication();
    };
    let Ok(parent) = std::fs::symlink_metadata(dir) else {
        return publication();
    };
    let real_ok = std::fs::canonicalize(dir).is_ok_and(|real| real == dir);
    let mode = parent.permissions().mode();
    if !parent.is_dir()
        || parent.file_type().is_symlink()
        || !real_ok
        || parent.uid() != files.uid
        || parent.gid() != files.gid
        || mode & 0o022 != 0
        || mode & 0o050 != 0o050
    {
        return publication();
    }
    clear_interrupted_link(dir, file, files.uid, files.gid);
    match std::fs::symlink_metadata(file) {
        Ok(old) => {
            if !old.is_file()
                || old.file_type().is_symlink()
                || old.nlink() != 1
                || old.uid() != files.uid
                || old.gid() != files.gid
                || old.mode() & 0o7777 != 0o440
            {
                return publication();
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(_) => return publication(),
    }
    let key = make()?;
    let temporary = dir.join(format!(".egress-key-{}.tmp", random_name()));
    let outcome = (|| -> Result<(), ControlError> {
        let mut out = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o440)
            .custom_flags(rustix::fs::OFlags::NOFOLLOW.bits() as i32)
            .open(&temporary)
            .map_err(|_| ControlError(Reason::Publication))?;
        let written = (|| -> std::io::Result<()> {
            fchown(&out, Some(files.uid), Some(files.gid))?;
            out.set_permissions(std::fs::Permissions::from_mode(0o440))?;
            out.write_all(format!("{}\n", hex(&key)).as_bytes())?;
            out.sync_all()
        })();
        if written.is_err() {
            return publication();
        }
        drop(out);
        let after =
            std::fs::symlink_metadata(dir).map_err(|_| ControlError(Reason::Publication))?;
        if after.dev() != parent.dev() || after.ino() != parent.ino() {
            return publication();
        }
        if replace {
            before_replace()?;
            std::fs::rename(&temporary, file).map_err(|_| ControlError(Reason::Publication))?;
        } else {
            std::fs::hard_link(&temporary, file).map_err(|_| ControlError(Reason::Publication))?;
            std::fs::remove_file(&temporary).map_err(|_| ControlError(Reason::Publication))?;
        }
        Ok(())
    })();
    if outcome.is_err() {
        let _ = std::fs::remove_file(&temporary);
        return publication();
    }
    Ok(key)
}

/// `keyFileState`: a FIFO at the path is never opened blocking and the file is never read past its
/// line (64 hex digits and a newline, as the proxy's keyfile.rs does).
pub fn key_file_state(file: &Path, key: &[u8; 32]) -> KeyState {
    let opened = OpenOptions::new()
        .read(true)
        .custom_flags(nofollow_nonblock())
        .open(file);
    let mut handle = match opened {
        Ok(h) => h,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return KeyState::Missing,
        Err(_) => return KeyState::Unknown,
    };
    if !handle.metadata().is_ok_and(|m| m.is_file()) {
        return KeyState::Unknown;
    }
    let want = format!("{}\n", hex(key)).into_bytes();
    let mut buf = vec![0u8; want.len() + 1];
    let mut got = 0usize;
    while got < buf.len() {
        match handle.read(buf.get_mut(got..).unwrap_or(&mut [])) {
            Ok(0) => break,
            Ok(n) => got += n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(_) => return KeyState::Unknown,
        }
    }
    if got == want.len() && buf.get(..got) == Some(want.as_slice()) {
        KeyState::Own
    } else {
        KeyState::Foreign
    }
}

/// The key the retired Node issuer left published (`CODE_EGRESS_CONTROL_KEY_FILE`), taken over once.
///
/// Node never kept private issuer state: its key is `HMAC(master, random nonce)`, the nonce is not
/// stored, and the session lives in memory. The published file is the only durable artifact, so the
/// takeover reads exactly that, under the checks Node's own publisher applies before it will touch
/// the file (and Node replaces that file on every start, so this is the stricter side): a trusted
/// 0750-class directory owned by the web uid/gid, a regular single-link file of the same uid/gid with
/// mode exactly 0440, content 64 lowercase hex digits and LF, and the process running as that uid.
///
/// `Ok(None)`: there is no such file (a first start). `Err(LegacyKey*)`: something is there that fails
/// a check, named by the reason; nothing is written, repaired or removed in any case.
pub fn read_legacy_key(files: &IssuerFiles) -> Result<Option<[u8; 32]>, ControlError> {
    let file = files.key_file.as_path();
    if !canonical_absolute(file) {
        return err(Reason::LegacyKeyPath);
    }
    if files.gid == 0
        || files.uid == u32::MAX
        || files.gid == u32::MAX
        || process_uid() != files.uid
    {
        return err(Reason::LegacyKeyUser);
    }
    let Some(dir) = file.parent() else {
        return err(Reason::LegacyKeyPath);
    };
    let parent = match std::fs::symlink_metadata(dir) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return err(Reason::LegacyKeyRead),
    };
    match std::fs::symlink_metadata(file) {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return err(Reason::LegacyKeyRead),
    }
    if !parent.is_dir()
        || parent.file_type().is_symlink()
        || !std::fs::canonicalize(dir).is_ok_and(|real| real == dir)
    {
        return err(Reason::LegacyKeyDirectory);
    }
    if parent.uid() != files.uid || parent.gid() != files.gid {
        return err(Reason::LegacyKeyDirOwner);
    }
    let dir_mode = parent.permissions().mode();
    if dir_mode & 0o022 != 0 || dir_mode & 0o050 != 0o050 {
        return err(Reason::LegacyKeyDirMode);
    }
    let mut handle = match OpenOptions::new()
        .read(true)
        .custom_flags(nofollow_nonblock())
        .open(file)
    {
        Ok(h) => h,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return err(Reason::LegacyKeyFile),
    };
    let Ok(meta) = handle.metadata() else {
        return err(Reason::LegacyKeyRead);
    };
    if !meta.is_file() {
        return err(Reason::LegacyKeyFile);
    }
    if meta.nlink() != 1 {
        return err(Reason::LegacyKeyLinks);
    }
    if meta.uid() != files.uid || meta.gid() != files.gid {
        return err(Reason::LegacyKeyOwner);
    }
    if meta.mode() & 0o7777 != 0o440 {
        return err(Reason::LegacyKeyMode);
    }
    // At most one byte more than the line, as the proxy's keyfile.rs reads it.
    let mut buf = [0u8; 66];
    let mut got = 0usize;
    while got < buf.len() {
        match handle.read(buf.get_mut(got..).unwrap_or(&mut [])) {
            Ok(0) => break,
            Ok(n) => got += n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(_) => return err(Reason::LegacyKeyRead),
        }
    }
    let line = buf.get(..got).unwrap_or(&[]);
    let key = line
        .strip_suffix(b"\n")
        .and_then(|hex| std::str::from_utf8(hex).ok())
        .and_then(unhex::<32>);
    let Some(key) = key else {
        return err(Reason::LegacyKeyContent);
    };
    // Still the same directory and the same file after the read.
    let again_dir = std::fs::symlink_metadata(dir);
    let again_file = std::fs::symlink_metadata(file);
    match (again_dir, again_file) {
        (Ok(d), Ok(f))
            if d.dev() == parent.dev()
                && d.ino() == parent.ino()
                && f.dev() == meta.dev()
                && f.ino() == meta.ino() =>
        {
            Ok(Some(key))
        }
        _ => err(Reason::LegacyKeyChanged),
    }
}

/// Creates the (single-level) directory that will hold the private state when it is absent, mode
/// 0700 and owned by the issuer. Nothing else is created; an existing path is left to
/// [`StateDir::open`] to validate (or refuse).
pub fn prepare_state_dir(state_file: &Path) -> Result<(), ControlError> {
    let Some(dir) = state_file.parent() else {
        return publication();
    };
    if !canonical_absolute(state_file) {
        return publication();
    }
    match std::fs::symlink_metadata(dir) {
        Ok(_) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            use std::os::unix::fs::DirBuilderExt;
            // Only into a real directory of the issuer's own that no one else can write to; the whole
            // chain is validated again, without following links, when the state is opened.
            let holder_ok = dir
                .parent()
                .and_then(|p| std::fs::symlink_metadata(p).ok())
                .is_some_and(|m| m.is_dir() && m.uid() == process_uid() && m.mode() & 0o022 == 0);
            if !holder_ok {
                return publication();
            }
            match std::fs::DirBuilder::new().mode(0o700).create(dir) {
                Ok(()) => Ok(()),
                // Created by someone in between: validated like any existing directory.
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Ok(()),
                Err(_) => publication(),
            }
        }
        Err(_) => publication(),
    }
}

// ---- private issuer state ----

/// The persisted private state. `key` is absent only before the first publication completed.
#[derive(Clone, PartialEq, Eq, Default)]
pub struct PrivateState {
    pub key: Option<[u8; 32]>,
    /// A key being published (rekey or first publication); promoted when the published file holds it.
    pub next: Option<[u8; 32]>,
    /// The last session this issuer held, for a Replace after a restart.
    pub epoch: Option<(String, String)>,
}

impl std::fmt::Debug for PrivateState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("PrivateState(<redacted>)")
    }
}

impl PrivateState {
    fn encode(&self) -> String {
        let key =
            |k: &Option<[u8; 32]>| k.map_or("null".to_string(), |k| format!("\"{}\"", hex(&k)));
        let epoch = self.epoch.as_ref().map_or("null".to_string(), |(b, s)| {
            format!("{{\"boot\":\"{b}\",\"session\":\"{s}\"}}")
        });
        format!(
            "{{\"version\":1,\"key\":{},\"next\":{},\"epoch\":{}}}\n",
            key(&self.key),
            key(&self.next),
            epoch
        )
    }
    fn decode(text: &str) -> Option<Self> {
        let v: serde_json::Value = serde_json::from_str(text).ok()?;
        let map = v.as_object()?;
        if map.len() != 4 || v.get("version")?.as_u64()? != 1 {
            return None;
        }
        let key = |name: &str| -> Option<Option<[u8; 32]>> {
            match map.get(name)? {
                serde_json::Value::Null => Some(None),
                serde_json::Value::String(s) => unhex::<32>(s).map(Some),
                _ => None,
            }
        };
        let epoch = match map.get("epoch")? {
            serde_json::Value::Null => None,
            e => {
                let b = e.get("boot")?.as_str()?;
                let s = e.get("session")?.as_str()?;
                if e.as_object()?.len() != 2 || unhex::<16>(b).is_none() || unhex::<16>(s).is_none()
                {
                    return None;
                }
                Some((b.to_string(), s.to_string()))
            }
        };
        Some(Self {
            key: key("key")?,
            next: key("next")?,
            epoch,
        })
    }
}

fn dir_flags() -> rustix::fs::OFlags {
    rustix::fs::OFlags::RDONLY
        | rustix::fs::OFlags::DIRECTORY
        | rustix::fs::OFlags::NOFOLLOW
        | rustix::fs::OFlags::CLOEXEC
}

fn fd_meta(fd: &OwnedFd) -> Option<std::fs::Metadata> {
    File::from(fd.try_clone().ok()?).metadata().ok()
}

/// A directory above the state directory: owned by root or the issuer, and not writable by group or
/// other unless it is a root-owned sticky directory (such as `/tmp`), where others cannot rename or
/// remove what is not theirs.
fn trusted_ancestor(m: &std::fs::Metadata, me: u32) -> bool {
    let mode = m.mode();
    m.is_dir()
        && (m.uid() == 0 || m.uid() == me)
        && (mode & 0o022 == 0 || (mode & 0o1000 != 0 && m.uid() == 0))
}

/// Why the directory holding the private state is not acceptable, if it is not: it must be a real
/// directory of the issuer's own, mode exactly 0700 (no group or other bit at all, so a process with
/// the proxy's supplementary group cannot even enter it). Checked on every open, so an existing
/// directory, a restored one and a later `chmod` are held to the same rule as one just created.
fn state_dir_fault(m: &std::fs::Metadata, me: u32) -> Option<Reason> {
    if !m.is_dir() {
        Some(Reason::StateDirNotDirectory)
    } else if m.uid() != me {
        Some(Reason::StateDirOwner)
    } else if m.mode() & 0o7777 != 0o700 {
        Some(Reason::StateDirMode)
    } else {
        None
    }
}

/// The state file itself: regular, one link, the issuer's, exactly 0600.
fn state_leaf_ok(m: &std::fs::Metadata, me: u32) -> bool {
    m.is_file() && m.nlink() == 1 && m.uid() == me && m.mode() & 0o7777 == 0o600
}

/// A validated, held directory descriptor for the private state. Every component from `/` was opened
/// without following links and is trusted; every read and publication goes through this descriptor
/// (never a path), and the path is re-resolved to the same identity before a publication.
struct StateDir {
    fd: OwnedFd,
    path: PathBuf,
    leaf: OsString,
    dev: u64,
    ino: u64,
}

impl StateDir {
    fn open(path: &Path) -> Result<Self, ControlError> {
        let me = process_uid();
        let (Some(parent), Some(leaf)) = (path.parent(), path.file_name()) else {
            return publication();
        };
        if !canonical_absolute(path) || leaf.as_encoded_bytes().starts_with(b".egress-issuer-") {
            return publication();
        }
        let parts: Vec<&OsStr> = parent
            .components()
            .filter_map(|c| match c {
                Component::Normal(p) => Some(p),
                _ => None,
            })
            .collect();
        let mut fd = rustix::fs::open("/", dir_flags(), rustix::fs::Mode::empty())
            .map_err(|_| ControlError(Reason::Publication))?;
        let mut meta = fd_meta(&fd).ok_or(ControlError(Reason::Publication))?;
        if parts.is_empty() {
            if let Some(fault) = state_dir_fault(&meta, me) {
                return err(fault);
            }
        } else if !trusted_ancestor(&meta, me) {
            return publication();
        }
        for (i, part) in parts.iter().enumerate() {
            let last = i + 1 == parts.len();
            fd = match rustix::fs::openat(&fd, *part, dir_flags(), rustix::fs::Mode::empty()) {
                Ok(fd) => fd,
                // The state directory itself is a link (or not a directory): named, never followed.
                Err(rustix::io::Errno::LOOP | rustix::io::Errno::NOTDIR) if last => {
                    return err(Reason::StateDirNotDirectory)
                }
                Err(_) => return publication(),
            };
            meta = fd_meta(&fd).ok_or(ControlError(Reason::Publication))?;
            if last {
                if let Some(fault) = state_dir_fault(&meta, me) {
                    return err(fault);
                }
            } else if !trusted_ancestor(&meta, me) {
                return publication();
            }
        }
        Ok(Self {
            fd,
            path: path.to_path_buf(),
            leaf: leaf.to_os_string(),
            dev: meta.dev(),
            ino: meta.ino(),
        })
    }

    /// The path still resolves, through trusted components, to the directory this descriptor holds.
    fn unchanged(&self) -> Result<(), ControlError> {
        let again = Self::open(&self.path)?;
        if again.dev == self.dev && again.ino == self.ino {
            Ok(())
        } else {
            publication()
        }
    }

    /// The state file as it is now: `None` when absent; an error (never "no state", never repaired)
    /// when something that is not exactly ours is there.
    fn open_leaf(&self) -> Result<Option<File>, ControlError> {
        let flags = rustix::fs::OFlags::RDONLY
            | rustix::fs::OFlags::NOFOLLOW
            | rustix::fs::OFlags::NONBLOCK
            | rustix::fs::OFlags::CLOEXEC;
        match rustix::fs::openat(&self.fd, &self.leaf, flags, rustix::fs::Mode::empty()) {
            Ok(fd) => {
                let file = File::from(fd);
                match file.metadata() {
                    Ok(m) if state_leaf_ok(&m, process_uid()) => Ok(Some(file)),
                    _ => publication(),
                }
            }
            Err(rustix::io::Errno::NOENT) => Ok(None),
            Err(_) => publication(),
        }
    }
}

/// Reads the private state: `Ok(None)` when there is none. Anything not exactly ours (an untrusted or
/// symlinked directory chain, or a file that is not a regular single-link 0600 file owned by the
/// issuer, or not valid) is an error, never "no state": a silent fresh key would be a silent rotation.
pub fn load_state(path: &Path) -> Result<Option<PrivateState>, ControlError> {
    let dir = StateDir::open(path)?;
    let Some(mut f) = dir.open_leaf()? else {
        dir.unchanged()?;
        return Ok(None);
    };
    let Ok(meta) = f.metadata() else {
        return publication();
    };
    if meta.len() > 4096 {
        return publication();
    }
    let mut text = String::new();
    if f.read_to_string(&mut text).is_err() {
        return publication();
    }
    dir.unchanged()?;
    PrivateState::decode(&text)
        .map(Some)
        .ok_or(ControlError(Reason::Publication))
}

/// Atomic 0600 write through the validated directory descriptor: temporary file, fsync, a fresh check
/// that the directory and the destination are still exactly ours, rename, fsync of the directory.
/// An unsafe destination (substituted symlink, foreign file, loosened mode) is refused, not replaced.
pub fn save_state(path: &Path, state: &PrivateState) -> Result<(), ControlError> {
    let dir = StateDir::open(path)?;
    dir.open_leaf()?;
    let name = OsString::from(format!(".egress-issuer-{}.tmp", random_name()));
    let done = (|| -> Result<(), ControlError> {
        let fd = rustix::fs::openat(
            &dir.fd,
            &name,
            rustix::fs::OFlags::WRONLY
                | rustix::fs::OFlags::CREATE
                | rustix::fs::OFlags::EXCL
                | rustix::fs::OFlags::NOFOLLOW
                | rustix::fs::OFlags::CLOEXEC,
            rustix::fs::Mode::RUSR | rustix::fs::Mode::WUSR,
        )
        .map_err(|_| ControlError(Reason::Publication))?;
        let mut out = File::from(fd);
        let written = (|| -> std::io::Result<()> {
            out.set_permissions(std::fs::Permissions::from_mode(0o600))?;
            out.write_all(state.encode().as_bytes())?;
            out.sync_all()
        })();
        drop(out);
        if written.is_err() {
            return publication();
        }
        dir.unchanged()?;
        dir.open_leaf()?;
        rustix::fs::renameat(&dir.fd, &name, &dir.fd, &dir.leaf)
            .map_err(|_| ControlError(Reason::Publication))?;
        rustix::fs::fsync(&dir.fd).map_err(|_| ControlError(Reason::Publication))
    })();
    if done.is_err() {
        let _ = rustix::fs::unlinkat(&dir.fd, &name, rustix::fs::AtFlags::empty());
        return publication();
    }
    Ok(())
}

// ---- key keeper ----

pub type Rng = Arc<dyn Fn(&mut [u8]) + Send + Sync>;
/// The secrets-store derivation (`derive('code-egress-grant', 32)`), looked up lazily because the
/// store may not exist yet on a first boot.
pub type Master = Arc<dyn Fn() -> Result<[u8; 32], ControlError> + Send + Sync>;

/// The key file as one issuer process sees it. Once it has seen another web's key it never writes
/// again.
pub struct KeyKeeper {
    files: IssuerFiles,
    master: Master,
    rng: Rng,
    current: [u8; 32],
    superseded: bool,
    state: PrivateState,
}

impl std::fmt::Debug for KeyKeeper {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("KeyKeeper(<redacted>)")
    }
}

/// How the keeper came up.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Opened {
    /// A new key was generated and published (first start, or a key published by someone else).
    Fresh,
    /// The persisted key is the published one: a restart, not a rotation.
    Kept,
    /// The persisted key was lost from the key directory and republished unchanged.
    Restored,
    /// The published key is another web's: nothing is written, ever.
    Superseded,
    /// The key the retired Node issuer had published was taken over unchanged (no rotation); only the
    /// private state was written.
    Adopted,
}

impl KeyKeeper {
    /// Brings the key up: keeps the persisted key (restart), finishes an interrupted publication,
    /// restores a lost file, or generates and publishes the first key.
    pub fn open(
        files: IssuerFiles,
        master: Master,
        rng: Rng,
    ) -> Result<(Self, Opened), ControlError> {
        let fresh_key = |rng: &Rng, master: &Master| -> Result<[u8; 32], ControlError> {
            let mut nonce = [0u8; 32];
            rng(&mut nonce);
            let master = master()?;
            egress::control::derive_web_file_key(
                &egress::control::FileKey::from_bytes(master),
                &nonce,
            )
            .map(|k| *k.as_bytes())
            .map_err(|_| ControlError(Reason::Key))
        };
        // With no state directory there is no state: check what is published before creating anything,
        // so a refused takeover leaves the directories exactly as they were.
        let state_dir_missing = files.state_file.parent().is_some_and(|d| {
            matches!(std::fs::symlink_metadata(d), Err(e) if e.kind() == std::io::ErrorKind::NotFound)
        });
        let early = if state_dir_missing {
            Some(read_legacy_key(&files)?)
        } else {
            None
        };
        prepare_state_dir(&files.state_file)?;
        let loaded = load_state(&files.state_file)?;
        // A start with no private state of ours at all (not an interrupted publication of ours): a key
        // already published is the retired Node issuer's, taken over once; a file that fails a check
        // is refused by name. Adopting only writes our own state; the published file is not touched.
        if matches!(
            &loaded,
            None | Some(PrivateState {
                key: None,
                next: None,
                ..
            })
        ) {
            let legacy = match early {
                Some(found) => found,
                None => read_legacy_key(&files)?,
            };
            if let Some(key) = legacy {
                let state = PrivateState {
                    key: Some(key),
                    next: None,
                    epoch: None,
                };
                save_state(&files.state_file, &state)?;
                return Ok((
                    Self {
                        files,
                        master,
                        rng,
                        current: key,
                        superseded: false,
                        state,
                    },
                    Opened::Adopted,
                ));
            }
        }
        let (state, current, how) = match loaded {
            Some(PrivateState {
                key: Some(key),
                next,
                epoch,
            }) => {
                let seen = key_file_state(&files.key_file, &key);
                let promoted = next.filter(|n| key_file_state(&files.key_file, n) == KeyState::Own);
                if let Some(n) = promoted {
                    let state = PrivateState {
                        key: Some(n),
                        next: None,
                        epoch: None,
                    };
                    save_state(&files.state_file, &state)?;
                    (state, n, Opened::Kept)
                } else {
                    match seen {
                        KeyState::Own | KeyState::Unknown => {
                            let state = PrivateState {
                                key: Some(key),
                                next: None,
                                epoch,
                            };
                            if next.is_some() {
                                save_state(&files.state_file, &state)?;
                            }
                            (state, key, Opened::Kept)
                        }
                        KeyState::Missing => {
                            write_key_file(&files, false, &mut || Ok(key), &mut || Ok(()))?;
                            (
                                PrivateState {
                                    key: Some(key),
                                    next: None,
                                    epoch,
                                },
                                key,
                                Opened::Restored,
                            )
                        }
                        KeyState::Foreign => (
                            PrivateState {
                                key: Some(key),
                                next: None,
                                epoch,
                            },
                            key,
                            Opened::Superseded,
                        ),
                    }
                }
            }
            other => {
                // First publication, or one that never completed: the candidate is persisted first,
                // so a crash before the rename is retried with the same key. A key somebody else
                // published is never touched: the file must be absent (created exclusively) or hold
                // exactly this candidate (an interrupted publication); anything else is refused as
                // superseded, before any state is written (a key from before our state existed is taken over
                // above, not here).
                let candidate = match other {
                    Some(PrivateState { next: Some(n), .. }) => n,
                    _ => fresh_key(&rng, &master)?,
                };
                let published = key_file_state(&files.key_file, &candidate);
                if !matches!(published, KeyState::Missing | KeyState::Own) {
                    return publication();
                }
                let pending = PrivateState {
                    key: None,
                    next: Some(candidate),
                    epoch: None,
                };
                save_state(&files.state_file, &pending)?;
                if published == KeyState::Missing {
                    write_key_file(&files, false, &mut || Ok(candidate), &mut || Ok(()))?;
                }
                let state = PrivateState {
                    key: Some(candidate),
                    next: None,
                    epoch: None,
                };
                save_state(&files.state_file, &state)?;
                (state, candidate, Opened::Fresh)
            }
        };
        let superseded = how == Opened::Superseded;
        Ok((
            Self {
                files,
                master,
                rng,
                current,
                superseded,
                state,
            },
            how,
        ))
    }

    pub fn key(&self) -> [u8; 32] {
        self.current
    }

    pub fn persisted_epoch(&self) -> Option<(String, String)> {
        self.state.epoch.clone()
    }

    fn seen(&mut self) -> KeyState {
        let seen = key_file_state(&self.files.key_file, &self.current);
        if seen == KeyState::Foreign {
            self.superseded = true;
        }
        seen
    }

    /// `restore`: 'restored', 'superseded' or 'present'.
    pub fn restore(&mut self) -> &'static str {
        if self.superseded {
            return "superseded";
        }
        match self.seen() {
            KeyState::Foreign => "superseded",
            KeyState::Missing => {
                let key = self.current;
                match write_key_file(&self.files, false, &mut || Ok(key), &mut || Ok(())) {
                    Ok(_) => "restored",
                    Err(_) => "present",
                }
            }
            _ => "present",
        }
    }

    /// A new key over this process's own key only; `None` if the file is anything else.
    pub fn rekey(&mut self) -> Option<[u8; 32]> {
        if self.superseded || self.seen() != KeyState::Own {
            return None;
        }
        let old = self.current;
        let path = self.files.key_file.clone();
        let own = move || -> Result<(), ControlError> {
            if key_file_state(&path, &old) == KeyState::Own {
                Ok(())
            } else {
                publication()
            }
        };
        let mut nonce = [0u8; 32];
        (self.rng)(&mut nonce);
        let master = (self.master)().ok()?;
        let next = egress::control::derive_web_file_key(
            &egress::control::FileKey::from_bytes(master),
            &nonce,
        )
        .ok()
        .map(|k| *k.as_bytes())?;
        // Persist the candidate before touching the published file, so a crash is recoverable.
        let pending = PrivateState {
            key: Some(old),
            next: Some(next),
            epoch: self.state.epoch.clone(),
        };
        save_state(&self.files.state_file, &pending).ok()?;
        let mut make = || own().map(|()| next);
        let mut before = || own();
        write_key_file(&self.files, true, &mut make, &mut before).ok()?;
        let done = PrivateState {
            key: Some(next),
            next: None,
            epoch: None,
        };
        self.current = next;
        // The key is published; persisting it is retried by every later save. A crash right here is
        // recovered by `open` (the published file holds `next`).
        if save_state(&self.files.state_file, &done).is_ok() {
            self.state = done;
        } else {
            self.state = PrivateState {
                key: Some(next),
                next: None,
                epoch: None,
            };
        }
        Some(next)
    }

    /// Remembers the live session (best effort: a failure only costs a conflict heal after a crash).
    pub fn save_epoch(&mut self, epoch: Option<(String, String)>) {
        if self.state.epoch == epoch {
            return;
        }
        let mut next = self.state.clone();
        next.key = Some(self.current);
        next.next = None;
        next.epoch = epoch;
        if save_state(&self.files.state_file, &next).is_ok() {
            self.state = next;
        }
    }
}
