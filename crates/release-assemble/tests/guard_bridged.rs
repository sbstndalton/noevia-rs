//! The bridged pairings (calibration cancel, calibration start/read, code egress issuer) against their
//! verbatim guard blocks (`fixtures/assemble-release-pairings-excerpt.sh`), run under `set -euo pipefail`
//! with the verbatim primitives and the script's `die`, over generated release trees: the verdict and the
//! refusal text must be equal, and every verdict of each guard must be reached.
//! `RELEASE_ASSEMBLE_GUARD_ROUNDS` sets the number of trees per pairing (default 400).
#![cfg(unix)]
#![allow(clippy::indexing_slicing, clippy::expect_used)]

mod common;
use release_assemble::bridged::{self, CalibrationPairing};
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

fn features(r: &mut Rng, root: &Path, feature: &str, moved: bool) {
    let feat = format!("\"{feature}\"");
    let lib = match r.below(12) {
        0 => None,
        1 => Some(format!("// pub const FEATURES: &[&str] = &[{feat}];\n")),
        2 => Some(format!("pub const FEATURES: &[&str] = &[{feat}, {feat}];\n")),
        _ if moved => Some(format!(
            "pub const FEATURES: &[&str] = &[\n    \"rust-auth\",\n    {feat},\n];\n"
        )),
        _ => Some("pub const FEATURES: &[&str] = &[\"rust-auth\"];\n".to_string()),
    };
    if let Some(lib) = lib {
        write(&root.join("rs/bins/noevia-server/src/lib.rs"), &lib);
    }
}

/// A literal line that each primitive ERE of the pairing matches.
fn primitive_text(re: &str) -> String {
    re.replace("[[:space:]]*", " ")
        .replace("[.]", ".")
        .replace("[,}]", ",")
}

