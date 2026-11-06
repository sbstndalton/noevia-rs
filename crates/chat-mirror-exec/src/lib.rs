//! The work behind the chat mirror schedule (sbstndalton/noevia#741, #742, #2153): one Diary
//! mirror pass for one account and one chat's brain build, ported from noevia-core
//! `chat-vault-mirror.cjs` (sync, renderNote and the per-user index), `chat-brain.cjs`
//! (validate/parse/render, the brain store, the builder and the runner) and `chat-export.cjs`
//! (chatMarkdown). Over the inputs core accepted, the output is core's (tests/oracle.rs): the same
//! note names, note text and frontmatter, the same Diary calls in the same order with the same
//! bodies, the same index file. Recorded deviations (review 2026-11-05): links, swapped account
//! folders and unreadable, corrupt or misshapen index state fail closed instead of reading as
//! empty; a Diary mutation is journalled, and after a failed one the next pass first reads that
//! path back; V8's non-ISO legacy date strings fail the note (see [`jsx::parse_date`]).
//!
//! The host supplies what lives elsewhere: the account's lists, tombstones and transcripts and its
//! workspace directory ([`MirrorSource`]), the Diary file API as that account ([`Diary`]) and the
//! reasoner role ([`brain::Reasoner`]).

use js_json::{wtf16, JValue};
use std::collections::HashSet;
use std::future::Future;
use std::pin::Pin;

pub mod brain;
pub mod jsx;
pub mod note;
pub mod root;
pub mod sync;

pub use root::{Leaf, UserRoot};

/// Grep marker for noevia deploy/tools/assemble-release.sh: this build runs the mirror pass and
/// the brain build natively (the paired Core has no executor bridge).
pub const MARKER: &str = "noevia-chat-mirror-exec/1";

pub type BoxFut<T> = Pin<Box<dyn Future<Output = T> + Send + 'static>>;

/// A failed Diary call: its HTTP status (404 = missing, 405 = folder exists) and a message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileError {
    pub status: Option<u16>,
    pub message: String,
}

/// The Diary files client (core `diaryFiles`), always as the chat's own user. Each answers the
/// reply body.
pub trait Diary: Send + Sync {
    /// POST /api/file {path}
    fn read(&self, user: &str, path: &str) -> BoxFut<Result<JValue, FileError>>;
    /// PUT /api/file {path, content, version}
    fn write(&self, user: &str, body: JValue) -> BoxFut<Result<JValue, FileError>>;
    /// POST /api/directory {path}
    fn mkdir(&self, user: &str, path: &str) -> BoxFut<Result<JValue, FileError>>;
    /// POST /api/workspace-ops {op, ...}
    fn ops(&self, user: &str, body: JValue) -> BoxFut<Result<JValue, FileError>>;
}

/// The account's own chat lists (internal projects already left out).
#[derive(Debug, Clone, Default)]
pub struct Lists {
    pub free_chats: Vec<JValue>,
    pub projects: Vec<JValue>,
}

/// What a pass reads from the account's own workspace. `Err` is a throw in core (the pass fails
/// before the index is saved).
pub trait MirrorSource: Send + Sync {
    fn lists(&self, user: &str) -> Result<Lists, String>;
    /// The account's tombstoned chat ids (deleted-chats.json).
    fn deleted(&self, user: &str) -> Result<HashSet<String>, String>;
    fn history(&self, user: &str, chat: &str) -> Result<Vec<JValue>, String>;
    /// The chat's stored, validated brain, if any.
    fn brain(&self, user: &str, chat: &str) -> Option<JValue>;
    /// The parsed, checked index: `{}` only when there is none. Unreadable, linked, corrupt or
    /// misshapen state is an `Err` (the pass fails before any Diary call; core read it as `{}`).
    fn read_index(&self, user: &str) -> Result<JValue, String>;
    fn write_index(&self, user: &str, state: &JValue) -> Result<(), String>;
    /// The intent journal: what the last pass was about to do to the Diary and may or may not
    /// have done (`{chatId: [intent, ...]}`, `{}` when none). Fail closed like the index.
    fn read_intents(&self, user: &str) -> Result<JValue, String>;
    /// Saves the journal durably (an empty one removes the file) before the Diary call it covers.
    fn write_intents(&self, user: &str, intents: &JValue) -> Result<(), String>;
    fn log(&self, _line: &str) {}
}

