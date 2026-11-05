//! What a tool result becomes before it goes back to the model (core tool-result-reduce.cjs):
//! under the budget, exactly what the tool said; a uniform JSON listing as one header row and
//! tab-separated records; other JSON compacted; anything else truncated. Every reduction says what
//! it did. Lengths and cuts are UTF-16 units.

use js_json::{wtf16, JValue};

/// tool-result-reduce.cjs `DEFAULT_MAX_CHARS`.
pub const DEFAULT_MAX_CHARS: usize = 8000;
/// tool-result-reduce.cjs `DEFAULT_MAX_VALUE_CHARS`.
pub const DEFAULT_MAX_VALUE_CHARS: usize = 300;
const MIN_ROWS: usize = 2;
const MAX_LEGEND_KEYS: usize = 50;
const HEADER_SHARE: f64 = 0.25;

fn len(s: &str) -> usize {
    wtf16::len(s)
}

fn slice(s: &str, n: usize) -> String {
    wtf16::slice(s, n)
}

fn list_keys(keys: &[String], budget: usize) -> String {
    let mut shown: Vec<String> = Vec::new();
    let mut length = 0;
    for key in keys.iter().take(MAX_LEGEND_KEYS) {
        let k = if len(key) > 80 { format!("{}\u{2026}", slice(key, 80)) } else { key.clone() };
        if !shown.is_empty() && length + len(&k) + 2 > budget {
            break;
        }
        length += len(&k) + 2;
        shown.push(k);
    }
    if keys.len() > shown.len() {
        format!("{}, \u{2026} {} more", shown.join(", "), keys.len() - shown.len())
    } else {
        shown.join(", ")
    }
}

struct Clamped {
    text: String,
    clamped: bool,
    note: String,
}

fn clamp_text(text: &str, max: usize) -> Clamped {
    if len(text) <= max {
        return Clamped { text: text.to_owned(), clamped: false, note: String::new() };
    }
    let mut note = format!(
        "[truncated: showing the first part of {} characters to fit. Ask for a narrower query if you need the rest.]",
        len(text)
    );
    if len(&note) + 1 >= max {
        note = slice(&note, max.saturating_sub(1));
    }
    let room = max.saturating_sub(len(&note) + 1);
    Clamped { text: slice(&format!("{}\n{note}", slice(text, room)), max), clamped: true, note }
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

fn truncate_value(v: &JValue, max: usize) -> String {
    let s = match v {
        JValue::Str(s) => s.clone(),
        other => wtf16::stringify(other).unwrap_or_else(|| "undefined".into()),
    };
    if len(&s) <= max {
        return s;
    }
    format!("{}\u{2026}[+{} chars]", slice(&s, max), len(&s) - max)
}

fn keys_of(v: &JValue) -> Vec<String> {
    match v {
        JValue::Obj(o) => o.iter().map(|(k, _)| k.clone()).collect(),
        _ => Vec::new(),
    }
}

fn useful_keys(rows: &[JValue]) -> Vec<String> {
    let mut keys: Vec<String> = Vec::new();
    for row in rows {
        for k in keys_of(row) {
            if !keys.contains(&k) {
                keys.push(k);
            }
        }
    }
    keys.into_iter().filter(|k| rows.iter().any(|r| !is_empty_value(r.get(k)))).collect()
}

fn flat_tabs(s: &str) -> String {
    let mut out = String::new();
    let mut run = false;
    for c in s.chars() {
        if matches!(c, '\t' | '\r' | '\n') {
            if !run {
                out.push(' ');
                run = true;
            }
        } else {
            run = false;
            out.push(c);
        }
    }
    out
}

fn tabular_lines(rows: &[JValue], keys: &[String], max_value: usize) -> Vec<String> {
    let mut lines = vec![keys.join("\t")];
    for row in rows {
        lines.push(
            keys.iter()
                .map(|k| match row.get(k) {
                    JValue::Undefined => String::new(),
                    v => flat_tabs(&truncate_value(v, max_value)),
                })
                .collect::<Vec<_>>()
                .join("\t"),
        );
    }
    lines
}

fn fit_lines(lines: &[String], fits: impl Fn(usize, usize) -> bool) -> (usize, String) {
    let mut kept = lines.len().saturating_sub(1);
    let mut length = lines.iter().map(|l| len(l)).sum::<usize>() + lines.len().saturating_sub(1);
    while kept > 1 && !fits(kept, length) {
        length -= lines.get(kept).map_or(0, |l| len(l)) + 1;
        kept -= 1;
    }
    (kept, lines.get(..=kept).unwrap_or(lines).join("\n"))
}

fn compact_value(v: &JValue, max_value: usize) -> JValue {
    match v {
        JValue::Str(_) => JValue::Str(truncate_value(v, max_value)),
        JValue::Arr(a) => JValue::Arr(a.iter().map(|x| compact_value(x, max_value)).collect()),
        JValue::Obj(o) => JValue::Obj(
            o.iter().filter(|(_, x)| !is_empty_value(x)).map(|(k, x)| (k.clone(), compact_value(x, max_value))).collect(),
        ),
        other => other.clone(),
    }
}

/// tool-result-reduce.cjs `reduceToolResult(result, {maxChars, maxValueChars})`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reduced {
    /// What goes to the model, already carrying `note` when reduced.
    pub text: String,
    pub reduced: bool,
    /// Empty when the result passed through untouched.
    pub note: String,
}

