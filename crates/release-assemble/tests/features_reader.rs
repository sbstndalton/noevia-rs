//! The strict FEATURES readers (`src/features_reader.rs`) against the shell functions they port, read verbatim
//! from `$ASSEMBLE_RELEASE_SH` (noevia's deploy/tools/assemble-release.sh) and run with bash and node over
//! generated lib.rs files: every reader's verdict must be equal on every file, and each reader must reach
//! `none`, `invalid` and its versions.
//!
//! Without `ASSEMBLE_RELEASE_SH`, bash or node the comparison is skipped (a failure under
//! `FEATURES_READER_ORACLE_REQUIRED=1` or `RELEASE_ASSEMBLE_ORACLES_REQUIRED=1`).
//! `FEATURES_READER_ROUNDS` sets the number of files (default 160; each costs ten node runs).

#![cfg(unix)]
#![allow(clippy::indexing_slicing, clippy::expect_used)]

mod common;

use release_assemble::features_reader::{self, Reader, ALL};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::process::Command;

fn required() -> bool {
    [
        "FEATURES_READER_ORACLE_REQUIRED",
        "RELEASE_ASSEMBLE_ORACLES_REQUIRED",
    ]
    .iter()
    .any(|k| std::env::var(k).is_ok_and(|v| v == "1"))
}

fn skip(what: &str) {
    assert!(!required(), "{what} (required by the environment)");
    common::skip(what);
}

fn tool(name: &str) -> bool {
    Command::new(name)
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// `NAME() {` up to the first line that is exactly `}`, as the script defines it.
fn function_text(script: &str, name: &str) -> Option<String> {
    let head = format!("{name}() {{\n");
    let at = if script.starts_with(&head) {
        0
    } else {
        script.find(&format!("\n{head}"))? + 1
    };
    let rest = &script[at..];
    let end = rest.find("\n}\n")? + 3;
    Some(rest[..end].to_string())
}

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
    fn pick<'a>(&mut self, xs: &[&'a str]) -> &'a str {
        xs[self.below(xs.len())]
    }
}

const MARKS: &[&str] = &[
    "rust-mcp-discovery",
    "rust-mcp-discovery-v2",
    "rust-auto-roles-write",
    "auto-roles",
    "noevia-maintenance-lease-authority/1",
    "noevia-maintenance-lease-authority/2",
    "noevia-maintenance-lease-authority/3",
    "noevia-inference-admission-authority/1",
    "noevia-inference-admission-authority/2",
    "rust-chat-mirror-v1",
    "rust-chat-mirror-v2",
    "rust-residual-fallbacks-v1",
    "rust-residual-fallbacks",
    "rust-source-startup-v1",
    "rust-dav-listener-v1",
    "rust-dav-listener-v0",
    "rust-index-outbox-v1",
    "rust-index-outbox-v3",
    "rust-index-outbox-v4",
    "rust-media-background-v1",
    "rust-media-background-v",
    "rust-chat-edge",
];

const PREFIXES: &[&str] = &[
    "",
    "//! The front.\n",
    "#![forbid(unsafe_code)]\n",
    "#![cfg(test)]\n",
    "#![cfg_attr(docsrs, feature(doc_cfg))]\n",
    "#![cfg_attrs]\n",
    "#![doc = \"x\"]\n",
    "pub mod a;\n",
    "mod m {\n",
    "fn f() { let _ = 'x'; let _ = '\\''; let _ = \"}\"; }\n",
    "/* nested /* comment */ \"rust-mcp-discovery\" */\n",
    "/* open comment\n",
    "const ALIAS: &str = \"MARK\";\n",
    "static FEATURES_NOTE: &str = \"FEATURES\";\n",
    "fn g<'a>(x: &'a str) -> &'a str { x }\n",
    "let r = r#\"raw \"MARK\" text\"#;\n",
    "let b = b\"MARK\";\n",
    "(\n",
];

const ATTRS: &[&str] = &[
    "",
    "",
    "",
    "/// The capabilities.\n",
    "#[doc = \"list\"]\n",
    "#[allow(dead_code)]\n",
    "#[allow(a(b))]\n",
    "#[allow(x)] #![y]\n",
    "#[inline]\n",
    "#[cfg(feature = \"x\")]\n",
    "# [doc = \"spaced\"]\n",
];

const HEADS: &[&str] = &[
    "pub const FEATURES: &[&str] = &[",
    "pub const FEATURES: &[&str] = &[",
    "pub  const\tFEATURES : & [ & str ] = & [",
    "pub const FEATURES: &[&'static str] = &[",
    "pub static FEATURES: &[&str] = &[",
    "const FEATURES: &[&str] = &[",
];

fn entry(rng: &mut Rng) -> String {
    let mark = rng.pick(MARKS);
    match rng.below(16) {
        0 => format!("r\"{mark}\""),
        1 => format!("b\"{mark}\""),
        2 => format!("#[cfg(x)] \"{mark}\""),
        3 => "crate::CAPABILITY".to_string(),
        4 => "server_projects :: source_jobs::SOURCE_JOBS_STORE_MARKER".to_string(),
        5 => "server_projects::research_jobs::RESEARCH_JOBS_STORE_MARKER".to_string(),
        6 => format!("/* \"{mark}\" */ \"rust-other\""),
        7 => format!("\"{}\"", mark.replacen('-', "\\x2d", 1)),
        8 => format!("\"{}\"", mark.replacen('-', "\\u{2_d}", 1)),
        9 => "\"bad \\q escape\"".to_string(),
        10 => format!("concat!(\"{mark}\", \"\")"),
        11 => "\"rust-plain\"".to_string(),
        12 => format!("\"{mark}\\\n    \""),
        _ => format!("\"{mark}\""),
    }
}

