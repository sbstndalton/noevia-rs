//! The CorpusStore read paths behind /api/months and /api/day (corpus_store.py), over a managed
//! tenant. Read failures degrade exactly as Python's do (to empty or partial text).

use crate::managed::{Managed, Settings};
use crate::py;
use chrono::{Datelike, NaiveDate};
use serde::Serialize;
use serde_json::Value;

pub const MONTHS: [&str; 12] = [
    "January",
    "February",
    "March",
    "April",
    "May",
    "June",
    "July",
    "August",
    "September",
    "October",
    "November",
    "December",
];
const DAYS: [&str; 7] = [
    "Monday",
    "Tuesday",
    "Wednesday",
    "Thursday",
    "Friday",
    "Saturday",
    "Sunday",
];

/// The service's base corpus settings (Python sends `_base_cfg.get("corpus.<key>")` raw).
#[derive(Debug, Clone, Default, serde::Deserialize)]
pub struct BaseCorpus {
    #[serde(default)]
    pub monthly_prefix: Value,
    #[serde(default)]
    pub entry_layout: Value,
    #[serde(default)]
    pub entries_prefix: Value,
    #[serde(default)]
    pub index_file: Value,
    #[serde(default)]
    pub month_file_template: Value,
    #[serde(default)]
    pub index_enabled: Value,
}

#[derive(Debug, Clone)]
pub struct Layout {
    pub daily: bool,
    pub monthly_prefix: String,
    pub entries_prefix: String,
    pub index_file: String,
    pub month_file_template: String,
}

/// Python truthiness of a JSON value.
fn truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64() != Some(0.0),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
    }
}

/// Python `x or default` for a config value that must be a str (or falsy). None when the value
/// is a truthy non-string (Python would fail with an AttributeError: leave that to Python).
fn str_or(value: &Value, default: &str) -> Option<String> {
    match value {
        Value::Null => Some(default.to_string()),
        Value::Bool(false) => Some(default.to_string()),
        Value::String(s) if s.is_empty() => Some(default.to_string()),
        Value::String(s) => Some(s.clone()),
        Value::Number(n) if n.as_f64() == Some(0.0) => Some(default.to_string()),
        Value::Array(a) if a.is_empty() => Some(default.to_string()),
        Value::Object(o) if o.is_empty() => Some(default.to_string()),
        _ => None,
    }
}

/// The managed tenant's effective layout: base settings overlaid with the tenant's stored ones,
/// then CorpusStore's defaulting. None for anything this side does not reproduce (an enabled
/// INDEX.md, an invalid layout, non-string values): Python serves those tenants.
pub fn layout(base: &BaseCorpus, tenant: &Settings) -> Option<Layout> {
    let pick = |key: &str, base_value: &Value| -> Value {
        tenant
            .get(key)
            .cloned()
            .unwrap_or_else(|| base_value.clone())
    };
    // Python: bool(cfg.get("corpus.index_enabled", True)); the base value arrives already as
    // that bool, a tenant's stored value replaces it (Python truthiness).
    let enabled = match tenant.get("index_enabled") {
        Some(v) => truthy(v),
        None => truthy(&base.index_enabled),
    };
    if enabled {
        return None;
    }
    let raw_layout = pick("entry_layout", &base.entry_layout);
    let layout_text = match &raw_layout {
        Value::Null => "monthly".to_string(),
        Value::String(s) => s.clone(),
        _ => return None,
    };
    let daily = match py::strip(&layout_text).to_lowercase().as_str() {
        "daily" => true,
        "monthly" => false,
        _ => return None,
    };
    let template = str_or(
        &pick("month_file_template", &base.month_file_template),
        "{year}-{month02}.md",
    )?
    .replace("{month:02d}", "{month02}");
    let l = Layout {
        daily,
        monthly_prefix: str_or(&pick("monthly_prefix", &base.monthly_prefix), "")?,
        entries_prefix: str_or(&pick("entries_prefix", &base.entries_prefix), "Entries")?,
        index_file: str_or(&pick("index_file", &base.index_file), "INDEX.md")?,
        month_file_template: template,
    };
    template_supported(&l).then_some(l)
}

