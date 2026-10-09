//! noevia-core's `server/provider-egress.cjs` in Rust, exported from `dav-parse.wasm`
//! (PROVIDER_EGRESS_IMPL): what may leave the server through an EXTERNAL provider (#447).
//!
//! - [`external`]: `isExternalProvider` / `isTrialTermsHost` (a ChatGPT sign-in row, a row the
//!   server flagged external, or a custom provider whose URL host is a trial-terms service).
//! - [`egress_refusal`]: `egressRefusal`, Diary text never goes to an external provider.
//! - [`private_toolboxes`]: `stripPrivateToolboxes`, the indices of the private toolboxes removed.
//! - [`tool_refusal`]: `toolRefusal`, Diary tools by name and every path-like argument of a storage
//!   tool that is in (or, for tree tools, contains) the Diary folder; fails closed when the folder
//!   cannot be worked out.
//! - [`canonical_path`] and [`diary_folder`]: `canonicalPath` and `diaryFolderFor`.
//!
//! The host (provider-egress.cjs under PROVIDER_EGRESS_IMPL=wasm) computes the JS answer first and
//! asks this port only when the JS lets something out; the port can then only take more away. So
//! wherever the port cannot be sure it answers the strict way.
//!
//! # Where the port is stricter than the JS (by design)
//!
//! - **Unicode in paths.** The JS NFC-normalizes a path, percent-decodes it up to three times and
//!   lowercases it. The port carries no normalization or case tables of its own beyond the Rust
//!   standard library's: it canonicalizes a path only when the raw text and its fully decoded text
//!   are made of NFC-inert code points (`project_file_names::NFC_INERT_RANGES`: Latin up to U+02FF,
//!   Cyrillic, CJK, kana, Hangul syllables, general punctuation, fullwidth forms, common emoji),
//!   where NFC is the identity and lowercasing is context-free. noevia-core's differential test
//!   checks every one of those code points, raw and percent-encoded, against the runtime's ICU.
//!   Any other path is *unknown*: a storage call with an unknown path argument is refused as if it
//!   were in the Diary folder, and an unknown Diary folder closes storage for external providers.
//!   (This also closes noevia#1208's decomposed, percent-encoded bypass under wasm.)
//! - **Unicode or percent signs in a provider URL**, or an `xn--` host label: whether the host is a
//!   trial-terms service is unknown, so the provider counts as external.
//! - **A storage connection URL with non-ASCII text**: the Diary folder is unknown (storage closed).
//! - Requests over [`MAX_INPUT_BYTES`] (`too_large`), which the host treats as a refusal.
//!
//! Linear in the input (the DAV-URL patterns are matched with precomputed next-slash and
//! line-terminator tables, never by backtracking; see noevia#1209), bounded recursion (four
//! levels, the JS's own `depth > 3` limit), no panics.

#![forbid(unsafe_code)]

use project_file_names::is_nfc_inert;
use prompt_framing::js::{decode_uri_component, to_lower, trim, units};
use prompt_framing::json::{self, Value};

/// The largest request [`call`] accepts (the op byte and the JSON).
pub const MAX_INPUT_BYTES: usize = 8 * 1024 * 1024 + 1;

/// JSON nesting kept below a tool call's arguments. `pathArguments` reads containers at most
/// eight levels down (depth 3, each step an object value or an array element of one), so deeper
/// containers can never contribute a path.
const ARGS_CAP: usize = 16;

/// `TRIAL_TERMS_HOSTS`.
pub const TRIAL_TERMS_HOSTS: [&str; 1] = ["nvidia.com"];
/// `PRIVATE_TOOLBOXES`.
pub const PRIVATE_TOOLBOXES: [&str; 1] = ["diary"];

/// Why the port gives no answer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refusal {
    Input,
    TooLarge,
}

impl Refusal {
    /// The refusal reply.
    pub fn json(self) -> &'static str {
        match self {
            Refusal::Input => r#"{"error":"input"}"#,
            Refusal::TooLarge => r#"{"error":"too_large"}"#,
        }
    }
}

/// The host's projection of a provider row.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Provider {
    /// `provider.kind` when it is a string.
    pub kind: Option<Vec<u16>>,
    /// `provider.external === true`.
    pub external: bool,
    /// `String(provider.baseUrl)`.
    pub base_url: Vec<u16>,
    /// `String(provider.label)` when it is truthy, else empty.
    pub label: Vec<u16>,
}

/// The host's projection of a storage connection.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Storage {
    /// `storage.kind` when it is a string.
    pub kind: Option<Vec<u16>>,
    /// `String(storage.corpusRoot || '')`.
    pub corpus_root: Vec<u16>,
    /// `String(storage.baseUrl || '')`.
    pub base_url: Vec<u16>,
}

