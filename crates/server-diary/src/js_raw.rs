//! `JSON.stringify(JSON.parse(text))` without the limits of a parsed tree, in time and memory linear
//! in the text: no nesting limit, every number JavaScript reads (`1e400` is `Infinity`, written back
//! as `null`), and every string as JavaScript holds it, UTF-16: a lone surrogate escape stays a lone
//! surrogate, and every character, U+10F800..U+10FFFF included, stays itself.
//!
//! * [`valid`] is `JSON.parse`'s acceptance: serde_json validates a raw value iteratively, with no
//!   depth limit, and accepts any `\u` escape (lone surrogates included), as `JSON.parse` does.
//! * A first pass finds the objects that are not written back in the order their keys came: those
//!   with an array-index key after another key or out of ascending order (JavaScript lists index
//!   keys first, ascending), and those with a duplicate key (the last value, in the first one's
//!   place). Only their entries are kept, as byte ranges of the text.
//! * A second pass writes one output buffer front to back: whitespace dropped, strings decoded and
//!   re-quoted here as `JSON.stringify` quotes them, numbers written as JavaScript writes them, and a
//!   reordered object's entries written from their ranges in JavaScript's order. Arrays and every
//!   other object are written as they come. Neither pass recurses or copies a value twice.
//!
//! Keys are handed out as `JSON.stringify` writes them (quotes included): that text names one
//! JavaScript string and no other, lone surrogates included.
use serde_json::value::RawValue;
use std::collections::{HashMap, HashSet};
use std::fmt::Write as _;

/// Whether `JSON.parse(text)` succeeds.
pub(crate) fn valid(text: &str) -> bool {
    serde_json::from_str::<&RawValue>(text).is_ok()
}

/// A parsed and re-written text.
pub(crate) enum Top {
    /// A top-level object: `(key, value)`, both as `JSON.stringify` writes them, in JavaScript's
    /// property order.
    Object(Vec<(String, String)>),
    /// Anything else, as `JSON.stringify` writes it.
    Other(String),
}

// ── strings ─────────────────────────────────────────────────────────────────────────────────────

/// One piece of a JavaScript string: a character, or a UTF-16 surrogate without its partner.
enum Piece {
    Char(char),
    Lone(u16),
}

fn hex4(b: &[u8]) -> Option<u16> {
    let digits = b.get(..4)?;
    if !digits.iter().all(u8::is_ascii_hexdigit) {
        return None;
    }
    u16::from_str_radix(std::str::from_utf8(digits).ok()?, 16).ok()
}

fn flush(high: &mut Option<u16>, f: &mut impl FnMut(Piece)) {
    if let Some(unit) = high.take() {
        f(Piece::Lone(unit));
    }
}

/// The JavaScript string a string literal (quotes included) denotes, piece by piece: an escaped
/// surrogate pair is its character; an escaped surrogate without its partner is a lone surrogate. A
/// character written as itself is always itself.
fn pieces(literal: &str, mut f: impl FnMut(Piece)) -> Option<()> {
    let mut rest = literal.strip_prefix('"')?.strip_suffix('"')?;
    let mut high: Option<u16> = None;
    while let Some(c) = rest.chars().next() {
        if c != '\\' {
            flush(&mut high, &mut f);
            f(Piece::Char(c));
            rest = rest.get(c.len_utf8()..)?;
            continue;
        }
        let simple = match *rest.as_bytes().get(1)? {
            b'"' => '"',
            b'\\' => '\\',
            b'/' => '/',
            b'b' => '\u{8}',
            b'f' => '\u{c}',
            b'n' => '\n',
            b'r' => '\r',
            b't' => '\t',
            b'u' => {
                let unit = hex4(rest.as_bytes().get(2..)?)?;
                rest = rest.get(6..)?;
                match unit {
                    0xD800..=0xDBFF => {
                        flush(&mut high, &mut f);
                        high = Some(unit);
                    }
                    0xDC00..=0xDFFF => match high.take() {
                        Some(h) => {
                            let code = 0x10000
                                + ((u32::from(h) - 0xD800) << 10)
                                + (u32::from(unit) - 0xDC00);
                            f(Piece::Char(char::from_u32(code)?));
                        }
                        None => f(Piece::Lone(unit)),
                    },
                    _ => {
                        flush(&mut high, &mut f);
                        f(Piece::Char(char::from_u32(u32::from(unit))?));
                    }
                }
                continue;
            }
            _ => return None,
        };
        flush(&mut high, &mut f);
        f(Piece::Char(simple));
        rest = rest.get(2..)?;
    }
    flush(&mut high, &mut f);
    Some(())
}

