//! Shipped-binary feature proof (port of dockerfile-rs-features.cjs): active Dockerfile instructions, the final
//! stage's FROM chain, the one COPY that writes the shipped binary, and the restricted front build program that
//! compiles it. Nothing is executed; anything outside the supported shapes refuses.

use std::collections::HashSet;
use crate::js;

const BIN: &str = "/usr/local/bin/noevia-server";
const PKG: &str = "noevia-server";
const PRODUCING: [&str; 9] = ["build", "b", "install", "rustc", "run", "r", "test", "t", "bench"];
const NONPRODUCING: [&str; 12] =
    ["fetch", "metadata", "fmt", "check", "c", "clippy", "doc", "d", "tree", "vendor", "version", "help"];

type Fail = String;

fn fail<T>(message: impl AsRef<str>) -> Result<T, Fail> {
    Err(format!("Dockerfile: {}", message.as_ref()))
}

#[derive(Debug, Clone)]
pub(crate) struct Instruction {
    pub kw: String,
    pub args: String,
}

/// Active instructions: comment lines vanish (also inside a continuation), continuations are joined.
pub(crate) fn instructions(text: &str) -> Result<Vec<Instruction>, Fail> {
    let physical: Vec<&str> = text.split('\n').map(|l| l.strip_suffix('\r').unwrap_or(l)).collect();
    let directive = js_re!(r"^\s*#\s*([eE][sS][cC][aA][pP][eE]|[sS][yY][nN][tT][aA][xX])\s*=")?;
    for l in &physical {
        if directive.is_match(l) && l.to_ascii_lowercase().contains("escape") {
            return fail("a parser directive changes the escape character");
        }
    }
    let comment = js_re!(r"^\s*#")?;
    let mut out = Vec::new();
    let mut cur: Option<String> = None;
    for raw in physical {
        let line = js::trim_end(raw);
        if comment.is_match(line) {
            continue;
        }
        if cur.is_none() && js::trim(line).is_empty() {
            continue;
        }
        let cont = line.ends_with('\\');
        let body = if cont { line.get(..line.len() - 1).unwrap_or("") } else { line };
        let mut joined = cur.take().unwrap_or_default();
        joined.push_str(body);
        if cont {
            cur = Some(joined);
            continue;
        }
        out.push(js::trim(&joined).to_string());
    }
    if cur.is_some() {
        return fail("the file ends inside a continued instruction");
    }
    let split = js_re!(r"^(\S+)\s*([\s\S]*)$")?;
    let mut list = Vec::new();
    for s in out.into_iter().filter(|s| !s.is_empty()) {
        let Some(m) = split.captures(&s) else { return fail("unreadable instruction") };
        let kw = m.get(1).map_or("", |g| g.as_str()).to_uppercase();
        let args = m.get(2).map_or("", |g| g.as_str()).to_string();
        list.push(Instruction { kw, args });
    }
    Ok(list)
}

#[derive(Debug, Clone)]
struct Stage {
    base: String,
    name: Option<String>,
    ins: Vec<Instruction>,
    index: usize,
}

fn stages(text: &str) -> Result<Vec<Stage>, Fail> {
    let from = js_re!(r"^(?:--\S+\s+)*(\S+)(?:\s+[aA][sS]\s+(\S+))?\s*$")?;
    let mut list: Vec<Stage> = Vec::new();
    for ins in instructions(text)? {
        // Heredoc bodies are data, and can contain apparent FROM/COPY instructions. Refuse them file-wide before
        // interpreting stage ancestry or binary copies.
        if matches!(ins.kw.as_str(), "RUN" | "COPY" | "ADD") && ins.args.contains("<<") {
            return fail("heredoc instruction data cannot prove the shipped binary");
        }
        if ins.kw == "FROM" {
            if ins.args.starts_with("--") {
                return fail("unsupported FROM options");
            }
            let Some(m) = from.captures(&ins.args) else { return fail(format!("unreadable FROM {}", ins.args)) };
            let base = m.get(1).map_or("", |g| g.as_str()).to_string();
            let name = m.get(2).map(|g| g.as_str().to_lowercase());
            let index = list.len();
            list.push(Stage { base, name, ins: Vec::new(), index });
        } else if let Some(last) = list.last_mut() {
            last.ins.push(ins);
        } else if ins.kw != "ARG" {
            return fail(format!("{} before the first FROM", ins.kw));
        }
    }
    if list.is_empty() {
        return fail("no FROM");
    }
    let mut seen = HashSet::new();
    for s in &list {
        if let Some(name) = &s.name {
            if !seen.insert(name.clone()) {
                return fail(format!("stage {name} is defined twice"));
            }
        }
    }
    Ok(list)
}

