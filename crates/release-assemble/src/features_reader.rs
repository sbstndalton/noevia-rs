//! The strict FEATURES readers of `assemble-release.sh` in Rust (council p15, pool item 9).
//!
//! Ten shell functions read the front's `bins/noevia-server/src/lib.rs` with an inline `node -e` tokenizer and
//! answer `none`, `invalid` or a version for one lease-authority capability: `discovery_rs_version`,
//! `auto_roles_rs_owner`, `lease_rs_version`, `admission_rs_version`, `chat_mirror_rs_version`,
//! `residual_rs_version`, `source_startup_rs_version`, `dav_listener_rs_version`, `outbox_rs_version` and
//! `media_rs_version`. They share one tokenizer (comments, raw, byte and ordinary string literals, char
//! literals) and one element splitter; they differ in where the declaration may stand, which non-literal
//! entries pass and how the version is read. [`Reader`] names the ten, [`read`] answers as the shell does.
//!
//! Port notes (behaviour, not lines):
//! - JavaScript's `\s` and `trim()` set is used, not Rust's (`U+FEFF` is space, `U+0085` is not).
//! - A `\u{…}` escape naming a surrogate keeps `U+FFFD` in the decoded value. Every check only asks whether a
//!   value contains or equals an ASCII marker, so the answer is the same.
//! - A placeholder index past the literal table, or a code point above `U+10FFFF`, makes node throw; the shell
//!   then prints `invalid`, and so does this.
//! - The shell answers `invalid` when node is missing or times out; this reader needs neither.
//!
//! Nothing in the release path calls this yet; the `features-version` subcommand prints one verdict.

use std::path::Path;

/// One of the ten readers, by the shell function it ports.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reader {
    /// `discovery_rs_version`: `rust-mcp-discovery` → `v1`.
    Discovery,
    /// `auto_roles_rs_owner`: `rust-auto-roles-write` → `rust`.
    AutoRolesWrite,
    /// `lease_rs_version`: `noevia-maintenance-lease-authority/1|2` → `v1|v2`.
    MaintenanceLease,
    /// `admission_rs_version`: `noevia-inference-admission-authority/1` → `v1`.
    InferenceAdmission,
    /// `chat_mirror_rs_version`: `rust-chat-mirror-v1` → `v1`.
    ChatMirror,
    /// `residual_rs_version`: `rust-residual-fallbacks-v1` → `v1`.
    Residual,
    /// `source_startup_rs_version`: `rust-source-startup-v1` → `v1`.
    SourceStartup,
    /// `dav_listener_rs_version`: `rust-dav-listener-v1` → `v1`.
    DavListener,
    /// `outbox_rs_version`: `rust-index-outbox-v1|v2|v3` → that version.
    IndexOutbox,
    /// `media_rs_version`: `rust-media-background-v1` → `v1`.
    MediaBackground,
}

/// Every reader, in the order the script defines them.
pub const ALL: &[Reader] = &[
    Reader::Discovery,
    Reader::AutoRolesWrite,
    Reader::MaintenanceLease,
    Reader::InferenceAdmission,
    Reader::ChatMirror,
    Reader::Residual,
    Reader::SourceStartup,
    Reader::DavListener,
    Reader::IndexOutbox,
    Reader::MediaBackground,
];

/// Where the declaration may stand, as each reader checks it.
#[derive(Clone, Copy)]
enum Scope {
    /// No check (auto-roles write).
    Any,
    /// Crate root: no crate `cfg` (`#!\s*\[\s*cfg`), `([{` balanced before it, only crate inner attributes
    /// (balanced, never `cfg`) between the previous `;`/`}` and the declaration (discovery, lease, admission,
    /// chat mirror, outbox, media).
    Root,
    /// `{}` balanced, no crate `cfg`/`cfg_attr` (word-bounded), and only `#[doc = "…"]`, `#[allow(…)]` and
    /// crate inner attributes before it (source startup, DAV listener).
    InertAttrs,
    /// `{}` never negative and balanced, no `#[` in the item before it, no crate `cfg`/`cfg_attr` inside an
    /// inner attribute (residual).
    NoAttrs,
}

/// Which entries that carry no marker may stand in the array besides string literals.
#[derive(Clone, Copy)]
enum Plain {
    /// Any path (`a::b::C`).
    Path,
    /// Any path, unless the marker appears in a literal outside the array (outbox, media).
    PathUnlessAliasable,
    /// Only the two reviewed store markers (residual).
    Listed(&'static [&'static str]),
}

struct Spec {
    marker: &'static str,
    scope: Scope,
    plain: Plain,
    /// A literal containing this, anywhere, makes a missing FEATURES entry `invalid` (outbox, media).
    none_unless: Option<&'static str>,
    /// No declaration but the word FEATURES anywhere in the code is `invalid` (residual).
    word_without_decl_invalid: bool,
    /// No literal containing this: `none` before anything else (auto-roles write).
    precheck: Option<&'static str>,
    version: fn(&str) -> Option<&'static str>,
}

const RESIDUAL_STORES: &[&str] = &[
    "server_projects::research_jobs::RESEARCH_JOBS_STORE_MARKER",
    "server_projects::source_jobs::SOURCE_JOBS_STORE_MARKER",
];