/// `JSON.stringify`'s quoting of one piece.
fn quote_piece(piece: Piece, out: &mut String) {
    match piece {
        Piece::Lone(unit) => {
            let _ = write!(out, "\\u{unit:04x}");
        }
        Piece::Char('"') => out.push_str("\\\""),
        Piece::Char('\\') => out.push_str("\\\\"),
        Piece::Char('\u{8}') => out.push_str("\\b"),
        Piece::Char('\u{c}') => out.push_str("\\f"),
        Piece::Char('\n') => out.push_str("\\n"),
        Piece::Char('\r') => out.push_str("\\r"),
        Piece::Char('\t') => out.push_str("\\t"),
        Piece::Char(c) if u32::from(c) < 0x20 => {
            let _ = write!(out, "\\u{:04x}", u32::from(c));
        }
        Piece::Char(c) => out.push(c),
    }
}

/// `JSON.stringify(JSON.parse(literal))` of one valid string literal, into `out`.
fn write_string(literal: &str, out: &mut String) -> Option<()> {
    // Without a backslash a valid literal already is what JSON.stringify writes: it holds no quote
    // and no control character, and every other character is written as itself.
    if !literal.contains('\\') {
        out.push_str(literal);
        return Some(());
    }
    out.push('"');
    pieces(literal, |p| quote_piece(p, out))?;
    out.push('"');
    Some(())
}

/// `JSON.stringify(s)`.
pub(crate) fn quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        quote_piece(Piece::Char(c), &mut out);
    }
    out.push('"');
    out
}

/// The string a written string (a key or a value from [`read`]) holds, a lone surrogate read as
/// U+FFFD: for comparing, never for writing back. `None` when it is not a string.
pub(crate) fn string_of(written: &str) -> Option<String> {
    if !written.starts_with('"') {
        return None;
    }
    let mut out = String::with_capacity(written.len());
    pieces(written, |p| match p {
        Piece::Char(c) => out.push(c),
        Piece::Lone(_) => out.push('\u{FFFD}'),
    })?;
    Some(out)
}

/// Whether a written key is `name` (a name that needs no escape).
pub(crate) fn is_key(written: &str, name: &str) -> bool {
    written.strip_prefix('"').and_then(|k| k.strip_suffix('"')) == Some(name)
}

/// The array index a written key names (`"0"`..`"4294967294"`), if any.
fn array_index(written: &str) -> Option<u32> {
    js_json::wtf16::array_index(written.strip_prefix('"')?.strip_suffix('"')?)
}

// ── first pass: which objects are reordered ─────────────────────────────────────────────────────

fn string_end(b: &[u8], start: usize) -> Option<usize> {
    let mut j = start + 1;
    loop {
        match *b.get(j)? {
            b'\\' => j += 2,
            b'"' => return Some(j + 1),
            _ => j += 1,
        }
    }
}

fn scalar_end(b: &[u8], start: usize) -> usize {
    let mut j = start;
    while let Some(c) = b.get(j) {
        if matches!(c, b',' | b'}' | b']' | b' ' | b'\t' | b'\n' | b'\r') {
            break;
        }
        j += 1;
    }
    j
}

/// A key as written back: the literal's own range when it has no escape (then it already is the
/// written text), or its re-quoted text.
enum Key {
    Range(usize, usize),
    Owned(String),
}

impl Key {
    fn text<'a>(&'a self, source: &'a str) -> &'a str {
        match self {
            Key::Range(s, e) => source.get(*s..*e).unwrap_or_default(),
            Key::Owned(t) => t,
        }
    }
}

struct Entry {
    key: Key,
    /// The value's byte range.
    value: (usize, usize),
}

/// An object the first pass is inside. Its entries are `entries[base..]` of the pass's one stack.
struct Open {
    start: usize,
    base: usize,
    expect_key: bool,
    reorder: bool,
    last_index: Option<u32>,
    named_key: bool,
    /// The keys seen, once there are more than [`SCAN_KEYS`] (fewer are compared one by one).
    seen: Option<HashSet<String>>,
}