fn starts_with(s: &[u16], prefix: &str) -> bool {
    let mut it = s.iter();
    prefix.encode_utf16().all(|c| it.next() == Some(&c))
}

fn eq_str(s: &[u16], t: &str) -> bool {
    s.iter().copied().eq(t.encode_utf16())
}

fn fold(c: u16) -> u16 {
    if (0x41..=0x5a).contains(&c) {
        c + 0x20
    } else {
        c
    }
}

/// `s[at..]` starts with the ASCII `lit` under a non-Unicode `/i` regex (only ASCII letters fold).
fn ci_at(s: &[u16], at: usize, lit: &str) -> bool {
    let Some(rest) = s.get(at..) else {
        return false;
    };
    let mut it = rest.iter();
    lit.bytes()
        .all(|b| it.next().is_some_and(|&c| fold(c) == fold(u16::from(b))))
}

fn ci_contains(s: &[u16], lit: &str) -> bool {
    (0..s.len()).any(|i| ci_at(s, i, lit))
}

// ── isExternalProvider / isTrialTermsHost ───────────────────────────────────

/// `isTrialTermsHost` on `String(baseUrl)`: `None` when the port cannot be sure (non-ASCII text,
/// a percent sign, or an `xn--` host label, where Node's URL parser and the `url` crate may follow
/// different IDNA tables).
pub fn trial_terms_host(base_url: &[u16]) -> Option<bool> {
    if base_url.iter().any(|&c| c >= 0x80 || c == u16::from(b'%')) || ci_contains(base_url, "xn--")
    {
        return None;
    }
    let text: String = base_url
        .iter()
        .map(|&c| char::from(u8::try_from(c).unwrap_or(b'?')))
        .collect();
    let Ok(url) = url::Url::parse(&text) else {
        return Some(false);
    };
    let host = url.host_str().unwrap_or("").to_ascii_lowercase();
    if host.split('.').any(|l| l.starts_with("xn--")) {
        return None;
    }
    let host = host.trim_end_matches('.');
    Some(
        !host.is_empty()
            && TRIAL_TERMS_HOSTS.iter().any(|h| {
                host == *h || host.strip_suffix(h).is_some_and(|head| head.ends_with('.'))
            }),
    )
}

/// `isTrialTermsHost(provider)` (`None`: unknown, see [`trial_terms_host`]).
pub fn trial(provider: Option<&Provider>) -> Option<bool> {
    match provider {
        Some(p) => trial_terms_host(&p.base_url),
        // hostOf(null) is '' (String(null) is not a URL).
        None => Some(false),
    }
}

/// `isExternalProvider(provider)` (`None`: unknown).
pub fn external(provider: Option<&Provider>) -> Option<bool> {
    let Some(p) = provider else {
        return Some(false);
    };
    if p.external
        || p.kind
            .as_deref()
            .is_some_and(|k| eq_str(k, "chatgpt-oauth"))
    {
        return Some(true);
    }
    trial_terms_host(&p.base_url)
}

/// [`external`], unknown counted as external.
pub fn external_strict(provider: Option<&Provider>) -> bool {
    external(provider).unwrap_or(true)
}

// ── egressRefusal / stripPrivateToolboxes ───────────────────────────────────

fn truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Num(n) => *n != 0.0 && !n.is_nan(),
        Value::Str(s) => !s.is_empty(),
        Value::Arr(_) | Value::Obj(_) | Value::Deep => true,
    }
}

/// `a === b` for the host's projections (primitives; containers never compare equal, since the
/// JS compares them by identity, which a projection cannot carry).
fn strict_eq(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Null, Value::Null) => true,
        (Value::Bool(x), Value::Bool(y)) => x == y,
        (Value::Num(x), Value::Num(y)) => x == y,
        (Value::Str(x), Value::Str(y)) => x == y,
        _ => false,
    }
}

fn cat(parts: &[&[u16]]) -> Vec<u16> {
    parts.iter().flat_map(|p| p.iter().copied()).collect()
}

/// `egressRefusal({ provider, spaceId, projectId, diaryProjectId })`.
pub fn egress_refusal(
    provider: Option<&Provider>,
    space_id: &Value,
    project_id: &Value,
    diary_project_id: &Value,
) -> Option<Vec<u16>> {
    if !external_strict(provider) {
        return None;
    }
    let diary_space = matches!(space_id, Value::Str(s) if starts_with(s, "diary"));
    let diary_project = truthy(diary_project_id) && strict_eq(project_id, diary_project_id);
    if !(diary_space || diary_project) {
        return None;
    }
    let label = match provider {
        Some(p) if !p.label.is_empty() => p.label.clone(),
        _ => units("ChatGPT"),
    };
    Some(cat(&[
        &units("Diary text is never sent to an external provider ("),
        &label,
        &units("). Choose a local model for Diary attachments and tools."),
    ]))
}