/// One reasoner call.
#[derive(Debug, Clone, PartialEq)]
pub struct Completion {
    pub model: String,
    pub schema: JValue,
    pub messages: JValue,
}

// ── The per-user index (chat-vault-mirror-index.json) ───────────────────────────────────────

pub const PREFERENCES_FILE: &str = "chat-vault-mirror.json";
pub const INDEX_FILE: &str = "chat-vault-mirror-index.json";
pub const INTENT_FILE: &str = "chat-vault-mirror-intent.json";
/// Larger than any index a pass writes (one short entry per chat); a larger file is refused.
pub const MAX_STATE_BYTES: u64 = 32 * 1024 * 1024;

/// A state file of the held account folder: `None` when missing, `Err` when it cannot be read
/// as JSON text (a link, not a regular file, too large, not UTF-8, not JSON).
fn read_state(root: &UserRoot, file: &str) -> Result<Option<JValue>, String> {
    match root.read(None, file, MAX_STATE_BYTES)? {
        Leaf::Missing => Ok(None),
        Leaf::File { over: true, .. } => Err(format!("refused: {file} is too large")),
        Leaf::File { bytes, .. } => {
            let text =
                String::from_utf8(bytes).map_err(|_| format!("refused: {file} is not text"))?;
            wtf16::parse(&text)
                .map(Some)
                .map_err(|_| format!("refused: {file} is not JSON"))
        }
    }
}

/// The index as a pass may act on it: an object whose `notes` (if present) is an object, every
/// chat entry of which is an object with a text `path` inside the mirror folder. A `taken:` entry
/// is only a name reservation (pruned by the pass when invalid, never read or moved).
pub fn check_index(state: &JValue) -> Result<(), String> {
    if !matches!(state, JValue::Obj(_)) {
        return Err("refused: the index is not an object".into());
    }
    let notes = match state.get("notes") {
        JValue::Undefined => return Ok(()),
        JValue::Obj(items) => items,
        _ => return Err("refused: the index notes are not an object".into()),
    };
    for (key, entry) in notes {
        if key.starts_with("taken:") {
            continue;
        }
        if !matches!(entry, JValue::Obj(_)) || !note::inside_mirror(entry.get("path")) {
            return Err(format!(
                "refused: the index entry of {key} is not a mirrored note"
            ));
        }
    }
    Ok(())
}

/// `readIndex(dir)`, fail closed: `{}` only when there is no index.
pub fn read_index(root: &UserRoot) -> Result<JValue, String> {
    let state = read_state(root, INDEX_FILE)?.unwrap_or(JValue::Obj(Vec::new()));
    check_index(&state)?;
    Ok(state)
}

/// `writeIndex(dir, state)`: compact JSON, temp file then rename within the held folder (a failed
/// write leaves the old index whole).
pub fn write_index(root: &UserRoot, state: &JValue) -> Result<(), String> {
    let text = wtf16::stringify(state).ok_or("index not serialisable")?;
    root.write(None, 0o777, INDEX_FILE, &jsx::utf8(&text))
}

/// The intent journal, fail closed: `{}` only when there is none.
pub fn read_intents(root: &UserRoot) -> Result<JValue, String> {
    let state = read_state(root, INTENT_FILE)?.unwrap_or(JValue::Obj(Vec::new()));
    match &state {
        JValue::Obj(items) if items.iter().all(|(_, v)| matches!(v, JValue::Arr(_))) => Ok(state),
        _ => Err("refused: the intent journal is misshapen".into()),
    }
}

/// Saves the intent journal (fsynced); an empty journal removes the file.
pub fn write_intents(root: &UserRoot, intents: &JValue) -> Result<(), String> {
    if matches!(intents, JValue::Obj(items) if items.is_empty()) {
        return root.remove(None, INTENT_FILE);
    }
    let text = wtf16::stringify(intents).ok_or("intents not serialisable")?;
    root.write(None, 0o777, INTENT_FILE, &jsx::utf8(&text))
}

/// `readPreferences(dir).enabled` (off when unreadable).
pub fn preference_enabled(root: &UserRoot) -> bool {
    read_state(root, PREFERENCES_FILE)
        .ok()
        .flatten()
        .is_some_and(|v| matches!(v.get("enabled"), JValue::Bool(true)))
}

