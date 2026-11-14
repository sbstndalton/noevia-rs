//! A chat's generated title: noevia-core's `server/chat-title.cjs` and the gate around it in `chat.cjs`.
//!
//! After a chat's first completed reply the turn asks the model that just answered for a short title,
//! with thinking off, and sends it as a `title` event. This module holds the pure parts: the title the
//! client derives from a first message, whether a stored title is still that one, the title request's
//! body, how the reply is cleaned into a title (or nothing), the per-chat attempt counter and the
//! conditions under which a title is asked for at all. The request itself, the timeout and the store
//! are the caller's. Nothing calls this yet.
//!
//! # Text and units
//!
//! Node measures strings in UTF-16 code units, and so does this module: every length and cut below
//! (80, 60, 600, 1200, the word-boundary positions) counts code units, and nothing here can panic
//! on any input. Text is `js_json::wtf16` text: a lone surrogate (half of an astral character, which
//! `slice` can leave and `JSON.stringify` writes as `\ud83d`) is one private-use character of
//! U+10F800..=U+10FFFF, counted as one unit. Titles and request bodies that hold one are written
//! with `js_json::wtf16::stringify`, never with a plain UTF-8 encoder; the request body is a
//! `JValue` in Node's key order.
//!
//! # Named differences from Node
//!
//! * **The reply is parsed by `wtf16::parse`** ([`title_from_reply_text`]), which refuses text that
//!   already holds a character of U+10F800..=U+10FFFF, and nesting deeper than serde_json follows
//!   (128). Node reads both; here they give no title. The caller turns the reply's bytes into text
//!   with `String::from_utf8_lossy`, as `Buffer.toString('utf8')` does (invalid bytes become U+FFFD).
//! * **Whitespace and letters.** `\s`, `\p{L}` and `\p{Extended_Pictographic}` are JavaScript's, not
//!   Rust's: `is_js_space` (no NEL, with the BOM), and tables generated from Node's own ICU
//!   (`title/tables.rs`) that the tests check code point by code point against the recording.
//!
//! The Node behaviour kept as is, quirks included: a `Title:` prefix is stripped before quotes, so
//! `"Title: X"` keeps its prefix; a reply cut by `finish_reason: length` is used as it is; only the
//! first line of the reply is read; the word limit is checked before the length cut; a title cut
//! through an astral letter ends in half of it.

mod tables;

use std::collections::{BTreeMap, HashMap};

use js_json::{wtf16, JValue};

/// The longest title the client derives from a first message.
pub const CHAT_TITLE_MAX: usize = 80;
/// The longest generated title kept.
pub const TITLE_MAX: usize = 60;
/// More words than this and the reply is no title.
pub const TITLE_MAX_WORDS: usize = 10;
/// How much of the message and of the reply the request carries.
pub const MESSAGE_CHARS: usize = 600;
pub const REPLY_CHARS: usize = 1200;
pub const MAX_TOKENS: u32 = 64;
/// The title request's own deadline, in milliseconds.
pub const TIMEOUT_MS: u64 = 8000;
/// The most of the engine's reply that is read.
pub const REPLY_BYTES_CAP: usize = 64 * 1024;
/// The most title requests one chat gets.
pub const MAX_ATTEMPTS: u32 = 3;
/// The attempt table's size.
pub const MAX_CHATS: usize = 2000;

pub const TITLE_SYSTEM: &str = "You name chat conversations. Reply with a short title of two to six words for the conversation below, in the language of the user's message. Name the topic, not the request: no quotes, no trailing punctuation, no \"Title:\" prefix, no emoji. Ignore labels, codes or IDs such as ticket numbers or test tags. Output the title only.";

// ---- JavaScript string helpers --------------------------------------------------------------------

/// JavaScript's `\s` (and `trim`): not Rust's `char::is_whitespace` (that has U+0085, lacks U+FEFF).
pub fn is_js_space(c: char) -> bool {
    matches!(
        c,
        '\t' | '\n' | '\u{b}' | '\u{c}' | '\r' | ' ' | '\u{a0}' | '\u{1680}' | '\u{2000}'
            ..='\u{200a}'
                | '\u{2028}'
                | '\u{2029}'
                | '\u{202f}'
                | '\u{205f}'
                | '\u{3000}'
                | '\u{feff}'
    )
}

