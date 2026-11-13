//! The auto router's own classifier (slice P5b): noevia-core `server/auto-router.cjs`
//! `heuristicWantsSmart`, `heuristicWantsCode`, `classifierBody`, `classifierVerdict` and the rules
//! of `classify` around its one model call. [`crate::system_one`] falls back to it.
//!
//! The flow, with the caller answering the seams:
//! 1. [`rules`]: no roles → Fast; a Code role and code in the message → Code; the smart rules →
//!    Smart; else [`Rules::Ask`].
//! 2. When the engine's admission refuses the Fast model (`admit(fast)` answers a cause or throws),
//!    the answer is Fast with no call. (The Fast role on an Inference machine is a later slice.)
//! 3. The call: POST [`classifier_body`] with `suppress_thinking` true; a 400 answer
//!    ([`retry_plainly`]) is sent once more with it false. [`role_of`] reads the last answer
//!    (`None` when the fetch threw): anything but a verdict is Fast.
//!
//! The rules' regexes are non-Unicode JS regexes: ASCII-only case folding and word boundaries,
//! ASCII digits, JS `\s`. The message is lower-cased in ASCII only and matched with those classes
//! spelt out (as [`crate::sampling`] does). A lone surrogate (a private-use character in `wtf16`
//! text) is neither a word character nor space, in both.

use std::sync::OnceLock;

use js_json::{wtf16, JValue};
use regex::Regex;

use crate::js;
use crate::roles::Role;

/// JS `\s` (WhiteSpace and LineTerminator) as a regex class.
pub(crate) const JS_SPACE: &str = r"[\t\n\x0b\x0c\r \u{00a0}\u{1680}\u{2000}-\u{200a}\u{2028}\u{2029}\u{202f}\u{205f}\u{3000}\u{feff}]";
/// A non-Unicode JS `\b`.
pub(crate) const WORD_EDGE: &str = r"(?-u:\b)";

/// `CLASSIFIER_MAX_TOKENS`.
pub const MAX_TOKENS: u32 = 512;
/// `CLASSIFIER_SYSTEM_PROMPT`.
pub const SYSTEM_PROMPT: &str = "You classify a user message for a model router. Reply with exactly one word: FAST for short simple questions or small talk, SMART for complex reasoning, multi-step work, code, or analysis. No other text.";
/// `CLASSIFIER_SYSTEM_PROMPT_CODE`.
pub const SYSTEM_PROMPT_CODE: &str = "You classify a user message for a model router. Reply with exactly one word: FAST for short simple questions or small talk, CODE for writing, reading, debugging or explaining source code, SMART for any other complex reasoning, multi-step work or analysis. No other text.";

struct Patterns {
    keywords: Regex,
    numbers: Regex,
    length: Regex,
    effort: Regex,
    verdict: Regex,
    verdict_code: Regex,
}

/// The constant patterns. (Were one ever refused, every rule would answer false and no verdict
/// would be read, and the recorded cases would fail.)
fn patterns() -> Option<&'static Patterns> {
    static P: OnceLock<Option<Patterns>> = OnceLock::new();
    P.get_or_init(|| {
        let (b, s) = (WORD_EDGE, JS_SPACE);
        let re = |p: String| Regex::new(&p).ok();
        Some(Patterns {
            keywords: re(format!(
                "{b}(function|algorithm|debug|refactor|implement|optimize|architecture|regex|sql|migration){b}"
            ))?,
            numbers: re("[0-9]+(?:[.,][0-9]+)?".into())?,
            length: re(format!(
                "{b}[0-9]{{2,}}{s}*(words?|paragraphs?|pages?|sentences?){b}"
            ))?,
            effort: re(format!(
                "{b}(step[- ]by[- ]step|in detail|detailed|thoroughly|comprehensive|deep dive|prove|derive){b}"
            ))?,
            verdict: re(format!("{b}(SMART|FAST){b}"))?,
            verdict_code: re(format!("{b}(SMART|FAST|CODE){b}"))?,
        })
    })
    .as_ref()
}

/// `heuristicWantsSmart(message)`: long (over 600 UTF-16 units), a fence, a code word, three or more
/// numbers, an explicit length (`600 words`) or an effort phrase.
pub fn wants_smart(message: &str) -> bool {
    if js::len(message) > 600 || message.contains("```") {
        return true;
    }
    let Some(p) = patterns() else {
        return false;
    };
    let lower = message.to_ascii_lowercase();
    p.keywords.is_match(&lower)
        || p.numbers.find_iter(message).nth(2).is_some()
        || p.length.is_match(&lower)
        || p.effort.is_match(&lower)
}

