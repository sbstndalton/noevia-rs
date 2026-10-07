//! A strict, streaming, non-validating XML reader for `word/document.xml`, fused with the walk
//! `extract_docx` does over `ElementTree.fromstring(xml)`.
//!
//! No DTD and no entities: anything starting `<!` other than a comment or a CDATA section is
//! refused (the caller has already refused DOCTYPE/ENTITY text, as Python does), and the only
//! references are the five predefined entities and character references. Everything expat
//! refuses is refused; a few rare things expat accepts are refused too (see the crate docs).
//! The walk runs on the event stream, but its result is used only if the whole document parsed.

use crate::{Refusal, MAX_DEPTH, TEXT_LIMIT};
use std::collections::HashSet;

const W: &str = "http://schemas.openxmlformats.org/wordprocessingml/2006/main";
const XML_NS: &str = "http://www.w3.org/XML/1998/namespace";
const XMLNS_NS: &str = "http://www.w3.org/2000/xmlns/";

const BAD: Refusal = Refusal::XmlMalformed;

fn is_xml_char(c: char) -> bool {
    matches!(c, '\t' | '\n' | '\r' | '\u{20}'..='\u{D7FF}' | '\u{E000}'..='\u{FFFD}' | '\u{10000}'..='\u{10FFFF}')
}

fn is_s(b: u8) -> bool {
    matches!(b, b' ' | b'\t' | b'\r' | b'\n')
}

/// The walk's handling of one element (Python's `walk`).
enum Mode {
    /// Not inside the walked `w:body` (the root, its other children, their subtrees).
    Outside,
    /// Not walked: a skipped tag's subtree, a `w:t`/`w:tab`/... subtree, or past the budget.
    Skip,
    /// A `w:t` whose `.text` (the character data before its first child) is being collected.
    Capture(String),
    /// Walked; on close, emit "\n" (w:p, w:tr), "\t" (w:tc) or nothing.
    Walk(Option<&'static str>),
}

struct Frame<'a> {
    raw: &'a str,
    ns_mark: usize,
    mode: Mode,
}

struct Text {
    chunks: String,
    size: usize,
    truncated: bool,
}

impl Text {
    /// Python's `emit`: append up to the remaining budget (counted in characters).
    fn emit(&mut self, value: &str) {
        let remaining = TEXT_LIMIT.saturating_sub(self.size);
        let len = value.chars().count();
        if len > remaining {
            self.truncated = true;
        }
        if remaining > 0 {
            let cut = value
                .char_indices()
                .nth(remaining)
                .map_or(value.len(), |(i, _)| i);
            self.chunks.push_str(value.get(..cut).unwrap_or_default());
            self.size += remaining.min(len);
        }
    }

    /// Python's `walk(node)` up to the point it recurses: the mode for a visited element.
    fn visit(&mut self, w_local: Option<&str>) -> Mode {
        if self.size >= TEXT_LIMIT {
            self.truncated = true;
            return Mode::Skip;
        }
        match w_local {
            Some("del" | "moveFrom" | "instrText" | "drawing" | "pict") => Mode::Skip,
            Some("t") => Mode::Capture(String::new()),
            Some("tab") => {
                self.emit("\t");
                Mode::Skip
            }
            Some("br" | "cr") => {
                self.emit("\n");
                Mode::Skip
            }
            Some("p" | "tr") => Mode::Walk(Some("\n")),
            Some("tc") => Mode::Walk(Some("\t")),
            _ => Mode::Walk(None),
        }
    }
}

struct Parser<'a> {
    s: &'a str,
    i: usize,
    frames: Vec<Frame<'a>>,
    ns: Vec<(&'a str, Option<String>)>,
    text: Text,
    root_ok: bool,
    body_seen: bool,
}

/// The walk's result: the joined (not yet stripped) text and the truncation flag, plus whether
/// the root was `w:document` and whether it had a `w:body` child.
pub(crate) struct Walked {
    pub(crate) text: String,
    pub(crate) truncated: bool,
    pub(crate) root_ok: bool,
    pub(crate) body_seen: bool,
}

pub(crate) fn walk(xml: &[u8]) -> Result<Walked, Refusal> {
    let s = std::str::from_utf8(xml).map_err(|_| BAD)?;
    let mut p = Parser {
        s,
        i: 0,
        frames: Vec::new(),
        ns: Vec::new(),
        text: Text {
            chunks: String::new(),
            size: 0,
            truncated: false,
        },
        root_ok: false,
        body_seen: false,
    };
    p.document()?;
    Ok(Walked {
        text: p.text.chunks,
        truncated: p.text.truncated,
        root_ok: p.root_ok,
        body_seen: p.body_seen,
    })
}