fn in_table(starts: &[u32], ends: &[u32], c: char) -> bool {
    let c = u32::from(c);
    let after = starts.partition_point(|&s| s <= c);
    after
        .checked_sub(1)
        .and_then(|i| ends.get(i))
        .is_some_and(|&end| c <= end)
}

/// `\p{L}`.
pub fn is_letter(c: char) -> bool {
    in_table(tables::LETTER_START, tables::LETTER_END, c)
}

/// `\p{Extended_Pictographic}`.
pub fn is_pictographic(c: char) -> bool {
    in_table(tables::PICTOGRAPHIC_START, tables::PICTOGRAPHIC_END, c)
}

fn trim_js(s: &str) -> &str {
    s.trim_matches(is_js_space)
}

fn is_space_unit(unit: u16) -> bool {
    char::from_u32(u32::from(unit)).is_some_and(is_js_space)
}

fn is_high(unit: u16) -> bool {
    (0xd800..=0xdbff).contains(&unit)
}

/// The UTF-16 code units of wtf16 text: a lone surrogate character is its unit.
fn units(s: &str) -> Vec<u16> {
    let mut out = Vec::new();
    let mut buf = [0u16; 2];
    for c in s.chars() {
        match wtf16::lone_unit(c) {
            Some(unit) => out.push(unit),
            None => out.extend_from_slice(c.encode_utf16(&mut buf)),
        }
    }
    out
}

/// The first `n` code units (`slice(0, n)`).
fn head(units: &[u16], n: usize) -> &[u16] {
    units.get(..n).unwrap_or(units)
}

/// Code units back to wtf16 text; a lone surrogate becomes its character.
fn text(units: &[u16]) -> String {
    char::decode_utf16(units.iter().copied())
        .map(|c| {
            c.unwrap_or_else(|e| wtf16::lone_char(e.unpaired_surrogate()).unwrap_or('\u{fffd}'))
        })
        .collect()
}

fn trim_end_units(units: &[u16], strip: impl Fn(u16) -> bool) -> &[u16] {
    let keep = units.iter().rposition(|&u| !strip(u)).map_or(0, |i| i + 1);
    head(units, keep)
}

// ---- the derived title and the history ------------------------------------------------------------

/// The title the web client derives from a first message: the message, cut at 80 code units.
pub fn derived_title(message: &str) -> String {
    let all = units(message);
    if all.len() <= CHAT_TITLE_MAX {
        return message.to_owned();
    }
    let mut cut = head(&all, CHAT_TITLE_MAX - 1);
    if let Some((&last, rest)) = cut.split_last() {
        if is_high(last) {
            cut = rest; // a pair split by the cut
        }
    }
    // `search(/\s\S*$/)`: the last whitespace, kept only from the middle on.
    if let Some(boundary) = cut.iter().rposition(|&u| is_space_unit(u)) {
        if boundary >= CHAT_TITLE_MAX / 2 {
            cut = head(cut, boundary);
        }
    }
    let mut title = text(trim_end_units(cut, is_space_unit));
    title.push('…');
    title
}

/// Whether `title` is still the one derived from the first message (or the old bare 80-unit cut, which
/// may end in half an astral character, or the trimmed message), or no title at all: only then may a
/// generated title replace it. `undefined` and `null` are no title; any value but a string is not derived.
pub fn is_derived_title(title: &JValue, first_message: &str) -> bool {
    let title = match title {
        JValue::Undefined | JValue::Null => return true,
        JValue::Str(title) if title.is_empty() => return true,
        JValue::Str(title) => title,
        _ => return false,
    };
    *title == derived_title(first_message)
        || *title == wtf16::slice(first_message, CHAT_TITLE_MAX)
        || title == trim_js(first_message)
}

/// The history this turn was sent with holds no assistant message.
pub fn first_reply(history: &JValue) -> bool {
    !matches!(history, JValue::Arr(turns)
        if turns.iter().any(|turn| turn.get("role").as_str() == Some("assistant")))
}

