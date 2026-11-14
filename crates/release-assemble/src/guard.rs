//! The shared primitives of `assemble-release.sh`'s ownership pairing guards (about 330 uses in the script):
//! `routes_toml` (the fail-closed reader of `contracts/http/routes.toml`) and its two filters
//! `project_routes_rs` and `switched_routes`, `project_routes_core` (a Core module's `OWNED_ROUTES` list),
//! `code_lines`, the comment filter of `code_has`, and `move_capability` (exact marker declarations, FEATURES
//! entries and `module.exports` shorthands). Each is a port of the shell/awk function of the same name,
//! compared byte for byte with the verbatim excerpt in `fixtures/assemble-release-guards-excerpt.sh` by
//! `tests/guard_primitives.rs` on generated inputs.
//!
//! Everything works on bytes, as the script's `LC_ALL=C awk` does. Sorted outputs (`sort -u` in the script)
//! are in byte order, which is `sort` under `LC_ALL=C`; the guards compare them only with each other.
//! Inert: nothing in the release path calls this yet.

use std::fs;
use std::path::Path;

/// One `[[route]]` table as `routes_toml` prints it: an absent or empty field is `-`, except that a present
/// but empty `switch` is `?` (still switched: fail closed).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Route {
    pub path: Vec<u8>,
    pub owner: Vec<u8>,
    pub switch: Vec<u8>,
    pub methods: Vec<u8>,
}

impl Route {
    /// The tab-separated record line, without its newline.
    pub fn record(&self) -> Vec<u8> {
        let mut out = Vec::new();
        for (i, f) in [&self.path, &self.owner, &self.switch, &self.methods]
            .into_iter()
            .enumerate()
        {
            if i > 0 {
                out.push(b'\t');
            }
            out.extend_from_slice(f);
        }
        out
    }
}

/// What `routes_toml` gives: the records printed (also those before a refusal, which the shell has already
/// written to stdout) and, when it refused, its one stderr line `line <n>: <reason>` (exit status 2).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RoutesToml {
    pub routes: Vec<Route>,
    pub error: Option<String>,
}

impl RoutesToml {
    /// The routes when the file was read without a refusal (the callers check that once before any use).
    pub fn ok(&self) -> Option<&[Route]> {
        match self.error {
            None => Some(&self.routes),
            Some(_) => None,
        }
    }

    /// The shell's stdout: one record per line.
    pub fn stdout(&self) -> Vec<u8> {
        let mut out = Vec::new();
        for r in &self.routes {
            out.extend_from_slice(&r.record());
            out.push(b'\n');
        }
        out
    }
}

fn is_blank(c: u8) -> bool {
    c == b' ' || c == b'\t'
}

/// awk `trim`: spaces and tabs at both ends.
fn trim(s: &[u8]) -> &[u8] {
    let start = s.iter().position(|c| !is_blank(*c)).unwrap_or(s.len());
    let end = s
        .iter()
        .rposition(|c| !is_blank(*c))
        .map_or(start, |i| i + 1);
    s.get(start..end.max(start)).unwrap_or_default()
}

pub(crate) fn contains(hay: &[u8], needle: &[u8]) -> bool {
    find(hay, needle).is_some()
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() {
        return Some(0);
    }
    hay.windows(needle.len()).position(|w| w == needle)
}

fn is_bare(c: u8) -> bool {
    c.is_ascii_alphanumeric() || c == b'_' || c == b'-'
}

fn dash(s: &[u8]) -> Vec<u8> {
    if s.is_empty() {
        b"-".to_vec()
    } else {
        s.to_vec()
    }
}

fn lossy(s: &[u8]) -> String {
    String::from_utf8_lossy(s).into_owned()
}

/// A refusal: the message is the awk `die` text.
struct Die(String);

#[derive(Default)]
struct Toml {
    out: Vec<Route>,
    path: Vec<u8>,
    owner: Vec<u8>,
    sw: Vec<u8>,
    methods: Vec<u8>,
    has_sw: bool,
    seen: bool,
    inroute: bool,
    hdr: bool,
    arr: bool,
    depth: i64,
    abuf: Vec<u8>,
    arrkey: Vec<u8>,
}

impl Toml {
    fn flush(&mut self) {
        if self.seen {
            let switch = if self.has_sw {
                if self.sw.is_empty() {
                    b"?".to_vec()
                } else {
                    self.sw.clone()
                }
            } else {
                b"-".to_vec()
            };
            self.out.push(Route {
                path: dash(&self.path),
                owner: dash(&self.owner),
                switch,
                methods: dash(&self.methods),
            });
        }
        self.path.clear();
        self.owner.clear();
        self.sw.clear();
        self.methods.clear();
        self.has_sw = false;
        self.seen = false;
    }

    fn setkey(&mut self, key: &[u8], val: &[u8]) -> Result<(), Die> {
        let known = matches!(key, b"path" | b"owner" | b"switch" | b"methods");
        if known && val.contains(&b'\\') {
            return Err(Die(format!(
                "an escape sequence in {} is not read",
                lossy(key)
            )));
        }
        match key {
            b"path" => self.path = val.to_vec(),
            b"owner" => self.owner = val.to_vec(),
            b"switch" => {
                self.has_sw = true;
                self.sw = val.to_vec();
            }
            b"methods" => self.methods = val.to_vec(),
            _ => {}
        }
        Ok(())
    }

