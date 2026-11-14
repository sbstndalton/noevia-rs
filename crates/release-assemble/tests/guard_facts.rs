//! The facts pairings (`src/facts.rs`) against their verbatim guard blocks
//! (`fixtures/assemble-release-pairings-excerpt.sh`), run under `set -euo pipefail` with the verbatim
//! primitives and the script's `die`, over generated release trees: the verdict and the refusal text must
//! be equal, and every verdict of each guard must be reached. Also `code_has` (with `posix_ere`) against the
//! verbatim shell `code_has` over bracket expressions the regex crate reads differently.
//! `RELEASE_ASSEMBLE_GUARD_ROUNDS` sets the number of trees per pairing (default 400).
#![cfg(unix)]
#![allow(clippy::indexing_slicing, clippy::expect_used)]

mod common;
use release_assemble::bridged::{code_has, posix_ere};
use release_assemble::facts::{self, Check, FactsPairing, Refusal};
use std::collections::BTreeMap;
use std::fs;
use std::path::Path;
use std::process::Command;

const PRIMITIVES: &str = include_str!("../fixtures/assemble-release-guards-excerpt.sh");
const PAIRINGS: &str = include_str!("../fixtures/assemble-release-pairings-excerpt.sh");

fn section(name: &str) -> String {
    let start = format!("#@{name}\n");
    let from = PAIRINGS
        .find(&start)
        .map(|i| i + start.len())
        .unwrap_or(PAIRINGS.len());
    let rest = PAIRINGS.get(from..).unwrap_or_default();
    let end = rest.find("\n#@").map(|i| i + 1).unwrap_or(rest.len());
    rest.get(..end).unwrap_or_default().to_string()
}

