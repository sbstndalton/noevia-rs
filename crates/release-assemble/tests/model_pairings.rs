//! `core_requires` (`src/core_requires.rs`) and the three track R11 model pairings (`src/model_pairings.rs`)
//! against their verbatim shell (`fixtures/assemble-release-model-pairings-excerpt.sh`, with `code_has` from the
//! guards excerpt), run under `set -euo pipefail`, `LC_ALL=C`, with the script's `die`:
//! - `core_requires` over generated JavaScript (comments, strings, templates, regex literals, escapes, broken
//!   literals, BOM, shebang, bytes that are not UTF-8): the same `yes`/`no`/`invalid` as node.
//! - each pairing over generated release trees: the same verdict and refusal text.
//!
//! The shell side needs bash and node (its `core_requires` runs node); without them each comparison is skipped
//! (a failure under `RELEASE_ASSEMBLE_ORACLES_REQUIRED=1`). `RELEASE_ASSEMBLE_GUARD_ROUNDS` sets the number of
//! trees per pairing (default 300), and four times that many sources for `core_requires`.
#![cfg(unix)]
#![allow(clippy::indexing_slicing, clippy::expect_used)]

mod common;
use release_assemble::core_requires::{core_requires, core_requires_file, Requires};
use release_assemble::model_pairings::{self, code_body};
use std::collections::BTreeSet;
use std::fs;
use std::path::Path;
use std::process::Command;

const EXCERPT: &str = include_str!("../fixtures/assemble-release-model-pairings-excerpt.sh");
const GUARDS: &str = include_str!("../fixtures/assemble-release-guards-excerpt.sh");

fn section_of(text: &str, name: &str) -> String {
    let start = format!("#@{name}\n");
    let Some(at) = text.find(&start) else {
        return String::new();
    };
    let rest = &text[at + start.len()..];
    let end = rest.find("\n#@").map(|i| i + 1).unwrap_or(rest.len());
    rest[..end].to_string()
}

fn section(name: &str) -> String {
    section_of(EXCERPT, name)
}

fn tool(name: &str) -> bool {
    Command::new(name)
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Both the shell and its node are there, or the comparison is skipped (or fails when required).
fn oracle(what: &str) -> bool {
    if tool("bash") && tool("node") {
        return true;
    }
    common::skip(&format!("{what}: bash and node needed"));
    false
}

fn rounds() -> u64 {
    std::env::var("RELEASE_ASSEMBLE_GUARD_ROUNDS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(300)
}

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n.max(1) as u64) as usize
    }
    fn chance(&mut self, pct: usize) -> bool {
        self.below(100) < pct
    }
    fn pick<'a>(&mut self, from: &[&'a str]) -> &'a str {
        from[self.below(from.len())]
    }
}

fn write(path: &Path, data: &[u8]) {
    if let Some(p) = path.parent() {
        fs::create_dir_all(p).expect("a parent directory");
    }
    fs::write(path, data).expect("a tree file");
}

/// The primitives every shell run sources: `code_has`, `core_requires`, `code_body`, the `ex_*` names.
fn primitives() -> String {
    format!(
        "{}{}{}{}",
        section_of(GUARDS, "code_has"),
        section("core_requires"),
        section("code_body"),
        section("ex_names")
    )
}

fn run_bash(dir: &Path, body: &str, args: &[&Path]) -> std::process::Output {
    let prims = dir.join("primitives.sh");
    if !prims.exists() {
        fs::write(&prims, primitives()).expect("the primitives");
    }
    let mut cmd = Command::new("bash");
    cmd.env("LC_ALL", "C").arg("-c").arg(body).arg(&prims);
    for a in args {
        cmd.arg(a);
    }
    cmd.output().expect("bash runs")
}

// ---------------------------------------------------------------------------------------------------------
// Verbatim

/// Each section of the excerpt is a contiguous run of lines of the real script (when `ASSEMBLE_RELEASE_SH`
/// names it), and the three pairings are there.
#[test]
fn model_pairings_excerpt_is_verbatim() {
    for name in [
        "core_requires",
        "code_body",
        "ex_names",
        "model_writes",
        "model_manager",
        "embedding_admin",
    ] {
        assert!(!section(name).is_empty(), "section {name}");
    }
    assert_eq!(EXCERPT.matches("\n#@").count(), 6);
    let Ok(path) = std::env::var("ASSEMBLE_RELEASE_SH") else {
        common::skip("model_pairings_excerpt_is_verbatim: ASSEMBLE_RELEASE_SH not set");
        return;
    };
    let script = fs::read_to_string(path).expect("the script");
    for name in [
        "core_requires",
        "code_body",
        "ex_names",
        "model_writes",
        "model_manager",
        "embedding_admin",
    ] {
        let s = section(name);
        assert!(script.contains(&s), "section {name} is not verbatim");
    }
}

