//! The sources a reply was grounded in (core chat-sources.cjs `buildSources`, `usedInjected`, #552,
//! #1793, #1866) and which project files can be read at all (source-readability.cjs, #586, #2237).
use crate::text::{as_str, get, trim};
use js_json::{wtf16, JValue};
use regex::Regex;
use std::collections::{HashMap, HashSet};
use std::sync::OnceLock;

/// chat-sources.cjs `MAX_SOURCES`.
pub const MAX_SOURCES: usize = 12;
const SNIPPET_MAX: usize = 200;
const NAME_MAX: usize = 300;
const PAGE_MAX: f64 = 100_000.0;
const ATTRIBUTE_MAX: usize = 24_000;

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

/// `String(v || '')`.
fn text_of(v: &JValue) -> String {
    if v.truthy() {
        js_json::to_js_string(v).unwrap_or_default()
    } else {
        String::new()
    }
}

/// `String(v)`.
fn string(v: &JValue) -> String {
    js_json::to_js_string(v).unwrap_or_default()
}

/// source-readability.cjs `isUnreadable(file)`.
pub fn is_unreadable(file: &JValue) -> bool {
    if !matches!(file, JValue::Obj(_) | JValue::Arr(_)) {
        return false;
    }
    let a = get(file, "attachment");
    let d = get(file, "document");
    if a.truthy()
        && as_str(get(a, "state")) == Some("stored")
        && matches!(as_str(get(a, "group")), Some("Text" | "Documents"))
    {
        return true;
    }
    let blank = trim(&text_of(get(file, "content"))).is_empty();
    d.truthy() && matches!(as_str(get(d, "state")), Some("failed" | "queued")) && blank
}

/// source-readability.cjs `isWaiting(file)`.
pub fn is_waiting(file: &JValue) -> bool {
    let d = if matches!(file, JValue::Obj(_) | JValue::Arr(_)) {
        get(file, "document")
    } else {
        &JValue::Null
    };
    d.truthy()
        && as_str(get(d, "state")) == Some("queued")
        && trim(&text_of(get(file, "content"))).is_empty()
}

fn snippet_of(text: &JValue) -> String {
    let mut flat = String::new();
    let mut space = false;
    for c in text_of(text).chars() {
        if js_space(c) {
            space = true;
        } else {
            if space {
                flat.push(' ');
                space = false;
            }
            flat.push(c);
        }
    }
    if space {
        flat.push(' ');
    }
    let flat = trim(&flat).to_owned();
    if wtf16::len(&flat) > SNIPPET_MAX {
        let cut = wtf16::slice(&flat, SNIPPET_MAX - 1);
        format!("{}\u{2026}", cut.trim_end_matches(js_space))
    } else {
        flat
    }
}

fn page_re() -> Option<&'static Regex> {
    static R: OnceLock<Option<Regex>> = OnceLock::new();
    R.get_or_init(|| {
        Regex::new(r"\A[\t\n\x0B\x0C\r \u{a0}\u{1680}\u{2000}-\u{200a}\u{2028}\u{2029}\u{202f}\u{205f}\u{3000}\u{feff}]*\[Page ([0-9]{1,6})\](?:\n|\z)").ok()
    })
    .as_ref()
}

fn valid_page(n: f64) -> Option<f64> {
    (n.fract() == 0.0 && (1.0..=PAGE_MAX).contains(&n)).then_some(n)
}

fn page_of(body: &JValue) -> Option<f64> {
    let head = wtf16::slice(&text_of(body), 40);
    let m = page_re()?.captures(&head)?;
    m.get(1)?.as_str().parse::<f64>().ok().and_then(valid_page)
}

/// `Math.round(x)`: ties toward +∞.
fn js_round(x: f64) -> f64 {
    let r = x.floor();
    if x - r >= 0.5 {
        r + 1.0
    } else {
        r
    }
}

fn shape(
    id: &JValue,
    file: &JValue,
    body: &JValue,
    kind: &JValue,
    score: &JValue,
    page: Option<f64>,
) -> JValue {
    let id = if id.truthy() {
        string(id)
    } else {
        string(file)
    };
    let excerpt = as_str(kind) == Some("excerpt");
    let mut item = vec![
        ("id".to_owned(), JValue::Str(wtf16::slice(&id, NAME_MAX))),
        (
            "file".to_owned(),
            JValue::Str(wtf16::slice(&string(file), NAME_MAX)),
        ),
        ("snippet".to_owned(), JValue::Str(snippet_of(body))),
        (
            "kind".to_owned(),
            JValue::Str(if excerpt { "excerpt" } else { "file" }.into()),
        ),
    ];
    if let Some(p) = page.filter(|_| excerpt).and_then(valid_page) {
        item.push(("page".into(), JValue::Num(p)));
    }
    if let JValue::Num(s) = score {
        if s.is_finite() {
            item.push(("score".into(), JValue::Num(js_round(s * 1000.0) / 1000.0)));
        }
    }
    JValue::Obj(item)
}

fn list(v: &JValue) -> &[JValue] {
    match v {
        JValue::Arr(a) => a,
        _ => &[],
    }
}

