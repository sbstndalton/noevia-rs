//! `server/final-answer.cjs` (#1693): a reply whose tools ran but that wrote no answer gets one
//! bounded extra round with no tools ([`FINAL_ANSWER_NOTE`]); if that also writes none, the reply
//! keeps the links the tools returned ([`fallback_text`]).

use super::js_regex::Regex;
use super::{text, units, Pattern};
use std::sync::LazyLock;

/// The last round that may call tools (`LAST_TOOL_ROUND`).
pub const LAST_TOOL_ROUND: u32 = 2;
/// The user message of the final-answer round (`FINAL_ANSWER_NOTE`).
pub const FINAL_ANSWER_NOTE: &str = "Write your final answer to the user now, using the tool results above. Cite the source links they contain. Do not call any more tools. If a tool failed, say so in one line and answer from the results that did come back.";
/// The most links kept from one tool result, and shown in the fallback (`MAX_SOURCES`).
pub const MAX_SOURCES: usize = 5;
/// The longest link kept, in UTF-16 units.
const MAX_LINK_UNITS: usize = 400;

/// `URL_RE` (`/gi`).
static URL_RE: Pattern = LazyLock::new(|| Regex::new(r#"\bhttps?:\/\/[^\s<>"'`)\]}]+"#, true).ok());

pub(super) fn patterns_compile() -> bool {
    URL_RE.is_some()
}

/// `needsFinalPass({ round, toolCalls, hasContent })`: the last tool round called tools (their
/// results were never read), or a round after tool results wrote no answer text.
pub fn needs_final_pass(round: u32, tool_calls: usize, has_content: bool) -> bool {
    if tool_calls > 0 {
        return round >= LAST_TOOL_ROUND;
    }
    round > 0 && !has_content
}

/// `sourceLinks(text)`: the distinct http(s) links in one tool result, in order, at most
/// [`MAX_SOURCES`], each without its trailing `.,;:!?` and at most 400 UTF-16 units.
pub fn source_links(result: &str) -> Vec<String> {
    let all = units(result);
    let mut out: Vec<Vec<u16>> = Vec::new();
    let Some(re) = URL_RE.as_ref() else {
        return Vec::new();
    };
    for (s, e) in re.find_all(&all) {
        let found = all.get(s..e).unwrap_or_default();
        let url =
            super::js_regex::strip_trailing(found, |u| ".,;:!?".encode_utf16().any(|p| p == u));
        if url.len() > MAX_LINK_UNITS || out.iter().any(|o| o == url) {
            continue;
        }
        out.push(url.to_vec());
        if out.len() >= MAX_SOURCES {
            break;
        }
    }
    out.iter().map(|u| text(u)).collect()
}

/// `fallbackText({ sources })`: the reply text when the final-answer round wrote nothing, with at
/// most [`MAX_SOURCES`] of the gathered links.
pub fn fallback_text<S: AsRef<str>>(sources: &[S]) -> String {
    let links = sources
        .get(..sources.len().min(MAX_SOURCES))
        .unwrap_or_default();
    if links.is_empty() {
        return "\n\nThe model did not write a final answer from the tool results. Its reasoning is in the thinking above. Try again or choose another model.".to_string();
    }
    let list: Vec<String> = links.iter().map(|u| format!("- {}", u.as_ref())).collect();
    format!(
        "\n\nThe model did not write a final answer. Its reasoning is in the thinking above; these are the sources the tools returned:\n{}",
        list.join("\n")
    )
}
