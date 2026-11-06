//! A chat's saved partial reply (core chat-partial.cjs, #1507) as another process reads it: is the
//! reply that wrote it still running, or was it cut off? sbstndalton/noevia#1071:
//! `GET /api/chats/{chatId}/history` ([`crate::history`]) uses it in place of the in-memory set
//! Node's chat turn keeps.
//!
//! - The file is `chat-partials/<sha256 hex of the chat id>.json` in the user's directory, the id
//!   being the one the history route reads (chat-lists.cjs `safeChatId`, `[A-Za-z0-9_-]` only).
//! - While a reply runs, Node rewrites it every few seconds with `running: { start }`, `start`
//!   being NOEVIA_START_ID, the web supervisor's id for this container start; the reply's end
//!   removes the file. So a marker naming the current start is a reply still running here; no
//!   marker (a file from before the marker), a malformed one or another start's is a reply a
//!   crash or restart cut off, which the history offers as interrupted.
//! - The marker is internal: [`offered`] is what the client sees, without it.

use js_json::JValue;
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

/// chat-partial.cjs `DIR`.
pub const DIR: &str = "chat-partials";

/// What a saved partial says about its reply.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    /// Written by a reply of this container start that has not ended: not offered.
    Running,
    /// Left by a reply that can no longer finish (another start's, or unmarked): offered.
    Interrupted,
}

/// A start id as the web supervisor makes it: 32 lowercase hex characters.
pub fn valid_start_id(id: &str) -> bool {
    id.len() == 32 && id.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// chat-partial.cjs `fileFor(dir, chatId)`: `sha256(String(chatId))` in hex, under [`DIR`].
pub fn file_for(user_dir: &Path, chat_id: &str) -> PathBuf {
    let hex: String = Sha256::digest(chat_id.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    user_dir.join(DIR).join(format!("{hex}.json"))
}

/// Whether the reply that wrote `partial` is still running, given this start's id. Only an
/// object marker whose `start` is exactly `current_start` (itself a valid id) is running.
pub fn state(partial: &JValue, current_start: &str) -> State {
    let start = partial.get("running").get("start").as_str();
    if valid_start_id(current_start) && start == Some(current_start) {
        State::Running
    } else {
        State::Interrupted
    }
}

/// chat-partial.cjs `partialFor`'s answer for a partial that is offered: the stored object
/// without `running`. `None` for a value readPartial does not take (not a plain object).
pub fn offered(partial: JValue) -> Option<JValue> {
    match partial {
        JValue::Obj(items) => Some(JValue::Obj(
            items.into_iter().filter(|(k, _)| k != "running").collect(),
        )),
        _ => None,
    }
}

/// chat-partial.cjs `PARTIAL_SAVE_MS`: how often a running reply's partial is rewritten.
pub const SAVE_MS: u64 = 4000;
/// chat-partial.cjs `PARTIAL_CAP_CHARS`: per text field, the newest UTF-16 units are kept.
pub const CAP_UNITS: usize = 64 * 1024;

/// `s.slice(-n)` over UTF-16 units: a pair cut in half keeps its low surrogate, as in JS.
fn tail_units(s: &str, n: usize) -> String {
    let len = js_json::wtf16::len(s);
    if len <= n {
        return s.to_owned();
    }
    let mut skip = len - n;
    let mut out = String::new();
    for c in s.chars() {
        let w = if js_json::wtf16::lone_unit(c).is_some() {
            1
        } else {
            c.len_utf16()
        };
        if skip >= w {
            skip -= w;
            continue;
        }
        if skip > 0 {
            let mut buf = [0u16; 2];
            if let Some(m) = c
                .encode_utf16(&mut buf)
                .get(1)
                .copied()
                .and_then(js_json::wtf16::lone_char)
            {
                out.push(m);
            }
            skip = 0;
            continue;
        }
        out.push(c);
    }
    out
}

/// chat-partial.cjs `bounded(entry)`: the text fields capped, `savedAt` stamped (`now_ms`).
pub fn bounded(entry: &JValue, now_ms: f64) -> JValue {
    let mut items = match entry {
        JValue::Obj(items) => items.clone(),
        _ => Vec::new(),
    };
    let trim = |v: &JValue| match v {
        JValue::Str(t) if js_json::wtf16::len(t) > CAP_UNITS => {
            JValue::Str(tail_units(t, CAP_UNITS))
        }
        other => other.clone(),
    };
    let content = match entry.get("content") {
        JValue::Undefined | JValue::Null => JValue::Str(String::new()),
        v => v.clone(),
    };
    js_json::wtf16::set(&mut items, "content", trim(&content));
    js_json::wtf16::set(&mut items, "savedAt", JValue::Num(now_ms));
    if let JValue::Str(_) = entry.get("reasoning") {
        js_json::wtf16::set(&mut items, "reasoning", trim(entry.get("reasoning")));
    }
    if matches!(entry.get("content"), JValue::Str(t) if js_json::wtf16::len(t) > CAP_UNITS) {
        js_json::wtf16::set(&mut items, "truncated", JValue::Bool(true));
    }
    JValue::Obj(items)
}

fn with(mut v: JValue, extra: Vec<(&str, JValue)>) -> JValue {
    if let JValue::Obj(items) = &mut v {
        for (k, x) in extra {
            js_json::wtf16::set(items, k, x);
        }
    }
    v
}

/// A fresh temporary name beside `file` (review P2.17: a shared `<pid>.tmp` let one writer truncate or
/// rename another's), created new.
fn tmp_for(file: &Path) -> PathBuf {
    use rand_core::{OsRng, RngCore};
    let mut random = [0u8; 12];
    OsRng.fill_bytes(&mut random);
    let nonce: String = random.iter().map(|b| format!("{b:02x}")).collect();
    let mut name = file.as_os_str().to_owned();
    name.push(format!(".{}-{nonce}.tmp", std::process::id()));
    PathBuf::from(name)
}

/// Writes `text` to a new temporary file, syncs it, and renames it over `file`; the temporary is
/// removed when any step fails.
fn replace_atomically(file: &Path, text: &str) -> std::io::Result<()> {
    use std::io::Write;
    let tmp = tmp_for(file);
    let written = (|| {
        let mut out = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp)?;
        out.write_all(text.as_bytes())?;
        out.sync_all()?;
        std::fs::rename(&tmp, file)
    })();
    if written.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    written?;
    if let Some(parent) = file.parent() {
        let _ = std::fs::File::open(parent).and_then(|d| d.sync_all());
    }
    Ok(())
}

/// The `chat-partials` folder under an existing user directory (a deleted account's directory is
/// never re-created by a late save).
fn partials_dir(file: &Path) -> std::io::Result<()> {
    let Some(parent) = file.parent() else {
        return Ok(());
    };
    match parent.parent() {
        Some(user) if !user.is_dir() => Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "the user directory is gone",
        )),
        _ => match std::fs::create_dir(parent) {
            Err(e) if e.kind() != std::io::ErrorKind::AlreadyExists => Err(e),
            _ => Ok(()),
        },
    }
}