fn label(s: &Stage) -> String {
    s.name.clone().unwrap_or_else(|| s.index.to_string())
}

/// The stage plus the stages it inherits from, root first. A base that is not an earlier stage name is an external
/// image and ends the chain.
fn chain_of<'a>(all: &'a [Stage], stage: &'a Stage) -> Result<Vec<&'a Stage>, Fail> {
    let mut chain = vec![stage];
    let mut cur = stage;
    loop {
        if cur.base.contains('$') {
            return fail(format!("stage {} has an unresolved base image {}", label(cur), cur.base));
        }
        let base = cur.base.to_lowercase();
        let Some(parent) = all.iter().find(|s| s.name.as_deref() == Some(base.as_str())) else { break };
        if parent.index >= cur.index {
            return fail(format!("stage {} inherits from a stage defined later", label(cur)));
        }
        chain.insert(0, parent);
        cur = parent;
    }
    Ok(chain)
}

// ---- shell -------------------------------------------------------------------------------------------------------

#[derive(Debug, Clone, Default)]
struct Word {
    v: String,
    raw: String,
    dollar: bool,
    glob: bool,
}

#[derive(Debug, Clone)]
enum Tok {
    Word(Word),
    Op(String),
    Redir { v: String, needs_target: bool },
}

#[derive(Debug, Clone, Default)]
struct Segment {
    words: Vec<Word>,
    redirs: Vec<(String, Option<String>)>,
    before: Option<String>,
    after: Option<String>,
}