impl<'a> Parser<'a> {
    fn rest(&self) -> &'a str {
        self.s.get(self.i..).unwrap_or_default()
    }

    fn peek(&self) -> Option<u8> {
        self.s.as_bytes().get(self.i).copied()
    }

    fn eat(&mut self, lit: &str) -> bool {
        if self.rest().starts_with(lit) {
            self.i += lit.len();
            true
        } else {
            false
        }
    }

    fn skip_s(&mut self) -> usize {
        let start = self.i;
        while self.peek().is_some_and(is_s) {
            self.i += 1;
        }
        self.i - start
    }

    /// Text up to (not including) `end`, which is consumed; every character must be an XML Char.
    fn until(&mut self, end: &str) -> Result<&'a str, Refusal> {
        let rest = self.rest();
        let at = rest.find(end).ok_or(BAD)?;
        let body = rest.get(..at).ok_or(BAD)?;
        if !body.chars().all(is_xml_char) {
            return Err(BAD);
        }
        self.i += at + end.len();
        Ok(body)
    }

    /// An NCName, ASCII only (stricter than expat, which takes any Unicode name).
    fn ncname(&mut self) -> Result<&'a str, Refusal> {
        let start = self.i;
        match self.peek() {
            Some(b) if b.is_ascii_alphabetic() || b == b'_' => self.i += 1,
            _ => return Err(BAD),
        }
        while self
            .peek()
            .is_some_and(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'_'))
        {
            self.i += 1;
        }
        self.s.get(start..self.i).ok_or(BAD)
    }

    /// A QName: NCName (':' NCName)?.
    fn qname(&mut self) -> Result<&'a str, Refusal> {
        let start = self.i;
        self.ncname()?;
        if self.eat(":") {
            self.ncname()?;
        }
        self.s.get(start..self.i).ok_or(BAD)
    }

    fn eq(&mut self) -> Result<(), Refusal> {
        self.skip_s();
        if !self.eat("=") {
            return Err(BAD);
        }
        self.skip_s();
        Ok(())
    }

    /// A quoted literal with no markup or references (XML declaration values).
    fn plain_literal(&mut self) -> Result<&'a str, Refusal> {
        let q = match self.peek() {
            Some(q @ (b'"' | b'\'')) => q,
            _ => return Err(BAD),
        };
        self.i += 1;
        let rest = self.rest();
        let at = rest.find(char::from(q)).ok_or(BAD)?;
        self.i += at + 1;
        rest.get(..at).ok_or(BAD)
    }

    /// `<?xml ...?>` at the very start (after an optional BOM). Only version 1.x and UTF-8.
    fn declaration(&mut self) -> Result<(), Refusal> {
        if self.skip_s() == 0 || !self.eat("version") {
            return Err(BAD);
        }
        self.eq()?;
        let v = self.plain_literal()?;
        let minor = v.strip_prefix("1.").ok_or(BAD)?;
        if minor.is_empty() || !minor.bytes().all(|b| b.is_ascii_digit()) {
            return Err(BAD);
        }
        let mut s = self.skip_s();
        if s > 0 && self.eat("encoding") {
            self.eq()?;
            // Stricter: expat also decodes declared single-byte encodings (latin-1, ...).
            if !self.plain_literal()?.eq_ignore_ascii_case("utf-8") {
                return Err(BAD);
            }
            s = self.skip_s();
        }
        if s > 0 && self.eat("standalone") {
            self.eq()?;
            if !matches!(self.plain_literal()?, "yes" | "no") {
                return Err(BAD);
            }
            self.skip_s();
        }
        if self.eat("?>") {
            Ok(())
        } else {
            Err(BAD)
        }
    }

    /// After `<?`: a processing instruction (ignored, like ElementTree's default builder).
    fn pi(&mut self) -> Result<(), Refusal> {
        let target = self.ncname()?;
        if target.eq_ignore_ascii_case("xml") {
            return Err(BAD);
        }
        if self.eat("?>") {
            return Ok(());
        }
        if self.skip_s() == 0 {
            return Err(BAD);
        }
        self.until("?>").map(|_| ())
    }

    /// After `<!--`: a comment (ignored). No `--` inside, none before the closing `>`.
    fn comment(&mut self) -> Result<(), Refusal> {
        self.until("--")?;
        if self.eat(">") {
            Ok(())
        } else {
            Err(BAD)
        }
    }

    /// After `&`: one of the five predefined entities or a character reference.
    fn reference(&mut self) -> Result<char, Refusal> {
        let rest = self.rest();
        let end = rest.bytes().take(13).position(|b| b == b';').ok_or(BAD)?;
        let body = rest.get(..end).ok_or(BAD)?;
        self.i += end + 1;
        let c = match body {
            "lt" => '<',
            "gt" => '>',
            "amp" => '&',
            "apos" => '\'',
            "quot" => '"',
            _ => {
                let (digits, radix) = if let Some(h) = body.strip_prefix("#x") {
                    (h, 16)
                } else if let Some(d) = body.strip_prefix('#') {
                    (d, 10)
                } else {
                    return Err(BAD);
                };
                if digits.is_empty() || !digits.chars().all(|c| c.is_digit(radix)) {
                    return Err(BAD);
                }
                let n = u32::from_str_radix(digits, radix).map_err(|_| BAD)?;
                char::from_u32(n).filter(|&c| is_xml_char(c)).ok_or(BAD)?
            }
        };
        Ok(c)
    }

    fn push_text(&mut self, piece: &str) {
        if let Some(Frame {
            mode: Mode::Capture(buf),
            ..
        }) = self.frames.last_mut()
        {
            buf.push_str(piece);
        }
    }

    /// Character data with line ends normalised (`\r\n` and `\r` become `\n`).
    fn push_normalised(&mut self, raw: &str) {
        if !matches!(
            self.frames.last(),
            Some(Frame {
                mode: Mode::Capture(_),
                ..
            })
        ) {
            return;
        }
        let mut out = String::with_capacity(raw.len());
        let mut chars = raw.chars().peekable();
        while let Some(c) = chars.next() {
            if c == '\r' {
                if chars.peek() == Some(&'\n') {
                    chars.next();
                }
                out.push('\n');
            } else {
                out.push(c);
            }
        }
        self.push_text(&out);
    }

    /// An attribute value after its opening quote: references expanded, whitespace normalised.
    fn attr_value(&mut self, q: u8) -> Result<String, Refusal> {
        let mut out = String::new();
        loop {
            let rest = self.rest();
            let stop = rest
                .find(|c| c == char::from(q) || c == '<' || c == '&')
                .ok_or(BAD)?;
            let run = rest.get(..stop).ok_or(BAD)?;
            if !run.chars().all(is_xml_char) {
                return Err(BAD);
            }
            let mut chars = run.chars().peekable();
            while let Some(c) = chars.next() {
                if c == '\r' && chars.peek() == Some(&'\n') {
                    chars.next();
                }
                out.push(if matches!(c, '\t' | '\n' | '\r') {
                    ' '
                } else {
                    c
                });
            }
            self.i += stop;
            match self.peek() {
                Some(b'&') => {
                    self.i += 1;
                    out.push(self.reference()?);
                }
                Some(b) if b == q => {
                    self.i += 1;
                    return Ok(out);
                }
                _ => return Err(BAD),
            }
        }
    }

    fn lookup(&self, prefix: &str) -> Result<Option<&str>, Refusal> {
        if prefix == "xml" {
            return Ok(Some(XML_NS));
        }
        for (p, uri) in self.ns.iter().rev() {
            if *p == prefix {
                return Ok(uri.as_deref());
            }
        }
        if prefix.is_empty() {
            Ok(None)
        } else {
            Err(BAD)
        }
    }

    /// After `<`: a start tag (an empty-element tag is closed at once).
    fn start_tag(&mut self) -> Result<(), Refusal> {
        let raw = self.qname()?;
        let mut attrs: Vec<(&'a str, String)> = Vec::new();
        let mut seen = HashSet::new();
        let empty = loop {
            let had_s = self.skip_s() > 0;
            if self.eat("/>") {
                break true;
            }
            if self.eat(">") {
                break false;
            }
            if !had_s {
                return Err(BAD);
            }
            let name = self.qname()?;
            self.eq()?;
            let q = match self.peek() {
                Some(q @ (b'"' | b'\'')) => q,
                _ => return Err(BAD),
            };
            self.i += 1;
            let value = self.attr_value(q)?;
            if !seen.insert(name) {
                return Err(BAD);
            }
            attrs.push((name, value));
        };
        if self.frames.len() >= MAX_DEPTH {
            return Err(Refusal::Nesting);
        }
        let ns_mark = self.ns.len();
        for (name, value) in &attrs {
            let reserved = value == XML_NS || value == XMLNS_NS;
            if *name == "xmlns" {
                if reserved {
                    return Err(BAD);
                }
                self.ns
                    .push(("", (!value.is_empty()).then(|| value.clone())));
            } else if let Some(prefix) = name.strip_prefix("xmlns:") {
                // Stricter: expat lets `xml` be re-bound to its own namespace.
                if prefix == "xml" || prefix == "xmlns" || value.is_empty() || reserved {
                    return Err(BAD);
                }
                self.ns.push((prefix, Some(value.clone())));
            }
        }
        let mut expanded = HashSet::new();
        for (name, _) in &attrs {
            if *name == "xmlns" || name.starts_with("xmlns:") {
                continue;
            }
            let key = match name.split_once(':') {
                Some((p, local)) => (self.lookup(p)?.map(str::to_owned), local),
                None => (None, *name),
            };
            if !expanded.insert(key) {
                return Err(BAD);
            }
        }
        let (prefix, local) = raw.split_once(':').unwrap_or(("", raw));
        // Stricter: elements in the `xml` namespace are refused (expat allows them).
        if prefix == "xmlns" || prefix == "xml" {
            return Err(BAD);
        }
        let in_w = self.lookup(prefix)? == Some(W);
        let depth = self.frames.len();
        let w_local = in_w.then_some(local);
        let mode = match self.frames.last_mut() {
            None => {
                self.root_ok = w_local == Some("document");
                Mode::Outside
            }
            Some(parent) => match &mut parent.mode {
                Mode::Skip => Mode::Skip,
                Mode::Capture(buf) => {
                    let captured = std::mem::take(buf);
                    parent.mode = Mode::Skip;
                    self.text.emit(&captured);
                    Mode::Skip
                }
                Mode::Walk(_) => self.text.visit(w_local),
                Mode::Outside => {
                    if depth == 1 && self.root_ok && !self.body_seen && w_local == Some("body") {
                        self.body_seen = true;
                        self.text.visit(w_local)
                    } else {
                        Mode::Outside
                    }
                }
            },
        };
        self.frames.push(Frame { raw, ns_mark, mode });
        if empty {
            self.close();
        }
        Ok(())
    }

    fn close(&mut self) {
        if let Some(frame) = self.frames.pop() {
            self.ns.truncate(frame.ns_mark);
            match frame.mode {
                Mode::Capture(buf) => self.text.emit(&buf),
                Mode::Walk(Some(post)) => self.text.emit(post),
                _ => {}
            }
        }
    }

    fn end_tag(&mut self) -> Result<(), Refusal> {
        let name = self.qname()?;
        self.skip_s();
        if !self.eat(">") || self.frames.last().map(|f| f.raw) != Some(name) {
            return Err(BAD);
        }
        self.close();
        Ok(())
    }

    /// Comments, processing instructions and white space outside the root element.
    fn misc(&mut self) -> Result<bool, Refusal> {
        self.skip_s();
        if self.eat("<!--") {
            self.comment()?;
            Ok(true)
        } else if self.eat("<?") {
            self.pi()?;
            Ok(true)
        } else {
            Ok(false)
        }
    }

    fn document(&mut self) -> Result<(), Refusal> {
        self.eat("\u{feff}");
        if self.rest().starts_with("<?xml")
            && self
                .s
                .as_bytes()
                .get(self.i + 5)
                .is_some_and(|&b| is_s(b) || b == b'?')
        {
            self.i += 5;
            self.declaration()?;
        }
        while self.misc()? {}
        if !self.eat("<") {
            return Err(BAD);
        }
        self.start_tag()?;
        while !self.frames.is_empty() {
            self.content()?;
        }
        while self.misc()? {}
        if self.i == self.s.len() {
            Ok(())
        } else {
            Err(BAD)
        }
    }

    /// One piece of element content.
    fn content(&mut self) -> Result<(), Refusal> {
        match self.peek() {
            None => Err(BAD),
            Some(b'<') => {
                if self.eat("</") {
                    self.end_tag()
                } else if self.eat("<!--") {
                    self.comment()
                } else if self.eat("<![CDATA[") {
                    let body = self.until("]]>")?;
                    self.push_normalised(body);
                    Ok(())
                } else if self.eat("<?") {
                    self.pi()
                } else if self.rest().starts_with("<!") {
                    Err(BAD)
                } else {
                    self.i += 1;
                    self.start_tag()
                }
            }
            Some(b'&') => {
                self.i += 1;
                let c = self.reference()?;
                let mut buf = [0u8; 4];
                self.push_text(c.encode_utf8(&mut buf));
                Ok(())
            }
            Some(_) => {
                let rest = self.rest();
                let stop = rest.find(['<', '&']).unwrap_or(rest.len());
                let run = rest.get(..stop).ok_or(BAD)?;
                if run.contains("]]>") || !run.chars().all(is_xml_char) {
                    return Err(BAD);
                }
                self.i += stop;
                self.push_normalised(run);
                Ok(())
            }
        }
    }
}