// ---------------------------------------------------------------------------------------------------------
// core_requires

const FRAGMENTS: &[&str] = &[
    "require('./m.cjs')",
    "require(\"./m.cjs\")",
    "require(`./m.cjs`)",
    "require ( './m.cjs' )",
    "x.require('./m.cjs')",
    "a?.require('./m.cjs')",
    "require('./m.cjs', 2)",
    "require('./other.cjs')",
    "// require('./m.cjs')\n",
    "/* require('./m.cjs') */",
    "'require(\\'./m.cjs\\')'",
    "`require('./m.cjs')`",
    "`${require('./m.cjs')}`",
    "`a${b}c${ {d: 1} }e`",
    "require(`./${m}.cjs`)",
    "require('\\x2e/m.cjs')",
    "require('\\056/m.cjs')",
    "require('\\u{2e}/m.cjs')",
    "require('\\u{00002e}/m.cjs')",
    "require('\\u002e/m.cjs')",
    "require(`\\x2e/m.cjs`)",
    "require(`\\056/m.cjs`)",
    "require('.\\/m.cjs')",
    "require('./m\\\n.cjs')",
    "require('./m\\\r.cjs')",
    "require('./m\\\r\n.cjs')",
    "require(`./m\\\r\n.cjs`)",
    "a / b / c",
    "x = /re[/]q\\//g",
    "return /x/",
    "(a) / 2",
    "[a] / 3",
    "typeof /x/",
    "in /y/i",
    "'a\\8'",
    "'\\08'",
    "'\\400'",
    "'\\777'",
    "'\\0'",
    "`\\0`",
    "`\\01`",
    "`\\8`",
    "'\\x4'",
    "'\\u12'",
    "'\\u{}'",
    "'\\u{110000}'",
    "'\\u{10FFFF}'",
    "'a\rb\\n'",
    "'a\rb'",
    "`a\r\nb\\n`",
    "`a\rb`",
    "'\u{2028}\\n'",
    "{",
    "}",
    "(",
    ")",
    ".",
    "\n",
    " ",
    "\u{2028}",
    "\u{a0}",
    "é",
    "$x",
    "_y",
    "1.5e3",
    ".5",
    "x.y",
    "/* open",
    "'open",
    "`open",
    "/open",
    "`${open",
];

fn js_source(r: &mut Rng) -> Vec<u8> {
    let mut out = Vec::new();
    if r.chance(5) {
        out.extend_from_slice("\u{feff}".as_bytes());
    }
    if r.chance(5) {
        out.extend_from_slice(b"#!/usr/bin/env node\n");
    }
    for _ in 0..1 + r.below(6) {
        let f = r.pick(FRAGMENTS);
        // The broken fragments only now and then, or most sources would be invalid.
        if f.contains("open") && !r.chance(25) {
            continue;
        }
        out.extend_from_slice(f.as_bytes());
        out.extend_from_slice(r.pick(&[" ", "\n", ";", "", " + "]).as_bytes());
        if r.chance(2) {
            out.push(0xff);
        }
    }
    out
}

#[test]
fn core_requires_matches_node() {
    if !oracle("core_requires_matches_node") {
        return;
    }
    let tmp = tempfile::tempdir().expect("a temp dir");
    let mut r = Rng(0x5eed_c0de_1234_5678);
    let n = rounds() * 4;
    let mut paths = Vec::new();
    for k in 0..n {
        let p = tmp.path().join(format!("s{k}.cjs"));
        write(&p, &js_source(&mut r));
        paths.push(p);
    }
    // A missing file, and one where the module name itself needs the decoding.
    paths.push(tmp.path().join("missing.cjs"));
    let refs: Vec<&Path> = paths.iter().map(|p| p.as_path()).collect();
    let out = run_bash(
        tmp.path(),
        "set -euo pipefail\n. \"$0\"\nfor f in \"$@\"; do printf '%s\\n' \"$(core_requires \"$f\" ./m.cjs)\"; done\n",
        &refs,
    );
    assert!(out.status.success(), "the shell loop ran");
    let shell: Vec<String> = String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(str::to_string)
        .collect();
    assert_eq!(shell.len(), paths.len());
    let mut seen = BTreeSet::new();
    for (p, want) in paths.iter().zip(&shell) {
        let got = core_requires_file(p, "./m.cjs").word();
        let src = fs::read(p).unwrap_or_default();
        assert_eq!(
            got,
            want.as_str(),
            "core_requires over {:?}",
            String::from_utf8_lossy(&src)
        );
        seen.insert(got);
    }
    assert_eq!(seen.len(), 3, "yes, no and invalid all reached: {seen:?}");
}