/// chat-sources.cjs `buildSources(entries, files)`: the retrieval entries placed in the prompt, in
/// prompt order, shaped for the browser; only readable files of the project appear.
pub fn build_sources(entries: &[JValue], files: &JValue) -> Vec<JValue> {
    let mut by_name: HashMap<String, &JValue> = HashMap::new();
    for f in list(files).iter().filter(|f| !is_unreadable(f)) {
        if let Some(n) = as_str(get(f, "name")) {
            if f.truthy() {
                by_name.insert(n.to_owned(), f);
            }
        }
    }
    let mut out = Vec::new();
    for e in entries {
        if out.len() >= MAX_SOURCES {
            break;
        }
        let Some(f) = (if e.truthy() {
            as_str(get(e, "file")).and_then(|n| by_name.get(n))
        } else {
            None
        }) else {
            continue;
        };
        let a = get(f, "attachment");
        let id = if a.truthy() {
            get(a, "id").clone()
        } else {
            a.clone()
        };
        let kind = get(e, "kind");
        let page = if as_str(kind) == Some("excerpt") {
            page_of(get(e, "body"))
        } else {
            None
        };
        out.push(shape(
            &id,
            get(f, "name"),
            get(e, "body"),
            kind,
            get(e, "score"),
            page,
        ));
    }
    out
}

fn words_re() -> Option<&'static Regex> {
    static R: OnceLock<Option<Regex>> = OnceLock::new();
    R.get_or_init(|| Regex::new(r"[\p{L}\p{N}]+(?:[\-.][\p{L}\p{N}]+)*").ok())
        .as_ref()
}

fn letter_re() -> Option<&'static Regex> {
    static R: OnceLock<Option<Regex>> = OnceLock::new();
    R.get_or_init(|| Regex::new(r"\p{L}").ok()).as_ref()
}

fn number_re() -> Option<&'static Regex> {
    static R: OnceLock<Option<Regex>> = OnceLock::new();
    R.get_or_init(|| Regex::new(r"\p{N}").ok()).as_ref()
}

struct Marks {
    /// In first-seen order (the JS `Set` iteration order).
    tokens: Vec<String>,
    token_set: HashSet<String>,
    runs: Vec<String>,
    run_set: HashSet<String>,
}

fn marks(text: &JValue) -> Marks {
    let lower = wtf16::slice(&text_of(text), ATTRIBUTE_MAX).to_lowercase();
    let words: Vec<&str> = words_re()
        .map(|r| r.find_iter(&lower).map(|m| m.as_str()).collect())
        .unwrap_or_default();
    let mut tokens = Vec::new();
    let mut token_set = HashSet::new();
    for w in &words {
        let n = wtf16::len(w);
        let has_n = number_re().is_some_and(|r| r.is_match(w));
        let has_l = letter_re().is_some_and(|r| r.is_match(w));
        if n >= 4 && has_n && (has_l || n >= 5) && token_set.insert((*w).to_owned()) {
            tokens.push((*w).to_owned());
        }
    }
    let mut runs = Vec::new();
    let mut run_set = HashSet::new();
    for win in words.windows(4) {
        let r = win.join(" ");
        if run_set.insert(r.clone()) {
            runs.push(r);
        }
    }
    Marks {
        tokens,
        token_set,
        runs,
        run_set,
    }
}

/// A `Set` key with SameValueZero equality: primitives by value; an object only equals itself, and
/// no two parsed objects are the same object.
fn set_key(v: &JValue) -> Option<String> {
    match v {
        JValue::Obj(_) | JValue::Arr(_) => None,
        JValue::Undefined => Some("u".into()),
        JValue::Num(n) if *n == 0.0 => Some("n0".into()),
        other => wtf16::stringify(other).map(|s| format!("v{s}")),
    }
}

/// chat-sources.cjs `usedInjected(injected, placed, answer, question)` (#1793): the whole files put in
/// the prompt (and not already placed) that the answer drew on, in prompt order: it carries a
/// distinctive token (one with a digit) or a four-word run of theirs that the question did not.
pub fn used_injected(
    injected: &JValue,
    placed: &JValue,
    answer: &JValue,
    question: &JValue,
) -> Vec<JValue> {
    if trim(&text_of(answer)).is_empty() {
        return Vec::new();
    }
    let mut already: HashSet<String> = list(placed)
        .iter()
        .filter_map(|e| {
            if e.truthy() {
                set_key(get(e, "file"))
            } else {
                set_key(&JValue::Undefined)
            }
        })
        .collect();
    let said = marks(answer);
    let asked = marks(question);
    let mut out = Vec::new();
    for e in list(injected) {
        if !e.truthy() || as_str(get(e, "kind")) != Some("file") {
            continue;
        }
        let key = set_key(get(e, "file"));
        if key.as_ref().is_some_and(|k| already.contains(k)) {
            continue;
        }
        let from = marks(get(e, "body"));
        let hit = from
            .tokens
            .iter()
            .any(|t| said.token_set.contains(t) && !asked.token_set.contains(t))
            || from
                .runs
                .iter()
                .any(|r| said.run_set.contains(r) && !asked.run_set.contains(r));
        if hit {
            out.push(e.clone());
            if let Some(k) = key {
                already.insert(k);
            }
        }
    }
    out
}