/// Words and operators of one RUN command. Quotes are honoured; `$`, backticks and globs are flagged on the word so
/// callers can refuse them.
fn shell(src: &str) -> Result<Vec<Segment>, Fail> {
    let s: Vec<char> = src.chars().collect();
    let n = s.len();
    let at = |i: usize| s.get(i).copied();
    let slice = |a: usize, b: usize| -> String { s.get(a..b.min(n)).map(|c| c.iter().collect()).unwrap_or_default() };
    let find = |from: usize, c: char| (from..n).find(|&k| at(k) == Some(c));
    let mut toks: Vec<Tok> = Vec::new();
    let mut w: Option<Word> = None;
    let flush = |w: &mut Option<Word>, toks: &mut Vec<Tok>| {
        if let Some(word) = w.take() {
            toks.push(Tok::Word(word));
        }
    };
    let mut i = 0;
    while let Some(c) = at(i) {
        if matches!(c, ' ' | '\t' | '\n') {
            flush(&mut w, &mut toks);
            i += 1;
            continue;
        }
        if c == '#' && w.is_none() {
            return fail("shell comment inside a RUN command");
        }
        if (c == '&' && at(i + 1) == Some('&')) || (c == '|' && at(i + 1) == Some('|')) {
            flush(&mut w, &mut toks);
            toks.push(Tok::Op(slice(i, i + 2)));
            i += 2;
            continue;
        }
        let after_redirect = i > 0 && matches!(at(i - 1), Some('<' | '>'));
        if matches!(c, '|' | ';' | '(' | ')') || (c == '&' && !after_redirect) {
            flush(&mut w, &mut toks);
            toks.push(Tok::Op(c.to_string()));
            i += 1;
            continue;
        }
        if c == '>' || c == '<' {
            let mut pre = String::new();
            match w.take() {
                Some(word) if !word.v.is_empty() && word.v.chars().all(|d| d.is_ascii_digit()) => pre = word.v,
                other => {
                    w = other;
                    flush(&mut w, &mut toks);
                }
            }
            let mut j = i + 1;
            if at(j) == Some(c) {
                j += 1;
            }
            let mut target = None;
            if at(j) == Some('&') {
                let digits = (j + 1..n).take_while(|&k| at(k).is_some_and(|d| d.is_ascii_digit())).count();
                let len = if digits > 0 {
                    digits
                } else if at(j + 1) == Some('-') {
                    1
                } else {
                    return fail("unreadable redirection");
                };
                target = Some(slice(j, j + 1 + len));
                j += 1 + len;
            }
            let op = slice(i, j);
            let op = op.split('&').next().unwrap_or("").to_string();
            let needs_target = target.is_none();
            toks.push(Tok::Redir { v: format!("{pre}{op}{}", target.unwrap_or_default()), needs_target });
            i = j;
            continue;
        }
        let word = w.get_or_insert_with(Word::default);
        match c {
            '\'' => {
                let Some(e) = find(i + 1, '\'') else { return fail("unterminated quote in a RUN command") };
                let lit = slice(i + 1, e);
                if lit.contains(['$', '`']) {
                    word.dollar = true;
                }
                word.v.push_str(&lit);
                word.raw.push_str(&slice(i, e + 1));
                i = e + 1;
            }
            '"' => {
                let mut j = i + 1;
                let mut lit = String::new();
                loop {
                    let Some(d) = at(j) else { return fail("unterminated quote in a RUN command") };
                    if d == '\\' && j + 1 < n {
                        lit.extend(at(j + 1));
                        j += 2;
                        continue;
                    }
                    if d == '"' {
                        break;
                    }
                    if d == '$' || d == '`' {
                        word.dollar = true;
                    }
                    lit.push(d);
                    j += 1;
                }
                word.v.push_str(&lit);
                word.raw.push_str(&slice(i, j + 1));
                i = j + 1;
            }
            '\\' => {
                word.v.extend(at(i + 1));
                word.raw.push_str(&slice(i, i + 2));
                i += 2;
            }
            '`' => {
                let Some(e) = find(i + 1, '`') else { return fail("unterminated backtick") };
                word.dollar = true;
                word.v.push_str(&slice(i, e + 1));
                word.raw.push_str(&slice(i, e + 1));
                i = e + 1;
            }
            '$' if at(i + 1) == Some('(') => {
                word.dollar = true;
                let mut depth = 0usize;
                let mut j = i + 1;
                while j < n {
                    match at(j) {
                        Some('(') => depth += 1,
                        Some(')') => {
                            depth = depth.saturating_sub(1);
                            if depth == 0 {
                                break;
                            }
                        }
                        _ => {}
                    }
                    j += 1;
                }
                if j >= n {
                    return fail("unterminated $(");
                }
                word.v.push_str(&slice(i, j + 1));
                word.raw.push_str(&slice(i, j + 1));
                i = j + 1;
            }
            '$' if at(i + 1) == Some('{') => {
                word.dollar = true;
                let Some(e) = find(i, '}') else { return fail("unterminated ${") };
                word.v.push_str(&slice(i, e + 1));
                word.raw.push_str(&slice(i, e + 1));
                i = e + 1;
            }
            _ => {
                if c == '$' {
                    word.dollar = true;
                }
                if matches!(c, '*' | '?' | '[') {
                    word.glob = true;
                }
                word.v.push(c);
                word.raw.push(c);
                i += 1;
            }
        }
    }
    flush(&mut w, &mut toks);
    // Segments: command groups between operators.
    let mut segs = Vec::new();
    let mut cur = Segment::default();
    let mut it = toks.into_iter();
    while let Some(t) = it.next() {
        match t {
            Tok::Op(v) => {
                cur.after = Some(v.clone());
                segs.push(std::mem::take(&mut cur));
                cur.before = Some(v);
            }
            Tok::Redir { v, needs_target: true } => match it.next() {
                Some(Tok::Word(target)) => cur.redirs.push((v, Some(target.v))),
                _ => return fail("redirection without a target"),
            },
            Tok::Redir { v, needs_target: false } => cur.redirs.push((v, None)),
            Tok::Word(word) => cur.words.push(word),
        }
    }
    segs.push(cur);
    Ok(segs
        .into_iter()
        .filter(|s| !s.words.is_empty() || !s.redirs.is_empty() || s.before.is_some() || s.after.is_some())
        .collect())
}

