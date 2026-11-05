//! A chat's generated title (core chat-title.cjs, sbstndalton/noevia#1785): the title the web client
//! derives from a first message, when a generated one may replace it, the request asked of the
//! local model that just answered, and the cleanup of its answer. Strings are JavaScript's
//! ([`js_json::wtf16`]); lengths and cuts are UTF-16 units.
use crate::text::{as_str, get, trim};
use js_json::{wtf16, JValue};
use regex::Regex;
use std::sync::OnceLock;

/// web src/chat-title.ts `CHAT_TITLE_MAX`.
pub const CHAT_TITLE_MAX: usize = 80;
/// The longest generated title kept.
pub const TITLE_MAX: usize = 60;
const TITLE_MAX_WORDS: usize = 10;
const MESSAGE_CHARS: usize = 600;
const REPLY_CHARS: usize = 1200;
/// The title request's `max_tokens`.
pub const MAX_TOKENS: u32 = 64;
/// The title request's deadline.
pub const TIMEOUT_MS: u64 = 8000;
/// How much of the title reply is read.
pub const REPLY_BYTES_CAP: usize = 64 * 1024;
/// chat-title.cjs `MAX_ATTEMPTS`: title requests per chat (per account).
pub const MAX_ATTEMPTS: u32 = 3;
/// chat-title.cjs `TITLE_SYSTEM`.
pub const TITLE_SYSTEM: &str = "You name chat conversations. Reply with a short title of two to six words for the conversation below, in the language of the user's message. Name the topic, not the request: no quotes, no trailing punctuation, no \"Title:\" prefix, no emoji. Ignore labels, codes or IDs such as ticket numbers or test tags. Output the title only.";

/// ECMAScript `\s`.
fn js_space(c: char) -> bool {
    matches!(c, '\t' | '\n' | '\u{b}' | '\u{c}' | '\r' | ' ' | '\u{a0}' | '\u{1680}' | '\u{2000}'..='\u{200a}' | '\u{2028}' | '\u{2029}' | '\u{202f}' | '\u{205f}' | '\u{3000}' | '\u{feff}')
}
const S: &str = r"\t\n\x0B\x0C\r \u{a0}\u{1680}\u{2000}-\u{200a}\u{2028}\u{2029}\u{202f}\u{205f}\u{3000}\u{feff}";

fn is_high(c: char) -> bool {
    wtf16::lone_unit(c).is_some_and(|u| (0xD800..=0xDBFF).contains(&u))
}

fn trim_end_js(s: &str) -> &str {
    s.trim_end_matches(js_space)
}

/// The UTF-16 index of a char index.
fn unit_index(s: &str, byte: usize) -> usize {
    wtf16::len(s.get(..byte).unwrap_or(""))
}

/// chat-title.cjs `derivedTitle(text)` (web src/chat-title.ts `deriveChatTitle`).
pub fn derived_title(value: &str) -> String {
    if wtf16::len(value) <= CHAT_TITLE_MAX {
        return value.to_owned();
    }
    let mut head = wtf16::slice(value, CHAT_TITLE_MAX - 1);
    if head.chars().last().is_some_and(is_high) {
        head.pop();
    }
    // `head.search(/\s\S*$/)`: the whitespace after which only non-space follows.
    let boundary = head
        .char_indices()
        .filter(|(_, c)| js_space(*c))
        .map(|(i, _)| i)
        .find(|&i| head.get(i..).is_some_and(|rest| rest.chars().skip(1).all(|c| !js_space(c))));
    if let Some(b) = boundary {
        if unit_index(&head, b) >= CHAT_TITLE_MAX / 2 {
            head.truncate(b);
        }
    }
    format!("{}\u{2026}", trim_end_js(&head))
}

/// chat-title.cjs `isDerivedTitle(title, firstMessage)`: no title, or still the one derived from the
/// first message (or the old bare 80-unit cut, or the trimmed message).
pub fn is_derived_title(title: &JValue, first_message: &str) -> bool {
    match title {
        JValue::Undefined | JValue::Null => true,
        JValue::Str(t) if t.is_empty() => true,
        JValue::Str(t) => {
            *t == derived_title(first_message)
                || *t == wtf16::slice(first_message, CHAT_TITLE_MAX)
                || t == trim(first_message)
        }
        _ => false,
    }
}

