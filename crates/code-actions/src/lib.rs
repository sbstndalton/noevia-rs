//! noevia-core's `server/code-actions.cjs` in Rust, exported from `dav-parse.wasm`
//! (CODE_ACTIONS_IMPL). code-actions.cjs is the CodeHarness contract v0: it reads one ACP tool call
//! of a coding agent into noevia's own action classes, decides what happens before a human sees it,
//! and maps a human's answer back to an ACP permission option. This crate answers the same three
//! questions over the host's projection of the call:
//!
//! - [`classify`] (`classify(call)`): the action class (`kind`, then the command text read by a
//!   small POSIX-shell reader: wrappers, env prefixes, `sh -c`/`eval`, interpreters with inline
//!   code, `find -exec`, git subcommands and global options, installers, publishers, curl/wget/
//!   httpie flags, redirects, substitutions), every class present, whether it is one plain
//!   command, whether a standing approval may cover it, and the paths it names or writes.
//! - [`decide`] (`decide({classified, capabilities, domains, inWorkspace})`): `allow` / `ask` /
//!   `deny` with the JS's reason text, including the domain allow-list check over every URL host
//!   the command names.
//! - [`pick_option`] (`pickOption(options, wanted)`): the ACP option to answer with, never a
//!   silent downgrade from a refusal to an allow.
//!
//! Strings are UTF-16 code units, compared exactly; every case-insensitive pattern of the JS is a
//! non-`u` regex over ASCII text, which is ASCII-only case folding. No Unicode tables, number
//! formatting or locale data are involved, so no answer depends on the Node/ICU version.
//!
//! # Stricter than the JS (refused; the host then fails closed)
//!
//! - A request over [`MAX_INPUT_BYTES`] or not the documented shape (`input`, `too_large`).
//! - `ambiguous`: a command array element that is an object or array, or a number other than a
//!   safe integer (the JS reads it through `String()`); in `decide`'s network branch, a URL host
//!   the port does not read with certainty: anything but ASCII letters, digits, `_` and `-` in
//!   non-empty labels (no `xn--` label, no label starting or ending with `-`, no trailing dot),
//!   an IP literal in brackets, or a numeric last label other than a canonical dotted quad.
//! - `too_large`: more than [`MAX_WORK`] units of reading work (lexed text, words visited), which
//!   keeps every refusal well under 10 ms. Nested `find -exec find …` is read like the JS after
//!   noevia#1201: a find reached through `-exec` does not re-read its own `-exec`s (the outer find
//!   already reads each of them), so chains are linear; more than [`MAX_FIND_EXECS`] `-exec`s in
//!   one find is every class but none/read, never standing, in the JS and here.
//!
//! The host (noevia-core CODE_ACTIONS_IMPL) always computes the JS answer first and keeps it only
//! when the port's reply is byte-identical; otherwise a classification is made stricter (never
//! auto-allowed, never standing, the union of both answers' classes and paths), a decision is at
//! least `ask` (or `deny` when either says so), and an option is `cancelled`.
//!
//! Linear in the input (each nesting level re-reads at most its own text, at most seven levels),
//! bounded by [`MAX_WORK`], no panics.

#![forbid(unsafe_code)]

use std::collections::HashSet;

use prompt_framing::js::{is_js_space, trim, units};
use prompt_framing::json::{self, Value};

/// The largest request [`call`] accepts (the op byte and the JSON).
///
/// noevia#1212: 2 MiB, so reading even a refused request stays under 10 ms; a command of more than
/// [`MAX_WORK`] units is refused whatever the request size.
pub const MAX_INPUT_BYTES: usize = 2 * 1024 * 1024 + 1;
/// Units of reading work (lexed text, words visited) one request may cost.
///
/// noevia#1212: 256 Ki units, so a refusal comes back in well under 10 ms in the wasm module; a
/// command of more than 256 Ki UTF-16 units is refused (`too_large`) and the host fails closed.
pub const MAX_WORK: u64 = 256 * 1024;
/// `MAX_FIND_EXECS` of the JS: a find with more `-exec`s is read as every class, never standing.
pub const MAX_FIND_EXECS: usize = 64;
/// Reading work charged for each command text and each command (segment, `-exec` slice) read, on
/// top of its length: the table lookups a command costs (noevia#1212), so many tiny segments or
/// substitutions are bounded like long text.
const CALL_COST: usize = 64;
/// Extra reading work per word of a curl/wget/httpie command (the flag tables).
const NETWORK_WORD_COST: usize = 4;
/// `MAX_DEPTH` of the JS: nested command texts read.
const MAX_DEPTH: usize = 6;
/// JSON nesting kept: request (0), call (1), rawInput / locations (2), a command array (3), its
/// elements (4, scalars; a container there is [`Value::Deep`]).
const JSON_DEPTH: usize = 4;

/// Why the port gives no answer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refusal {
    /// Not the documented request shape.
    Input,
    /// Over [`MAX_INPUT_BYTES`] or [`MAX_WORK`].
    TooLarge,
    /// A value whose JS reading the port does not reproduce with certainty.
    Ambiguous,
}

impl Refusal {
    /// The refusal reply.
    pub fn json(self) -> &'static str {
        match self {
            Refusal::Input => r#"{"error":"input"}"#,
            Refusal::TooLarge => r#"{"error":"too_large"}"#,
            Refusal::Ambiguous => r#"{"error":"ambiguous"}"#,
        }
    }
}

type R<T> = Result<T, Refusal>;

/// noevia's action classes (`ACTIONS`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Action {
    Read,
    Edit,
    Execute,
    Install,
    Network,
    Delete,
    GitPush,
    Browser,
    External,
    None,
}

/// Most consequential first (`SEVERITY`).
const SEVERITY: [Action; 10] = [
    Action::External,
    Action::GitPush,
    Action::Delete,
    Action::Browser,
    Action::Install,
    Action::Execute,
    Action::Network,
    Action::Edit,
    Action::Read,
    Action::None,
];

impl Action {
    /// The JS name.
    pub fn name(self) -> &'static str {
        match self {
            Action::Read => "read_repository",
            Action::Edit => "edit_file",
            Action::Execute => "execute_command",
            Action::Install => "install_dependency",
            Action::Network => "network",
            Action::Delete => "delete",
            Action::GitPush => "git_push",
            Action::Browser => "open_browser",
            Action::External => "external_account",
            Action::None => "none",
        }
    }

    fn rank(self) -> usize {
        SEVERITY.iter().position(|&a| a == self).unwrap_or(0)
    }

    /// The action named `s`, if any.
    pub fn from_units(s: &[u16]) -> Option<Action> {
        SEVERITY.iter().copied().find(|a| is(s, a.name()))
    }
}

/// `worst(a, b)`.
pub fn worst(a: Action, b: Action) -> Action {
    if a.rank() <= b.rank() {
        a
    } else {
        b
    }
}

// ── UTF-16 helpers ───────────────────────────────────────────────────────────

fn is(s: &[u16], lit: &str) -> bool {
    s.iter().copied().eq(lit.encode_utf16())
}

fn starts(s: &[u16], lit: &str) -> bool {
    let n = lit.len();
    s.len() >= n && s.get(..n).is_some_and(|p| is(p, lit))
}

fn one_of(s: &[u16], list: &[&str]) -> bool {
    list.iter().any(|l| is(s, l))
}

/// ASCII-only case-insensitive equality of one unit with an ASCII byte (a non-`u` `/i` regex over
/// an ASCII pattern never matches a non-ASCII unit).
fn ieq(c: u16, b: u8) -> bool {
    let fold = |x: u16| {
        if (0x41..=0x5a).contains(&x) {
            x + 0x20
        } else {
            x
        }
    };
    fold(c) == fold(u16::from(b))
}

fn istarts(s: &[u16], lit: &str) -> bool {
    lit.len() <= s.len() && lit.bytes().zip(s.iter()).all(|(b, &c)| ieq(c, b))
}

fn iis(s: &[u16], lit: &str) -> bool {
    lit.len() == s.len() && istarts(s, lit)
}

fn is_ascii_letter(c: u16) -> bool {
    (0x41..=0x5a).contains(&c) || (0x61..=0x7a).contains(&c)
}

fn is_digit(c: u16) -> bool {
    (0x30..=0x39).contains(&c)
}

/// ECMAScript LineTerminator (what `.` does not match).
fn is_line_terminator(c: u16) -> bool {
    matches!(c, 0x0a | 0x0d | 0x2028 | 0x2029)
}

const fn u(c: char) -> u16 {
    c as u16
}

/// `w.split('/').pop()`.
fn last_component(w: &[u16]) -> &[u16] {
    match w.iter().rposition(|&c| c == u('/')) {
        Some(i) => w.get(i + 1..).unwrap_or(&[]),
        None => w,
    }
}

/// `/^[A-Za-z_][A-Za-z0-9_]*=/`.
fn is_assignment(w: &[u16]) -> bool {
    let Some((&first, rest)) = w.split_first() else {
        return false;
    };
    if !(is_ascii_letter(first) || first == u('_')) {
        return false;
    }
    for &c in rest {
        if c == u('=') {
            return true;
        }
        if !(is_ascii_letter(c) || is_digit(c) || c == u('_')) {
            return false;
        }
    }
    false
}