// ---- cargo -------------------------------------------------------------------------------------------------------

#[derive(Debug, Default)]
struct CargoBuild {
    sub: String,
    packages: Vec<String>,
    bins: Vec<String>,
    features: Vec<String>,
    no_default: bool,
    options: Vec<String>,
}

/// `None` for a non-producing subcommand (`cargo check`); refusal for anything that might build the binary in a way
/// this proof does not understand.
fn parse_cargo(seg: &Segment, at: usize) -> Result<Option<CargoBuild>, Fail> {
    let w = seg.words.get(at + 1..).unwrap_or_default();
    if let Some(x) = seg.words.iter().find(|x| x.dollar || x.glob) {
        return fail(format!("a cargo command contains an expansion or glob ({}); refusing", x.raw));
    }
    let mut k = 0;
    let sub = match w.first() {
        Some(x) if !x.v.starts_with('-') && !x.v.starts_with('+') => x.v.clone(),
        other => {
            return fail(format!(
                "cargo is called with an unsupported global option ({})",
                other.map_or("none", |x| x.v.as_str())
            ))
        }
    };
    k += 1;
    if NONPRODUCING.contains(&sub.as_str()) {
        return Ok(None);
    }
    if !PRODUCING.contains(&sub.as_str()) {
        return fail(format!("cargo subcommand \"{sub}\" may build the binary and is not understood"));
    }
    let mut info = CargoBuild { sub: sub.clone(), ..CargoBuild::default() };
    while let Some(word) = w.get(k) {
        k += 1;
        let a = word.v.as_str();
        let (name, inline) = match a.find('=') {
            Some(eq) if a.starts_with("--") && eq > 0 => (a.get(..eq).unwrap_or(a), a.get(eq + 1..)),
            _ => (a, None),
        };
        let mut value = |k: &mut usize| -> Result<String, Fail> {
            if let Some(v) = inline {
                return Ok(v.to_string());
            }
            match w.get(*k) {
                Some(v) if !v.v.starts_with('-') => {
                    *k += 1;
                    Ok(v.v.clone())
                }
                _ => fail(format!("cargo {name} has no value")),
            }
        };
        match name {
            "--release" | "--locked" | "--no-default-features" => {
                if inline.is_some() {
                    return fail("boolean cargo option has a value");
                }
                if name == "--no-default-features" {
                    info.no_default = true;
                } else {
                    info.options.push(name.to_string());
                }
            }
            "-p" | "--package" => info.packages.push(value(&mut k)?),
            "--bin" => info.bins.push(value(&mut k)?),
            "--features" | "-F" => info.features.extend(value(&mut k)?.split(',').map(str::to_string)),
            _ => return fail(format!("cargo {sub} uses an option outside the supported set: {a}")),
        }
    }
    if sub != "build" && sub != "b" {
        return fail(format!("cargo {sub} could produce {PKG} and is not a supported build command"));
    }
    Ok(Some(info))
}

// ---- paths -------------------------------------------------------------------------------------------------------