#[test]
fn core_requires_reads_bytes_like_the_file() {
    assert_eq!(core_requires(b"require('./m.cjs')", "./m.cjs"), Requires::Yes);
    assert_eq!(
        core_requires_file(Path::new("/nonexistent/x.cjs"), "./m.cjs"),
        Requires::No
    );
}

// ---------------------------------------------------------------------------------------------------------
// The pairings

/// One pairing's tree shape.
struct Spec {
    section: &'static str,
    run: fn(&Path) -> Result<(), String>,
    front: &'static str,
    switch: &'static str,
    module: &'static str,
    /// Lines the front module needs beside its switch.
    front_lines: &'static [&'static str],
    /// Lines Core's index.cjs needs beside the require.
    index_lines: &'static [&'static str],
    /// (file under core/server, line) pairs Core needs beside its refusal module's switch.
    core_lines: &'static [(&'static str, &'static str)],
}

const MODEL_WRITES: Spec = Spec {
    section: "model_writes",
    run: model_pairings::model_writes_pairing,
    front: "model_writes_routes.rs",
    switch: "NOEVIA_RUST_MODEL_WRITES",
    module: "rust-model-writes.cjs",
    front_lines: &[],
    index_lines: &[],
    core_lines: &[("inference-budget.cjs", "  function savedGib(row) {")],
};

const MODEL_MANAGER: Spec = Spec {
    section: "model_manager",
    run: model_pairings::model_manager_pairing,
    front: "model_manager_routes.rs",
    switch: "NOEVIA_RUST_MODEL_MANAGER",
    module: "rust-model-manager-proxy.cjs",
    front_lines: &[],
    index_lines: &[],
    core_lines: &[(
        "rust-model-manager-scan-cache.cjs",
        "  if (op === 'clear') cache.clear();",
    )],
};

const EMBEDDING_ADMIN: Spec = Spec {
    section: "embedding_admin",
    run: model_pairings::embedding_admin_pairing,
    front: "embedding_admin_routes.rs",
    switch: "NOEVIA_RUST_EMBEDDING_ADMIN",
    module: "rust-embedding-admin.cjs",
    front_lines: &["const PROTOCOL: &str = \"rust-embedding-service-intent-v1\";"],
    index_lines: &[
        "  if (require('./rust-embedding-admin.cjs').refuseOwned(req, res)) return;",
        "  const config = createEmbeddingConfig({ reread: require('./rust-embedding-admin.cjs').frontOwns() });",
        "  const service = createEmbeddingService({ intentFile: require('./rust-embedding-admin.cjs').frontOwns() ? intent : null });",
    ],
    core_lines: &[
        ("embedding-config.cjs", "  function refresh() {"),
        (
            "embedding-service.cjs",
            "const INTENT_PROTOCOL = 'rust-embedding-service-intent-v1';",
        ),
    ],
};

/// A line as code (mostly), as a comment, or left out.
fn maybe(r: &mut Rng, line: &str) -> Option<String> {
    match r.below(100) {
        0..=84 => Some(line.to_string()),
        85..=91 => Some(format!("// {line}")),
        _ => None,
    }
}

fn require_line(r: &mut Rng, module: &str) -> String {
    match r.below(100) {
        0..=69 => format!("const owner = require('./{module}');"),
        70..=74 => format!("// const owner = require('./{module}');"),
        75..=78 => format!("const s = 'require(\\'./{module}\\')';"),
        79..=81 => format!("const owner = x.require('./{module}');"),
        82..=85 => format!("const owner = require(\"\\x2e/{module}\");"),
        86..=88 => format!("const owner = require(`./{module}`);"),
        89..=91 => format!("const owner = require(`./${{name}}`); // ./{module}"),
        92..=95 => format!("/* unterminated require('./{module}')"),
        _ => format!("const t = `${{require('./{module}')}}`;"),
    }
}