/// `reduceToolResult(text, {maxChars, maxValueChars})`; a 0 option is the default (`||`).
pub fn reduce_tool_result(text: &str, max_chars: usize, max_value_chars: usize) -> Reduced {
    let max = if max_chars == 0 { DEFAULT_MAX_CHARS } else { max_chars };
    let max_value = if max_value_chars == 0 { DEFAULT_MAX_VALUE_CHARS } else { max_value_chars };
    if len(text) <= max {
        return Reduced { text: text.to_owned(), reduced: false, note: String::new() };
    }
    let Ok(parsed) = wtf16::parse(text) else {
        let note = format!(
            "\n\n[truncated: showing the first {max} of {} characters. Ask for a narrower query if you need the rest.]",
            len(text)
        );
        return Reduced { text: format!("{}{note}", slice(text, max)), reduced: true, note: note.trim().to_owned() };
    };
    if let JValue::Arr(rows) = &parsed {
        if rows.len() >= MIN_ROWS && rows.iter().all(is_plain_object) {
            let all = useful_keys(rows);
            if !all.is_empty() {
                let mut dropped: Vec<String> = Vec::new();
                for r in rows {
                    for k in keys_of(r) {
                        if !dropped.contains(&k) {
                            dropped.push(k);
                        }
                    }
                }
                dropped.retain(|k| !all.contains(k));
                let header_budget = ((max as f64 * HEADER_SHARE).floor() as usize).max(1);
                let mut keys: Vec<String> = Vec::new();
                let mut header = 0;
                for k in &all {
                    let cost = len(k) + usize::from(!keys.is_empty());
                    if !keys.is_empty() && header + cost > header_budget {
                        break;
                    }
                    keys.push(k.clone());
                    header += cost;
                }
                let cut: Vec<String> = all.get(keys.len()..).unwrap_or(&[]).to_vec();
                let legend_budget = ((max as f64 * 0.1).floor() as usize).max(80);
                let mut legend = format!("[{} records, tab-separated, first line is the column names", rows.len());
                if !dropped.is_empty() {
                    legend.push_str(&format!(
                        "; {} column(s) omitted because they were empty in every record: {}",
                        dropped.len(),
                        list_keys(&dropped, legend_budget)
                    ));
                }
                if !cut.is_empty() {
                    legend.push_str(&format!("; {} further column(s) not shown to fit: {}", cut.len(), list_keys(&cut, legend_budget)));
                }
                legend.push(']');
                let lines = tabular_lines(rows, &keys, max_value);
                let mut body = lines.join("\n");
                let mut kept = rows.len();
                let total = rows.len();
                let note_for = |n: usize| {
                    format!("[showing {n} of {total} records; {} omitted to fit. Narrow the query if you need them.]", total - n)
                };
                let legend_len = len(&legend);
                if legend_len + 1 + len(&body) > max {
                    let fits = |n: usize, length: usize| legend_len + 1 + len(&note_for(n)) + 1 + length <= max;
                    (kept, body) = fit_lines(lines.get(..rows.len()).unwrap_or(&lines), fits);
                }
                let omitted = total - kept;
                let note = if omitted > 0 { note_for(kept) } else { legend.clone() };
                let head = if omitted > 0 { format!("{legend}\n{note}") } else { legend.clone() };
                let c = clamp_text(&format!("{head}\n{body}"), max);
                let note = if c.clamped { format!("{note}\n{}", c.note) } else { note };
                return Reduced { text: c.text, reduced: true, note };
            }
        }
    }
    let compacted = wtf16::stringify(&compact_value(&parsed, max_value)).unwrap_or_default();
    let compact_note = format!(
        "[compacted: empty fields removed and long values shortened; {} \u{2192} {} characters.]",
        len(text),
        len(&compacted)
    );
    if len(&compact_note) + 1 + len(&compacted) <= max {
        return Reduced { text: format!("{compact_note}\n{compacted}"), reduced: true, note: compact_note };
    }
    let note = format!(
        "\n\n[truncated: showing the first {max} of {} characters after compaction. Ask for a narrower query if you need the rest.]",
        len(&compacted)
    );
    Reduced { text: format!("{}{note}", slice(&compacted, max)), reduced: true, note: note.trim().to_owned() }
}