impl Reader {
    /// The shell function this reader ports.
    pub fn shell_name(self) -> &'static str {
        match self {
            Reader::Discovery => "discovery_rs_version",
            Reader::AutoRolesWrite => "auto_roles_rs_owner",
            Reader::MaintenanceLease => "lease_rs_version",
            Reader::InferenceAdmission => "admission_rs_version",
            Reader::ChatMirror => "chat_mirror_rs_version",
            Reader::Residual => "residual_rs_version",
            Reader::SourceStartup => "source_startup_rs_version",
            Reader::DavListener => "dav_listener_rs_version",
            Reader::IndexOutbox => "outbox_rs_version",
            Reader::MediaBackground => "media_rs_version",
        }
    }

    /// The CLI name (`features-version NAME FILE`).
    pub fn name(self) -> &'static str {
        match self {
            Reader::Discovery => "discovery",
            Reader::AutoRolesWrite => "auto-roles-write",
            Reader::MaintenanceLease => "maintenance-lease",
            Reader::InferenceAdmission => "inference-admission",
            Reader::ChatMirror => "chat-mirror",
            Reader::Residual => "residual",
            Reader::SourceStartup => "source-startup",
            Reader::DavListener => "dav-listener",
            Reader::IndexOutbox => "index-outbox",
            Reader::MediaBackground => "media-background",
        }
    }

    /// The reader with this CLI name or shell function name.
    pub fn by_name(name: &str) -> Option<Reader> {
        ALL.iter()
            .copied()
            .find(|r| r.name() == name || r.shell_name() == name)
    }

    fn spec(self) -> Spec {
        fn none(_: &str) -> Option<&'static str> {
            None
        }
        let base = Spec {
            marker: "",
            scope: Scope::Root,
            plain: Plain::Path,
            none_unless: None,
            word_without_decl_invalid: false,
            precheck: None,
            version: none,
        };
        match self {
            Reader::Discovery => Spec {
                marker: "rust-mcp-discovery",
                version: |v| (v == "rust-mcp-discovery").then_some("v1"),
                ..base
            },
            Reader::AutoRolesWrite => Spec {
                marker: "rust-auto-roles-write",
                scope: Scope::Any,
                precheck: Some("auto-roles"),
                version: |v| (v == "rust-auto-roles-write").then_some("rust"),
                ..base
            },
            Reader::MaintenanceLease => Spec {
                marker: "noevia-maintenance-lease-authority",
                version: |v| match v {
                    "noevia-maintenance-lease-authority/1" => Some("v1"),
                    "noevia-maintenance-lease-authority/2" => Some("v2"),
                    _ => None,
                },
                ..base
            },
            Reader::InferenceAdmission => Spec {
                marker: "noevia-inference-admission-authority",
                version: |v| (v == "noevia-inference-admission-authority/1").then_some("v1"),
                ..base
            },
            Reader::ChatMirror => Spec {
                marker: "rust-chat-mirror-v",
                version: |v| (v == "rust-chat-mirror-v1").then_some("v1"),
                ..base
            },
            Reader::Residual => Spec {
                marker: "rust-residual-fallbacks",
                scope: Scope::NoAttrs,
                plain: Plain::Listed(RESIDUAL_STORES),
                word_without_decl_invalid: true,
                version: |v| (v == "rust-residual-fallbacks-v1").then_some("v1"),
                ..base
            },
            Reader::SourceStartup => Spec {
                marker: "rust-source-startup",
                scope: Scope::InertAttrs,
                version: |v| (v == "rust-source-startup-v1").then_some("v1"),
                ..base
            },
            Reader::DavListener => Spec {
                marker: "rust-dav-listener",
                scope: Scope::InertAttrs,
                version: |v| (v == "rust-dav-listener-v1").then_some("v1"),
                ..base
            },
            Reader::IndexOutbox => Spec {
                marker: "rust-index-outbox",
                plain: Plain::PathUnlessAliasable,
                none_unless: Some("rust-index-outbox-v"),
                version: |v| match v {
                    "rust-index-outbox-v1" => Some("v1"),
                    "rust-index-outbox-v2" => Some("v2"),
                    "rust-index-outbox-v3" => Some("v3"),
                    _ => None,
                },
                ..base
            },
            Reader::MediaBackground => Spec {
                marker: "rust-media-background-v",
                plain: Plain::PathUnlessAliasable,
                none_unless: Some("rust-media-background-v"),
                version: |v| (v == "rust-media-background-v1").then_some("v1"),
                ..base
            },
        }
    }
}

/// The shell's answer for `reader FILE`: `none` (no file, or no entry), `invalid`, or the version.
pub fn read(reader: Reader, file: &Path) -> &'static str {
    // `[ -f "$1" ] || { echo none; return; }`
    if !file.is_file() {
        return "none";
    }
    match std::fs::read(file) {
        Ok(bytes) => read_source(reader, &String::from_utf8_lossy(&bytes)),
        Err(_) => "invalid",
    }
}

/// The verdict for a source text (`readFileSync(…, "utf8")` already applied).
pub fn read_source(reader: Reader, src: &str) -> &'static str {
    match verdict(reader.spec(), src) {
        Ok(v) | Err(v) => v,
    }
}

const INVALID: &str = "invalid";
const NONE: &str = "none";

/// One decoded literal: `str` (an ordinary string) or `other` (raw or byte string).
struct Lit {
    plain: bool,
    v: String,
}

