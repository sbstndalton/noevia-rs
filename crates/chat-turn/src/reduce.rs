//! noevia-core's `server/tool-result-reduce.cjs` `reduceToolResult`: what a tool returns, compacted
//! before it goes back to the model, with a note the model can read whenever anything was cut.
//!
//! Byte for byte Node's output (replayed against `tests/fixtures/node-reduce.v1.json`, recorded by
//! `tests/record_reduce.cjs`). Text is held as [`crate::js`] holds it: a lone surrogate is one
//! private-use character, and every length, slice and budget counts UTF-16 units. The branches:
//! - at or under `max_chars`: the result as it is;
//! - not JSON: the first `max_chars` units and a truncation note;
//! - an array of at least two plain objects with a useful key: a tab-separated table (header row
//!   once, a legend naming the omitted columns), whole records dropped from the end to fit, and
//!   a final clamp to `max_chars` that keeps its note;
//! - other JSON: empty fields dropped, long strings shortened, serialised without indentation.
//!
//! What Node does and this port copies, because the result is a parsed JS object: a key missing
//! from a row reads `Object.prototype`'s property of that name (`row.constructor` is a function,
//! so the column is useful and the cell reads `undefined`; `row.__proto__` is `{}`), and the
//! compacted copy loses every `__proto__` key (`out.__proto__ = v` sets the prototype).
//!
//! Differences, tested in `tests/node_reduce.rs` against Node's recorded output: text `JSON.parse`
//! accepts but `js_json::wtf16::parse` refuses ([`wtf16::Refusal::Unsupported`]: a number outside
//! the `f64` range, which Node reads as `Infinity`; nesting deeper than 128 levels; an escaped
//! character of U+10F800..=U+10FFFF) takes the not-JSON branch here, a plain truncation, where
//! Node tabulates or compacts it. Neither loses the fact of the cut: both carry a note.
//!
//! Linear in the input: per-row keys are looked up through one column index, line lengths are
//! measured once, and records are dropped by subtracting lengths, as Node's `fitLines` does.

use std::collections::HashMap;
use std::fmt::Write as _;

use js_json::{wtf16, JValue};

use crate::js;

pub const DEFAULT_MAX_CHARS: usize = 8000;
/// Long enough for a path, a title or a one-line summary.
pub const DEFAULT_MAX_VALUE_CHARS: usize = 300;
/// Two rows is where a header row starts paying for itself.
const MIN_ROWS: usize = 2;
/// The legend names at most this many omitted columns.
pub const MAX_LEGEND_KEYS: usize = 50;
/// `Object.prototype`'s methods: what a JS object without such an own key reads for it.
const INHERITED_FUNCTIONS: [&str; 11] = [
    "constructor",
    "__defineGetter__",
    "__defineSetter__",
    "hasOwnProperty",
    "__lookupGetter__",
    "__lookupSetter__",
    "isPrototypeOf",
    "propertyIsEnumerable",
    "toString",
    "valueOf",
    "toLocaleString",
];
const PROTO: &str = "__proto__";

/// `reduceToolResult`'s `{ text, reduced, note }`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reduced {
    /// What goes to the model, already carrying `note` when reduced.
    pub text: String,
    pub reduced: bool,
    /// Empty when the result passed through untouched.
    pub note: String,
}

/// `reduceToolResult(result, { maxChars })`; `max_chars` 0 is the default (`opts.maxChars || 8000`).
pub fn reduce_tool_result(result: &str, max_chars: usize) -> Reduced {
    let max = if max_chars == 0 {
        DEFAULT_MAX_CHARS
    } else {
        max_chars
    };
    let length = js::len(result);
    if length <= max {
        return Reduced {
            text: result.to_string(),
            reduced: false,
            note: String::new(),
        };
    }
    let Some(parsed) = parse(result) else {
        let note = format!(
            "[truncated: showing the first {max} of {length} characters. Ask for a narrower query if you need the rest.]"
        );
        return Reduced {
            text: format!("{}\n\n{note}", js::slice(result, max)),
            reduced: true,
            note,
        };
    };
    if let JValue::Arr(items) = &parsed {
        let rows: Vec<&[(String, JValue)]> = items
            .iter()
            .filter_map(|v| match v {
                JValue::Obj(fields) => Some(fields.as_slice()),
                _ => None,
            })
            .collect();
        if rows.len() >= MIN_ROWS && rows.len() == items.len() {
            if let Some(reduced) = tabulated(&rows, max) {
                return reduced;
            }
        }
    }
    let compacted = js::stringify(&compact_value(&parsed));
    let compacted_len = js::len(&compacted);
    let compact_note = format!(
        "[compacted: empty fields removed and long values shortened; {length} \u{2192} {compacted_len} characters.]"
    );
    if js::len(&compact_note) + 1 + compacted_len <= max {
        return Reduced {
            text: format!("{compact_note}\n{compacted}"),
            reduced: true,
            note: compact_note,
        };
    }
    let note = format!(
        "[truncated: showing the first {max} of {compacted_len} characters after compaction. Ask for a narrower query if you need the rest.]"
    );
    Reduced {
        text: format!("{}\n\n{note}", js::slice(&compacted, max)),
        reduced: true,
        note,
    }
}

