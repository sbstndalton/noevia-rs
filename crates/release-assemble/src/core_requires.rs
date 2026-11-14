//! `core_requires FILE MODULE` (council p12): whether a Core module really calls `require('<module>')`.
//!
//! The shell runs a JavaScript tokenizer under `node -e` for this, so every pairing that asks it needs node on
//! the release host. This is the same tokenizer in Rust, over the same UTF-16 code units node reads:
//! line and block comments, `'…'` and `"…"` strings, template literals with nested `${…}`, regex literals (a
//! slash after an operator, an opening bracket or a keyword), identifiers and numbers. Only the token run
//! `require ( <module> )` counts, where `require` is not a property (`x.require`) and `<module>` is a literal
//! whose value, escapes decoded as node decodes them, is the module: a quoted string or a template without
//! substitutions.
//!
//! The shell's literal decoding is `vm.runInNewContext(literal)`: a quoted string is read with the sloppy-mode
//! escapes (legacy octal `\101`, `\8`), a template with the template rules (no octal, CR and CRLF read as LF).
//! A literal without a backslash is taken as it is written (no check at all, as `lit.slice(1, -1)` does).
//!
//! Verdicts: [`Requires::Invalid`] for an unterminated comment, string, template or regex, an undecodable
//! literal or an unreadable file (callers refuse it); [`Requires::No`] for a missing file. Named differences:
//! the shell also answers `invalid` when node is missing or the 10 s timeout ends; the port needs neither.
//! A file that is not UTF-8 is decoded with U+FFFD for each maximal bad subsequence, as node's `utf8` does.

use std::fs;
use std::path::Path;

/// The three answers the shell prints.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Requires {
    Yes,
    No,
    Invalid,
}

impl Requires {
    /// The word the shell prints (`yes`, `no`, `invalid`).
    pub fn word(self) -> &'static str {
        match self {
            Requires::Yes => "yes",
            Requires::No => "no",
            Requires::Invalid => "invalid",
        }
    }
}

/// `core_requires FILE MODULE` over a path: `No` when it is not a regular file (`[ -f ]`), `Invalid` when it
/// cannot be read.
pub fn core_requires_file(path: &Path, want: &str) -> Requires {
    if !path.is_file() {
        return Requires::No;
    }
    match fs::read(path) {
        Ok(bytes) => core_requires(&bytes, want),
        Err(_) => Requires::Invalid,
    }
}