/// `/^-[A-Za-z]*[<set>]/` (a letter in `set` somewhere in the leading letter run after `-`).
fn dash_letters_with(w: &[u16], set: &str) -> bool {
    let Some(rest) = w.strip_prefix(&[u('-')]) else {
        return false;
    };
    rest.iter()
        .take_while(|&&c| is_ascii_letter(c))
        .any(|&c| set.encode_utf16().any(|s| s == c))
}

/// An insertion-ordered set of actions (`new Set()`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct Found(Vec<Action>);

impl Found {
    fn add(&mut self, a: Action) {
        if !self.0.contains(&a) {
            self.0.push(a);
        }
    }
}

/// Reading work left.
struct Work {
    left: u64,
}

impl Work {
    fn charge(&mut self, n: usize) -> R<()> {
        let n = u64::try_from(n).unwrap_or(u64::MAX).saturating_add(1);
        if n > self.left {
            self.left = 0;
            return Err(Refusal::TooLarge);
        }
        self.left -= n;
        Ok(())
    }
}

// ── lex ──────────────────────────────────────────────────────────────────────

type Word = Vec<u16>;

/// What `lex(text)` returns.
#[derive(Debug, Default)]
struct Lexed {
    segments: Vec<Vec<Word>>,
    nested: Vec<Word>,
    redirects: Vec<Word>,
    compound: bool,
    broken: bool,
    dynamic: bool,
}

struct Lexer<'a> {
    text: &'a [u16],
    out: Lexed,
    words: Vec<Word>,
    word: Option<Word>,
    pending_redirect: bool,
}

impl Lexer<'_> {
    fn at(&self, i: usize) -> Option<u16> {
        self.text.get(i).copied()
    }
    fn slice(&self, from: usize, to: usize) -> Word {
        self.text
            .get(from..to.min(self.text.len()))
            .unwrap_or(&[])
            .to_vec()
    }
    fn word_push(&mut self, units: &[u16]) {
        self.word
            .get_or_insert_with(Vec::new)
            .extend_from_slice(units);
    }
    fn end_word(&mut self) {
        let Some(w) = self.word.take() else { return };
        if self.pending_redirect {
            self.out.redirects.push(w);
            self.pending_redirect = false;
        } else {
            self.words.push(w);
        }
    }
    fn end_segment(&mut self) {
        self.end_word();
        if !self.words.is_empty() {
            let words = std::mem::take(&mut self.words);
            self.out.segments.push(words);
        }
    }
    /// `readBalanced(i)`: `text[i]` is just past `(`.
    fn read_balanced(&mut self, i: usize) -> (Word, usize) {
        let mut depth = 1usize;
        let mut j = i;
        let mut q: Option<u16> = None;
        while j < self.text.len() {
            let c = self.at(j).unwrap_or(0);
            if let Some(quote) = q {
                if c == quote {
                    q = None;
                } else if c == u('\\') && quote == u('"') {
                    j += 1;
                }
                j += 1;
                continue;
            }
            if c == u('\\') {
                j += 2;
                continue;
            }
            if c == u('\'') || c == u('"') {
                q = Some(c);
                j += 1;
                continue;
            }
            if c == u('(') {
                depth += 1;
            } else if c == u(')') {
                depth -= 1;
                if depth == 0 {
                    return (self.slice(i, j), j + 1);
                }
            }
            j += 1;
        }
        self.out.broken = true;
        (self.slice(i, self.text.len()), self.text.len())
    }
    fn read_backtick(&mut self, i: usize) -> (Word, usize) {
        let end = self
            .text
            .get(i..)
            .and_then(|t| t.iter().position(|&c| c == u('`')))
            .map(|p| p + i);
        match end {
            Some(end) => (self.slice(i, end), end + 1),
            None => {
                self.out.broken = true;
                (self.slice(i, self.text.len()), self.text.len())
            }
        }
    }
}

/// `/[\s;&|<>()]/`.
fn is_stop(c: u16) -> bool {
    is_js_space(c)
        || [';', '&', '|', '<', '>', '(', ')']
            .iter()
            .any(|&s| u(s) == c)
}

/// code-actions.cjs `lex(text)`.
fn lex(text: &[u16], work: &mut Work) -> R<Lexed> {
    work.charge(text.len())?;
    let mut l = Lexer {
        text,
        out: Lexed::default(),
        words: Vec::new(),
        word: None,
        pending_redirect: false,
    };
    let n = text.len();
    let mut i = 0usize;
    while i < n {
        let c = l.at(i).unwrap_or(0);
        if c == u('\\') {
            if l.at(i + 1) == Some(u('\n')) {
                i += 2;
                continue;
            }
            let next: Vec<u16> = l.at(i + 1).into_iter().collect();
            l.word_push(&next);
            i += 2;
            continue;
        }
        if c == u('\'') {
            let end = text
                .get(i + 1..)
                .and_then(|t| t.iter().position(|&x| x == u('\'')))
                .map(|p| p + i + 1);
            match end {
                None => {
                    l.out.broken = true;
                    let rest = l.slice(i + 1, n);
                    l.word_push(&rest);
                    break;
                }
                Some(end) => {
                    let inner = l.slice(i + 1, end);
                    l.word_push(&inner);
                    i = end + 1;
                    continue;
                }
            }
        }
        if c == u('"') {
            let mut j = i + 1;
            let mut buf: Vec<u16> = Vec::new();
            while j < n && l.at(j) != Some(u('"')) {
                let d = l.at(j).unwrap_or(0);
                if d == u('\\') && j + 1 < n {
                    j += 1;
                    buf.extend(l.at(j));
                    j += 1;
                    continue;
                }
                if d == u('`') {
                    l.out.compound = true;
                    let (inner, next) = l.read_backtick(j + 1);
                    l.out.nested.push(inner);
                    j = next;
                    continue;
                }
                if d == u('$') && l.at(j + 1) == Some(u('(')) {
                    l.out.compound = true;
                    let (inner, next) = l.read_balanced(j + 2);
                    l.out.nested.push(inner);
                    j = next;
                    continue;
                }
                buf.push(d);
                j += 1;
            }
            if j >= n {
                l.out.broken = true;
            }
            l.word_push(&buf);
            i = j + 1;
            continue;
        }
        if c == u('`') {
            l.out.compound = true;
            let (inner, next) = l.read_backtick(i + 1);
            l.out.nested.push(inner);
            l.word_push(&units("$SUB"));
            i = next;
            continue;
        }
        if c == u('$') && l.at(i + 1) == Some(u('(')) {
            l.out.compound = true;
            let (inner, next) = l.read_balanced(i + 2);
            l.out.nested.push(inner);
            l.word_push(&units("$SUB"));
            i = next;
            continue;
        }
        if (c == u('<') || c == u('>')) && l.at(i + 1) == Some(u('(')) {
            l.out.compound = true;
            l.end_word();
            let (inner, next) = l.read_balanced(i + 2);
            l.out.nested.push(inner);
            i = next;
            continue;
        }
        if c == u('>') || c == u('<') || (c == u('&') && l.at(i + 1) == Some(u('>'))) {
            l.out.compound = true;
            let fd = l
                .word
                .as_ref()
                .is_some_and(|w| !w.is_empty() && w.iter().all(|&x| is_digit(x)));
            if fd {
                l.word = None;
            } else {
                l.end_word();
            }
            let mut j = i + usize::from(c == u('&'));
            let output = l.at(j) == Some(u('>'));
            j += 1;
            if l.at(j) == Some(u('>')) || l.at(j) == Some(u('|')) {
                j += 1;
            }
            if l.at(j) == Some(u('<')) && c == u('<') {
                j += 1;
                if l.at(j) == Some(u('<')) {
                    j += 1;
                }
            }
            if l.at(j) == Some(u('&')) {
                j += 1;
                while l.at(j).is_some_and(|x| is_digit(x) || x == u('-')) {
                    j += 1;
                }
                i = j;
                continue;
            }
            while l.at(j) == Some(u(' ')) || l.at(j) == Some(u('\t')) {
                j += 1;
            }
            l.pending_redirect = output;
            if !output {
                let mut k = j;
                while k < n && !l.at(k).is_some_and(is_stop) {
                    k += 1;
                }
                i = k;
                continue;
            }
            i = j;
            continue;
        }
        if c == u(';') || c == u('|') || c == u('&') || c == u('\n') || c == u('\r') {
            l.out.compound = true;
            l.end_segment();
            i += 1;
            continue;
        }
        // `/[\s;]|$/.test(…)` is always true (`$` matches at the end of any string).
        if c == u('(') || c == u(')') || ((c == u('{') || c == u('}')) && l.word.is_none()) {
            l.out.compound = true;
            l.end_segment();
            i += 1;
            continue;
        }
        if c == u(' ') || c == u('\t') {
            l.end_word();
            i += 1;
            continue;
        }
        if c == u('$') && l.word.is_none() && l.words.is_empty() {
            l.out.dynamic = true;
        }
        l.word_push(&[c]);
        i += 1;
    }
    l.end_segment();
    if l.pending_redirect {
        l.out.broken = true;
    }
    Ok(l.out)
}

// ── analyzeCommand / classifyWords ───────────────────────────────────────────

/// `analyzeCommand(command)`: every class, the worst, whether it is one plain command, whether a
/// standing approval may cover it, and what it writes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Analysis {
    pub action: Action,
    pub actions: Vec<Action>,
    pub simple: bool,
    pub standable: bool,
    pub writes: Vec<Word>,
}

