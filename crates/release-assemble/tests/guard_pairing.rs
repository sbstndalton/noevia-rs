//! The namespace ownership pairings (feature, model reads) against their verbatim guard blocks
//! (`fixtures/assemble-release-pairings-excerpt.sh`), run under `set -euo pipefail` with the verbatim
//! primitives and the script's `die`, over generated release trees: the verdict and the refusal text must
//! be equal. `RELEASE_ASSEMBLE_GUARD_ROUNDS` sets the number of trees per pairing (default 400).
#![cfg(unix)]
#![allow(clippy::indexing_slicing)]

mod common;
use release_assemble::guard::{self, NamespacePairing};
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
        fs::create_dir_all(p).ok();
    }
    assert!(fs::write(path, data).is_ok());
}

/// A release tree for one pairing: Core's marker, the front's FEATURES list and routes.toml, each present,
/// absent or bent in one of the ways the guard must notice.
fn gen_tree(r: &mut Rng, root: &Path, p: &NamespacePairing) {
    fs::remove_dir_all(root).ok();
    assert!(fs::create_dir_all(root).is_ok());
    let paired = r.chance(60);
    // Core.
    match r.below(10) {
        0 => {}
        1 => write(
            &root.join("core/server/index.cjs"),
            "module.exports = {};\n",
        ),
        _ if paired || r.chance(30) => write(
            &root.join("core/server/index.cjs"),
            &format!("'use strict';\nconst x = {};\n", p.core_marker),
        ),
        _ => write(&root.join("core/server/index.cjs"), "'use strict';\n"),
    }
    // The front.
    let feat = format!("\"{}\"", p.rs_feature);
    let lib = match r.below(10) {
        0 => None,
        1 => Some(format!(
            "pub const FEATURES: &[&str] = &[{feat}];\npub const FEATURES: &[&str] = &[{feat}];\n"
        )),
        2 => Some(format!("// pub const FEATURES: &[&str] = &[{feat}];\n")),
        3 => Some(format!(
            "pub const FEATURES: &[&str] = &[{feat}, {feat}];\n"
        )),
        _ if paired || r.chance(30) => Some(format!(
            "pub const FEATURES: &[&str] = &[\n    \"rust-auth\",\n    {feat},\n];\n"
        )),
        _ => Some("pub const FEATURES: &[&str] = &[\"rust-auth\"];\n".to_string()),
    };
    if let Some(lib) = lib {
        write(&root.join("rs/bins/noevia-server/src/lib.rs"), &lib);
    }
    // The route contract.
    if r.chance(8) {
        return;
    }
    let mut toml = String::from("# routes\n");
    if r.chance(5) {
        toml.push_str("[[\"route\"]]\n");
    }
    let mut paths: Vec<&str> = p.paths.to_vec();
    if r.chance(15) && !paths.is_empty() {
        let gone = r.below(paths.len());
        paths.remove(gone);
    }
    if r.chance(10) {
        if let Some(first) = paths.first().copied() {
            paths.push(first);
        }
    }
    paths.push("/api/other");
    for path in paths {
        toml.push_str("[[route]]\n");
        toml.push_str(&format!("path = \"{path}\"\n"));
        let owner = if r.chance(90) { "rust" } else { "node" };
        toml.push_str(&format!("owner = \"{owner}\"\n"));
        if r.chance(7) {
            toml.push_str("switch = \"NOEVIA_RUST_X\"\n");
        }
        let methods = match r.below(12) {
            0 => Some("[\"GET\", \"POST\"]"),
            1 => Some("[\"POST\"]"),
            2 => Some("[\"GET\", \"GET\"]"),
            3 => Some("[]"),
            _ => None,
        };
        let methods = match (p.methods, methods) {
            (guard::Methods::OnlyGet, None) if r.chance(85) => Some("[\"GET\"]"),
            (_, m) => m,
        };
        if let Some(m) = methods {
            toml.push_str(&format!("methods = {m}\n"));
        }
    }
    write(&root.join("rs/contracts/http/routes.toml"), &toml);
}