fn resolve_dir(workdir: &str, p: &str, what: &str) -> Result<String, Fail> {
    if p.contains('$') {
        return fail(format!("{what} uses an unresolved variable ({p})"));
    }
    Ok(if p.starts_with('/') { js::normalize(p) } else { js::join(workdir, p) })
}

/// Does a COPY/ADD (sources, dest) possibly write `target`?
fn writes(srcs: &[String], dest: &str, target: &str) -> bool {
    let dir = dest.ends_with('/')
        || dest == "."
        || dest.ends_with("/.")
        || srcs.len() > 1
        || srcs.iter().any(|s| s.contains(['*', '?', '[']) || s.ends_with('/') || s.ends_with("/.") || s == ".");
    let trimmed = dest.trim_end_matches('/');
    let d = if trimmed.is_empty() { "/" } else { trimmed };
    if d == target {
        return true;
    }
    let prefix = if d == "/" { "/".to_string() } else { format!("{d}/") };
    if target.starts_with(&prefix) {
        return true; // a directory copy can place it
    }
    dir && srcs.iter().any(|s| js::join(d, js::basename(s)) == target)
}

struct Copy {
    from: Option<String>,
    srcs: Vec<String>,
    dest: String,
}

fn copy_of(ins: &Instruction, workdir: &str) -> Result<Copy, Fail> {
    if js::trim(&ins.args).starts_with('[') {
        return fail(format!("{} in JSON form cannot be checked", ins.kw));
    }
    let mut words: Vec<String> = ins.args.split(js::is_space).filter(|w| !w.is_empty()).map(str::to_string).collect();
    let mut from = None;
    while words.first().is_some_and(|w| w.starts_with("--")) {
        let first = words.remove(0);
        match first.strip_prefix("--from=") {
            Some(f) if !f.is_empty() && !f.contains(js::is_line_terminator) && ins.kw == "COPY" => {
                from = Some(f.to_string())
            }
            _ => return fail(format!("{} uses an option other than --from ({first})", ins.kw)),
        }
    }
    if words.len() < 2 {
        return fail(format!("unreadable {}", ins.kw));
    }
    if let Some(x) = words.iter().find(|x| x.contains('$')) {
        return fail(format!("{} uses an unresolved variable ({x})", ins.kw));
    }
    let dest = words.pop().unwrap_or_default();
    let mut d = if dest.starts_with('/') { js::normalize(&dest) } else { js::join(workdir, &dest) };
    if dest == "." && !d.ends_with('/') {
        d.push('/');
    }
    Ok(Copy { from, srcs: words, dest: d })
}

// ---- the front build ---------------------------------------------------------------------------------------------

/// The proven literal feature list of the shipped build and whether it disables default features.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Proof {
    pub features: Vec<String>,
    pub no_default: bool,
    pub build_stage: Option<String>,
}