fn have_bash() -> bool {
    Command::new("bash")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn rounds() -> u64 {
    std::env::var("RELEASE_ASSEMBLE_GUARD_ROUNDS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(400)
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
}

fn write(path: &Path, data: &str) {
    if let Some(p) = path.parent() {
        fs::create_dir_all(p).expect("a parent directory");
    }
    fs::write(path, data).expect("a tree file");
}

/// A literal line each check ERE of the facts pairings matches.
fn sample(ere: &str) -> String {
    ere.replace(".*", " ")
        .replace("(^|[^A-Za-z0-9_$])", "")
        .replace("(^|[^[:alnum:]_$])", "")
        .replace("[[:space:]]*", " ")
        .replace("[[:space:]]+", " ")
        .replace("[.]", ".")
        .replace("[?]", "?")
        .replace("[(]", "(")
        .replace("\\(", "(")
        .replace("latest?.version", "latest.version")
}

/// The tree file a check names: `rs/…` from the root, anything else under `core/server/`.
fn rel(file: &str) -> String {
    if file.starts_with("rs/") {
        file.to_string()
    } else {
        format!("core/server/{file}")
    }
}

fn gen_facts(r: &mut Rng, root: &Path, p: &FactsPairing) {
    fs::remove_dir_all(root).ok();
    fs::create_dir_all(root).expect("the tree root");
    let moved = r.chance(70);
    let core_moved = if r.chance(5) { !moved } else { moved };
    let mut files = BTreeMap::<String, String>::new();
    let mut add = |f: &str, line: &str| {
        let e = files.entry(rel(f)).or_insert_with(|| "'use strict';\n".to_string());
        e.push_str(line);
        e.push('\n');
    };
    // The front's FEATURES.
    let feat = format!("\"{}\"", p.rs_feature);
    let lib = match r.below(25) {
        0 => None,
        1 => Some(format!("// pub const FEATURES: &[&str] = &[{feat}];\n")),
        _ if moved => Some(format!(
            "pub const FEATURES: &[&str] = &[\n    \"rust-auth\",\n    {feat},\n];\n"
        )),
        _ => Some("pub const FEATURES: &[&str] = &[\"rust-auth\"];\n".to_string()),
    };
    if let Some(lib) = lib {
        write(&root.join("rs/bins/noevia-server/src/lib.rs"), &lib);
    }
    // The route contract.
    if !r.chance(5) {
        let owner = if moved { "rust" } else { "node" };
        let mut toml = String::from("# routes\n");
        if r.chance(3) {
            toml.push_str("[[\"route\"]]\n");
        }
        let rows = match r.below(25) {
            0 => 0,
            1 => 2,
            _ => 1,
        };
        for _ in 0..rows {
            let o = if r.chance(4) { "node" } else { owner };
            let o = if r.chance(3) { "rust" } else { o };
            toml.push_str(&format!("[[route]]\npath = \"{}\"\nowner = \"{o}\"\n", p.path));
            if r.chance(4) {
                toml.push_str("switch = \"NOEVIA_RUST_X\"\n");
            }
            let methods = if p.methods == "GET" {
                match r.below(25) {
                    0 => None,
                    1 => Some("[\"GET\", \"PUT\"]"),
                    _ => Some("[\"GET\"]"),
                }
            } else if r.chance(4) {
                Some("[\"POST\"]")
            } else {
                None
            };
            if let Some(m) = methods {
                toml.push_str(&format!("methods = {m}\n"));
            }
        }
        toml.push_str("[[route]]\npath = \"/api/other\"\nowner = \"node\"\n");
        if r.chance(10) {
            toml.push_str(&format!("[[route]]\npath = \"{}/x\"\nowner = \"node\"\n", p.path));
        }
        write(&root.join("rs/contracts/http/routes.toml"), &toml);
    }
    // The refusal module.
    let module = format!("core/server/{}", p.module);
    let decl = format!(
        "const OWNED_ROUTES = Object.freeze(new Set(['GET {}']));",
        p.path
    );
    let text = match p.refusal {
        Refusal::Owned(entry) => match r.below(20) {
            0 => None,
            1 => Some(format!(
                "const OWNED_ROUTES = new Set((['{entry}', 'GET /api/models']));\n"
            )),
            2 => Some(format!("const OWNED_ROUTES = new Set((['PATCH {}']));\n", p.path)),
            3 => Some(format!("// const OWNED_ROUTES = new Set((['{entry}']));\n")),
            _ => Some(format!("const OWNED_ROUTES = new Set((['{entry}']));\n")),
        },
        Refusal::Line(_) => match r.below(20) {
            0 => None,
            1 => Some(format!("// {decl}\n")),
            2 => Some(format!("{decl} \n")),
            3 => Some(format!("  {decl}\n")),
            4 => Some(format!("/* old\n{decl}\n*/\n{decl}\n")),
            _ => Some(format!("{decl}\n")),
        },
        Refusal::OnlyDeclaration(line) => match r.below(20) {
            0 => None,
            1 => Some(format!("{line}\n{line}\n")),
            2 => Some(format!("/*\n{line}\n*/\n")),
            3 => Some(format!("{line} // the GET\n")),
            4 => Some(format!("// const OWNED_ROUTES = x;\n  /* a\n  */\n{line}\n")),
            5 => Some(format!("const OWNED_ROUTES = new Set();\n{line}\n")),
            _ => Some(format!("{line}\n")),
        },
    };
    if let Some(t) = text {
        write(
            &root.join(module),
            &format!("'use strict';\n{t}module.exports = {{ OWNED_ROUTES }};\n"),
        );
    }
    // The checks: an old handler or policy left live (or only commented out), a kept primitive missing.
    for check in p.checks {
        match *check {
            Check::Gone { file, ere, .. } => match r.below(16) {
                0 => add(file, &format!("  {}", sample(ere))),
                1 => add(file, &format!("  // {}", sample(ere))),
                2 => add(file, &format!("/* {}\n*/", sample(ere))),
                3 if ere.starts_with("latest") => add(file, "  if (latest?.version === VERSION) {"),
                _ => add(file, "  return null;"),
            },
            Check::Kept { all, .. } => {
                let drop = if r.chance(8) { Some(r.below(all.len())) } else { None };
                for (i, (file, ere)) in all.iter().enumerate() {
                    if drop == Some(i) {
                        if r.chance(50) {
                            add(file, &format!("  // {}", sample(ere)));
                        }
                        continue;
                    }
                    add(file, &format!("  {}", sample(ere)));
                }
            }
            Check::NoFile { file, .. } => {
                if r.chance(8) {
                    add(file, "module.exports = { format };");
                }
            }
        }
    }
    for (f, body) in &files {
        write(&root.join(f), body);
    }
    if !r.chance(7) {
        write(&root.join("core/server").join(p.bridge_file), "'use strict';\n");
    }
    // index.cjs: the marker, the captured secret, the bridge constructor with the secret, the mount.
    let mut index = String::from("'use strict';\n");
    if core_moved {
        index.push_str(&format!("const refusal = require('./{}');\n", p.module));
    }
    match r.below(25) {
        0 => {}
        1 => index.push_str(
            "// const FRONT_SECRET = String(process.env.NOEVIA_PROJECT_STORE_TOKEN || '');\n",
        ),
        2 => index.push_str(
            "const FRONT_SECRET = String(process.env.NOEVIA_PROJECT_STORE_TOKEN);\n",
        ),
        _ => index.push_str(
            "  const FRONT_SECRET = String(process.env.NOEVIA_PROJECT_STORE_TOKEN || ''); // before clientFromEnv\n",
        ),
    }
    let ctor = p.ctor.replace('\\', "");
    if !r.chance(4) {
        index.push_str(&format!("const bridge = {ctor}\n"));
    }
    let gap = if r.chance(5) { 4 } else { r.below(3) };
    for _ in 0..gap {
        index.push_str("  store,\n");
    }
    match r.below(25) {
        0 => {}
        1 => index.push_str("  secret: OTHER_SECRET,\n"),
        _ => index.push_str("  secret: FRONT_SECRET,\n"),
    }
    index.push_str("});\n");
    let mount = p.mount.replace("(req, res)", "");
    match r.below(16) {
        0 => {}
        1 => index.push_str(&format!("// if (await {mount}(req, res)) return;\n")),
        _ => index.push_str(&format!("if (await {mount}(req, res)) return;\n")),
    }
    write(&root.join("core/server/index.cjs"), &index);
}

fn shell_verdict(dir: &Path, block: &str, tree: &Path) -> String {
    let prims = dir.join("primitives.sh");
    if !prims.exists() {
        fs::write(&prims, PRIMITIVES).expect("the primitives excerpt");
    }
    let script = format!(
        "set -euo pipefail\ndie() {{ echo \"assemble-release: $*\" >&2; exit 1; }}\n. \"$0\"\ntree=\"$1\"\n{block}echo PASS\n"
    );
    let out = Command::new("bash")
        .env("LC_ALL", "C")
        .arg("-c")
        .arg(script)
        .arg(&prims)
        .arg(tree)
        .output()
        .expect("bash runs");
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

fn verdict(got: Result<(), String>) -> String {
    match got {
        Ok(()) => "PASS".to_string(),
        Err(e) => format!("assemble-release: {e} (status Some(1))"),
    }
}

/// The section name of a CLI name (`autotune-settings` → `autotune_settings`).
fn section_name(cli: &str) -> String {
    cli.replace('-', "_")
}

/// PASS, move together, rows, incomplete, refusal, one per check, bridge file, mount, secret.
fn verdicts(p: &FactsPairing) -> usize {
    8 + p.checks.len()
}

#[test]
fn facts_pairings_match_the_shell() {
    if !have_bash() {
        common::skip("guard_facts: bash is not installed");
        return;
    }
    let dir = tempfile::tempdir().expect("a temporary directory");
    let tree = dir.path().join("tree");
    let n = rounds();
    for (k, (cli, p)) in facts::ALL.iter().enumerate() {
        let name = section_name(cli);
        let block = section(&name);
        assert!(block.contains(p.label), "the {name} section does not name {:?}", p.label);
        let mut r = Rng(0xFAC7_0000_0000_0001 + k as u64);
        let mut seen = BTreeMap::<String, u64>::new();
        for _ in 0..n {
            gen_facts(&mut r, &tree, p);
            let want = shell_verdict(dir.path(), &block, &tree);
            let got = verdict(facts::facts_pairing(&tree, p));
            assert_eq!(got, want, "{name} pairing over the generated tree");
            *seen.entry(want).or_default() += 1;
        }
        eprintln!("COUNTS guard_facts {name} trees={n} verdicts={seen:?}");
        assert_eq!(seen.len(), verdicts(p), "{name} verdicts reached: {seen:?}");
    }
}

#[test]
fn every_facts_pairing_has_a_cli_name_and_a_section() {
    assert_eq!(facts::ALL.len(), 8);
    for (cli, p) in facts::ALL {
        assert!(facts::by_name(cli).is_some_and(|q| q.label == p.label));
        let block = section(&section_name(cli));
        assert!(block.lines().count() > 20, "section for {cli} is short");
        assert!(block.contains(&format!("require('./{}')", p.module)));
        assert!(block.contains(&format!("FEATURES {} features", p.rs_feature)));
        assert!(block.contains(&format!("$1 == \"{}\"", p.path)));
        assert!(block.contains(p.mount));
        assert!(block.contains(p.refusal_text));
        assert!(block.contains(p.rows_text));
    }
    assert!(facts::by_name("model-downloads-read").is_none());
}

#[test]
fn sample_lines_match_their_check_regexes() {
    for (_, p) in facts::ALL {
        for check in p.checks {
            let pairs: Vec<(&str, &str)> = match *check {
                Check::Gone { file, ere, .. } => vec![(file, ere)],
                Check::Kept { all, .. } => all.to_vec(),
                Check::NoFile { .. } => vec![],
            };
            for (_, ere) in pairs {
                let line = format!("  {}\n", sample(ere));
                assert!(code_has(Some(line.as_bytes()), ere), "{ere:?} misses {line:?}");
                let commented = format!("  // {}\n", sample(ere));
                assert!(!code_has(Some(commented.as_bytes()), ere), "{ere:?} reads a comment");
            }
        }
    }
}

/// `latest?.version === VERSION` as an ERE is `lates`, an optional `t`, any one character, `version`: it
/// does not match the optional-chaining source it names (the shell's guard has the same blind spot; the
/// port keeps it, and both read the generated `latest?.version` line as gone).
#[test]
fn untuned_candidate_policy_regex_misses_optional_chaining() {
    let ere = "latest?.version === VERSION";
    assert!(!code_has(Some(b"if (latest?.version === VERSION) {\n"), ere));
    assert!(code_has(Some(b"if (latest.version === VERSION) {\n"), ere));
}

#[test]
fn model_assignments_declaration_awk() {
    let line = "const OWNED_ROUTES = Object.freeze(new Set(['GET /api/models/assignments']));";
    let ok = |s: String| facts::only_declaration(Some(s.as_bytes()), line);
    assert!(ok(format!("{line}\n")));
    assert!(ok(line.to_string()));
    assert!(ok(format!("// const OWNED_ROUTES = x;\n{line}\n")));
    assert!(ok(format!("  /* a\n const OWNED_ROUTES = y;\n */\n{line}\n")));
    assert!(ok(format!("/* one line */\n{line}\n")));
    assert!(!ok(format!("{line}\n{line}\n")));
    assert!(!ok(format!("/*\n{line}\n*/\n")));
    assert!(!ok(format!("{line} \n")));
    assert!(!ok(format!("{line}\r\n")));
    assert!(!ok(" const OWNED_ROUTES = x;\n".to_string()));
    assert!(!ok(String::new()));
    assert!(!facts::only_declaration(None, line));
}

#[test]
fn posix_ere_translates_bracket_expressions() {
    let cases: &[(&str, Option<&str>)] = &[
        ("abc", Some("abc")),
        ("[[]'GET x'[]]", Some(r"[\[]'GET x'[\]]")),
        ("[]a]", Some(r"[\]a]")),
        ("[^]a]", Some(r"[^\]a]")),
        (r"[\]", Some(r"[\\]")),
        ("[a&&b]", Some(r"[a\&\&b]")),
        ("[~~]", Some(r"[\~\~]")),
        ("[--]", Some(r"[-\-]")),
        ("[[:space:]]*", Some("[[:space:]]*")),
        ("[^[:alnum:]_$]", Some("[^[:alnum:]_$]")),
        ("[,}]", Some("[,}]")),
        (r"a\(\{", Some(r"a\(\{")),
        ("x{2}", Some("x{2}")),
        ("x{2,}", Some("x{2,}")),
        ("x{2,3}", Some("x{2,3}")),
        ("x{,3}", Some("x{0,3}")),
        ("x{", Some(r"x\{")),
        ("x{a}", Some(r"x\{a\}")),
        ("{}", Some(r"\{\}")),
        ("[[=a=]]", None),
        ("[[.a.]]", None),
        ("[abc", None),
        ("[[:alpha:", None),
    ];
    for (input, want) in cases {
        assert_eq!(posix_ere(input).as_deref(), *want, "posix_ere({input:?})");
    }
}

/// `code_has` against the verbatim shell `code_has` (awk comment filter, then `grep -qE`) over patterns
/// whose bracket expressions the regex crate reads differently from POSIX.
#[test]
fn code_has_matches_the_shell_on_posix_brackets() {
    if !have_bash() {
        common::skip("guard_facts code_has: bash is not installed");
        return;
    }
    let dir = tempfile::tempdir().expect("a temporary directory");
    let prims = dir.path().join("primitives.sh");
    fs::write(&prims, PRIMITIVES).expect("the primitives excerpt");
    let src = dir.path().join("src.cjs");
    let patterns = [
        "^const OWNED_ROUTES = Object[.]freeze[(]new Set[(][[]'GET /api/models/preset'[]][)][)];$",
        "[[]",
        "[]]",
        "[]a]x",
        "[^]a]x",
        r"[\]",
        "[a&&b]x",
        "[~]",
        "[!--]",
        "[[:space:]]+y",
        "(^|[^[:alnum:]_$])function[[:space:]]+resume[[:space:]]*[(]",
        "x{2}",
        "x{,2}y",
        "presetFacts:.*presets[?][.]get[(]",
        "latest?.version === VERSION",
    ];
    let lines = [
        "const OWNED_ROUTES = Object.freeze(new Set(['GET /api/models/preset']));",
        "const a = [];",
        "a[0]",
        "]x ax bx",
        "&x",
        "~",
        "+",
        r"a \ b",
        "x  y",
        "function resume(job) {",
        "afunction resume(",
        "xx",
        "xy",
        "presetFacts: () => presets?.get(name)",
        "if (latest?.version === VERSION) {",
        "// [ ] commented",
    ];
    let mut compared = 0;
    for pat in patterns {
        for line in lines {
            let text = format!("'use strict';\n{line}\n");
            fs::write(&src, &text).expect("the source file");
            let out = Command::new("bash")
                .env("LC_ALL", "C")
                .arg("-c")
                .arg(". \"$0\"; code_has \"$1\" \"$2\"")
                .arg(&prims)
                .arg(&src)
                .arg(pat)
                .output()
                .expect("bash runs");
            let shell = out.status.code() == Some(0);
            let rust = code_has(Some(text.as_bytes()), pat);
            assert_eq!(rust, shell, "code_has {pat:?} over {line:?}");
            compared += 1;
        }
    }
    eprintln!("COUNTS guard_facts code_has compared={compared}");
}
