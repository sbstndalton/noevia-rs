//! `server/search-query.cjs` (#1523): a model can copy the request wrapper around a question into
//! the search it runs ("Search the web: what is the latest ... Cite the source."). A search query is
//! the question itself, so the wrapper is taken off the `query` of a search tool before the call is
//! made or kept. Only a tool whose name says it searches, and only its string `query`, is changed;
//! everything else is returned byte for byte. A query is never emptied.

use super::js_regex::{is_js_space, strip_trailing, trim, Regex};
use super::{test, text, units, Pattern};
use js_json::{wtf16, JValue};
use std::borrow::Cow;
use std::sync::LazyLock;

/// `LEADING_PUNCTUATION`: leading punctuation and quote marks.
static LEADING: Pattern = LazyLock::new(|| Regex::new(r#"^[\s:;,.\-–—"'`“”‘’]+"#, false).ok());
/// `SEARCH_WRAPPER`: "Search the web:", "Please search the internet for", "Web search:".
static SEARCH_WRAPPER: Pattern = LazyLock::new(|| {
    Regex::new(
        r"^(?:(?:please\s+)?(?:search|look\s*up|google)\s+(?:the\s+)?(?:web|internet|online)(?:\s+(?:for|about))?\s*[:,-]?|(?:web|internet)\s+search\s*[:,-])\s*",
        true,
    )
    .ok()
});
/// `CITE_LEAD`: a citation instruction at the start.
static CITE_LEAD: Pattern = LazyLock::new(|| {
    Regex::new(
        r"^cite\s+(?:the\s+|your\s+|a\s+|any\s+)?(?:sources?|links?|citations?|urls?|references?)\s*[:,-]?\s*",
        true,
    )
    .ok()
});
/// `CITE_TAIL`: a citation instruction at the end.
static CITE_TAIL: Pattern = LazyLock::new(|| {
    Regex::new(
        r"[\s.?!;,-]*(?:\band\s+)?(?:\bplease\s+)?\bcite\s+(?:the\s+|your\s+|a\s+|any\s+|all\s+)?(?:sources?|links?|citations?|urls?|references?)\b(?:\s+(?:for|with|in|from|to)\b[^.?!]*)?[\s.?!;,]*$",
        true,
    )
    .ok()
});
static SEARCH_NAME: Pattern = LazyLock::new(|| Regex::new("search", true).ok());

pub(super) fn patterns_compile() -> bool {
    [
        &LEADING,
        &SEARCH_WRAPPER,
        &CITE_LEAD,
        &CITE_TAIL,
        &SEARCH_NAME,
    ]
    .iter()
    .all(|p| p.is_some())
}

fn remove(re: &Pattern, text: &[u16]) -> Vec<u16> {
    match re.as_ref() {
        Some(re) => re.remove_first(text),
        None => text.to_vec(),
    }
}

/// Whether `text` holds "cite" in any ASCII case: every `CITE_TAIL` match does, so text without it
/// skips the pattern's backtracking.
fn mentions_cite(text: &[u16]) -> bool {
    let lower = |u: u16| {
        if (0x41..=0x5A).contains(&u) {
            u + 0x20
        } else {
            u
        }
    };
    let want = units("cite");
    text.windows(4)
        .any(|w| w.iter().map(|&u| lower(u)).eq(want.iter().copied()))
}

fn clean_units(query: &[u16]) -> Option<Vec<u16>> {
    let mut text = trim(query).to_vec();
    loop {
        let previous = text.clone();
        let step = remove(&LEADING, &text);
        let step = remove(&SEARCH_WRAPPER, &step);
        let step = remove(&CITE_LEAD, &step);
        text = trim(&step).to_vec();
        if text == previous {
            break;
        }
    }
    if mentions_cite(&text) {
        text = remove(&CITE_TAIL, &text);
    }
    let trailing = |u: u16| is_js_space(u) || ".?!;,:".encode_utf16().any(|p| p == u);
    let text = trim(strip_trailing(&text, trailing)).to_vec();
    (text.len() >= 3).then_some(text)
}

/// `cleanSearchQuery(query)`: the question a search query should carry. The input unchanged when
/// nothing is removed or when removing the wrapper would leave fewer than three UTF-16 units.
pub fn clean_search_query(query: &str) -> String {
    match clean_units(&units(query)) {
        Some(cleaned) => text(&cleaned),
        None => query.to_string(),
    }
}

/// `JSON.parse` input for `js_json::wtf16::parse`: a lone surrogate written raw inside a JSON
/// string (legal for `JSON.parse`) becomes its `\udxxx` escape. `None` where `JSON.parse` throws
/// on the raw surrogate itself: outside a string, or right after a backslash.
fn escape_raw_lone(raw: &str) -> Option<Cow<'_, str>> {
    if !raw.chars().any(|c| wtf16::lone_unit(c).is_some()) {
        return Some(Cow::Borrowed(raw));
    }
    let mut out = String::with_capacity(raw.len() + 16);
    let (mut in_string, mut escaped) = (false, false);
    for c in raw.chars() {
        if let Some(u) = wtf16::lone_unit(c) {
            if !in_string || escaped {
                return None;
            }
            out.push_str(&format!("\\u{u:04x}"));
            continue;
        }
        if escaped {
            escaped = false;
        } else if in_string && c == '\\' {
            escaped = true;
        } else if c == '"' {
            in_string = !in_string;
        }
        out.push(c);
    }
    Some(Cow::Owned(out))
}

/// `cleanSearchArgs(name, rawArgs)`: the arguments a call carries, with a search tool's `query`
/// cleaned. Anything that is not a JSON object with a string `query` on a tool whose name says it
/// searches is returned as it arrived; a cleaned one is `JSON.stringify({ ...args, query })`.
pub fn clean_search_args<'a>(name: &str, raw_args: &'a str) -> Cow<'a, str> {
    let unchanged = Cow::Borrowed(raw_args);
    if !test(&SEARCH_NAME, &units(name)) || raw_args.is_empty() {
        return unchanged;
    }
    let Some(parse_text) = escape_raw_lone(raw_args) else {
        return unchanged;
    };
    let Ok(JValue::Obj(mut items)) = wtf16::parse(&parse_text) else {
        return unchanged;
    };
    let Some(query) = items.iter_mut().find_map(|(k, v)| match v {
        JValue::Str(s) if k == "query" => Some(s),
        _ => None,
    }) else {
        return unchanged;
    };
    let before = units(query);
    let Some(cleaned) = clean_units(&before) else {
        return unchanged;
    };
    if cleaned == before {
        return unchanged;
    }
    *query = text(&cleaned);
    match wtf16::stringify(&JValue::Obj(items)) {
        Some(out) => Cow::Owned(out),
        None => unchanged,
    }
}