    /// One line of an array value (awk `scan`): quotes are honoured, a `#` outside them ends the line.
    fn scan(&mut self, s: &[u8]) -> Result<(), Die> {
        if contains(s, b"\"\"\"") || contains(s, b"'''") {
            return Err(Die("multi-line strings are not read".into()));
        }
        let mut q: Option<u8> = None;
        for (i, &c) in s.iter().enumerate() {
            if let Some(quote) = q {
                if c == b'\\' {
                    return Err(Die("an escape sequence in an array is not read".into()));
                }
                if c == quote {
                    q = None;
                } else {
                    self.abuf.push(c);
                }
                continue;
            }
            if c == b'"' || c == b'\'' {
                q = Some(c);
                continue;
            }
            if c == b'#' {
                break;
            }
            if c == b'[' {
                self.depth += 1;
                continue;
            }
            if c == b']' {
                self.depth -= 1;
                if self.depth == 0 {
                    tailok(s.get(i + 1..).unwrap_or_default())?;
                    self.arr = false;
                    return Ok(());
                }
                continue;
            }
            self.abuf.push(c);
        }
        if q.is_some() {
            return Err(Die("unterminated string in an array".into()));
        }
        self.abuf.push(b' ');
        Ok(())
    }

    fn close_array(&mut self) -> Result<(), Die> {
        if !self.arr && self.inroute {
            let key = self.arrkey.clone();
            let val = trim(&self.abuf).to_vec();
            self.setkey(&key, &val)?;
        }
        Ok(())
    }

    fn line(&mut self, raw: &[u8]) -> Result<(), Die> {
        let line = trim(raw.strip_suffix(b"\r").unwrap_or(raw));
        if self.arr {
            self.scan(line)?;
            return self.close_array();
        }
        if line.is_empty() || line.first() == Some(&b'#') {
            return Ok(());
        }
        if line.first() == Some(&b'[') {
            self.flush();
            self.hdr = true;
            let Some(len) = header_len(line) else {
                return Err(Die(format!(
                    "table header is not a plain bare name, so it cannot be classified: {}",
                    lossy(line)
                )));
            };
            let head = line.get(..len).unwrap_or_default();
            tailok(line.get(len..).unwrap_or_default())?;
            let opens = head.iter().take_while(|c| **c == b'[').count();
            let closes = head.iter().rev().take_while(|c| **c == b']').count();
            let name: Vec<u8> = head
                .iter()
                .copied()
                .filter(|c| !matches!(c, b'[' | b']' | b' ' | b'\t'))
                .collect();
            if opens != closes {
                return Err(Die(format!("unbalanced table header: {}", lossy(line))));
            }
            if name == b"route" {
                if opens != 2 {
                    return Err(Die(format!(
                        "route must be an array of tables, [[route]]: {}",
                        lossy(line)
                    )));
                }
                self.inroute = true;
                self.seen = true;
            } else if name.starts_with(b"route.") {
                return Err(Die(format!(
                    "a sub-table of route is not read: {}",
                    lossy(line)
                )));
            } else {
                self.inroute = false;
            }
            return Ok(());
        }
        let (key, rest): (Vec<u8>, &[u8]) = if self.inroute {
            let Some(klen) = route_key_len(line) else {
                return Err(Die(format!(
                    "cannot classify this line inside a [[route]] table (dotted keys and inline tables are not read): {}",
                    lossy(line)
                )));
            };
            // substr(line, 1, RLENGTH - 1): the match without its `=`, trailing blanks off, quotes off.
            let k = line.get(..klen - 1).unwrap_or_default();
            let k = trim_end(k);
            let key: Vec<u8> = k
                .iter()
                .copied()
                .filter(|c| *c != b'"' && *c != b'\'')
                .collect();
            (key, trim(line.get(klen..).unwrap_or_default()))
        } else {
            let Some(eq) = line.iter().position(|c| *c == b'=') else {
                return Ok(());
            };
            let key: Vec<u8> = line
                .get(..eq)
                .unwrap_or_default()
                .iter()
                .copied()
                .filter(|c| !matches!(c, b' ' | b'\t' | b'"' | b'\''))
                .collect();
            if !self.hdr && (key == b"route" || key.starts_with(b"route.")) {
                return Err(Die(format!(
                    "a top-level route key (an inline route array) is not read; use [[route]] tables: {}",
                    lossy(line)
                )));
            }
            (key, trim(line.get(eq + 1..).unwrap_or_default()))
        };
        if rest.starts_with(b"\"\"\"") || rest.starts_with(b"'''") {
            return Err(Die("multi-line strings are not read".into()));
        }
        if rest.first() == Some(&b'[') {
            self.arrkey = key;
            self.arr = true;
            self.depth = 1;
            self.abuf.clear();
            self.scan(rest.get(1..).unwrap_or_default())?;
            return self.close_array();
        }
        if self.inroute {
            let val = scalar(rest)?;
            self.setkey(&key, &val)?;
        }
        Ok(())
    }
}

fn trim_end(s: &[u8]) -> &[u8] {
    let end = s.iter().rposition(|c| !is_blank(*c)).map_or(0, |i| i + 1);
    s.get(..end).unwrap_or_default()
}

/// awk `tailok`: only blanks or a comment may follow a value.
fn tailok(t: &[u8]) -> Result<(), Die> {
    let t = trim(t);
    if !t.is_empty() && t.first() != Some(&b'#') {
        return Err(Die(format!(
            "unexpected text after the value: {}",
            lossy(t)
        )));
    }
    Ok(())
}

/// awk `scalar`: a quoted string up to the next same quote (no escapes), or a bare value up to a comment.
fn scalar(rest: &[u8]) -> Result<Vec<u8>, Die> {
    match rest.first() {
        Some(&q) if q == b'"' || q == b'\'' => {
            let body = rest.get(1..).unwrap_or_default();
            let Some(e) = body.iter().position(|c| *c == q) else {
                return Err(Die("unterminated string".into()));
            };
            tailok(body.get(e + 1..).unwrap_or_default())?;
            Ok(body.get(..e).unwrap_or_default().to_vec())
        }
        _ => {
            // sub(/[ \t]*#.*$/, "", rest) then trim: everything from the first `#` goes.
            let cut = rest.iter().position(|c| *c == b'#').unwrap_or(rest.len());
            Ok(trim(rest.get(..cut).unwrap_or_default()).to_vec())
        }
    }
}

