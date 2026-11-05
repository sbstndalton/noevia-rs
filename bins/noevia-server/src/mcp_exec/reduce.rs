//! core `tool-result-reduce.cjs` `reduceToolResult`: what a tool returned, compacted before it goes
//! back to the model. Lengths and cuts are in UTF-16 code units, as JavaScript's `String` measures
//! them; a cut through a surrogate pair drops the stray half (a Rust `String` cannot hold it).
use js_json::{parse, stringify, JValue};

const DEFAULT_MAX_VALUE_CHARS: usize = 300;
const MIN_ROWS: usize = 2;
const MAX_LEGEND_KEYS: usize = 50;

/// `{ text, reduced, note }`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reduced {
    pub text: String,
    pub reduced: bool,
    pub note: String,
}

fn len16(s: &str) -> usize {
    s.encode_utf16().count()
}

/// `s.slice(start, end)` by UTF-16 code units (a split pair loses the half that is cut).
fn slice16(s: &str, start: usize, end: usize) -> String {
    let mut out = String::new();
    let mut at = 0usize;
    for ch in s.chars() {
        let w = ch.len_utf16();
        if at >= start && at + w <= end {
            out.push(ch);
        }
        at += w;
        if at >= end {
            break;
        }
    }
    out
}

fn head16(s: &str, n: usize) -> String {
    slice16(s, 0, n)
}

fn list_keys(keys: &[String], char_budget: usize) -> String {
    let mut shown: Vec<String> = Vec::new();
    let mut length = 0usize;
    for key in keys.iter().take(MAX_LEGEND_KEYS) {
        let k = if len16(key) > 80 {
            format!("{}…", head16(key, 80))
        } else {
            key.clone()
        };
        if !shown.is_empty() && length + len16(&k) + 2 > char_budget {
            break;
        }
        length += len16(&k) + 2;
        shown.push(k);
    }
    if keys.len() > shown.len() {
        format!("{}, … {} more", shown.join(", "), keys.len() - shown.len())
    } else {
        shown.join(", ")
    }
}

struct Clamped {
    text: String,
    clamped: bool,
    note: String,
}

fn clamp_text(text: &str, max_chars: usize) -> Clamped {
    if len16(text) <= max_chars {
        return Clamped {
            text: text.to_owned(),
            clamped: false,
            note: String::new(),
        };
    }
    let mut note = format!(
        "[truncated: showing the first part of {} characters to fit. Ask for a narrower query if you need the rest.]",
        len16(text)
    );
    if len16(&note) + 1 >= max_chars {
        note = head16(&note, max_chars.saturating_sub(1));
    }
    let room = max_chars.saturating_sub(len16(&note) + 1);
    let joined = format!("{}\n{note}", head16(text, room));
    Clamped {
        text: head16(&joined, max_chars),
        clamped: true,
        note,
    }
}

fn is_plain_object(v: &JValue) -> bool {
    matches!(v, JValue::Obj(_))
}

fn is_empty_value(v: &JValue) -> bool {
    match v {
        JValue::Null | JValue::Undefined => true,
        JValue::Str(s) => s.is_empty(),
        JValue::Arr(a) => a.is_empty(),
        JValue::Obj(o) => o.is_empty(),
        _ => false,
    }
}

fn truncate_value(v: &JValue, max_value_chars: usize) -> String {
    let s = match v {
        JValue::Str(s) => s.clone(),
        other => stringify(other).unwrap_or_else(|| "undefined".to_owned()),
    };
    let n = len16(&s);
    if n <= max_value_chars {
        return s;
    }
    format!(
        "{}…[+{} chars]",
        head16(&s, max_value_chars),
        n - max_value_chars
    )
}

fn keys_of(row: &JValue) -> Vec<&String> {
    match row {
        JValue::Obj(items) => items.iter().map(|(k, _)| k).collect(),
        _ => Vec::new(),
    }
}

