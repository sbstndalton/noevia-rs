//! #1793: a knowledge file injected whole into the prompt counts as a source of the reply once the
//! answer quotes something distinctive from it that the question did not (chat-sources.cjs
//! `usedInjected`, used by chat.cjs's close). Core does the part that does not depend on the answer
//! before the hand-off (rust-chat-native.cjs `injectedSources`: the candidates, the slice of each
//! text the attribution reads, the source item each would add); this is the rest.
//!
//! "Distinctive" is a token with a digit ("orchid-42", "inv-2042", "34.95") of at least four UTF-16
//! units (five when it has no letter), or a run of four words. Words are the lowercased text's runs
//! of letters and digits (`\p{L}`, `\p{N}`), joined by single `-` or `.`; each text is read up to
//! [`ATTRIBUTE_MAX`] UTF-16 units. Proven by `tests/node_sources.rs` (recorded by
//! `tests/record_sources.cjs`) and the `injected_*` recorded turns.
//!
//! Named difference: lowercasing and the letter/digit classes use this build's Unicode tables, Node
//! its ICU's; they differ only for characters added in a Unicode version one of them lacks.

use std::collections::HashSet;
use std::sync::LazyLock;

use js_json::JValue;
use regex::Regex;

use crate::js;

/// chat-sources.cjs `ATTRIBUTE_MAX`: the UTF-16 units of each text the attribution reads.
pub const ATTRIBUTE_MAX: usize = 24000;
/// chat-sources.cjs `MAX_SOURCES`: the most sources a reply lists.
pub const MAX_SOURCES: usize = 12;

static WORD: LazyLock<Option<Regex>> =
    LazyLock::new(|| Regex::new(r"[\p{L}\p{N}]+(?:[-.][\p{L}\p{N}]+)*").ok());
static DIGIT: LazyLock<Option<Regex>> = LazyLock::new(|| Regex::new(r"\p{N}").ok());
static LETTER: LazyLock<Option<Regex>> = LazyLock::new(|| Regex::new(r"\p{L}").ok());

fn found(re: &LazyLock<Option<Regex>>, text: &str) -> bool {
    re.as_ref().is_some_and(|r| r.is_match(text))
}

/// `words(text)`: the lowercased words of the text's first [`ATTRIBUTE_MAX`] units.
fn words(text: &str) -> Vec<String> {
    let lower = js::slice(text, ATTRIBUTE_MAX).to_lowercase();
    WORD.as_ref().map_or_else(Vec::new, |r| {
        r.find_iter(&lower)
            .map(|m| m.as_str().to_string())
            .collect()
    })
}

/// What in a text is distinctive enough to show it was quoted (`marks`).
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Marks {
    pub tokens: HashSet<String>,
    pub runs: HashSet<String>,
}

/// chat-sources.cjs `marks`.
pub fn marks(text: &str) -> Marks {
    let list = words(text);
    let tokens = list
        .iter()
        .filter(|w| {
            let n = js::len(w);
            n >= 4 && found(&DIGIT, w) && (found(&LETTER, w) || n >= 5)
        })
        .cloned()
        .collect();
    let runs = list.windows(4).map(|run| run.join(" ")).collect();
    Marks { tokens, runs }
}

/// One injected whole file, as core prepared it.
#[derive(Debug, Clone, PartialEq)]
pub struct Injected {
    /// Its name: what the attribution keys on (a file used once is not counted again).
    pub file: String,
    /// The part of its text the attribution reads.
    pub text: String,
    /// The source item it adds to the reply's list, or `Null` when its file is not readable.
    pub item: JValue,
}

/// The sources part of the prepared turn.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Sources {
    /// The sources the reply showed before the cut (its first `sources` event).
    pub reply: Vec<JValue>,
    /// The injected whole files not among the placed ones, in prompt order.
    pub injected: Vec<Injected>,
}

impl Sources {
    /// `turn.sources` of the prepared frame (rust-chat-native.cjs `injectedSources`):
    /// `{reply: [item], injected: [{file, text, item | null}]}`, or `None` when it is not that shape.
    pub fn read(v: &JValue) -> Option<Sources> {
        let reply = match v.get("reply") {
            JValue::Arr(items) if items.iter().all(|i| matches!(i, JValue::Obj(_))) => {
                items.clone()
            }
            _ => return None,
        };
        let JValue::Arr(list) = v.get("injected") else {
            return None;
        };
        let injected = list
            .iter()
            .map(|e| match (e.get("file"), e.get("text"), e.get("item")) {
                (JValue::Str(file), JValue::Str(text), item @ (JValue::Obj(_) | JValue::Null)) => {
                    Some(Injected {
                        file: file.clone(),
                        text: text.clone(),
                        item: item.clone(),
                    })
                }
                _ => None,
            })
            .collect::<Option<Vec<_>>>()?;
        Some(Sources { reply, injected })
    }
}

/// chat-sources.cjs `usedInjected` over core's candidates: the ones the answer drew on, in order.
pub fn used_injected<'a>(
    injected: &'a [Injected],
    answer: &str,
    question: &str,
) -> Vec<&'a Injected> {
    if !js::has_text(answer) {
        return Vec::new();
    }
    let said = marks(answer);
    let asked = marks(question);
    let mut already: HashSet<&str> = HashSet::new();
    let mut out = Vec::new();
    for e in injected {
        if already.contains(e.file.as_str()) {
            continue;
        }
        let from = marks(&e.text);
        let hit = from
            .tokens
            .iter()
            .any(|t| said.tokens.contains(t) && !asked.tokens.contains(t))
            || from
                .runs
                .iter()
                .any(|r| said.runs.contains(r) && !asked.runs.contains(r));
        if hit {
            out.push(e);
            already.insert(e.file.as_str());
        }
    }
    out
}

/// chat.cjs's close: the reply's sources again with the injected files the answer drew on, when that
/// adds one (the client keeps the last `sources` event). `None` when nothing changes.
pub fn reattributed(sources: &Sources, answer: &str, question: &str) -> Option<Vec<JValue>> {
    if sources.injected.is_empty() {
        return None;
    }
    let used = used_injected(&sources.injected, answer, question);
    if used.is_empty() {
        return None;
    }
    // buildSources([...placed, ...used], readable): the placed ones are the reply's own list.
    let list: Vec<JValue> = sources
        .reply
        .iter()
        .cloned()
        .chain(
            used.iter()
                .filter(|e| !matches!(e.item, JValue::Null))
                .map(|e| e.item.clone()),
        )
        .take(MAX_SOURCES)
        .collect();
    (list.len() > sources.reply.len()).then_some(list)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn the_patterns_compile() {
        assert!(WORD.is_some() && DIGIT.is_some() && LETTER.is_some());
    }

    #[test]
    fn a_digit_token_or_a_four_word_run_counts() {
        let m = marks("See ORCHID-42 and the big red barn door");
        assert!(m.tokens.contains("orchid-42"));
        assert!(m.runs.contains("the big red barn"));
        assert!(!m.tokens.contains("see"));
    }
}