/// CorpusStore._join with remote_root "" (managed tenants).
fn join(parts: &[&str]) -> String {
    parts
        .iter()
        .filter(|p| !p.is_empty())
        .map(|p| p.trim_matches('/'))
        .collect::<Vec<_>>()
        .join("/")
}

fn month_name(month: u32) -> &'static str {
    MONTHS
        .get(month.saturating_sub(1) as usize)
        .copied()
        .unwrap_or("")
}

#[derive(Debug, Serialize)]
pub struct MonthItem {
    pub id: String,
    pub label: String,
    pub file: String,
}

/// Python `$` without MULTILINE: end of string or just before a final "\n".
const END: &str = r"\n?\z";

pub enum ListError {
    /// The month template cannot compile in Python (duplicate group): an unhandled 500.
    Template,
}

pub fn list_months(store: &Managed, l: &Layout) -> Result<Vec<MonthItem>, ListError> {
    if l.daily {
        return Ok(list_daily_months(store, l));
    }
    // Build the regex as Python does: escape, then replace the escaped fields.
    let fields: [(&str, &str); 4] = [
        ("{year}", "(?P<year>[0-9]{4})"),
        ("{month02}", "(?P<month02>[0-9]{2})"),
        ("{month}", "(?P<month>[0-9]{1,2})"),
        ("{month_name}", ""),
    ];
    let names = MONTHS.map(regex::escape).join("|");
    let month_name_group = format!("(?P<month_name>{names})");
    let mut pattern = String::new();
    let mut rest = l.month_file_template.as_str();
    let mut used: Vec<&str> = Vec::new();
    'outer: while !rest.is_empty() {
        // Python replaces each field in turn over the whole string; with distinct fields this is
        // the same as a left-to-right scan for the earliest field occurrence.
        let mut best: Option<(usize, &str, &str)> = None;
        for (field, group) in fields {
            if let Some(at) = rest.find(field) {
                if best.is_none_or(|(b, _, _)| at < b) {
                    best = Some((at, field, group));
                }
            }
        }
        match best {
            None => {
                pattern.push_str(&regex::escape(rest));
                break 'outer;
            }
            Some((at, field, group)) => {
                pattern.push_str(&regex::escape(rest.get(..at).unwrap_or("")));
                if used.contains(&field) {
                    return Err(ListError::Template);
                }
                used.push(field);
                pattern.push_str(if field == "{month_name}" {
                    &month_name_group
                } else {
                    group
                });
                rest = rest.get(at + field.len()..).unwrap_or("");
            }
        }
    }
    let Ok(rx) = regex::Regex::new(&format!("^{pattern}{END}")) else {
        return Err(ListError::Template);
    };
    let Ok(entries) = store.list_dir(&join(&[&l.monthly_prefix])) else {
        return Ok(Vec::new());
    };
    let mut months: Vec<MonthItem> = Vec::new();
    let mut seen: Vec<(u32, u32)> = Vec::new();
    for entry in entries {
        if entry.is_dir || entry.name == l.index_file {
            continue;
        }
        let Some(m) = rx.captures(&entry.name) else {
            continue;
        };
        let Some(year) = m.name("year").and_then(|y| y.as_str().parse::<u32>().ok()) else {
            // Python: int(gd["year"]) raises KeyError without {year} -> the entry is skipped
            // (caught as TypeError/ValueError? KeyError is not caught; see Template).
            return Err(ListError::Template);
        };
        let month = if let Some(name) = m.name("month_name").filter(|g| !g.as_str().is_empty()) {
            match MONTHS
                .iter()
                .position(|mn| mn.to_lowercase() == name.as_str().to_lowercase())
            {
                Some(i) => i as u32 + 1,
                None => continue,
            }
        } else if let Some(g) = m.name("month02").filter(|g| !g.as_str().is_empty()) {
            g.as_str().parse().unwrap_or(0)
        } else if let Some(g) = m.name("month").filter(|g| !g.as_str().is_empty()) {
            g.as_str().parse().unwrap_or(0)
        } else {
            1
        };
        if !(1..=12).contains(&month) || !(2000..=2100).contains(&year) {
            continue;
        }
        if seen.contains(&(year, month)) {
            continue;
        }
        seen.push((year, month));
        months.push(MonthItem {
            id: format!("{year:04}-{month:02}"),
            label: format!("{} {year}", month_name(month)),
            file: entry.name.clone(),
        });
    }
    months.sort_by(|a, b| a.id.cmp(&b.id));
    Ok(months)
}

