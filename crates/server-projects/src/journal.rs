//! A user's chat change journal, `chat-changes.jsonl` (sbstndalton/noevia#2153, #1071 slice 5a,
//! decision D9). Rust writes every chat transcript ([`crate::transcript`]); Node still owns what a
//! changed transcript sets off (the Diary chat mirror and the per-chat brain) until those move too.
//! So that Node learns of a change it did not make, every successful transcript mutation appends
//! one line here, in the same hold of the user's `chat-transcripts.lock`:
//!
//! `{"seq":7,"chatId":"c1","revision":"<sha256 hex>"|null,"op":"write"|"append"|"remove","origin":"history"|"import"|"turn"|"remove"|"other","start":"<NOEVIA_START_ID>","at":<ms>}`
//!
//! - `origin` is who asked for the change ([`Origin`]): Node builds a chat's brain only for a
//!   `write` whose origin is `history` (the user's own save), never for an import, an
//!   interrupted-turn append, a removal or anything unnamed (`other`). Entries written before the
//!   field existed carry none; readers treat them as mirror-only.
//!
//! - `seq` is monotonic per user and survives restarts: the next one is the last line's plus one
//!   (read under the lock, so two writers never take the same number).
//! - Bounded: once the file passes [`MAX_BYTES`] it is rewritten, under the same lock and
//!   atomically (temp file and rename), to its newest [`KEEP`] entries. The rewrite replaces the
//!   file's content; it removes no user data (the journal only points at transcripts).
//! - A torn last line (a crash mid-append) is never read as an entry: readers take only lines that
//!   end in a newline and parse, and the next append starts on a fresh line.
//! - Readers take no lock: appends are single `O_APPEND` writes and a rotation is a rename.
//!
//! Node is the only reader, through the internal project store (`POST /v1/changes/read`); see
//! [`read`] for the cursor contract.

use crate::store::{self, StoreError};
use serde_json::{json, Value};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;
use std::sync::OnceLock;
use std::time::{SystemTime, UNIX_EPOCH};

pub const FILE: &str = "chat-changes.jsonl";
/// Past this size the journal is rotated (a line is about 200 bytes: some 1,300 entries).
pub const MAX_BYTES: u64 = 256 * 1024;
/// Entries a rotation keeps.
pub const KEEP: usize = 500;
/// The most entries one read answers (`more`: there are newer ones; read again after the last).
pub const READ_LIMIT: usize = 1000;
/// How much of the file's end is read to find the last sequence number before a full scan.
const TAIL_BYTES: u64 = 16 * 1024;

static START: OnceLock<String> = OnceLock::new();

/// This process's NOEVIA_START_ID, stamped on every entry. Set once at start (main.rs); later
/// calls are ignored.
pub fn set_start_id(id: &str) {
    let _ = START.set(id.to_string());
}

fn start_id() -> &'static str {
    START.get().map(String::as_str).unwrap_or("")
}

/// What changed the transcript.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Op {
    Write,
    Append,
    Remove,
    /// A metadata move with a durable event identity (not a transcript save).
    Move,
}

impl Op {
    pub fn as_str(self) -> &'static str {
        match self {
            Op::Write => "write",
            Op::Append => "append",
            Op::Remove => "remove",
            Op::Move => "move",
        }
    }
}

/// Who asked for a change: the closed set a store caller may name (anything else is refused).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Origin {
    /// The user's own save of the chat (the history POST).
    History,
    /// The conversation import publishing a transcript.
    Import,
    /// An interrupted reply's turn appended after the user's message.
    Turn,
    /// The conversation import taking back a transcript it published.
    Remove,
    /// Anything else, and a caller that names none.
    Other,
}

impl Origin {
    pub fn as_str(self) -> &'static str {
        match self {
            Origin::History => "history",
            Origin::Import => "import",
            Origin::Turn => "turn",
            Origin::Remove => "remove",
            Origin::Other => "other",
        }
    }

    /// The origin `s` names, or nothing (not one of the closed set).
    pub fn parse(s: &str) -> Option<Origin> {
        match s {
            "history" => Some(Origin::History),
            "import" => Some(Origin::Import),
            "turn" => Some(Origin::Turn),
            "remove" => Some(Origin::Remove),
            "other" => Some(Origin::Other),
            _ => None,
        }
    }
}

