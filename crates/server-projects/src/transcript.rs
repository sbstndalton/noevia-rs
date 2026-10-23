//! A chat's transcript, `history-<id>.json`, written by Rust alone (sbstndalton/noevia#1071 slice 4).
//! Node had several writers of the same file, none locked: the history save route (a
//! compare-and-swap that read, compared and wrote without holding anything), the chat turn's
//! interrupted-reply path (a read-modify-write through one fixed `<file>.tmp`) and the conversation
//! import (hard links). Every one of them now goes through these operations, over the internal
//! project store (bins/noevia-server src/project_store.rs) or, for the history save, the front's
//! own route; nothing else writes the file.
//!
//! - One lock per user, `chat-transcripts.lock` in the user's directory ([`crate::lock`], flock),
//!   held for each read-compare-write, so two saves never interleave and a revision compared is
//!   the revision replaced.
//! - Every write goes to a temp file unique to that write, is fsynced and renamed over the
//!   transcript: a reader sees the old or the new file, never a part.
//! - The tombstone list (`deleted-chats.json`) is read under the lock: a deleted chat is never
//!   written back, however late its writer comes.
//! - The user's directory is never created ([`StoreError::Gone`]: the account was removed).
//! - What is written is bounded as the client cannot be trusted to ([`crate::history_sanitize`],
//!   the last [`HISTORY_CAP`] entries), as `JSON.stringify({ history }, null, 2)`.
//! - The revision is the one the history GET answers ([`crate::history::revision_of`]): sha256 hex
//!   of `JSON.stringify(history)`, `history` read as `.history || []`.
//!
//! - Every successful mutation appends one entry to the user's change journal
//!   ([`crate::journal`], `chat-changes.jsonl`) in the same hold of the lock, so Node learns of
//!   transcripts it did not write (sbstndalton/noevia#2153). The transcript is already saved when
//!   the entry is written: a failed append is logged, never the request's failure.
//!
//! The chat id is reduced to `[A-Za-z0-9_-]` as Node's `safeChatId` does; one that reduces to
//! nothing is refused ([`TranscriptError::InvalidChat`]).

use crate::chats::TOMBSTONES_FILE;
use crate::history::{history_file, revision_of, safe_chat_id};
use crate::history_sanitize::{sanitize_history, Thrown};
use crate::journal;
pub use crate::journal::Origin;
use crate::lock;
use crate::partial::{self, State};
use crate::store::{self, StoreError};
use js_json::wtf16;
use js_json::JValue;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

/// core index.cjs `STORED_HISTORY_CAP`: the newest entries kept.
pub const HISTORY_CAP: usize = 5000;
/// The per-user lock's data name (the lock file is this plus [`lock::SUFFIX`]).
pub const LOCK_NAME: &str = "chat-transcripts";

#[derive(Debug)]
pub enum TranscriptError {
    /// The chat id reduces to nothing.
    InvalidChat,
    /// Node's sanitiser would have thrown on this history (the route's invalid JSON).
    Unsanitisable,
    Store(StoreError),
}

impl From<StoreError> for TranscriptError {
    fn from(e: StoreError) -> Self {
        TranscriptError::Store(e)
    }
}

impl From<Thrown> for TranscriptError {
    fn from(_: Thrown) -> Self {
        TranscriptError::Unsanitisable
    }
}

/// The transcript as it is now.
#[derive(Debug, Clone, PartialEq)]
pub struct Current {
    /// `.history || []` of the file.
    pub history: JValue,
    pub revision: String,
    /// Whether the file exists at all (whatever it holds).
    pub exists: bool,
}

/// What a write must find.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Base {
    /// Anything (a save without a base revision, accepted as Node always did).
    Any,
    /// The transcript at this revision.
    Revision(String),
    /// No file at all (the conversation import never overwrites a transcript).
    Absent,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Saved {
    Saved {
        revision: String,
    },
    /// The base did not hold: the transcript as it is, and the revision this write would have
    /// given (equal to `current.revision` when the same transcript is already there).
    Conflict {
        current: Current,
        proposed: String,
    },
    /// The chat is tombstoned: nothing written.
    Deleted,
}

/// chat-interrupted-turn.cjs `saveInterruptedTurn`'s outcomes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Appended {
    Saved {
        revision: String,
    },
    /// Someone saved past this turn, or the user message never reached storage.
    NotLast,
    /// No transcript, or one that is not a list.
    NoUserMessage,
    Deleted,
}

impl Appended {
    pub fn outcome(&self) -> &'static str {
        match self {
            Appended::Saved { .. } => "saved",
            Appended::NotLast => "not-last",
            Appended::NoUserMessage => "no-user-message",
            Appended::Deleted => "deleted",
        }
    }
}

pub(crate) fn existing_dir(dir: &Path) -> Result<(), StoreError> {
    match std::fs::symlink_metadata(dir) {
        Ok(m) if m.is_dir() => Ok(()),
        Ok(_) => Err(StoreError::Corrupt),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Err(StoreError::Gone),
        Err(e) => Err(StoreError::Io(e)),
    }
}

/// The user's directory (which must exist) and the chat's reduced id.
fn locate(data_dir: &Path, user: &str, chat: &str) -> Result<(PathBuf, String), TranscriptError> {
    let safe = safe_chat_id(chat);
    if safe.is_empty() {
        return Err(TranscriptError::InvalidChat);
    }
    let dir = store::user_dir(data_dir, user)?;
    existing_dir(&dir)?;
    Ok((dir, safe))
}