/// `^<Month> \d{1,2}, <year>\.md$` (Unicode `\d`); Some(day number) on a match.
fn daily_name(name: &str, month: &str, year: u32) -> Option<u32> {
    let body = name.strip_suffix('\n').unwrap_or(name);
    let rest = body.strip_prefix(month)?.strip_prefix(' ')?;
    let rest = rest.strip_suffix(&format!(", {year}.md"))?;
    py::digits(rest, 1, 2)
}

fn list_daily_months(store: &Managed, l: &Layout) -> Vec<MonthItem> {
    let run = || -> Result<Vec<MonthItem>, crate::managed::Error> {
        let mut months = Vec::new();
        for year_entry in store.list_dir(&join(&[&l.entries_prefix]))? {
            if !year_entry.is_dir {
                continue;
            }
            let Some(year) = py::digits(&year_entry.name, 4, 4) else {
                continue;
            };
            if !(2000..=2100).contains(&year) {
                continue;
            }
            for month_entry in store.list_dir(&year_entry.path)? {
                let Some(index) = MONTHS.iter().position(|m| *m == month_entry.name) else {
                    // calendar.month_name also contains "" (index 0): an empty name is not listed.
                    continue;
                };
                if !month_entry.is_dir {
                    continue;
                }
                let items = store.list_dir(&month_entry.path)?;
                if !items
                    .iter()
                    .any(|i| !i.is_dir && daily_name(&i.name, &month_entry.name, year).is_some())
                {
                    continue;
                }
                let month = index as u32 + 1;
                months.push(MonthItem {
                    id: format!("{year:04}-{month:02}"),
                    label: format!("{} {year}", month_name(month)),
                    file: month_entry.path.clone(),
                });
            }
        }
        Ok(months)
    };
    match run() {
        Ok(mut months) => {
            months.sort_by(|a, b| a.id.cmp(&b.id));
            months
        }
        Err(_) => Vec::new(),
    }
}

fn days_in_month(year: i32, month: u32) -> u32 {
    let next = if month == 12 {
        NaiveDate::from_ymd_opt(year + 1, 1, 1)
    } else {
        NaiveDate::from_ymd_opt(year, month + 1, 1)
    };
    next.and_then(|n| n.pred_opt())
        .map(|d| d.day())
        .unwrap_or(28)
}

fn daily_path(l: &Layout, day: NaiveDate) -> String {
    let month = month_name(day.month());
    join(&[
        &l.entries_prefix,
        &day.year().to_string(),
        month,
        &format!("{month} {}, {}.md", day.day(), day.year()),
    ])
}

fn month_filename(l: &Layout, day: NaiveDate) -> String {
    // str.format with the four fields; the template has no other fields (Python would raise).
    l.month_file_template
        .replace("{year}", &day.year().to_string())
        .replace("{month02}", &format!("{:02}", day.month()))
        .replace("{month}", &day.month().to_string())
        .replace("{month_name}", month_name(day.month()))
}

fn document_path(l: &Layout, day: NaiveDate) -> String {
    if l.daily {
        daily_path(l, day)
    } else {
        join(&[&l.monthly_prefix, &month_filename(l, day)])
    }
}

/// Can the month template be formatted the way Python's str.format would (only the four named
/// fields, no other braces)? Anything else is left to Python.
pub fn template_supported(l: &Layout) -> bool {
    let mut rest = l.month_file_template.clone();
    for f in ["{year}", "{month02}", "{month}", "{month_name}"] {
        rest = rest.replace(f, "");
    }
    !rest.contains('{') && !rest.contains('}')
}