fn history(v: &JValue) -> &[JValue] {
    match v {
        JValue::Arr(a) => a,
        _ => &[],
    }
}

/// chat-title.cjs `firstReply(history)`: the history holds no assistant message.
pub fn first_reply(h: &JValue) -> bool {
    !history(h).iter().any(|m| m.truthy() && as_str(get(m, "role")) == Some("assistant"))
}

/// chat-title.cjs `firstUserMessage(history, message)`.
pub fn first_user_message(h: &JValue, message: &JValue) -> String {
    history(h)
        .iter()
        .find(|m| m.truthy() && as_str(get(m, "role")) == Some("user") && as_str(get(m, "content")).is_some())
        .and_then(|m| as_str(get(m, "content")).map(str::to_owned))
        .unwrap_or_else(|| {
            if message.is_nullish() {
                String::new()
            } else {
                js_json::to_js_string(message).unwrap_or_default()
            }
        })
}

struct Res {
    think: Regex,
    special: Regex,
    pictographic: Regex,
    hashes: Regex,
    stars: Regex,
    prefix: Regex,
    quotes: Regex,
    tail: Regex,
    spaces: Regex,
    letter: Regex,
    cut_tail: Regex,
}

fn res() -> Option<&'static Res> {
    static R: OnceLock<Option<Res>> = OnceLock::new();
    R.get_or_init(|| {
        Some(Res {
            // `/<think>[\s\S]*?(<\/think>|$)/gi`: ASCII letters fold ASCII only (the Kelvin sign is guarded).
            think: Regex::new(r"(?is)<think>.*?(?:</think>|\z)").ok()?,
            special: Regex::new(r"<\|[^|>]*\|>").ok()?,
            pictographic: Regex::new(r"[\p{Extended_Pictographic}\u{200d}\u{fe0f}]").ok()?,
            hashes: Regex::new(&format!(r"\A#+[{S}]*")).ok()?,
            stars: Regex::new(r"\A\*{1,2}|\*{1,2}\z").ok()?,
            prefix: Regex::new(&format!(r"(?i)\A(?:chat[{S}]+)?title[{S}]*[:\-–][{S}]*")).ok()?,
            quotes: Regex::new(r#"\A["'“”‘’`«»]+|["'“”‘’`«»]+\z"#).ok()?,
            tail: Regex::new(&format!(r"[{S}.,;:!?…]+\z")).ok()?,
            spaces: Regex::new(&format!(r"[{S}]+")).ok()?,
            letter: Regex::new(r"\p{L}").ok()?,
            cut_tail: Regex::new(&format!(r"[{S}.,;:!?]+\z")).ok()?,
        })
    })
    .as_ref()
}

/// chat-title.cjs `cleanTitle(raw)`: a model's answer reduced to a usable title, or `None`.
pub fn clean_title(raw: &JValue) -> Option<String> {
    let raw = as_str(raw)?;
    let r = res()?;
    // Rust folds U+212A (Kelvin) to `k`; JavaScript's non-unicode `i` does not.
    let guarded = raw.replace('\u{212A}', "\u{FFFF}");
    let text = r.think.replace_all(&guarded, "").replace('\u{FFFF}', "\u{212A}");
    let text = r.special.replace_all(&text, " ");
    let line = text
        .split('\n')
        .map(|l| trim(l.strip_suffix('\r').unwrap_or(l)))
        .find(|l| !l.is_empty())?
        .to_owned();
    let t = r.pictographic.replace_all(&line, "");
    let t = trim(&t).to_owned();
    let t = r.hashes.replace(&t, "").into_owned();
    let t = r.stars.replace_all(&t, "").into_owned();
    let t = r.prefix.replace(&t, "").into_owned();
    let t = r.quotes.replace_all(&t, "").into_owned();
    let t = r.pictographic.replace_all(&t, "").into_owned();
    let t = r.tail.replace(&t, "").into_owned();
    let t = r.spaces.replace_all(&t, " ").into_owned();
    let mut text = trim(&t).to_owned();
    if text.is_empty() || !r.letter.is_match(&text) {
        return None;
    }
    if text.split(' ').count() > TITLE_MAX_WORDS {
        return None;
    }
    if wtf16::len(&text) > TITLE_MAX {
        let cut = wtf16::slice(&text, TITLE_MAX - 1);
        let boundary = cut.rfind(' ').map(|b| (b, unit_index(&cut, b)));
        let kept = match boundary {
            Some((b, u)) if u >= TITLE_MAX / 2 => cut.get(..b).unwrap_or("").to_owned(),
            _ => cut,
        };
        text = format!("{}\u{2026}", r.cut_tail.replace(&kept, ""));
    }
    Some(text)
}

