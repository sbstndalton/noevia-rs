//! Pure transformations for Diary's INDEX.md format. This crate does no IO,
//! journal work, authorization, clock reading, or service selection.

const MAX_INPUT_BYTES: usize = 1024 * 1024;
const MAX_OUTPUT_BYTES: usize = 2 * 1024 * 1024;
const MAX_SOURCE_LINES: usize = 20_000;
const MAX_RENDER_LINES: usize = 24_000;
const MAX_SECTIONS: usize = 1_000;
const MAX_OPS: usize = 2_000;
const MAX_FIELD_BYTES: usize = 64 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    Capacity,
    InvalidUtf8,
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Capacity => write!(f, "diary INDEX.md capacity exceeded"),
            Self::InvalidUtf8 => write!(f, "diary INDEX.md source is not UTF-8"),
        }
    }
}

impl std::error::Error for Error {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexFile {
    pub month_links: Vec<String>,
    /// Section insertion order is significant, matching Python dict order.
    pub sections: Vec<(String, Vec<String>)>,
    pub preamble: Vec<String>,
}

impl Default for IndexFile {
    fn default() -> Self {
        Self {
            month_links: Vec::new(),
            sections: vec![
                ("Open Questions".to_owned(), Vec::new()),
                ("Timeline of Key Events".to_owned(), Vec::new()),
            ],
            preamble: Vec::new(),
        }
    }
}

fn py_space(c: char) -> bool {
    matches!(c, '\u{0009}'..='\u{000d}' | '\u{001c}'..='\u{0020}' | '\u{0085}' | '\u{00a0}'
        | '\u{1680}' | '\u{2000}'..='\u{200a}' | '\u{2028}' | '\u{2029}' | '\u{202f}' | '\u{205f}' | '\u{3000}')
}

fn py_strip(s: &str) -> &str {
    s.trim_matches(py_space)
}

fn py_rstrip(s: &str) -> &str {
    s.trim_end_matches(py_space)
}

fn py_rstrip_period(s: &str) -> &str {
    s.trim_end_matches('.')
}

mod unicode15;

fn in_ranges(c: char, ranges: &[(u32, u32)]) -> bool {
    let cp = u32::from(c);
    let index = ranges.partition_point(|(_, end)| *end < cp);
    ranges.get(index).is_some_and(|(start, _)| *start <= cp)
}

fn lower_scalar(c: char, out: &mut String) {
    let entry = unicode15::LOWER
        .binary_search_by_key(&u32::from(c), |(cp, _)| *cp)
        .ok()
        .and_then(|index| unicode15::LOWER.get(index));
    if let Some((_, lower)) = entry {
        out.push_str(lower);
    } else {
        out.push(c);
    }
}

fn py_lower(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    if !s.contains('Σ') {
        for c in s.chars() {
            lower_scalar(c, &mut out);
        }
        return out;
    }
    // CPython's language-neutral Final_Sigma rule uses the nearest
    // non-Case_Ignorable character on either side. Two linear passes avoid
    // quadratic scans across long runs of combining marks or apostrophes.
    let chars: Vec<char> = s.chars().collect();
    let mut following_cased = vec![false; chars.len()];
    let mut next_cased = false;
    for (&c, following) in chars.iter().zip(following_cased.iter_mut()).rev() {
        *following = next_cased;
        if !in_ranges(c, unicode15::IGNORABLE) {
            next_cased = in_ranges(c, unicode15::CASED);
        }
    }
    let mut previous_cased = false;
    for (&c, following) in chars.iter().zip(following_cased) {
        if c == 'Σ' {
            out.push(if previous_cased && !following {
                'ς'
            } else {
                'σ'
            });
        } else {
            lower_scalar(c, &mut out);
        }
        if !in_ranges(c, unicode15::IGNORABLE) {
            previous_cased = in_ranges(c, unicode15::CASED);
        }
    }
    out
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

fn py_splitlines(s: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut start = 0;
    let mut chars = s.char_indices().peekable();
    while let Some((at, c)) = chars.next() {
        if line_break(c) {
            if let Some(line) = s.get(start..at) {
                out.push(line);
            }
            start = at + c.len_utf8();
            if c == '\r' && chars.peek().is_some_and(|(_, next)| *next == '\n') {
                chars.next();
                start += 1;
            }
        }
    }
    if let Some(tail) = s.get(start..) {
        if !tail.is_empty() {
            out.push(tail);
        }
    }
    out
}

fn check_field(s: &str) -> Result<(), Error> {
    if s.len() > MAX_FIELD_BYTES {
        Err(Error::Capacity)
    } else {
        Ok(())
    }
}

fn section_title(line: &str) -> Option<&str> {
    let rest = line.strip_prefix("##")?;
    let first = rest.chars().next()?;
    if !py_space(first) {
        return None;
    }
    let tail = rest.get(first.len_utf8()..)?;
    if tail.is_empty() {
        return None;
    }
    Some(py_strip(tail))
}

fn is_month_link(line: &str) -> bool {
    let mut s = line.trim_start_matches(py_space);
    let Some(tail) = s.strip_prefix('-') else {
        return false;
    };
    s = tail.trim_start_matches(py_space);
    let Some(tail) = s.strip_prefix('[') else {
        return false;
    };
    let Some((label, tail)) = tail.split_once("](") else {
        return false;
    };
    if label.is_empty() || label.contains(']') {
        return false;
    }
    let Some((file, trailing)) = tail.split_once(")") else {
        return false;
    };
    let Some(stem) = file.strip_suffix(".md") else {
        return false;
    };
    !stem.is_empty() && !stem.contains(')') && py_strip(trailing).is_empty()
}

impl IndexFile {
    fn ensure_section(&mut self, title: &str) -> Result<usize, Error> {
        if let Some(index) = self.sections.iter().position(|(name, _)| name == title) {
            return Ok(index);
        }
        if self.sections.len() >= MAX_SECTIONS {
            return Err(Error::Capacity);
        }
        self.sections.push((title.to_owned(), Vec::new()));
        Ok(self.sections.len() - 1)
    }

    pub fn section(&self, title: &str) -> Option<&[String]> {
        self.sections
            .iter()
            .find(|(name, _)| name == title)
            .map(|(_, lines)| lines.as_slice())
    }

    pub fn render(&self) -> Result<String, Error> {
        if self.sections.len() > MAX_SECTIONS {
            return Err(Error::Capacity);
        }
        // Count before cloning or joining public fields. A caller may have
        // assembled IndexFile directly, outside parse_index's input cap.
        let mut line_count = self
            .preamble
            .len()
            .saturating_add(self.month_links.len())
            .saturating_add(3);
        let mut byte_count: usize = self
            .preamble
            .iter()
            .chain(&self.month_links)
            .fold(0usize, |sum, line| sum.saturating_add(line.len()));
        for (title, bullets) in &self.sections {
            check_field(title)?;
            line_count = line_count
                .saturating_add(3)
                .saturating_add(bullets.len().max(1));
            byte_count = byte_count.saturating_add(3).saturating_add(title.len());
            if bullets.is_empty() {
                byte_count = byte_count.saturating_add("_None yet._".len());
            }
            for bullet in bullets {
                byte_count = byte_count.saturating_add(bullet.len());
            }
        }
        if line_count > MAX_RENDER_LINES || byte_count.saturating_add(line_count) > MAX_OUTPUT_BYTES
        {
            return Err(Error::Capacity);
        }
        let mut out = Vec::new();
        out.extend(self.preamble.iter().cloned());
        out.push("## Months".to_owned());
        out.push(String::new());
        out.extend(self.month_links.iter().cloned());
        out.push(String::new());
        for (title, bullets) in &self.sections {
            check_field(title)?;
            if bullets.len() > MAX_SOURCE_LINES {
                return Err(Error::Capacity);
            }
            out.push(format!("## {title}"));
            out.push(String::new());
            if bullets.is_empty() {
                out.push("_None yet._".to_owned());
            } else {
                out.extend(bullets.iter().cloned());
            }
            out.push(String::new());
        }
        let mut rendered = py_strip(&out.join("\n")).to_owned();
        rendered.push('\n');
        if rendered.len() > MAX_OUTPUT_BYTES {
            Err(Error::Capacity)
        } else {
            Ok(rendered)
        }
    }
}

/// Decode and parse source bytes. Invalid UTF-8 is an explicit refusal and
/// never becomes an empty INDEX.md value.
pub fn parse_index_bytes(bytes: Option<&[u8]>) -> Result<IndexFile, Error> {
    let Some(bytes) = bytes else {
        return parse_index(None);
    };
    if bytes.len() > MAX_INPUT_BYTES {
        return Err(Error::Capacity);
    }
    let text = std::str::from_utf8(bytes).map_err(|_| Error::InvalidUtf8)?;
    parse_index(Some(text))
}

/// Parse an optional source document. Missing and empty sources both produce
/// standing sections with no bullets, as in the shipped Python helper.
pub fn parse_index(text: Option<&str>) -> Result<IndexFile, Error> {
    let mut idx = IndexFile::default();
    let Some(text) = text.filter(|s| !s.is_empty()) else {
        return Ok(idx);
    };
    if text.len() > MAX_INPUT_BYTES {
        return Err(Error::Capacity);
    }
    let lines = py_splitlines(text);
    if lines.len() > MAX_SOURCE_LINES {
        return Err(Error::Capacity);
    }
    let mut current: Option<usize> = None;
    let mut in_months = false;
    let mut preamble_done = false;
    for line in lines {
        if let Some(title) = section_title(line) {
            check_field(title)?;
            if py_lower(title) == "months" {
                in_months = true;
                current = None;
                preamble_done = true;
                continue;
            }
            in_months = false;
            current = Some(idx.ensure_section(title)?);
            preamble_done = true;
            continue;
        }
        if !preamble_done {
            idx.preamble.push(line.to_owned());
            continue;
        }
        if in_months && is_month_link(line) {
            idx.month_links.push(py_rstrip(line).to_owned());
            continue;
        }
        if let Some(section) = current {
            let trimmed = py_strip(line);
            if trimmed.starts_with('-') || trimmed.starts_with('*') || trimmed.starts_with("<!--") {
                let Some((_, bullets)) = idx.sections.get_mut(section) else {
                    return Err(Error::Capacity);
                };
                bullets.push(py_rstrip(line).to_owned());
            }
        }
    }
    Ok(idx)
}

/// Adds a raw link only if its full bytes are not a substring of an existing
/// link. This retains the Python helper's unusual deduplication rule.
pub fn register_month(idx: &mut IndexFile, label: &str, filename: &str) -> Result<bool, Error> {
    check_field(label)?;
    check_field(filename)?;
    idx.render()?;
    let link = format!("- [{label}]({filename})");
    if idx
        .month_links
        .iter()
        .any(|existing| existing.contains(&link))
    {
        return Ok(false);
    }
    if idx.month_links.len() >= MAX_SOURCE_LINES {
        return Err(Error::Capacity);
    }
    idx.month_links.push(link);
    if idx.render().is_err() {
        idx.month_links.pop();
        return Err(Error::Capacity);
    }
    Ok(true)
}

/// Strip a leading hyphen and optional empty checkbox using Python regex rules.
pub fn bullet_text(bullet: &str) -> &str {
    let mut rest = bullet.trim_start_matches(py_space);
    let Some(tail) = rest.strip_prefix('-') else {
        return bullet;
    };
    rest = tail.trim_start_matches(py_space);
    if let Some(tail) = rest.strip_prefix('[') {
        let ws = tail.trim_start_matches(py_space);
        if let Some(after) = ws.strip_prefix(']') {
            return after.trim_start_matches(py_space);
        }
    }
    rest
}

/// Exact normalized bullet match wins; otherwise match a substring of raw bullets.
pub fn match_indices(bullets: &[String], text: &str) -> Vec<usize> {
    let norm = py_rstrip_period(py_strip(&py_lower(text))).to_owned();
    let exact: Vec<usize> = bullets
        .iter()
        .enumerate()
        .filter_map(|(i, bullet)| {
            let lower = py_lower(bullet_text(bullet));
            (py_rstrip_period(py_strip(&lower)) == norm).then_some(i)
        })
        .collect();
    if !exact.is_empty() {
        return exact;
    }
    bullets
        .iter()
        .enumerate()
        .filter_map(|(i, bullet)| py_lower(bullet).contains(&norm).then_some(i))
        .collect()
}

/// Rewrite an empty checkbox while retaining the Python helper's spacing.
pub fn check_bullet(bullet: &str, checked: bool) -> String {
    let mut prefix_end = bullet.len() - bullet.trim_start_matches(py_space).len();
    if !bullet
        .get(prefix_end..)
        .is_some_and(|tail| tail.starts_with('-'))
    {
        return bullet.to_owned();
    }
    prefix_end += 1;
    let rest = bullet.get(prefix_end..).unwrap_or_default();
    prefix_end += rest.len() - rest.trim_start_matches(py_space).len();
    let rest = bullet.get(prefix_end..).unwrap_or_default();
    let Some(after_open) = rest.strip_prefix('[') else {
        return bullet.to_owned();
    };
    let bracket_ws = after_open.trim_start_matches([' ', '\t']);
    if !bracket_ws.starts_with(']') {
        return bullet.to_owned();
    }
    let Some(after_close) = bracket_ws.strip_prefix(']') else {
        return bullet.to_owned();
    };
    let newmark = if checked { "x" } else { " " };
    format!(
        "{}[{newmark}]{}",
        bullet.get(..prefix_end).unwrap_or_default(),
        after_close
    )
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EditOp {
    pub action: String,
    pub text: String,
    /// Absent replacement differs from explicit null in Python f-strings.
    pub replacement: Option<Option<String>>,
    pub date: Option<String>,
}

fn edit_text(op: &EditOp) -> &str {
    match &op.replacement {
        None => py_strip(&op.text),
        Some(None) => "None",
        Some(Some(value)) => value,
    }
}

fn apply_inner(
    idx: &mut IndexFile,
    questions: &[EditOp],
    timeline: &[EditOp],
    today: &str,
) -> Result<bool, Error> {
    let mut changed = false;
    for op in questions {
        check_field(&op.action)?;
        check_field(&op.text)?;
        if let Some(Some(replacement)) = &op.replacement {
            check_field(replacement)?;
        }
        if let Some(date) = &op.date {
            check_field(date)?;
        }
        let text = py_strip(&op.text);
        if text.is_empty() {
            continue;
        }
        let section = idx.ensure_section("Open Questions")?;
        let Some((_, bullets)) = idx.sections.get_mut(section) else {
            return Err(Error::Capacity);
        };
        if op.action == "add" {
            let norm = py_rstrip_period(&py_lower(text)).to_owned();
            if bullets
                .iter()
                .any(|bullet| py_lower(bullet).contains(&norm))
            {
                continue;
            }
            bullets.push(format!("- [ ] {text}"));
            changed = true;
        } else if op.action == "resolve" || op.action == "edit" {
            let hits = match_indices(bullets, text);
            for hit in hits {
                let Some(bullet) = bullets.get_mut(hit) else {
                    return Err(Error::Capacity);
                };
                let next = if op.action == "resolve" {
                    check_bullet(bullet, true)
                } else {
                    format!("- [ ] {}", edit_text(op))
                };
                if *bullet != next {
                    *bullet = next;
                    changed = true;
                }
            }
        }
    }
    for op in timeline {
        check_field(&op.action)?;
        check_field(&op.text)?;
        if let Some(Some(replacement)) = &op.replacement {
            check_field(replacement)?;
        }
        if let Some(date) = &op.date {
            check_field(date)?;
        }
        let text = py_strip(&op.text);
        if text.is_empty() {
            continue;
        }
        let section = idx.ensure_section("Timeline of Key Events")?;
        let Some((_, bullets)) = idx.sections.get_mut(section) else {
            return Err(Error::Capacity);
        };
        if op.action == "add" {
            let norm = py_rstrip_period(&py_lower(text)).to_owned();
            if bullets
                .iter()
                .any(|bullet| py_lower(bullet).contains(&norm))
            {
                continue;
            }
            let stamp = op
                .date
                .as_deref()
                .filter(|s| !s.is_empty())
                .unwrap_or(today);
            bullets.push(format!("- **{stamp}** — {text}"));
            changed = true;
        } else if op.action == "edit" {
            let repl = op
                .replacement
                .as_ref()
                .and_then(|r| r.as_deref())
                .filter(|s| !s.is_empty());
            let Some(repl) = repl else {
                continue;
            };
            let hits = match_indices(bullets, text);
            let stamp = op
                .date
                .as_deref()
                .filter(|s| !s.is_empty())
                .unwrap_or(today);
            for hit in hits {
                let Some(bullet) = bullets.get_mut(hit) else {
                    return Err(Error::Capacity);
                };
                let next = format!("- **{stamp}** — {repl}");
                if *bullet != next {
                    *bullet = next;
                    changed = true;
                }
            }
        }
    }
    Ok(changed)
}

/// Apply edits atomically to the in-memory value. `today` is the explicit date
/// Python would use when no operation date was provided; no clock is read here.
pub fn apply_index_edits(
    idx: &mut IndexFile,
    questions: &[EditOp],
    timeline: &[EditOp],
    today: &str,
) -> Result<bool, Error> {
    if questions.len().saturating_add(timeline.len()) > MAX_OPS {
        return Err(Error::Capacity);
    }
    check_field(today)?;
    idx.render()?;
    let mut next = idx.clone();
    let changed = apply_inner(&mut next, questions, timeline, today)?;
    next.render()?;
    *idx = next;
    Ok(changed)
}

#[cfg(test)]
mod unicode_tests {
    include!("../tests/oracles/unicode_contexts.rs");

    #[test]
    fn pinned_lower_all_scalars_and_context_boundaries() {
        for cp in 0..=0x10ffff {
            if let Some(c) = char::from_u32(cp) {
                let expected = super::unicode15::LOWER
                    .binary_search_by_key(&cp, |(key, _)| *key)
                    .ok()
                    .and_then(|index| super::unicode15::LOWER.get(index))
                    .map_or_else(|| c.to_string(), |(_, lower)| (*lower).to_owned());
                assert_eq!(super::py_lower(&c.to_string()), expected, "U+{cp:04X}");
            }
        }
        for &(input, expected) in CONTEXTS {
            assert_eq!(super::py_lower(input), expected, "{input:?}");
        }
    }
}