/// `JSON.parse(text)` over text held as `wtf16` holds it. A lone surrogate written raw inside a
/// JSON string is one code unit there, as its `\uXXXX` escape is, so it is handed to the parser
/// as that escape; anywhere else it is invalid JSON either way and is left for the parser to refuse.
fn parse(text: &str) -> Option<JValue> {
    if !text.chars().any(|c| wtf16::lone_unit(c).is_some()) {
        return wtf16::parse(text).ok();
    }
    let mut out = String::with_capacity(text.len() + 16);
    let (mut in_string, mut escaped) = (false, false);
    for c in text.chars() {
        match wtf16::lone_unit(c) {
            Some(unit) if in_string && !escaped => {
                let _ = write!(out, "\\u{unit:04x}");
            }
            _ => out.push(c),
        }
        if !in_string {
            in_string = c == '"';
        } else if escaped {
            escaped = false;
        } else if c == '\\' {
            escaped = true;
        } else if c == '"' {
            in_string = false;
        }
    }
    wtf16::parse(&out).ok()
}

/// `isEmptyValue`: no information for the model. `false` and `0` are information.
fn is_empty(v: &JValue) -> bool {
    match v {
        JValue::Undefined | JValue::Null => true,
        JValue::Str(s) => s.is_empty(),
        JValue::Arr(items) => items.is_empty(),
        JValue::Obj(items) => items.is_empty(),
        JValue::Bool(_) | JValue::Num(_) => false,
    }
}

/// `truncateValue`: a string as it is, anything else as `JSON.stringify` writes it, cut past
/// `DEFAULT_MAX_VALUE_CHARS` with a count of what went.
fn truncate_value(v: &JValue) -> String {
    let s = match v {
        JValue::Str(s) => s.clone(),
        other => js::stringify(other),
    };
    truncate_string(s)
}

fn truncate_string(s: String) -> String {
    let length = js::len(&s);
    if length <= DEFAULT_MAX_VALUE_CHARS {
        return s;
    }
    format!(
        "{}\u{2026}[+{} chars]",
        js::slice(&s, DEFAULT_MAX_VALUE_CHARS),
        length - DEFAULT_MAX_VALUE_CHARS
    )
}

/// `s.replace(/[\t\r\n]+/g, ' ')`.
fn one_line(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut in_run = false;
    for c in s.chars() {
        if matches!(c, '\t' | '\r' | '\n') {
            if !in_run {
                out.push(' ');
            }
            in_run = true;
        } else {
            out.push(c);
            in_run = false;
        }
    }
    out
}

/// A row's cell, as `row[k]` reads it on a JS object from `JSON.parse`.
fn cell(own: Option<&JValue>, key: &str) -> String {
    match own {
        Some(v) => one_line(&truncate_value(v)),
        // `Object.prototype`: JSON.stringify writes it as `{}`.
        None if key == PROTO => "{}".to_string(),
        // A function: `JSON.stringify` gives `undefined`, and `String(undefined)` is the cell.
        None if INHERITED_FUNCTIONS.contains(&key) => "undefined".to_string(),
        None => String::new(),
    }
}

/// `listKeys`: the first keys (each at most 80 units) that fit `budget`, and how many more.
fn list_keys(keys: &[&str], budget: usize) -> String {
    let mut shown: Vec<String> = Vec::new();
    let mut length = 0usize;
    for key in keys.iter().take(MAX_LEGEND_KEYS) {
        let k = if js::len(key) > 80 {
            format!("{}\u{2026}", js::slice(key, 80))
        } else {
            (*key).to_string()
        };
        let k_len = js::len(&k);
        if !shown.is_empty() && length + k_len + 2 > budget {
            break;
        }
        shown.push(k);
        length += k_len + 2;
    }
    let joined = shown.join(", ");
    if keys.len() > shown.len() {
        format!("{joined}, \u{2026} {} more", keys.len() - shown.len())
    } else {
        joined
    }
}