/// Length of `^\[\[?[ \t]*[A-Za-z0-9_-]+([ \t]*\.[ \t]*[A-Za-z0-9_-]+)*[ \t]*\]\]?` at the start of `s`.
fn header_len(s: &[u8]) -> Option<usize> {
    let at = |i: usize| s.get(i).copied();
    let blanks = |mut i: usize| {
        while at(i).is_some_and(is_blank) {
            i += 1;
        }
        i
    };
    let bare = |mut i: usize| {
        let start = i;
        while at(i).is_some_and(is_bare) {
            i += 1;
        }
        (i > start).then_some(i)
    };
    let mut i = 0;
    if at(i) != Some(b'[') {
        return None;
    }
    i += 1;
    if at(i) == Some(b'[') {
        i += 1;
    }
    i = blanks(i);
    i = bare(i)?;
    loop {
        let j = blanks(i);
        if at(j) != Some(b'.') {
            break;
        }
        let k = blanks(j + 1);
        match bare(k) {
            Some(end) => i = end,
            None => break,
        }
    }
    i = blanks(i);
    if at(i) != Some(b']') {
        return None;
    }
    i += 1;
    if at(i) == Some(b']') {
        i += 1;
    }
    Some(i)
}

/// Length (with the `=`) of `^("[A-Za-z0-9_-]+"|'[A-Za-z0-9_-]+'|[A-Za-z0-9_-]+)[ \t]*=` at the start of `s`.
fn route_key_len(s: &[u8]) -> Option<usize> {
    let at = |i: usize| s.get(i).copied();
    let mut i = 0;
    match at(0) {
        Some(q) if q == b'"' || q == b'\'' => {
            i += 1;
            let start = i;
            while at(i).is_some_and(is_bare) {
                i += 1;
            }
            if i == start || at(i) != Some(q) {
                return None;
            }
            i += 1;
        }
        _ => {
            while at(i).is_some_and(is_bare) {
                i += 1;
            }
            if i == 0 {
                return None;
            }
        }
    }
    while at(i).is_some_and(is_blank) {
        i += 1;
    }
    if at(i) != Some(b'=') {
        return None;
    }
    Some(i + 1)
}

/// awk records: `\n`-separated, a final line without a newline still counts, none after a final newline.
pub(crate) fn records(src: &[u8]) -> Vec<&[u8]> {
    if src.is_empty() {
        return Vec::new();
    }
    let body = src.strip_suffix(b"\n").unwrap_or(src);
    body.split(|c| *c == b'\n').collect()
}

/// `routes_toml FILE` over the file's bytes.
pub fn routes_toml(src: &[u8]) -> RoutesToml {
    let mut t = Toml::default();
    let lines = records(src);
    let mut nr = 0usize;
    for (i, raw) in lines.iter().enumerate() {
        nr = i + 1;
        let mut raw: &[u8] = raw;
        if i == 0 && raw.starts_with(&[0xEF, 0xBB, 0xBF]) {
            raw = raw.get(3..).unwrap_or_default();
        }
        if let Err(Die(msg)) = t.line(raw) {
            return RoutesToml {
                routes: t.out,
                error: Some(format!("line {nr}: {msg}")),
            };
        }
    }
    if t.arr {
        return RoutesToml {
            routes: t.out,
            error: Some(format!("line {nr}: unterminated array")),
        };
    }
    t.flush();
    RoutesToml {
        routes: t.out,
        error: None,
    }
}

/// `routes_toml` over a file; a missing or unreadable file reads as empty (awk reports it and prints
/// nothing; the callers have checked `-f` first).
pub fn routes_toml_file(path: &Path) -> RoutesToml {
    match fs::read(path) {
        Ok(b) => routes_toml(&b),
        Err(e) => RoutesToml {
            routes: Vec::new(),
            error: Some(format!("cannot read {}: {e}", path.display())),
        },
    }
}

/// sed `s/{[^}]*}/{}/g`.
pub fn collapse_params(s: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(s.len());
    let mut i = 0;
    while let Some(&c) = s.get(i) {
        if c == b'{' {
            if let Some(off) = s
                .get(i + 1..)
                .and_then(|r| r.iter().position(|c| *c == b'}'))
            {
                out.extend_from_slice(b"{}");
                i += off + 2;
                continue;
            }
            // No closing brace after this one: no match from here to the end of the line.
            out.extend_from_slice(s.get(i..).unwrap_or_default());
            return out;
        }
        out.push(c);
        i += 1;
    }
    out
}

fn sort_unique(mut v: Vec<Vec<u8>>) -> Vec<Vec<u8>> {
    v.sort();
    v.dedup();
    v
}

/// `^/api/(projects/|freechats$|freechats/[^/]+$)` or `^/api/chats/[^/]+/(context|move)$`.
fn project_path(p: &[u8]) -> bool {
    if let Some(rest) = p.strip_prefix(b"/api/") {
        if rest.starts_with(b"projects/") || rest == b"freechats" {
            return true;
        }
        if let Some(id) = rest.strip_prefix(b"freechats/") {
            return !id.is_empty() && !id.contains(&b'/');
        }
        if let Some(tail) = rest.strip_prefix(b"chats/") {
            for end in [&b"/context"[..], &b"/move"[..]] {
                if let Some(id) = tail.strip_suffix(end) {
                    return !id.is_empty() && !id.contains(&b'/');
                }
            }
        }
    }
    false
}