/// What one part contributes to `add(r)`.
struct Part {
    actions: Found,
    standable: bool,
    /// `r.complex || r.simple === false`
    complex: bool,
    writes: Vec<Word>,
}

fn one(a: Action, standable: bool) -> Part {
    Part {
        actions: Found(vec![a]),
        standable,
        complex: false,
        writes: Vec::new(),
    }
}

/// `[...new Set(words)]`: each word once, first occurrence order, in linear time (thousands of
/// distinct `-fprint`/redirect targets or locations must not cost a quadratic scan, noevia#1212).
fn unique_words<'a>(words: impl Iterator<Item = &'a Word>) -> Vec<Word> {
    let mut seen: HashSet<&'a [u16]> = HashSet::new();
    let mut out = Vec::new();
    for w in words {
        if seen.insert(w.as_slice()) {
            out.push(w.clone());
        }
    }
    out
}

fn execute_only() -> Analysis {
    Analysis {
        action: Action::Execute,
        actions: vec![Action::Execute],
        simple: false,
        standable: false,
        writes: Vec::new(),
    }
}

/// `/^\/dev\/(null|stdout|stderr|fd\/\d+)$/`.
fn is_dev_stream(t: &[u16]) -> bool {
    if one_of(t, &["/dev/null", "/dev/stdout", "/dev/stderr"]) {
        return true;
    }
    match t.strip_prefix(units("/dev/fd/").as_slice()) {
        Some(d) => !d.is_empty() && d.iter().all(|&c| is_digit(c)),
        None => false,
    }
}

fn analyze(command: &[u16], depth: usize, work: &mut Work) -> R<Analysis> {
    work.charge(CALL_COST)?;
    let text = trim(command);
    if text.is_empty() || depth > MAX_DEPTH {
        return Ok(execute_only());
    }
    let lexed = lex(text, work)?;
    let mut found = Found::default();
    let mut standable = !lexed.broken && !lexed.dynamic;
    let mut complex = false;
    let mut writes: Vec<Word> = Vec::new();
    let mut add = |r: Part| {
        for a in r.actions.0 {
            found.add(a);
        }
        if !r.standable {
            standable = false;
        }
        if r.complex {
            complex = true;
        }
        writes.extend(r.writes);
    };
    for words in &lexed.segments {
        add(classify_words(words, depth, work, false)?);
    }
    for inner in &lexed.nested {
        let r = analyze(inner, depth + 1, work)?;
        add(Part {
            actions: Found(r.actions),
            standable: r.standable,
            complex: !r.simple,
            writes: r.writes,
        });
    }
    let written: Vec<Word> = lexed
        .redirects
        .iter()
        .filter(|t| !is_dev_stream(t))
        .cloned()
        .collect();
    if !written.is_empty() {
        found.add(Action::Edit);
    }
    writes.extend(written);
    let moves = lexed.segments.iter().any(|w| {
        let first = w.first().map(Vec::as_slice).unwrap_or(&[]);
        one_of(last_component(first), &["cd", "pushd", "popd"])
    });
    if writes.iter().any(|t| {
        t.first() == Some(&u('~')) || t.contains(&u('$')) || (moves && t.first() != Some(&u('/')))
    }) {
        standable = false;
    }
    if lexed.broken || lexed.dynamic {
        found.add(Action::Execute);
    }
    if found.0.is_empty() {
        found.add(Action::Execute);
    }
    let mut action = Action::None;
    for &a in &found.0 {
        action = worst(action, a);
    }
    if action == Action::None {
        action = Action::Execute;
    }
    let simple = !complex
        && !lexed.compound
        && !lexed.broken
        && !lexed.dynamic
        && lexed.segments.len() == 1
        && found.0.len() == 1;
    let unique = unique_words(writes.iter());
    Ok(Analysis {
        action,
        actions: found.0,
        simple,
        standable,
        writes: unique,
    })
}

fn wrapper_args(name: &[u16]) -> Option<&'static [&'static str]> {
    const TABLE: &[(&str, &[&str])] = &[
        (
            "sudo",
            &["-u", "-g", "-C", "-D", "-h", "-p", "-r", "-t", "-U", "-T"],
        ),
        ("doas", &["-u", "-C"]),
        (
            "env",
            &["-u", "-C", "-S", "--unset", "--chdir", "--split-string"],
        ),
        ("nohup", &[]),
        ("setsid", &[]),
        ("time", &["-f", "-o"]),
        ("nice", &["-n", "--adjustment"]),
        ("ionice", &["-c", "-n", "-p"]),
        ("command", &[]),
        ("builtin", &[]),
        ("exec", &["-a"]),
        ("stdbuf", &["-i", "-o", "-e"]),
        ("timeout", &["-s", "-k", "--signal", "--kill-after"]),
        ("chronic", &[]),
        ("unbuffer", &[]),
        (
            "xargs",
            &[
                "-I",
                "-i",
                "-n",
                "-P",
                "-L",
                "-l",
                "-d",
                "-s",
                "-E",
                "-e",
                "-a",
                "--arg-file",
                "--delimiter",
                "--max-args",
                "--max-procs",
                "--replace",
            ],
        ),
    ];
    TABLE.iter().find(|(n, _)| is(name, n)).map(|(_, a)| *a)
}

fn installer_subcommands(name: &[u16]) -> Option<&'static [&'static str]> {
    const TABLE: &[(&str, &[&str])] = &[
        ("npm", &["install", "i", "add", "ci", "update", "exec"]),
        ("pnpm", &["install", "i", "add", "update", "dlx"]),
        ("yarn", &["install", "add", "up"]),
        ("bun", &["install", "i", "add", "x"]),
        ("pip", &["install"]),
        ("pip3", &["install"]),
        ("uv", &["pip", "add", "sync", "tool"]),
        ("pipx", &["install", "run"]),
        ("cargo", &["install", "add", "fetch"]),
        ("gem", &["install"]),
        ("go", &["get", "install"]),
        ("composer", &["install", "require", "update"]),
        ("bundle", &["install"]),
        ("apt", &["install"]),
        ("apt-get", &["install"]),
        ("apk", &["add"]),
        ("brew", &["install"]),
        ("dnf", &["install"]),
        ("yum", &["install"]),
    ];
    TABLE.iter().find(|(n, _)| is(name, n)).map(|(_, a)| *a)
}

fn publish_subcommands(name: &[u16]) -> Option<&'static [&'static str]> {
    const TABLE: &[(&str, &[&str])] = &[
        ("npm", &["publish"]),
        ("pnpm", &["publish"]),
        ("yarn", &["publish", "npm"]),
        ("docker", &["push"]),
        ("podman", &["push"]),
        ("buildah", &["push"]),
        ("twine", &["upload"]),
        ("hub", &["push", "release", "pull-request"]),
        ("gem", &["push"]),
        ("cargo", &["publish"]),
        ("helm", &["push"]),
        ("poetry", &["publish"]),
        ("flit", &["publish"]),
    ];
    TABLE.iter().find(|(n, _)| is(name, n)).map(|(_, a)| *a)
}

const NETWORK_COMMANDS: &[&str] = &["curl", "wget", "http", "https"];
const REMOTE_EXEC_COMMANDS: &[&str] = &[
    "nc", "ncat", "netcat", "ssh", "scp", "sftp", "rsync", "telnet", "ftp",
];
const PACKAGE_RUNNERS: &[&str] = &["npx", "bunx", "pnpx"];
const DELETE_COMMANDS: &[&str] = &["rm", "rmdir", "unlink", "shred", "truncate", "srm"];
const BROWSER_COMMANDS: &[&str] = &[
    "open",
    "xdg-open",
    "chromium",
    "chrome",
    "google-chrome",
    "firefox",
    "safari",
];
const SHELLS: &[&str] = &[
    "sh", "bash", "zsh", "dash", "ksh", "mksh", "ash", "fish", "busybox",
];
const INTERPRETERS: &[&str] = &[
    "python",
    "python2",
    "python3",
    "node",
    "nodejs",
    "perl",
    "ruby",
    "php",
    "deno",
    "bun",
    "lua",
    "osascript",
    "pwsh",
    "powershell",
];

fn arg_at<'a>(args: &[&'a Word], i: usize) -> Option<&'a [u16]> {
    args.get(i).map(|w| w.as_slice())
}

/// `publishes(name, args)`.
fn publishes(name: &[u16], args: &[&Word]) -> bool {
    if let Some(subs) = publish_subcommands(name) {
        return arg_at(args, 0).is_some_and(|a| one_of(a, subs));
    }
    if is(name, "glab") {
        let a0 = arg_at(args, 0);
        let a1 = arg_at(args, 1);
        return a0.is_some_and(|a| is(a, "api"))
            || (a0.is_some_and(|a| one_of(a, &["mr", "release", "issue"]))
                && a1.is_some_and(|a| {
                    one_of(
                        a,
                        &[
                            "create", "merge", "update", "delete", "upload", "close", "note",
                        ],
                    )
                }));
    }
    if is(name, "gcloud") {
        return args.iter().any(|a| is(a, "deploy"));
    }
    if is(name, "aws") {
        return arg_at(args, 0).is_some_and(|a| is(a, "s3"))
            && arg_at(args, 1).is_some_and(|a| one_of(a, &["cp", "sync", "mv", "rm", "rb", "mb"]));
    }
    false
}