/// JavaScript's WhiteSpace and LineTerminator set (`\s`, `trim()`).
pub fn js_space(c: char) -> bool {
    matches!(
        c,
        '\t' | '\n' | '\u{0B}' | '\u{0C}' | '\r' | ' ' | '\u{A0}' | '\u{1680}' | '\u{2000}'
            ..='\u{200A}'
                | '\u{2028}'
                | '\u{2029}'
                | '\u{202F}'
                | '\u{205F}'
                | '\u{3000}'
                | '\u{FEFF}'
    )
}

fn js_trim(s: &str) -> &str {
    s.trim_matches(js_space)
}

fn ident(c: Option<char>) -> bool {
    matches!(c, Some(c) if c.is_ascii_alphanumeric() || c == '_')
}

fn word_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

fn find_from(s: &[char], pat: &[char], from: usize) -> Option<usize> {
    if from > s.len() {
        return None;
    }
    (from..=s.len().saturating_sub(pat.len()))
        .find(|&k| k + pat.len() <= s.len() && s[k..k + pat.len()] == *pat)
}

/// The tokenizer: comments become one space, literals a `\0<index>\0` placeholder, char literals one space.
fn lex(src: &str) -> Result<(String, Vec<Lit>), &'static str> {
    let s: Vec<char> = src.chars().collect();
    let n = s.len();
    let at = |k: usize| s.get(k).copied();
    let mut code = String::with_capacity(src.len());
    let mut strs: Vec<Lit> = Vec::new();
    let mut i = 0usize;
    fn push(code: &mut String, strs: &mut Vec<Lit>, lit: Lit) {
        strs.push(lit);
        code.push('\0');
        code.push_str(&(strs.len() - 1).to_string());
        code.push('\0');
    }
    while i < n {
        let c = s[i];
        let d = at(i + 1);
        let prev = if i == 0 { None } else { at(i - 1) };
        if c == '/' && d == Some('/') {
            while i < n && s[i] != '\n' {
                i += 1;
            }
            continue;
        }
        if c == '/' && d == Some('*') {
            let mut depth = 1;
            i += 2;
            while i < n && depth > 0 {
                if s[i] == '/' && at(i + 1) == Some('*') {
                    depth += 1;
                    i += 2;
                } else if s[i] == '*' && at(i + 1) == Some('/') {
                    depth -= 1;
                    i += 2;
                } else {
                    i += 1;
                }
            }
            if depth > 0 {
                return Err(INVALID);
            }
            code.push(' ');
            continue;
        }
        // /^b?r(#*)"/ over the next 260 code units, not after an identifier character.
        if !ident(prev) {
            let window = &s[i..n.min(i + 260)];
            let mut k = 0;
            if window.first() == Some(&'b') {
                k = 1;
            }
            if window.get(k) == Some(&'r') {
                let mut h = k + 1;
                while window.get(h) == Some(&'#') {
                    h += 1;
                }
                if window.get(h) == Some(&'"') {
                    let hashes = h - (k + 1);
                    let mut close = vec!['"'];
                    close.extend(std::iter::repeat_n('#', hashes));
                    let s0 = i + h + 1;
                    let Some(e) = find_from(&s, &close, s0) else {
                        return Err(INVALID);
                    };
                    let v: String = s[s0..e].iter().collect();
                    push(&mut code, &mut strs, Lit { plain: false, v });
                    i = e + close.len();
                    continue;
                }
            }
        }
        if c == '"' || (c == 'b' && d == Some('"') && !ident(prev)) {
            let bytes = c == 'b';
            i += if bytes { 2 } else { 1 };
            let mut v = String::new();
            while i < n && s[i] != '"' {
                if s[i] != '\\' {
                    v.push(s[i]);
                    i += 1;
                    continue;
                }
                let e = at(i + 1);
                let simple = match e {
                    Some('n') => Some('\n'),
                    Some('r') => Some('\r'),
                    Some('t') => Some('\t'),
                    Some('\\') => Some('\\'),
                    Some('0') => Some('\0'),
                    Some('\'') => Some('\''),
                    Some('"') => Some('"'),
                    _ => None,
                };
                if let Some(ch) = simple {
                    v.push(ch);
                    i += 2;
                } else if e == Some('x') && hex_escape(&s, i + 2, bytes) {
                    let hex: String = s[i + 2..i + 4].iter().collect();
                    let code_unit = u32::from_str_radix(&hex, 16).map_err(|_| INVALID)?;
                    v.push(char::from_u32(code_unit).ok_or(INVALID)?);
                    i += 4;
                } else if !bytes && e == Some('u') && at(i + 2) == Some('{') {
                    let Some(close) = find_from(&s, &['}'], i + 3) else {
                        return Err(INVALID);
                    };
                    let hex: String = s[i + 3..close].iter().filter(|&&c| c != '_').collect();
                    if hex.is_empty()
                        || hex.len() > 6
                        || !hex.chars().all(|c| c.is_ascii_hexdigit())
                    {
                        return Err(INVALID);
                    }
                    let cp = u32::from_str_radix(&hex, 16).map_err(|_| INVALID)?;
                    if cp > 0x10FFFF {
                        // String.fromCodePoint throws: node exits without output.
                        return Err(INVALID);
                    }
                    v.push(char::from_u32(cp).unwrap_or('\u{FFFD}'));
                    i = close + 1;
                } else if e == Some('\n') || e == Some('\r') {
                    i += 2;
                    while i < n && matches!(s[i], ' ' | '\t' | '\r' | '\n') {
                        i += 1;
                    }
                } else {
                    return Err(INVALID);
                }
            }
            if i >= n {
                return Err(INVALID);
            }
            i += 1;
            push(&mut code, &mut strs, Lit { plain: !bytes, v });
            continue;
        }
        if c == '\'' {
            if d == Some('\\') {
                let Some(e) = find_from(&s, &['\''], i + 3) else {
                    return Err(INVALID);
                };
                i = e + 1;
                code.push(' ');
                continue;
            }
            if at(i + 2) == Some('\'') {
                i += 3;
                code.push(' ');
                continue;
            }
        }
        code.push(c);
        i += 1;
    }
    Ok((code, strs))
}