fn gen_tree(r: &mut Rng, root: &Path, s: &Spec) {
    fs::remove_dir_all(root).ok();
    fs::create_dir_all(root).expect("the tree root");
    let src = root.join("rs/bins/noevia-server/src");
    let core = root.join("core/server");
    let front_here = r.chance(85);
    let mut front = String::from("use crate::Env;\n");
    let mut test_mod = String::new();
    if front_here {
        let decl = format!("pub const SWITCH: &str = \"{}\";", s.switch);
        let read = "    if env.get(SWITCH).as_deref() == Some(\"rust\") {".to_string();
        let mut lines = vec![
            match r.below(100) {
                0..=89 => decl.clone(),
                90..=94 => "pub const SWITCH: &str = \"NOEVIA_RUST_OTHER\";".to_string(),
                _ => format!("// {decl}"),
            },
            maybe(r, &read).unwrap_or_default(),
        ];
        for l in s.front_lines {
            lines.push(maybe(r, l).unwrap_or_default());
        }
        if s.section == "model_writes" {
            let preset = "pub const PRESET: &str = \"/api/models/preset\";";
            lines.push(match r.below(100) {
                0..=49 => preset.to_string(),
                50..=54 => format!("    {preset}"),
                _ => String::new(),
            });
        }
        // Some of the contract only inside the test module (it must not count).
        if r.chance(10) && !lines.is_empty() {
            let k = r.below(lines.len());
            test_mod.push_str(&lines[k]);
            test_mod.push('\n');
            lines[k] = String::new();
        }
        for l in lines {
            front.push_str(&l);
            front.push('\n');
        }
        if r.chance(10) {
            front.push_str("/* a block\n   pub const PRESET: &str = \"x\";\n*/\n");
        }
        if !test_mod.is_empty() || r.chance(20) {
            front.push_str("#[cfg(test)]\nmod tests {\n");
            front.push_str(&test_mod);
            front.push_str("}\n");
        }
        write(&src.join(s.front), front.as_bytes());
    } else {
        match r.below(10) {
            0..=3 => write(
                &src.join("sub/other.rs"),
                format!("// reads {}\n", s.switch).as_bytes(),
            ),
            4 => write(&src.join("notes.txt"), s.switch.as_bytes()),
            _ => {}
        }
    }
    if s.section == "model_writes" && r.chance(50) {
        let mut pull = String::from("use x;\n");
        match r.below(100) {
            0..=49 => pull.push_str("pub const PULL: &str = \"/api/models/pull\";\n"),
            50..=59 => pull.push_str("#[cfg(test)]\nmod t {\npub const PULL: &str = \"x\";\n}\n"),
            _ => {}
        }
        write(&src.join("model_pull_routes.rs"), pull.as_bytes());
    }
    if r.chance(95) {
        let mut index = String::from("'use strict';\n");
        index.push_str(&require_line(r, s.module));
        index.push('\n');
        for l in s.index_lines {
            if let Some(l) = maybe(r, l) {
                index.push_str(&l);
                index.push('\n');
            }
        }
        write(&core.join("index.cjs"), index.as_bytes());
    }
    if r.chance(92) {
        let mut module = String::from("'use strict';\n");
        let decl = format!("const SWITCH = '{}';", s.switch);
        module.push_str(&match r.below(100) {
            0..=89 => decl,
            90..=94 => "const SWITCH = 'NOEVIA_RUST_OTHER';".to_string(),
            _ => format!("  {decl}"),
        });
        module.push('\n');
        let reads = [
            "const owns = () => process.env[SWITCH] === 'rust';",
            "const owns = () => env[SWITCH]==='rust';",
        ];
        let read = r.pick(&reads);
        if let Some(l) = maybe(r, read) {
            module.push_str(&l);
            module.push('\n');
        }
        if s.section == "model_writes" {
            module.push_str(&mw_lists(r, root));
        }
        write(&core.join(s.module), module.as_bytes());
    }
    for (file, line) in s.core_lines {
        if let Some(l) = maybe(r, line) {
            let p = core.join(file);
            let mut old = fs::read(&p).unwrap_or_else(|_| b"'use strict';\n".to_vec());
            old.extend_from_slice(l.as_bytes());
            old.push(b'\n');
            write(&p, &old);
        }
    }
}