/// `stripPrivateToolboxes(selected, provider)`: the indices (ascending) of the entries removed.
pub fn private_toolboxes(provider: Option<&Provider>, selected: &[Value]) -> Vec<usize> {
    if !external_strict(provider) {
        return Vec::new();
    }
    selected
        .iter()
        .enumerate()
        .filter(
            |(_, v)| matches!(v, Value::Str(s) if PRIVATE_TOOLBOXES.iter().any(|p| eq_str(s, p))),
        )
        .map(|(i, _)| i)
        .collect()
}

// ── canonicalPath ───────────────────────────────────────────────────────────

fn all_inert(s: &[u16]) -> bool {
    char::decode_utf16(s.iter().copied()).all(|r| r.is_ok_and(|c| is_nfc_inert(u32::from(c))))
}

/// `/%[0-9a-f]{2}/i.test(s)`.
fn has_pct_hex(s: &[u16]) -> bool {
    let hex = |c: Option<&u16>| c.is_some_and(|&c| c < 0x80 && (c as u8).is_ascii_hexdigit());
    s.iter()
        .enumerate()
        .any(|(i, &c)| c == u16::from(b'%') && hex(s.get(i + 1)) && hex(s.get(i + 2)))
}

const SLASH: u16 = b'/' as u16;

/// Precomputed tables for the two DAV-URL patterns: the next `/` at or after each position, and
/// whether a line terminator (which `.` does not match) occurs at or after it.
struct Scan<'a> {
    s: &'a [u16],
    next_slash: Vec<usize>,
    lt_after: Vec<bool>,
}

impl<'a> Scan<'a> {
    fn new(s: &'a [u16]) -> Self {
        let n = s.len();
        let mut next_slash = vec![n; n + 1];
        let mut lt_after = vec![false; n + 1];
        for i in (0..n).rev() {
            let c = s.get(i).copied().unwrap_or(0);
            let ns = if c == SLASH {
                i
            } else {
                next_slash.get(i + 1).copied().unwrap_or(n)
            };
            let lt = matches!(c, 0x0a | 0x0d | 0x2028 | 0x2029)
                || lt_after.get(i + 1).copied().unwrap_or(false);
            if let Some(slot) = next_slash.get_mut(i) {
                *slot = ns;
            }
            if let Some(slot) = lt_after.get_mut(i) {
                *slot = lt;
            }
        }
        Scan {
            s,
            next_slash,
            lt_after,
        }
    }

    fn slash(&self, i: usize) -> usize {
        self.next_slash.get(i).copied().unwrap_or(self.s.len())
    }

    fn lt(&self, i: usize) -> bool {
        self.lt_after.get(i).copied().unwrap_or(false)
    }

    /// `(?:dav\/files\/[^/]+|webdav)(\/.*)?$` at `q`: `Some(group start)` (the group runs to the
    /// end) or `Some(None)` when the group does not take part; `None` when no match.
    fn tail(&self, q: usize) -> Option<Option<usize>> {
        let n = self.s.len();
        let e = if ci_at(self.s, q, "dav/files/") {
            let r = q + 10;
            let e = self.slash(r);
            if e == r {
                return None;
            }
            e
        } else if ci_at(self.s, q, "webdav") {
            q + 6
        } else {
            return None;
        };
        if e == n {
            Some(None)
        } else if self.s.get(e) == Some(&SLASH) && !self.lt(e) {
            Some(Some(e))
        } else {
            None
        }
    }

    /// `\/?remote\.php\/` + [`Self::tail`] at `p` (the `/?` greedy: with the slash first).
    fn at(&self, p: usize) -> Option<Option<usize>> {
        if self.s.get(p) == Some(&SLASH) && ci_at(self.s, p + 1, "remote.php/") {
            if let Some(g) = self.tail(p + 12) {
                return Some(g);
            }
        }
        if ci_at(self.s, p, "remote.php/") {
            return self.tail(p + 11);
        }
        None
    }

    /// `^[a-z][a-z0-9+.-]*:\/\/` (case-insensitive): the position after `://`.
    fn scheme(&self) -> Option<usize> {
        let first = self.s.first().copied()?;
        if !(fold(first) >= u16::from(b'a') && fold(first) <= u16::from(b'z')) {
            return None;
        }
        let mut i = 1;
        while let Some(&c) = self.s.get(i) {
            let f = fold(c);
            if (u16::from(b'a')..=u16::from(b'z')).contains(&f)
                || (u16::from(b'0')..=u16::from(b'9')).contains(&c)
                || c == u16::from(b'+')
                || c == u16::from(b'.')
                || c == u16::from(b'-')
            {
                i += 1;
            } else {
                break;
            }
        }
        ci_at(self.s, i, "://").then_some(i + 3)
    }