/// `clampText`: at most `max` units, the note kept at the end. `(text, clamped, note)`.
fn clamp_text(text: String, max: usize) -> (String, bool, String) {
    let length = js::len(&text);
    if length <= max {
        return (text, false, String::new());
    }
    let mut note = format!(
        "[truncated: showing the first part of {length} characters to fit. Ask for a narrower query if you need the rest.]"
    );
    if js::len(&note) + 1 >= max {
        note = js::slice(&note, max.saturating_sub(1));
    }
    let room = max.saturating_sub(js::len(&note) + 1);
    let joined = format!("{}\n{note}", js::slice(&text, room));
    (js::slice(&joined, max), true, note)
}

/// Per key, in first-seen order: in how many rows it is an own key, and whether one of those
/// values is not empty.
struct KeyStat<'a> {
    key: &'a str,
    own_rows: usize,
    filled: bool,
}

/// The table branch, or `None` when no key is useful (Node then compacts).
fn tabulated(rows: &[&[(String, JValue)]], max: usize) -> Option<Reduced> {
    // `usefulKeys`: the union of keys in first-seen order, minus keys empty in every row. A row
    // without the key reads the inherited property: a method is not empty, `__proto__` is.
    let mut stats: Vec<KeyStat> = Vec::new();
    let mut index: HashMap<&str, usize> = HashMap::new();
    for row in rows {
        for (k, v) in row.iter() {
            let at = *index.entry(k.as_str()).or_insert_with(|| {
                stats.push(KeyStat {
                    key: k.as_str(),
                    own_rows: 0,
                    filled: false,
                });
                stats.len() - 1
            });
            if let Some(stat) = stats.get_mut(at) {
                stat.own_rows += 1;
                stat.filled |= !is_empty(v);
            }
        }
    }
    let useful =
        |s: &KeyStat| s.filled || (s.own_rows < rows.len() && INHERITED_FUNCTIONS.contains(&s.key));
    let all_keys: Vec<&str> = stats.iter().filter(|s| useful(s)).map(|s| s.key).collect();
    if all_keys.is_empty() {
        return None;
    }
    let dropped: Vec<&str> = stats.iter().filter(|s| !useful(s)).map(|s| s.key).collect();

    // The header takes at most a quarter of the budget; the first key always goes in.
    let header_budget = (max / 4).max(1);
    let mut keys: Vec<&str> = Vec::new();
    let mut header_len = 0usize;
    for k in &all_keys {
        let cost = js::len(k) + usize::from(!keys.is_empty());
        if !keys.is_empty() && header_len + cost > header_budget {
            break;
        }
        keys.push(k);
        header_len += cost;
    }
    let cut_keys = all_keys.get(keys.len()..).unwrap_or(&[]);
    // `Math.floor(maxChars * 0.1)`: 0.1 as a double is a little over a tenth, so this is `max / 10`.
    let legend_budget = (max / 10).max(80);
    let mut legend = format!(
        "[{} records, tab-separated, first line is the column names",
        rows.len()
    );
    if !dropped.is_empty() {
        let _ = write!(
            legend,
            "; {} column(s) omitted because they were empty in every record: {}",
            dropped.len(),
            list_keys(&dropped, legend_budget)
        );
    }
    if !cut_keys.is_empty() {
        let _ = write!(
            legend,
            "; {} further column(s) not shown to fit: {}",
            cut_keys.len(),
            list_keys(cut_keys, legend_budget)
        );
    }
    legend.push(']');

    let lines = tabular_lines(rows, &keys);
    let lens: Vec<usize> = lines.iter().map(|l| js::len(l)).collect();
    let legend_len = js::len(&legend);
    let total = rows.len();
    let note_for = |n: usize| {
        format!("[showing {n} of {total} records; {} omitted to fit. Narrow the query if you need them.]", total - n)
    };
    let full_len = lens.iter().sum::<usize>() + lens.len().saturating_sub(1);
    let mut kept = total;
    if legend_len + 1 + full_len > max {
        // Node's `fitLines(lines.slice(0, rows.length), fits)`: the full listing is never a candidate.
        let candidates = lens.get(..total).unwrap_or(&lens);
        kept = candidates.len().saturating_sub(1);
        let mut length = candidates.iter().sum::<usize>() + candidates.len().saturating_sub(1);
        while kept > 1 && legend_len + 1 + js::len(&note_for(kept)) + 1 + length > max {
            length -= candidates.get(kept).copied().unwrap_or(0) + 1;
            kept -= 1;
        }
    }
    let body = lines.get(..=kept).unwrap_or(&lines).join("\n");
    let omitted = total - kept;
    let note = if omitted > 0 {
        note_for(kept)
    } else {
        legend.clone()
    };
    let head = if omitted > 0 {
        format!("{legend}\n{note}")
    } else {
        legend
    };
    let (text, clamped, clamp_note) = clamp_text(format!("{head}\n{body}"), max);
    Some(Reduced {
        text,
        reduced: true,
        note: if clamped {
            format!("{note}\n{clamp_note}")
        } else {
            note
        },
    })
}