/// rust-model-writes.cjs's three lists, mostly agreeing with what the front answers (read from the tree
/// written so far), sometimes not, on one line or several, with comments, or not closed.
fn mw_lists(r: &mut Rng, root: &Path) -> String {
    let src = root.join("rs/bins/noevia-server/src");
    let front = code_body(fs::read(src.join("model_writes_routes.rs")).ok().as_deref());
    let preset = String::from_utf8_lossy(&front)
        .lines()
        .any(|l| l.starts_with("pub const PRESET: &str = "));
    let pull_body = code_body(fs::read(src.join("model_pull_routes.rs")).ok().as_deref());
    let pull = String::from_utf8_lossy(&pull_body)
        .lines()
        .any(|l| l.starts_with("pub const PULL: &str = "));
    let mtp = src.join("model_pull_routes.rs").is_file();
    let mut out = String::new();
    for (list, path, answers) in [
        ("OWNED_PATHS", "/api/models/preset", preset),
        ("OWNED_GETS", "/api/models/mtp-artifact", mtp),
        ("OWNED_POSTS", "/api/models/pull", pull),
    ] {
        let listed = if r.chance(88) { answers } else { !answers };
        let entry = if listed {
            format!("'{path}'")
        } else {
            "'/api/models/other'".to_string()
        };
        match r.below(100) {
            0..=39 => out.push_str(&format!("const {list} = new Set([{entry}]);\n")),
            40..=74 => out.push_str(&format!(
                "const {list} = [\n  '/api/models/x', // '{path}' kept here only in a comment\n  {entry},\n];\n"
            )),
            75..=84 => out.push_str(&format!(
                "const {list} = new Set([\n  {entry},\n]); // end\n"
            )),
            85..=91 => {
                if !listed {
                    out.push_str(&format!("// const {list} = ['{path}'];\n"));
                }
            }
            _ => out.push_str(&format!(
                "const {list} = [\n  {entry},\nfunction later() {{}}\n"
            )),
        }
    }
    out.push_str("module.exports = { owns };\n");
    out
}

fn shell_verdict(dir: &Path, block: &str, tree: &Path) -> String {
    let body = format!(
        "set -euo pipefail\ndie() {{ echo \"assemble-release: $*\" >&2; exit 1; }}\ntree=\"$1\"\n. \"$0\"\n{block}echo PASS\n"
    );
    let out = run_bash(dir, &body, &[tree]);
    if out.status.success() {
        String::from_utf8_lossy(&out.stdout).trim_end().to_string()
    } else {
        format!(
            "{} (status {:?})",
            String::from_utf8_lossy(&out.stderr)
                .lines()
                .last()
                .unwrap_or_default(),
            out.status.code()
        )
    }
}

fn rust_verdict(s: &Spec, tree: &Path) -> String {
    match (s.run)(tree) {
        Ok(()) => "PASS".to_string(),
        Err(m) => format!("assemble-release: {m} (status Some(1))"),
    }
}

fn compare(s: &Spec, seed: u64, min_verdicts: usize) {
    if !oracle(s.section) {
        return;
    }
    let tmp = tempfile::tempdir().expect("a temp dir");
    let root = tmp.path().join("tree");
    let block = section(s.section);
    let mut r = Rng(seed);
    let mut seen = BTreeSet::new();
    for round in 0..rounds() {
        gen_tree(&mut r, &root, s);
        let want = shell_verdict(tmp.path(), &block, &root);
        let got = rust_verdict(s, &root);
        assert_eq!(got, want, "{} round {round}", s.section);
        seen.insert(got);
    }
    assert!(seen.contains("PASS"), "{}: PASS reached", s.section);
    assert!(
        seen.len() >= min_verdicts,
        "{}: {} verdicts reached ({seen:#?})",
        s.section,
        seen.len()
    );
}

#[test]
fn model_writes_pairing_matches_the_shell() {
    // PASS, named, invalid, not loaded, switch, budget, then three requests × three refusals.
    compare(&MODEL_WRITES, 0x6d77_0001, 10);
}