/// `project_routes_rs FILE`: `METHOD /path` (or `ALL /path`) for every unswitched Rust project route, with
/// `{param}` collapsed to `{}`, byte-sorted and unique. `None` when `routes_toml` refused (the shell pipeline
/// then prints what came before the refusal; every caller checks the contract once before, so that output is
/// never used: see [`project_routes_rs_shell`] for the byte-exact form).
pub fn project_routes_rs(rt: &RoutesToml) -> Option<Vec<Vec<u8>>> {
    rt.ok()?;
    Some(project_routes_rs_shell(rt))
}

/// awk `-F'\t'` over one record: fields 1..4 (a tab inside a quoted value shifts them, as in the shell).
pub(crate) fn tab_fields(r: &Route) -> [Vec<u8>; 4] {
    let rec = r.record();
    let mut it = rec.split(|c| *c == b'\t');
    let mut f = || it.next().unwrap_or_default().to_vec();
    [f(), f(), f(), f()]
}

/// [`project_routes_rs`] exactly as the pipeline prints it, also after a refusal.
pub fn project_routes_rs_shell(rt: &RoutesToml) -> Vec<Vec<u8>> {
    let mut out = Vec::new();
    for r in &rt.routes {
        let [path, owner, switch, methods] = tab_fields(r);
        if owner != b"rust" || switch != b"-" || !project_path(&path) {
            continue;
        }
        if methods == b"-" {
            let mut l = b"ALL ".to_vec();
            l.extend_from_slice(&path);
            out.push(collapse_params(&l));
            continue;
        }
        // split($4, m, /[^A-Z]+/): the runs of capitals.
        for m in methods.split(|c| !c.is_ascii_uppercase()) {
            if m.is_empty() {
                continue;
            }
            let mut l = m.to_vec();
            l.push(b' ');
            l.extend_from_slice(&path);
            out.push(collapse_params(&l));
        }
    }
    sort_unique(out)
}

/// `switched_routes FILE`: `<switch> <path>` for every route that carries a switch, in file order.
pub fn switched_routes(rt: &RoutesToml) -> Vec<Vec<u8>> {
    let mut out = Vec::new();
    for r in &rt.routes {
        let [path, _, switch, _] = tab_fields(r);
        if switch != b"-" {
            let mut l = switch;
            l.push(b' ');
            l.extend_from_slice(&path);
            out.push(l);
        }
    }
    out
}

/// `project_routes_core FILE [NAME]` over the file's bytes (`None` = no such file, which prints nothing):
/// the `'METHOD /path'` literals inside every `const NAME = ` ... `]))` line range, quotes off, params
/// collapsed, byte-sorted and unique. `name` must be a plain identifier (the script passes constants);
/// sed reads it as a regex, which an identifier spells literally.
pub fn project_routes_core(src: Option<&[u8]>, name: &str) -> Vec<Vec<u8>> {
    let Some(src) = src else {
        return Vec::new();
    };
    let start = format!("const {name} = ");
    let mut out = Vec::new();
    let mut inside = false;
    for line in records(src) {
        let take = if inside {
            if contains(line, b"]))") {
                inside = false;
            }
            true
        } else if line.starts_with(start.as_bytes()) {
            // sed checks the end address only from the next line on.
            inside = true;
            true
        } else {
            false
        };
        if take {
            for lit in quoted_routes(line) {
                out.push(collapse_params(lit));
            }
        }
    }
    sort_unique(out)
}

/// grep -o `'[A-Z]* /[^']*'` with the quotes taken off (tr -d "'").
fn quoted_routes(line: &[u8]) -> Vec<&[u8]> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < line.len() {
        if line.get(i) == Some(&b'\'') {
            let mut j = i + 1;
            while line.get(j).is_some_and(u8::is_ascii_uppercase) {
                j += 1;
            }
            if line.get(j) == Some(&b' ') && line.get(j + 1) == Some(&b'/') {
                let body = j + 2;
                if let Some(off) = line
                    .get(body..)
                    .and_then(|r| r.iter().position(|c| *c == b'\''))
                {
                    out.push(line.get(i + 1..body + off).unwrap_or_default());
                    i = body + off + 1;
                    continue;
                }
            }
        }
        i += 1;
    }
    out
}

fn posix_space(c: u8) -> bool {
    matches!(c, b' ' | b'\t' | b'\n' | 0x0b | 0x0c | b'\r')
}

/// `code_lines FILE`: the lines that do not start (after whitespace) with `//`, `/*`, `*` or `#`, each with a
/// newline. A missing file gives nothing.
pub fn code_lines(src: Option<&[u8]>) -> Vec<u8> {
    let mut out = Vec::new();
    for line in records(src.unwrap_or_default()) {
        let start = line
            .iter()
            .position(|c| !posix_space(*c))
            .unwrap_or(line.len());
        let t = line.get(start..).unwrap_or_default();
        if t.starts_with(b"//")
            || t.starts_with(b"/*")
            || t.starts_with(b"*")
            || t.starts_with(b"#")
        {
            continue;
        }
        out.extend_from_slice(line);
        out.push(b'\n');
    }
    out
}

/// The text `code_has` searches: every line outside `//` lines and `/* ... */` blocks (a line that opens a
/// block is dropped whole, and so is the line that closes it), as `"$(awk …)"` gives it (trailing newlines
/// off). `None` = no such file (status 1).
pub fn code_text(src: Option<&[u8]>) -> Option<Vec<u8>> {
    let src = src?;
    let mut out = Vec::new();
    let mut block = false;
    for raw in records(src) {
        if block {
            if contains(raw, b"*/") {
                block = false;
            }
            continue;
        }
        let start = raw.iter().position(|c| !is_blank(*c)).unwrap_or(raw.len());
        let line = raw.get(start..).unwrap_or_default();
        if line.starts_with(b"//") {
            continue;
        }
        if line.starts_with(b"/*") {
            if !contains(line, b"*/") {
                block = true;
            }
            continue;
        }
        out.extend_from_slice(raw);
        out.push(b'\n');
    }
    while out.last() == Some(&b'\n') {
        out.pop();
    }
    Some(out)
}