/// The chat's first user message: the first user turn of the history whose content is a string, or this
/// turn's own message when there is none.
pub fn first_user_message(history: &JValue, message: &str) -> String {
    let JValue::Arr(turns) = history else {
        return message.to_owned();
    };
    turns
        .iter()
        .find(|turn| {
            turn.get("role").as_str() == Some("user") && turn.get("content").as_str().is_some()
        })
        .and_then(|turn| turn.get("content").as_str())
        .unwrap_or(message)
        .to_owned()
}

// ---- the attempt counter --------------------------------------------------------------------------

/// The attempt key: the account, the list (empty for none) and the chat, so one account's chats never
/// count against another's.
pub fn attempt_key(user_id: Option<&str>, list_id: Option<&str>, chat_id: &str) -> String {
    format!(
        "{}\0{}\0{chat_id}",
        user_id.unwrap_or(""),
        list_id.unwrap_or("")
    )
}

/// Title attempts per chat, in memory and bounded: a key is recorded as the newest, and past `max_chats`
/// keys the oldest is forgotten (which only allows it a few more tries).
#[derive(Debug, Clone)]
pub struct AttemptCounter {
    max: u32,
    max_chats: usize,
    clock: u64,
    counts: HashMap<String, (u64, u32)>,
    order: BTreeMap<u64, String>,
}

impl Default for AttemptCounter {
    fn default() -> Self {
        Self::new(MAX_ATTEMPTS, MAX_CHATS)
    }
}

impl AttemptCounter {
    pub fn new(max: u32, max_chats: usize) -> Self {
        Self {
            max,
            max_chats,
            clock: 0,
            counts: HashMap::new(),
            order: BTreeMap::new(),
        }
    }

    pub fn allowed(&self, key: &str) -> bool {
        self.counts.get(key).map_or(0, |&(_, n)| n) < self.max
    }

    /// `allowed` and `record` as one step: Node checks and records with no await between them, so a
    /// caller that awaits between the two (the engine-busy bridge) must re-check under its lock with
    /// this, or two turns of one chat could both pass the last allowed attempt.
    pub fn try_record(&mut self, key: &str) -> bool {
        if !self.allowed(key) {
            return false;
        }
        self.record(key);
        true
    }

    /// One more request for `key`; returns how many it now has.
    pub fn record(&mut self, key: &str) -> u32 {
        let n = self.counts.remove(key).map_or(0, |(stamp, n)| {
            self.order.remove(&stamp);
            n
        });
        let n = n.saturating_add(1);
        self.clock = self.clock.wrapping_add(1);
        self.counts.insert(key.to_owned(), (self.clock, n));
        self.order.insert(self.clock, key.to_owned());
        while self.counts.len() > self.max_chats {
            let Some((_, oldest)) = self.order.pop_first() else {
                break;
            };
            self.counts.remove(&oldest);
        }
        n
    }
}

// ---- when a title is asked for ----------------------------------------------------------------------

/// The facts `chat.cjs` checks, in its order, before it asks for a title.
#[derive(Debug, Clone, Copy)]
pub struct AskFacts<'a> {
    /// The chat title store is wired.
    pub store_wired: bool,
    /// The reply ended in a pause (a declined write, a step check).
    pub paused: bool,
    pub round_has_content: bool,
    pub reply: &'a str,
    pub chat_id: &'a str,
    pub compact_only: bool,
    /// A Diary space or the Diary project.
    pub diary: bool,
    /// The turn runs on the local engine (the default provider or one of the owner's machines) and is not off-box.
    pub local: bool,
    /// The provider is one of the owner's inference machines (it skips the busy check).
    pub machine: bool,
}

/// Whether a title is asked for after this reply. The three closures are the caller's reads, called only
/// when every fact before them held, in Node's order: `due` is `titleDue` (the store's `autoNamed`),
/// `allowed` the attempt counter, `engine_busy` the inference gate.
pub fn ask_for_title(
    facts: &AskFacts<'_>,
    due: impl FnOnce() -> bool,
    allowed: impl FnOnce() -> bool,
    engine_busy: impl FnOnce() -> bool,
) -> bool {
    facts.store_wired
        && !facts.paused
        && facts.round_has_content
        && !trim_js(facts.reply).is_empty()
        && !facts.chat_id.is_empty()
        && !facts.compact_only
        && !facts.diary
        && facts.local
        && due()
        && allowed()
        && (facts.machine || !engine_busy())
}