#[test]
fn model_manager_pairing_matches_the_shell() {
    // PASS, named, invalid, not loaded, switch, scan cache clear.
    compare(&MODEL_MANAGER, 0x6d6d_0002, 6);
}

#[test]
fn embedding_admin_pairing_matches_the_shell() {
    // PASS, named, invalid, not refused, switch, re-read, front announcement, Core reading it.
    compare(&EMBEDDING_ADMIN, 0x6561_0003, 8);
}

// ---------------------------------------------------------------------------------------------------------
// Fixed cases (run without the shell)

fn tree_with(files: &[(&str, &str)]) -> tempfile::TempDir {
    let tmp = tempfile::tempdir().expect("a temp dir");
    for (p, data) in files {
        write(&tmp.path().join(p), data.as_bytes());
    }
    tmp
}

#[test]
fn old_trees_pass_and_a_named_switch_without_its_module_is_refused() {
    let t = tree_with(&[("core/server/index.cjs", "'use strict';\n")]);
    for (_, run) in model_pairings::ALL {
        assert_eq!(run(t.path()), Ok(()));
    }
    let t = tree_with(&[(
        "rs/bins/noevia-server/src/lib.rs",
        "// NOEVIA_RUST_MODEL_MANAGER moves later\n",
    )]);
    let err = model_pairings::model_manager_pairing(t.path()).expect_err("refused");
    assert!(err.contains("names NOEVIA_RUST_MODEL_MANAGER but has no"), "{err}");
}

#[test]
fn a_full_model_writes_pair_passes() {
    let t = tree_with(&[
        (
            "rs/bins/noevia-server/src/model_writes_routes.rs",
            "pub const SWITCH: &str = \"NOEVIA_RUST_MODEL_WRITES\";\nfn on(env: &E) -> bool {\n    env.get(SWITCH).as_deref() == Some(\"rust\")\n}\npub const PRESET: &str = \"/api/models/preset\";\n",
        ),
        (
            "rs/bins/noevia-server/src/model_pull_routes.rs",
            "pub const PULL: &str = \"/api/models/pull\";\n",
        ),
        (
            "core/server/index.cjs",
            "const mw = require('./rust-model-writes.cjs');\n",
        ),
        (
            "core/server/rust-model-writes.cjs",
            "const SWITCH = 'NOEVIA_RUST_MODEL_WRITES';\nconst owns = () => process.env[SWITCH] === 'rust';\nconst OWNED_PATHS = new Set([\n  '/api/models/preset', // the editor\n]);\nconst OWNED_GETS = ['/api/models/mtp-artifact'];\nconst OWNED_POSTS = ['/api/models/pull'];\n",
        ),
        (
            "core/server/inference-budget.cjs",
            "  function savedGib(row) {\n",
        ),
    ]);
    assert_eq!(model_pairings::model_writes_pairing(t.path()), Ok(()));
    // An unclosed list is a read failure for that request.
    fs::write(
        t.path().join("core/server/rust-model-writes.cjs"),
        "const SWITCH = 'NOEVIA_RUST_MODEL_WRITES';\nconst owns = () => process.env[SWITCH] === 'rust';\nconst OWNED_PATHS = [\n  '/api/models/preset',\nfunction f() {}\n",
    )
    .expect("rewrite");
    let err = model_pairings::model_writes_pairing(t.path()).expect_err("refused");
    assert!(
        err.contains("could not be read for PUT /api/models/preset"),
        "{err}"
    );
}

#[test]
fn a_commented_require_is_not_a_load() {
    let t = tree_with(&[
        (
            "rs/bins/noevia-server/src/model_manager_routes.rs",
            "pub const SWITCH: &str = \"NOEVIA_RUST_MODEL_MANAGER\";\n",
        ),
        (
            "core/server/index.cjs",
            "// require('./rust-model-manager-proxy.cjs')\n",
        ),
        ("core/server/rust-model-manager-proxy.cjs", "'use strict';\n"),
    ]);
    let err = model_pairings::model_manager_pairing(t.path()).expect_err("refused");
    assert!(err.contains("Core does not load server/rust-model-manager-proxy.cjs"), "{err}");
    fs::write(t.path().join("core/server/index.cjs"), "/* open\n").expect("rewrite");
    let err = model_pairings::model_manager_pairing(t.path()).expect_err("refused");
    assert_eq!(err, "model manager proxy pairing: Core's index.cjs could not be read");
}