/// `code_has FILE ERE` for a fixed string: whether a line of [`code_text`] contains `needle` (a needle
/// without a newline). Callers with a real ERE match [`code_text`]'s lines themselves.
pub fn code_has_fixed(src: Option<&[u8]>, needle: &[u8]) -> bool {
    match code_text(src) {
        Some(text) => text.split(|c| *c == b'\n').any(|l| contains(l, needle)),
        None => false,
    }
}

/// The `kind` argument of `move_capability`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MoveKind {
    /// `pub const NAME: &str = "VALUE";` exactly once.
    Rust,
    /// `const NAME = 'VALUE';` exactly once and `NAME` exported once from `module.exports = { … }`.
    Js,
    /// `async function NAME(` exactly once and exported once.
    Function,
    /// One `pub const FEATURES: &[&str] = &[` list holding `"VALUE"` exactly once.
    Features,
    /// One FEATURES list holding the bare path `VALUE` exactly once, or `NAME` exported once.
    FeatureRef,
}

impl MoveKind {
    pub fn parse(s: &str) -> Option<MoveKind> {
        Some(match s {
            "rust" => MoveKind::Rust,
            "js" => MoveKind::Js,
            "function" => MoveKind::Function,
            "features" => MoveKind::Features,
            "feature_ref" => MoveKind::FeatureRef,
            _ => return None,
        })
    }
}

/// awk `split(s, f, ",")`: no fields for an empty string.
fn fields(s: &[u8]) -> Vec<&[u8]> {
    if s.is_empty() {
        Vec::new()
    } else {
        s.split(|c| *c == b',').collect()
    }
}

/// `move_capability FILE NAME VALUE KIND` (status 0 = true) over the file's bytes; `None` = no such file.
/// For `Function`, the script reads NAME as part of a regex; NAME must be a plain identifier here (anything
/// else is `false`), which is what every caller passes.
pub fn move_capability(src: Option<&[u8]>, name: &str, value: &str, kind: MoveKind) -> bool {
    let Some(src) = src else {
        return false;
    };
    if kind == MoveKind::Function
        && (name.is_empty() || !name.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'_'))
    {
        return false;
    }
    let name = name.as_bytes();
    let value = value.as_bytes();
    const FEATURES_OPEN: &[u8] = b"pub const FEATURES: &[&str] = &[";
    const EXPORTS_OPEN: &[u8] = b"module.exports = {";
    let rust_decl: Vec<u8> = [
        &b"pub const "[..],
        name,
        &b": &str = \""[..],
        value,
        &b"\";"[..],
    ]
    .concat();
    let js_decl: Vec<u8> = [&b"const "[..], name, &b" = '"[..], value, &b"';"[..]].concat();
    let fn_decl: Vec<u8> = [&b"async function "[..], name, &b"("[..]].concat();
    let feature_want: Vec<u8> = match kind {
        MoveKind::FeatureRef => value.to_vec(),
        _ => [&b"\""[..], value, &b"\""[..]].concat(),
    };
    let features_kind = matches!(kind, MoveKind::Features | MoveKind::FeatureRef);
    let (mut decl, mut observed, mut exported) = (0u64, 0u64, 0u64);
    let (mut block, mut features, mut exporting) = (false, false, false);
    for raw in records(src) {
        let mut line: Vec<u8> = trim(raw).to_vec();
        if block {
            if contains(&line, b"*/") {
                block = false;
            }
            continue;
        }
        if line.starts_with(b"//") {
            continue;
        }
        if contains(&line, b"/*") {
            if !contains(&line, b"*/") {
                block = true;
            }
            continue;
        }
        if features_kind && line.starts_with(FEATURES_OPEN) {
            decl += 1;
            features = true;
            line = line.get(FEATURES_OPEN.len()..).unwrap_or_default().to_vec();
        }
        if features {
            let fend = find(&line, b"];");
            if let Some(e) = fend {
                line.truncate(e);
            }
            observed += fields(&line)
                .into_iter()
                .filter(|f| trim(f) == feature_want.as_slice())
                .count() as u64;
            if fend.is_some() {
                features = false;
            }
        }
        if kind == MoveKind::Rust && line == rust_decl {
            decl += 1;
        }
        if kind == MoveKind::Js && line == js_decl {
            decl += 1;
        }
        if kind == MoveKind::Function && line.starts_with(&fn_decl) {
            decl += 1;
        }
        if line.starts_with(EXPORTS_OPEN) {
            exporting = true;
            line = line.get(EXPORTS_OPEN.len()..).unwrap_or_default().to_vec();
        }
        if exporting {
            let ending = line.iter().position(|c| *c == b'}');
            if let Some(e) = ending {
                line.truncate(e);
            }
            exported += fields(&line)
                .into_iter()
                .filter(|f| trim(f) == name)
                .count() as u64;
            if ending.is_some() {
                exporting = false;
            }
        }
    }
    decl == 1
        && (kind == MoveKind::Rust
            || (features_kind && observed == 1)
            || (kind != MoveKind::Features && exported == 1))
}

/// Read a file for the functions above: `None` when it is not a regular file (the script's `[ -f ]`).
pub fn read_file(path: &Path) -> Option<Vec<u8>> {
    if !path.is_file() {
        return None;
    }
    fs::read(path).ok()
}

