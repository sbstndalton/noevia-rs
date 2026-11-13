//! What a native turn keeps when it ends: chat.cjs `handleChatTurn`'s `finally` (lines 345-371).
//!
//! The turn record follows every event the client was sent, the relayed ones before the hand-off
//! included (an `error` after the client left is not a failed reply, #679 review). When the client
//! went away, the reply as it stood is appended to the stored transcript after the user message
//! this turn carried (`transcripts.appendIfLast`, the Rust store); when the store did not take it
//! (no user message to follow, or a failure), the partial file keeps it, unmarked, as the only
//! copy. Otherwise the partial is cleared. A chat deleted while the turn ran has its state removed.

use js_json::JValue;

use crate::record::TurnRecord;

/// The account store calls the end of a turn makes.
pub trait Keep {
    /// `transcripts.appendIfLast(chatId, message, entry, { origin: 'turn' })`: "saved", or why not.
    fn append_if_last(&self, chat: &str, message: &str, entry: &JValue) -> Result<String, String>;
    /// Writes the chat's partial file with this content (no running marker).
    fn keep_partial(&self, chat: &str, file: &JValue);
    /// Removes the chat's partial file (or its running marker when it cannot be removed).
    fn end_partial(&self, chat: &str);
    /// The chat is in the account's tombstones.
    fn tombstoned(&self, chat: &str) -> bool;
    /// The store's delete of a chat no list holds (`deleteChat(id, { kind: 'unlisted', origin: 'turn' })`).
    fn delete_unlisted(&self, chat: &str);
}

/// chat-lists.cjs `safeChatId`: the characters a chat id may keep.
pub fn safe_chat_id(raw: &str) -> String {
    raw.chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '-')
        .collect()
}

/// The record and what it saw.
pub struct Saving {
    pub record: TurnRecord,
}

impl Saving {
    pub fn new(record: TurnRecord) -> Self {
        Saving { record }
    }

    /// chat.cjs `send`: every event, except an error once the client has left.
    pub fn observe(&mut self, event: &JValue, left: bool) {
        if !(left && event.get("type").as_str() == Some("error")) {
            self.record.observe(event);
        }
    }

    /// The end of the turn. `chat`: the request's own chat id (ordinary chats only; `None` when the
    /// turn keeps no record). `left`: the client went away. `now_ms`: the partial's `savedAt`.
    pub fn finish<K: Keep>(
        &self,
        keep: &K,
        chat: Option<&str>,
        message: &str,
        left: bool,
        now_ms: i64,
    ) {
        let Some(raw) = chat else { return };
        let id = safe_chat_id(raw);
        let mut kept = None;
        if left {
            let entry = self
                .record
                .partial_entry()
                .or_else(|| self.record.assistant_entry());
            // chat-interrupted-turn.cjs saveInterruptedTurn.
            let outcome = match &entry {
                _ if id.is_empty() => Ok("no-chat".to_string()),
                None => Ok("nothing-to-keep".to_string()),
                Some(_) if keep.tombstoned(&id) => Ok("deleted".to_string()),
                Some(e) => keep.append_if_last(&id, message, e),
            };
            match outcome {
                Ok(o) if o == "no-user-message" => kept = entry,
                Ok(_) => {}
                Err(_) => kept = entry,
            }
        }
        if !id.is_empty() {
            match kept {
                Some(entry) => keep.keep_partial(&id, &crate::record::partial_file(&entry, now_ms)),
                None => keep.end_partial(&id),
            }
            if keep.tombstoned(&id) {
                keep.delete_unlisted(&id);
            }
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
mod tests {
    use super::*;

    #[test]
    fn safe_ids_keep_only_the_allowed_characters() {
        assert_eq!(safe_chat_id("qa-chat_1/../x y"), "qa-chat_1xy");
    }
}
