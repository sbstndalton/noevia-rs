//! A conservative Rust token-tree reader, not a Rust evaluator (port of `detect` in rs-capability.cjs). No source is
//! executed. Only a single crate-root public const/static `&[&str]` declaration is supported. Unknown attributes and
//! expressions fail closed; callers must refuse invalid results.

use std::collections::{BTreeSet, HashMap};
use std::fmt;

use crate::js;

/// The detector's answer, printed exactly as the Node CLI prints it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    V1,
    Legacy,
    Invalid(String),
}

impl Verdict {
    /// The verdict without its reason: `v1`, `legacy` or `invalid` (what the oracle suites compare).
    pub fn class(&self) -> &'static str {
        match self {
            Verdict::V1 => "v1",
            Verdict::Legacy => "legacy",
            Verdict::Invalid(_) => "invalid",
        }
    }
}

impl fmt::Display for Verdict {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Verdict::V1 => f.write_str("v1"),
            Verdict::Legacy => f.write_str("legacy"),
            Verdict::Invalid(reason) => write!(f, "invalid({reason})"),
        }
    }
}

const SOURCE_LIMIT_UTF16: usize = 8 * 1024 * 1024;
const TOKEN_LIMIT: usize = 1_000_000;
/// V8 ends deep recursion with a RangeError, which the Node detector reports as invalid. Rust would abort instead, so
/// nested braced bodies are capped well below the main-thread stack.
const SCOPE_DEPTH_LIMIT: usize = 2048;
const HARMLESS: [&str; 7] = ["allow", "warn", "deny", "forbid", "doc", "deprecated", "must_use"];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Id,
    Punct,
    Lifetime,
    Str,
    Raw,
    Byte,
    Char,
}

#[derive(Debug, Clone)]
struct Token {
    kind: Kind,
    value: String,
}

type Fail = String;

fn fail<T>(reason: &str) -> Result<T, Fail> {
    Err(reason.to_string())
}

fn ident_start(c: char) -> bool {
    c.is_ascii_alphabetic() || c == '_'
}

fn ident_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_'
}

/// `detect(source, declaration, capability, enabledFeatures)`. `enabled` is `None` when the shipped build's features
/// could not be derived: then only a queried literal under `#[cfg(feature = ...)]` refuses.
pub fn detect(source: &str, declaration: &str, capability: &str, enabled: Option<&BTreeSet<String>>) -> Verdict {
    match run(source, declaration, capability, enabled) {
        Ok(true) => Verdict::V1,
        Ok(false) => Verdict::Legacy,
        Err(reason) => Verdict::Invalid(reason),
    }
}

fn run(source: &str, declaration: &str, capability: &str, enabled: Option<&BTreeSet<String>>) -> Result<bool, Fail> {
    let valid_name = declaration.chars().next().is_some_and(ident_start) && declaration.chars().all(ident_char);
    if !valid_name || capability.is_empty() || source.encode_utf16().count() > SOURCE_LIMIT_UTF16 {
        return fail("input");
    }
    let tokens = tokenize(source)?;
    let pairs = balance(&tokens)?;
    let mut reader = Reader { tokens: &tokens, pairs: &pairs, declaration, capability, enabled, found: None };
    reader.scope(0, tokens.len(), true, 0)?;
    Ok(reader.found.unwrap_or(false))
}

struct Lexer {
    s: Vec<char>,
    i: usize,
    tokens: Vec<Token>,
}

impl Lexer {
    fn at(&self, i: usize) -> Option<char> {
        self.s.get(i).copied()
    }

    fn starts(&self, i: usize, text: &str) -> bool {
        text.chars().enumerate().all(|(k, c)| self.at(i + k) == Some(c))
    }

    fn find(&self, from: usize, text: &str) -> Option<usize> {
        (from..self.s.len()).find(|&k| self.starts(k, text))
    }

    fn slice(&self, from: usize, to: usize) -> String {
        self.s.get(from..to.min(self.s.len())).map(|cs| cs.iter().collect()).unwrap_or_default()
    }

    fn push(&mut self, kind: Kind, value: String) {
        self.tokens.push(Token { kind, value });
    }