const SCAN_KEYS: usize = 8;

impl Open {
    /// Notes one more key (not yet on `mine`, the object's entries so far).
    fn key(&mut self, text: &str, mine: &[Entry], source: &str) {
        let duplicate = match &mut self.seen {
            Some(seen) => !seen.insert(text.to_owned()),
            None => {
                let duplicate = mine.iter().any(|e| e.key.text(source) == text);
                if mine.len() >= SCAN_KEYS {
                    let mut seen: HashSet<String> =
                        mine.iter().map(|e| e.key.text(source).to_owned()).collect();
                    seen.insert(text.to_owned());
                    self.seen = Some(seen);
                }
                duplicate
            }
        };
        match array_index(text) {
            Some(n) => {
                if self.named_key || self.last_index.is_some_and(|m| m >= n) {
                    self.reorder = true;
                }
                self.last_index = Some(n);
            }
            None => self.named_key = true,
        }
        if duplicate {
            self.reorder = true;
        }
    }
}

/// The objects not written in the order their keys came, by the offset of their `{`: the offset
/// just past their `}`, and their entries in written order. The top-level object is always here.
#[derive(Default)]
struct Plan {
    objects: HashMap<usize, (usize, Vec<Entry>)>,
}

/// One object's entries in written order: index keys ascending, then the others as they came, a
/// duplicate's last value in its first place.
fn ordered(source: &str, entries: Vec<Entry>) -> Vec<Entry> {
    let mut first: HashMap<String, usize> = HashMap::new();
    let mut out: Vec<Entry> = Vec::with_capacity(entries.len());
    for entry in entries {
        let text = entry.key.text(source);
        match first.get(text).and_then(|&at| out.get_mut(at)) {
            Some(slot) => slot.value = entry.value,
            None => {
                first.insert(text.to_owned(), out.len());
                out.push(entry);
            }
        }
    }
    out.sort_by_key(|e| match array_index(e.key.text(source)) {
        Some(n) => (0u8, n),
        None => (1u8, 0),
    });
    out
}

/// The first pass over a [`valid`] text.
fn scan(source: &str) -> Option<Plan> {
    let b = source.as_bytes();
    let mut plan = Plan::default();
    // One flag per open level, `true` for an object; the objects' own state on `objects`.
    let mut levels: Vec<bool> = Vec::new();
    let mut objects: Vec<Open> = Vec::new();
    let mut entries: Vec<Entry> = Vec::new();
    let mut i = 0usize;
    while let Some(&c) = b.get(i) {
        let in_object = levels.last() == Some(&true);
        match c {
            b' ' | b'\t' | b'\n' | b'\r' | b':' => i += 1,
            b',' => {
                if in_object {
                    objects.last_mut()?.expect_key = true;
                }
                i += 1;
            }
            b'{' | b'[' => {
                if in_object {
                    entries.last_mut()?.value.0 = i;
                }
                levels.push(c == b'{');
                if c == b'{' {
                    objects.push(Open {
                        start: i,
                        base: entries.len(),
                        expect_key: true,
                        reorder: false,
                        last_index: None,
                        named_key: false,
                        seen: None,
                    });
                }
                i += 1;
            }
            b'}' | b']' => {
                i += 1;
                if levels.pop()? {
                    let open = objects.pop()?;
                    if open.reorder || levels.is_empty() {
                        let mine = entries.split_off(open.base);
                        plan.objects.insert(open.start, (i, ordered(source, mine)));
                    } else {
                        entries.truncate(open.base);
                    }
                }
                if levels.last() == Some(&true) {
                    entries.last_mut()?.value.1 = i;
                }
            }
            b'"' => {
                let end = string_end(b, i)?;
                let literal = source.get(i..end)?;
                if in_object && objects.last()?.expect_key {
                    let key = if literal.contains('\\') {
                        let mut text = String::with_capacity(literal.len());
                        write_string(literal, &mut text)?;
                        Key::Owned(text)
                    } else {
                        Key::Range(i, end)
                    };
                    let open = objects.last_mut()?;
                    open.expect_key = false;
                    open.key(key.text(source), entries.get(open.base..)?, source);
                    entries.push(Entry { key, value: (i, i) });
                } else if in_object {
                    entries.last_mut()?.value = (i, end);
                }
                i = end;
            }
            _ => {
                let end = scalar_end(b, i);
                if in_object {
                    entries.last_mut()?.value = (i, end);
                }
                i = end;
            }
        }
    }
    levels.is_empty().then_some(plan)
}