    /// canonicalPath's `/(?:^[a-z][a-z0-9+.-]*:\/\/[^/]*)?\/?remote\.php\/(?:dav\/files\/[^/]+|webdav)(\/.*)?$/i`
    /// exec: `Some(group)` for the leftmost match.
    fn full_url(&self) -> Option<Option<usize>> {
        if let Some(h0) = self.scheme() {
            // `[^/]*` greedy: up to the next slash, then (backtracking) ten code units shorter,
            // the only other length after which `\/?remote\.php\/` can match.
            let h1 = self.slash(h0);
            if self.s.get(h1) == Some(&SLASH) && ci_at(self.s, h1 + 1, "remote.php/") {
                if let Some(g) = self.tail(h1 + 12) {
                    return Some(g);
                }
            }
            if let Some(k) = h1.checked_sub(10).filter(|&k| k >= h0) {
                if ci_at(self.s, k, "remote.php/") {
                    if let Some(g) = self.tail(k + 11) {
                        return Some(g);
                    }
                }
            }
        }
        (0..=self.s.len()).find_map(|p| self.at(p))
    }

    /// diaryFolderFor's `/\/remote\.php\/(?:dav\/files\/[^/]+|webdav)(\/.*)?$/i` exec.
    fn dav_root(&self) -> Option<Option<usize>> {
        (0..self.s.len()).find_map(|p| {
            if self.s.get(p) == Some(&SLASH) && ci_at(self.s, p + 1, "remote.php/") {
                self.tail(p + 12)
            } else {
                None
            }
        })
    }
}

/// `canonicalPath(value)` for a string `value`: its folder-relative, lowercased form (`''` is the
/// files root), or `None` when it is not made of NFC-inert code points before and after decoding
/// (see the crate docs).
pub fn canonical_path(value: &[u16]) -> Option<Vec<u16>> {
    // normalize('NFC') is the identity on NFC-inert text.
    if !all_inert(value) {
        return None;
    }
    let mut text = value.to_vec();
    for _ in 0..3 {
        if !has_pct_hex(&text) {
            break;
        }
        match decode_uri_component(&text) {
            Some(t) => text = t,
            None => break,
        }
    }
    // toLowerCase is context-free on NFC-inert text (no capital sigma among them).
    if !all_inert(&text) {
        return None;
    }
    for c in &mut text {
        if *c == u16::from(b'\\') {
            *c = SLASH;
        }
    }
    let scan = Scan::new(&text);
    let rest: &[u16] = match scan.full_url() {
        Some(Some(g)) => text.get(g..).unwrap_or(&[]),
        Some(None) => &[],
        None => match scan.scheme() {
            Some(h0) => text.get(scan.slash(h0)..).unwrap_or(&[]),
            None => &text,
        },
    };
    let mut out: Vec<&[u16]> = Vec::new();
    for segment in rest.split(|&c| c == SLASH) {
        if segment.is_empty() || segment == [u16::from(b'.')] {
            continue;
        }
        if segment == [u16::from(b'.'), u16::from(b'.')] {
            out.pop();
            continue;
        }
        out.push(segment);
    }
    Some(to_lower(&out.join(&SLASH)))
}

// ── diaryFolderFor ──────────────────────────────────────────────────────────

/// `diaryFolderFor(storage)`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Folder {
    /// The folder, relative to the files root.
    Known(Vec<u16>),
    /// `null`: the JS cannot work it out either.
    Unidentified,
    /// The port cannot work it out (non-inert text, or a non-ASCII connection URL).
    Unknown,
}

/// `new URL(base).pathname`, or `base` itself when it is not a URL; `None` for non-ASCII text.
fn pathname(base: &[u16]) -> Option<Vec<u16>> {
    if base.iter().any(|&c| c >= 0x80) {
        return None;
    }
    let text: String = base
        .iter()
        .map(|&c| char::from(u8::try_from(c).unwrap_or(b'?')))
        .collect();
    Some(match url::Url::parse(&text) {
        Ok(u) => units(u.path()),
        Err(_) => base.to_vec(),
    })
}

/// `diaryFolderFor(storage)`.
pub fn diary_folder(storage: Option<&Storage>) -> Folder {
    let Some(s) = storage else {
        return Folder::Unidentified;
    };
    if !s
        .kind
        .as_deref()
        .is_some_and(|k| eq_str(k, "nextcloud") || eq_str(k, "webdav"))
    {
        return Folder::Unidentified;
    }
    let root = trim(&s.corpus_root);
    if root.is_empty() {
        return Folder::Unidentified;
    }
    let Some(base) = pathname(&s.base_url) else {
        return Folder::Unknown;
    };
    let scan = Scan::new(&base);
    let prefix = match scan.dav_root() {
        Some(Some(g)) => match canonical_path(base.get(g..).unwrap_or(&[])) {
            Some(p) => p,
            None => return Folder::Unknown,
        },
        Some(None) | None => Vec::new(),
    };
    let Some(root) = canonical_path(root) else {
        return Folder::Unknown;
    };
    let folder: Vec<u16> = [prefix, root]
        .into_iter()
        .filter(|p| !p.is_empty())
        .collect::<Vec<_>>()
        .join(&SLASH);
    if folder.is_empty() {
        Folder::Unidentified
    } else {
        Folder::Known(folder)
    }
}