fn gen_calibration(r: &mut Rng, root: &Path, p: &CalibrationPairing) {
    fs::remove_dir_all(root).ok();
    fs::create_dir_all(root).expect("the tree root");
    let moved = r.chance(70);
    let core_moved = if r.chance(8) { !moved } else { moved };
    features(r, root, p.rs_feature, moved);
    let owner = if moved { "rust" } else { "node" };
    // The route contract.
    if !r.chance(8) {
        let mut toml = String::from("# routes\n");
        if r.chance(4) {
            toml.push_str("[[\"route\"]]\n");
        }
        let rows = match r.below(20) {
            0 => 0,
            1 => 2,
            _ => 1,
        };
        for _ in 0..rows {
            let o = if r.chance(8) { "node" } else { owner };
            let o = if r.chance(5) { "rust" } else { o };
            toml.push_str(&format!("[[route]]\npath = \"{}\"\nowner = \"{o}\"\n", p.path));
            if r.chance(5) {
                toml.push_str("switch = \"NOEVIA_RUST_X\"\n");
            }
            if r.chance(5) {
                toml.push_str("methods = [\"POST\"]\n");
            }
        }
        toml.push_str("[[route]]\npath = \"/api/other\"\nowner = \"node\"\n");
        if r.chance(10) {
            toml.push_str(&format!("[[route]]\npath = \"{}/x\"\nowner = \"node\"\n", p.path));
        }
        write(&root.join("rs/contracts/http/routes.toml"), &toml);
    }
    // Core: the refusal module's OWNED_ROUTES.
    let refusal = match r.below(14) {
        0 => None,
        1 => Some(format!("'ALL {}', 'GET /api/models'", p.path)),
        2 => Some(format!("'POST {}'", p.path)),
        _ => Some(format!("'ALL {}'", p.path)),
    };
    if let Some(list) = refusal {
        write(
            &root.join("core/server").join(p.module),
            &format!("'use strict';\nconst OWNED_ROUTES = new Set(([{list}]));\nmodule.exports = {{ OWNED_ROUTES }};\n"),
        );
    }
    // The old public handler and policy, live, commented out or gone.
    match r.below(14) {
        0 => write(
            &root.join("core/server/routes/models.cjs"),
            &format!("router.post('{}', h);\n", p.path),
        ),
        1 => write(
            &root.join("core/server/routes/models.cjs"),
            &format!("// router.post('{}', h);\n", p.path),
        ),
        _ => write(&root.join("core/server/routes/models.cjs"), "router.get('/api/models', h);\n"),
    }
    let policy = if p.policy.contains("cancel") {
        "function cancel(job) {}"
    } else {
        "async function start (job) {}"
    };
    match r.below(14) {
        0 => write(&root.join("core/server/llamacpp-calibration.cjs"), &format!("{policy}\n")),
        1 => write(
            &root.join("core/server/llamacpp-calibration.cjs"),
            &format!("/* {policy}\n*/\nfunction cancelled() {{}}\n"),
        ),
        2 => write(
            &root.join("core/server/llamacpp-calibration.cjs"),
            "const x = { mycancel(a) {}, restart() {} };\n",
        ),
        _ => write(&root.join("core/server/llamacpp-calibration.cjs"), "'use strict';\n"),
    }
    // The live actor primitives.
    let mut manager = String::from("return {\n");
    for re in p.primitives {
        match r.below(30) {
            0 => {}
            1 => manager.push_str(&format!("  // {}\n", primitive_text(re))),
            _ => manager.push_str(&format!("  {}\n", primitive_text(re))),
        }
    }
    manager.push_str("};\n");
    write(&root.join("core/server/llamacpp-manager.cjs"), &manager);
    if !r.chance(7) {
        write(&root.join("core/server").join(p.bridge_file), "'use strict';\n");
    }
    // index.cjs: marker, captured secret, bridge constructor with the secret, the mount.
    let mut index = String::from("'use strict';\n");
    if core_moved {
        index.push_str(&format!("const refusal = require('./{}');\n", p.module));
    }
    match r.below(16) {
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
    let gap = r.below(6);
    if !r.chance(5) {
        index.push_str(&format!("const bridge = {ctor}\n"));
    }
    for _ in 0..gap {
        index.push_str("  store,\n");
    }
    match r.below(12) {
        0 => {}
        1 => index.push_str("  secret: OTHER_SECRET,\n"),
        _ => index.push_str("  secret: FRONT_SECRET,\n"),
    }
    index.push_str("});\n");
    let mount = p.mount.replace("(req, res)", "");
    match r.below(14) {
        0 => {}
        1 => index.push_str(&format!("// if (await {mount}(req, res)) return;\n")),
        _ => index.push_str(&format!("if (await {mount}(req, res)) return;\n")),
    }
    write(&root.join("core/server/index.cjs"), &index);
}

fn gen_code_egress(r: &mut Rng, root: &Path) {
    fs::remove_dir_all(root).ok();
    fs::create_dir_all(root).expect("the tree root");
    let moved = r.chance(70);
    let marker = "const CODE_EGRESS_CLIENT_MARKER = 'noevia-code-egress-client/1';";
    let client = if r.chance(8) { !moved } else { moved };
    if client {
        write(
            &root.join("core/server/code-egress-client.cjs"),
            &format!("'use strict';\n{marker}\n"),
        );
    } else {
        match r.below(4) {
            0 => {}
            1 => write(
                &root.join("core/server/code-egress-client.cjs"),
                &format!("// {marker}\n"),
            ),
            2 => write(
                &root.join("core/server/code-egress-client.cjs"),
                &format!("  {marker}\n"),
            ),
            _ => write(
                &root.join("core/server/code-egress-client.cjs"),
                "const CODE_EGRESS_CLIENT_MARKER = 'noevia-code-egress-client/0';\n",
            ),
        }
    }
    features(r, root, "rust-code-egress-issuer", moved);
    if r.chance(8) {
        write(&root.join("core/server/code-egress-control.cjs"), "'use strict';\n");
    }
    let mut egress = String::from("'use strict';\n");
    match r.below(12) {
        0 => egress.push_str("const k = publishWebKey(dir);\n"),
        1 => egress.push_str("const g = mintGrant(task);\n"),
        2 => egress.push_str("const s = secrets.derive('x');\n"),
        3 => egress.push_str("const s = secretsXderive('x');\n"),
        4 => egress.push_str("// const g = mintGrant(task);\n"),
        _ => {}
    }
    match r.below(12) {
        0 => {}
        1 => egress.push_str("// const client = require('./code-egress-client.cjs');\n"),
        2 => egress.push_str("const client = require(\"./code-egress-client.cjs\");\n"),
        _ => egress.push_str("const client = require('./code-egress-client.cjs');\n"),
    }
    if !r.chance(4) {
        write(&root.join("core/server/code-egress.cjs"), &egress);
    }
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

fn compare(
    name: &str,
    label: &str,
    seed: u64,
    verdicts: usize,
    gen: &dyn Fn(&mut Rng, &Path),
    rust: &dyn Fn(&Path) -> Result<(), String>,
) {
    if !have_bash() {
        common::skip("guard_bridged: bash is not installed");
        return;
    }
    let dir = tempfile::tempdir().expect("a temporary directory");
    let block = section(name);
    assert!(block.contains(label), "the {name} section does not name {label:?}");
    let tree = dir.path().join("tree");
    let mut r = Rng(seed);
    let mut seen = std::collections::BTreeMap::<String, u64>::new();
    let n = rounds();
    for _ in 0..n {
        gen(&mut r, &tree);
        let want = shell_verdict(dir.path(), &block, &tree);
        let got = verdict(rust(&tree));
        assert_eq!(got, want, "{name} pairing over the generated tree");
        *seen.entry(want).or_default() += 1;
    }
    eprintln!("COUNTS guard_bridged {name} trees={n} verdicts={seen:?}");
    assert_eq!(seen.len(), verdicts, "verdicts reached: {seen:?}");
}

#[test]
fn calibration_cancel_pairing_matches_the_shell() {
    let p = &bridged::CALIBRATION_CANCEL;
    // PASS, move together, rows, incomplete, refused, handler, policy, primitives, bridge, mount, secret.
    compare(
        "calibration_cancel",
        p.label,
        0xCA11_0000_0000_0001,
        11,
        &|r, t| gen_calibration(r, t, p),
        &|t| bridged::calibration_pairing(t, p),
    );
}

#[test]
fn calibration_routes_pairing_matches_the_shell() {
    let p = &bridged::CALIBRATION_ROUTES;
    compare(
        "calibration_routes",
        p.label,
        0xCA11_0000_0000_0002,
        11,
        &|r, t| gen_calibration(r, t, p),
        &|t| bridged::calibration_pairing(t, p),
    );
}

#[test]
fn code_egress_pairing_matches_the_shell() {
    // PASS, move together, Node issuer, publishes keys, no native client.
    compare(
        "code_egress",
        "code egress issuer pairing",
        0xCA11_0000_0000_0003,
        5,
        &gen_code_egress,
        &bridged::code_egress_pairing,
    );
}

#[test]
fn code_has_reads_extended_regexes_outside_comments() {
    let src = b"// function cancel(x) {}\n/* function cancel(\n*/\nconst a = 1;\n  async function start (j) {}\n";
    let s = Some(src.as_slice());
    assert!(!bridged::code_has(
        s,
        r"(^|[^A-Za-z0-9_$])function[[:space:]]+cancel[[:space:]]*\("
    ));
    assert!(bridged::code_has(
        s,
        r"(async[[:space:]]+)?function[[:space:]]+(start|status)[[:space:]]*\("
    ));
    assert!(bridged::code_has(s, "^const a"));
    assert!(!bridged::code_has(s, "^async"));
    // Not a regex: grep refuses it (status 2), which every caller reads as no match.
    assert!(!bridged::code_has(s, "("));
    assert!(!bridged::code_has(None, "const"));
    // An empty code text is one empty line to grep.
    assert!(bridged::code_has(Some(b"// only a comment\n".as_slice()), "^$"));
}

#[test]
fn captured_secret_and_its_constructor() {
    let index = b"const a = 1;\n  const S1 = String(process.env.NOEVIA_PROJECT_STORE_TOKEN || ''); // x\nconst S2 = String(process.env.NOEVIA_PROJECT_STORE_TOKEN || '');\nconst b = createCalibrationCancelBridge({\n  x,\n  y,\n  secret: S1,\n});\n";
    assert_eq!(bridged::captured_secret(index), Some(b"S1".to_vec()));
    assert!(bridged::secret_near(index, r"createCalibrationCancelBridge\(\{", b"S1"));
    assert!(!bridged::secret_near(index, r"createCalibrationRoutesBridge\(\{", b"S1"));
    // Four lines after the constructor is too far.
    let far = b"const b = createCalibrationCancelBridge({\n  a,\n  b,\n  c,\n  secret: S1,\n";
    assert!(!bridged::secret_near(far, r"createCalibrationCancelBridge\(\{", b"S1"));
    // The same line counts.
    let same = b"const b = createCalibrationCancelBridge({ secret: S1 });\n";
    assert!(bridged::secret_near(same, r"createCalibrationCancelBridge\(\{", b"S1"));
    assert_eq!(bridged::captured_secret(b"const S = String(process.env.X || '');\n"), None);
}