/// `tabularLines`: the header, then one tab-separated line per record, rendered once.
fn tabular_lines(rows: &[&[(String, JValue)]], keys: &[&str]) -> Vec<String> {
    let column: HashMap<&str, usize> = keys.iter().enumerate().map(|(i, k)| (*k, i)).collect();
    let mut lines = Vec::with_capacity(rows.len() + 1);
    lines.push(keys.join("\t"));
    let mut own: Vec<Option<&JValue>> = vec![None; keys.len()];
    for row in rows {
        own.iter_mut().for_each(|slot| *slot = None);
        for (k, v) in row.iter() {
            if let Some(slot) = column.get(k.as_str()).and_then(|&i| own.get_mut(i)) {
                *slot = Some(v);
            }
        }
        let cells: Vec<String> = keys.iter().zip(&own).map(|(k, v)| cell(*v, k)).collect();
        lines.push(cells.join("\t"));
    }
    lines
}

/// `compactValue`: empty fields dropped, long strings shortened, all the way down.
fn compact_value(v: &JValue) -> JValue {
    match v {
        JValue::Str(s) => JValue::Str(truncate_string(s.clone())),
        JValue::Arr(items) => JValue::Arr(items.iter().map(compact_value).collect()),
        JValue::Obj(items) => JValue::Obj(
            items
                .iter()
                .filter(|(k, val)| !is_empty(val) && k != PROTO)
                .map(|(k, val)| (k.clone(), compact_value(val)))
                .collect(),
        ),
        other => other.clone(),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    #[test]
    fn under_the_cap_passes_through_and_zero_is_the_default() {
        let r = reduce_tool_result("QA short", 8);
        assert_eq!(
            (r.text.as_str(), r.reduced, r.note.as_str()),
            ("QA short", false, "")
        );
        let long = "q".repeat(8000);
        assert!(!reduce_tool_result(&long, 0).reduced);
        assert!(reduce_tool_result(&format!("{long}q"), 0).reduced);
    }

    #[test]
    fn a_raw_lone_surrogate_in_a_string_is_one_unit_of_json() {
        let lone = wtf16::lone_char(0xd83d).unwrap();
        let text = format!("{{\"qa\":\"{lone}\",\"pad\":\"{}\"}}", " ".repeat(40));
        let r = reduce_tool_result(&text, 30);
        assert!(r.note.contains("after compaction"), "{r:?}");
        // Outside a string it is not JSON, nor after a backslash.
        let bad = format!("[{lone}{}]", " ".repeat(40));
        assert!(reduce_tool_result(&bad, 30)
            .note
            .starts_with("[truncated: showing the first 30 of"));
        let escaped = format!("[\"\\{lone}\"{}]", " ".repeat(40));
        assert!(reduce_tool_result(&escaped, 30)
            .note
            .starts_with("[truncated: showing the first 30 of"));
    }

    #[test]
    fn a_large_listing_with_many_distinct_keys_is_linear() {
        let mut rows = Vec::new();
        for i in 0..20_000 {
            let mut fields: Vec<String> = (0..20)
                .map(|j| format!("\"qa{}_{j}\":\"\"", i % 500))
                .collect();
            fields.push(format!("\"qaName\":\"QA record {i}\""));
            rows.push(format!("{{{}}}", fields.join(",")));
        }
        let text = format!("[{}]", rows.join(","));
        let started = std::time::Instant::now();
        let r = reduce_tool_result(&text, 8000);
        assert!(js::len(&r.text) <= 8000);
        assert!(r.note.starts_with("[showing "), "{}", r.note);
        assert!(started.elapsed().as_secs() < 20, "{:?}", started.elapsed());
    }
}