// ── toolRefusal ─────────────────────────────────────────────────────────────

/// `PATH_KEY`: `/path|dir|folder|file|scope|source|destination|target|href|url|location|from|to$/i`.
fn path_key(k: &[u16]) -> bool {
    const WORDS: [&str; 12] = [
        "path",
        "dir",
        "folder",
        "file",
        "scope",
        "source",
        "destination",
        "target",
        "href",
        "url",
        "location",
        "from",
    ];
    WORDS.iter().any(|w| ci_contains(k, w))
        || k.len().checked_sub(2).is_some_and(|at| ci_at(k, at, "to"))
}

/// `recursiveArgs(args)`.
fn recursive(args: &Value) -> bool {
    let Value::Obj(m) = args else {
        // Arrays and strings only have index keys; other values have none.
        return false;
    };
    m.iter().any(|(k, v)| {
        (ci_contains(k, "recurs") || ci_contains(k, "depth") || ci_contains(k, "deep"))
            && !matches!(v, Value::Bool(false) | Value::Null)
            && !matches!(v, Value::Num(n) if *n == 0.0 || *n == 1.0)
            && !matches!(v, Value::Str(s) if eq_str(s, "0") || eq_str(s, "1"))
    })
}

/// `pathArguments(args, depth, out)`.
fn path_arguments<'a>(args: &'a Value, depth: usize, out: &mut Vec<&'a [u16]>) {
    if depth > 3 {
        return;
    }
    let entry = |key: &[u16], value: &'a Value, out: &mut Vec<&'a [u16]>| match value {
        Value::Str(s) => {
            if path_key(key) {
                out.push(s);
            }
        }
        Value::Arr(items) => {
            for v in items {
                match v {
                    Value::Str(s) if path_key(key) => out.push(s),
                    v => path_arguments(v, depth + 1, out),
                }
            }
        }
        Value::Obj(_) => path_arguments(value, depth + 1, out),
        // Deep: unreachable within depth 3 (see ARGS_CAP); scalars carry no path.
        _ => {}
    };
    match args {
        Value::Obj(m) => {
            for (k, v) in m {
                entry(k, v, out);
            }
        }
        Value::Arr(items) => {
            for (i, v) in items.iter().enumerate() {
                entry(&units(&i.to_string()), v, out);
            }
        }
        _ => {}
    }
}

fn is_tree_tool(name: &[u16]) -> bool {
    ["search_files", "find_by_name", "find_by_type"]
        .iter()
        .any(|t| starts_with(name, "nc_webdav_") && name.get(10..).is_some_and(|r| eq_str(r, t)))
}

/// `toolRefusal({ provider, toolName, rawArgs, storage })` with `name = String(toolName || '')`
/// and `raw_args` the JSON value the host sent: a string is parsed (`''` as `{}`), a falsy
/// non-string is `{}`, anything else is used as is.
pub fn tool_refusal(
    provider: Option<&Provider>,
    name: &[u16],
    raw_args: &Value,
    storage: Option<&Storage>,
) -> Option<Vec<u16>> {
    if !external_strict(provider) {
        return None;
    }
    let label = match provider {
        Some(p) if !p.label.is_empty() => p.label.clone(),
        _ => units("an external provider"),
    };
    let msg = |parts: &[&[u16]]| Some(cat(parts));
    if starts_with(name, "diary_") {
        return msg(&[
            &units("ERROR: "),
            name,
            &units(" is not available with "),
            &label,
            &units(": Diary content is never sent to an external provider."),
        ]);
    }
    if !starts_with(name, "nc_webdav_") {
        return None;
    }
    let Folder::Known(folder) = diary_folder(storage) else {
        return msg(&[
            &units("ERROR: "),
            name,
            &units(" is not available with "),
            &label,
            &units(": the Diary folder could not be identified, so storage is closed to external providers. Use a local model for file work."),
        ]);
    };
    let parsed;
    let empty = Value::Obj(Vec::new());
    let args: &Value = match raw_args {
        Value::Str(s) => {
            let text = if s.is_empty() { units("{}") } else { s.clone() };
            match json::parse(&text, ARGS_CAP) {
                Some(v) => {
                    parsed = v;
                    &parsed
                }
                None => {
                    return msg(&[
                        &units("ERROR: "),
                        name,
                        &units(" arguments could not be read, so it was not run."),
                    ])
                }
            }
        }
        v if !truthy(v) => &empty,
        v => v,
    };
    let mut paths = Vec::new();
    path_arguments(args, 0, &mut paths);
    let tree = is_tree_tool(name) || recursive(args);
    if tree && paths.is_empty() {
        return msg(&[
            &units("ERROR: "),
            name,
            &units(" needs a folder to search in when used with "),
            &label,
            &units(", so it was not run. Search a specific folder outside the Diary."),
        ]);
    }
    let inside = || {
        msg(&[
            &units("ERROR: "),
            name,
            &units(" was not run: that path is in the Diary folder, and Diary content is never sent to "),
            &label,
            &units(". Do not retry; tell the user to use a local model for Diary files."),
        ])
    };
    let mut folder_slash = folder.clone();
    folder_slash.push(SLASH);
    for value in paths {
        let Some(target) = canonical_path(value) else {
            // Unknown: it may be the Diary folder.
            return inside();
        };
        if target == folder || target.starts_with(&folder_slash) {
            return inside();
        }
        if tree && (target.is_empty() || folder.starts_with(&cat(&[&target, &[SLASH]]))) {
            return msg(&[
                &units("ERROR: "),
                name,
                &units(" was not run: that folder contains the Diary folder, and Diary content is never sent to "),
                &label,
                &units(". Search a folder that does not contain the Diary."),
            ]);
        }
    }
    None
}

