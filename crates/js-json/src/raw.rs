//! Shared copy of noevia-rs crates/server-diary js_raw.rs (move/diary-local-exchange @ a936a3a8),
//! public here for the other ports that need `JSON.parse` acceptance without a tree's limits (chat
//! port S4, bins/noevia-server chat_framing_suggest.rs). Changes from that file: `pub` visibility,
//! `valid` through `IgnoredAny`, and [`slice_string`]. The Diary port can switch to this module once
//! both are on main.
//!
//! `JSON.stringify(JSON.parse(text))` without the limits of a parsed tree, in time and memory linear
//! in the text (keys are hashed, at expected constant cost each): no nesting limit, every number
//! JavaScript reads, written with V8's digits (`1e400` is `Infinity`, written back as `null`), and
//! every string as JavaScript holds it, UTF-16: a lone surrogate escape stays a lone surrogate, and
//! every character, U+10F800..U+10FFFF included, stays itself.
//!
//! * [`valid`] is `JSON.parse`'s acceptance: serde_json validates a raw value iteratively, with no
//!   depth limit, and accepts any `\u` escape (lone surrogates included), as `JSON.parse` does.
//! * A first pass finds the objects that are not written back in the order their keys came: those
//!   with an array-index key after another key or out of ascending order (JavaScript lists index
//!   keys first, ascending), and those with a duplicate key (the last value, in the first one's
//!   place). Only their entries are kept, as byte ranges of the text, and put in order without
//!   comparing keys: index keys are distinct `u32`s, radix-sorted.
//! * A second pass writes one output buffer front to back: whitespace dropped, strings decoded and
//!   re-quoted here as `JSON.stringify` quotes them, numbers written as JavaScript writes them, and a
//!   reordered object's entries written from their ranges in JavaScript's order. Arrays and every
//!   other object are written as they come. Neither pass recurses or copies a value twice.
//!
//! Keys are handed out as `JSON.stringify` writes them (quotes included): that text names one
//! JavaScript string and no other, lone surrogates included.
use crate::{wtf16, JValue};
use std::collections::{HashMap, HashSet};
use std::fmt::Write as _;

/// Whether `JSON.parse(text)` succeeds. serde_json skips an ignored value with the same iterative
/// scanner its `RawValue` uses (no depth limit, digits read but not converted, any `\u` escape), so
/// this needs no `raw_value` feature.
pub fn valid(text: &str) -> bool {
    serde_json::from_str::<serde::de::IgnoredAny>(text).is_ok()
}

/// A parsed and re-written text.
pub enum Top {
    /// A top-level object's properties, in JavaScript's property order.
    Object(Vec<Field>),
    /// Anything else, as `JSON.stringify` writes it, and the number it is when it is one.
    Other(String, Option<f64>),
}

/// One property of a top-level object.
pub struct Field {
    /// The key, as `JSON.stringify` writes it.
    pub key: String,
    /// The value, as `JSON.stringify` writes it.
    pub value: String,
    /// The number the value is, when it is one, as JavaScript holds it: `1e400` is `Infinity`,
    /// which is written `null` but is truthy, not nullish, and no finite number.
    pub number: Option<f64>,
}

