//! Pure Diary corpus rendering, marker, edit and month-append transforms.
//!
//! These functions mirror the format-level Python corpus helpers. They do no
//! filesystem, journal, authorization, service, or model work. Callers retain
//! consent, duplicate protection and write durability responsibilities.
use chrono::{Datelike, NaiveDate, NaiveTime, Weekday};
use sha2::{Digest, Sha256};

mod unicode;

fn py_space(c: char) -> bool {
    matches!(c, '\u{0009}'..='\u{000d}' | '\u{001c}'..='\u{0020}' | '\u{0085}' | '\u{00a0}'
        | '\u{1680}' | '\u{2000}'..='\u{200a}' | '\u{2028}' | '\u{2029}' | '\u{202f}' | '\u{205f}' | '\u{3000}')
}

fn py_strip(s: &str) -> &str {
    s.trim_matches(py_space)
}

fn line_break(c: char) -> bool {
    matches!(
        c,
        '\r' | '\n'
            | '\u{000b}'
            | '\u{000c}'
            | '\u{001c}'
            | '\u{001d}'
            | '\u{001e}'
            | '\u{0085}'
            | '\u{2028}'
            | '\u{2029}'
    )
}

fn line_break_runs_to_space(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut in_break = false;
    for c in s.chars() {
        if line_break(c) {
            if !in_break {
                out.push(' ');
            }
            in_break = true;
        } else {
            out.push(c);
            in_break = false;
        }
    }
    out
}

fn py_splitlines_keepends(s: &str) -> Vec<&str> {
    let mut result = Vec::new();
    let mut start = 0;
    let mut iter = s.char_indices().peekable();
    while let Some((index, c)) = iter.next() {
        if line_break(c) {
            let mut end = index + c.len_utf8();
            if c == '\r' && iter.peek().is_some_and(|(_, next)| *next == '\n') {
                iter.next();
                end += 1;
            }
            if let Some(line) = s.get(start..end) {
                result.push(line);
            }
            start = end;
        }
    }
    if let Some(last) = s.get(start..) {
        if !last.is_empty() {
            result.push(last);
        }
    }
    result
}

fn heading_line(body: &str) -> bool {
    let hashes = body.bytes().take_while(|b| *b == b'#').count();
    (1..=6).contains(&hashes)
        && body
            .as_bytes()
            .get(hashes)
            .is_none_or(|b| matches!(b, b' ' | b'\t'))
}

fn role_line(body: &str) -> bool {
    ["**Me:**", "**Assistant:**", "**Claude:**"]
        .iter()
        .any(|prefix| body.starts_with(prefix))
}

fn marker_open_end(s: &str, at: usize) -> Option<usize> {
    let after_open = at.checked_add(4)?;
    if s.get(at..after_open)? != "<!--" {
        return None;
    }
    let tail = s.get(after_open..)?;
    let mut rest = tail;
    while let Some(c) = rest.chars().next() {
        if !py_space(c) {
            break;
        }
        rest = rest.get(c.len_utf8()..)?;
    }
    if rest.starts_with("xid:") {
        Some(after_open)
    } else {
        None
    }
}

fn escape_marker_open(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut offset = 0;
    while let Some(relative) = s.get(offset..).and_then(|tail| tail.find("<!--")) {
        let Some(at) = offset.checked_add(relative) else {
            break;
        };
        if let Some(before) = s.get(offset..at) {
            out.push_str(before);
        }
        if marker_open_end(s, at).is_some() {
            out.push_str("<\\!--");
        } else {
            out.push_str("<!--");
        }
        offset = at + 4;
    }
    if let Some(tail) = s.get(offset..) {
        out.push_str(tail);
    }
    out
}

/// Prefix structural Markdown lines and escape xid comment openers in prose.
/// Matching uses Python 3.12 `str.splitlines(keepends=True)` and `str.strip` rules.
pub fn escape_entry_text(text: &str) -> String {
    let text = escape_marker_open(text);
    let mut out = String::with_capacity(text.len());
    for (index, line) in py_splitlines_keepends(&text).into_iter().enumerate() {
        let body = line.trim_end_matches(line_break);
        let end = line.get(body.len()..).unwrap_or_default();
        if index > 0 && heading_line(body) {
            out.push('\\');
            out.push_str(body);
        } else if role_line(body) {
            out.push_str("\\*\\*");
            out.push_str(body.get(2..).unwrap_or_default());
        } else {
            out.push_str(body);
        }
        out.push_str(end);
    }
    out
}