// ── wire ────────────────────────────────────────────────────────────────────

fn exact_keys<'a>(v: &'a Value, keys: &[&str]) -> Option<Vec<&'a Value>> {
    let Value::Obj(m) = v else { return None };
    if m.len() != keys.len() {
        return None;
    }
    keys.iter().map(|k| v.get(k)).collect()
}

fn opt_str(v: &Value) -> Result<Option<Vec<u16>>, Refusal> {
    match v {
        Value::Null => Ok(None),
        Value::Str(s) => Ok(Some(s.clone())),
        _ => Err(Refusal::Input),
    }
}

fn string(v: &Value) -> Result<Vec<u16>, Refusal> {
    v.as_str().map(<[u16]>::to_vec).ok_or(Refusal::Input)
}

/// `null` or `{kind: string|null, external: boolean, baseUrl: string, label: string}`.
fn provider_of(v: &Value) -> Result<Option<Provider>, Refusal> {
    if *v == Value::Null {
        return Ok(None);
    }
    let Some([kind, ext, base, label]) = exact_keys(v, &["kind", "external", "baseUrl", "label"])
        .and_then(|f| <[&Value; 4]>::try_from(f).ok())
    else {
        return Err(Refusal::Input);
    };
    let Value::Bool(external) = ext else {
        return Err(Refusal::Input);
    };
    Ok(Some(Provider {
        kind: opt_str(kind)?,
        external: *external,
        base_url: string(base)?,
        label: string(label)?,
    }))
}

/// `null` or `{kind: string|null, corpusRoot: string, baseUrl: string}`.
fn storage_of(v: &Value) -> Result<Option<Storage>, Refusal> {
    if *v == Value::Null {
        return Ok(None);
    }
    let Some([kind, root, base]) = exact_keys(v, &["kind", "corpusRoot", "baseUrl"])
        .and_then(|f| <[&Value; 3]>::try_from(f).ok())
    else {
        return Err(Refusal::Input);
    };
    Ok(Some(Storage {
        kind: opt_str(kind)?,
        corpus_root: string(root)?,
        base_url: string(base)?,
    }))
}

fn push_bool_or_null(out: &mut Vec<u8>, v: Option<bool>) {
    out.extend_from_slice(match v {
        Some(true) => b"true",
        Some(false) => b"false",
        None => b"null",
    });
}

fn push_refusal(out: &mut Vec<u8>, r: Option<Vec<u16>>) {
    out.extend_from_slice(b"{\"refusal\":");
    match r {
        Some(text) => json::push_str(out, &text),
        None => out.extend_from_slice(b"null"),
    }
    out.push(b'}');
}