/// Rotation thresholds (tests use small ones).
#[derive(Debug, Clone, Copy)]
pub struct Limits {
    pub max_bytes: u64,
    pub keep: usize,
}

pub const LIMITS: Limits = Limits {
    max_bytes: MAX_BYTES,
    keep: KEEP,
};

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

/// A complete line's entry (its `seq` a positive integer), or nothing.
fn parse_line(line: &[u8]) -> Option<(u64, Value)> {
    let v: Value = serde_json::from_slice(line).ok()?;
    let seq = v.get("seq")?.as_u64().filter(|s| *s > 0)?;
    Some((seq, v))
}

/// The complete lines' entries, in file order (a final line without its newline is torn).
fn entries_of(bytes: &[u8]) -> Vec<(u64, Value)> {
    let complete = match bytes.iter().rposition(|b| *b == b'\n') {
        Some(end) => bytes.get(..end).unwrap_or(&[]),
        None => return Vec::new(),
    };
    complete
        .split(|b| *b == b'\n')
        .filter_map(parse_line)
        .collect()
}

fn read_all(file: &Path) -> Result<Vec<u8>, StoreError> {
    match std::fs::read(file) {
        Ok(b) => Ok(b),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(e) => Err(StoreError::Io(e)),
    }
}

/// The highest sequence number in the journal (0: none), and whether the file ends mid-line.
fn last_seq(file: &Path) -> Result<(u64, bool), StoreError> {
    let mut f = match std::fs::File::open(file) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok((0, false)),
        Err(e) => return Err(StoreError::Io(e)),
    };
    let len = f.metadata().map_err(StoreError::Io)?.len();
    if len == 0 {
        return Ok((0, false));
    }
    let from = len.saturating_sub(TAIL_BYTES);
    f.seek(SeekFrom::Start(from)).map_err(StoreError::Io)?;
    let mut tail = Vec::new();
    f.read_to_end(&mut tail).map_err(StoreError::Io)?;
    let torn = tail.last() != Some(&b'\n');
    // The tail's first line may be cut by the seek; a parse failure skips it.
    let seen = entries_of(&tail).iter().map(|(s, _)| *s).max();
    if let Some(s) = seen {
        return Ok((s, torn));
    }
    // Nothing whole in the tail (garbage, or one huge line): scan everything.
    let all = read_all(file)?;
    Ok((
        entries_of(&all).iter().map(|(s, _)| *s).max().unwrap_or(0),
        torn,
    ))
}

/// Appends one entry for `chat` to the journal in `dir`. The caller holds the user's transcript
/// lock (the sequence number is read and taken in that hold).
pub(crate) fn record(
    dir: &Path,
    chat: &str,
    revision: Option<&str>,
    op: Op,
    origin: Origin,
    limits: Limits,
) -> Result<u64, StoreError> {
    record_entry(dir, chat, revision, op, origin, limits, None)
}

pub(crate) fn record_move(
    dir: &Path,
    chat: &str,
    event_id: &str,
    generation: &str,
) -> Result<u64, StoreError> {
    let seq = record_entry(
        dir,
        chat,
        None,
        Op::Move,
        Origin::Other,
        LIMITS,
        Some((event_id, generation)),
    )?;
    // A move's prepared intent is not cleared until both the appended/rotated file and parent
    // are durable. Legacy transcript notifications keep their existing best-effort behavior.
    std::fs::File::open(dir.join(FILE))?.sync_all()?;
    std::fs::File::open(dir)?.sync_all()?;
    Ok(seq)
}

fn record_entry(
    dir: &Path,
    chat: &str,
    revision: Option<&str>,
    op: Op,
    origin: Origin,
    limits: Limits,
    moved: Option<(&str, &str)>,
) -> Result<u64, StoreError> {
    let file = dir.join(FILE);
    let (last, torn) = last_seq(&file)?;
    let seq = last.checked_add(1).ok_or(StoreError::Corrupt)?;
    let mut entry = json!({
        "seq": seq,
        "chatId": chat,
        "revision": revision,
        "op": op.as_str(),
        "origin": origin.as_str(),
        "start": start_id(),
        "at": now_ms(),
    });
    if let Some((event_id, generation)) = moved {
        let obj = entry.as_object_mut().ok_or(StoreError::Corrupt)?;
        obj.insert("eventId".into(), Value::String(event_id.into()));
        obj.insert("generation".into(), Value::String(generation.into()));
    }
    let mut line = String::new();
    if torn {
        line.push('\n');
    }
    line.push_str(&entry.to_string());
    line.push('\n');
    let mut options = std::fs::OpenOptions::new();
    options.append(true).create(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    }
    let mut f = options.open(&file).map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            StoreError::Gone
        } else {
            StoreError::Io(e)
        }
    })?;
    f.write_all(line.as_bytes()).map_err(StoreError::Io)?;
    let len = f.metadata().map_err(StoreError::Io)?.len();
    drop(f);
    if len > limits.max_bytes {
        rotate(&file, limits.keep)?;
    }
    Ok(seq)
}