/// `\x` escapes: two hex digits for byte strings, `[0-7][0-9A-Fa-f]` otherwise.
fn hex_escape(s: &[char], k: usize, bytes: bool) -> bool {
    let (Some(&a), Some(&b)) = (s.get(k), s.get(k + 1)) else {
        return false;
    };
    let first = if bytes {
        a.is_ascii_hexdigit()
    } else {
        ('0'..='7').contains(&a)
    };
    first && b.is_ascii_hexdigit()
}

/// Every `\0<digits>\0` in `text`, left to right without overlap, as the digit runs.
fn placeholders(text: &str) -> Vec<&str> {
    let b = text.as_bytes();
    let mut out = Vec::new();
    let mut k = 0;
    while k < b.len() {
        if b[k] == 0 {
            let mut j = k + 1;
            while j < b.len() && b[j].is_ascii_digit() {
                j += 1;
            }
            if j > k + 1 && j < b.len() && b[j] == 0 {
                out.push(&text[k + 1..j]);
                k = j + 1;
                continue;
            }
        }
        k += 1;
    }
    out
}

/// `strs[+digits]`, or `None` when node would read `undefined` (and throw on `.v` or `.kind`).
fn lit_at<'a>(strs: &'a [Lit], digits: &str) -> Option<&'a Lit> {
    digits.parse::<usize>().ok().and_then(|k| strs.get(k))
}

/// The whole of `text` is one placeholder: `/^\u0000(\d+)\u0000$/`.
fn sole_placeholder(text: &str) -> Option<&str> {
    let inner = text.strip_prefix('\0')?.strip_suffix('\0')?;
    (!inner.is_empty() && inner.bytes().all(|b| b.is_ascii_digit())).then_some(inner)
}

/// `/^(::)?[A-Za-z_][A-Za-z0-9_]*(\s*::\s*[A-Za-z_][A-Za-z0-9_]*)*$/`
fn plain_path(text: &str) -> bool {
    let rest = text.strip_prefix("::").unwrap_or(text);
    let mut parts = rest.split("::");
    let seg_ok = |seg: &str, lead: bool, trail: bool| {
        let seg = if lead {
            seg.trim_start_matches(js_space)
        } else {
            seg
        };
        let seg = if trail {
            seg.trim_end_matches(js_space)
        } else {
            seg
        };
        let mut cs = seg.chars();
        matches!(cs.next(), Some(c) if c.is_ascii_alphabetic() || c == '_')
            && cs.all(|c| c.is_ascii_alphanumeric() || c == '_')
    };
    let Some(first) = parts.next() else {
        return false;
    };
    let rest: Vec<&str> = parts.collect();
    // The first segment may not carry space (it is anchored at ^ or right after a leading ::); the space
    // allowed around each later :: belongs to the segments on either side of it.
    if !seg_ok(first, false, !rest.is_empty()) {
        return false;
    }
    let last = rest.len().saturating_sub(1);
    rest.iter()
        .enumerate()
        .all(|(k, seg)| seg_ok(seg, true, k != last))
}

/// `/pub\s+const\s+FEATURES\s*:\s*&\s*\[\s*&\s*str\s*\]\s*=\s*&\s*\[/g`: start and end of every match.
fn heads(code: &str) -> Vec<(usize, usize)> {
    let ws = |s: &str, k: usize, min: usize| -> Option<usize> {
        let mut j = k;
        let mut count = 0;
        for c in s[k..].chars() {
            if !js_space(c) {
                break;
            }
            j += c.len_utf8();
            count += 1;
        }
        (count >= min).then_some(j)
    };
    let lit = |s: &str, k: usize, t: &str| s[k..].starts_with(t).then(|| k + t.len());
    let mut out = Vec::new();
    let mut from = 0;
    while let Some(off) = code[from..].find("pub") {
        let start = from + off;
        let m = (|| {
            let mut k = start + 3;
            k = ws(code, k, 1)?;
            k = lit(code, k, "const")?;
            k = ws(code, k, 1)?;
            k = lit(code, k, "FEATURES")?;
            for t in [":", "&", "[", "&", "str", "]", "=", "&", "["] {
                k = ws(code, k, 0)?;
                k = lit(code, k, t)?;
            }
            Some(k)
        })();
        match m {
            Some(end) => {
                out.push((start, end));
                from = end;
            }
            None => from = start + 3,
        }
    }
    out
}