/// `core_requires` over a file's bytes.
pub fn core_requires(bytes: &[u8], want: &str) -> Requires {
    let mut s: Vec<u16> = String::from_utf8_lossy(bytes).encode_utf16().collect();
    if s.first() == Some(&0xfeff) {
        s.remove(0);
    }
    let want: Vec<u16> = want.encode_utf16().collect();
    match tokens(s) {
        Err(()) => Requires::Invalid,
        Ok(toks) => {
            if calls(&toks, &want) {
                Requires::Yes
            } else {
                Requires::No
            }
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Tok {
    /// One punctuation code unit (`(` and `)` also stand for a template's `${` and its closing `}`).
    P(u16),
    /// A string value: a quoted string or a whole template without substitutions.
    S(Vec<u16>),
    /// The tail of a template with substitutions.
    T,
    /// A regex literal.
    R,
    Id(Vec<u16>),
    N,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Frame {
    Brace,
    Template,
}

const BACKSLASH: u16 = b'\\' as u16;
const NL: u16 = b'\n' as u16;
const CR: u16 = b'\r' as u16;
const LS: u16 = 0x2028;
const PS: u16 = 0x2029;

fn u(c: u8) -> u16 {
    u16::from(c)
}

/// JavaScript's `\s` over one code unit.
fn js_space(c: u16) -> bool {
    matches!(
        c,
        0x09..=0x0d
            | 0x20
            | 0xa0
            | 0x1680
            | 0x2000..=0x200a
            | 0x2028
            | 0x2029
            | 0x202f
            | 0x205f
            | 0x3000
            | 0xfeff
    )
}

fn ascii_letter(c: u16) -> bool {
    (u(b'A')..=u(b'Z')).contains(&c) || (u(b'a')..=u(b'z')).contains(&c)
}

fn digit(c: u16) -> bool {
    (u(b'0')..=u(b'9')).contains(&c)
}

/// `\w`: `[A-Za-z0-9_]`.
fn word(c: u16) -> bool {
    ascii_letter(c) || digit(c) || c == u(b'_')
}

fn ident_start(c: u16) -> bool {
    ascii_letter(c) || c == u(b'_') || c == u(b'$') || c >= 0x80
}

fn ident_part(c: u16) -> bool {
    word(c) || c == u(b'$') || c >= 0x80
}

const KEYWORDS: [&str; 14] = [
    "return",
    "typeof",
    "instanceof",
    "in",
    "of",
    "new",
    "delete",
    "void",
    "throw",
    "case",
    "do",
    "else",
    "yield",
    "await",
];

fn keyword(v: &[u16]) -> bool {
    KEYWORDS
        .iter()
        .any(|k| k.encode_utf16().eq(v.iter().copied()))
}

struct Lexer {
    s: Vec<u16>,
    i: usize,
    toks: Vec<Tok>,
    stack: Vec<Frame>,
}

impl Lexer {
    fn at(&self, k: usize) -> Option<u16> {
        self.s.get(k).copied()
    }

    fn slice(&self, from: usize, to: usize) -> Vec<u16> {
        self.s.get(from..to).unwrap_or_default().to_vec()
    }

    /// A slash starts a regex after an operator, an opening bracket or a keyword; after a value it divides.
    fn regex_ok(&self) -> bool {
        match self.toks.last() {
            None => true,
            Some(Tok::P(v)) => *v != u(b')') && *v != u(b']'),
            Some(Tok::Id(v)) => keyword(v),
            Some(_) => false,
        }
    }

    /// Template text from `i` (just past the backtick, or the closing brace of `${…}` when `first` is false)
    /// to its end or the next `${`. A whole template without substitutions is a string value.
    fn template(&mut self, first: bool) -> Result<(), ()> {
        let start = self.i.saturating_sub(1);
        loop {
            let c = self.at(self.i).ok_or(())?;
            if c == BACKSLASH {
                self.i += 2;
                continue;
            }
            if c == u(b'`') {
                self.i += 1;
                let tok = if first {
                    Tok::S(cooked(&self.slice(start, self.i))?)
                } else {
                    Tok::T
                };
                self.toks.push(tok);
                return Ok(());
            }
            if c == u(b'$') && self.at(self.i + 1) == Some(u(b'{')) {
                self.i += 2;
                self.stack.push(Frame::Template);
                self.toks.push(Tok::P(u(b'(')));
                return Ok(());
            }
            self.i += 1;
        }
    }

    fn run(&mut self) -> Result<(), ()> {
        let n = self.s.len();
        if self.at(0) == Some(u(b'#')) && self.at(1) == Some(u(b'!')) {
            while self.i < n && self.at(self.i) != Some(NL) {
                self.i += 1;
            }
        }
        while let Some(c) = self.at(self.i) {
            let next = self.at(self.i + 1);
            if js_space(c) {
                self.i += 1;
                continue;
            }
            if c == u(b'/') && next == Some(u(b'/')) {
                while self.i < n && self.at(self.i) != Some(NL) {
                    self.i += 1;
                }
                continue;
            }
            if c == u(b'/') && next == Some(u(b'*')) {
                let mut e = self.i + 2;
                loop {
                    match (self.at(e), self.at(e + 1)) {
                        (Some(a), Some(b)) if a == u(b'*') && b == u(b'/') => break,
                        (Some(_), Some(_)) => e += 1,
                        _ => return Err(()),
                    }
                }
                self.i = e + 2;
                continue;
            }
            if c == u(b'\'') || c == u(b'"') {
                let mut j = self.i + 1;
                loop {
                    let d = self.at(j).ok_or(())?;
                    if d == NL {
                        return Err(());
                    }
                    if d == BACKSLASH {
                        j += 2;
                        continue;
                    }
                    if d == c {
                        break;
                    }
                    j += 1;
                }
                let value = cooked(&self.slice(self.i, j + 1))?;
                self.toks.push(Tok::S(value));
                self.i = j + 1;
                continue;
            }
            if c == u(b'`') {
                self.i += 1;
                self.template(true)?;
                continue;
            }
            if c == u(b'{') {
                self.stack.push(Frame::Brace);
                self.toks.push(Tok::P(c));
                self.i += 1;
                continue;
            }
            if c == u(b'}') {
                if self.stack.last() == Some(&Frame::Template) {
                    self.stack.pop();
                    self.toks.push(Tok::P(u(b')')));
                    self.i += 1;
                    self.template(false)?;
                    continue;
                }
                self.stack.pop();
                self.toks.push(Tok::P(c));
                self.i += 1;
                continue;
            }
            if c == u(b'/') && self.regex_ok() {
                let mut j = self.i + 1;
                let mut class = false;
                loop {
                    let d = self.at(j).ok_or(())?;
                    if d == NL {
                        return Err(());
                    }
                    if d == BACKSLASH {
                        j += 2;
                        continue;
                    }
                    if class {
                        if d == u(b']') {
                            class = false;
                        }
                        j += 1;
                        continue;
                    }
                    if d == u(b'[') {
                        class = true;
                    } else if d == u(b'/') {
                        break;
                    }
                    j += 1;
                }
                j += 1;
                while self.at(j).is_some_and(ascii_letter) {
                    j += 1;
                }
                self.toks.push(Tok::R);
                self.i = j;
                continue;
            }
            if ident_start(c) {
                let mut j = self.i + 1;
                while self.at(j).is_some_and(ident_part) {
                    j += 1;
                }
                let v = self.slice(self.i, j);
                self.toks.push(Tok::Id(v));
                self.i = j;
                continue;
            }
            if digit(c) || (c == u(b'.') && next.is_some_and(digit)) {
                let mut j = self.i + 1;
                while self.at(j).is_some_and(|d| word(d) || d == u(b'.')) {
                    j += 1;
                }
                self.toks.push(Tok::N);
                self.i = j;
                continue;
            }
            self.toks.push(Tok::P(c));
            self.i += 1;
        }
        Ok(())
    }
}

fn tokens(s: Vec<u16>) -> Result<Vec<Tok>, ()> {
    let mut lx = Lexer {
        s,
        i: 0,
        toks: Vec::new(),
        stack: Vec::new(),
    };
    lx.run()?;
    Ok(lx.toks)
}

/// The token run `require ( "<want>" )` with `require` not after a `.`.
fn calls(toks: &[Tok], want: &[u16]) -> bool {
    let require: Vec<u16> = "require".encode_utf16().collect();
    toks.windows(4).enumerate().any(|(k, w)| {
        let before_dot = k
            .checked_sub(1)
            .and_then(|b| toks.get(b))
            .is_some_and(|t| *t == Tok::P(u(b'.')));
        matches!(w, [Tok::Id(a), Tok::P(b), Tok::S(m), Tok::P(e)]
            if *a == require && *b == u(b'(') && m.as_slice() == want && *e == u(b')'))
            && !before_dot
    })
}

/// The value of one complete string or template literal (its delimiters included), as node reads it; `Err`
/// where node throws.
fn cooked(lit: &[u16]) -> Result<Vec<u16>, ()> {
    let body = lit
        .get(1..lit.len().saturating_sub(1))
        .unwrap_or_default();
    if !lit.contains(&BACKSLASH) {
        return Ok(body.to_vec());
    }
    let template = lit.first() == Some(&u(b'`'));
    decode(body, template)
}

fn hex(c: u16) -> Option<u32> {
    char::from_u32(u32::from(c)).and_then(|c| c.to_digit(16))
}

fn octal(c: Option<u16>) -> Option<u32> {
    c.filter(|c| (u(b'0')..=u(b'7')).contains(c))
        .map(|c| u32::from(c - u(b'0')))
}

fn push_code_point(out: &mut Vec<u16>, cp: u32) -> Result<(), ()> {
    if cp <= 0xffff {
        out.push(u16::try_from(cp).map_err(|_| ())?);
        return Ok(());
    }
    if cp > 0x10ffff {
        return Err(());
    }
    let v = cp - 0x10000;
    out.push(u16::try_from(0xd800 + (v >> 10)).map_err(|_| ())?);
    out.push(u16::try_from(0xdc00 + (v & 0x3ff)).map_err(|_| ())?);
    Ok(())
}

/// The body of a literal with at least one backslash: string rules (sloppy mode) or template rules.
fn decode(body: &[u16], template: bool) -> Result<Vec<u16>, ()> {
    let mut out = Vec::with_capacity(body.len());
    let mut i = 0usize;
    let at = |k: usize| body.get(k).copied();
    while let Some(c) = at(i) {
        if c != BACKSLASH {
            if c == CR {
                // A raw CR ends a string literal (a syntax error); a template reads CR and CRLF as LF.
                if !template {
                    return Err(());
                }
                out.push(NL);
                i += if at(i + 1) == Some(NL) { 2 } else { 1 };
                continue;
            }
            out.push(c);
            i += 1;
            continue;
        }
        let e = at(i + 1).ok_or(())?;
        i += 2;
        match e {
            NL | LS | PS => {}
            CR => {
                if at(i) == Some(NL) {
                    i += 1;
                }
            }
            _ if e == u(b'b') => out.push(0x08),
            _ if e == u(b't') => out.push(0x09),
            _ if e == u(b'n') => out.push(0x0a),
            _ if e == u(b'v') => out.push(0x0b),
            _ if e == u(b'f') => out.push(0x0c),
            _ if e == u(b'r') => out.push(0x0d),
            _ if e == u(b'x') => {
                let (a, b) = (at(i).and_then(hex), at(i + 1).and_then(hex));
                let (Some(a), Some(b)) = (a, b) else {
                    return Err(());
                };
                push_code_point(&mut out, a * 16 + b)?;
                i += 2;
            }
            _ if e == u(b'u') => {
                if at(i) == Some(u(b'{')) {
                    let mut j = i + 1;
                    let mut cp: u32 = 0;
                    let mut any = false;
                    while let Some(h) = at(j).and_then(hex) {
                        cp = cp.saturating_mul(16).saturating_add(h).min(0x11_0000);
                        any = true;
                        j += 1;
                    }
                    if !any || at(j) != Some(u(b'}')) {
                        return Err(());
                    }
                    push_code_point(&mut out, cp)?;
                    i = j + 1;
                } else {
                    let mut cp: u32 = 0;
                    for k in 0..4 {
                        cp = cp * 16 + at(i + k).and_then(hex).ok_or(())?;
                    }
                    push_code_point(&mut out, cp)?;
                    i += 4;
                }
            }
            _ if digit(e) => {
                let next_is_digit = at(i).is_some_and(digit);
                if e == u(b'0') && !next_is_digit {
                    out.push(0);
                } else if template {
                    // Untagged templates have no octal escapes and no `\8`/`\9`.
                    return Err(());
                } else if e == u(b'8') || e == u(b'9') {
                    out.push(e);
                } else if e == u(b'0') && octal(at(i)).is_none() {
                    // `\0` before 8 or 9: NUL, the digit follows as itself.
                    out.push(0);
                } else {
                    let mut v = u32::from(e - u(b'0'));
                    let more = if v <= 3 { 2 } else { 1 };
                    for _ in 0..more {
                        match octal(at(i)) {
                            Some(d) => {
                                v = v * 8 + d;
                                i += 1;
                            }
                            None => break,
                        }
                    }
                    push_code_point(&mut out, v)?;
                }
            }
            _ => out.push(e),
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn r(src: &str) -> Requires {
        core_requires(src.as_bytes(), "./m.cjs")
    }

    #[test]
    fn plain_and_property_calls() {
        assert_eq!(r("const m = require('./m.cjs');"), Requires::Yes);
        assert_eq!(r("const m = require(\"./m.cjs\");"), Requires::Yes);
        assert_eq!(r("x.require('./m.cjs')"), Requires::No);
        assert_eq!(r("require('./m.cjs', 1)"), Requires::No);
        assert_eq!(r("require ( './m.cjs' )"), Requires::Yes);
        assert_eq!(r("require('./other.cjs')"), Requires::No);
        assert_eq!(r(""), Requires::No);
    }

    #[test]
    fn comments_strings_templates_do_not_count() {
        assert_eq!(r("// require('./m.cjs')"), Requires::No);
        assert_eq!(r("/* require('./m.cjs') */"), Requires::No);
        assert_eq!(r("'require(\\'./m.cjs\\')'"), Requires::No);
        assert_eq!(r("`require('./m.cjs')`"), Requires::No);
        assert_eq!(r("`${require('./m.cjs')}`"), Requires::Yes);
        assert_eq!(r("require(`./m.cjs`)"), Requires::Yes);
        assert_eq!(r("require(`./${x}.cjs`)"), Requires::No);
    }

    #[test]
    fn escapes_are_decoded() {
        assert_eq!(r("require('\\x2e/m.cjs')"), Requires::Yes);
        assert_eq!(r("require('\\u002e/m.cjs')"), Requires::Yes);
        assert_eq!(r("require('\\u{2e}/m.cjs')"), Requires::Yes);
        assert_eq!(r("require('\\056/m.cjs')"), Requires::Yes);
        assert_eq!(r("require('.\\/m.cjs')"), Requires::Yes);
        assert_eq!(r("require('./m\\\n.cjs')"), Requires::Yes);
        assert_eq!(r("require(`\\056/m.cjs`)"), Requires::Invalid);
        assert_eq!(r("require('\\x2/m.cjs')"), Requires::Invalid);
        assert_eq!(r("require('\\u{110000}')"), Requires::Invalid);
    }

    #[test]
    fn regex_or_division() {
        assert_eq!(r("x = /require('.\\/m.cjs')/; "), Requires::No);
        assert_eq!(r("a / b; require('./m.cjs') / 2"), Requires::Yes);
        assert_eq!(r("return /[/]/g; require('./m.cjs')"), Requires::Yes);
        assert_eq!(r("x = /abc"), Requires::Invalid);
    }

    #[test]
    fn unterminated_is_invalid() {
        assert_eq!(r("/* open"), Requires::Invalid);
        assert_eq!(r("'open"), Requires::Invalid);
        assert_eq!(r("'a\nb'"), Requires::Invalid);
        assert_eq!(r("`open"), Requires::Invalid);
        // An open `${` at the end is not checked: the loop just ends.
        assert_eq!(r("`${x"), Requires::No);
    }

    #[test]
    fn bom_shebang_and_cr() {
        assert_eq!(r("\u{feff}require('./m.cjs')"), Requires::Yes);
        assert_eq!(r("#!/usr/bin/env node\nrequire('./m.cjs')"), Requires::Yes);
        // A raw CR in a quoted string is only checked when the literal has a backslash.
        assert_eq!(r("'a\rb'; require('./m.cjs')"), Requires::Yes);
        assert_eq!(r("'a\r\\nb'; require('./m.cjs')"), Requires::Invalid);
    }
}