/// chat-partial.cjs `writePartial`: the bounded entry, marked interrupted and running in `start`.
pub fn write_partial(
    user_dir: &Path,
    chat_id: &str,
    entry: &JValue,
    start: &str,
    now_ms: f64,
) -> std::io::Result<()> {
    let file = file_for(user_dir, chat_id);
    partials_dir(&file)?;
    let value = with(
        bounded(entry, now_ms),
        vec![
            ("interrupted", JValue::Bool(true)),
            (
                "running",
                JValue::Obj(vec![("start".into(), JValue::Str(start.into()))]),
            ),
        ],
    );
    replace_atomically(
        &file,
        &js_json::wtf16::stringify(&value).unwrap_or_default(),
    )
}

/// chat-partial.cjs `clearPartial` (`rmSync(..., {force: true})`).
pub fn clear_partial(user_dir: &Path, chat_id: &str) -> std::io::Result<()> {
    match std::fs::remove_file(file_for(user_dir, chat_id)) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        other => other,
    }
}

/// chat-partial.cjs `readPartial`: the stored plain object, or `None`.
pub fn read_partial(user_dir: &Path, chat_id: &str) -> Option<JValue> {
    let text = std::fs::read_to_string(file_for(user_dir, chat_id)).ok()?;
    match js_json::wtf16::parse(&text).ok()? {
        v @ JValue::Obj(_) => Some(v),
        _ => None,
    }
}