/// `/#!\s*\[\s*cfg/` (Root) or `/#!\s*\[\s*cfg(?:_attr)?\b/` (InertAttrs) anywhere in `code`.
fn crate_cfg(code: &str, bounded: bool) -> bool {
    let b = code.as_bytes();
    let mut from = 0;
    while let Some(off) = code[from..].find("#!") {
        let start = from + off;
        from = start + 2;
        let mut k = skip_ws(code, start + 2);
        if b.get(k) != Some(&b'[') {
            continue;
        }
        k = skip_ws(code, k + 1);
        if !code[k..].starts_with("cfg") {
            continue;
        }
        if !bounded {
            return true;
        }
        let after = k + 3;
        let bound = |at: usize| b.get(at).is_none_or(|&c| !word_byte(c));
        if bound(after) || (code[after..].starts_with("_attr") && bound(after + 5)) {
            return true;
        }
    }
    false
}

fn skip_ws(s: &str, k: usize) -> usize {
    k + s[k..]
        .chars()
        .take_while(|&c| js_space(c))
        .map(char::len_utf8)
        .sum::<usize>()
}

/// `/#\s*!\s*\[[^\]]*\bcfg(?:_attr)?\b/` (residual) anywhere in `text`.
fn inner_cfg_anywhere(text: &str) -> bool {
    let b = text.as_bytes();
    let mut from = 0;
    while let Some(off) = text[from..].find('#') {
        let start = from + off;
        from = start + 1;
        let mut k = skip_ws(text, start + 1);
        if b.get(k) != Some(&b'!') {
            continue;
        }
        k = skip_ws(text, k + 1);
        if b.get(k) != Some(&b'[') {
            continue;
        }
        let open = k + 1;
        let close = text[open..].find(']').map_or(text.len(), |c| open + c);
        // A `cfg` word-bounded on both sides at some position inside `[ … ` (before the first `]`); the
        // match may run to the end of the text when no `]` follows.
        let inside = &text[open..close];
        let ib = inside.as_bytes();
        let mut p = 0;
        while let Some(o) = inside[p..].find("cfg") {
            let at = p + o;
            let before_ok = at == 0 || !word_byte(ib[at - 1]);
            let bound = |q: usize| ib.get(q).is_none_or(|&c| !word_byte(c));
            // At the end of `inside` the next character is `]` or the end of the text: both bound.
            if before_ok
                && (bound(at + 3) || (inside[at + 3..].starts_with("_attr") && bound(at + 8)))
            {
                return true;
            }
            p = at + 1;
        }
    }
    false
}

/// `/#\s*\[/`
fn outer_attr(text: &str) -> bool {
    let b = text.as_bytes();
    text.match_indices('#')
        .any(|(k, _)| b.get(skip_ws(text, k + 1)) == Some(&b'['))
}

/// `/\bFEATURES\b/`
fn features_word(code: &str) -> bool {
    let b = code.as_bytes();
    code.match_indices("FEATURES").any(|(k, _)| {
        (k == 0 || !word_byte(b[k - 1])) && b.get(k + 8).is_none_or(|&c| !word_byte(c))
    })
}

fn last_item_start(before: &str) -> usize {
    match (before.rfind(';'), before.rfind('}')) {
        (None, None) => 0,
        (a, b) => a.max(b).map_or(0, |k| k + 1),
    }
}

/// Root scope: balanced `([{`, then only crate inner attributes before the declaration.
fn root_scope(code: &str, before: &str) -> Result<(), &'static str> {
    if crate_cfg(code, false) {
        return Err(INVALID);
    }
    let mut scope = 0i64;
    for c in before.bytes() {
        match c {
            b'(' | b'[' | b'{' => scope += 1,
            b')' | b']' | b'}' => scope -= 1,
            _ => {}
        }
    }
    if scope != 0 {
        return Err(INVALID);
    }
    let mut lead = js_trim(&before[last_item_start(before)..]);
    while let Some(after_hash) = lead.strip_prefix('#') {
        let bang = after_hash.trim_start_matches(js_space);
        let Some(after_bang) = bang.strip_prefix('!') else {
            return Err(INVALID);
        };
        let open = after_bang.trim_start_matches(js_space);
        if !open.starts_with('[') {
            return Err(INVALID);
        }
        let mut d = 0i64;
        let mut end = None;
        for (j, c) in open.bytes().enumerate() {
            match c {
                b'[' | b'(' | b'{' => d += 1,
                b']' | b')' | b'}' => {
                    d -= 1;
                    if d == 0 {
                        end = Some(j);
                        break;
                    }
                }
                _ => {}
            }
        }
        let Some(end) = end else {
            return Err(INVALID);
        };
        // /^\[\s*cfg/
        if open[skip_ws(open, 1)..].starts_with("cfg") {
            return Err(INVALID);
        }
        lead = js_trim(&open[end + 1..]);
    }
    if lead.is_empty() {
        Ok(())
    } else {
        Err(INVALID)
    }
}