/// `classifyWords(input, depth, viaExec)`.
fn classify_words(input: &[Word], depth: usize, work: &mut Work, via_exec: bool) -> R<Part> {
    work.charge(input.len().saturating_add(CALL_COST))?;
    let mut words: &[Word] = input;
    let mut standable = true;
    let mut prefixed = false;
    loop {
        while words.first().is_some_and(|w| is_assignment(w)) {
            words = words.get(1..).unwrap_or(&[]);
            prefixed = true;
        }
        // Node returns one(NONE) here, which keeps `standable` true even after `xargs`
        // (`xargs FOO=bar` runs a program chosen at run time); stricter here, never looser.
        let Some(first) = words.first() else {
            return Ok(one(Action::None, standable));
        };
        let name = last_component(first);
        let Some(with_arg) = wrapper_args(name) else {
            break;
        };
        if is(name, "xargs") {
            standable = false;
        }
        prefixed = true;
        let env = is(name, "env");
        let mut k = 1usize;
        while let Some(w) = words.get(k) {
            if is(w, "--") {
                k += 1;
                break;
            }
            if env && is_assignment(w) {
                k += 1;
                continue;
            }
            if !starts(w, "-") || is(w, "-") {
                break;
            }
            if one_of(w, with_arg) {
                k += 2;
            } else {
                k += 1;
            }
        }
        if is(name, "timeout") && k < words.len() {
            k += 1;
        }
        words = words.get(k..).unwrap_or(&[]);
        if words.is_empty() {
            return Ok(one(Action::Execute, standable));
        }
    }
    let first = words.first().map(Vec::as_slice).unwrap_or(&[]);
    let name = last_component(first);
    if name.is_empty() || name.contains(&u('$')) {
        return Ok(one(Action::Execute, false));
    }
    let rest: &[Word] = words.get(1..).unwrap_or(&[]);

    if one_of(name, SHELLS) || one_of(name, &["eval", "source", "."]) {
        let inner: Option<Word> = if is(name, "eval") {
            Some(join(rest))
        } else {
            rest.iter().position(|w| is_c_flag(w)).map(|idx| {
                rest.get(idx + 1..)
                    .unwrap_or(&[])
                    .iter()
                    .find(|w| !starts(w, "-"))
                    .cloned()
                    .unwrap_or_default()
            })
        };
        let Some(inner) = inner else {
            return Ok(one(Action::Execute, false));
        };
        let r = analyze(&inner, depth + 1, work)?;
        let mut actions = Found(r.actions);
        actions.add(Action::Execute);
        return Ok(Part {
            actions,
            standable: false,
            complex: false,
            writes: Vec::new(),
        });
    }
    if one_of(name, INTERPRETERS) && rest.iter().any(|w| inline_code_flag(w)) {
        return Ok(one(Action::Execute, false));
    }
    if is(name, "find") {
        return find_words(rest, standable, depth, work, via_exec);
    }
    if is(name, "git") || starts(name, "git-") {
        let (action, own) = git_action(name, rest);
        return Ok(one(action, standable && own));
    }
    let args: Vec<&Word> = rest.iter().filter(|w| !starts(w, "-")).collect();
    if is(name, "gh") || publishes(name, &args) {
        return Ok(one(Action::External, false));
    }
    if let Some(subs) = installer_subcommands(name) {
        let install = arg_at(&args, 0).is_some_and(|a| one_of(a, subs));
        return Ok(one(
            if install {
                Action::Install
            } else {
                Action::Execute
            },
            standable,
        ));
    }
    if one_of(name, PACKAGE_RUNNERS) {
        return Ok(one(Action::Install, standable));
    }
    if one_of(name, REMOTE_EXEC_COMMANDS) {
        return Ok(one(Action::Execute, false));
    }
    if one_of(name, NETWORK_COMMANDS) {
        // Each word is matched against the flag tables, twice (`networkWords`, `plainNetwork`).
        work.charge(rest.len().saturating_mul(NETWORK_WORD_COST))?;
        let mut actions = network_words(name, rest);
        if prefixed {
            actions.add(Action::Execute);
            return Ok(Part {
                actions,
                standable: false,
                complex: false,
                writes: Vec::new(),
            });
        }
        return Ok(Part {
            actions,
            standable,
            complex: !plain_network(name, rest),
            writes: Vec::new(),
        });
    }
    if one_of(name, DELETE_COMMANDS) {
        return Ok(one(Action::Delete, standable));
    }
    if one_of(name, BROWSER_COMMANDS) {
        return Ok(one(Action::Browser, standable));
    }
    Ok(one(Action::Execute, standable))
}

fn join(words: &[Word]) -> Word {
    let mut out = Vec::new();
    for (i, w) in words.iter().enumerate() {
        if i > 0 {
            out.push(u(' '));
        }
        out.extend_from_slice(w);
    }
    out
}

/// `/^-[A-Za-z]*c[A-Za-z]*$/`.
fn is_c_flag(w: &[u16]) -> bool {
    let Some(rest) = w.strip_prefix(&[u('-')]) else {
        return false;
    };
    rest.iter().all(|&c| is_ascii_letter(c)) && rest.contains(&u('c'))
}

/// `/^-[A-Za-z]*[ceEprlM]/` or `/^--(eval|command|print|require|import|loader|experimental-loader|preload)(=|$)/`.
fn inline_code_flag(w: &[u16]) -> bool {
    if dash_letters_with(w, "ceEprlM") {
        return true;
    }
    let Some(rest) = w.strip_prefix(units("--").as_slice()) else {
        return false;
    };
    [
        "eval",
        "command",
        "print",
        "require",
        "import",
        "loader",
        "experimental-loader",
        "preload",
    ]
    .iter()
    .any(|opt| {
        starts(rest, opt) && {
            let after = rest.get(opt.len()..).unwrap_or(&[]);
            after.is_empty() || after.first() == Some(&u('='))
        }
    })
}

/// `-exec` and friends: the words up to the next `;` or `+` run as a command.
const FIND_EXEC: &[&str] = &["-exec", "-execdir", "-ok", "-okdir"];

/// The `find` branch of `classifyWords` (noevia#1201). Every `-exec` slice of the outer find is
/// classified by the outer loop, including the `-exec`s of a find that is itself run by `-exec`
/// (the slices nest), and the outer loop sees every `-delete`/`-fprint` word too. So a find reached
/// through `-exec` (`via_exec`) scans its own words but does not classify its own `-exec` slices
/// again. More than [`MAX_FIND_EXECS`] `-exec`s: every class but none/read, never standing.
fn find_words(
    rest: &[Word],
    standable: bool,
    depth: usize,
    work: &mut Work,
    via_exec: bool,
) -> R<Part> {
    work.charge(rest.len())?;
    if rest.iter().filter(|w| one_of(w, FIND_EXEC)).count() > MAX_FIND_EXECS {
        let every = SEVERITY
            .iter()
            .copied()
            .filter(|a| *a != Action::None && *a != Action::Read)
            .collect();
        return Ok(Part {
            actions: Found(every),
            standable: false,
            complex: false,
            writes: Vec::new(),
        });
    }
    let mut actions = Found(vec![Action::Execute]);
    let mut writes = Vec::new();
    let mut own = true;
    for (k, w) in rest.iter().enumerate() {
        if is(w, "-delete") {
            actions.add(Action::Delete);
        }
        if one_of(w, &["-fprint", "-fprint0", "-fprintf", "-fls"]) {
            actions.add(Action::Edit);
            if let Some(target) = rest.get(k + 1) {
                writes.push(target.clone());
            }
            own = false;
        }
        if one_of(w, FIND_EXEC) {
            own = false;
            if via_exec {
                continue;
            }
            let after = rest.get(k + 1..).unwrap_or(&[]);
            let end = after.iter().position(|x| is(x, ";") || is(x, "+"));
            let inner = match end {
                Some(e) => after.get(..e).unwrap_or(&[]),
                None => after,
            };
            work.charge(inner.len())?;
            let r = classify_words(inner, depth + 1, work, true)?;
            for a in r.actions.0 {
                actions.add(a);
            }
        }
    }
    Ok(Part {
        actions,
        standable: own && standable,
        complex: false,
        writes,
    })
}

// ── network commands ─────────────────────────────────────────────────────────

/// `/^https?:\/\/\S+$/i`.
fn is_url(w: &[u16]) -> bool {
    let rest = if istarts(w, "https://") {
        w.get(8..)
    } else if istarts(w, "http://") {
        w.get(7..)
    } else {
        None
    };
    rest.is_some_and(|r| !r.is_empty() && !r.iter().any(|&c| is_js_space(c)))
}

fn starts_any(w: &[u16], list: &[&str]) -> bool {
    list.iter().any(|p| starts(w, p))
}

/// `CURL_WRITE_FLAGS.test(w)`.
fn curl_writes(w: &[u16]) -> bool {
    dash_letters_with(w, "oOJDc")
        || starts_any(
            w,
            &[
                "--output",
                "--output-dir",
                "--remote-name",
                "--remote-name-all",
                "--remote-header-name",
                "--dump-header",
                "--cookie-jar",
                "--create-dirs",
                "--trace",
                "--trace-ascii",
                "--stderr",
                "--libcurl",
                "--etag-save",
                "--hsts",
                "--alt-svc",
            ],
        )
}