/// Rewrites the journal to its newest `keep` entries (torn or unparsable lines dropped).
fn rotate(file: &Path, keep: usize) -> Result<(), StoreError> {
    let all = entries_of(&read_all(file)?);
    let from = all.len().saturating_sub(keep);
    let mut text = String::new();
    for (_, v) in all.get(from..).unwrap_or(&[]) {
        text.push_str(&v.to_string());
        text.push('\n');
    }
    crate::transcript::write_atomic(file, &text)
}

/// One read of the journal.
#[derive(Debug, Clone, PartialEq)]
pub struct Changes {
    /// The entries after the cursor, oldest first, at most [`READ_LIMIT`].
    pub entries: Vec<Value>,
    /// The oldest entry still kept (`None`: the journal is empty). A reader whose cursor is below
    /// `oldest_seq - 1` missed entries to a rotation and must rescan.
    pub oldest_seq: Option<u64>,
    /// The newest entry (`None`: empty). Below the reader's cursor: the journal was reset; rescan.
    pub latest_seq: Option<u64>,
    /// More entries follow the last one answered.
    pub more: bool,
}

/// The entries of `user`'s journal with `seq > after`. The user's directory must exist (a removed
/// account is [`StoreError::Gone`]); the user id names only that directory ([`store::user_dir`]).
pub fn read(data_dir: &Path, user: &str, after: u64) -> Result<Changes, StoreError> {
    let dir = store::user_dir(data_dir, user)?;
    crate::transcript::existing_dir(&dir)?;
    let all = entries_of(&read_all(&dir.join(FILE))?);
    let oldest_seq = all.first().map(|(s, _)| *s);
    let latest_seq = all.last().map(|(s, _)| *s);
    let mut newer = all.into_iter().filter(|(s, _)| *s > after);
    let entries: Vec<Value> = newer.by_ref().take(READ_LIMIT).map(|(_, v)| v).collect();
    let more = newer.next().is_some();
    Ok(Changes {
        entries,
        oldest_seq,
        latest_seq,
        more,
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
mod tests {
    use super::*;

    const USER: &str = "11111111-2222-4333-8444-555555555555";

    fn setup() -> (tempfile::TempDir, std::path::PathBuf) {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("users").join(USER);
        std::fs::create_dir_all(&dir).unwrap();
        (root, dir)
    }

    fn seqs(c: &Changes) -> Vec<u64> {
        c.entries
            .iter()
            .map(|e| e["seq"].as_u64().unwrap())
            .collect()
    }

    #[test]
    fn entries_carry_the_fields_and_seq_counts_up() {
        set_start_id("0123456789abcdef0123456789abcdef");
        let (root, dir) = setup();
        assert_eq!(
            record(
                &dir,
                "qa-chat",
                Some("r1"),
                Op::Write,
                Origin::History,
                LIMITS
            )
            .unwrap(),
            1
        );
        assert_eq!(
            record(&dir, "qa-chat", None, Op::Remove, Origin::Other, LIMITS).unwrap(),
            2
        );
        let c = read(root.path(), USER, 0).unwrap();
        assert_eq!(seqs(&c), vec![1, 2]);
        let e = &c.entries[0];
        assert_eq!(e["chatId"], "qa-chat");
        assert_eq!(e["revision"], "r1");
        assert_eq!(e["op"], "write");
        assert_eq!(e["origin"], "history");
        assert_eq!(c.entries[1]["origin"], "other");
        assert_eq!(e["start"], start_id());
        assert!(e["at"].as_u64().unwrap() > 0);
        assert_eq!(c.entries[1]["revision"], Value::Null);
        assert_eq!(
            (c.oldest_seq, c.latest_seq, c.more),
            (Some(1), Some(2), false)
        );
    }

    #[test]
    fn origin_is_a_closed_set_that_round_trips() {
        for o in [
            Origin::History,
            Origin::Import,
            Origin::Turn,
            Origin::Remove,
            Origin::Other,
        ] {
            assert_eq!(Origin::parse(o.as_str()), Some(o));
        }
        for bad in ["", "History", "brain", "write", " history"] {
            assert_eq!(Origin::parse(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn the_cursor_answers_only_newer_entries() {
        let (root, dir) = setup();
        let empty = read(root.path(), USER, 0).unwrap();
        assert_eq!(
            (empty.entries.len(), empty.oldest_seq, empty.latest_seq),
            (0, None, None)
        );
        for i in 0..5 {
            record(
                &dir,
                &format!("qa-{i}"),
                Some("r"),
                Op::Write,
                Origin::Other,
                LIMITS,
            )
            .unwrap();
        }
        assert_eq!(seqs(&read(root.path(), USER, 3).unwrap()), vec![4, 5]);
        assert!(read(root.path(), USER, 5).unwrap().entries.is_empty());
        assert!(read(root.path(), USER, 99).unwrap().entries.is_empty());
        assert_eq!(read(root.path(), USER, 99).unwrap().latest_seq, Some(5));
    }

    #[test]
    fn rotation_keeps_the_newest_and_the_sequence_goes_on() {
        let (root, dir) = setup();
        let small = Limits {
            max_bytes: 2_000,
            keep: 4,
        };
        for i in 0..40 {
            record(
                &dir,
                &format!("qa-{i}"),
                Some("r"),
                Op::Append,
                Origin::Other,
                small,
            )
            .unwrap();
        }
        let len = std::fs::metadata(dir.join(FILE)).unwrap().len();
        assert!(len <= 2_000 + 400, "bounded: {len}");
        let c = read(root.path(), USER, 0).unwrap();
        assert_eq!(c.latest_seq, Some(40));
        assert!(c.oldest_seq.unwrap() > 1, "rotated: {:?}", c.oldest_seq);
        let s = seqs(&c);
        assert!(s.windows(2).all(|w| w[1] == w[0] + 1), "{s:?}");
        // A reader at 2 fell behind the rotation: oldest_seq tells it so.
        assert!(c.oldest_seq.unwrap() > 2 + 1);
        // No temp file is left beside it.
        let names: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        assert_eq!(names, vec![FILE.to_string()]);
    }

    #[test]
    fn a_torn_last_line_is_skipped_and_the_next_entry_starts_clean() {
        let (root, dir) = setup();
        record(&dir, "qa-a", Some("r"), Op::Write, Origin::Other, LIMITS).unwrap();
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(dir.join(FILE))
            .unwrap();
        f.write_all(b"{\"seq\":2,\"chatId\":\"qa-to").unwrap();
        drop(f);
        assert_eq!(seqs(&read(root.path(), USER, 0).unwrap()), vec![1]);
        assert_eq!(
            record(&dir, "qa-b", Some("r"), Op::Write, Origin::Other, LIMITS).unwrap(),
            2
        );
        let c = read(root.path(), USER, 0).unwrap();
        assert_eq!(seqs(&c), vec![1, 2]);
        assert_eq!(c.entries[1]["chatId"], "qa-b");
    }

    #[test]
    fn a_read_is_bounded_and_says_more_follow() {
        let (root, dir) = setup();
        let big = Limits {
            max_bytes: u64::MAX,
            keep: usize::MAX,
        };
        for _ in 0..READ_LIMIT + 3 {
            record(&dir, "qa", None, Op::Write, Origin::Other, big).unwrap();
        }
        let c = read(root.path(), USER, 0).unwrap();
        assert_eq!((c.entries.len(), c.more), (READ_LIMIT, true));
        let rest = read(root.path(), USER, READ_LIMIT as u64).unwrap();
        assert_eq!((rest.entries.len(), rest.more), (3, false));
    }

    #[test]
    fn the_user_id_is_checked_and_a_removed_account_is_gone() {
        let (root, _dir) = setup();
        assert!(matches!(
            read(root.path(), "../../etc", 0),
            Err(StoreError::InvalidUser)
        ));
        assert!(matches!(
            read(root.path(), "99999999-2222-4333-8444-555555555555", 0),
            Err(StoreError::Gone)
        ));
    }
}