    fn quoted(&mut self, quote: char, kind: Kind) -> Result<(), Fail> {
        self.i += 1;
        let mut value = String::new();
        while let Some(c) = self.at(self.i).filter(|&c| c != quote) {
            if c != '\\' {
                if c == '\r' && self.at(self.i + 1) == Some('\n') {
                    value.push('\n');
                    self.i += 2;
                } else {
                    if c == '\r' {
                        return fail("bare-carriage-return");
                    }
                    value.push(c);
                    self.i += 1;
                }
                continue;
            }
            self.i += 1;
            let e = self.at(self.i);
            self.i += 1;
            let simple = match e {
                Some('n') => Some('\n'),
                Some('r') => Some('\r'),
                Some('t') => Some('\t'),
                Some('0') => Some('\0'),
                Some('\\') => Some('\\'),
                Some('"') => Some('"'),
                Some('\'') => Some('\''),
                _ => None,
            };
            if let Some(ch) = simple {
                value.push(ch);
            } else if e == Some('x') {
                let hex = self.slice(self.i, self.i + 2);
                if hex.chars().count() != 2 || !hex.chars().all(|c| c.is_ascii_hexdigit()) {
                    return fail("escape");
                }
                let byte = u32::from_str_radix(&hex, 16).map_err(|_| "escape".to_string())?;
                if kind == Kind::Str && byte > 127 {
                    return fail("escape");
                }
                value.push(char::from_u32(byte).ok_or_else(|| "escape".to_string())?);
                self.i += 2;
            } else if e == Some('u') && self.at(self.i) == Some('{') && kind != Kind::Byte {
                self.i += 1;
                let Some(end) = self.find(self.i, "}") else { return fail("escape") };
                let hex: String = self.slice(self.i, end).chars().filter(|&c| c != '_').collect();
                if hex.is_empty() || hex.chars().count() > 6 || !hex.chars().all(|c| c.is_ascii_hexdigit()) {
                    return fail("escape");
                }
                let cp = u32::from_str_radix(&hex, 16).map_err(|_| "escape".to_string())?;
                let Some(ch) = char::from_u32(cp) else { return fail("escape") };
                value.push(ch);
                self.i = end + 1;
            } else if quote == '"' && (e == Some('\n') || (e == Some('\r') && self.at(self.i) == Some('\n'))) {
                if e == Some('\r') {
                    self.i += 1;
                }
                while self.at(self.i).is_some_and(|c| matches!(c, ' ' | '\t' | '\r' | '\n')) {
                    self.i += 1;
                }
            } else {
                return fail("escape");
            }
        }
        if self.at(self.i) != Some(quote) {
            return fail("unterminated-literal");
        }
        self.i += 1;
        if quote == '\'' && value.chars().count() != 1 {
            return fail("character");
        }
        self.push(kind, value);
        Ok(())
    }

    /// `^(?:br|cr|r)(#{0,255})"`: the hash count and the index of the opening quote.
    fn raw_prefix(&self) -> Option<(usize, usize)> {
        let i = self.i;
        let p = match (self.at(i), self.at(i + 1)) {
            (Some('b' | 'c'), Some('r')) => i + 2,
            (Some('r'), _) => i + 1,
            _ => return None,
        };
        let hashes = (p..self.s.len()).take_while(|&k| self.at(k) == Some('#')).count();
        (hashes <= 255 && self.at(p + hashes) == Some('"')).then_some((hashes, p + hashes))
    }

    fn ident_end(&self, from: usize) -> usize {
        (from..self.s.len()).find(|&k| !self.at(k).is_some_and(ident_char)).unwrap_or(self.s.len())
    }
}

fn tokenize(source: &str) -> Result<Vec<Token>, Fail> {
    let mut lx = Lexer { s: source.chars().collect(), i: 0, tokens: Vec::new() };
    while let Some(c) = lx.at(lx.i) {
        if js::is_space(c) {
            lx.i += 1;
            continue;
        }
        if lx.starts(lx.i, "//") {
            lx.i = lx.find(lx.i, "\n").unwrap_or(lx.s.len());
            continue;
        }
        if lx.starts(lx.i, "/*") {
            lx.i += 2;
            let mut depth = 1usize;
            while lx.i < lx.s.len() && depth > 0 {
                if lx.starts(lx.i, "/*") {
                    depth += 1;
                    lx.i += 2;
                } else if lx.starts(lx.i, "*/") {
                    depth -= 1;
                    lx.i += 2;
                } else {
                    lx.i += 1;
                }
            }
            if depth > 0 {
                return fail("unterminated-comment");
            }
            continue;
        }
        if let Some((hashes, quote_at)) = lx.raw_prefix() {
            let close = format!("\"{}", "#".repeat(hashes));
            let start = quote_at + 1;
            let Some(end) = lx.find(start, &close) else { return fail("unterminated-raw") };
            let value = lx.slice(start, end);
            lx.push(Kind::Raw, value);
            lx.i = end + close.chars().count();
            continue;
        }
        if matches!(c, 'b' | 'c') && lx.at(lx.i + 1) == Some('"') {
            lx.i += 1;
            lx.quoted('"', Kind::Byte)?;
            continue;
        }
        if c == '"' {
            lx.quoted('"', Kind::Str)?;
            continue;
        }
        if lx.starts(lx.i, "b'") {
            lx.i += 1;
            lx.quoted('\'', Kind::Byte)?;
            continue;
        }
        if c == '\'' {
            // An identifier after an apostrophe without a closing apostrophe is a lifetime or label.
            if lx.at(lx.i + 1).is_some_and(ident_start) {
                let end = lx.ident_end(lx.i + 1);
                if lx.at(end) != Some('\'') {
                    let value = lx.slice(lx.i, end);
                    lx.push(Kind::Lifetime, value);
                    lx.i = end;
                    continue;
                }
            }
            lx.quoted('\'', Kind::Char)?;
            continue;
        }
        let ident_from = if c == 'r' && lx.at(lx.i + 1) == Some('#') && lx.at(lx.i + 2).is_some_and(ident_start) {
            lx.i + 2
        } else {
            lx.i
        };
        if lx.at(ident_from).is_some_and(ident_start) {
            let end = lx.ident_end(ident_from);
            let value = lx.slice(ident_from, end);
            lx.push(Kind::Id, value);
            lx.i = end;
            continue;
        }
        if (c as u32) < 0x20 || (c as u32) >= 0x7f {
            return fail("unsupported-token");
        }
        lx.push(Kind::Punct, c.to_string());
        lx.i += 1;
        if lx.tokens.len() > TOKEN_LIMIT {
            return fail("token-limit");
        }
    }
    Ok(lx.tokens)
}