fn run(op: u8, args: &[Value]) -> Result<Vec<u8>, Refusal> {
    let mut out = Vec::new();
    match (op, args) {
        (1, [p]) => {
            let p = provider_of(p)?;
            out.extend_from_slice(b"{\"external\":");
            push_bool_or_null(&mut out, external(p.as_ref()));
            out.extend_from_slice(b",\"trial\":");
            push_bool_or_null(&mut out, trial(p.as_ref()));
            out.push(b'}');
        }
        (2, [p, space, project, diary]) => {
            let p = provider_of(p)?;
            push_refusal(&mut out, egress_refusal(p.as_ref(), space, project, diary));
        }
        (3, [p, Value::Arr(selected)]) => {
            let p = provider_of(p)?;
            out.extend_from_slice(b"{\"removed\":[");
            for (k, i) in private_toolboxes(p.as_ref(), selected).iter().enumerate() {
                if k > 0 {
                    out.push(b',');
                }
                out.extend_from_slice(i.to_string().as_bytes());
            }
            out.extend_from_slice(b"]}");
        }
        (4, [p, Value::Str(name), raw, storage]) => {
            let p = provider_of(p)?;
            let s = storage_of(storage)?;
            push_refusal(&mut out, tool_refusal(p.as_ref(), name, raw, s.as_ref()));
        }
        (5, [Value::Arr(paths)]) => {
            out.extend_from_slice(b"{\"canonical\":[");
            for (k, v) in paths.iter().enumerate() {
                if k > 0 {
                    out.push(b',');
                }
                match canonical_path(v.as_str().ok_or(Refusal::Input)?) {
                    Some(c) => json::push_str(&mut out, &c),
                    None => out.extend_from_slice(b"null"),
                }
            }
            out.extend_from_slice(b"]}");
        }
        (6, [storage]) => {
            let s = storage_of(storage)?;
            match diary_folder(s.as_ref()) {
                Folder::Known(f) => {
                    out.extend_from_slice(b"{\"folder\":");
                    json::push_str(&mut out, &f);
                    out.extend_from_slice(b",\"known\":true}");
                }
                Folder::Unidentified => out.extend_from_slice(b"{\"folder\":null,\"known\":true}"),
                Folder::Unknown => out.extend_from_slice(b"{\"folder\":null,\"known\":false}"),
            }
        }
        _ => return Err(Refusal::Input),
    }
    Ok(out)
}