fn lock_user(dir: &Path) -> Result<lock::FileLock, StoreError> {
    lock::acquire(&dir.join(LOCK_NAME)).map_err(|e| match StoreError::from(e) {
        StoreError::Io(io) if io.kind() == std::io::ErrorKind::NotFound => StoreError::Gone,
        other => other,
    })
}

/// Whether `deleted-chats.json` lists `safe`.
pub fn tombstoned(dir: &Path, safe: &str) -> Result<bool, StoreError> {
    Ok(match store::read_json(&dir.join(TOMBSTONES_FILE))? {
        Some(JValue::Arr(ids)) => ids.iter().any(|t| t.as_str() == Some(safe)),
        _ => false,
    })
}

/// The parsed file (`None`: missing, unreadable or not JSON, Node's catch).
fn read_file(file: &Path) -> Result<(Option<JValue>, bool), StoreError> {
    let exists = file.exists();
    Ok((store::read_json(file)?, exists))
}

/// projects.cjs `readHistory`'s value of a parsed file.
fn history_of(parsed: Option<&JValue>) -> JValue {
    let stored = match parsed {
        Some(v @ JValue::Obj(_)) => v.get("history").clone(),
        _ => JValue::Undefined,
    };
    if stored.truthy() {
        stored
    } else {
        JValue::Arr(Vec::new())
    }
}

fn current(file: &Path) -> Result<Current, StoreError> {
    let (parsed, exists) = read_file(file)?;
    let history = history_of(parsed.as_ref());
    let revision = revision_of(&history).ok_or(StoreError::Corrupt)?;
    Ok(Current {
        history,
        revision,
        exists,
    })
}

static TEMP_SEQ: AtomicU64 = AtomicU64::new(0);

/// `file`'s contents replaced by `text`: a temp file unique to this write (pid, a process-wide
/// sequence and random bits), fsynced, renamed over it. The temp file is removed on failure.
pub(crate) fn write_atomic(file: &Path, text: &str) -> Result<(), StoreError> {
    use rand_core::RngCore;
    let mut name = file.as_os_str().to_owned();
    name.push(format!(
        ".{}.{}.{:016x}.tmp",
        std::process::id(),
        TEMP_SEQ.fetch_add(1, Ordering::Relaxed),
        rand_core::OsRng.next_u64()
    ));
    let tmp = PathBuf::from(name);
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let result = (|| -> std::io::Result<()> {
        let mut f = options.open(&tmp)?;
        f.write_all(text.as_bytes())?;
        f.sync_all()?;
        std::fs::rename(&tmp, file)
    })();
    if let Err(e) = result {
        let _ = std::fs::remove_file(&tmp);
        return Err(if e.kind() == std::io::ErrorKind::NotFound {
            StoreError::Gone
        } else {
            StoreError::Io(e)
        });
    }
    sync_parent(file);
    Ok(())
}

/// Best effort: the rename is made durable by fsyncing the folder that holds `file`. Some
/// filesystems refuse a directory fsync (EINVAL, ENOTSUP, EBADF on some network mounts); the
/// write has already landed, so a failure is logged and never fails the save.
fn sync_parent(file: &Path) {
    let Some(parent) = file.parent() else { return };
    let result = std::fs::File::open(parent).and_then(|d| d.sync_all());
    if let Err(e) = result {
        #[allow(clippy::print_stderr)]
        {
            eprintln!(
                "[transcript] could not fsync {} after a save (the save itself landed): {e}",
                parent.display()
            );
        }
    }
}

/// How old one of this module's own temp files must be before a write sweeps it.
const STALE_TEMP: std::time::Duration = std::time::Duration::from_secs(60 * 60);

/// Whether `name` is exactly a temp name [`write_atomic`] makes for `base`
/// (`<base>.<pid>.<seq>.<16 hex>.tmp`).
fn is_own_temp(name: &str, base: &str) -> bool {
    let Some(rest) = name.strip_prefix(base).and_then(|r| r.strip_prefix('.')) else {
        return false;
    };
    let Some(rest) = rest.strip_suffix(".tmp") else {
        return false;
    };
    let parts: Vec<&str> = rest.split('.').collect();
    let digits = |p: &str| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit());
    matches!(parts.as_slice(), [pid, seq, rand]
        if digits(pid) && digits(seq) && rand.len() == 16 && rand.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')))
}

