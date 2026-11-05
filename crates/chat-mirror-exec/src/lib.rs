//! The work behind the chat mirror schedule (sbstndalton/noevia#741, #742, #2153): one Diary
//! mirror pass for one account and one chat's brain build, ported from noevia-core
//! `chat-vault-mirror.cjs` (sync, renderNote and the per-user index), `chat-brain.cjs`
//! (validate/parse/render, the brain store, the builder and the runner) and `chat-export.cjs`
//! (chatMarkdown). The output is core's byte for byte: the same note names, the same note text and
//! frontmatter, the same Diary calls in the same order with the same bodies, the same index file.
//!
//! The host supplies what lives elsewhere: the account's lists, tombstones and transcripts and its
//! workspace directory ([`MirrorSource`]), the Diary file API as that account ([`Diary`]) and the
//! reasoner role ([`brain::Reasoner`]).

use js_json::{wtf16, JValue};
use std::collections::HashSet;
use std::future::Future;
use std::path::Path;
use std::pin::Pin;

pub mod brain;
pub mod fsx;
pub mod jsx;
pub mod note;
pub mod sync;

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
    /// `readIndex`: the parsed index, or `{}`.
    fn read_index(&self, user: &str) -> JValue;
    fn write_index(&self, user: &str, state: &JValue) -> Result<(), String>;
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

fn read_json(file: &Path) -> Option<JValue> {
    let bytes = fsx::read_plain(file)?;
    wtf16::parse(&String::from_utf8_lossy(&bytes)).ok()
}

/// `readIndex(dir)`: `{}` when missing or unreadable.
pub fn read_index(dir: &Path) -> JValue {
    read_json(&dir.join(INDEX_FILE)).unwrap_or(JValue::Obj(Vec::new()))
}

/// `writeIndex(dir, state)`: compact JSON, temp file then rename (a failed write leaves the old
/// index whole).
pub fn write_index(dir: &Path, state: &JValue) -> Result<(), String> {
    fsx::ensure_dir(dir, 0o777)?;
    let text = wtf16::stringify(state).unwrap_or_default();
    fsx::write_atomic(&dir.join(INDEX_FILE), &jsx::utf8(&text))
}

/// `readPreferences(dir).enabled`.
pub fn preference_enabled(dir: &Path) -> bool {
    read_json(&dir.join(PREFERENCES_FILE))
        .is_some_and(|v| matches!(v.get("enabled"), JValue::Bool(true)))
}

// ── The brain runner (createBrainRunner) ────────────────────────────────────────────────────

/// What a build reads and writes for an account.
pub trait BrainSource: Send + Sync {
    /// framingReasoner AND the mirror is available AND the account opted in.
    fn enabled(&self, user: &str) -> bool;
    /// The chat's entry in the account's own lists.
    fn chat(&self, user: &str, chat: &str) -> Result<Option<JValue>, String>;
    fn history(&self, user: &str, chat: &str) -> Result<Vec<JValue>, String>;
    fn deleted(&self, user: &str) -> Result<HashSet<String>, String>;
    fn read(&self, user: &str, chat: &str) -> Option<brain::Record>;
    fn write(
        &self,
        user: &str,
        chat: &str,
        brain: &JValue,
        source_updated_at: f64,
        built_at: f64,
    ) -> Result<(), String>;
    fn remove(&self, user: &str, chat: &str);
    fn now_ms(&self) -> f64;
    fn log(&self, _line: &str) {}
}

/// One chat's brain build: `built`, `current`, `off`, `gone`, `error` or a builder fallback.
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
    let history = source.history(user, chat_id)?;
    let brain = match brain::build(reasoner, &meta, &history).await {
        Ok(b) => b,
        Err(reason) => return Ok(reason),
    };
    // Re-checked after the model call: switching the mirror off stops the write at once.
    if !source.enabled(user) {
        return Ok("off".into());
    }
    // #811: so is deletion while the model ran.
    let tombstoned = source.deleted(user)?.contains(chat_id);
    if tombstoned || source.chat(user, chat_id)?.is_none() {
        if tombstoned {
            source.remove(user, chat_id);
        }
        return Ok("gone".into());
    }
    source.write(user, chat_id, &brain, updated, source.now_ms())?;
    Ok("built".into())
}
