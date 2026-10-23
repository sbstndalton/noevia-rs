//! A chat's saved partial reply (core chat-partial.cjs, #1507) as another process reads it: is the
//! reply that wrote it still running, or was it cut off? sbstndalton/noevia#1071 slice 1: no route
//! uses this yet; `GET /api/chats/{chatId}/history` will (slice 2), in place of the in-memory set
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
