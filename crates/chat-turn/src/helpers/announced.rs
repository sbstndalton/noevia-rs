//! `server/announced-action.cjs` (#1552): a small model with tools on offer sometimes ends its reply
//! with a plan ("I will read the full file and total the months.") and no tool call. When the end
//! of a reply announces a tool action and no tool was called, the chat loop gives the model one more
//! try with [`ANNOUNCED_NOTE`]. Only the last sentence of the reply's last [`TAIL_CHARS`] UTF-16
//! units is checked, and only first-person future phrasing of a tool-shaped verb.

use super::js_regex::{is_js_space, trim, Regex};
use super::{test, units, Pattern};
use std::sync::LazyLock;

/// How many UTF-16 units of the reply's end are checked (`TAIL_CHARS`).
pub const TAIL_CHARS: usize = 280;

/// The user message of the retry round (`ANNOUNCED_NOTE`).
pub const ANNOUNCED_NOTE: &str = "You said you would use a tool, but no tool was called, so nothing was read or run. Call the tool now. If no tool fits, give the final answer from what you already have, and say plainly what you could not check. Do not describe files, excerpts or results you have not seen.";

const VERBS: &str = r"read|open|check|look(?:\s+(?:at|up|into|through))?|search|list|fetch|load|scan|inspect|examine|retrieve|browse|query|calculate|count|compute|go\s+through|use\s+the";

/// `ANNOUNCE`, built from the same pieces as in Node.
static ANNOUNCE: Pattern = LazyLock::new(|| {
    Regex::new(
        &format!(
            r"(?:^|[\s.!?:;(])(?:I\s*(?:will|'ll|’ll|am\s+going\s+to|'m\s+going\s+to|’m\s+going\s+to|need\s+to|have\s+to|should)|let\s+me|let's|(?:first|then|next),?\s+I(?:\s*'ll|\s*’ll|\s+will)?)\s+(?:now\s+|first\s+|quickly\s+|carefully\s+|go\s+(?:and\s+)?)?(?:{VERBS})\b"
        ),
        true,
    )
    .ok()
});

pub(super) fn patterns_compile() -> bool {
    ANNOUNCE.is_some()
}

/// `tail.split(/(?<=[.!?:])\s+/).filter(Boolean).at(-1) || ''`, written out: the split points are
/// runs of `\s` right after `.`, `!`, `?` or `:`, found left to right as `String.prototype.split`
/// finds them.
fn last_sentence(tail: &[u16]) -> &[u16] {
    let after_stop = |q: usize| {
        q.checked_sub(1)
            .and_then(|i| tail.get(i))
            .is_some_and(|&u| ".!?:".encode_utf16().any(|s| s == u))
    };
    let mut last: &[u16] = &[];
    let (mut p, mut q) = (0usize, 0usize);
    while q < tail.len() {
        let run = tail
            .get(q..)
            .unwrap_or_default()
            .iter()
            .take_while(|&&u| is_js_space(u))
            .count();
        if run == 0 || !after_stop(q) {
            q += 1;
            continue;
        }
        let piece = tail.get(p..q).unwrap_or_default();
        if !piece.is_empty() {
            last = piece;
        }
        p = q + run;
        q = p;
    }
    let piece = tail.get(p..).unwrap_or_default();
    if piece.is_empty() {
        last
    } else {
        piece
    }
}

/// `announcesToolAction(text)`: true when the last sentence of the reply's end announces a tool
/// action (`I will read ...`, `Let me check ...`) that was not taken.
pub fn announces_tool_action(text: &str) -> bool {
    let all = units(text);
    let trimmed = trim(&all);
    let tail = trimmed
        .get(trimmed.len().saturating_sub(TAIL_CHARS)..)
        .unwrap_or_default();
    if tail.is_empty() {
        return false;
    }
    test(&ANNOUNCE, last_sentence(tail))
}