/// `heuristicWantsCode(message)`: a fence, or a line that starts a diff (`/^(diff --git|@@ -|\+\+\+ b\/)/m`;
/// a line starts after `\n`, `\r`, U+2028 or U+2029).
pub fn wants_code(message: &str) -> bool {
    if message.contains("```") {
        return true;
    }
    let diff = |at: &str| {
        ["diff --git", "@@ -", "+++ b/"]
            .iter()
            .any(|p| at.starts_with(p))
    };
    diff(message)
        || message
            .char_indices()
            .filter(|(_, c)| matches!(c, '\n' | '\r' | '\u{2028}' | '\u{2029}'))
            .any(|(i, c)| message.get(i + c.len_utf8()..).is_some_and(diff))
}

/// What `classify` decides before its model call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rules {
    Role(Role),
    /// Ask the Fast model (`roles.fast`, which the admission check reads too); `with_code`: a Code
    /// role is saved, so CODE is offered.
    Ask {
        with_code: bool,
    },
}

/// `classify`'s rules over `roles()` (the workspace's roles, `null` when none).
pub fn rules(roles: &JValue, message: &str) -> Rules {
    if !roles.truthy() {
        return Rules::Role(Role::Fast);
    }
    let with_code = roles.get("code").truthy();
    if with_code && wants_code(message) {
        Rules::Role(Role::Code)
    } else if wants_smart(message) {
        Rules::Role(Role::Smart)
    } else {
        Rules::Ask { with_code }
    }
}

/// `classifierBody(model, message, suppressThinking, withCode)`: the request's JSON text. `model` is
/// `roles.fast`; the message is cut to 1,000 UTF-16 units.
pub fn classifier_body(
    model: &JValue,
    message: &str,
    suppress_thinking: bool,
    with_code: bool,
) -> String {
    let prompt = if with_code {
        SYSTEM_PROMPT_CODE
    } else {
        SYSTEM_PROMPT
    };
    let mut body = vec![
        ("model", model.clone()),
        (
            "messages",
            JValue::Arr(vec![
                js::message("system", prompt.to_string()),
                js::message("user", js::slice(message, 1000)),
            ]),
        ),
        ("max_tokens", JValue::Num(f64::from(MAX_TOKENS))),
        ("temperature", JValue::Num(0.0)),
        ("stream", JValue::Bool(false)),
    ];
    if suppress_thinking {
        body.push((
            "chat_template_kwargs",
            JValue::obj([("enable_thinking", JValue::Bool(false))]),
        ));
    }
    wtf16::stringify(&JValue::obj(body)).unwrap_or_default()
}

/// `fetchJson`'s answer to the classifier call. `body` must come from `wtf16::parse`.
#[derive(Debug, Clone, Copy)]
pub struct Answer<'a> {
    pub ok: bool,
    pub status: u16,
    pub body: &'a JValue,
}

/// A 400 is sent again without `chat_template_kwargs` (once).
pub fn retry_plainly(first: &Answer<'_>) -> bool {
    first.status == 400
}

/// `value[0]`.
fn first(value: &JValue) -> &JValue {
    match value {
        JValue::Arr(items) => items.first().unwrap_or(&JValue::Undefined),
        other => other.get("0"),
    }
}

/// `String(value || '').toUpperCase()`.
fn upper(value: &JValue) -> Result<String, js_json::CoerceError> {
    if !value.truthy() {
        return Ok(String::new());
    }
    js_json::to_js_string(value).map(|s| s.to_uppercase())
}

/// `classifierVerdict(msg, withCode)`: the last FAST/SMART(/CODE) word of the content, else of the
/// reasoning channel; `Err` where `String()` throws.
pub fn verdict(msg: &JValue, with_code: bool) -> Result<Option<Role>, js_json::CoerceError> {
    let Some(p) = patterns() else {
        return Ok(None);
    };
    let words = if with_code {
        &p.verdict_code
    } else {
        &p.verdict
    };
    let last = |text: &str| {
        words.find_iter(text).last().and_then(|m| match m.as_str() {
            "SMART" => Some(Role::Smart),
            "FAST" => Some(Role::Fast),
            "CODE" => Some(Role::Code),
            _ => None,
        })
    };
    if let Some(r) = last(&upper(msg.get("content"))?) {
        return Ok(Some(r));
    }
    Ok(last(&upper(msg.get("reasoning_content"))?))
}

/// The role `classify` answers for the last answer (`None`: the fetch threw). Everything but a
/// verdict fails open to Fast, a `String()` that throws included (caught there).
pub fn role_of(answer: Option<&Answer<'_>>, with_code: bool) -> Role {
    let Some(a) = answer.filter(|a| a.ok) else {
        return Role::Fast;
    };
    let choice = match first(a.body.get("choices")) {
        c if c.truthy() => c,
        _ => &JValue::Undefined,
    };
    let msg = match choice.get("message") {
        m if m.truthy() => m,
        _ => &JValue::Undefined,
    };
    verdict(msg, with_code).ok().flatten().unwrap_or(Role::Fast)
}