/// `WGET_WRITE_FLAGS.test(w)`.
fn wget_writes(w: &[u16]) -> bool {
    dash_letters_with(w, "OoaPx")
        || starts_any(
            w,
            &[
                "--output-document",
                "--output-file",
                "--append-output",
                "--directory-prefix",
                "--mirror",
                "--recursive",
                "-r",
                "-m",
                "--save-headers",
                "--save-cookies",
                "--force-directories",
                "--backups",
                "--warc-file",
            ],
        )
}

/// `/^--(a|b|…)=/`.
fn long_eq(w: &[u16], opts: &[&str]) -> bool {
    let Some(rest) = w.strip_prefix(units("--").as_slice()) else {
        return false;
    };
    opts.iter()
        .any(|o| starts(rest, o) && rest.get(o.len()) == Some(&u('=')))
}

/// `networkWords(name, rest)`.
fn network_words(name: &[u16], rest: &[Word]) -> Found {
    let mut actions = Found(vec![Action::Network]);
    let mut k = 0usize;
    while k < rest.len() {
        let w = rest.get(k).map(Vec::as_slice).unwrap_or(&[]);
        let next = rest.get(k + 1).map(Vec::as_slice);
        let mut other = |writes: bool| {
            actions.add(if writes {
                Action::Edit
            } else {
                Action::Execute
            });
        };
        if is_url(w) {
            k += 1;
            continue;
        }
        if is(name, "curl") {
            let short_safe = w.len() > 1
                && starts(w, "-")
                && w.iter()
                    .skip(1)
                    .all(|&c| "sSLIf".encode_utf16().any(|s| s == c));
            if one_of(
                w,
                &[
                    "-q",
                    "--disable",
                    "--silent",
                    "--show-error",
                    "--location",
                    "--head",
                    "--fail",
                    "--compressed",
                ],
            ) || short_safe
            {
                k += 1;
                continue;
            }
            if one_of(
                w,
                &[
                    "-H",
                    "--header",
                    "-A",
                    "--user-agent",
                    "-m",
                    "--max-time",
                    "--retry",
                ],
            ) && next.is_some_and(|n| !starts(n, "@"))
            {
                k += 2;
                continue;
            }
            if long_eq(w, &["header", "user-agent", "max-time", "retry"]) {
                k += 1;
                continue;
            }
            if (is(w, "-X") || is(w, "--request")) && next.is_some_and(|n| iis(n, "GET")) {
                k += 2;
                continue;
            }
            if iis(w, "-XGET") || iis(w, "--request=GET") {
                k += 1;
                continue;
            }
            other(curl_writes(w));
        } else if is(name, "wget") {
            if one_of(w, &["-q", "--quiet", "--spider", "-qO-", "-O-"]) {
                k += 1;
                continue;
            }
            if one_of(w, &["-O", "-qO", "--output-document"]) && next.is_some_and(|n| is(n, "-")) {
                k += 2;
                continue;
            }
            if is(w, "--output-document=-") {
                k += 1;
                continue;
            }
            if one_of(
                w,
                &[
                    "--timeout",
                    "--tries",
                    "--header",
                    "-U",
                    "--user-agent",
                    "-T",
                    "-t",
                ],
            ) && next.is_some()
            {
                k += 2;
                continue;
            }
            if long_eq(w, &["timeout", "tries", "header", "user-agent"]) {
                k += 1;
                continue;
            }
            other(wget_writes(w));
        } else {
            other(dash_letters_with(w, "od") || starts_any(w, &["--download", "--output"]));
        }
        k += 1;
    }
    actions
}

/// `/^\s*(host|:authority)\s*:/i`.
fn retargets(h: &[u16]) -> bool {
    let mut i = h.iter().take_while(|&&c| is_js_space(c)).count();
    let rest = h.get(i..).unwrap_or(&[]);
    if istarts(rest, "host") {
        i += 4;
    } else if istarts(rest, ":authority") {
        i += 10;
    } else {
        return false;
    }
    let tail = h.get(i..).unwrap_or(&[]);
    let spaces = tail.iter().take_while(|&&c| is_js_space(c)).count();
    tail.get(spaces) == Some(&u(':'))
}

/// `plainNetwork(name, rest)`.
fn plain_network(name: &[u16], rest: &[Word]) -> bool {
    for (k, w) in rest.iter().enumerate() {
        if one_of(w, &["-H", "--header"]) {
            if let Some(v) = rest.get(k + 1) {
                if retargets(v) {
                    return false;
                }
            }
        }
        if let Some(v) = w.strip_prefix(units("--header=").as_slice()) {
            if retargets(v) {
                return false;
            }
        }
    }
    if is(name, "curl")
        && !rest
            .first()
            .is_some_and(|w| one_of(w, &["-q", "--disable"]))
    {
        return false;
    }
    true
}

// ── git ──────────────────────────────────────────────────────────────────────

fn git_subcommand(sub: Option<&[u16]>) -> Option<Action> {
    let sub = sub?;
    if one_of(sub, &["push"]) {
        return Some(Action::GitPush);
    }
    if one_of(
        sub,
        &["fetch", "clone", "pull", "remote", "submodule", "ls-remote"],
    ) {
        return Some(Action::Network);
    }
    if is(sub, "clean") {
        return Some(Action::Delete);
    }
    None
}

/// ASCII-insensitive search for `needle` starting at or after `from` and before the first line
/// terminator at or after `from` (`<prefix>.*<needle>` without the `s` flag).
fn found_before_line_end(key: &[u16], from: usize, needle: &str) -> bool {
    let tail = key.get(from..).unwrap_or(&[]);
    let stop = tail
        .iter()
        .position(|&c| is_line_terminator(c))
        .unwrap_or(tail.len());
    (0..stop).any(|s| istarts(tail.get(s..).unwrap_or(&[]), needle))
}

/// `GIT_DANGEROUS_KEY.test(key)`.
fn git_dangerous_key(key: &[u16]) -> bool {
    const PREFIXES: &[&str] = &[
        "core.sshcommand",
        "core.gitproxy",
        "core.fsmonitor",
        "core.hookspath",
        "core.pager",
        "core.editor",
        "credential.",
        "http.proxy",
        "https.proxy",
        "protocol.",
        "url.",
        "uploadpack.",
        "include.",
        "includeif.",
        "diff.",
        "filter.",
        "merge.",
        "gpg.",
        "ssh.",
    ];
    if PREFIXES.iter().any(|p| istarts(key, p)) {
        return true;
    }
    if istarts(key, "http.") && found_before_line_end(key, 5, ".proxy") {
        return true;
    }
    istarts(key, "remote.")
        && [".uploadpack", ".receivepack", ".proxy"]
            .iter()
            .any(|n| found_before_line_end(key, 7, n))
}

/// `gitAction(name, rest)`: the class and whether it may stand.
fn git_action(name: &[u16], rest: &[Word]) -> (Action, bool) {
    if !is(name, "git") {
        let sub = name.get(4..).unwrap_or(&[]);
        if one_of(sub, &["send-pack", "http-push", "receive-pack"]) {
            return (Action::GitPush, false);
        }
        return (git_subcommand(Some(sub)).unwrap_or(Action::Execute), true);
    }
    let mut k = 0usize;
    let mut aliased = false;
    let mut rerouted = false;
    let config_key = |value: Option<&[u16]>, aliased: &mut bool, rerouted: &mut bool| {
        let v = value.unwrap_or(&[]);
        let key = match v.iter().position(|&c| c == u('=')) {
            Some(i) => v.get(..i).unwrap_or(&[]),
            None => v,
        };
        if istarts(key, "alias.") {
            *aliased = true;
        }
        if git_dangerous_key(key) {
            *rerouted = true;
        }
    };
    while let Some(w) = rest.get(k).filter(|w| starts(w, "-")) {
        let next = rest.get(k + 1).map(Vec::as_slice);
        if is(w, "--") {
            k += 1;
            break;
        }
        if is(w, "--config-env") || starts(w, "--config-env=") {
            rerouted = true;
            if is(w, "--config-env") {
                config_key(next, &mut aliased, &mut rerouted);
                k += 2;
            } else {
                config_key(w.get(13..), &mut aliased, &mut rerouted);
                k += 1;
            }
            continue;
        }
        if is(w, "-c") {
            config_key(next, &mut aliased, &mut rerouted);
            k += 2;
            continue;
        }
        // `/^-c.+/`: `-c` and at least one unit that is not a line terminator.
        if starts(w, "-c") && w.get(2).is_some_and(|&c| !is_line_terminator(c)) {
            config_key(w.get(2..), &mut aliased, &mut rerouted);
            k += 1;
            continue;
        }
        if one_of(
            w,
            &[
                "-C",
                "-c",
                "--git-dir",
                "--work-tree",
                "--namespace",
                "--exec-path",
                "--super-prefix",
                "--config-env",
                "--list-cmds",
                "--attr-source",
            ],
        ) {
            k += 2;
            continue;
        }
        k += 1;
    }
    if aliased {
        return (Action::GitPush, false);
    }
    let sub = rest.get(k).map(Vec::as_slice);
    let args: &[Word] = rest.get(k + 1..).unwrap_or(&[]);
    let has = |flags: &[&str]| {
        args.iter().any(|a| {
            one_of(a, flags)
                || flags
                    .iter()
                    .any(|f| f.starts_with("--") && starts(a, f) && a.get(f.len()) == Some(&u('=')))
        })
    };
    let sub_is = |s: &str| sub.is_some_and(|x| is(x, s));
    let first_arg = args.first().map(Vec::as_slice);
    if sub_is("send-pack") || sub_is("http-push") || sub_is("receive-pack") {
        return (Action::GitPush, false);
    }
    let destructive = (sub_is("update-ref") && has(&["-d", "--delete"]))
        || (sub_is("reset") && has(&["--hard", "--merge", "--keep"]))
        || (sub_is("branch")
            && args
                .iter()
                .any(|a| dash_letters_with(a, "dD") || is(a, "--delete")))
        || (sub_is("tag")
            && args
                .iter()
                .any(|a| dash_letters_with(a, "d") || is(a, "--delete")))
        || (sub_is("stash") && first_arg.is_some_and(|a| one_of(a, &["drop", "clear"])))
        || (sub_is("reflog") && first_arg.is_some_and(|a| one_of(a, &["expire", "delete"])))
        || (sub_is("gc")
            && args
                .iter()
                .any(|a| is(a, "--prune") || starts(a, "--prune=")));
    if destructive {
        return (Action::Delete, false);
    }
    let action = git_subcommand(sub).unwrap_or(Action::Execute);
    if action == Action::Network
        && (rerouted
            || has(&[
                "--upload-pack",
                "-u",
                "--receive-pack",
                "--exec",
                "--config",
                "-c",
                "--template",
            ]))
    {
        return (Action::Execute, false);
    }
    if rerouted {
        return (
            if action == Action::GitPush {
                action
            } else {
                Action::Execute
            },
            false,
        );
    }
    (action, true)
}

