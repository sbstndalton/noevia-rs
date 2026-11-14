//! The namespace ownership pairings (feature, model reads) against their verbatim guard blocks
//! (`fixtures/assemble-release-pairings-excerpt.sh`), run under `set -euo pipefail` with the verbatim
//! primitives and the script's `die`, over generated release trees: the verdict and the refusal text must
//! be equal. `RELEASE_ASSEMBLE_GUARD_ROUNDS` sets the number of trees per pairing (default 400).
#![cfg(unix)]
#![allow(clippy::indexing_slicing, clippy::expect_used)]

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
    // The route contract: often exactly right when the pair moved, so the checks after it are reached.
    if paired && r.chance(35) {
        let mut toml = String::from("# routes\n");
        for (i, path) in p.paths.iter().enumerate() {
            toml.push_str(&format!("[[route]]\npath = \"{path}\"\nowner = \"rust\"\n"));
            let methods = match p.methods {
                guard::Methods::All => None,
                guard::Methods::OnlyGet => Some("[\"GET\"]".to_string()),
                guard::Methods::Each(sets) => sets.get(i).map(|w| methods_array(w, r.chance(30))),
            };
            if let Some(m) = methods {
                toml.push_str(&format!("methods = {m}\n"));
            }
        }
        toml.push_str("[[route]]\npath = \"/api/other\"\nowner = \"node\"\n");
        write(&root.join("rs/contracts/http/routes.toml"), &toml);
        return;
    }
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
        let index = p.paths.iter().position(|w| *w == path);
        let methods = match (p.methods, methods) {
            (guard::Methods::OnlyGet, None) if r.chance(85) => Some("[\"GET\"]".to_string()),
            (guard::Methods::Each(sets), None) if r.chance(85) => index
                .and_then(|i| sets.get(i))
                .map(|w| methods_array(w, r.chance(20))),
            (_, m) => m.map(str::to_string),
        };
        if let Some(m) = methods {
            toml.push_str(&format!("methods = {m}\n"));
        }
    }
    write(&root.join("rs/contracts/http/routes.toml"), &toml);
}

/// `methods = [...]` for a comma-separated set, reversed when asked (the guard reads it as a set).
fn methods_array(set: &str, reverse: bool) -> String {
    let mut m: Vec<String> = set.split(',').map(|x| format!("\"{x}\"")).collect();
    if reverse {
        m.reverse();
    }
    format!("[{}]", m.join(", "))
}