/// Balance every token tree first, including irrelevant function and macro bodies.
fn balance(tokens: &[Token]) -> Result<HashMap<usize, usize>, Fail> {
    let mut stack: Vec<usize> = Vec::new();
    let mut pairs = HashMap::new();
    for (k, t) in tokens.iter().enumerate() {
        if t.kind != Kind::Punct {
            continue;
        }
        let opener = match t.value.as_str() {
            "(" | "[" | "{" => {
                stack.push(k);
                continue;
            }
            ")" => "(",
            "]" => "[",
            "}" => "{",
            _ => continue,
        };
        let open = stack.pop();
        match open.and_then(|o| tokens.get(o).map(|t| (o, t))) {
            Some((o, t)) if t.value == opener => {
                pairs.insert(o, k);
            }
            _ => return fail("unbalanced"),
        }
    }
    if !stack.is_empty() {
        return fail("unbalanced");
    }
    Ok(pairs)
}

struct Reader<'a> {
    tokens: &'a [Token],
    pairs: &'a HashMap<usize, usize>,
    declaration: &'a str,
    capability: &'a str,
    enabled: Option<&'a BTreeSet<String>>,
    found: Option<bool>,
}

impl Reader<'_> {
    fn v(&self, k: usize) -> Option<&str> {
        self.tokens
            .get(k)
            .filter(|t| matches!(t.kind, Kind::Id | Kind::Punct | Kind::Lifetime))
            .map(|t| t.value.as_str())
    }

    fn kind(&self, k: usize) -> Option<Kind> {
        self.tokens.get(k).map(|t| t.kind)
    }

    fn close(&self, open: usize) -> Option<usize> {
        self.pairs.get(&open).copied()
    }

    /// We deliberately support only crate-root declarations. Nested targets are refused, even in unconditional
    /// modules: reachability and export semantics are not inferred.
    fn scope(&mut self, start: usize, end: usize, root: bool, depth: usize) -> Result<(), Fail> {
        if depth > SCOPE_DEPTH_LIMIT {
            return fail("Maximum call stack size exceeded");
        }
        let mut attrs: Vec<bool> = Vec::new();
        let mut crate_unsafe = false;
        let mut k = start;
        while k < end {
            if self.v(k) == Some("#") {
                let inner = self.v(k + 1) == Some("!");
                let open = k + if inner { 2 } else { 1 };
                let close = match (self.v(open), self.close(open)) {
                    (Some("["), Some(close)) => close,
                    _ => return fail("attribute"),
                };
                let unsafe_attr = !self.v(open + 1).is_some_and(|n| HARMLESS.contains(&n));
                if inner {
                    crate_unsafe |= unsafe_attr;
                } else {
                    attrs.push(unsafe_attr);
                }
                k = close + 1;
                continue;
            }
            let begin = k;
            // An item ends at its semicolon or first top-level braced body.
            let mut stop = k;
            let mut body = None;
            while stop < end && self.v(stop) != Some(";") {
                if self.v(stop) == Some("{") {
                    body = Some(stop);
                    break;
                }
                stop = match self.close(stop) {
                    Some(close) => close + 1,
                    None => stop + 1,
                };
            }
            let mut target = None;
            for j in begin..stop {
                if matches!(self.v(j), Some("const" | "static"))
                    && (self.v(j + 1) == Some(self.declaration)
                        || (self.v(j + 1) == Some("mut") && self.v(j + 2) == Some(self.declaration)))
                {
                    target = Some(j);
                }
            }
            if let Some(target) = target {
                if self.found.is_some() {
                    return fail("multiple-declarations");
                }
                if !root || crate_unsafe || attrs.iter().any(|&a| a) {
                    return fail("conditional-or-nested-declaration");
                }
                if target != begin + 1 || self.v(begin) != Some("pub") {
                    return fail("visibility");
                }
                self.found = Some(self.declaration_value(target + 2, stop)?);
            }
            match body {
                Some(open) => {
                    let close = self.close(open).ok_or_else(|| "unbalanced".to_string())?;
                    self.scope(open + 1, close, false, depth + 1)?;
                    k = close + 1;
                }
                None => k = stop + 1,
            }
            attrs.clear();
        }
        if root && self.found.is_some() && crate_unsafe {
            return fail("conditional-crate");
        }
        Ok(())
    }

    /// `: &['static] [&['static] str] = &[ elements ];` from just after the declaration's name to the item's end.
    fn declaration_value(&self, mut p: usize, stop: usize) -> Result<bool, Fail> {
        let take = |p: &mut usize, x: &str| -> Result<(), Fail> {
            let ok = self.v(*p) == Some(x);
            *p += 1;
            if ok {
                Ok(())
            } else {
                fail("declaration-shape")
            }
        };
        let skip_static = |p: &mut usize| {
            if self.kind(*p) == Some(Kind::Lifetime) && self.v(*p) == Some("'static") {
                *p += 1;
            }
        };
        take(&mut p, ":")?;
        take(&mut p, "&")?;
        skip_static(&mut p);
        take(&mut p, "[")?;
        take(&mut p, "&")?;
        skip_static(&mut p);
        for x in ["str", "]", "=", "&", "["] {
            take(&mut p, x)?;
        }
        let mut matches = 0usize;
        let mut present = false;
        while self.v(p) != Some("]") {
            if p >= stop {
                return fail("array");
            }
            let mut attributes: Vec<&[Token]> = Vec::new();
            while self.v(p) == Some("#") {
                let open = p + 1;
                let close = match (self.v(open), self.close(open)) {
                    (Some("["), Some(close)) => close,
                    _ => return fail("element-attribute"),
                };
                attributes.push(self.tokens.get(open + 1..close).unwrap_or_default());
                p = close + 1;
            }
            if self.kind(p) == Some(Kind::Str) {
                let value = self.tokens.get(p).map(|t| t.value.as_str()).unwrap_or_default();
                p += 1;
                if value == self.capability {
                    matches += 1;
                    if matches > 1 {
                        return fail("duplicate-queried-literal");
                    }
                    let cfg_feature = queried_cfg(&attributes)?;
                    present = match (cfg_feature, self.enabled) {
                        (None, _) => true,
                        (Some(_), None) => return fail("production-features-underivable"),
                        (Some(f), Some(set)) => set.contains(f),
                    };
                }
            } else {
                // Unresolved paths cannot satisfy a literal query. Never inspect their values.
                if self.v(p) == Some(":") && self.v(p + 1) == Some(":") {
                    p += 2;
                }
                if self.kind(p) != Some(Kind::Id) {
                    return fail("nonliteral-element");
                }
                p += 1;
                while self.v(p) == Some(":") && self.v(p + 1) == Some(":") {
                    p += 2;
                    if self.kind(p) != Some(Kind::Id) {
                        return fail("nonliteral-element");
                    }
                    p += 1;
                }
            }
            if self.v(p) == Some(",") {
                p += 1;
                if self.v(p) == Some("]") {
                    break;
                }
            } else if self.v(p) != Some("]") {
                return fail("element-shape");
            }
        }
        p += 1;
        if p != stop || self.v(stop) != Some(";") {
            return fail("declaration-tail");
        }
        Ok(present)
    }
}

/// Harmless attributes are allowed on the queried literal; besides them only one exact `cfg(feature = "F")`.
fn queried_cfg<'t>(attributes: &[&'t [Token]]) -> Result<Option<&'t str>, Fail> {
    let mut cfg_feature = None;
    for attr in attributes {
        let first = attr.first();
        if first.is_some_and(|t| t.kind == Kind::Id && HARMLESS.contains(&t.value.as_str())) {
            continue;
        }
        let is = |i: usize, kind: Kind, value: Option<&str>| {
            attr.get(i).is_some_and(|t| t.kind == kind && value.is_none_or(|v| t.value == v))
        };
        let exact = attr.len() == 6
            && is(0, Kind::Id, Some("cfg"))
            && is(1, Kind::Punct, Some("("))
            && is(2, Kind::Id, Some("feature"))
            && is(3, Kind::Punct, Some("="))
            && is(4, Kind::Str, None)
            && is(5, Kind::Punct, Some(")"));
        if !exact || cfg_feature.is_some() {
            return fail("queried-element-cfg");
        }
        cfg_feature = attr.get(4).map(|t| t.value.as_str());
    }
    Ok(cfg_feature)
}