/// `grep -qF NEEDLE FILE 2>/dev/null`: some line contains the needle (a needle without a newline); a missing
/// or unreadable file is false.
pub fn grep_fixed(path: &Path, needle: &str) -> bool {
    match fs::read(path) {
        Ok(b) => contains(&b, needle.as_bytes()),
        Err(_) => false,
    }
}

/// Which methods of a namespace's routes a pairing needs Rust to own.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Methods<'a> {
    /// No `switch` and no `methods` key: every method is Rust's.
    All,
    /// No `switch` and `methods` naming exactly `GET` (Node keeps every other method).
    OnlyGet,
    /// No `switch` and `methods` naming exactly the comma-separated set at the same index as the path in
    /// [`NamespacePairing::paths`], each method once (Node keeps every other method); a set of `-` is
    /// every method (no `methods` key).
    Each(&'a [&'a str]),
}

impl Methods<'_> {
    /// The awk rule over `$4` for the path at `index` in the pairing's paths.
    fn allow(&self, index: usize, methods: &[u8]) -> bool {
        let want: &str = match self {
            Methods::All => return methods == b"-",
            Methods::OnlyGet => "GET",
            // `-` in a set: that path is Rust's for every method (`$4 == "-"`, the public pages' /device).
            Methods::Each(sets) => match sets.get(index).copied() {
                Some("-") => return methods == b"-",
                Some(w) => w,
                None => return false,
            },
        };
        // split(w, e, ","): the wanted set; split($4, m, /[^A-Z]+/): the runs of capitals.
        let wanted: Vec<&[u8]> = want.as_bytes().split(|c| *c == b',').collect();
        let mut seen: Vec<&[u8]> = Vec::new();
        for m in methods
            .split(|c| !c.is_ascii_uppercase())
            .filter(|m| !m.is_empty())
        {
            if !wanted.contains(&m) || seen.contains(&m) {
                return false;
            }
            seen.push(m);
        }
        seen.len() == wanted.len()
    }
}

/// One namespace ownership pairing: Core's refusal module (found by a fixed string in
/// `core/server/index.cjs`) and the front's FEATURES capability move together, and when they have moved,
/// `rs/contracts/http/routes.toml` gives Rust exactly `paths` (each once) under `methods`.
#[derive(Clone, Copy, Debug)]
pub struct NamespacePairing<'a> {
    /// The refusal prefix, e.g. `feature ownership pairing`.
    pub label: &'a str,
    pub core_marker: &'a str,
    pub rs_feature: &'a str,
    pub paths: &'a [&'a str],
    pub methods: Methods<'a>,
    /// The refusal when a route row breaks the rule (or routes.toml is refused).
    pub rule_text: &'a str,
    /// The refusal when the set of paths differs.
    pub incomplete_text: &'a str,
}

/// The feature namespace (`assemble-release.sh`: "Feature authority moves as one three-namespace owner").
pub const FEATURE_OWNERSHIP: NamespacePairing<'static> = NamespacePairing {
    label: "feature ownership pairing",
    core_marker: ".createReadOnlyFeatures(",
    rs_feature: "rust-feature-owner",
    paths: &[
        "/api/features",
        "/api/admin/features",
        "/api/admin/features/{name}",
    ],
    methods: Methods::All,
    rule_text: "all methods must belong to Rust",
    incomplete_text: "incomplete namespace ownership",
};

/// The two model reads (`rust-models.cjs` + the front's `rust-model-reads`): GET only.
pub const MODEL_READ_OWNERSHIP: NamespacePairing<'static> = NamespacePairing {
    label: "model read ownership pairing",
    core_marker: "require('./rust-models.cjs')",
    rs_feature: "rust-model-reads",
    paths: &["/api/models/capabilities", "/api/models/hardware"],
    methods: Methods::OnlyGet,
    rule_text: "exactly GET of both routes must belong to Rust",
    incomplete_text: "incomplete route ownership",
};

/// Run one namespace pairing over an assembled tree (`web/`, `core/`, `rs/`, ...). `Err` holds the `die`
/// text without the `assemble-release: ` prefix.
pub fn namespace_pairing(tree: &Path, p: &NamespacePairing) -> Result<(), String> {
    if !pairing_owner(tree, p)? {
        // Historic old/old pins remain valid rollback pairs.
        return Ok(());
    }
    pairing_routes(tree, p)
}

/// The first half of every pairing: Core's marker and the front's FEATURES capability must agree. `Ok(true)`
/// when both have moved.
pub fn pairing_owner(tree: &Path, p: &NamespacePairing) -> Result<bool, String> {
    let core = grep_fixed(&tree.join("core/server/index.cjs"), p.core_marker);
    let rs = move_capability(
        read_file(&tree.join("rs/bins/noevia-server/src/lib.rs")).as_deref(),
        "FEATURES",
        p.rs_feature,
        MoveKind::Features,
    );
    if core != rs {
        return Err(format!("{}: Core and Rust must move together", p.label));
    }
    Ok(rs)
}

/// The route half of a moved pairing: `rs/contracts/http/routes.toml` gives Rust exactly the pairing's
/// paths (each once) under its methods rule.
pub fn pairing_routes(tree: &Path, p: &NamespacePairing) -> Result<(), String> {
    let contract = tree.join("rs/contracts/http/routes.toml");
    if !contract.is_file() {
        return Err(format!("{}: missing route contract", p.label));
    }
    let rule = || format!("{}: {}", p.label, p.rule_text);
    let rt = routes_toml_file(&contract);
    if rt.error.is_some() {
        // set -o pipefail: a refused routes.toml fails the pipeline.
        return Err(rule());
    }
    let mut got: Vec<Vec<u8>> = Vec::new();
    for r in &rt.routes {
        let [path, owner, switch, methods] = tab_fields(r);
        let Some(index) = p
            .paths
            .iter()
            .position(|w| w.as_bytes() == path.as_slice())
        else {
            continue;
        };
        if owner != b"rust" || switch != b"-" || !p.methods.allow(index, &methods) {
            return Err(rule());
        }
        got.push(path);
    }
    let mut want: Vec<Vec<u8>> = p.paths.iter().map(|s| s.as_bytes().to_vec()).collect();
    got.sort();
    want.sort();
    if got != want {
        return Err(format!("{}: {}", p.label, p.incomplete_text));
    }
    Ok(())
}

