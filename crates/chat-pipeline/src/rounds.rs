//! The round loop's own decisions about a reply's text (core announced-action.cjs #1552 and
//! final-answer.cjs #1693). JavaScript regular expressions are kept exactly: `\s` is ECMAScript's
//! WhiteSpace and LineTerminator set, `\b` is ASCII, and `i` folds as `toUpperCase` does.
use crate::text::trim;
use js_json::wtf16;
use regex::Regex;
use std::sync::OnceLock;

/// ECMAScript `\s` as a class body.
const S: &str = r"\t\n\x0B\x0C\r \u{a0}\u{1680}\u{2000}-\u{200a}\u{2028}\u{2029}\u{202f}\u{205f}\u{3000}\u{feff}";

fn js_space(c: char) -> bool {
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

/// announced-action.cjs `TAIL_CHARS`.
pub const TAIL_CHARS: usize = 280;
/// announced-action.cjs `ANNOUNCED_NOTE`.
pub const ANNOUNCED_NOTE: &str = "You said you would use a tool, but no tool was called, so nothing was read or run. Call the tool now. If no tool fits, give the final answer from what you already have, and say plainly what you could not check. Do not describe files, excerpts or results you have not seen.";
/// final-answer.cjs `LAST_TOOL_ROUND`.
pub const LAST_TOOL_ROUND: u32 = 2;
/// final-answer.cjs `FINAL_ANSWER_NOTE`.
pub const FINAL_ANSWER_NOTE: &str = "Write your final answer to the user now, using the tool results above. Cite the source links they contain. Do not call any more tools. If a tool failed, say so in one line and answer from the results that did come back.";
/// final-answer.cjs `MAX_SOURCES`.
pub const MAX_SOURCES: usize = 5;

fn announce() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| {
        let s = format!("[{S}]");
        let verbs = format!(r"read|open|check|look(?:{s}+(?:at|up|into|through))?|search|list|fetch|load|scan|inspect|examine|retrieve|browse|query|calculate|count|compute|go{s}+through|use{s}+the");
        let src = format!(
            r"(?i)(?:^|[{S}.!?:;(])(?:I{s}*(?:will|'ll|’ll|am{s}+going{s}+to|'m{s}+going{s}+to|’m{s}+going{s}+to|need{s}+to|have{s}+to|should)|let{s}+me|let's|(?:first|then|next),?{s}+I(?:{s}*'ll|{s}*’ll|{s}+will)?){s}+(?:now{s}+|first{s}+|quickly{s}+|carefully{s}+|go{s}+(?:and{s}+)?)?(?:{verbs})(?-u:\b)"
        );
        Regex::new(&crate::text::js_fold(&src)).unwrap_or_else(|_| Regex::new("$^").unwrap_or_else(|_| unreachable!()))
    })
}

fn url_re() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| {
        let src = format!(r#"(?i)(?-u:\b)https?://[^{S}<>"'`)\]}}]+"#);
        Regex::new(&crate::text::js_fold(&src)).unwrap_or_else(|_| Regex::new("$^").unwrap_or_else(|_| unreachable!()))
    })
}

/// `s.slice(-n)` over UTF-16 units: a surrogate pair cut in half leaves its low half, held here as
/// U+FFFD (neither a word nor a space character, as the lone surrogate is in JS).
fn tail_units(s: &str, n: usize) -> String {
    let len = wtf16::len(s);
    if len <= n {
        return s.to_owned();
    }
    let mut skip = len - n;
    let mut out = String::new();
    for c in s.chars() {
        let w = c.len_utf16();
        if skip >= w {
            skip -= w;
            continue;
        }
        if skip > 0 {
            out.push('\u{FFFD}');
            skip = 0;
            continue;
        }
        out.push(c);
    }
    out
}

/// `tail.split(/(?<=[.!?:])\s+/).filter(Boolean).at(-1)`.
fn last_sentence(tail: &str) -> &str {
    let mut start = 0;
    let mut prev: Option<char> = None;
    let mut it = tail.char_indices().peekable();
    while let Some((i, c)) = it.next() {
        if js_space(c) && matches!(prev, Some('.' | '!' | '?' | ':')) {
            let mut end = i + c.len_utf8();
            while let Some(&(j, d)) = it.peek() {
                if !js_space(d) {
                    break;
                }
                end = j + d.len_utf8();
                it.next();
            }
            if end < tail.len() {
                start = end;
            }
            prev = None;
            continue;
        }
        prev = Some(c);
    }
    tail.get(start..).unwrap_or("")
}

/// announced-action.cjs `announcesToolAction(text)`: the last sentence of the reply's tail promises
/// a tool action (first person, future) that was not taken.
pub fn announces_tool_action(text: &str) -> bool {
    let tail = tail_units(trim(text), TAIL_CHARS);
    if tail.is_empty() {
        return false;
    }
    announce().is_match(last_sentence(&tail))
}

/// final-answer.cjs `needsFinalPass({round, toolCalls, hasContent})`.
pub fn needs_final_pass(round: u32, tool_calls: usize, has_content: bool) -> bool {
    if tool_calls > 0 {
        return round >= LAST_TOOL_ROUND;
    }
    round > 0 && !has_content
}

/// final-answer.cjs `sourceLinks(text)`: the distinct http(s) links of one tool result, in order.
pub fn source_links(text: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for m in url_re().find_iter(text) {
        let url = m
            .as_str()
            .trim_end_matches(['.', ',', ';', ':', '!', '?'])
            .to_owned();
        if wtf16::len(&url) > 400 || out.contains(&url) {
            continue;
        }
        out.push(url);
        if out.len() >= MAX_SOURCES {
            break;
        }
    }
    out
}

/// final-answer.cjs `fallbackText({sources})`.
pub fn fallback_text(sources: &[String]) -> String {
    let links: Vec<&String> = sources.iter().take(MAX_SOURCES).collect();
    if links.is_empty() {
        "\n\nThe model did not write a final answer from the tool results. Its reasoning is in the thinking above. Try again or choose another model.".to_owned()
    } else {
        format!(
            "\n\nThe model did not write a final answer. Its reasoning is in the thinking above; these are the sources the tools returned:\n{}",
            links.iter().map(|u| format!("- {u}")).collect::<Vec<_>>().join("\n")
        )
    }
}