fn useful_keys(rows: &[JValue]) -> Vec<String> {
    let mut keys: Vec<String> = Vec::new();
    for row in rows {
        for k in keys_of(row) {
            if !keys.iter().any(|x| x == k) {
                keys.push(k.clone());
            }
        }
    }
    keys.into_iter()
        .filter(|k| rows.iter().any(|row| !is_empty_value(row.get(k))))
        .collect()
}

fn tabular_lines(rows: &[JValue], keys: &[String], max_value_chars: usize) -> Vec<String> {
    let mut lines = vec![keys.join("\t")];
    for row in rows {
        let cells: Vec<String> = keys
            .iter()
            .map(|k| {
                let v = row.get(k);
                if matches!(v, JValue::Undefined) {
                    return String::new();
                }
                collapse_breaks(&truncate_value(v, max_value_chars))
            })
            .collect();
        lines.push(cells.join("\t"));
    }
    lines
}

/// `.replace(/[\t\r\n]+/g, ' ')`.
fn collapse_breaks(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut in_run = false;
    for c in s.chars() {
        if matches!(c, '\t' | '\r' | '\n') {
            if !in_run {
                out.push(' ');
                in_run = true;
            }
        } else {
            in_run = false;
            out.push(c);
        }
    }
    out
}

fn fit_lines(lines: &[String], fits: impl Fn(usize, usize) -> bool) -> (usize, String) {
    let mut kept = lines.len() - 1;
    let mut length: usize = lines.iter().map(|l| len16(l)).sum::<usize>() + lines.len() - 1;
    while kept > 1 && !fits(kept, length) {
        length -= lines.get(kept).map_or(0, |l| len16(l)) + 1;
        kept -= 1;
    }
    (kept, lines.get(..=kept).unwrap_or(lines).join("\n"))
}

fn compact_value(v: &JValue, max_value_chars: usize) -> JValue {
    match v {
        JValue::Str(_) => JValue::Str(truncate_value(v, max_value_chars)),
        JValue::Arr(items) => JValue::Arr(
            items
                .iter()
                .map(|x| compact_value(x, max_value_chars))
                .collect(),
        ),
        JValue::Obj(items) => JValue::Obj(
            items
                .iter()
                .filter(|(_, val)| !is_empty_value(val))
                .map(|(k, val)| (k.clone(), compact_value(val, max_value_chars)))
                .collect(),
        ),
        other => other.clone(),
    }
}

/// `reduceToolResult(text, { maxChars })`.
pub fn reduce_tool_result(text: &str, max_chars: usize) -> Reduced {
    let max_value_chars = DEFAULT_MAX_VALUE_CHARS;
    let total = len16(text);
    if total <= max_chars {
        return Reduced {
            text: text.to_owned(),
            reduced: false,
            note: String::new(),
        };
    }
    let Ok(parsed) = parse(text) else {
        let note = format!(
            "\n\n[truncated: showing the first {max_chars} of {total} characters. Ask for a narrower query if you need the rest.]"
        );
        return Reduced {
            text: format!("{}{note}", head16(text, max_chars)),
            reduced: true,
            note: js_trim(&note),
        };
    };
    if let JValue::Arr(rows) = &parsed {
        if rows.len() >= MIN_ROWS && rows.iter().all(is_plain_object) {
            let all_keys = useful_keys(rows);
            if !all_keys.is_empty() {
                return tabular(rows, &all_keys, max_chars, max_value_chars);
            }
        }
    }
    let compacted = stringify(&compact_value(&parsed, max_value_chars)).unwrap_or_default();
    let compact_note = format!(
        "[compacted: empty fields removed and long values shortened; {total} → {} characters.]",
        len16(&compacted)
    );
    if len16(&compact_note) + 1 + len16(&compacted) <= max_chars {
        return Reduced {
            text: format!("{compact_note}\n{compacted}"),
            reduced: true,
            note: compact_note,
        };
    }
    let note = format!(
        "\n\n[truncated: showing the first {max_chars} of {} characters after compaction. Ask for a narrower query if you need the rest.]",
        len16(&compacted)
    );
    Reduced {
        text: format!("{}{note}", head16(&compacted, max_chars)),
        reduced: true,
        note: js_trim(&note),
    }
}