// ── classify ─────────────────────────────────────────────────────────────────

/// `KIND_ACTIONS` (own properties only).
fn kind_action(kind: &[u16]) -> Option<Action> {
    const TABLE: &[(&str, Action)] = &[
        ("read", Action::Read),
        ("search", Action::Read),
        ("edit", Action::Edit),
        ("move", Action::Edit),
        ("delete", Action::Delete),
        ("fetch", Action::Network),
        ("execute", Action::Execute),
        ("think", Action::None),
        ("other", Action::Execute),
    ];
    TABLE.iter().find(|(k, _)| is(kind, k)).map(|(_, a)| *a)
}

/// `approvalFor(action)`.
pub fn approval_for(action: Action) -> &'static str {
    match action {
        Action::None | Action::Read => "never",
        Action::Network => "capability",
        _ => "always",
    }
}

/// The host's projection of one ACP tool call: what `classify(call)` reads.
#[derive(Clone, Debug, Default)]
pub struct Call {
    /// `call.kind` when it is a string.
    pub kind: Option<Word>,
    /// Whether `call.rawInput` is an object.
    pub raw_input: bool,
    /// `rawInput[key]` for `command`, `cmd`, `script`, `shell`, `commandLine`, then `args`: a
    /// string, an array of its elements' `String()`, or `None` (neither).
    pub values: Vec<(CommandValue, bool)>,
    /// `rawInput.noeviaOutsideWorkspace === true`.
    pub outside: bool,
    /// `call.locations`, each `l.path` when it is a string.
    pub locations: Vec<Option<Word>>,
}

/// One `rawInput[key]` value as `commandOf` reads it.
#[derive(Clone, Debug)]
pub enum CommandValue {
    /// Neither a string nor an array.
    Other,
    /// A string.
    Text(Word),
    /// An array, each element through `String()`.
    List(Vec<Word>),
}

/// `classify(call)`'s answer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Classified {
    pub action: Action,
    pub approval: &'static str,
    pub command: Word,
    pub paths: Vec<Word>,
    pub readable: bool,
    pub actions: Vec<Action>,
    pub simple: bool,
    pub standable: bool,
}

/// `commandOf(rawInput)`.
fn command_of(call: &Call) -> Word {
    if !call.raw_input {
        return Vec::new();
    }
    for (value, is_args) in &call.values {
        match value {
            CommandValue::Text(s) if !is_args => {
                let t = trim(s);
                if !t.is_empty() {
                    return t.to_vec();
                }
            }
            CommandValue::List(items) if !items.is_empty() => {
                return trim(&join(items)).to_vec();
            }
            _ => {}
        }
    }
    Vec::new()
}

/// code-actions.cjs `classify(call)`.
pub fn classify(call: &Call) -> R<Classified> {
    let mut work = Work { left: MAX_WORK };
    let mut action = call
        .kind
        .as_deref()
        .and_then(kind_action)
        .unwrap_or(Action::Execute);
    let command = command_of(call);
    let mut readable = true;
    let mut actions = vec![action];
    let mut simple = true;
    let mut standable = true;
    let mut writes: Vec<Word> = Vec::new();
    if action == Action::Execute {
        if !command.is_empty() {
            let a = analyze(&command, 0, &mut work)?;
            action = a.action;
            actions = a.actions;
            simple = a.simple;
            standable = a.standable;
            writes = a.writes;
        } else {
            readable = false;
        }
    }
    if call.raw_input && call.outside {
        standable = false;
    }
    let named = call.locations.iter().flatten().filter(|p| !p.is_empty());
    let paths = unique_words(named.chain(writes.iter()));
    Ok(Classified {
        action,
        approval: approval_for(action),
        command,
        paths,
        readable,
        actions,
        simple,
        standable,
    })
}

/// `analyzeCommand(command)` (exposed for tests and properties).
pub fn analyze_command(command: &[u16]) -> R<Analysis> {
    analyze(command, 0, &mut Work { left: MAX_WORK })
}

// ── hostsOf ──────────────────────────────────────────────────────────────────

/// What `new URL(rest).hostname` gives for one matched URL.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Host {
    /// The lowercased hostname.
    Name(Word),
    /// `new URL()` throws: the JS records `''`.
    Fails,
    /// The port does not read it with certainty.
    Unsure,
}

/// The hostname WHATWG URL parsing gives `rest` (which starts with `http://` or `https://`, any
/// case, and holds no JS whitespace), within the subset the port reads with certainty.
fn url_host(rest: &[u16]) -> Host {
    let Some(colon) = rest.iter().position(|&c| c == u(':')) else {
        return Host::Unsure;
    };
    // "special authority (ignore) slashes": any run of `/` and `\` after the scheme.
    let mut i = colon + 1;
    while rest.get(i).is_some_and(|&c| c == u('/') || c == u('\\')) {
        i += 1;
    }
    let after = rest.get(i..).unwrap_or(&[]);
    let end = after
        .iter()
        .position(|&c| [u('/'), u('\\'), u('?'), u('#')].contains(&c))
        .unwrap_or(after.len());
    let authority = after.get(..end).unwrap_or(&[]);
    let host_port = match authority.iter().rposition(|&c| c == u('@')) {
        Some(at) => authority.get(at + 1..).unwrap_or(&[]),
        None => authority,
    };
    if host_port.first() == Some(&u('[')) {
        return Host::Unsure;
    }
    let (host, port) = match host_port.iter().position(|&c| c == u(':')) {
        Some(p) => (
            host_port.get(..p).unwrap_or(&[]),
            Some(host_port.get(p + 1..).unwrap_or(&[])),
        ),
        None => (host_port, None),
    };
    if host.is_empty() {
        return Host::Fails;
    }
    if let Some(port) = port {
        if !port.iter().all(|&c| is_digit(c)) {
            return if port.iter().all(|&c| c < 0x80 && c > 0x20) {
                Host::Fails
            } else {
                Host::Unsure
            };
        }
        // Leading zeros are fine; the value must fit 0..=65535.
        let digits: Vec<u16> = port.iter().copied().skip_while(|&c| c == u('0')).collect();
        let value = if digits.len() > 5 {
            u32::MAX
        } else {
            digits
                .iter()
                .fold(0u32, |n, &c| n * 10 + u32::from(c - u('0')))
        };
        if value > 65535 {
            return Host::Fails;
        }
    }
    let labels: Vec<&[u16]> = host.split(|&c| c == u('.')).collect();
    let mut lower = Vec::with_capacity(host.len());
    for (n, label) in labels.iter().enumerate() {
        if label.is_empty()
            || label.first() == Some(&u('-'))
            || label.last() == Some(&u('-'))
            || istarts(label, "xn--")
            || !label
                .iter()
                .all(|&c| is_ascii_letter(c) || is_digit(c) || c == u('_') || c == u('-'))
        {
            return Host::Unsure;
        }
        if n > 0 {
            lower.push(u('.'));
        }
        lower.extend(label.iter().map(|&c| {
            if (0x41..=0x5a).contains(&c) {
                c + 0x20
            } else {
                c
            }
        }));
    }
    // "ends in a number": an IPv4 address the URL parser would rewrite or reject.
    let last = labels.last().copied().unwrap_or(&[]);
    let numeric = last.iter().all(|&c| is_digit(c))
        || (istarts(last, "0x")
            && last
                .iter()
                .skip(2)
                .all(|&c| c < 0x80 && (c as u8).is_ascii_hexdigit()));
    if numeric {
        let canonical = labels.len() == 4
            && labels.iter().all(|l| {
                l.iter().all(|&c| is_digit(c))
                    && l.len() <= 3
                    && (l.len() == 1 || l.first() != Some(&u('0')))
                    && l.iter().fold(0u32, |n, &c| n * 10 + u32::from(c - u('0'))) <= 255
            });
        if !canonical {
            return Host::Unsure;
        }
    }
    Host::Name(lower)
}

