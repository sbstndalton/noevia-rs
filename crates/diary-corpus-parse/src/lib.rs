//! Pure tolerant Diary parsing and retrieval chunk projection (#1071).
//! No storage, editing, rendering, model calls, or production consumer.
use sha2::{Digest, Sha256};
mod unicode;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Exchange {
    pub me: String,
    /// Legacy field name retained: both Assistant and Claude labels populate it.
    pub claude: String,
    pub xid: Option<String>,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubSection {
    pub header: String,
    pub time: Option<String>,
    pub topic: String,
    pub exchanges: Vec<Exchange>,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DaySection {
    /// Original heading, with Python-compatible trailing whitespace removed.
    pub header: String,
    /// Valid Gregorian date as YYYY-MM-DD; malformed calendar dates remain None.
    pub date: Option<String>,
    pub subsections: Vec<SubSection>,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Chunk {
    pub day: String,
    pub header: String,
    pub text: String,
    pub content_hash: String,
}

fn space(c: char) -> bool {
    matches!(c, '\u{0009}'..='\u{000d}' | '\u{001c}'..='\u{0020}' | '\u{0085}' | '\u{00a0}'
        | '\u{1680}' | '\u{2000}'..='\u{200a}' | '\u{2028}' | '\u{2029}' | '\u{202f}' | '\u{205f}' | '\u{3000}')
}
fn trim(s: &str) -> &str {
    s.trim_matches(space)
}
fn splitlines(text: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut start = 0;
    let mut chars = text.char_indices().peekable();
    while let Some((i, c)) = chars.next() {
        if matches!(
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
        ) {
            out.push(text.get(start..i).unwrap_or_default());
            start = i + c.len_utf8();
            if c == '\r' && chars.peek().is_some_and(|(_, c)| *c == '\n') {
                chars.next();
                start += 1;
            }
        }
    }
    if start < text.len() {
        out.push(text.get(start..).unwrap_or_default());
    }
    out
}
fn ascii_word(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|b| b.is_ascii_alphabetic())
}
fn digits(s: &str, min: usize, max: usize) -> Option<u32> {
    let count = s.chars().count();
    if count < min || count > max {
        return None;
    }
    s.chars()
        .try_fold(0u32, |n, c| Some(n * 10 + unicode::decimal(c)?))
}
/// Outer Some means a syntactically matching day header; inner None is invalid date.
fn day(line: &str) -> Option<Option<String>> {
    let mut rest = line.strip_prefix("## ")?;
    let (first, tail) = rest.split_once(' ')?;
    if let Some(dow) = first.strip_suffix(',') {
        if !ascii_word(dow) {
            return None;
        }
        rest = tail;
    }
    let (month, date) = rest.split_once(' ')?;
    if !ascii_word(month) {
        return None;
    }
    let (d, y) = date.split_once(", ")?;
    let d = digits(d, 1, 2)?;
    let y = digits(y, 4, 4)?;
    let m = [
        "january",
        "february",
        "march",
        "april",
        "may",
        "june",
        "july",
        "august",
        "september",
        "october",
        "november",
        "december",
    ]
    .iter()
    .position(|name| *name == month.to_ascii_lowercase())
    .map(|i| i + 1);
    let Some(m) = m else {
        return Some(None);
    };
    let days = match m {
        4 | 6 | 9 | 11 => 30,
        2 if y % 4 == 0 && (y % 100 != 0 || y % 400 == 0) => 29,
        2 => 28,
        _ => 31,
    };
    if y == 0 || d == 0 || d > days {
        return Some(None);
    }
    Some(Some(format!("{y:04}-{m:02}-{d:02}")))
}
fn time_topic(header: &str) -> (Option<String>, String) {
    let mut chars = header.char_indices().peekable();
    let mut hours = 0;
    while chars
        .peek()
        .is_some_and(|(_, c)| unicode::decimal(*c).is_some())
    {
        chars.next();
        hours += 1;
    }
    if !(1..=2).contains(&hours) || chars.next().map(|(_, c)| c) != Some(':') {
        return (None, trim(header).to_owned());
    }
    for _ in 0..2 {
        if !chars
            .next()
            .is_some_and(|(_, c)| unicode::decimal(c).is_some())
        {
            return (None, trim(header).to_owned());
        }
    }
    let end = chars.peek().map_or(header.len(), |(i, _)| *i);
    let rest = header.get(end..).unwrap_or_default();
    let topic = if rest.is_empty() {
        ""
    } else {
        let after = rest.trim_start_matches(space);
        let Some(first) = after.chars().next() else {
            return (None, trim(header).to_owned());
        };
        if !matches!(first, '—' | '–' | '-') {
            return (None, trim(header).to_owned());
        }
        after
            .get(first.len_utf8()..)
            .unwrap_or_default()
            .trim_start_matches(space)
    };
    (
        Some(header.get(..end).unwrap_or_default().to_owned()),
        trim(topic).to_owned(),
    )
}
fn markers(s: &str) -> (String, Option<String>) {
    let mut out = String::new();
    let mut offset = 0;
    let mut first = None;
    for (start, _) in s.match_indices("<!--") {
        if start < offset {
            continue;
        }
        let after = s
            .get(start + 4..)
            .unwrap_or_default()
            .trim_start_matches(space);
        let Some(after) = after.strip_prefix("xid:") else {
            continue;
        };
        let Some(close) = after.find("-->") else {
            continue;
        };
        let value = after
            .get(..close)
            .unwrap_or_default()
            .trim_end_matches(space);
        if value.is_empty() || !value.bytes().all(|b| b.is_ascii_hexdigit() || b == b'-') {
            continue;
        }
        // Recover the absolute closing byte position without altering Unicode text.
        let end = s.len() - after.len() + close + 3;
        out.push_str(s.get(offset..start).unwrap_or_default());
        if first.is_none() {
            first = Some(value.to_owned());
        }
        offset = end;
    }
    out.push_str(s.get(offset..).unwrap_or_default());
    (out, first)
}
fn role_end(s: &str, labels: &[&str]) -> Option<(usize, usize)> {
    for (offset, _) in s.char_indices() {
        if offset != 0 && s.as_bytes().get(offset - 1) != Some(&b'\n') {
            continue;
        }
        let tail = s.get(offset..)?;
        for label in labels {
            if let Some(after) = tail.strip_prefix(label) {
                let extra = usize::from(after.starts_with(' ') || after.starts_with('\t'));
                return Some((offset, offset + label.len() + extra));
            }
        }
    }
    None
}
fn append(sub: &mut SubSection, body: &str) {
    let mut starts = Vec::new();
    for (i, _) in body.match_indices("**Me:**") {
        if i == 0 || body.as_bytes().get(i - 1) == Some(&b'\n') {
            starts.push(i);
        }
    }
    let mut previous = 0;
    for end in starts.iter().copied().chain(std::iter::once(body.len())) {
        if end != previous {
            parse_part(sub, body.get(previous..end).unwrap_or_default());
        }
        previous = end;
    }
}
fn parse_part(sub: &mut SubSection, part: &str) {
    let part = part.trim_matches('\n');
    if part.is_empty() {
        return;
    }
    let Some((_, end)) = role_end(part, &["**Me:**"]) else {
        if let Some(last) = sub.exchanges.last_mut() {
            last.claude.push('\n');
            last.claude.push_str(part);
        }
        return;
    };
    let rest = part.get(end..).unwrap_or_default();
    let (me, assistant) =
        if let Some((start, end)) = role_end(rest, &["**Assistant:**", "**Claude:**"]) {
            (
                trim(rest.get(..start).unwrap_or_default()),
                trim(rest.get(end..).unwrap_or_default()),
            )
        } else {
            (trim(rest), "")
        };
    let (me, me_xid) = markers(me);
    let (assistant, assistant_xid) = markers(assistant);
    sub.exchanges.push(Exchange {
        me: trim(&me).to_owned(),
        claude: trim(&assistant).to_owned(),
        xid: assistant_xid.or(me_xid),
    });
}
fn flush(days: &mut [DaySection], index: Option<usize>, buf: &mut Vec<&str>) {
    if let Some(sub) =
        index.and_then(|i| days.last_mut().and_then(|day| day.subsections.get_mut(i)))
    {
        if !buf.is_empty() {
            append(sub, trim(&buf.join("\n")));
        }
    }
    buf.clear();
}
/// Tolerant source-order parse; text before the first syntactic day is ignored.
pub fn parse_diary(text: &str) -> Vec<DaySection> {
    let mut days = Vec::new();
    let mut current_sub = None;
    let mut buf = Vec::new();
    for raw in splitlines(text) {
        let line = raw.trim_end_matches(space);
        if let Some(date) = day(line) {
            flush(&mut days, current_sub, &mut buf);
            days.push(DaySection {
                header: line.to_owned(),
                date,
                subsections: Vec::new(),
            });
            current_sub = None;
        } else if let Some(header) = line
            .strip_prefix("### ")
            .filter(|s| !s.is_empty())
            .filter(|_| !days.is_empty())
        {
            flush(&mut days, current_sub, &mut buf);
            if let Some(day) = days.last_mut() {
                let header = trim(header).to_owned();
                let (time, topic) = time_topic(&header);
                day.subsections.push(SubSection {
                    header,
                    time,
                    topic,
                    exchanges: Vec::new(),
                });
                current_sub = day.subsections.len().checked_sub(1);
            }
        } else if !days.is_empty() {
            buf.push(raw);
        }
    }
    flush(&mut days, current_sub, &mut buf);
    days
}
/// Project exactly the Python retrieval chunk strings and UTF-8 SHA256 hashes.
/// Invalid dates are omitted; duplicate rows remain in source order.
pub fn parse_chunks(text: Option<&str>) -> Vec<Chunk> {
    let mut chunks = Vec::new();
    for day in parse_diary(text.unwrap_or_default()) {
        let Some(date) = day.date else {
            continue;
        };
        for sub in day.subsections {
            let body = sub
                .exchanges
                .iter()
                .map(|ex| {
                    if ex.claude.is_empty() {
                        format!("**Me:** {}", ex.me)
                    } else {
                        format!("**Me:** {}\n\n**Assistant:** {}", ex.me, ex.claude)
                    }
                })
                .collect::<Vec<_>>()
                .join("\n");
            let body = trim(&body);
            if body.is_empty() {
                continue;
            }
            let text = format!("[{date}] {}\n### {}\n{body}", day.header, sub.header);
            let content_hash = format!("{:x}", Sha256::digest(text.as_bytes()));
            chunks.push(Chunk {
                day: date.clone(),
                header: sub.header,
                text,
                content_hash,
            });
        }
    }
    chunks
}
