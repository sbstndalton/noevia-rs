//! A search tool's query without the request wrapper a model copied into it (core search-query.cjs,
//! #1523). JavaScript's non-unicode `/i`, `\s` and `\b` are kept (see [`crate::rounds`]).
use crate::text::{as_str, get, set, spread, trim};
use js_json::{wtf16, JValue};
use regex::Regex;
use std::sync::OnceLock;

const S: &str = r"\t\n\x0B\x0C\r \u{a0}\u{1680}\u{2000}-\u{200a}\u{2028}\u{2029}\u{202f}\u{205f}\u{3000}\u{feff}";

struct Res {
    leading: Regex,
    wrapper: Regex,
    cite_lead: Regex,
    cite_tail: Regex,
    trailing: Regex,
    search: Regex,
}

fn res() -> Option<&'static Res> {
    static R: OnceLock<Option<Res>> = OnceLock::new();
    R.get_or_init(|| {
        let s = format!("[{S}]");
        let b = r"(?-u:\b)";
        Some(Res {
            leading: Regex::new(&format!(r#"\A[{S}:;,.\-–—"'`“”‘’]+"#)).ok()?,
            wrapper: Regex::new(&format!(
                r"(?i)\A(?:(?:please{s}+)?(?:search|look{s}*up|google){s}+(?:the{s}+)?(?:web|internet|online)(?:{s}+(?:for|about))?{s}*[:,\-]?|(?:web|internet){s}+search{s}*[:,\-]){s}*"
            ))
            .ok()?,
            cite_lead: Regex::new(&format!(
                r"(?i)\Acite{s}+(?:the{s}+|your{s}+|a{s}+|any{s}+)?(?:sources?|links?|citations?|urls?|references?){s}*[:,\-]?{s}*"
            ))
            .ok()?,
            cite_tail: Regex::new(&format!(
                r"(?i)[{S}.?!;,\-]*(?:{b}and{s}+)?(?:{b}please{s}+)?{b}cite{s}+(?:the{s}+|your{s}+|a{s}+|any{s}+|all{s}+)?(?:sources?|links?|citations?|urls?|references?){b}(?:{s}+(?:for|with|in|from|to){b}[^.?!]*)?[{S}.?!;,]*\z"
            ))
            .ok()?,
            trailing: Regex::new(&format!(r"[{S}.?!;,:]+\z")).ok()?,
            search: Regex::new(r"(?i)search").ok()?,
        })
    })
    .as_ref()
}

/// Rust's Unicode folding matches U+212A for `k` and U+017F for `s`; JavaScript's `/i` does not.
fn guard(s: &str) -> String {
    s.replace('\u{212A}', "\u{FFFF}").replace('\u{17F}', "\u{FFFE}")
}

fn unguard(s: &str) -> String {
    s.replace('\u{FFFF}', "\u{212A}").replace('\u{FFFE}', "\u{17F}")
}

/// search-query.cjs `cleanSearchQuery(query)`.
pub fn clean_search_query(query: &str) -> String {
    let Some(r) = res() else {
        return query.to_owned();
    };
    let mut text = guard(trim(query));
    loop {
        let previous = text.clone();
        let t = r.leading.replace(&text, "").into_owned();
        let t = r.wrapper.replace(&t, "").into_owned();
        let t = r.cite_lead.replace(&t, "").into_owned();
        text = trim(&t).to_owned();
        if previous == text {
            break;
        }
    }
    let t = r.cite_tail.replace(&text, "").into_owned();
    let t = r.trailing.replace(&t, "").into_owned();
    let text = unguard(trim(&t));
    if wtf16::len(&text) >= 3 {
        text
    } else {
        query.to_owned()
    }
}

/// search-query.cjs `cleanSearchArgs(name, rawArgs)`: a search tool's JSON-object arguments with
/// their string `query` cleaned; anything else exactly as it arrived.
pub fn clean_search_args(name: &JValue, raw: &JValue) -> JValue {
    let (Some(n), Some(text)) = (as_str(name), as_str(raw)) else {
        return raw.clone();
    };
    let Some(r) = res() else {
        return raw.clone();
    };
    if !r.search.is_match(&guard(n)) || text.is_empty() {
        return raw.clone();
    }
    let Ok(args) = wtf16::parse(text) else {
        return raw.clone();
    };
    if !matches!(args, JValue::Obj(_)) {
        return raw.clone();
    }
    let Some(q) = as_str(get(&args, "query")) else {
        return raw.clone();
    };
    let cleaned = clean_search_query(q);
    if cleaned == q {
        return raw.clone();
    }
    let mut out = spread(&args);
    set(&mut out, "query", JValue::Str(cleaned));
    wtf16::stringify(&out).map_or_else(|| raw.clone(), JValue::Str)
}