fn js_trim(s: &str) -> String {
    s.trim_matches(crate::mcp_broker::js_ws).to_owned()
}

fn tabular(
    rows: &[JValue],
    all_keys: &[String],
    max_chars: usize,
    max_value_chars: usize,
) -> Reduced {
    let mut dropped: Vec<String> = Vec::new();
    for r in rows {
        for k in keys_of(r) {
            if !all_keys.contains(k) && !dropped.contains(k) {
                dropped.push(k.clone());
            }
        }
    }
    let header_budget = std::cmp::max(1, max_chars / 4);
    let mut keys: Vec<String> = Vec::new();
    let mut header_length = 0usize;
    for k in all_keys {
        let cost = len16(k) + usize::from(!keys.is_empty());
        if !keys.is_empty() && header_length + cost > header_budget {
            break;
        }
        header_length += cost;
        keys.push(k.clone());
    }
    let cut_keys: Vec<String> = all_keys.iter().skip(keys.len()).cloned().collect();
    let legend_budget = std::cmp::max(80, max_chars / 10);
    let mut legend = format!(
        "[{} records, tab-separated, first line is the column names",
        rows.len()
    );
    if !dropped.is_empty() {
        legend.push_str(&format!(
            "; {} column(s) omitted because they were empty in every record: {}",
            dropped.len(),
            list_keys(&dropped, legend_budget)
        ));
    }
    if !cut_keys.is_empty() {
        legend.push_str(&format!(
            "; {} further column(s) not shown to fit: {}",
            cut_keys.len(),
            list_keys(&cut_keys, legend_budget)
        ));
    }
    legend.push(']');
    let lines = tabular_lines(rows, &keys, max_value_chars);
    let mut body = lines.join("\n");
    let mut kept = rows.len();
    let note_for = |n: usize| {
        format!(
            "[showing {n} of {} records; {} omitted to fit. Narrow the query if you need them.]",
            rows.len(),
            rows.len() - n
        )
    };
    let legend_len = len16(&legend);
    let fits =
        |n: usize, length: usize| legend_len + 1 + len16(&note_for(n)) + 1 + length <= max_chars;
    if legend_len + 1 + len16(&body) > max_chars {
        let (k, b) = fit_lines(lines.get(..rows.len()).unwrap_or(&lines), fits);
        kept = k;
        body = b;
    }
    let omitted = rows.len() - kept;
    let note = if omitted > 0 {
        note_for(kept)
    } else {
        legend.clone()
    };
    let head = if omitted > 0 {
        format!("{legend}\n{note}")
    } else {
        legend.clone()
    };
    let clamped = clamp_text(&format!("{head}\n{body}"), max_chars);
    Reduced {
        text: clamped.text,
        reduced: true,
        note: if clamped.clamped {
            format!("{note}\n{}", clamped.note)
        } else {
            note
        },
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    #[test]
    fn a_result_under_the_cap_is_untouched() {
        let r = reduce_tool_result("hello", 8000);
        assert_eq!(
            (r.text.as_str(), r.reduced, r.note.as_str()),
            ("hello", false, "")
        );
    }

    #[test]
    fn prose_over_the_cap_is_cut_with_a_note() {
        let r = reduce_tool_result(&"x".repeat(50), 10);
        assert!(r.reduced);
        assert!(r
            .text
            .starts_with("xxxxxxxxxx\n\n[truncated: showing the first 10 of 50 characters."));
    }

    #[test]
    fn units_are_utf16_and_a_split_pair_loses_the_half() {
        assert_eq!(slice16("a😀b", 0, 2), "a");
        assert_eq!(slice16("a😀b", 0, 3), "a😀");
        assert_eq!(len16("a😀b"), 4);
    }
}