/// Removes temp files a crashed write of `file` left behind: only names [`write_atomic`] makes
/// for this very file, only regular files (a symlink is never followed or removed), only ones
/// older than [`STALE_TEMP`]. Called under the user's transcript lock, which every transcript
/// write holds, so none of them belongs to a running write. The journal's rotation temp files
/// are named after the journal, never after a transcript, so they never match. Best effort: a
/// failure never fails the save.
fn sweep_stale_temps(file: &Path) {
    let (Some(dir), Some(base)) = (file.parent(), file.file_name().and_then(|n| n.to_str())) else {
        return;
    };
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if !is_own_temp(name, base) {
            continue;
        }
        // DirEntry::metadata does not follow symlinks.
        let Ok(meta) = entry.metadata() else { continue };
        if !meta.file_type().is_file() {
            continue;
        }
        let old = meta
            .modified()
            .ok()
            .and_then(|m| m.elapsed().ok())
            .is_some_and(|age| age > STALE_TEMP);
        if old {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

/// What a save of `history` stores: the newest [`HISTORY_CAP`] entries, sanitised.
pub fn bounded(history: &[JValue]) -> Result<Vec<JValue>, Thrown> {
    let from = history.len().saturating_sub(HISTORY_CAP);
    sanitize_history(history.get(from..).unwrap_or(&[]))
}

fn store_text(next: &[JValue]) -> Result<String, StoreError> {
    wtf16::stringify_pretty(&JValue::obj([("history", JValue::Arr(next.to_vec()))]))
        .ok_or(StoreError::Corrupt)
}

/// The journal entry for a mutation just made (the lock still held). Best effort: the transcript
/// is saved whatever happens here.
fn note(dir: &Path, safe: &str, revision: Option<&str>, op: journal::Op, origin: Origin) {
    if let Err(e) = journal::record(dir, safe, revision, op, origin, journal::LIMITS) {
        eprintln!(
            "noevia-server: chat change journal append failed ({}): {e:?}",
            op.as_str()
        );
    }
}

/// The transcript now (no lock: every write is a rename).
pub fn read(data_dir: &Path, user: &str, chat: &str) -> Result<Current, TranscriptError> {
    let (dir, safe) = locate(data_dir, user, chat)?;
    Ok(current(&history_file(&dir, &safe))?)
}

/// chat-partial.cjs `supersedePartial`: a transcript the client saved replaces a saved partial,
/// unless the reply that writes it is still running in this container start (`start_id`), as Node
/// left it while its reply streamed. Best effort, like Node's.
fn supersede_partial(dir: &Path, safe: &str, start_id: &str) {
    let file = partial::file_for(dir, safe);
    if let Ok(Some(saved)) = store::read_json(&file) {
        if partial::state(&saved, start_id) == State::Running {
            return;
        }
    }
    let _ = std::fs::remove_file(&file);
}

/// Saves `history` (bounded, see [`bounded`]) if the transcript is at `base` and the chat is not
/// deleted. `supersede`: the start id when this is the client's own save, whose saved partial
/// reply it then supersedes ([`supersede_partial`]). Journalled with origin `other`; see
/// [`write_as`].
pub fn write(
    data_dir: &Path,
    user: &str,
    chat: &str,
    history: &[JValue],
    base: &Base,
    supersede: Option<&str>,
) -> Result<Saved, TranscriptError> {
    write_as(
        data_dir,
        user,
        chat,
        history,
        base,
        supersede,
        Origin::Other,
    )
}

/// [`write`], journalled with `origin` (the user's own save is [`Origin::History`], the only
/// write Node builds a chat's brain for).
pub fn write_as(
    data_dir: &Path,
    user: &str,
    chat: &str,
    history: &[JValue],
    base: &Base,
    supersede: Option<&str>,
    origin: Origin,
) -> Result<Saved, TranscriptError> {
    let (dir, safe) = locate(data_dir, user, chat)?;
    let next = bounded(history)?;
    let proposed = revision_of(&JValue::Arr(next.clone())).ok_or(StoreError::Corrupt)?;
    let text = store_text(&next)?;
    let file = history_file(&dir, &safe);
    let _held = lock_user(&dir)?;
    if tombstoned(&dir, &safe)? {
        return Ok(Saved::Deleted);
    }
    let now = current(&file)?;
    let holds = match base {
        Base::Any => true,
        Base::Revision(r) => *r == now.revision,
        Base::Absent => !now.exists,
    };
    if !holds {
        return Ok(Saved::Conflict {
            current: now,
            proposed,
        });
    }
    sweep_stale_temps(&file);
    write_atomic(&file, &text)?;
    if let Some(start_id) = supersede {
        supersede_partial(&dir, &safe, start_id);
    }
    note(&dir, &safe, Some(&proposed), journal::Op::Write, origin);
    Ok(Saved::Saved { revision: proposed })
}

/// chat-interrupted-turn.cjs `saveInterruptedTurn`: `entry` appended after the user message
/// `message` when that is still the transcript's last entry, in one hold of the lock (Node read,
/// checked and wrote through a fixed `<file>.tmp` with nothing held, so a save landing meanwhile
/// was lost or the two writers' temp files collided).
pub fn append_if_last(
    data_dir: &Path,
    user: &str,
    chat: &str,
    message: &str,
    entry: &JValue,
) -> Result<Appended, TranscriptError> {
    append_if_last_as(data_dir, user, chat, message, entry, Origin::Other)
}

/// [`append_if_last`], journalled with `origin`.
pub fn append_if_last_as(
    data_dir: &Path,
    user: &str,
    chat: &str,
    message: &str,
    entry: &JValue,
    origin: Origin,
) -> Result<Appended, TranscriptError> {
    let (dir, safe) = locate(data_dir, user, chat)?;
    let file = history_file(&dir, &safe);
    let _held = lock_user(&dir)?;
    if tombstoned(&dir, &safe)? {
        return Ok(Appended::Deleted);
    }
    // `JSON.parse(file).history`, which must be a list.
    let (parsed, _) = read_file(&file)?;
    let Some(JValue::Arr(history)) = parsed.as_ref().map(|v| v.get("history")) else {
        return Ok(Appended::NoUserMessage);
    };
    let is_turn = history.last().is_some_and(|last| {
        last.truthy()
            && last.get("role").as_str() == Some("user")
            && last.get("content").as_str() == Some(message)
    });
    if !is_turn {
        return Ok(Appended::NotLast);
    }
    let mut all = history.clone();
    all.push(entry.clone());
    let next = bounded(&all)?;
    let revision = revision_of(&JValue::Arr(next.clone())).ok_or(StoreError::Corrupt)?;
    sweep_stale_temps(&file);
    write_atomic(&file, &store_text(&next)?)?;
    note(&dir, &safe, Some(&revision), journal::Op::Append, origin);
    Ok(Appended::Saved { revision })
}

/// Removes the transcript only if it is exactly what a save of `history` would have written
/// (the conversation import taking back a transcript it published, never one saved since).
pub fn remove_if(
    data_dir: &Path,
    user: &str,
    chat: &str,
    history: &[JValue],
) -> Result<bool, TranscriptError> {
    remove_if_as(data_dir, user, chat, history, Origin::Other)
}

/// [`remove_if`], journalled with `origin`.
pub fn remove_if_as(
    data_dir: &Path,
    user: &str,
    chat: &str,
    history: &[JValue],
    origin: Origin,
) -> Result<bool, TranscriptError> {
    let (dir, safe) = locate(data_dir, user, chat)?;
    let ours = revision_of(&JValue::Arr(bounded(history)?)).ok_or(StoreError::Corrupt)?;
    let file = history_file(&dir, &safe);
    let _held = lock_user(&dir)?;
    let now = current(&file)?;
    if !now.exists || now.revision != ours {
        return Ok(false);
    }
    match std::fs::remove_file(&file) {
        Ok(()) => {
            note(&dir, &safe, None, journal::Op::Remove, origin);
            Ok(true)
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(StoreError::Io(e).into()),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
mod tests {
    use super::*;

    const USER: &str = "11111111-2222-4333-8444-555555555555";
    const OTHER: &str = "99999999-2222-4333-8444-555555555555";
    const NOW: &str = "0123456789abcdef0123456789abcdef";
    const BEFORE: &str = "ffffffffffffffffffffffffffffffff";

    fn setup() -> (tempfile::TempDir, PathBuf) {
        let root = tempfile::tempdir().unwrap();
        let dir = store::user_dir(root.path(), USER).unwrap();
        std::fs::create_dir_all(&dir).unwrap();
        (root, dir)
    }

    fn list(text: &str) -> Vec<JValue> {
        match wtf16::parse(text).unwrap() {
            JValue::Arr(items) => items,
            _ => panic!("not a list"),
        }
    }

    fn rev(text: &str) -> String {
        revision_of(&wtf16::parse(text).unwrap()).unwrap()
    }

    fn saved_text(dir: &Path, id: &str) -> String {
        std::fs::read_to_string(history_file(dir, id)).unwrap()
    }

    #[test]
    fn write_atomic_lands_and_syncs_its_folder() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("history-c1.json");
        write_atomic(&file, "{\"history\":[]}").unwrap();
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "{\"history\":[]}");
        // A folder that cannot be opened for the fsync never fails a save that landed.
        sync_parent(Path::new("/nonexistent-qa-dir/history-c1.json"));
        sync_parent(Path::new("history-c1.json"));
    }

    fn aged(p: &Path) {
        let old_ts = std::time::SystemTime::now() - std::time::Duration::from_secs(2 * 60 * 60);
        std::fs::File::options()
            .write(true)
            .open(p)
            .unwrap()
            .set_modified(old_ts)
            .unwrap();
    }

    #[test]
    fn only_this_modules_own_temp_names_match() {
        let base = "history-c1.json";
        assert!(is_own_temp(
            "history-c1.json.123.0.00ff00ff00ff00ff.tmp",
            base
        ));
        for other in [
            "history-c1.json",
            "history-c1.json.tmp",
            "history-c1.json.123.tmp",
            "history-c1.json.123.0.00FF00FF00FF00FF.tmp",
            "history-c1.json.123.0.00ff.tmp",
            "history-c1.json.x.0.00ff00ff00ff00ff.tmp",
            "history-c10.json.123.0.00ff00ff00ff00ff.tmp",
            "history-c1.json.123.0.00ff00ff00ff00ff.tmp.keep",
            "notes.json.123.0.00ff00ff00ff00ff.tmp",
            "chat-changes.jsonl.123.0.00ff00ff00ff00ff.tmp",
        ] {
            assert!(!is_own_temp(other, base), "{other}");
        }
    }

    #[test]
    fn every_transcript_write_path_sweeps_only_its_own_stale_temps() {
        for path in 0..3 {
            let (root, dir) = setup();
            let data = root.path().to_path_buf();
            let entry = |t: &str| list(&format!(r#"[{{"role":"user","content":"{t}"}}]"#));
            write(&data, USER, "c1", &entry("qa"), &Base::Any, None).unwrap();
            let make = |name: &str, old: bool| {
                let p = dir.join(name);
                std::fs::write(&p, "x").unwrap();
                if old {
                    aged(&p);
                }
                p
            };
            let stale = make("history-c1.json.9.1.0123456789abcdef.tmp", true);
            let fresh = make("history-c1.json.9.2.0123456789abcdef.tmp", false);
            let other_chat = make("history-c2.json.9.3.0123456789abcdef.tmp", true);
            let unrelated = make("history-c1.json.old.tmp", true);
            let journal_tmp = make("chat-changes.jsonl.9.4.0123456789abcdef.tmp", true);
            match path {
                0 => {
                    write(&data, USER, "c1", &entry("b"), &Base::Any, None).unwrap();
                }
                1 => {
                    write_as(
                        &data,
                        USER,
                        "c1",
                        &entry("b"),
                        &Base::Any,
                        None,
                        Origin::History,
                    )
                    .unwrap();
                }
                _ => {
                    let a = append_if_last_as(
                        &data,
                        USER,
                        "c1",
                        "qa",
                        &wtf16::parse(r#"{"role":"assistant","content":"hi"}"#).unwrap(),
                        Origin::Other,
                    )
                    .unwrap();
                    assert!(matches!(a, Appended::Saved { .. }));
                }
            }
            assert!(
                !stale.exists(),
                "path {path}: a stale temp of this transcript is swept"
            );
            assert!(fresh.exists(), "path {path}: a recent one is left");
            assert!(
                other_chat.exists(),
                "path {path}: another transcript's temp is left"
            );
            assert!(
                unrelated.exists(),
                "path {path}: a name this module never makes is left"
            );
            assert!(
                journal_tmp.exists(),
                "path {path}: the journal's temp is left"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn an_old_symlink_named_like_a_temp_is_never_followed_or_removed() {
        let (root, dir) = setup();
        let data = root.path().to_path_buf();
        let outside = tempfile::tempdir().unwrap();
        let target = outside.path().join("victim.txt");
        std::fs::write(&target, "precious").unwrap();
        let link = dir.join("history-c1.json.9.1.0123456789abcdef.tmp");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        // Age the link itself (touch -h does not follow it) and the target too, so only the
        // file-type check stands between the sweep and the link.
        aged(&target);
        let aged_link = std::process::Command::new("touch")
            .args(["-h", "-d", "2 hours ago"])
            .arg(&link)
            .status()
            .unwrap();
        assert!(aged_link.success());
        let target_modified = std::fs::metadata(&target).unwrap().modified().unwrap();
        assert!(std::fs::symlink_metadata(&link)
            .unwrap()
            .file_type()
            .is_symlink());
        write(
            &data,
            USER,
            "c1",
            &list(r#"[{"role":"user","content":"qa"}]"#),
            &Base::Any,
            None,
        )
        .unwrap();
        assert!(
            std::fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink(),
            "the link itself is left"
        );
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "precious");
        assert_eq!(
            std::fs::metadata(&target).unwrap().modified().unwrap(),
            target_modified
        );
    }

    #[test]
    fn a_save_is_nodes_bytes_and_answers_the_get_revision() {
        let (root, dir) = setup();
        let got = write(
            root.path(),
            USER,
            "c-1",
            &list(r#"[{"role":"user","content":"qa"}]"#),
            &Base::Any,
            None,
        )
        .unwrap();
        assert_eq!(
            got,
            Saved::Saved {
                revision: rev(r#"[{"role":"user","content":"qa"}]"#)
            }
        );
        assert_eq!(
            saved_text(&dir, "c-1"),
            "{\n  \"history\": [\n    {\n      \"role\": \"user\",\n      \"content\": \"qa\"\n    }\n  ]\n}"
        );
        let now = read(root.path(), USER, "c-1").unwrap();
        assert_eq!(now.revision, rev(r#"[{"role":"user","content":"qa"}]"#));
        assert!(now.exists);
        // No temp file is left behind.
        let left: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .filter(|n| n.ends_with(".tmp"))
            .collect();
        assert!(left.is_empty(), "{left:?}");
    }

    #[test]
    fn a_stale_base_revision_is_a_conflict_with_the_current_copy() {
        let (root, dir) = setup();
        write(root.path(), USER, "c1", &list("[1]"), &Base::Any, None).unwrap();
        let stale = rev("[]");
        let got = write(
            root.path(),
            USER,
            "c1",
            &list("[2]"),
            &Base::Revision(stale),
            None,
        )
        .unwrap();
        match got {
            Saved::Conflict { current, proposed } => {
                assert_eq!(current.history, wtf16::parse("[1]").unwrap());
                assert_eq!(current.revision, rev("[1]"));
                assert_eq!(proposed, rev("[2]"));
            }
            other => panic!("{other:?}"),
        }
        assert!(saved_text(&dir, "c1").contains('1'));
        let ok = write(
            root.path(),
            USER,
            "c1",
            &list("[2]"),
            &Base::Revision(rev("[1]")),
            None,
        )
        .unwrap();
        assert_eq!(
            ok,
            Saved::Saved {
                revision: rev("[2]")
            }
        );
    }

    #[test]
    fn a_missing_transcript_is_at_the_empty_revision() {
        let (root, _dir) = setup();
        let got = write(
            root.path(),
            USER,
            "c1",
            &list("[1]"),
            &Base::Revision(rev("[]")),
            None,
        )
        .unwrap();
        assert_eq!(
            got,
            Saved::Saved {
                revision: rev("[1]")
            }
        );
    }

    #[test]
    fn absent_never_overwrites_and_reports_the_same_transcript() {
        let (root, _dir) = setup();
        assert!(matches!(
            write(root.path(), USER, "c1", &list("[1]"), &Base::Absent, None).unwrap(),
            Saved::Saved { .. }
        ));
        match write(root.path(), USER, "c1", &list("[1]"), &Base::Absent, None).unwrap() {
            Saved::Conflict { current, proposed } => assert_eq!(current.revision, proposed),
            other => panic!("{other:?}"),
        }
        match write(root.path(), USER, "c1", &list("[2]"), &Base::Absent, None).unwrap() {
            Saved::Conflict { current, proposed } => assert_ne!(current.revision, proposed),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_tombstoned_chat_is_never_written_back() {
        let (root, dir) = setup();
        std::fs::write(dir.join(TOMBSTONES_FILE), r#"["c-gone"]"#).unwrap();
        assert_eq!(
            write(root.path(), USER, "c-gone", &list("[1]"), &Base::Any, None).unwrap(),
            Saved::Deleted
        );
        assert_eq!(
            write(root.path(), USER, "c-gone!", &list("[1]"), &Base::Any, None).unwrap(),
            Saved::Deleted
        );
        assert_eq!(
            append_if_last(root.path(), USER, "c-gone", "q", &JValue::Null).unwrap(),
            Appended::Deleted
        );
        assert!(!history_file(&dir, "c-gone").exists());
    }

    #[test]
    fn a_removed_account_is_gone_and_its_directory_is_not_recreated() {
        let root = tempfile::tempdir().unwrap();
        let err = write(root.path(), USER, "c1", &list("[1]"), &Base::Any, None).unwrap_err();
        assert!(
            matches!(err, TranscriptError::Store(StoreError::Gone)),
            "{err:?}"
        );
        assert!(!store::user_dir(root.path(), USER).unwrap().exists());
        for bad in ["", "../x", "not-a-uuid"] {
            assert!(
                matches!(
                    write(root.path(), bad, "c1", &list("[1]"), &Base::Any, None).unwrap_err(),
                    TranscriptError::Store(StoreError::InvalidUser)
                ),
                "{bad}"
            );
        }
    }

    #[test]
    fn one_users_save_never_touches_another_users_transcript() {
        let (root, dir) = setup();
        let other = store::user_dir(root.path(), OTHER).unwrap();
        std::fs::create_dir_all(&other).unwrap();
        std::fs::write(history_file(&other, "c1"), r#"{"history":["qa theirs"]}"#).unwrap();
        write(
            root.path(),
            USER,
            "c1",
            &list(r#"["qa mine"]"#),
            &Base::Any,
            None,
        )
        .unwrap();
        assert_eq!(
            std::fs::read_to_string(history_file(&other, "c1")).unwrap(),
            r#"{"history":["qa theirs"]}"#
        );
        assert!(saved_text(&dir, "c1").contains("qa mine"));
        // A chat id cannot climb out of the user's directory.
        write(
            root.path(),
            USER,
            "../99999999-2222-4333-8444-555555555555/c1",
            &list("[3]"),
            &Base::Any,
            None,
        )
        .unwrap();
        assert_eq!(
            std::fs::read_to_string(history_file(&other, "c1")).unwrap(),
            r#"{"history":["qa theirs"]}"#
        );
        assert!(matches!(
            write(root.path(), USER, "../..", &list("[]"), &Base::Any, None).unwrap_err(),
            TranscriptError::InvalidChat
        ));
    }

    #[test]
    fn the_cap_keeps_the_newest_entries_and_sanitises() {
        let (root, dir) = setup();
        let many: Vec<JValue> = (0..HISTORY_CAP + 3)
            .map(|i| JValue::Num(i as f64))
            .collect();
        write(root.path(), USER, "c1", &many, &Base::Any, None).unwrap();
        let now = read(root.path(), USER, "c1").unwrap();
        let JValue::Arr(items) = now.history else {
            panic!()
        };
        assert_eq!(items.len(), HISTORY_CAP);
        assert_eq!(items.first(), Some(&JValue::Num(3.0)));
        write(
            root.path(),
            USER,
            "c2",
            &list(r#"[{"role":"user","content":"q","skill":"nope"}]"#),
            &Base::Any,
            None,
        )
        .unwrap();
        assert!(!saved_text(&dir, "c2").contains("nope"));
    }

    #[test]
    fn a_client_save_supersedes_an_interrupted_partial_but_not_a_running_one() {
        let (root, dir) = setup();
        let pfile = partial::file_for(&dir, "c1");
        std::fs::create_dir_all(pfile.parent().unwrap()).unwrap();
        std::fs::write(
            &pfile,
            format!(r#"{{"content":"qa","running":{{"start":"{NOW}"}}}}"#),
        )
        .unwrap();
        write(root.path(), USER, "c1", &list("[1]"), &Base::Any, Some(NOW)).unwrap();
        assert!(pfile.exists(), "a running reply keeps its partial");
        std::fs::write(
            &pfile,
            format!(r#"{{"content":"qa","running":{{"start":"{BEFORE}"}}}}"#),
        )
        .unwrap();
        write(root.path(), USER, "c1", &list("[1]"), &Base::Any, Some(NOW)).unwrap();
        assert!(!pfile.exists(), "a restart's leftover is superseded");
        std::fs::write(&pfile, r#"{"content":"qa"}"#).unwrap();
        write(root.path(), USER, "c1", &list("[1]"), &Base::Any, None).unwrap();
        assert!(pfile.exists(), "only the client's own save supersedes");
        // Another account's running partial for the same chat id does not keep this one.
        let other = store::user_dir(root.path(), OTHER).unwrap();
        let ofile = partial::file_for(&other, "c1");
        std::fs::create_dir_all(ofile.parent().unwrap()).unwrap();
        std::fs::write(&ofile, format!(r#"{{"running":{{"start":"{NOW}"}}}}"#)).unwrap();
        write(root.path(), USER, "c1", &list("[1]"), &Base::Any, Some(NOW)).unwrap();
        assert!(!pfile.exists());
        assert!(ofile.exists());
    }

    #[test]
    fn append_if_last_follows_save_interrupted_turn() {
        let (root, dir) = setup();
        let entry =
            wtf16::parse(r#"{"role":"assistant","content":"qa part","interrupted":true}"#).unwrap();
        assert_eq!(
            append_if_last(root.path(), USER, "c1", "q", &entry).unwrap(),
            Appended::NoUserMessage
        );
        std::fs::write(history_file(&dir, "c1"), r#"{"history":{"a":1}}"#).unwrap();
        assert_eq!(
            append_if_last(root.path(), USER, "c1", "q", &entry).unwrap(),
            Appended::NoUserMessage
        );
        std::fs::write(history_file(&dir, "c1"), r#"{"history":[]}"#).unwrap();
        assert_eq!(
            append_if_last(root.path(), USER, "c1", "q", &entry).unwrap(),
            Appended::NotLast
        );
        std::fs::write(
            history_file(&dir, "c1"),
            r#"{"history":[{"role":"user","content":"other"}]}"#,
        )
        .unwrap();
        assert_eq!(
            append_if_last(root.path(), USER, "c1", "q", &entry).unwrap(),
            Appended::NotLast
        );
        std::fs::write(
            history_file(&dir, "c1"),
            r#"{"history":[{"role":"user","content":"q"}]}"#,
        )
        .unwrap();
        let got = append_if_last(root.path(), USER, "c1", "q", &entry).unwrap();
        let want = r#"[{"role":"user","content":"q"},{"role":"assistant","content":"qa part","interrupted":true}]"#;
        assert_eq!(
            got,
            Appended::Saved {
                revision: rev(want)
            }
        );
        assert_eq!(
            read(root.path(), USER, "c1").unwrap().history,
            wtf16::parse(want).unwrap()
        );
        // Once kept, a second try finds its own reply last.
        assert_eq!(
            append_if_last(root.path(), USER, "c1", "q", &entry).unwrap(),
            Appended::NotLast
        );
    }

    #[test]
    fn remove_if_takes_only_the_exact_transcript() {
        let (root, dir) = setup();
        write(root.path(), USER, "c1", &list("[1]"), &Base::Any, None).unwrap();
        assert!(!remove_if(root.path(), USER, "c1", &list("[2]")).unwrap());
        assert!(history_file(&dir, "c1").exists());
        assert!(remove_if(root.path(), USER, "c1", &list("[1]")).unwrap());
        assert!(!history_file(&dir, "c1").exists());
        assert!(!remove_if(root.path(), USER, "c1", &list("[1]")).unwrap());
    }

    #[test]
    fn concurrent_saves_on_one_base_let_exactly_one_win() {
        // Node compared and wrote without a lock, so both of two saves from the same base
        // succeeded and the first was lost without a conflict.
        let (root, _dir) = setup();
        write(root.path(), USER, "c1", &list("[0]"), &Base::Any, None).unwrap();
        let base = rev("[0]");
        let wins: usize = std::thread::scope(|s| {
            let handles: Vec<_> = (0..16)
                .map(|i| {
                    let (root, base) = (root.path(), base.clone());
                    s.spawn(move || {
                        let h = vec![JValue::Num(0.0), JValue::Num(f64::from(i + 1))];
                        matches!(
                            write(root, USER, "c1", &h, &Base::Revision(base), None).unwrap(),
                            Saved::Saved { .. }
                        )
                    })
                })
                .collect();
            handles
                .into_iter()
                .map(|h| usize::from(h.join().unwrap()))
                .sum()
        });
        assert_eq!(wins, 1);
    }

    fn journal(root: &Path, user: &str) -> Vec<(u64, String, String, String)> {
        journal::read(root, user, 0)
            .unwrap()
            .entries
            .iter()
            .map(|e| {
                (
                    e.get("seq").and_then(serde_json::Value::as_u64).unwrap(),
                    e.get("chatId")
                        .and_then(|v| v.as_str())
                        .unwrap()
                        .to_string(),
                    e.get("op").and_then(|v| v.as_str()).unwrap().to_string(),
                    e.get("revision")
                        .and_then(|v| v.as_str())
                        .unwrap_or("-")
                        .to_string(),
                )
            })
            .collect()
    }

    #[test]
    fn each_mutation_journals_once_and_a_refused_one_not_at_all() {
        // sbstndalton/noevia#2153: Node tails this to run the chat mirror and brain.
        let (root, dir) = setup();
        let r1 = match write(
            root.path(),
            USER,
            "qa-c1",
            &list("[{\"role\":\"user\",\"content\":\"q\"}]"),
            &Base::Any,
            None,
        )
        .unwrap()
        {
            Saved::Saved { revision } => revision,
            other => panic!("{other:?}"),
        };
        // A conflict, an absent-only write over a file and a not-last append write nothing.
        write(
            root.path(),
            USER,
            "qa-c1",
            &list("[9]"),
            &Base::Revision("stale".into()),
            None,
        )
        .unwrap();
        write(
            root.path(),
            USER,
            "qa-c1",
            &list("[9]"),
            &Base::Absent,
            None,
        )
        .unwrap();
        assert_eq!(
            append_if_last(
                root.path(),
                USER,
                "qa-c1",
                "other",
                &JValue::obj([("role", JValue::Str("assistant".into()))])
            )
            .unwrap(),
            Appended::NotLast
        );
        let r2 = match append_if_last(
            root.path(),
            USER,
            "qa-c1",
            "q",
            &JValue::obj([
                ("role", JValue::Str("assistant".into())),
                ("content", JValue::Str("a".into())),
            ]),
        )
        .unwrap()
        {
            Appended::Saved { revision } => revision,
            other => panic!("{other:?}"),
        };
        write(
            root.path(),
            USER,
            "qa-c2",
            &list("[1]"),
            &Base::Absent,
            None,
        )
        .unwrap();
        assert!(remove_if(root.path(), USER, "qa-c2", &list("[1]")).unwrap());
        assert!(!remove_if(root.path(), USER, "qa-c2", &list("[1]")).unwrap());
        // A tombstoned chat is never written, so never journalled.
        std::fs::write(dir.join(TOMBSTONES_FILE), "[\"qa-c3\"]").unwrap();
        assert_eq!(
            write(root.path(), USER, "qa-c3", &list("[1]"), &Base::Any, None).unwrap(),
            Saved::Deleted
        );
        assert_eq!(
            journal(root.path(), USER),
            vec![
                (1, "qa-c1".into(), "write".into(), r1),
                (2, "qa-c1".into(), "append".into(), r2),
                (3, "qa-c2".into(), "write".into(), rev("[1]")),
                (4, "qa-c2".into(), "remove".into(), "-".into()),
            ]
        );
        // The journalled revision is the one the history GET answers.
        assert_eq!(
            read(root.path(), USER, "qa-c1").unwrap().revision,
            journal(root.path(), USER)[1].3
        );
    }

    #[test]
    fn each_mutation_journals_the_origin_it_was_given() {
        let (root, _dir) = setup();
        let origins = |root: &Path| -> Vec<String> {
            journal::read(root, USER, 0)
                .unwrap()
                .entries
                .iter()
                .map(|e| e["origin"].as_str().unwrap().to_string())
                .collect()
        };
        write_as(
            root.path(),
            USER,
            "qa-o",
            &list("[{\"role\":\"user\",\"content\":\"q\"}]"),
            &Base::Any,
            None,
            Origin::History,
        )
        .unwrap();
        append_if_last_as(
            root.path(),
            USER,
            "qa-o",
            "q",
            &JValue::obj([("role", JValue::Str("assistant".into()))]),
            Origin::Turn,
        )
        .unwrap();
        write_as(
            root.path(),
            USER,
            "qa-i",
            &list("[1]"),
            &Base::Absent,
            None,
            Origin::Import,
        )
        .unwrap();
        assert!(remove_if_as(root.path(), USER, "qa-i", &list("[1]"), Origin::Remove).unwrap());
        // The un-suffixed calls name no origin: `other`.
        write(root.path(), USER, "qa-x", &list("[2]"), &Base::Any, None).unwrap();
        assert_eq!(
            origins(root.path()),
            vec!["history", "turn", "import", "remove", "other"]
        );
    }

    #[test]
    fn concurrent_writes_journal_in_order_with_no_gap_or_repeat() {
        let (root, _dir) = setup();
        let saved: usize = std::thread::scope(|s| {
            let handles: Vec<_> = (0..12)
                .map(|i| {
                    let root = root.path();
                    s.spawn(move || {
                        let mut n = 0;
                        for j in 0..5 {
                            let h = vec![JValue::Num(f64::from(i * 10 + j))];
                            if let Saved::Saved { .. } =
                                write(root, USER, &format!("qa-{i}"), &h, &Base::Any, None).unwrap()
                            {
                                n += 1;
                            }
                        }
                        n
                    })
                })
                .collect();
            handles.into_iter().map(|h| h.join().unwrap()).sum()
        });
        let seqs: Vec<u64> = journal(root.path(), USER).iter().map(|e| e.0).collect();
        assert_eq!(saved, 60);
        assert_eq!(seqs, (1..=60).collect::<Vec<u64>>());
    }

    #[test]
    fn one_users_journal_never_holds_another_users_changes() {
        let (root, _dir) = setup();
        std::fs::create_dir_all(store::user_dir(root.path(), OTHER).unwrap()).unwrap();
        write(root.path(), USER, "qa-mine", &list("[1]"), &Base::Any, None).unwrap();
        write(
            root.path(),
            OTHER,
            "qa-theirs",
            &list("[2]"),
            &Base::Any,
            None,
        )
        .unwrap();
        let mine = journal(root.path(), USER);
        let theirs = journal(root.path(), OTHER);
        assert_eq!((mine.len(), mine[0].1.as_str()), (1, "qa-mine"));
        assert_eq!(
            (theirs.len(), theirs[0].1.as_str(), theirs[0].0),
            (1, "qa-theirs", 1)
        );
    }
}