fn generate(rng: &mut Rng) -> String {
    let mut s = String::new();
    for _ in 0..rng.below(3) {
        s.push_str(rng.pick(PREFIXES));
    }
    let decls = match rng.below(10) {
        0 => 0,
        1 => 2,
        _ => 1,
    };
    for _ in 0..decls {
        s.push_str(rng.pick(ATTRS));
        s.push_str(rng.pick(HEADS));
        let n = rng.below(5);
        let entries: Vec<String> = (0..n).map(|_| entry(rng)).collect();
        s.push_str(&entries.join(", "));
        if rng.below(3) == 0 {
            s.push(',');
        }
        match rng.below(12) {
            0 => {}
            1 => s.push_str("];\n}\n"),
            _ => s.push_str("];\n"),
        }
    }
    if rng.below(4) == 0 {
        s.push_str(rng.pick(PREFIXES));
    }
    let alias = rng.pick(MARKS);
    s.replace("MARK", alias)
}

#[test]
fn features_readers_match_the_shell() {
    let Ok(script_path) = std::env::var("ASSEMBLE_RELEASE_SH") else {
        return skip("features_readers_match_the_shell: ASSEMBLE_RELEASE_SH not set");
    };
    if !tool("bash") || !tool("node") {
        return skip("features_readers_match_the_shell: bash or node missing");
    }
    let script = fs::read_to_string(&script_path).expect("read ASSEMBLE_RELEASE_SH");
    let mut defs = String::new();
    for r in ALL {
        let text = function_text(&script, r.shell_name())
            .unwrap_or_else(|| panic!("{} not found in {script_path}", r.shell_name()));
        defs.push_str(&text);
    }
    let rounds: usize = std::env::var("FEATURES_READER_ROUNDS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(160);
    let dir = tempfile::tempdir().expect("tempdir");
    let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
    let mut files = Vec::new();
    for k in 0..rounds {
        let path = dir.path().join(format!("lib{k}.rs"));
        fs::write(&path, generate(&mut rng)).expect("write");
        files.push(path);
    }
    // A missing file and a directory are none on both sides.
    files.push(dir.path().join("missing.rs"));
    files.push(dir.path().to_path_buf());
    let mut body = String::from("set -u\nexport LC_ALL=C\n");
    body.push_str(&defs);
    body.push_str("for f in \"$@\"; do\n");
    for r in ALL {
        body.push_str(&format!(
            "  printf '%s\\n' \"$({} \"$f\")\"\n",
            r.shell_name()
        ));
    }
    body.push_str("done\n");
    let script_file = dir.path().join("readers.sh");
    fs::write(&script_file, body).expect("write script");
    let out = Command::new("bash")
        .arg(&script_file)
        .args(&files)
        .output()
        .expect("run bash");
    assert!(
        out.status.success(),
        "bash: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let text = String::from_utf8(out.stdout).expect("utf8");
    let shell: Vec<&str> = text.lines().collect();
    assert_eq!(
        shell.len(),
        files.len() * ALL.len(),
        "one verdict per file and reader"
    );
    let mut seen: BTreeMap<&str, BTreeSet<String>> = BTreeMap::new();
    let mut diffs = Vec::new();
    for (fi, f) in files.iter().enumerate() {
        for (ri, r) in ALL.iter().enumerate() {
            let want = shell[fi * ALL.len() + ri];
            let got = features_reader::read(*r, f);
            seen.entry(r.name()).or_default().insert(want.to_string());
            if want != got {
                let src = fs::read_to_string(f).unwrap_or_default();
                diffs.push(format!(
                    "{} on {}: shell {want}, rust {got}\n{src}",
                    r.name(),
                    f.display()
                ));
            }
        }
    }
    for (name, verdicts) in &seen {
        println!("COUNTS features_reader {name}: {verdicts:?}");
    }
    assert!(
        diffs.is_empty(),
        "{} differences, first:\n{}",
        diffs.len(),
        diffs[0]
    );
    for r in ALL {
        let got = &seen[r.name()];
        assert!(
            got.contains("none") && got.contains("invalid"),
            "{} reached {got:?}",
            r.name()
        );
    }
}

#[test]
fn cli_prints_one_verdict() {
    let dir = tempfile::tempdir().expect("tempdir");
    let lib = dir.path().join("lib.rs");
    fs::write(
        &lib,
        "pub const FEATURES: &[&str] = &[\"noevia-maintenance-lease-authority/2\"];\n",
    )
    .expect("write");
    let bin = env!("CARGO_BIN_EXE_release-assemble");
    let run = |reader: &str| {
        Command::new(bin)
            .args(["features-version", reader])
            .arg(&lib)
            .output()
            .expect("run")
    };
    let out = run("maintenance-lease");
    assert!(out.status.success());
    assert_eq!(out.stdout, b"v2");
    let out = run("lease_rs_version");
    assert_eq!(out.stdout, b"v2");
    let out = run("discovery");
    assert_eq!(out.stdout, b"none");
    let out = run("bogus");
    assert_eq!(out.status.code(), Some(2));
    assert_eq!(
        Reader::by_name("media_rs_version"),
        Some(Reader::MediaBackground)
    );
}