// ── The brain runner (createBrainRunner) ────────────────────────────────────────────────────

/// What a build reads and writes for an account.
pub trait BrainSource: Send + Sync {
    /// framingReasoner AND the mirror is available AND the account opted in.
    fn enabled(&self, user: &str) -> bool;
    /// The chat's entry in the account's own lists.
    fn chat(&self, user: &str, chat: &str) -> Result<Option<JValue>, String>;
    /// The transcript the model will read and the store's revision of it, read together.
    fn snapshot(&self, user: &str, chat: &str) -> Result<(Vec<JValue>, String), String>;
    fn deleted(&self, user: &str) -> Result<HashSet<String>, String>;
    fn read(&self, user: &str, chat: &str) -> Option<brain::Record>;
    /// Compare-and-swap at the store: under the locks every transcript save, chat delete and chat
    /// list change holds, the chat is read again; the brain is written only when the chat is still
    /// listed with `expected`'s `updatedAt` and transcript revision. A tombstoned chat loses its
    /// stored brain there.
    fn write_if(
        &self,
        user: &str,
        chat: &str,
        expected: &Revision,
        brain: &JValue,
        source_updated_at: f64,
        built_at: f64,
    ) -> Result<Cas, String>;
    fn remove(&self, user: &str, chat: &str);
    fn now_ms(&self) -> f64;
    fn log(&self, _line: &str) {}
}

/// What the model's input was taken from: the chat's `updatedAt` in its list and the store's
/// transcript revision.
#[derive(Debug, Clone, PartialEq)]
pub struct Revision {
    pub updated_at: JValue,
    pub transcript: String,
}

/// What [`BrainSource::write_if`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cas {
    Written,
    /// The chat changed since the snapshot: nothing written.
    Stale,
    /// The chat is no longer listed (deleted, tombstoned or moved away): nothing written.
    Gone,
}

/// One chat's brain build: `built`, `current`, `off`, `gone`, `stale` (the chat changed while the
/// model ran: nothing written, built again later), `error` or a builder fallback.
pub async fn run_brain(
    user: &str,
    chat_id: &str,
    source: &dyn BrainSource,
    reasoner: &dyn brain::Reasoner,
) -> String {
    match run_brain_inner(user, chat_id, source, reasoner).await {
        Ok(r) => r,
        Err(e) => {
            source.log(&format!("chat-brain: build failed {e}"));
            "error".into()
        }
    }
}

async fn run_brain_inner(
    user: &str,
    chat_id: &str,
    source: &dyn BrainSource,
    reasoner: &dyn brain::Reasoner,
) -> Result<String, String> {
    if !source.enabled(user) {
        return Ok("off".into());
    }
    let Some(meta) = source.chat(user, chat_id)? else {
        // Deleted (tombstoned) chats lose their brain; merely absent ones keep it.
        if source.deleted(user)?.contains(chat_id) {
            source.remove(user, chat_id);
        }
        return Ok("gone".into());
    };
    let updated = jsx::to_number(meta.get("updatedAt"));
    let updated = if updated.is_nan() { 0.0 } else { updated };
    if let Some(stored) = source.read(user, chat_id) {
        #[allow(clippy::float_cmp)]
        if updated != 0.0 && stored.source_updated_at == updated {
            return Ok("current".into());
        }
    }
    let (history, transcript) = source.snapshot(user, chat_id)?;
    let expected = Revision {
        updated_at: meta.get("updatedAt").clone(),
        transcript,
    };
    let brain = match brain::build(reasoner, &meta, &history).await {
        Ok(b) => b,
        Err(reason) => return Ok(reason),
    };
    // Re-checked after the model call: switching the mirror off stops the write at once.
    if !source.enabled(user) {
        return Ok("off".into());
    }
    // #811 and the stale-publication fence (root ruling 2026-11-05; core checked deletion only,
    // and outside any lock): the write is a compare-and-swap at the store.
    Ok(
        match source.write_if(user, chat_id, &expected, &brain, updated, source.now_ms())? {
            Cas::Written => "built",
            Cas::Stale => "stale",
            Cas::Gone => "gone",
        }
        .into(),
    )
}