/// The number a scalar literal is (`None`: not a number).
fn number_of(literal: &str) -> Option<f64> {
    if matches!(literal.as_bytes().first(), Some(b'-' | b'0'..=b'9')) {
        literal.parse::<f64>().ok()
    } else {
        None
    }
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
pub fn quote(s: &str) -> String {
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
pub fn string_of(written: &str) -> Option<String> {
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

/// `s.slice(0, n)` of the string a written string literal (from [`read`]) holds, written back as
/// `JSON.stringify` writes it: UTF-16 units are counted, and a surrogate pair cut in half keeps its
/// high surrogate, as `String.prototype.slice` does. `None` when it is not a string.
pub fn slice_string(written: &str, n: usize) -> Option<String> {
    if !written.starts_with('"') {
        return None;
    }
    let mut out = String::with_capacity(written.len().min(n.saturating_mul(6).saturating_add(2)));
    out.push('"');
    let mut units = 0usize;
    pieces(written, |p| {
        if units >= n {
            return;
        }
        match p {
            Piece::Lone(unit) => {
                units += 1;
                quote_piece(Piece::Lone(unit), &mut out);
            }
            Piece::Char(c) => {
                let w = c.len_utf16();
                if units + w > n {
                    let mut buf = [0u16; 2];
                    if let Some(&high) = c.encode_utf16(&mut buf).first() {
                        quote_piece(Piece::Lone(high), &mut out);
                    }
                    units = n;
                } else {
                    units += w;
                    quote_piece(Piece::Char(c), &mut out);
                }
            }
        }
    })?;
    out.push('"');
    Some(out)
}

/// Whether a written key is `name` (a name that needs no escape).
pub fn is_key(written: &str, name: &str) -> bool {
    written.strip_prefix('"').and_then(|k| k.strip_suffix('"')) == Some(name)
}

/// The array index a written key names (`"0"`..`"4294967294"`), if any.
fn array_index(written: &str) -> Option<u32> {
    crate::wtf16::array_index(written.strip_prefix('"')?.strip_suffix('"')?)
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
    js_order(out, |e| array_index(e.key.text(source)))
}

/// JavaScript's property order for distinct keys: those naming an array index first, ascending,
/// then the rest as they came. Linear: the indices are radix-sorted.
fn js_order<T>(items: Vec<T>, index: impl Fn(&T) -> Option<u32>) -> Vec<T> {
    let mut indexed: Vec<(u32, T)> = Vec::new();
    let mut named: Vec<T> = Vec::with_capacity(items.len());
    for item in items {
        match index(&item) {
            Some(n) => indexed.push((n, item)),
            None => named.push(item),
        }
    }
    radix_sort(&mut indexed);
    indexed
        .into_iter()
        .map(|(_, item)| item)
        .chain(named)
        .collect()
}

/// Sorts by the `u32`, a byte at a time from the least significant (stable). A few items are sorted
/// by comparison, a bounded cost per object.
fn radix_sort<T>(items: &mut Vec<(u32, T)>) {
    if items.len() <= 64 {
        items.sort_by_key(|(n, _)| *n);
        return;
    }
    for shift in [0u32, 8, 16, 24] {
        let mut buckets: Vec<Vec<(u32, T)>> = (0..256).map(|_| Vec::new()).collect();
        for item in items.drain(..) {
            let byte = ((item.0 >> shift) & 0xff) as usize;
            if let Some(bucket) = buckets.get_mut(byte) {
                bucket.push(item);
            }
        }
        for bucket in buckets {
            items.extend(bucket);
        }
    }
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
                                out.push_str(&crate::number_to_string(n));
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
pub fn read(text: &str) -> Option<Top> {
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
            out.push(Field {
                key: entry.key.text(text).to_owned(),
                value,
                number: number_of(text.get(entry.value.0..entry.value.1)?),
            });
        }
        return Some(Top::Object(out));
    }
    let mut out = String::with_capacity(text.len());
    write(text, &plan, 0, text.len(), &mut out)?;
    let number = number_of(text.trim_matches([' ', '\t', '\n', '\r']));
    Some(Top::Other(out, number))
}

/// `{k:v,...}` of properties [`read`] gave.
pub fn join<'a>(fields: impl IntoIterator<Item = &'a Field>) -> String {
    let mut out = String::from("{");
    for (i, field) in fields.into_iter().enumerate() {
        if i != 0 {
            out.push(',');
        }
        out.push_str(&field.key);
        out.push(':');
        out.push_str(&field.value);
    }
    out.push('}');
    out
}

/// `JSON.stringify(JSON.parse(text))`; `None` when `JSON.parse` throws.
pub fn canonical(text: &str) -> Option<String> {
    match read(text)? {
        Top::Object(fields) => Some(join(&fields)),
        Top::Other(out, _) => Some(out),
    }
}

/// How deep [`tree`] goes: [`JValue`] is dropped recursively.
pub const TREE_DEPTH: usize = 128;

/// The string a literal holds, as [`wtf16`] holds it (a lone surrogate as its mapped character);
/// `None` for a character of U+10F800..U+10FFFF, which that mapping cannot tell from one.
fn tree_string(literal: &str) -> Option<String> {
    let mut out = String::with_capacity(literal.len());
    let mut held = true;
    pieces(literal, |p| match p {
        Piece::Char(c) if wtf16::lone_unit(c).is_none() => out.push(c),
        Piece::Lone(unit) => match wtf16::lone_char(unit) {
            Some(c) => out.push(c),
            None => held = false,
        },
        Piece::Char(_) => held = false,
    })?;
    held.then_some(out)
}

enum Node {
    Arr(Vec<JValue>),
    Obj {
        items: Vec<(String, JValue)>,
        at: HashMap<String, usize>,
        key: Option<String>,
    },
}

/// Puts a finished value in its parent, or makes it the result.
fn place(stack: &mut [Node], done: &mut Option<JValue>, value: JValue) -> Option<()> {
    match stack.last_mut() {
        None => *done = Some(value),
        Some(Node::Arr(items)) => items.push(value),
        Some(Node::Obj { items, at, key }) => {
            let key = key.take()?;
            match at.get(&key).and_then(|&i| items.get_mut(i)) {
                Some(slot) => slot.1 = value,
                None => {
                    at.insert(key.clone(), items.len());
                    items.push((key, value));
                }
            }
        }
    }
    Some(())
}

/// `JSON.parse(text)` as a [`JValue`]: keys in JavaScript's order, a duplicate key's last value in
/// its first place, numbers as JavaScript reads them (`1e400` is `Infinity`). `None` when
/// `JSON.parse` throws, and where the tree cannot hold the value: nested past [`TREE_DEPTH`]
/// levels, or a string holding a character of U+10F800..U+10FFFF (see [`tree_string`]).
pub fn tree(text: &str) -> Option<JValue> {
    if !valid(text) {
        return None;
    }
    let b = text.as_bytes();
    let mut stack: Vec<Node> = Vec::new();
    let mut done: Option<JValue> = None;
    let mut i = 0usize;
    while let Some(&c) = b.get(i) {
        match c {
            b' ' | b'\t' | b'\n' | b'\r' | b',' | b':' => i += 1,
            b'[' | b'{' => {
                if stack.len() >= TREE_DEPTH {
                    return None;
                }
                stack.push(if c == b'[' {
                    Node::Arr(Vec::new())
                } else {
                    Node::Obj {
                        items: Vec::new(),
                        at: HashMap::new(),
                        key: None,
                    }
                });
                i += 1;
            }
            b']' | b'}' => {
                let value = match stack.pop()? {
                    Node::Arr(items) => JValue::Arr(items),
                    Node::Obj { items, .. } => {
                        JValue::Obj(js_order(items, |(k, _)| wtf16::array_index(k)))
                    }
                };
                place(&mut stack, &mut done, value)?;
                i += 1;
            }
            b'"' => {
                let end = string_end(b, i)?;
                let s = tree_string(text.get(i..end)?)?;
                i = end;
                if let Some(Node::Obj { key, .. }) = stack.last_mut() {
                    if key.is_none() {
                        *key = Some(s);
                        continue;
                    }
                }
                place(&mut stack, &mut done, JValue::Str(s))?;
            }
            _ => {
                let end = scalar_end(b, i);
                let value = match text.get(i..end)? {
                    "true" => JValue::Bool(true),
                    "false" => JValue::Bool(false),
                    "null" => JValue::Null,
                    number => JValue::Num(number.parse::<f64>().ok()?),
                };
                i = end;
                place(&mut stack, &mut done, value)?;
            }
        }
    }
    if stack.is_empty() {
        done
    } else {
        None
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    #[test]
    fn slice_string_cuts_utf16_units_as_slice_does() {
        let cut = |w: &str, n| slice_string(w, n).unwrap();
        assert_eq!(cut(r#""abc""#, 2), r#""ab""#);
        assert_eq!(cut(r#""abc""#, 9), r#""abc""#);
        assert_eq!(cut("\"a\u{1F600}b\"", 2), r#""a\ud83d""#);
        assert_eq!(cut("\"a\u{1F600}b\"", 3), "\"a\u{1F600}\"");
        assert_eq!(cut(r#""\udc00x\ud83d""#, 2), r#""\udc00x""#);
        assert_eq!(cut("\"\u{10FFFF}z\"", 1), r#""\udbff""#);
        assert_eq!(cut("\"\u{10F800}z\"", 1), r#""\udbfe""#);
        assert_eq!(cut(r#""q\"\n""#, 2), r#""q\"""#);
        assert_eq!(slice_string("12", 1), None);
    }

    #[test]
    fn valid_is_json_parse_without_a_depth_limit() {
        let deep = format!("{}1{}", "[".repeat(5000), "]".repeat(5000));
        for ok in [
            deep.as_str(),
            "1e400",
            "-1e400",
            r#""\ud800""#,
            " {} ",
            "\"\u{10FFFF}\"",
        ] {
            assert!(valid(ok), "{ok}");
        }
        for bad in [
            "",
            "\u{feff}{}",
            "{",
            "[1,]",
            "{} x",
            "'x'",
            "01",
            "1.",
            "NaN",
        ] {
            assert!(!valid(bad), "{bad}");
        }
    }

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
    fn wide_objects_of_shuffled_index_keys_are_ordered_as_javascript_orders_them() {
        // A million distinct index keys in a shuffled order, every 10th repeated later with a new
        // value, and names in between: index keys ascending (a repeat's value in the first place),
        // then the names as they came.
        let n: u32 = 1_000_000;
        let mut keys: Vec<u32> = (0..n)
            .map(|i| (u64::from(i) * 2_654_435_761 % 4_294_967_295) as u32)
            .collect();
        let mut x: u64 = 88172645463325252;
        for i in (1..keys.len()).rev() {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            keys.swap(i, (x % (i as u64 + 1)) as usize);
        }
        let mut text = String::from("{");
        let mut values = std::collections::BTreeMap::new();
        for (i, k) in keys.iter().enumerate() {
            if i % 1000 == 0 {
                text.push_str(&format!("\"name{i}\":{i},"));
            }
            text.push_str(&format!("\"{k}\":{i},"));
            values.insert(*k, i);
        }
        for (i, k) in keys.iter().enumerate().step_by(10) {
            text.push_str(&format!("\"{k}\":-{},", i + 1));
            values.insert(*k, usize::MAX - (i + 1));
        }
        text.push_str("\"last\":0}");
        let mut expected = String::from("{");
        for (k, v) in &values {
            let v = if *v > usize::MAX / 2 {
                format!("-{}", usize::MAX - v)
            } else {
                v.to_string()
            };
            expected.push_str(&format!("\"{k}\":{v},"));
        }
        for i in (0..keys.len()).step_by(1000) {
            expected.push_str(&format!("\"name{i}\":{i},"));
        }
        expected.push_str("\"last\":0}");
        let started = std::time::Instant::now();
        assert!(canonical(&text).unwrap() == expected, "order");
        eprintln!(
            "{} bytes, {} keys: {:?}",
            text.len(),
            keys.len(),
            started.elapsed()
        );
    }

    #[test]
    fn the_tree_keeps_infinity_order_and_lone_surrogates() {
        let tree = tree(r#"{"b":1e400,"1":[-1e400,"\ud800"],"0":null,"b":2}"#).unwrap();
        assert_eq!(
            wtf16::stringify(&tree).unwrap(),
            r#"{"0":null,"1":[null,"\ud800"],"b":2}"#
        );
        let tree = super::tree(r#"{"n":1e400}"#).unwrap();
        assert_eq!(tree.get("n").as_num(), Some(f64::INFINITY));
        assert!(super::tree(&format!("\"{}\"", '\u{10FFFF}')).is_none());
        let deep = format!("{}{}", "[".repeat(TREE_DEPTH), "]".repeat(TREE_DEPTH));
        assert!(super::tree(&deep).is_some());
        let deeper = format!("[{deep}]");
        assert!(super::tree(&deeper).is_none());
        assert!(super::tree("[1,]").is_none());
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