/// The complete supported front RUN grammar. The fixed selector language is evaluated against the same Cargo
/// snapshot, never by invoking a shell. Only its proven literal result may replace the one known "$features" argument.
fn front_run(text: &str, cargo: &str) -> Result<Proof, Fail> {
    if js_re!(r"^\s*(--|\[)")?.is_match(text) || text.contains("<<") {
        return fail("unsupported RUN form");
    }
    let prefix = js_re!(concat!(
        r"^set -e;\s*mkdir -p /out;\s*if \[ -f bins/noevia-server/Cargo%DOT%toml \]; then\s*",
        r"features=([A-Za-z0-9_,-]+);\s*"
    ))?;
    let command = if let Some(p) = prefix.captures(text) {
        let mut features: Vec<String> = Vec::new();
        for f in p.get(1).map_or("", |g| g.as_str()).split(',') {
            if !features.iter().any(|x| x == f) {
                features.push(f.to_string());
            }
        }
        let mut tail = text.get(p.get(0).map_or(0, |g| g.end())..).unwrap_or("");
        let selector = js_re!(concat!(
            r#"^if grep -(qx|q) '([^']+)' bins/noevia-server/Cargo\.toml; then features="\$features,([A-Za-z0-9_-]+)"; "#,
            r"fi;\s*"
        ))?;
        while tail.starts_with("if grep ") {
            let Some(m) = selector.captures(tail) else { return fail("unsupported feature selector") };
            let mode = m.get(1).map_or("", |g| g.as_str());
            let predicate = m.get(2).map_or("", |g| g.as_str());
            let name = m.get(3).map_or("", |g| g.as_str());
            let exact = predicate == format!("{name} = \\[\\]");
            let head = predicate == format!("^{name} = ");
            if (mode == "qx" && !exact) || (mode == "q" && !head) {
                return fail("unsupported feature predicate");
            }
            let hit = cargo.split('\n').any(|line| {
                if exact {
                    line == format!("{name} = []")
                } else {
                    line.starts_with(&format!("{name} = "))
                }
            });
            if hit && !features.iter().any(|x| x == name) {
                features.push(name.to_string());
            }
            tail = tail.get(m.get(0).map_or(0, |g| g.end())..).unwrap_or("");
        }
        let program = js_re!(concat!(
            r"^(cargo[\s\S]*?);\s*cp target/release/noevia-server /out/noevia-server;\s*",
            r"/out/noevia-server 2>&1 \| grep -q NOEVIA_LEGACY_UPSTREAM;\s*else\s*",
            r#"echo "noevia-rs in this tree has no bins/noevia-server: the image ships without the Rust front";\s*fi$"#
        ))?;
        let Some(m) = program.captures(tail) else { return fail("unsupported complete front build/output program") };
        m.get(1).map_or("", |g| g.as_str()).replacen("\"$features\"", &features.join(","), 1)
    } else {
        // Literal unconditional build form, also used for the adversarial probe suite.
        let literal = js_re!(r"^(?:mkdir -p /out && )?(cargo[\s\S]*?) && cp target/release/noevia-server /out/noevia-server$")?;
        let Some(m) = literal.captures(text) else { return fail("unsupported unconditional front build/output program") };
        m.get(1).map_or("", |g| g.as_str()).to_string()
    };
    let segments = shell(&command)?;
    let plain = match segments.as_slice() {
        [only] => {
            only.redirs.is_empty()
                && only.before.is_none()
                && only.after.is_none()
                && only.words.first().is_some_and(|w| w.v == "cargo")
        }
        _ => false,
    };
    let Some(segment) = segments.first().filter(|_| plain) else {
        return fail("exactly one plain producing cargo command is required");
    };
    let build = match parse_cargo(segment, 0)? {
        Some(b)
            if (b.sub == "build" || b.sub == "b")
                && b.packages.len() == 1
                && b.packages.first().is_some_and(|p| p == PKG)
                && b.bins.iter().all(|x| x == PKG)
                && b.options.iter().any(|o| o == "--release") =>
        {
            b
        }
        _ => return fail("unsupported binary/package build"),
    };
    let item = js_re!(r"^(?:noevia-server/)?([A-Za-z0-9_-]+)$")?;
    let mut features = Vec::new();
    for f in &build.features {
        let Some(m) = item.captures(f) else { return fail("feature must be a literal local/package feature") };
        features.push(m.get(1).map_or("", |g| g.as_str()).to_string());
    }
    Ok(Proof { features, no_default: build.no_default, build_stage: None })
}