/// A project source's stored reads (`rust-project-reads.cjs` + the front's `rust-project-reads`): GET of the
/// three routes.
pub const PROJECT_READ_OWNERSHIP: NamespacePairing<'static> = NamespacePairing {
    label: "project source read ownership pairing",
    core_marker: "require('./rust-project-reads.cjs')",
    rs_feature: "rust-project-reads",
    paths: &[
        "/api/projects/{id}/sources/text",
        "/api/projects/{id}/documents/original",
        "/api/projects/{id}/{*}/original",
    ],
    methods: Methods::OnlyGet,
    rule_text: "exactly GET of the three routes must belong to Rust",
    incomplete_text: "incomplete route ownership",
};

/// A project's create and delete (`rust-project-lifecycle.cjs` + `rust-project-lifecycle`): POST of
/// `/api/projects`, DELETE of `/api/projects/{projectId}`.
pub const PROJECT_LIFECYCLE_OWNERSHIP: NamespacePairing<'static> = NamespacePairing {
    label: "project create/delete ownership pairing",
    core_marker: "require('./rust-project-lifecycle.cjs')",
    rs_feature: "rust-project-lifecycle",
    paths: &["/api/projects", "/api/projects/{projectId}"],
    methods: Methods::Each(&["POST", "DELETE"]),
    rule_text: "exactly POST /api/projects and DELETE /api/projects/{projectId} must belong to Rust",
    incomplete_text: "incomplete route ownership",
};

/// A project source file's delete (`rust-project-files.cjs` + `rust-project-files`): DELETE of
/// `/api/projects/{id}/files`. Run it with [`project_files_pairing`]: it needs the project storage pair.
pub const PROJECT_FILES_OWNERSHIP: NamespacePairing<'static> = NamespacePairing {
    label: "project file delete ownership pairing",
    core_marker: "require('./rust-project-files.cjs')",
    rs_feature: "rust-project-files",
    paths: &["/api/projects/{id}/files"],
    methods: Methods::Each(&["DELETE"]),
    rule_text: "exactly DELETE /api/projects/{id}/files must belong to Rust",
    incomplete_text: "incomplete route ownership",
};

/// A project's configuration, instruction skills and skill install (`rust-project-config.cjs` +
/// `rust-project-config`): the methods Node answered. Run it with [`project_config_pairing`].
pub const PROJECT_CONFIG_OWNERSHIP: NamespacePairing<'static> = NamespacePairing {
    label: "project configuration ownership pairing",
    core_marker: "require('./rust-project-config.cjs')",
    rs_feature: "rust-project-config",
    paths: &[
        "/api/projects/{projectId}/config",
        "/api/projects/{projectId}/instruction-skills",
        "/api/projects/{projectId}/instruction-skills/manifests",
        "/api/projects/{projectId}/instruction-skills/manifests/{skillId}/content",
        "/api/projects/{project}/skills/install",
    ],
    methods: Methods::Each(&["POST", "GET,PUT", "GET", "GET", "POST"]),
    rule_text: "exactly the methods Node answered must belong to Rust",
    incomplete_text: "incomplete route ownership",
};

/// The six routes Core's `rust-project-config.cjs` must refuse (its `OWNED_ROUTES`, params collapsed).
pub const PROJECT_CONFIG_REFUSED: &[&str] = &[
    "GET /api/projects/{}/instruction-skills",
    "GET /api/projects/{}/instruction-skills/manifests",
    "GET /api/projects/{}/instruction-skills/manifests/{}/content",
    "POST /api/projects/{}/config",
    "POST /api/projects/{}/skills/install",
    "PUT /api/projects/{}/instruction-skills",
];

/// The project file delete pairing. `storage_rs_owner` is the project storage write pairing's front verdict
/// (`project_storage_rs_owner`: the front's FEATURES names `rust-project-storage`), which the script reaches
/// through Core's `require` chain and is not ported yet.
pub fn project_files_pairing(tree: &Path, storage_rs_owner: bool) -> Result<(), String> {
    let p = &PROJECT_FILES_OWNERSHIP;
    if !pairing_owner(tree, p)? {
        return Ok(());
    }
    if !storage_rs_owner {
        return Err(format!("{}: needs the project storage pair", p.label));
    }
    pairing_routes(tree, p)
}

/// The front half of the project storage write pairing: `rust-project-storage` in the front's FEATURES.
pub fn project_storage_rs_owner(tree: &Path) -> bool {
    move_capability(
        read_file(&tree.join("rs/bins/noevia-server/src/lib.rs")).as_deref(),
        "FEATURES",
        "rust-project-storage",
        MoveKind::Features,
    )
}

