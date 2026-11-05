//! The writes that already succeeded in one chat (core recent-writes.cjs, #658): the fingerprint an
//! approval card's repeat flag compares, and the bounded, short-lived record per account and chat.
//! Advisory only: nothing here approves, refuses or skips a write.
use crate::text::units;
use js_json::{wtf16, JValue};
use sha2::{Digest, Sha256};
use std::collections::VecDeque;

/// recent-writes.cjs `TTL_MS`.
pub const TTL_MS: i64 = 6 * 60 * 60 * 1000;
const PER_CHAT: usize = 20;
const MAX_CHATS: usize = 500;

fn quoted(s: &str) -> String {
    wtf16::stringify(&JValue::Str(s.to_owned())).unwrap_or_default()
}

/// recent-writes.cjs `canonical(value)`: JSON with object keys sorted by UTF-16 units.
pub fn canonical(v: &JValue) -> String {
    match v {
        JValue::Arr(items) => format!(
            "[{}]",
            items.iter().map(canonical).collect::<Vec<_>>().join(",")
        ),
        JValue::Obj(items) => {
            let mut keys: Vec<&(String, JValue)> = items.iter().collect();
            keys.sort_by_key(|a| units(&a.0));
            format!(
                "{{{}}}",
                keys.iter()
                    .map(|(k, x)| format!("{}:{}", quoted(k), canonical(x)))
                    .collect::<Vec<_>>()
                    .join(",")
            )
        }
        JValue::Undefined => "null".into(),
        other => wtf16::stringify(other).unwrap_or_else(|| "null".into()),
    }
}

/// recent-writes.cjs `fingerprint(name, target, rawArgs)`: one write as its approval card shows it.
pub fn fingerprint(name: &JValue, target: &JValue, raw_args: &JValue) -> String {
    let text = match raw_args {
        JValue::Str(s) if !s.is_empty() => s.as_str(),
        _ => "{}",
    };
    let args = match wtf16::parse(text) {
        Ok(v) => canonical(&v),
        // `String(rawArgs || '')`: only a non-empty string fails to parse.
        Err(_) => text.to_owned(),
    };
    let name = if name.truthy() {
        js_json::to_js_string(name).unwrap_or_default()
    } else {
        String::new()
    };
    let where_ = match target {
        JValue::Str(t) if !t.is_empty() => JValue::Str(t.clone()),
        _ => JValue::Null,
    };
    let line = wtf16::stringify(&JValue::Arr(vec![
        JValue::Str(name),
        where_,
        JValue::Str(args),
    ]))
    .unwrap_or_default();
    // A lone surrogate never reaches the hash: JSON.stringify wrote it as an escape.
    Sha256::digest(line.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// recent-writes.cjs `createRecentWrites`: per account and chat, the last writes within the TTL.
pub struct RecentWrites {
    ttl_ms: i64,
    per_chat: usize,
    max_chats: usize,
    /// Least recently written first.
    chats: VecDeque<(String, Vec<(String, i64)>)>,
}

impl Default for RecentWrites {
    fn default() -> Self {
        Self::new(TTL_MS, PER_CHAT, MAX_CHATS)
    }
}

impl RecentWrites {
    pub fn new(ttl_ms: i64, per_chat: usize, max_chats: usize) -> Self {
        Self {
            ttl_ms,
            per_chat,
            max_chats,
            chats: VecDeque::new(),
        }
    }

    fn key(user: &str, chat: &str) -> Option<String> {
        (!user.is_empty() && !chat.is_empty()).then(|| format!("{user}\u{0}{chat}"))
    }

    fn live(&mut self, key: &str, now: i64) -> Vec<(String, i64)> {
        let Some(at) = self.chats.iter().position(|(k, _)| k == key) else {
            return Vec::new();
        };
        let ttl = self.ttl_ms;
        let list: Vec<(String, i64)> = self
            .chats
            .get(at)
            .map(|(_, l)| l.iter().filter(|(_, t)| now - t <= ttl).cloned().collect())
            .unwrap_or_default();
        if list.is_empty() {
            self.chats.remove(at);
        } else if let Some(slot) = self.chats.get_mut(at) {
            slot.1 = list.clone();
        }
        list
    }

    pub fn record(&mut self, user: &str, chat: &str, fp: &str, now: i64) {
        let Some(key) = Self::key(user, chat) else {
            return;
        };
        let mut list = self.live(&key, now);
        list.push((fp.to_owned(), now));
        let drop = list.len().saturating_sub(self.per_chat);
        list.drain(..drop);
        if let Some(at) = self.chats.iter().position(|(k, _)| *k == key) {
            self.chats.remove(at);
        }
        self.chats.push_back((key, list));
        while self.chats.len() > self.max_chats {
            self.chats.pop_front();
        }
    }

    pub fn has(&mut self, user: &str, chat: &str, fp: &str, now: i64) -> bool {
        Self::key(user, chat).is_some_and(|key| self.live(&key, now).iter().any(|(f, _)| f == fp))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_write_is_remembered_per_chat_within_its_ttl() {
        let mut w = RecentWrites::new(100, 2, 2);
        w.record("u", "c", "a", 0);
        assert!(w.has("u", "c", "a", 100));
        assert!(!w.has("u", "c", "a", 101), "past the TTL");
        assert!(!w.has("v", "c", "a", 0), "another account");
        for (i, fp) in ["a", "b", "c"].iter().enumerate() {
            w.record("u", "c", fp, i as i64);
        }
        assert!(
            !w.has("u", "c", "a", 3) && w.has("u", "c", "b", 3),
            "the last two"
        );
        w.record("u", "d", "x", 3);
        w.record("u", "e", "y", 3);
        assert!(
            !w.has("u", "c", "b", 3),
            "the least recently written chat went first"
        );
        w.record("", "c", "z", 3);
        assert!(!w.has("", "c", "z", 3));
    }
}