fn prove_build(all: &[Stage], source: &Stage, cargo: &str) -> Result<Proof, Fail> {
    let chain = chain_of(all, source)?;
    // Restrict the external toolchain and setup, including every instruction in all ancestors. An inherited stage
    // cannot hide a second build or output overwrite.
    let base_ok = chain.first().is_some_and(|s| js_re!(r"^rust:[0-9]+(?:\.[0-9]+)*-slim-bookworm$").is_ok_and(|r| r.is_match(&s.base)));
    if !base_ok {
        return fail("unsupported front base");
    }
    let ins: Vec<&Instruction> = chain.iter().flat_map(|s| s.ins.iter()).collect();
    let env = js_re!(r"^RUSTUP_TOOLCHAIN=[0-9]+(?:\.[0-9]+)* CARGO_TERM_COLOR=never$")?;
    match ins.as_slice() {
        [e, c, w, r]
            if e.kw == "ENV"
                && env.is_match(&e.args)
                && c.kw == "COPY"
                && c.args == "rs/ /src/"
                && w.kw == "WORKDIR"
                && w.args == "/src"
                && r.kw == "RUN" =>
        {
            front_run(&r.args, cargo)
        }
        _ => fail("unsupported complete front stage chain"),
    }
}

/// `proveFeatures(dockerfile, cargo)`: the feature list the shipped `/usr/local/bin/noevia-server` was built with.
pub(crate) fn prove_features(text: &str, cargo: &str) -> Result<Proof, Fail> {
    let all = stages(text)?;
    let Some(last) = all.last() else { return fail("no FROM") };
    let chain = chain_of(&all, last)?;
    if chain.first().map(|s| s.base.as_str()) != Some("node:22-bookworm") {
        return fail("unsupported runtime base");
    }
    let touches = js_re!(r"noevia-server|/usr/local/bin|/usr/local(?-u:\b)")?;
    let mut workdir = "/".to_string();
    let mut hit: Option<(&Instruction, Copy, &Stage)> = None;
    let mut count = 0;
    for st in &chain {
        for ins in &st.ins {
            if ins.kw == "ONBUILD" || ins.kw == "SHELL" {
                return fail(format!("{} in runtime chain", ins.kw));
            }
            if ins.kw == "WORKDIR" {
                workdir = resolve_dir(&workdir, js::trim(&ins.args), "WORKDIR")?;
                continue;
            }
            if ins.kw == "RUN" && touches.is_match(&ins.args) {
                return fail("runtime RUN may overwrite binary");
            }
            if ins.kw != "COPY" && ins.kw != "ADD" {
                continue;
            }
            let c = copy_of(ins, &workdir)?;
            if writes(&c.srcs, &c.dest, BIN) {
                count += 1;
                hit = Some((ins, c, st));
            }
        }
    }
    let Some((ins, c, stage)) = hit.filter(|_| count == 1) else {
        return fail("runtime binary must be written exactly once");
    };
    let single = |src: &str, dest: &str| c.srcs.len() == 1 && c.srcs.first().is_some_and(|s| s == src) && c.dest == dest;
    let output = single("/out/", "/usr/local/bin/") || single("/out/noevia-server", BIN);
    let Some(from) = c.from.as_deref().filter(|_| ins.kw == "COPY" && output) else {
        return fail("runtime must COPY the proven output from one stage");
    };
    let from = from.to_lowercase();
    let Some(source) = all.iter().find(|s| s.name.as_deref() == Some(from.as_str())).filter(|s| s.index < stage.index)
    else {
        return fail("runtime copy must name an earlier local stage");
    };
    let mut proof = prove_build(&all, source, cargo)?;
    proof.build_stage = source.name.clone();
    Ok(proof)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn comment_lines_vanish_inside_continuations() {
        let ins = instructions("# RUN bogus\nRUN cargo \\\n# continued comment\n build").unwrap_or_default();
        assert_eq!(ins.first().map(|i| i.args.as_str()), Some("cargo  build"));
    }

    #[test]
    fn shell_splits_operators_and_flags_expansions() {
        let segs = shell("a && b 2>&1 | c > out; d \"$x\"").unwrap_or_default();
        assert_eq!(segs.len(), 4);
        assert!(segs.get(3).is_some_and(|s| s.words.get(1).is_some_and(|w| w.dollar)));
        assert!(segs.get(1).is_some_and(|s| s.redirs.first().is_some_and(|r| r.0 == "2>&1")));
    }
}