/// The source startup / DAV listener tail rule:
/// `/^(?:#\s*\[\s*(?:doc\s*=\s*\u0000\d+\u0000|allow\s*\([^\[\]]*\))\s*\]\s*|#!\s*\[[^\[\]]*\]\s*)+$/`.
fn inert_attrs_only(tail: &str) -> bool {
    let b = tail.as_bytes();
    let mut k = 0;
    let mut any = false;
    while k < b.len() {
        if b[k] != b'#' {
            return false;
        }
        // Alternative 2 first where it applies: `#!` cannot start alternative 1 (`#\s*\[`).
        let j = skip_ws(tail, k + 1);
        if b.get(k + 1) == Some(&b'!') {
            let j = skip_ws(tail, k + 2);
            if b.get(j) != Some(&b'[') {
                return false;
            }
            let Some(close) = tail[j + 1..].find([']', '[']) else {
                return false;
            };
            if b[j + 1 + close] != b']' {
                return false;
            }
            k = skip_ws(tail, j + 1 + close + 1);
            any = true;
            continue;
        }
        if b.get(j) != Some(&b'[') {
            return false;
        }
        let j = skip_ws(tail, j + 1);
        let next = if tail[j..].starts_with("doc") {
            let j = skip_ws(tail, j + 3);
            if b.get(j) != Some(&b'=') {
                return false;
            }
            let j = skip_ws(tail, j + 1);
            if b.get(j) != Some(&0) {
                return false;
            }
            let digits = tail[j + 1..].bytes().take_while(u8::is_ascii_digit).count();
            if digits == 0 || b.get(j + 1 + digits) != Some(&0) {
                return false;
            }
            j + 1 + digits + 1
        } else if tail[j..].starts_with("allow") {
            // `allow\s*\([^\[\]]*\)\s*\]`: the `[^\[\]]*` backtracks to the last `)` before the first
            // bracket, which must be the `]` closing the attribute with only space between.
            let j = skip_ws(tail, j + 5);
            if b.get(j) != Some(&b'(') {
                return false;
            }
            let Some(q) = tail[j + 1..].find(['[', ']']).map(|q| j + 1 + q) else {
                return false;
            };
            if b[q] != b']' {
                return false;
            }
            let content = tail[j + 1..q].trim_end_matches(js_space);
            if !content.ends_with(')') {
                return false;
            }
            j + 1 + content.len()
        } else {
            return false;
        };
        let j = skip_ws(tail, next);
        if b.get(j) != Some(&b']') {
            return false;
        }
        k = skip_ws(tail, j + 1);
        any = true;
    }
    any
}

fn verdict(spec: Spec, src: &str) -> Result<&'static str, &'static str> {
    let (code, strs) = lex(src)?;
    if let Some(word) = spec.precheck {
        if !strs.iter().any(|s| s.v.contains(word)) {
            return Ok(NONE);
        }
    }
    let decls = heads(&code);
    if decls.len() > 1 {
        return Err(INVALID);
    }
    let Some(&(start, end)) = decls.first() else {
        return Ok(if spec.word_without_decl_invalid && features_word(&code) {
            INVALID
        } else {
            NONE
        });
    };
    let before = &code[..start];
    match spec.scope {
        Scope::Any => {}
        Scope::Root => root_scope(&code, before)?,
        Scope::InertAttrs => {
            let braces: i64 = before
                .bytes()
                .map(|c| {
                    if c == b'{' {
                        1
                    } else if c == b'}' {
                        -1
                    } else {
                        0
                    }
                })
                .sum();
            if braces != 0 || crate_cfg(&code, true) {
                return Err(INVALID);
            }
            let tail = js_trim(&before[last_item_start(before)..]);
            if !tail.is_empty() && !inert_attrs_only(tail) {
                return Err(INVALID);
            }
        }
        Scope::NoAttrs => {
            let mut scope = 0i64;
            for c in before.bytes() {
                match c {
                    b'{' => scope += 1,
                    b'}' => scope -= 1,
                    _ => {}
                }
                if scope < 0 {
                    return Err(INVALID);
                }
            }
            if scope != 0 {
                return Err(INVALID);
            }
            let item = &before[last_item_start(before)..];
            if outer_attr(item) || inner_cfg_anywhere(before) {
                return Err(INVALID);
            }
        }
    }
    let elements = split_elements(&code[end..])?;
    let aliasable = matches!(spec.plain, Plain::PathUnlessAliasable) && {
        let listed: Vec<usize> = elements
            .iter()
            .flat_map(|e| placeholders(e))
            .filter_map(|d| d.parse().ok())
            .collect();
        strs.iter()
            .enumerate()
            .any(|(k, t)| !listed.contains(&k) && t.v.contains(spec.marker))
    };
    let mut found: Vec<&str> = Vec::new();
    let count = elements.len();
    for (k, element) in elements.iter().enumerate() {
        let mut rest = js_trim(element);
        let mut gated = false;
        if rest.is_empty() && k == count - 1 {
            continue;
        }
        while rest.starts_with('#') {
            let open = rest[1..].trim_start_matches(js_space);
            if !open.starts_with('[') {
                return Err(INVALID);
            }
            let from = rest.len() - open.len();
            let mut dep = 0i64;
            let mut close = None;
            for (j, c) in rest.bytes().enumerate().skip(from) {
                if c == b'[' {
                    dep += 1;
                } else if c == b']' {
                    dep -= 1;
                    if dep == 0 {
                        close = Some(j);
                        break;
                    }
                }
            }
            let Some(close) = close else {
                return Err(INVALID);
            };
            gated = true;
            rest = js_trim(&rest[close + 1..]);
        }
        let lit = sole_placeholder(rest);
        // `.some(…)` stops at the first marked literal; an index past the table throws before that.
        let mut marked = false;
        for digits in placeholders(element) {
            let Some(l) = lit_at(&strs, digits) else {
                return Err(INVALID);
            };
            if l.v.contains(spec.marker) {
                marked = true;
                break;
            }
        }
        if let Some(digits) = lit {
            let l = lit_at(&strs, digits).ok_or(INVALID)?;
            if l.plain {
                if !marked {
                    continue;
                }
                if gated || !l.v.contains(spec.marker) {
                    return Err(INVALID);
                }
                found.push(&l.v);
                continue;
            }
        }
        let plain_ok = match spec.plain {
            Plain::Path => plain_path(rest),
            Plain::PathUnlessAliasable => !aliasable && plain_path(rest),
            Plain::Listed(list) => {
                let squeezed: String = rest.chars().filter(|&c| !js_space(c)).collect();
                list.contains(&squeezed.as_str())
            }
        };
        if !marked && plain_ok {
            continue;
        }
        return Err(INVALID);
    }
    match found.as_slice() {
        [] => match spec.none_unless {
            Some(p) if strs.iter().any(|s| s.v.contains(p)) => Err(INVALID),
            _ => Ok(NONE),
        },
        [one] => (spec.version)(one).ok_or(INVALID),
        _ => Err(INVALID),
    }
}