/// chat-partial.cjs `writeUnmarked`: atomically when the folder allows it, else in place.
fn write_unmarked(file: &Path, text: &str) -> &'static str {
    let attempt = || -> std::io::Result<()> {
        partials_dir(file)?;
        if replace_atomically(file, text).is_err() {
            std::fs::write(file, text)?;
        }
        Ok(())
    };
    match attempt() {
        Ok(()) => "unmarked",
        Err(e) => {
            eprintln!("[chat] could not rewrite a finished reply's partial ({}) either; its running marker stays until the next start, and the history hides it until then: {e}", file.display());
            "stuck"
        }
    }
}

/// chat-partial.cjs `endPartial` (#2146): cleared, or rewritten without this start's marker.
pub fn end_partial(user_dir: &Path, chat_id: &str) -> &'static str {
    let file = file_for(user_dir, chat_id);
    match clear_partial(user_dir, chat_id) {
        Ok(()) => return "cleared",
        Err(e) => eprintln!("[chat] could not remove a finished reply's partial ({}); rewriting it without the running marker: {e}", file.display()),
    }
    let Some(JValue::Obj(items)) = read_partial(user_dir, chat_id) else {
        return "kept";
    };
    if !items.iter().any(|(k, _)| k == "running") {
        return "kept";
    }
    let ended = JValue::Obj(items.into_iter().filter(|(k, _)| k != "running").collect());
    write_unmarked(
        &file,
        &js_json::wtf16::stringify(&ended).unwrap_or_default(),
    )
}