/// `hostsOf(command)`: every URL host the command's words name, in order; `None` when one of them
/// is a host the port does not read with certainty.
fn hosts_of(command: &[u16], work: &mut Work) -> R<Option<Vec<Word>>> {
    let lexed = lex(command, work)?;
    let words = lexed
        .segments
        .iter()
        .flatten()
        .chain(lexed.redirects.iter())
        .chain(lexed.nested.iter());
    let mut hosts = Vec::new();
    for word in words {
        work.charge(word.len())?;
        let mut i = 0usize;
        while i < word.len() {
            let at = word.get(i..).unwrap_or(&[]);
            let len = if istarts(at, "https://") {
                8
            } else if istarts(at, "http://") {
                7
            } else {
                i += 1;
                continue;
            };
            let end = at.iter().position(|&c| is_js_space(c)).unwrap_or(at.len());
            match url_host(at.get(..end).unwrap_or(&[])) {
                Host::Name(h) => hosts.push(h),
                Host::Fails => hosts.push(Vec::new()),
                Host::Unsure => return Ok(None),
            }
            i += len;
        }
    }
    Ok(Some(hosts))
}

/// `hostOf(command)` and `hostsOf(command)` (exposed for tests).
pub fn hosts(command: &[u16]) -> R<Option<Vec<Word>>> {
    hosts_of(command, &mut Work { left: MAX_WORK })
}

// ── decide ───────────────────────────────────────────────────────────────────

/// The host's projection of `decide()`'s input.
#[derive(Clone, Debug, Default)]
pub struct DecideInput {
    pub action: Word,
    pub approval: Word,
    pub command: Word,
    pub readable: bool,
    pub paths: Vec<Word>,
    pub actions: Vec<Word>,
    pub simple: bool,
    pub capabilities: Vec<Word>,
    pub domains: Vec<Word>,
    pub in_workspace: Option<bool>,
}

/// `decide()`'s answer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Decision {
    pub decision: &'static str,
    pub reason: Word,
}

fn decision(decision: &'static str, reason: &str) -> Decision {
    Decision {
        decision,
        reason: units(reason),
    }
}

/// code-actions.cjs `decide({ classified, capabilities, domains, inWorkspace })`.
pub fn decide(d: &DecideInput) -> R<Decision> {
    let single = [d.action.clone()];
    let all: &[Word] = if d.actions.is_empty() {
        &single
    } else {
        &d.actions
    };
    let has = |name: &str| all.iter().any(|a| is(a, name));
    let writes = has(Action::Edit.name()) || has(Action::Delete.name());
    if writes && !d.paths.is_empty() && d.in_workspace == Some(false) {
        return Ok(decision(
            "deny",
            "The path is outside this task\u{2019}s workspace.",
        ));
    }
    let missing = all.iter().find(|a| {
        !is(a, Action::None.name())
            && !is(a, Action::Read.name())
            && !d.capabilities.is_empty()
            && !d.capabilities.contains(a)
    });
    // `.find()` hands back the first such class; an empty one is falsy, so nothing is missing.
    if let Some(m) = missing.filter(|m| !m.is_empty()) {
        let mut reason = units("This task was not granted ");
        reason.extend(m.iter().map(|&c| if c == u('_') { u(' ') } else { c }));
        reason.push(u('.'));
        return Ok(Decision {
            decision: "deny",
            reason,
        });
    }
    if is(&d.approval, "never") {
        return Ok(decision("allow", "Read-only."));
    }
    if is(&d.approval, "capability") {
        let mut work = Work { left: MAX_WORK };
        let Some(hosts) = hosts_of(&d.command, &mut work)? else {
            return Err(Refusal::Ambiguous);
        };
        let host = hosts.iter().find(|h| !h.is_empty());
        let listed = |h: &Word| {
            d.domains.iter().any(|dom| {
                h == dom || {
                    let mut suffix = vec![u('.')];
                    suffix.extend_from_slice(dom);
                    h.ends_with(&suffix)
                }
            })
        };
        if d.simple && !hosts.is_empty() && hosts.iter().all(|h| !h.is_empty() && listed(h)) {
            let mut reason = host.cloned().unwrap_or_default();
            reason.extend(units(" is on this task\u{2019}s allowed list."));
            return Ok(Decision {
                decision: "allow",
                reason,
            });
        }
        return Ok(match host {
            Some(h) => {
                let mut reason = units("Network request to ");
                reason.extend_from_slice(h);
                reason.push(u('.'));
                Decision {
                    decision: "ask",
                    reason,
                }
            }
            None => decision("ask", "Network request."),
        });
    }
    Ok(if d.readable {
        decision("ask", "")
    } else {
        decision("ask", "The harness did not say what it would run.")
    })
}

// ── pickOption ───────────────────────────────────────────────────────────────

/// One offered option as `pickOption` reads it (`o.optionId` a string; `o.kind` if a string).
#[derive(Clone, Debug)]
pub struct PermissionOption {
    pub option_id: Word,
    pub kind: Option<Word>,
}

/// code-actions.cjs `pickOption(options, wanted)`: the chosen option id, or `None` (cancelled).
pub fn pick_option(options: &[PermissionOption], wanted: &[u16]) -> Option<Word> {
    let by_kind = |kind: &[u16]| {
        options
            .iter()
            .find(|o| o.kind.as_deref() == Some(kind) || o.option_id == kind)
    };
    let fallback: &str = if starts(wanted, "allow") {
        if is(wanted, "allow_always") {
            "allow_once"
        } else {
            "allow_always"
        }
    } else if is(wanted, "reject_always") {
        "reject_once"
    } else {
        "reject_always"
    };
    by_kind(wanted)
        .or_else(|| by_kind(&units(fallback)))
        .map(|o| o.option_id.clone())
}

// ── wire ─────────────────────────────────────────────────────────────────────

fn string_of(v: &Value) -> R<Word> {
    match v {
        Value::Str(s) => Ok(s.clone()),
        Value::Null => Ok(units("null")),
        Value::Bool(b) => Ok(units(if *b { "true" } else { "false" })),
        Value::Num(n) => {
            const SAFE: f64 = 9_007_199_254_740_991.0;
            if n.is_finite() && n.fract() == 0.0 && n.abs() <= SAFE {
                // An exact integer within ±(2^53 - 1): its decimal digits (-0 prints as 0).
                #[allow(clippy::cast_possible_truncation)]
                let i = *n as i64;
                Ok(units(&i.to_string()))
            } else {
                Err(Refusal::Ambiguous)
            }
        }
        _ => Err(Refusal::Ambiguous),
    }
}

fn command_value(v: &Value) -> R<CommandValue> {
    match v {
        Value::Str(s) => Ok(CommandValue::Text(s.clone())),
        Value::Arr(items) => Ok(CommandValue::List(
            items.iter().map(string_of).collect::<R<Vec<_>>>()?,
        )),
        Value::Deep => Err(Refusal::Input),
        _ => Ok(CommandValue::Other),
    }
}

const COMMAND_KEYS: [&str; 5] = ["command", "cmd", "script", "shell", "commandLine"];

/// The call projection: `{ kind: string|null, rawInput: {command?, cmd?, script?, shell?,
/// commandLine?, args?, noeviaOutsideWorkspace: bool}|null, locations: [string|null] }`.
fn call_from(v: &Value) -> R<Call> {
    let Value::Obj(_) = v else {
        return Err(Refusal::Input);
    };
    let kind = match v.get("kind") {
        Some(Value::Str(s)) => Some(s.clone()),
        Some(Value::Null) => None,
        _ => return Err(Refusal::Input),
    };
    let mut call = Call {
        kind,
        ..Call::default()
    };
    match v.get("rawInput") {
        Some(Value::Null) => {}
        Some(raw @ Value::Obj(_)) => {
            call.raw_input = true;
            for key in COMMAND_KEYS {
                if let Some(value) = raw.get(key) {
                    call.values.push((command_value(value)?, false));
                }
            }
            if let Some(value) = raw.get("args") {
                call.values.push((command_value(value)?, true));
            }
            call.outside = match raw.get("noeviaOutsideWorkspace") {
                Some(Value::Bool(b)) => *b,
                _ => return Err(Refusal::Input),
            };
        }
        _ => return Err(Refusal::Input),
    }
    let Some(Value::Arr(locations)) = v.get("locations") else {
        return Err(Refusal::Input);
    };
    for l in locations {
        call.locations.push(match l {
            Value::Str(s) => Some(s.clone()),
            Value::Null => None,
            _ => return Err(Refusal::Input),
        });
    }
    Ok(call)
}

fn strings(v: Option<&Value>) -> R<Vec<Word>> {
    let Some(Value::Arr(items)) = v else {
        return Err(Refusal::Input);
    };
    items
        .iter()
        .map(|i| i.as_str().map(<[u16]>::to_vec).ok_or(Refusal::Input))
        .collect()
}

fn string(v: Option<&Value>) -> R<Word> {
    v.and_then(Value::as_str)
        .map(<[u16]>::to_vec)
        .ok_or(Refusal::Input)
}

fn boolean(v: Option<&Value>) -> R<bool> {
    match v {
        Some(Value::Bool(b)) => Ok(*b),
        _ => Err(Refusal::Input),
    }
}