/// `clip` of `titleMessages`: trimmed, and past `n` units cut with an ellipsis.
fn clip(s: &str, n: usize) -> String {
    let v = trim(s);
    if wtf16::len(v) > n {
        format!("{}\u{2026}", wtf16::slice(v, n))
    } else {
        v.to_owned()
    }
}

/// chat-title.cjs `titleMessages(message, reply)`.
pub fn title_messages(message: &str, reply: &str) -> JValue {
    let msg = |role: &str, content: String| {
        JValue::Obj(vec![
            ("role".into(), JValue::Str(role.into())),
            ("content".into(), JValue::Str(content)),
        ])
    };
    JValue::Arr(vec![
        msg("system", TITLE_SYSTEM.into()),
        msg(
            "user",
            format!(
                "User message:\n{}\n\nAssistant reply (start):\n{}\n\nTitle:",
                clip(message, MESSAGE_CHARS),
                clip(reply, REPLY_CHARS)
            ),
        ),
    ])
}

/// The title request's JSON body, byte for byte as `generateTitle` sends it; `None` when Node sends
/// nothing (no model, an empty message or reply).
pub fn title_request_body(model: &str, message: &str, reply: &str) -> Option<String> {
    if model.is_empty() || trim(message).is_empty() || trim(reply).is_empty() {
        return None;
    }
    let body = JValue::Obj(vec![
        ("model".into(), JValue::Str(model.into())),
        ("stream".into(), JValue::Bool(false)),
        ("temperature".into(), JValue::Num(0.2)),
        ("max_tokens".into(), JValue::Num(f64::from(MAX_TOKENS))),
        (
            "chat_template_kwargs".into(),
            JValue::Obj(vec![("enable_thinking".into(), JValue::Bool(false))]),
        ),
        ("messages".into(), title_messages(message, reply)),
    ]);
    wtf16::stringify(&body)
}

/// `cleanTitle(body?.choices?.[0]?.message?.content)` of the title reply.
pub fn title_from_reply(body: &JValue) -> Option<String> {
    let first = match get(body, "choices") {
        JValue::Arr(a) => a.first().cloned().unwrap_or(JValue::Undefined),
        _ => JValue::Undefined,
    };
    clean_title(get(get(&first, "message"), "content"))
}

/// chat-title.cjs `createAttemptCounter`: title attempts per chat key, in memory, bounded (the oldest
/// key goes first; recording a key makes it the newest).
pub struct AttemptCounter {
    max: u32,
    max_chats: usize,
    counts: Vec<(String, u32)>,
}

impl AttemptCounter {
    pub fn new(max: u32, max_chats: usize) -> Self {
        Self { max, max_chats, counts: Vec::new() }
    }
    pub fn allowed(&self, key: &str) -> bool {
        self.counts.iter().find(|(k, _)| k == key).map_or(0, |(_, n)| *n) < self.max
    }
    pub fn record(&mut self, key: &str) -> u32 {
        let n = self.counts.iter().position(|(k, _)| k == key).map_or(0, |i| self.counts.remove(i).1) + 1;
        self.counts.push((key.to_owned(), n));
        while self.counts.len() > self.max_chats {
            self.counts.remove(0);
        }
        n
    }
}

impl Default for AttemptCounter {
    fn default() -> Self {
        Self::new(MAX_ATTEMPTS, 2000)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn the_counter_stops_a_chat_after_its_attempts_and_forgets_the_oldest() {
        let mut c = AttemptCounter::new(2, 2);
        assert!(c.allowed("a"));
        assert_eq!(c.record("a"), 1);
        assert_eq!(c.record("a"), 2);
        assert!(!c.allowed("a"));
        c.record("b");
        c.record("c");
        assert!(c.allowed("a"), "a was the oldest and was forgotten");
    }
}