fn weekday_name(day: Weekday) -> &'static str {
    match day {
        Weekday::Mon => "Monday",
        Weekday::Tue => "Tuesday",
        Weekday::Wed => "Wednesday",
        Weekday::Thu => "Thursday",
        Weekday::Fri => "Friday",
        Weekday::Sat => "Saturday",
        Weekday::Sun => "Sunday",
    }
}

fn month_name(month: u32) -> Option<&'static str> {
    const MONTHS: [&str; 12] = [
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
    MONTHS.get(month.checked_sub(1)? as usize).copied()
}

/// Canonical English header for a Python-representable date (years 1..=9999).
/// `None` rejects `chrono` dates outside Python's `datetime.date` range.
pub fn canonical_day_header(day: NaiveDate) -> Option<String> {
    if !(1..=9999).contains(&day.year()) {
        return None;
    }
    Some(format!(
        "## {}, {} {}, {}",
        weekday_name(day.weekday()),
        month_name(day.month())?,
        day.day(),
        day.year()
    ))
}

/// Render a fixed-time subsection header; the caller supplies `now`.
pub fn render_subsection_header(now: NaiveTime, topic: &str) -> String {
    let topic = line_break_runs_to_space(topic);
    let topic = py_strip(&topic);
    if topic.is_empty() {
        format!("### {}", now.format("%H:%M"))
    } else {
        format!("### {} — {}", now.format("%H:%M"), topic)
    }
}

/// Render one exchange with the canonical hidden marker.
pub fn render_exchange(me: &str, claude: &str, xid: &str) -> String {
    format!(
        "**Me:** {}\n\n**Assistant:** {}\n\n<!-- xid:{} -->\n",
        escape_entry_text(py_strip(me)),
        escape_entry_text(py_strip(claude)),
        xid
    )
}

pub fn render_subsection_block(header: &str, body: &str) -> String {
    format!("### {}\n\n{}\n", py_strip(header), py_strip(body))
}

fn marker_match_end(s: &str, at: usize) -> Option<usize> {
    let after_open = marker_open_end(s, at)?;
    let mut rest = s.get(after_open..)?;
    let mut consumed = after_open;
    while let Some(c) = rest.chars().next() {
        if !py_space(c) {
            break;
        }
        consumed += c.len_utf8();
        rest = rest.get(c.len_utf8()..)?;
    }
    rest = rest.strip_prefix("xid:")?;
    consumed += 4;
    let xid_len = rest
        .bytes()
        .take_while(|b| b.is_ascii_hexdigit() || *b == b'-')
        .count();
    if xid_len == 0 {
        return None;
    }
    consumed += xid_len;
    rest = rest.get(xid_len..)?;
    while let Some(c) = rest.chars().next() {
        if !py_space(c) {
            break;
        }
        consumed += c.len_utf8();
        rest = rest.get(c.len_utf8()..)?;
    }
    if rest.starts_with("-->") {
        Some(consumed + 3)
    } else {
        None
    }
}

/// Remove every Python `MARKER_RE` match, retaining all surrounding bytes.
pub fn strip_markers(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut offset = 0;
    while let Some(relative) = text.get(offset..).and_then(|tail| tail.find("<!--")) {
        let Some(at) = offset.checked_add(relative) else {
            break;
        };
        if let Some(before) = text.get(offset..at) {
            out.push_str(before);
        }
        if let Some(end) = marker_match_end(text, at) {
            offset = end;
        } else {
            out.push_str("<!--");
            offset = at + 4;
        }
    }
    if let Some(tail) = text.get(offset..) {
        out.push_str(tail);
    }
    out
}

pub fn has_marker(text: &str, xid: &str) -> bool {
    text.contains(&format!("<!-- xid:{xid} -->"))
}

fn exchange_bounds<'a>(text: &'a str, xid: &str) -> Option<(Vec<&'a str>, usize, usize)> {
    let marker = format!("<!-- xid:{xid} -->");
    if !text.contains(&marker) {
        return None;
    }
    let lines: Vec<&str> = text.split('\n').collect();
    let marker_idx = lines.iter().position(|line| line.contains(&marker))?;
    for index in (0..marker_idx).rev() {
        let line = lines.get(index)?;
        if line.starts_with("**Me:**") {
            return Some((lines, index, marker_idx));
        }
        if line.starts_with("## ") || line.starts_with("### ") {
            return None;
        }
    }
    None
}

