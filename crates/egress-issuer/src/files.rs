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
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
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

/// Reads the private state: `Ok(None)` when there is none. A file that is not exactly ours (not a
/// regular single-link file owned by the issuer with no group/other access, or not valid) is an
/// error, never "no state": a silent fresh key would be a silent rotation.
pub fn load_state(path: &Path) -> Result<Option<PrivateState>, ControlError> {
    let mut f = match OpenOptions::new()
        .read(true)
        .custom_flags(nofollow_nonblock())
        .open(path)
    {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return publication(),
    };
    let Ok(meta) = f.metadata() else {
        return publication();
    };
    if !meta.is_file()
        || meta.nlink() != 1
        || meta.uid() != process_uid()
        || meta.mode() & 0o077 != 0
        || meta.len() > 4096
    {
        return publication();
    }
    let mut text = String::new();
    if f.read_to_string(&mut text).is_err() {
        return publication();
    }
    PrivateState::decode(&text)
        .map(Some)
        .ok_or(ControlError(Reason::Publication))
}

/// Atomic 0600 write: temporary file in the same directory, fsync, rename, fsync of the directory.
pub fn save_state(path: &Path, state: &PrivateState) -> Result<(), ControlError> {
    let Some(dir) = path.parent() else {
        return publication();
    };
    let temporary = dir.join(format!(".egress-issuer-{}.tmp", random_name()));
    let done = (|| -> std::io::Result<()> {
        let mut out = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(rustix::fs::OFlags::NOFOLLOW.bits() as i32)
            .open(&temporary)?;
        out.set_permissions(std::fs::Permissions::from_mode(0o600))?;
        out.write_all(state.encode().as_bytes())?;
        out.sync_all()?;
        drop(out);
        std::fs::rename(&temporary, path)?;
        File::open(dir)?.sync_all()
    })();
    if done.is_err() {
        let _ = std::fs::remove_file(&temporary);
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
        let loaded = load_state(&files.state_file)?;
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
                // so a crash before the rename is retried with the same key.
                let candidate = match other {
                    Some(PrivateState { next: Some(n), .. }) => n,
                    _ => fresh_key(&rng, &master)?,
                };
                let pending = PrivateState {
                    key: None,
                    next: Some(candidate),
                    epoch: None,
                };
                save_state(&files.state_file, &pending)?;
                write_key_file(&files, true, &mut || Ok(candidate), &mut || Ok(()))?;
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