/// `[classified{action, approval, command, readable, paths, actions, simple}, capabilities,
/// domains, inWorkspace (true|false|null)]`.
fn decide_from(args: &[Value]) -> R<DecideInput> {
    let [c, caps, domains, inside] = args else {
        return Err(Refusal::Input);
    };
    let Value::Obj(_) = c else {
        return Err(Refusal::Input);
    };
    Ok(DecideInput {
        action: string(c.get("action"))?,
        approval: string(c.get("approval"))?,
        command: string(c.get("command"))?,
        readable: boolean(c.get("readable"))?,
        paths: strings(c.get("paths"))?,
        actions: strings(c.get("actions"))?,
        simple: boolean(c.get("simple"))?,
        capabilities: strings(Some(caps))?,
        domains: strings(Some(domains))?,
        in_workspace: match inside {
            Value::Bool(b) => Some(*b),
            Value::Null => None,
            _ => return Err(Refusal::Input),
        },
    })
}

fn options_from(args: &[Value]) -> R<(Vec<PermissionOption>, Word)> {
    let [Value::Arr(items), Value::Str(wanted)] = args else {
        return Err(Refusal::Input);
    };
    let mut options = Vec::new();
    for o in items {
        match o {
            Value::Null => {}
            Value::Obj(_) => options.push(PermissionOption {
                option_id: string(o.get("optionId"))?,
                kind: match o.get("kind") {
                    Some(Value::Str(s)) => Some(s.clone()),
                    Some(Value::Null) => None,
                    _ => return Err(Refusal::Input),
                },
            }),
            _ => return Err(Refusal::Input),
        }
    }
    Ok((options, wanted.clone()))
}

fn push_names(out: &mut Vec<u8>, names: impl Iterator<Item = &'static str>) {
    out.push(b'[');
    for (i, n) in names.enumerate() {
        if i > 0 {
            out.push(b',');
        }
        json::push_ascii(out, n);
    }
    out.push(b']');
}

fn push_words(out: &mut Vec<u8>, words: &[Word]) {
    out.push(b'[');
    for (i, w) in words.iter().enumerate() {
        if i > 0 {
            out.push(b',');
        }
        json::push_str(out, w);
    }
    out.push(b']');
}

/// `JSON.stringify(classify(call))`.
pub fn classified_json(c: &Classified) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(b"{\"action\":");
    json::push_ascii(&mut out, c.action.name());
    out.extend_from_slice(b",\"approval\":");
    json::push_ascii(&mut out, c.approval);
    out.extend_from_slice(b",\"command\":");
    json::push_str(&mut out, &c.command);
    out.extend_from_slice(b",\"paths\":");
    push_words(&mut out, &c.paths);
    out.extend_from_slice(b",\"readable\":");
    out.extend_from_slice(if c.readable { b"true" } else { b"false" });
    out.extend_from_slice(b",\"actions\":");
    push_names(&mut out, c.actions.iter().map(|a| a.name()));
    out.extend_from_slice(b",\"simple\":");
    out.extend_from_slice(if c.simple { b"true" } else { b"false" });
    out.extend_from_slice(b",\"standable\":");
    out.extend_from_slice(if c.standable { b"true" } else { b"false" });
    out.push(b'}');
    out
}

fn run(op: u8, args: &[Value]) -> R<Vec<u8>> {
    match op {
        1 => {
            let [call] = args else {
                return Err(Refusal::Input);
            };
            Ok(classified_json(&classify(&call_from(call)?)?))
        }
        2 => {
            let d = decide(&decide_from(args)?)?;
            let mut out = Vec::new();
            out.extend_from_slice(b"{\"decision\":");
            json::push_ascii(&mut out, d.decision);
            out.extend_from_slice(b",\"reason\":");
            json::push_str(&mut out, &d.reason);
            out.push(b'}');
            Ok(out)
        }
        3 => {
            let (options, wanted) = options_from(args)?;
            let mut out = Vec::new();
            match pick_option(&options, &wanted) {
                Some(id) => {
                    out.extend_from_slice(b"{\"outcome\":\"selected\",\"optionId\":");
                    json::push_str(&mut out, &id);
                    out.push(b'}');
                }
                None => out.extend_from_slice(b"{\"outcome\":\"cancelled\"}"),
            }
            Ok(out)
        }
        _ => Err(Refusal::Input),
    }
}

/// The `code_actions` wasm call: input `u8(op)` and UTF-8 JSON args (at most
/// [`MAX_INPUT_BYTES`]). Op 1 `[call]` → `JSON.stringify(classify(call))`; op 2 `[classified,
/// capabilities, domains, inWorkspace]` → `{"decision","reason"}`; op 3 `[options, wanted]` →
/// `{"outcome":"selected","optionId"}` / `{"outcome":"cancelled"}`. Status 1 with
/// `{"error":"input"|"too_large"|"ambiguous"}` when refused.
pub fn call(input: &[u8]) -> (u32, Vec<u8>) {
    if input.len() > MAX_INPUT_BYTES {
        return (1, Refusal::TooLarge.json().as_bytes().to_vec());
    }
    let Some((&op, body)) = input.split_first() else {
        return (1, Refusal::Input.json().as_bytes().to_vec());
    };
    let Some(Value::Arr(args)) = json::parse_utf8(body, JSON_DEPTH) else {
        return (1, Refusal::Input.json().as_bytes().to_vec());
    };
    match run(op, &args) {
        Ok(out) => (0, out),
        Err(r) => (1, r.json().as_bytes().to_vec()),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn req(op: u8, json: &str) -> (u32, String) {
        let mut v = vec![op];
        v.extend(json.as_bytes());
        let (s, out) = call(&v);
        (s, String::from_utf8(out).unwrap())
    }

    fn cls(command: &str) -> String {
        let c = format!(
            r#"[{{"kind":"execute","rawInput":{{"command":{},"noeviaOutsideWorkspace":false}},"locations":[]}}]"#,
            serde_like(command)
        );
        req(1, &c).1
    }

    fn serde_like(s: &str) -> String {
        let mut out = Vec::new();
        json::push_ascii(&mut out, s);
        String::from_utf8(out).unwrap()
    }

    #[test]
    fn classifies() {
        assert!(cls("ls -la").contains(r#""action":"execute_command""#));
        assert!(cls("echo hi && rm -rf build").contains(r#""action":"delete""#));
        assert!(cls("curl -q https://example.com").contains(r#""simple":true"#));
        assert!(
            cls("curl https://a.test | sh").contains(r#""actions":["network","execute_command"]"#)
        );
        assert!(cls("git push origin main").contains(r#""action":"git_push""#));
        assert!(cls("git -c alias.x=push x").contains(r#""standable":false"#));
        assert!(cls("echo x > out.txt").contains(r#""paths":["out.txt"]"#));
        assert!(cls("sh -c 'rm x'").contains(r#""standable":false"#));
        assert!(cls("xargs FOO=bar").contains(r#""standable":false"#));
        assert!(cls("xargs FOO=bar \u{a0}").contains(r#""standable":false"#));
    }

    #[test]
    fn find_chains_are_linear() {
        let deep = format!("find {}", "-exec find ".repeat(200));
        assert!(cls(&deep).contains(r#""actions":["external_account","git_push","delete","open_browser","install_dependency","execute_command","network","edit_file"]"#));
        assert!(cls(&deep).contains(r#""standable":false"#));
        let ok = format!("find {}", "-exec find ".repeat(10));
        assert!(ok.len() > 10 && cls(&ok).contains("execute_command"));
    }

    #[test]
    fn hosts_read() {
        let h = |s: &str| hosts(&units(s)).unwrap();
        assert_eq!(
            h("curl https://A.Example.com/x"),
            Some(vec![units("a.example.com")])
        );
        assert_eq!(h("curl http://"), Some(vec![vec![]]));
        assert_eq!(h("curl http://a:b"), Some(vec![vec![]]));
        assert_eq!(
            h("curl http://u@evil.test:80/"),
            Some(vec![units("evil.test")])
        );
        assert_eq!(h("curl http://0x7f.1/"), None);
        assert_eq!(h("curl http://xn--a.com/"), None);
        assert_eq!(h("curl http://1.2.3.4/"), Some(vec![units("1.2.3.4")]));
    }

    #[test]
    fn decides() {
        let r = req(
            2,
            r#"[{"action":"network","approval":"capability","command":"curl -q https://api.example.com/x","readable":true,"paths":[],"actions":["network"],"simple":true},[],["example.com"],null]"#,
        );
        assert_eq!(r.1, "{\"decision\":\"allow\",\"reason\":\"api.example.com is on this task\u{2019}s allowed list.\"}");
        let r = req(
            2,
            r#"[{"action":"edit_file","approval":"always","command":"","readable":true,"paths":["/x"],"actions":["edit_file"],"simple":true},[],[],false]"#,
        );
        assert!(r.1.contains("deny"));
    }

    #[test]
    fn picks() {
        let r = req(
            3,
            r#"[[{"optionId":"a","kind":"allow_once"},null],"allow_always"]"#,
        );
        assert_eq!(r.1, r#"{"outcome":"selected","optionId":"a"}"#);
        let r = req(
            3,
            r#"[[{"optionId":"a","kind":"allow_once"}],"reject_once"]"#,
        );
        assert_eq!(r.1, r#"{"outcome":"cancelled"}"#);
    }
}