/// The exact block hashed for optimistic diary edits. Duplicate markers select
/// the first line containing the exact marker, matching the Python helper.
pub fn exchange_text(text: Option<&str>, xid: &str) -> Option<String> {
    let (lines, start, end) = exchange_bounds(text?, xid)?;
    Some(py_strip(&lines.get(start..end)?.join("\n")).to_owned())
}

pub fn exchange_hash(text: Option<&str>, xid: &str) -> Option<String> {
    let block = exchange_text(text, xid)?;
    Some(format!("{:x}", Sha256::digest(block.as_bytes())))
}

/// Rewrite exactly the first marker's bounded exchange. `None` means missing
/// or malformed. A no-op edit still follows Python's render path.
pub fn replace_exchange_text(
    text: &str,
    xid: &str,
    new_me: &str,
    new_claude: &str,
) -> Option<String> {
    let (lines, start, marker_idx) = exchange_bounds(text, xid)?;
    let me = escape_entry_text(py_strip(new_me));
    let claude = escape_entry_text(py_strip(new_claude));
    let mut result = lines.get(..start)?.join("\n");
    if start > 0 {
        result.push('\n');
    }
    result.push_str("**Me:** ");
    result.push_str(&me);
    if !claude.is_empty() {
        result.push_str("\n\n**Assistant:** ");
        result.push_str(&claude);
    }
    result.push_str("\n\n<!-- xid:");
    result.push_str(xid);
    result.push_str(" -->");
    for line in lines.get(marker_idx + 1..)? {
        result.push('\n');
        result.push_str(line);
    }
    Some(result)
}

fn ascii_letters(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|c| c.is_ascii_alphabetic())
}

fn decimal_digits(s: &str, min: usize, max: usize) -> bool {
    let count = s.chars().count();
    (min..=max).contains(&count) && s.chars().all(|c| unicode::decimal(c).is_some())
}

/// Syntactic `DAY_HEADER_RE` check. Python allows a malformed calendar date
/// (and Unicode decimal digits) to terminate a subsection during insertion.
fn day_header_line(line: &str) -> bool {
    let Some(mut rest) = line.strip_prefix("## ") else {
        return false;
    };
    rest = rest.trim_end_matches(py_space);
    if let Some((dow, tail)) = rest.split_once(", ") {
        if ascii_letters(dow) {
            rest = tail;
        }
    }
    let Some((month, tail)) = rest.split_once(' ') else {
        return false;
    };
    if !ascii_letters(month) {
        return false;
    }
    let Some((day, year)) = tail.split_once(", ") else {
        return false;
    };
    decimal_digits(day, 1, 2) && decimal_digits(year, 4, 4)
}

fn drop_trailing_blank_lines(lines: &mut Vec<String>) {
    while lines.last().is_some_and(|line| py_strip(line).is_empty()) {
        lines.pop();
    }
}

/// Append a subsection inside the last exact matching day, or add a new day.
/// This is only the string transform: journal and marker checks stay upstream.
pub fn append_to_month_text(
    month_text: Option<&str>,
    day: NaiveDate,
    sub_header: &str,
    body: &str,
) -> Option<String> {
    let header = canonical_day_header(day)?;
    let block = render_subsection_block(sub_header, body);
    let block_lines: Vec<&str> = block.trim_end_matches('\n').split('\n').collect();
    let Some(month_text) = month_text.filter(|s| !py_strip(s).is_empty()) else {
        return Some(format!("{header}\n\n{}\n", block_lines.join("\n")));
    };
    let mut lines: Vec<String> = month_text
        .trim_end_matches('\n')
        .split('\n')
        .map(str::to_owned)
        .collect();
    if month_text.contains(&header) {
        if let Some(start) = lines.iter().rposition(|line| py_strip(line) == header) {
            let end = lines
                .iter()
                .enumerate()
                .skip(start + 1)
                .find(|(_, line)| day_header_line(line))
                .map_or(lines.len(), |(index, _)| index);
            let mut tail = lines.split_off(end);
            drop_trailing_blank_lines(&mut lines);
            lines.push(String::new());
            lines.extend(block_lines.iter().map(|line| (*line).to_owned()));
            lines.append(&mut tail);
            return Some(format!("{}\n", lines.join("\n").trim_end_matches('\n')));
        }
    }
    drop_trailing_blank_lines(&mut lines);
    lines.extend([String::new(), header, String::new()]);
    lines.extend(block_lines.iter().map(|line| (*line).to_owned()));
    Some(format!("{}\n", lines.join("\n").trim_end_matches('\n')))
}