/// The project configuration pairing: the routes, then Core refuses exactly the six routes, no longer mounts
/// `server/routes/projects.cjs`, and publishes its toolbox ids.
pub fn project_config_pairing(tree: &Path) -> Result<(), String> {
    let p = &PROJECT_CONFIG_OWNERSHIP;
    if !pairing_owner(tree, p)? {
        return Ok(());
    }
    pairing_routes(tree, p)?;
    let refused = project_routes_core(
        read_file(&tree.join("core/server/rust-project-config.cjs")).as_deref(),
        "OWNED_ROUTES",
    );
    let mut want: Vec<Vec<u8>> = PROJECT_CONFIG_REFUSED
        .iter()
        .map(|s| s.as_bytes().to_vec())
        .collect();
    want.sort();
    if refused != want {
        return Err(format!(
            "{}: Core must refuse exactly the six routes the front answers",
            p.label
        ));
    }
    let index = tree.join("core/server/index.cjs");
    if tree.join("core/server/routes/projects.cjs").exists()
        || grep_fixed(&index, "routes/projects.cjs")
    {
        return Err(format!("{}: Core still mounts the project routes", p.label));
    }
    if !grep_fixed(&index, "known: allToolboxes()") {
        return Err(format!(
            "{}: Core does not publish the toolbox ids the front validates skills against",
            p.label
        ));
    }
    Ok(())
}

/// The project document page reader (`rust-project-document-pages.cjs` + the front's
/// `rust-project-document-pages`): exactly GET of the pages route.
pub const PROJECT_DOCUMENT_PAGES_OWNERSHIP: NamespacePairing<'static> = NamespacePairing {
    label: "project document page reader ownership pairing",
    core_marker: "require('./rust-project-document-pages.cjs')",
    rs_feature: "rust-project-document-pages",
    paths: &["/api/projects/{id}/documents/pages"],
    methods: Methods::Each(&["GET"]),
    rule_text: "exactly GET /api/projects/{id}/documents/pages must belong to Rust",
    incomplete_text: "incomplete route ownership",
};

/// The conversation import (#2191 round 4): exactly POST of `/api/import/conversations`. It creates
/// projects through the front's create, so it needs the project create/delete pair.
pub const CONVERSATION_IMPORT_OWNERSHIP: NamespacePairing<'static> = NamespacePairing {
    label: "conversation import ownership pairing",
    core_marker: "require('./rust-conversation-import.cjs')",
    rs_feature: "rust-conversation-import",
    paths: &["/api/import/conversations"],
    methods: Methods::Each(&["POST"]),
    rule_text: "exactly POST /api/import/conversations must belong to Rust",
    incomplete_text: "incomplete route ownership",
};

/// The project document upload (#2191 round 5): exactly POST of `/api/projects/{id}/documents`, on the
/// front's extraction pipeline (`rust-document-extraction`).
pub const PROJECT_DOCUMENTS_OWNERSHIP: NamespacePairing<'static> = NamespacePairing {
    label: "project document upload ownership pairing",
    core_marker: "require('./rust-project-documents.cjs')",
    rs_feature: "rust-project-documents",
    paths: &["/api/projects/{id}/documents"],
    methods: Methods::Each(&["POST"]),
    rule_text: "exactly POST /api/projects/{id}/documents must belong to Rust",
    incomplete_text: "incomplete route ownership",
};

/// The public pages (#2190): GET and HEAD of `/about` and `/privacy`, every method of `/device`.
pub const PUBLIC_PAGES_OWNERSHIP: NamespacePairing<'static> = NamespacePairing {
    label: "public page ownership pairing",
    core_marker: "require('./rust-public-pages.cjs')",
    rs_feature: "rust-public-pages",
    paths: &["/about", "/privacy", "/device"],
    methods: Methods::Each(&["GET,HEAD", "GET,HEAD", "-"]),
    rule_text: "GET and HEAD of the pages and every method of /device must belong to Rust",
    incomplete_text: "incomplete route ownership",
};

/// The plugin directory (#2190): every method of the route.
pub const PLUGIN_DIRECTORY_OWNERSHIP: NamespacePairing<'static> = NamespacePairing {
    label: "plugin directory ownership pairing",
    core_marker: "require('./rust-plugin-directory.cjs')",
    rs_feature: "rust-plugin-directory",
    paths: &["/api/plugins/directory"],
    methods: Methods::All,
    rule_text: "all methods must belong to Rust",
    incomplete_text: "incomplete route ownership",
};

/// The conversation import pairing. `lifecycle_rs_owner` is the project create/delete pair's verdict,
/// which runs first in the script (Core and the front agree by then).
pub fn conversation_import_pairing(tree: &Path, lifecycle_rs_owner: bool) -> Result<(), String> {
    let p = &CONVERSATION_IMPORT_OWNERSHIP;
    if !pairing_owner(tree, p)? {
        return Ok(());
    }
    if !lifecycle_rs_owner {
        return Err(format!("{}: needs the project create/delete pair", p.label));
    }
    pairing_routes(tree, p)
}

/// The front half of the project create/delete pair (its `FEATURES` names `rust-project-lifecycle`).
pub fn project_lifecycle_rs_owner(tree: &Path) -> bool {
    move_capability(
        read_file(&tree.join("rs/bins/noevia-server/src/lib.rs")).as_deref(),
        "FEATURES",
        PROJECT_LIFECYCLE_OWNERSHIP.rs_feature,
        MoveKind::Features,
    )
}

/// The project document upload pairing: the moved pair also needs the front's extraction pipeline.
pub fn project_documents_pairing(tree: &Path) -> Result<(), String> {
    let p = &PROJECT_DOCUMENTS_OWNERSHIP;
    if !pairing_owner(tree, p)? {
        return Ok(());
    }
    let extraction = move_capability(
        read_file(&tree.join("rs/bins/noevia-server/src/lib.rs")).as_deref(),
        "FEATURES",
        "rust-document-extraction",
        MoveKind::Features,
    );
    if !extraction {
        return Err(format!("{}: needs the rust-document-extraction pipeline", p.label));
    }
    pairing_routes(tree, p)
}