/// read_month_text(year, month, include_xids=True).
pub fn read_month_text(store: &Managed, l: &Layout, year: i32, month: u32) -> String {
    let Some(first) = NaiveDate::from_ymd_opt(year, month, 1) else {
        return String::new();
    };
    if !l.daily {
        return store
            .get_text(&document_path(l, first))
            .ok()
            .flatten()
            .unwrap_or_default();
    }
    let mut parts: Vec<String> = Vec::new();
    let month_dir = join(&[&l.entries_prefix, &year.to_string(), month_name(month)]);
    let Ok(entries) = store.list_dir(&month_dir) else {
        return String::new();
    };
    let last = days_in_month(year, month);
    let mut dated: Vec<(u32, String)> = Vec::new();
    for entry in entries {
        let Some(day) = daily_name(&entry.name, month_name(month), year as u32) else {
            continue;
        };
        if entry.is_dir {
            continue;
        }
        if (1..=last).contains(&day) {
            dated.push((day, entry.path));
        }
    }
    dated.sort();
    for (_, path) in dated {
        match store.get_text(&path) {
            Ok(Some(text)) if !text.is_empty() => parts.push(py::strip(&text).to_string()),
            Ok(_) => {}
            Err(_) => break,
        }
    }
    parts
        .into_iter()
        .filter(|p| !p.is_empty())
        .collect::<Vec<_>>()
        .join("\n\n")
}

pub fn canonical_day_header(d: NaiveDate) -> String {
    let dow = DAYS
        .get(d.weekday().num_days_from_monday() as usize)
        .copied()
        .unwrap_or("");
    format!(
        "## {dow}, {} {}, {}",
        month_name(d.month()),
        d.day(),
        d.year()
    )
}

/// get_day_text(day, max_chars=12000, include_markers=True).
pub fn day_text(store: &Managed, l: &Layout, day: NaiveDate, max_chars: usize) -> String {
    let Ok(text) = store.get_text(&document_path(l, day)) else {
        return String::new();
    };
    let Some(text) = text.filter(|t| !t.is_empty()) else {
        return String::new();
    };
    let header = canonical_day_header(day);
    for d in diary_corpus_parse::parse_diary(&text) {
        if d.header != header {
            continue;
        }
        let mut parts: Vec<String> = vec![d.header.clone(), String::new()];
        for sub in &d.subsections {
            parts.push(format!("### {}", sub.header));
            parts.push(String::new());
            for ex in &sub.exchanges {
                if !ex.me.is_empty() {
                    parts.push(format!("**Me:** {}", ex.me));
                    parts.push(String::new());
                }
                if !ex.claude.is_empty() {
                    parts.push(format!("**Assistant:** {}", ex.claude));
                    parts.push(String::new());
                }
                if let Some(xid) = ex.xid.as_deref().filter(|x| !x.is_empty()) {
                    parts.push(format!("<!-- xid:{xid} -->"));
                    parts.push(String::new());
                }
            }
        }
        let joined = parts.join("\n");
        let text = py::strip(&joined);
        if max_chars > 0 && text.chars().count() > max_chars {
            return format!(
                "(earlier exchanges today truncated)\n…{}",
                py::tail_chars(text, max_chars)
            );
        }
        return text.to_string();
    }
    String::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_is_python_strftime() {
        let d = NaiveDate::from_ymd_opt(2030, 1, 6).unwrap_or_default();
        assert_eq!(canonical_day_header(d), "## Sunday, January 6, 2030");
    }

    #[test]
    fn daily_names_take_unicode_digits() {
        assert_eq!(daily_name("January 6, 2030.md", "January", 2030), Some(6));
        assert_eq!(
            daily_name("January \u{666}, 2030.md", "January", 2030),
            Some(6)
        );
        assert_eq!(daily_name("January 6, 2030.md\n", "January", 2030), Some(6));
        assert_eq!(daily_name("January 123, 2030.md", "January", 2030), None);
    }

    #[test]
    fn layout_defaults_and_refusals() {
        let base = BaseCorpus {
            entry_layout: Value::String("daily".into()),
            index_enabled: Value::Bool(false),
            ..BaseCorpus::default()
        };
        let l = layout(&base, &Settings::new());
        assert!(l
            .as_ref()
            .is_some_and(|l| l.daily && l.entries_prefix == "Entries"));
        let mut s = Settings::new();
        s.insert("index_enabled".into(), Value::Bool(true));
        assert!(layout(&base, &s).is_none());
        let mut s = Settings::new();
        s.insert("entry_layout".into(), Value::String("weekly".into()));
        assert!(layout(&base, &s).is_none());
    }
}