/// `titleDue`: the chat's first reply always asks; a later one only while the store says the chat still
/// carries its derived title.
pub fn title_due(first_reply: bool, auto_named: impl FnOnce() -> bool) -> bool {
    first_reply || auto_named()
}

// ---- the request ------------------------------------------------------------------------------------

/// `generateTitle` makes no request without a model and a message and a reply that are not blank.
pub fn can_request(model: &str, message: &str, reply: &str) -> bool {
    !model.is_empty() && !trim_js(message).is_empty() && !trim_js(reply).is_empty()
}

/// `trim`, then the first `n` code units and an ellipsis when it is longer.
fn clip(s: &str, n: usize) -> String {
    let trimmed = trim_js(s);
    let all = units(trimmed);
    if all.len() > n {
        let mut out = text(head(&all, n));
        out.push('…');
        out
    } else {
        trimmed.to_owned()
    }
}

fn user_prompt(message: &str, reply: &str) -> String {
    format!(
        "User message:\n{}\n\nAssistant reply (start):\n{}\n\nTitle:",
        clip(message, MESSAGE_CHARS),
        clip(reply, REPLY_CHARS)
    )
}

/// The request body `generateTitle` sends, with Node's key order.
pub fn title_request_body(model: &str, message: &str, reply: &str) -> JValue {
    let say = |role: &str, content: &str| {
        JValue::obj([
            ("role", JValue::from(role)),
            ("content", JValue::from(content)),
        ])
    };
    JValue::obj([
        ("model", JValue::from(model)),
        ("stream", JValue::Bool(false)),
        ("temperature", JValue::Num(0.2)),
        ("max_tokens", JValue::Num(f64::from(MAX_TOKENS))),
        (
            "chat_template_kwargs",
            JValue::obj([("enable_thinking", JValue::Bool(false))]),
        ),
        (
            "messages",
            JValue::Arr(vec![
                say("system", TITLE_SYSTEM),
                say("user", &user_prompt(message, reply)),
            ]),
        ),
    ])
}

/// The bytes `JSON.stringify` makes of [`title_request_body`], lone surrogates written as `\udxxx`.
pub fn title_request_json(model: &str, message: &str, reply: &str) -> String {
    wtf16::stringify(&title_request_body(model, message, reply)).unwrap_or_default()
}

// ---- the reply --------------------------------------------------------------------------------------

/// The engine's reply text read into a title: `JSON.parse`, then `choices[0].message.content` cleaned.
/// Text `JSON.parse` refuses, or `wtf16::parse` cannot take (see the module's named differences), is no
/// title. The caller decodes the reply's bytes lossily (`String::from_utf8_lossy`) as Node's
/// `Buffer.toString('utf8')` does.
pub fn title_from_reply_text(reply: &str) -> Option<String> {
    title_from_reply(&wtf16::parse(reply).ok()?)
}

/// `choices?.[0]?.message?.content` of a parsed reply, cleaned. `choices[0]` is read as JavaScript does,
/// so an object with a `"0"` key counts as well as an array.
pub fn title_from_reply(reply: &JValue) -> Option<String> {
    let first = match reply.get("choices") {
        JValue::Arr(list) => list.first()?,
        choices @ JValue::Obj(_) => choices.get("0"),
        _ => return None,
    };
    clean_title(first.get("message").get("content").as_str()?)
}

/// `/<think>[\s\S]*?(<\/think>|$)/gi`: a reasoning block, to its close or to the end.
fn strip_think(s: &str) -> String {
    fn find(hay: &[u8], needle: &[u8], from: usize) -> Option<usize> {
        let rest = hay.get(from..)?;
        rest.windows(needle.len())
            .position(|w| w.eq_ignore_ascii_case(needle))
            .map(|i| i + from)
    }
    let bytes = s.as_bytes();
    let (mut out, mut copied) = (String::new(), 0);
    while let Some(open) = find(bytes, b"<think>", copied) {
        out.push_str(s.get(copied..open).unwrap_or_default());
        copied = find(bytes, b"</think>", open + 7).map_or(bytes.len(), |close| close + 8);
    }
    out.push_str(s.get(copied..).unwrap_or_default());
    out
}