// ── second pass: the one output buffer ──────────────────────────────────────────────────────────

enum Task<'p> {
    /// Write `source[start..end]`.
    Range(usize, usize),
    Text(&'static str),
    Key(&'p Key),
}

/// Writes `source[start..end]` (whole values and the separators between them) as `JSON.stringify`
/// writes them.
fn write(source: &str, plan: &Plan, start: usize, end: usize, out: &mut String) -> Option<()> {
    let b = source.as_bytes();
    let mut tasks: Vec<Task<'_>> = vec![Task::Range(start, end)];
    while let Some(task) = tasks.pop() {
        let (mut i, end) = match task {
            Task::Text(text) => {
                out.push_str(text);
                continue;
            }
            Task::Key(key) => {
                out.push_str(key.text(source));
                continue;
            }
            Task::Range(s, e) => (s, e),
        };
        while i < end {
            let c = *b.get(i)?;
            match c {
                b' ' | b'\t' | b'\n' | b'\r' => i += 1,
                b'{' if plan.objects.contains_key(&i) => {
                    let (after, entries) = plan.objects.get(&i)?;
                    out.push('{');
                    // What follows the object, then its `}`, then its entries, first on top.
                    tasks.push(Task::Range(*after, end));
                    tasks.push(Task::Text("}"));
                    for (n, entry) in entries.iter().enumerate().rev() {
                        tasks.push(Task::Range(entry.value.0, entry.value.1));
                        tasks.push(Task::Text(":"));
                        tasks.push(Task::Key(&entry.key));
                        if n != 0 {
                            tasks.push(Task::Text(","));
                        }
                    }
                    break;
                }
                b'{' | b'}' | b'[' | b']' | b',' | b':' => {
                    out.push(char::from(c));
                    i += 1;
                }
                b'"' => {
                    let e = string_end(b, i)?;
                    write_string(source.get(i..e)?, out)?;
                    i = e;
                }
                _ => {
                    let e = scalar_end(b, i);
                    match source.get(i..e)? {
                        literal @ ("true" | "false" | "null") => out.push_str(literal),
                        number => {
                            let n = number.parse::<f64>().ok()?;
                            if n.is_finite() {
                                out.push_str(&js_json::number_to_string(n));
                            } else {
                                out.push_str("null");
                            }
                        }
                    }
                    i = e;
                }
            }
        }
    }
    Some(())
}

/// A text, re-written as `JSON.stringify(JSON.parse(text))` writes it. `None` only when
/// `JSON.parse` throws.
pub(crate) fn read(text: &str) -> Option<Top> {
    if !valid(text) {
        return None;
    }
    let plan = scan(text)?;
    let first = text
        .bytes()
        .position(|c| !matches!(c, b' ' | b'\t' | b'\n' | b'\r'))?;
    if text.as_bytes().get(first) == Some(&b'{') {
        let (_, entries) = plan.objects.get(&first)?;
        let mut out = Vec::with_capacity(entries.len());
        for entry in entries {
            let mut value = String::new();
            write(text, &plan, entry.value.0, entry.value.1, &mut value)?;
            out.push((entry.key.text(text).to_owned(), value));
        }
        return Some(Top::Object(out));
    }
    let mut out = String::with_capacity(text.len());
    write(text, &plan, 0, text.len(), &mut out)?;
    Some(Top::Other(out))
}

/// `{k:v,...}` of entries [`read`] gave.
pub(crate) fn join<'a>(entries: impl IntoIterator<Item = &'a (String, String)>) -> String {
    let mut out = String::from("{");
    for (i, (k, v)) in entries.into_iter().enumerate() {
        if i != 0 {
            out.push(',');
        }
        out.push_str(k);
        out.push(':');
        out.push_str(v);
    }
    out.push('}');
    out
}