/// The `provider_egress` wasm call: input `u8(op)` and UTF-8 JSON (at most [`MAX_INPUT_BYTES`]):
///
/// - op 1 `[provider]`: `{"external":b|null,"trial":b|null}` (`null`: unknown);
/// - op 2 `[provider, spaceId, projectId, diaryProjectId]`: `{"refusal":null|"…"}`;
/// - op 3 `[provider, selected]`: `{"removed":[i,…]}`;
/// - op 4 `[provider, toolName, rawArgs, storage]`: `{"refusal":null|"…"}`;
/// - op 5 `[[path,…]]`: `{"canonical":["…"|null,…]}` (`null`: unknown);
/// - op 6 `[storage]`: `{"folder":"…"|null,"known":b}`.
///
/// `provider` is `null` or `{kind,external,baseUrl,label}`, `storage` `null` or
/// `{kind,corpusRoot,baseUrl}` (the host's projections). Status 1 with `{"error":"input"|
/// "too_large"}` when refused. Ops 2-4 fold every unknown into the strict answer.
pub fn call(input: &[u8]) -> (u32, Vec<u8>) {
    let refuse = |r: Refusal| (1, r.json().as_bytes().to_vec());
    if input.len() > MAX_INPUT_BYTES {
        return refuse(Refusal::TooLarge);
    }
    let Some((&op, body)) = input.split_first() else {
        return refuse(Refusal::Input);
    };
    // The outer array, a projection or the arguments (one level), then ARGS_CAP below them.
    let Some(Value::Arr(args)) = json::parse_utf8(body, ARGS_CAP + 2) else {
        return refuse(Refusal::Input);
    };
    match run(op, &args) {
        Ok(out) => (0, out),
        Err(e) => refuse(e),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
mod tests {
    use super::*;

    fn s(u: &[u16]) -> String {
        String::from_utf16(u).unwrap()
    }
    fn canon(t: &str) -> Option<String> {
        canonical_path(&units(t)).map(|u| s(&u))
    }
    fn chatgpt() -> Provider {
        Provider {
            kind: Some(units("chatgpt-oauth")),
            label: units("ChatGPT"),
            ..Provider::default()
        }
    }
    fn nc(root: &str) -> Storage {
        Storage {
            kind: Some(units("nextcloud")),
            corpus_root: units(root),
            base_url: units("https://nc.example/remote.php/dav/files/u"),
        }
    }
    fn refusal(name: &str, args: &str, storage: &Storage) -> Option<String> {
        tool_refusal(
            Some(&chatgpt()),
            &units(name),
            &Value::Str(units(args)),
            Some(storage),
        )
        .map(|u| s(&u))
    }

    #[test]
    fn canonical_paths() {
        assert_eq!(canon("Diary/x.md").as_deref(), Some("diary/x.md"));
        assert_eq!(canon("./a//b/../Diary/").as_deref(), Some("a/diary"));
        assert_eq!(canon("%2544iary").as_deref(), Some("diary"));
        assert_eq!(canon("a\\Diary").as_deref(), Some("a/diary"));
        assert_eq!(
            canon("https://h/remote.php/dav/files/u/Diary/x").as_deref(),
            Some("diary/x")
        );
        assert_eq!(canon("https://hremote.php/webdav/D").as_deref(), Some("d"));
        assert_eq!(canon("/remote.php/webdav").as_deref(), Some(""));
        assert_eq!(canon("https://h/a/b").as_deref(), Some("a/b"));
        // `.` does not cross a line terminator: no DAV match, so only the scheme goes.
        assert_eq!(
            canon("https://h/remote.php/webdav/a\nb").as_deref(),
            Some("remote.php/webdav/a\nb")
        );
        assert_eq!(canon("%ZZ%41").as_deref(), Some("%zz%41"));
        assert_eq!(canon("TAGEBÜCHER").as_deref(), Some("tagebücher"));
        assert_eq!(canon("Tagebu\u{308}cher"), None);
        assert_eq!(canon("Tagebu%CC%88cher"), None);
        assert_eq!(canon("\u{3a3}"), None);
    }

    #[test]
    fn folders() {
        assert_eq!(
            diary_folder(Some(&nc(" Diary "))),
            Folder::Known(units("diary"))
        );
        let mut st = nc("Diary");
        st.base_url = units("https://nc.example/remote.php/webdav/Shared");
        assert_eq!(
            diary_folder(Some(&st)),
            Folder::Known(units("shared/diary"))
        );
        assert_eq!(diary_folder(Some(&nc(""))), Folder::Unidentified);
        assert_eq!(diary_folder(None), Folder::Unidentified);
        assert_eq!(diary_folder(Some(&nc("Ημερολόγιο"))), Folder::Unknown);
    }

    #[test]
    fn tool_refusals() {
        let st = nc("Diary");
        assert!(
            refusal("nc_webdav_read_file", r#"{"path":"Diary/a.md"}"#, &st)
                .unwrap()
                .contains("is in the Diary folder")
        );
        assert_eq!(
            refusal("nc_webdav_read_file", r#"{"path":"Work/a.md"}"#, &st),
            None
        );
        assert_eq!(
            refusal("nc_webdav_list_directory", r#"{"path":""}"#, &st),
            None
        );
        assert!(refusal("nc_webdav_search_files", r#"{"path":""}"#, &st)
            .unwrap()
            .contains("contains the Diary folder"));
        assert!(refusal("nc_webdav_search_files", r#"{"q":"x"}"#, &st)
            .unwrap()
            .contains("needs a folder"));
        assert!(
            refusal("nc_webdav_list_directory", r#"{"path":"/","depth":2}"#, &st)
                .unwrap()
                .contains("contains the Diary folder")
        );
        assert!(refusal("nc_webdav_read_file", "{", &st)
            .unwrap()
            .contains("could not be read"));
        assert!(refusal("diary_search", "{}", &st)
            .unwrap()
            .contains("not available"));
        assert!(refusal("nc_webdav_read_file", r#"{"path":"x"}"#, &nc(""))
            .unwrap()
            .contains("could not be identified"));
        // Unknown path: refused.
        assert!(refusal(
            "nc_webdav_read_file",
            r#"{"path":"Tagebu%CC%88cher/a"}"#,
            &st
        )
        .is_some());
        // Local provider: nothing.
        assert_eq!(
            tool_refusal(None, &units("diary_x"), &Value::Null, Some(&st)),
            None
        );
    }

    #[test]
    fn providers() {
        let p = |u: &str| Provider {
            base_url: units(u),
            ..Provider::default()
        };
        assert_eq!(
            external(Some(&p("https://integrate.api.nvidia.com/v1"))),
            Some(true)
        );
        assert_eq!(external(Some(&p("https://NVIDIA.com./v1"))), Some(true));
        assert_eq!(external(Some(&p("https://notnvidia.com/v1"))), Some(false));
        assert_eq!(external(Some(&p("http://localhost:8000"))), Some(false));
        assert_eq!(external(Some(&p("not a url"))), Some(false));
        assert_eq!(external(Some(&p("https://nvіdia.com"))), None);
        assert_eq!(external(Some(&p("https://xn--nvda-x.com"))), None);
        assert_eq!(external(Some(&chatgpt())), Some(true));
        assert_eq!(external(None), Some(false));
    }

    #[test]
    fn wire() {
        let (st, out) = call(b"\x01[null]");
        assert_eq!(
            (st, out.as_slice()),
            (0, br#"{"external":false,"trial":false}"#.as_slice())
        );
        let mut input = vec![3u8];
        input.extend_from_slice(
            br#"[{"kind":"chatgpt-oauth","external":false,"baseUrl":"","label":""},["a","diary",5,"diary"]]"#,
        );
        let (st, out) = call(&input);
        assert_eq!(
            (st, out.as_slice()),
            (0, br#"{"removed":[1,3]}"#.as_slice())
        );
        assert_eq!(call(b"\x07[]").0, 1);
        assert_eq!(call(b"\x01[{}]").0, 1);
        assert_eq!(call(b"").0, 1);
    }
}