/// Split the array body at depth-0 commas up to its closing `]`, balancing `()`, `[]` and `{}`.
fn split_elements(body: &str) -> Result<Vec<String>, &'static str> {
    let mut elements = Vec::new();
    let mut depth = 0i64;
    let mut cur = String::new();
    let mut closed = false;
    for ch in body.chars() {
        match ch {
            '(' | '[' | '{' => depth += 1,
            ')' | '}' => depth -= 1,
            ']' => {
                if depth == 0 {
                    closed = true;
                    break;
                }
                depth -= 1;
            }
            _ => {}
        }
        if depth < 0 {
            return Err(INVALID);
        }
        if ch == ',' && depth == 0 {
            elements.push(std::mem::take(&mut cur));
        } else {
            cur.push(ch);
        }
    }
    if !closed {
        return Err(INVALID);
    }
    elements.push(cur);
    Ok(elements)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn r(reader: Reader, src: &str) -> &'static str {
        read_source(reader, src)
    }

    const DECL: &str = "pub const FEATURES: &[&str] = &[";

    #[test]
    fn discovery_reads_the_crate_root_declaration() {
        let ok = format!("//! front\n#![forbid(unsafe_code)]\nmod a;\n{DECL}\"rust-x\", \"rust-mcp-discovery\",];\n");
        assert_eq!(r(Reader::Discovery, &ok), "v1");
        assert_eq!(r(Reader::Discovery, "fn main() {}\n"), "none");
        // No entry names it: none; a second declaration: invalid.
        assert_eq!(r(Reader::Discovery, &format!("{DECL}\"rust-x\"];")), "none");
        assert_eq!(
            r(Reader::Discovery, &format!("{DECL}\"a\"];\n{DECL}\"b\"];")),
            "invalid"
        );
        // A crate cfg, an outer attribute, a module around it, a gated or doubled entry, a raw literal.
        assert_eq!(
            r(
                Reader::Discovery,
                &format!("#![cfg(test)]\n{DECL}\"rust-mcp-discovery\"];")
            ),
            "invalid"
        );
        assert_eq!(
            r(
                Reader::Discovery,
                &format!("#[allow(x)]\n{DECL}\"rust-mcp-discovery\"];")
            ),
            "invalid"
        );
        assert_eq!(
            r(
                Reader::Discovery,
                &format!("mod m {{ {DECL}\"rust-mcp-discovery\"]; }}")
            ),
            "invalid"
        );
        assert_eq!(
            r(
                Reader::Discovery,
                &format!("{DECL}#[cfg(x)] \"rust-mcp-discovery\"];")
            ),
            "invalid"
        );
        assert_eq!(
            r(
                Reader::Discovery,
                &format!("{DECL}\"rust-mcp-discovery\", \"rust-mcp-discovery\"];")
            ),
            "invalid"
        );
        assert_eq!(
            r(
                Reader::Discovery,
                &format!("{DECL}r\"rust-mcp-discovery\"];")
            ),
            "invalid"
        );
        assert_eq!(
            r(
                Reader::Discovery,
                &format!("{DECL}\"rust-mcp-discovery-v2\"];")
            ),
            "invalid"
        );
        // A named constant passes; a commented entry is gone; a gated unmarked entry passes.
        assert_eq!(
            r(
                Reader::Discovery,
                &format!("{DECL}crate::X, #[cfg(y)] \"z\", /* \"rust-mcp-discovery\" */ \"rust-mcp-discovery\"];")
            ),
            "v1"
        );
        // Escapes decode: \x2d is '-'.
        assert_eq!(
            r(
                Reader::Discovery,
                &format!("{DECL}\"rust\\x2dmcp-discovery\"];")
            ),
            "v1"
        );
        assert_eq!(
            r(
                Reader::Discovery,
                &format!("{DECL}\"rust-mcp\\u{{2d}}discovery\"];")
            ),
            "v1"
        );
        assert_eq!(
            r(Reader::Discovery, &format!("{DECL}\"bad \\q\"];")),
            "invalid"
        );
        assert_eq!(
            r(Reader::Discovery, &format!("{DECL}\"\\u{{110000}}\"];")),
            "invalid"
        );
        assert_eq!(
            r(Reader::Discovery, &format!("{DECL}\"rust-mcp-discovery\"")),
            "invalid"
        );
        assert_eq!(r(Reader::Discovery, "/* open"), "invalid");
    }

    #[test]
    fn auto_roles_needs_a_literal_first_and_ignores_scope() {
        assert_eq!(
            r(
                Reader::AutoRolesWrite,
                &format!("{DECL}\"x\"]; {DECL}\"y\"];")
            ),
            "none"
        );
        assert_eq!(
            r(
                Reader::AutoRolesWrite,
                &format!("mod m {{ {DECL}\"rust-auto-roles-write\"]; }}")
            ),
            "rust"
        );
        assert_eq!(
            r(
                Reader::AutoRolesWrite,
                &format!("{DECL}\"rust-auto-roles-write2\"];")
            ),
            "invalid"
        );
        assert_eq!(
            r(Reader::AutoRolesWrite, "const A: &str = \"auto-roles\";"),
            "none"
        );
    }

    #[test]
    fn versions_of_the_lease_and_the_outbox() {
        let one = |m: &str| format!("{DECL}\"{m}\"];");
        assert_eq!(
            r(
                Reader::MaintenanceLease,
                &one("noevia-maintenance-lease-authority/2")
            ),
            "v2"
        );
        assert_eq!(
            r(
                Reader::MaintenanceLease,
                &one("noevia-maintenance-lease-authority/3")
            ),
            "invalid"
        );
        assert_eq!(
            r(
                Reader::InferenceAdmission,
                &one("noevia-inference-admission-authority/1")
            ),
            "v1"
        );
        assert_eq!(r(Reader::ChatMirror, &one("rust-chat-mirror-v1")), "v1");
        assert_eq!(r(Reader::IndexOutbox, &one("rust-index-outbox-v3")), "v3");
        assert_eq!(
            r(Reader::MediaBackground, &one("rust-media-background-v1")),
            "v1"
        );
        // The outbox marker in a literal outside the array: a path entry may alias it.
        let alias = format!("const O: &str = \"rust-index-outbox-v2\";\n{DECL}O];");
        assert_eq!(r(Reader::IndexOutbox, &alias), "invalid");
        assert_eq!(
            r(
                Reader::Discovery,
                &format!("const O: &str = \"rust-mcp-discovery\";\n{DECL}O];")
            ),
            "none"
        );
    }

    #[test]
    fn residual_takes_only_the_listed_stores() {
        let d = |entries: &str| format!("{DECL}{entries}];");
        assert_eq!(
            r(
                Reader::Residual,
                &d("server_projects :: source_jobs::SOURCE_JOBS_STORE_MARKER, \"rust-residual-fallbacks-v1\"")
            ),
            "v1"
        );
        assert_eq!(r(Reader::Residual, &d("crate::OTHER")), "invalid");
        assert_eq!(r(Reader::Residual, "static FEATURES: u8 = 0;"), "invalid");
        assert_eq!(r(Reader::Residual, "fn f() {}"), "none");
        assert_eq!(
            r(
                Reader::Residual,
                &format!("#[doc = \"x\"] {}", d("\"rust-residual-fallbacks-v1\""))
            ),
            "invalid"
        );
        assert_eq!(
            r(
                Reader::Residual,
                &format!("#![cfg_attr(x, y)] {}", d("\"rust-residual-fallbacks-v1\""))
            ),
            "invalid"
        );
    }

    #[test]
    fn source_startup_allows_doc_and_allow_attributes() {
        let d = format!("{DECL}\"rust-source-startup-v1\"];");
        assert_eq!(
            r(
                Reader::SourceStartup,
                &format!("/// the list\n#[allow(dead_code)]\n{d}")
            ),
            "v1"
        );
        assert_eq!(
            r(
                Reader::SourceStartup,
                &format!("#[doc = \"list\"] #![x] {d}")
            ),
            "v1"
        );
        assert_eq!(
            r(Reader::SourceStartup, &format!("#[inline] {d}")),
            "invalid"
        );
        assert_eq!(
            r(Reader::SourceStartup, &format!("#![cfg_attr(a, b)] {d}")),
            "invalid"
        );
        // cfg_attrs is not cfg_attr (word bound): allowed as a crate inner attribute.
        assert_eq!(
            r(
                Reader::DavListener,
                "#![cfg_attrs] pub const FEATURES: &[&str] = &[\"rust-dav-listener-v1\"];"
            ),
            "v1"
        );
    }

    #[test]
    fn tokenizer_edges() {
        // A char literal holding a quote, a lifetime, a byte string, a raw string with hashes.
        let src = format!("fn f<'a>(x: &'a str) {{ let _ = '\"'; let _ = b\"rust-mcp-discovery\"; let _ = r#\"\"#; }}\n{DECL}\"rust-mcp-discovery\"];");
        assert_eq!(r(Reader::Discovery, &src), "v1");
        assert!(plain_path("a::b :: C"));
        assert!(plain_path("::a"));
        assert!(!plain_path("a ::"));
        assert!(!plain_path(":: a"));
        assert!(!plain_path("a b"));
        assert_eq!(placeholders("\0\x001\0x\012\0"), vec!["1", "12"]);
        assert!(js_space('\u{FEFF}') && !js_space('\u{85}'));
    }
}