/// The turns whose partial saves may publish: one generation per chat partial file, in this process.
/// Every write, rename and removal of a [`Saves`] happens under this lock after its generation is
/// checked, so a superseded turn (a newer turn of the chat, or the chat's deletion: [`retire`]) can
/// neither publish nor clean up, and two ticks never interleave.
fn owners() -> std::sync::MutexGuard<'static, std::collections::HashMap<PathBuf, u64>> {
    static OWNERS: std::sync::OnceLock<std::sync::Mutex<std::collections::HashMap<PathBuf, u64>>> =
        std::sync::OnceLock::new();
    OWNERS
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn next_generation() -> u64 {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

/// Any running turn's saves of the chat are superseded: none of them writes or removes its partial
/// again. The store's chat delete calls this before it removes the file (a save in progress finishes
/// first, under the same lock, and its file is then removed with the chat).
pub fn supersede(user_dir: &Path, chat_id: &str) {
    owners().remove(&file_for(user_dir, chat_id));
}

/// chat-partial.cjs `startPartialSaves`: a running reply's partial. [`Saves::save`] is the timer's
/// tick (every [`SAVE_MS`]); [`Saves::finish`] ends it.
///
/// Dropped without `finish` (the turn's future was cancelled, or panicked, before its end ran), the
/// last saved entry is kept, rewritten without this start's marker, so the history offers it as an
/// interrupted turn with any write it records (review P2.17: it was deleted). Only an explicit
/// `finish` clears it.
pub struct Saves {
    dir: PathBuf,
    chat: String,
    file: PathBuf,
    start: String,
    generation: u64,
    /// The bounded entry last written (without the marker), kept for a drop.
    last: std::sync::Mutex<Option<JValue>>,
    done: bool,
}

impl Saves {
    /// Supersedes an earlier partial of the chat, and any turn of it still saving.
    pub fn start(user_dir: &Path, chat_id: &str, start: &str) -> std::io::Result<Saves> {
        let file = file_for(user_dir, chat_id);
        let generation = next_generation();
        let mut owners = owners();
        clear_partial(user_dir, chat_id)?;
        owners.insert(file.clone(), generation);
        drop(owners);
        Ok(Saves {
            dir: user_dir.to_owned(),
            chat: chat_id.to_owned(),
            file,
            start: start.to_owned(),
            generation,
            last: std::sync::Mutex::new(None),
            done: false,
        })
    }

    fn current(&self, owners: &std::collections::HashMap<PathBuf, u64>) -> bool {
        owners.get(&self.file) == Some(&self.generation)
    }

    /// One tick: the record's `partialEntry()` (`None`: nothing to keep yet). Nothing is written once
    /// the turn is superseded or retired.
    pub fn save(&self, entry: Option<&JValue>, now_ms: f64) {
        if self.done {
            return;
        }
        let Some(entry) = entry else {
            return;
        };
        let owners = owners();
        if !self.current(&owners) {
            return;
        }
        match write_partial(&self.dir, &self.chat, entry, &self.start, now_ms) {
            Ok(()) => {
                let kept = with(
                    bounded(entry, now_ms),
                    vec![("interrupted", JValue::Bool(true))],
                );
                *self
                    .last
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(kept);
            }
            Err(e) => eprintln!("[chat] could not save the partial reply: {e}"),
        }
    }

    /// The reply has ended. `keep`: its final partial entry when the transcript did not take it;
    /// the file then keeps it without the marker, so the reply is offered as interrupted.
    pub fn finish(mut self, keep: Option<&JValue>, now_ms: f64) {
        self.done = true;
        let mut owners = owners();
        if !self.current(&owners) {
            return;
        }
        owners.remove(&self.file);
        if let Some(keep) = keep {
            let value = with(
                bounded(keep, now_ms),
                vec![("interrupted", JValue::Bool(true))],
            );
            if write_unmarked(
                &self.file,
                &js_json::wtf16::stringify(&value).unwrap_or_default(),
            ) == "unmarked"
            {
                return;
            }
        }
        end_partial(&self.dir, &self.chat);
    }
}

impl Drop for Saves {
    fn drop(&mut self) {
        if self.done {
            return;
        }
        self.done = true;
        let mut owners = owners();
        if !self.current(&owners) {
            return;
        }
        owners.remove(&self.file);
        let last = self
            .last
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        if let Some(kept) = last {
            write_unmarked(
                &self.file,
                &js_json::wtf16::stringify(&kept).unwrap_or_default(),
            );
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    const NOW: &str = "0123456789abcdef0123456789abcdef";
    const BEFORE: &str = "ffffffffffffffffffffffffffffffff";

    fn parse(text: &str) -> JValue {
        js_json::parse(text).unwrap()
    }

    #[test]
    fn this_starts_marker_is_running() {
        let p = parse(&format!(
            r#"{{"role":"assistant","content":"x","interrupted":true,"running":{{"start":"{NOW}"}}}}"#
        ));
        assert_eq!(state(&p, NOW), State::Running);
    }

    #[test]
    fn a_crash_or_restart_leaves_an_interrupted_reply() {
        for text in [
            // The previous start died mid-reply.
            format!(r#"{{"content":"x","running":{{"start":"{BEFORE}"}}}}"#),
            // Written before the marker existed.
            r#"{"content":"x","interrupted":true}"#.to_string(),
            // Malformed markers never read as running.
            r#"{"content":"x","running":true}"#.to_string(),
            r#"{"content":"x","running":null}"#.to_string(),
            format!(r#"{{"content":"x","running":"{NOW}"}}"#),
            format!(r#"{{"content":"x","running":[{{"start":"{NOW}"}}]}}"#),
            format!(
                r#"{{"content":"x","running":{{"start":"{}"}}}}"#,
                NOW.to_uppercase()
            ),
            r#"{"content":"x","running":{"start":5}}"#.to_string(),
        ] {
            assert_eq!(state(&parse(&text), NOW), State::Interrupted, "{text}");
        }
    }

    #[test]
    fn an_invalid_current_start_never_matches() {
        for current in ["", "short", "x"] {
            let p = parse(&format!(r#"{{"running":{{"start":"{current}"}}}}"#));
            assert_eq!(state(&p, current), State::Interrupted, "{current:?}");
        }
    }

    #[test]
    fn start_ids_are_32_lowercase_hex() {
        assert!(valid_start_id(NOW));
        for bad in [
            String::new(),
            NOW[1..].to_string(),
            format!("{NOW}0"),
            NOW.to_uppercase(),
            "g".repeat(32),
        ] {
            assert!(!valid_start_id(&bad), "{bad}");
        }
    }

    #[test]
    fn the_offered_partial_drops_only_the_marker() {
        let p = parse(&format!(
            r#"{{"role":"assistant","content":"x","running":{{"start":"{NOW}"}},"interrupted":true}}"#
        ));
        assert_eq!(
            js_json::wtf16::stringify(&offered(p).unwrap()).unwrap(),
            r#"{"role":"assistant","content":"x","interrupted":true}"#
        );
        assert_eq!(offered(parse("[1]")), None);
        assert_eq!(offered(parse("null")), None);
    }

    #[test]
    fn file_for_is_chat_partials_name() {
        // Node: crypto.createHash('sha256').update(String('qa-chat')).digest('hex').
        assert_eq!(
            file_for(Path::new("/d"), "qa-chat"),
            PathBuf::from(
                "/d/chat-partials/f01c3cc642e11be467b9d1f03d9d0aaa6581bac98c9181c74d076993a9d6f3ef.json"
            )
        );
    }
}