/// `/<\|[^|>]*\|>/g` replaced by one space: a template token.
fn strip_tokens(s: &str) -> String {
    let chars: Vec<char> = s.chars().collect();
    let (mut out, mut i) = (String::new(), 0);
    while let Some(&c) = chars.get(i) {
        let token_end = (c == '<' && chars.get(i + 1) == Some(&'|')).then(|| {
            let mut j = i + 2;
            while chars.get(j).is_some_and(|&x| x != '|' && x != '>') {
                j += 1;
            }
            (chars.get(j) == Some(&'|') && chars.get(j + 1) == Some(&'>')).then_some(j + 2)
        });
        if let Some(Some(end)) = token_end {
            out.push(' ');
            i = end;
        } else {
            out.push(c);
            i += 1;
        }
    }
    out
}

/// `/^\*{1,2}|\*{1,2}$/g`: up to two stars off the start, then up to two off the end.
fn strip_stars(s: &str) -> &str {
    let stars = |t: &str| t.chars().take_while(|&c| c == '*').take(2).count();
    let s = s.get(stars(s)..).unwrap_or_default();
    let trailing = s.chars().rev().take_while(|&c| c == '*').take(2).count();
    s.get(..s.len() - trailing).unwrap_or_default()
}

fn strip_prefix_ci<'a>(s: &'a str, word: &str) -> Option<&'a str> {
    let front = s.get(..word.len())?;
    front
        .eq_ignore_ascii_case(word)
        .then(|| s.get(word.len()..))
        .flatten()
}

/// `/^(?:chat\s+)?title\s*[:\-–]\s*/i`.
fn strip_title_prefix(s: &str) -> &str {
    let after_chat = strip_prefix_ci(s, "chat")
        .map(|r| r.trim_start_matches(is_js_space))
        .filter(|r| r.len() < s.len() - 4);
    for start in after_chat.into_iter().chain(Some(s)) {
        let Some(rest) = strip_prefix_ci(start, "title") else {
            continue;
        };
        if let Some(rest) = rest
            .trim_start_matches(is_js_space)
            .strip_prefix([':', '-', '–'])
        {
            return rest.trim_start_matches(is_js_space);
        }
    }
    s
}

fn is_quote(c: char) -> bool {
    matches!(c, '"' | '\'' | '“' | '”' | '‘' | '’' | '`' | '«' | '»')
}

/// A model's answer reduced to a usable title, or none.
pub fn clean_title(raw: &str) -> Option<String> {
    let cleaned = strip_tokens(&strip_think(raw));
    let line = cleaned.split('\n').map(trim_js).find(|l| !l.is_empty())?;
    let plain: String = line
        .chars()
        .filter(|&c| !is_pictographic(c) && c != '\u{200d}' && c != '\u{fe0f}')
        .collect();
    // Node removes the pictographs a second time after the quotes; nothing before it can add one.
    let s = trim_js(&plain)
        .trim_start_matches('#')
        .trim_start_matches(is_js_space);
    let s = strip_title_prefix(strip_stars(s));
    let s = s.trim_start_matches(is_quote).trim_end_matches(is_quote);
    let s = s.trim_end_matches(|c: char| is_js_space(c) || ".,;:!?…".contains(c));
    let mut collapsed = String::new();
    let mut in_space = false;
    for c in s.chars() {
        if is_js_space(c) {
            in_space = true;
        } else {
            if in_space {
                collapsed.push(' ');
            }
            in_space = false;
            collapsed.push(c);
        }
    }
    let collapsed = trim_js(&collapsed);
    if collapsed.is_empty()
        || !collapsed.chars().any(is_letter)
        || collapsed.split(' ').count() > TITLE_MAX_WORDS
    {
        return None;
    }
    let all = units(collapsed);
    if all.len() <= TITLE_MAX {
        return Some(collapsed.to_owned());
    }
    let cut = head(&all, TITLE_MAX - 1);
    let kept = match cut.iter().rposition(|&u| u == 0x20) {
        Some(boundary) if boundary >= TITLE_MAX / 2 => head(cut, boundary),
        _ => cut,
    };
    let strip = |u: u16| is_space_unit(u) || b".,;:!?".iter().any(|&b| u16::from(b) == u);
    let mut title = text(trim_end_units(kept, strip));
    title.push('…');
    Some(title)
}