/// `JSON.stringify(JSON.parse(text))`; `None` when `JSON.parse` throws.
pub(crate) fn canonical(text: &str) -> Option<String> {
    match read(text)? {
        Top::Object(entries) => Some(join(&entries)),
        Top::Other(out) => Some(out),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use js_json::wtf16;

    #[test]
    fn what_node_writes_back() {
        for (input, expected) in [
            (
                " { \"b\" : 1.50 , \"2\":0,\"1\":[ 1e400 ,-0, 1E2],\"b\":true } ",
                r#"{"1":[null,0,100],"2":0,"b":true}"#,
            ),
            (r#""A\ud800""#, r#""A\ud800""#),
            ("[]", "[]"),
            ("{}", "{}"),
            (
                r#"{"a":{"z":null,"0":"x"},"4294967295":1,"10":2,"9":3}"#,
                r#"{"9":3,"10":2,"a":{"0":"x","z":null},"4294967295":1}"#,
            ),
            ("12345678901234567890", "12345678901234567000"),
            (
                r#"{"b":1,"b":2,"a\/":"\u001F\t \"\\"}"#,
                "{\"b\":2,\"a/\":\"\\u001f\\t\u{2028}\\\"\\\\\"}",
            ),
            (
                r#"[{"x":[{"1":0,"0":{"k":1,"k":[2],"j":{}}}]},3]"#,
                r#"[{"x":[{"0":{"k":[2],"j":{}},"1":0}]},3]"#,
            ),
            (
                r#"{"a":1,"b":2,"c":3,"d":4,"e":5,"f":6,"g":7,"h":8,"i":9,"a":0,"i":[]}"#,
                r#"{"a":0,"b":2,"c":3,"d":4,"e":5,"f":6,"g":7,"h":8,"i":[]}"#,
            ),
        ] {
            assert_eq!(canonical(input).as_deref(), Some(expected), "{input}");
            // Where the parsed tree can hold the value, both give Node's bytes.
            if !input.contains("e400") {
                let tree = wtf16::stringify(&wtf16::parse(input).unwrap());
                assert_eq!(tree.as_deref(), Some(expected), "{input}");
            }
        }
        for bad in [
            "",
            "{",
            "[1,]",
            "{\"a\" 1}",
            "01",
            "\"\u{1}\"",
            "{} x",
            "\u{feff}{}",
        ] {
            assert!(read(bad).is_none(), "{bad:?}");
        }
    }

    #[test]
    fn top_plane_characters_stay_themselves_and_lone_surrogates_stay_lone() {
        let top = '\u{10FFFF}';
        let low = '\u{10F800}';
        for (input, expected) in [
            (format!("\"{top}\""), format!("\"{top}\"")),
            (r#""􏿿""#.to_owned(), format!("\"{top}\"")),
            (r#""􏠀""#.to_owned(), format!("\"{low}\"")),
            // A lone escape beside the character a lone surrogate maps to elsewhere: both kept.
            (
                format!(r#""\udbff{top}\udfff\ud800""#),
                format!(r#""\udbff{top}\udfff\ud800""#),
            ),
            (
                format!(r#"{{"{top}":1,"􏿿":2}}"#),
                format!(r#"{{"{top}":2}}"#),
            ),
            (
                format!(r#"{{"\udfff":1,"{top}":2}}"#),
                format!(r#"{{"\udfff":1,"{top}":2}}"#),
            ),
            (
                format!(r#"{{"\ud800":"{low}"}}"#),
                format!(r#"{{"\ud800":"{low}"}}"#),
            ),
        ] {
            assert_eq!(
                canonical(&input).as_deref(),
                Some(expected.as_str()),
                "{input}"
            );
        }
        assert_eq!(
            string_of(&format!(r#""a{top}\ud800""#)).unwrap(),
            format!("a{top}\u{FFFD}")
        );
        assert_eq!(
            quote(&format!("\"{top}\n\u{1}")),
            format!(r#""\"{top}\n\u0001""#)
        );
        assert!(is_key("\"reason\"", "reason"));
        assert!(!is_key("\"reasons\"", "reason"));
    }

    #[test]
    fn no_nesting_limit() {
        let depth = 100_000;
        let text = format!("{{\"a\":{}1{}}}", "[".repeat(depth), "]".repeat(depth));
        assert!(valid(&text));
        assert_eq!(canonical(&text).unwrap(), text);
        let text = format!("{}0{}", "{\"b\":0,\"1\":".repeat(depth), "}".repeat(depth));
        let expected = format!("{}0{}", "{\"1\":".repeat(depth), ",\"b\":0}".repeat(depth));
        assert_eq!(canonical(&text).unwrap(), expected);
    }
}