/// The project configuration pairing's Core side beyond the marker: the refusal module's OWNED_ROUTES,
/// the old route mount and the toolbox publication, each right or bent.
fn gen_project_config_extra(r: &mut Rng, root: &Path) {
    let mut routes: Vec<&str> = vec![
        "'GET /api/projects/{projectId}/instruction-skills'",
        "'PUT /api/projects/{projectId}/instruction-skills'",
        "'GET /api/projects/{id}/instruction-skills/manifests'",
        "'GET /api/projects/{p}/instruction-skills/manifests/{skillId}/content'",
        "'POST /api/projects/{projectId}/config'",
        "'POST /api/projects/{project}/skills/install'",
    ];
    match r.below(10) {
        0 => {}
        1 => {
            routes.remove(r.below(routes.len()));
        }
        2 => routes.push("'DELETE /api/projects/{projectId}/config'"),
        3 => routes.push("'GET /api/projects/{projectId}/config'"),
        _ => {}
    }
    if r.chance(90) {
        let mut src = String::from("'use strict';\n");
        if r.chance(50) {
            src.push_str(&format!(
                "const OWNED_ROUTES = new Set(([\n  {},\n]));\n",
                routes.join(",\n  ")
            ));
        } else {
            src.push_str(&format!(
                "const OWNED_ROUTES = new Set(([{}]));\n",
                routes.join(", ")
            ));
        }
        src.push_str("module.exports = { OWNED_ROUTES };\n");
        write(&root.join("core/server/rust-project-config.cjs"), &src);
    }
    if r.chance(10) {
        write(
            &root.join("core/server/routes/projects.cjs"),
            "module.exports = {};\n",
        );
    }
    let index = root.join("core/server/index.cjs");
    let mut src = fs::read_to_string(&index).unwrap_or_default();
    if r.chance(8) {
        src.push_str("const projects = require('./routes/projects.cjs');\n");
    }
    if r.chance(88) {
        src.push_str("const toolboxes = { known: allToolboxes() };\n");
    }
    write(&index, &src);
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

fn verdict(got: Result<(), String>) -> String {
    match got {
        Ok(()) => "PASS".to_string(),
        Err(e) => format!("assemble-release: {e} (status Some(1))"),
    }
}

fn compare(name: &str, p: &NamespacePairing, seed: u64) {
    compare_with(name, p, seed, 5, &|_, _| false, &|t, _| {
        guard::namespace_pairing(t, p)
    });
}

/// One pairing over generated trees. `extra` adds the pairing's own files and returns a flag the block
/// reads from an earlier pairing (sent to the shell as `project_storage_rs_owner`); `rust` is the port.
/// `verdicts` is the number of distinct verdicts the guard has (PASS and each refusal).
fn compare_with(
    name: &str,
    p: &NamespacePairing,
    seed: u64,
    verdicts: usize,
    extra: &dyn Fn(&mut Rng, &Path) -> bool,
    rust: &dyn Fn(&Path, bool) -> Result<(), String>,
) {
    if !have_bash() {
        common::skip("guard_pairing: bash is not installed");
        return;
    }
    let dir = tempfile::tempdir().expect("a temporary directory");
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
        let flag = extra(&mut r, &tree);
        let prefix = format!(
            "project_storage_rs_owner={}\n",
            if flag { "rust" } else { "\"\"" }
        );
        let want = shell_verdict(dir.path(), &format!("{prefix}{block}"), &tree);
        let got = verdict(rust(&tree, flag));
        assert_eq!(got, want, "{name} pairing over the generated tree");
        *seen.entry(want).or_default() += 1;
    }
    eprintln!("COUNTS guard_pairing {name} trees={n} verdicts={seen:?}");
    assert_eq!(seen.len(), verdicts, "verdicts reached: {seen:?}");
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

#[test]
fn project_read_pairing_matches_the_shell() {
    compare(
        "project_reads",
        &guard::PROJECT_READ_OWNERSHIP,
        0x5EED_0000_0000_0001,
    );
}

#[test]
fn project_lifecycle_pairing_matches_the_shell() {
    compare(
        "project_lifecycle",
        &guard::PROJECT_LIFECYCLE_OWNERSHIP,
        0x5EED_0000_0000_0002,
    );
}

#[test]
fn project_files_pairing_matches_the_shell() {
    // PASS, move together, needs the storage pair, missing contract, rule, incomplete.
    compare_with(
        "project_files",
        &guard::PROJECT_FILES_OWNERSHIP,
        0x5EED_0000_0000_0003,
        6,
        &|r, _| r.chance(80),
        &guard::project_files_pairing,
    );
}

#[test]
fn project_config_pairing_matches_the_shell() {
    // PASS, the four namespace refusals, refused set, old mount, toolbox ids.
    compare_with(
        "project_config",
        &guard::PROJECT_CONFIG_OWNERSHIP,
        0x5EED_0000_0000_0004,
        8,
        &|r, t| {
            gen_project_config_extra(r, t);
            false
        },
        &|t, _| guard::project_config_pairing(t),
    );
}

/// The pairings excerpt's sections must still be contiguous blocks of the real script (when
/// `ASSEMBLE_RELEASE_SH` names it).
#[test]
fn pairings_excerpt_is_verbatim() {
    let Ok(path) = std::env::var("ASSEMBLE_RELEASE_SH") else {
        common::skip("pairings_excerpt_is_verbatim: ASSEMBLE_RELEASE_SH not set");
        return;
    };
    let real = fs::read_to_string(path).expect("ASSEMBLE_RELEASE_SH is readable");
    let names = [
        "feature",
        "model_reads",
        "project_reads",
        "project_lifecycle",
        "project_files",
        "project_config",
        "code_egress",
        "calibration_cancel",
        "calibration_routes",
        "research",
        "task_reads",
    ];
    for name in names {
        let block = section(name);
        assert!(block.lines().count() > 10, "section {name} is short");
        assert!(
            real.contains(&block),
            "section {name} is not a block of the script"
        );
    }
    assert_eq!(PAIRINGS.matches("\n#@").count(), names.len());
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