fn shell_verdict(dir: &Path, block: &str, tree: &Path) -> String {
    let prims = dir.join("primitives.sh");
    if !prims.exists() {
        assert!(fs::write(&prims, PRIMITIVES).is_ok());
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
        .output();
    match out {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).trim_end().to_string(),
        // The refusal is the last stderr line (a refused routes.toml writes its own `line N:` first).
        Ok(o) => format!(
            "{} (status {:?})",
            String::from_utf8_lossy(&o.stderr)
                .lines()
                .last()
                .unwrap_or_default(),
            o.status.code()
        ),
        Err(e) => format!("bash: {e}"),
    }
}

fn rust_verdict(tree: &Path, p: &NamespacePairing) -> String {
    match guard::namespace_pairing(tree, p) {
        Ok(()) => "PASS".to_string(),
        Err(e) => format!("assemble-release: {e} (status Some(1))"),
    }
}

fn compare(name: &str, p: &NamespacePairing, seed: u64) {
    if !have_bash() {
        common::skip("guard_pairing: bash is not installed");
        return;
    }
    let Some(dir) = tempfile::tempdir().ok() else {
        return;
    };
    let block = section(name);
    assert!(
        block.contains(p.label),
        "the {name} section does not name {:?}",
        p.label
    );
    let tree = dir.path().join("tree");
    let mut r = Rng(seed);
    let mut seen = std::collections::BTreeMap::<String, u64>::new();
    let n = rounds();
    for _ in 0..n {
        gen_tree(&mut r, &tree, p);
        let want = shell_verdict(dir.path(), &block, &tree);
        let got = rust_verdict(&tree, p);
        assert_eq!(got, want, "{name} pairing over the generated tree");
        *seen.entry(want).or_default() += 1;
    }
    eprintln!("COUNTS guard_pairing {name} trees={n} verdicts={seen:?}");
    // Every verdict of the guard is reached: PASS, the four refusals.
    assert_eq!(seen.len(), 5, "verdicts reached: {seen:?}");
}

#[test]
fn feature_pairing_matches_the_shell() {
    compare("feature", &guard::FEATURE_OWNERSHIP, 0x1234_5678_9ABC_DEF1);
}

#[test]
fn model_read_pairing_matches_the_shell() {
    compare(
        "model_reads",
        &guard::MODEL_READ_OWNERSHIP,
        0x0FED_CBA9_8765_4321,
    );
}

/// Hand-made trees: today's old/old pins, a full new/new pair, each mixed generation.
#[test]
fn pairing_named_trees() {
    let Some(dir) = tempfile::tempdir().ok() else {
        return;
    };
    let t = dir.path();
    let p = &guard::MODEL_READ_OWNERSHIP;
    assert_eq!(guard::namespace_pairing(t, p), Ok(()));
    write(
        &t.join("core/server/index.cjs"),
        "require('./rust-models.cjs');\n",
    );
    assert_eq!(
        guard::namespace_pairing(t, p),
        Err("model read ownership pairing: Core and Rust must move together".into())
    );
    write(
        &t.join("rs/bins/noevia-server/src/lib.rs"),
        "pub const FEATURES: &[&str] = &[\"rust-model-reads\"];\n",
    );
    assert_eq!(
        guard::namespace_pairing(t, p),
        Err("model read ownership pairing: missing route contract".into())
    );
    write(
        &t.join("rs/contracts/http/routes.toml"),
        "[[route]]\npath = \"/api/models/capabilities\"\nowner = \"rust\"\nmethods = [\"GET\"]\n[[route]]\npath = \"/api/models/hardware\"\nowner = \"rust\"\nmethods = [\"GET\"]\n",
    );
    assert_eq!(guard::namespace_pairing(t, p), Ok(()));
    write(
        &t.join("rs/contracts/http/routes.toml"),
        "[[route]]\npath = \"/api/models/capabilities\"\nowner = \"rust\"\nmethods = [\"GET\", \"PUT\"]\n",
    );
    assert_eq!(
        guard::namespace_pairing(t, p),
        Err("model read ownership pairing: exactly GET of both routes must belong to Rust".into())
    );
    write(
        &t.join("rs/contracts/http/routes.toml"),
        "[[route]]\npath = \"/api/models/capabilities\"\nowner = \"rust\"\nmethods = [\"GET\"]\n",
    );
    assert_eq!(
        guard::namespace_pairing(t, p),
        Err("model read ownership pairing: incomplete route ownership".into())
    );
}
