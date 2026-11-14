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
    if !r.chance(10) {
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

fn gen_research(r: &mut Rng, root: &Path) {
    fs::remove_dir_all(root).ok();
    fs::create_dir_all(root).expect("the tree root");
    let moved = r.chance(70);
    let core_moved = if r.chance(6) { !moved } else { moved };
    features(r, root, "rust-research", moved);
    let owner = if moved { "rust" } else { "node" };
    let base = "/api/projects/{projectId}/research";
    if !r.chance(6) {
        let mut toml = String::from("# routes\n");
        if r.chance(3) {
            toml.push_str("[[\"route\"]]\n");
        }
        let mut tails = vec!["", "/plan", "/{job}", "/{job}/cancel", "/{job}/save"];
        if r.chance(6) {
            tails.remove(r.below(tails.len()));
        }
        if r.chance(10) {
            tails.push("/plan");
        }
        for tail in tails {
            let o = if r.chance(4) { "node" } else { owner };
            let o = if r.chance(3) { "rust" } else { o };
            toml.push_str(&format!("[[route]]\npath = \"{base}{tail}\"\nowner = \"{o}\"\n"));
            if r.chance(2) {
                toml.push_str("switch = \"NOEVIA_RUST_X\"\n");
            }
            if r.chance(2) {
                toml.push_str("methods = [\"GET\"]\n");
            }
        }
        // Near misses the matcher must not take.
        toml.push_str(&format!("[[route]]\npath = \"{base}/{{job}}/x\"\nowner = \"node\"\n"));
        toml.push_str("[[route]]\npath = \"/api/projects/{id}/research\"\nowner = \"node\"\n");
        write(&root.join("rs/contracts/http/routes.toml"), &toml);
    }
    let mut refused = vec![
        "'ALL /api/projects/{projectId}/research'",
        "'ALL /api/projects/{projectId}/research/plan'",
        "'ALL /api/projects/{projectId}/research/{job}'",
        "'ALL /api/projects/{p}/research/{j}/cancel'",
        "'ALL /api/projects/{projectId}/research/{job}/save'",
    ];
    match r.below(16) {
        0 => refused.clear(),
        1 => {
            refused.remove(r.below(refused.len()));
        }
        2 => refused.push("'GET /api/projects/{projectId}/research'"),
        _ => {}
    }
    write(
        &root.join("core/server/rust-research.cjs"),
        &format!("const OWNED_ROUTES = new Set(([\n  {},\n]));\n", refused.join(",\n  ")),
    );
    if r.chance(5) {
        write(&root.join("core/server/routes/research.cjs"), "'use strict';\n");
    }
    if r.chance(4) {
        write(&root.join("core/server/research-service.cjs"), "'use strict';\n");
    }
    if !r.chance(5) {
        write(&root.join("core/server/inference-admission-lease.cjs"), "'use strict';\n");
    }
    if !r.chance(5) {
        write(&root.join("core/server/rust-research-bridge.cjs"), "'use strict';\n");
    }
    // index.cjs: marker, capture, clientFromEnv, the constructors with the secret, the mounts.
    let mut lines: Vec<String> = vec!["'use strict';".into()];
    if core_moved {
        lines.push("const research = require('./rust-research.cjs');".into());
    }
    let capture = "const FRONT = String(process.env.NOEVIA_PROJECT_STORE_TOKEN || '');";
    let client = "const store = clientFromEnv();";
    match r.below(17) {
        0 => lines.push(client.into()),
        1 => {
            lines.push(client.into());
            lines.push(capture.into());
        }
        2 => {
            lines.push(format!("// {capture}"));
            lines.push(client.into());
        }
        3 => {
            lines.push(capture.into());
            lines.push(client.into());
            lines.push("const again = process.env.NOEVIA_PROJECT_STORE_TOKEN;".into());
        }
        4 => lines.push(capture.into()),
        5 => lines.push(format!("{capture} const s = clientFromEnv();")),
        6 => {
            lines.push(capture.replacen("const", "let", 1));
            lines.push(client.into());
        }
        _ => {
            lines.push(format!("  {capture}"));
            lines.push(client.into());
        }
    }
    for ctor in ["createAdmissionRoutes({", "createResearchBridge({"] {
        if r.chance(4) {
            continue;
        }
        lines.push(format!("const x = {ctor}"));
        for _ in 0..r.below(5) {
            lines.push("  store,".into());
        }
        let secret = if r.chance(5) { "OTHER" } else { "FRONT" };
        lines.push(format!("  secret: {secret},"));
        lines.push("});".into());
    }
    for mount in ["inferenceAdmission.handle(req, res)", "researchBridge(req, res)"] {
        match r.below(16) {
            0 => {}
            1 => lines.push(format!("// if (await {mount}) return;")),
            _ => lines.push(format!("if (await {mount}) return;")),
        }
    }
    let mut index = lines.join("\n");
    index.push('\n');
    write(&root.join("core/server/index.cjs"), &index);
}

fn gen_task_reads(r: &mut Rng, root: &Path) {
    fs::remove_dir_all(root).ok();
    fs::create_dir_all(root).expect("the tree root");
    let moved = r.chance(70);
    let core_moved = if r.chance(6) { !moved } else { moved };
    features(r, root, "rust-task-reads", moved);
    let owner = if moved { "rust" } else { "node" };
    if !r.chance(6) {
        let mut toml = String::from("# routes\n");
        if r.chance(3) {
            toml.push_str("[[\"route\"]]\n");
        }
        let mut rows = vec![
            "/api/code/active",
            "/api/projects/{projectId}/code",
            "/api/projects/{projectId}/code/{task}",
            "/api/projects/{projectId}/browser",
            "/api/projects/{projectId}/browser/{task}",
        ];
        if r.chance(6) {
            rows.remove(r.below(rows.len()));
        }
        if r.chance(10) {
            rows.push("/api/code/active");
        }
        for path in rows {
            let o = if r.chance(4) { "node" } else { owner };
            let o = if r.chance(3) { "rust" } else { o };
            toml.push_str(&format!("[[route]]\npath = \"{path}\"\nowner = \"{o}\"\n"));
            if r.chance(2) {
                toml.push_str("switch = \"NOEVIA_RUST_X\"\n");
            }
            let active = path == "/api/code/active";
            match r.below(30) {
                0 => toml.push_str("methods = [\"GET\", \"POST\"]\n"),
                1 => toml.push_str("methods = [\"GET\"]\n"),
                _ if active || !moved => {}
                _ => toml.push_str("methods = [\"GET\"]\n"),
            }
        }
        toml.push_str(
            "[[route]]\npath = \"/api/projects/{projectId}/code/{task}/x\"\nowner = \"node\"\n",
        );
        write(&root.join("rs/contracts/http/routes.toml"), &toml);
    }
    let mut refused = vec![
        "'ALL /api/code/active'",
        "'GET /api/projects/{projectId}/code'",
        "'GET /api/projects/{projectId}/code/{task}'",
        "'GET /api/projects/{id}/browser'",
        "'GET /api/projects/{projectId}/browser/{task}'",
    ];
    match r.below(16) {
        0 => refused.clear(),
        1 => {
            refused.remove(r.below(refused.len()));
        }
        2 => refused.push("'POST /api/projects/{projectId}/code'"),
        _ => {}
    }
    write(
        &root.join("core/server/rust-task-reads.cjs"),
        &format!("const OWNED_ROUTES = new Set(([\n  {},\n]));\n", refused.join(",\n  ")),
    );
    for file in ["routes/code.cjs", "routes/browser.cjs"] {
        match r.below(20) {
            0 => write(&root.join("core/server").join(file), "const t = service.get(id);\n"),
            1 => write(
                &root.join("core/server").join(file),
                "// const t = service.listActive(id);\n",
            ),
            2 => write(
                &root.join("core/server").join(file),
                "const t = service.start(id);\n",
            ),
            _ => {}
        }
    }
    if !r.chance(10) {
        write(&root.join("core/server/rust-task-facts.cjs"), "'use strict';\n");
    }
    let mut index = String::from("'use strict';\n");
    if core_moved {
        index.push_str("const reads = require('./rust-task-reads.cjs');\n");
    }
    match r.below(16) {
        0 => {}
        1 => index.push_str(
            "// const FRONT = String(process.env.NOEVIA_PROJECT_STORE_TOKEN || '');\n",
        ),
        _ => index.push_str("const FRONT = String(process.env.NOEVIA_PROJECT_STORE_TOKEN || '');\n"),
    }
    if !r.chance(5) {
        index.push_str("const facts = createTaskFactsBridge({\n");
        for _ in 0..r.below(5) {
            index.push_str("  store,\n");
        }
        index.push_str(if r.chance(6) { "  secret: OTHER,\n" } else { "  secret: FRONT,\n" });
        index.push_str("});\n");
    }
    match r.below(14) {
        0 => {}
        1 => index.push_str("// if (await taskFactsBridge(req, res)) return;\n"),
        _ => index.push_str("if (await taskFactsBridge(req, res)) return;\n"),
    }
    write(&root.join("core/server/index.cjs"), &index);
}

fn gen_model_scan(r: &mut Rng, root: &Path) {
    fs::remove_dir_all(root).ok();
    fs::create_dir_all(root).expect("the tree root");
    let moved = r.chance(70);
    let core_moved = if r.chance(6) { !moved } else { moved };
    features(r, root, "rust-model-manager-scan", moved);
    let owner = if moved { "rust" } else { "node" };
    let path = "/api/model-manager/models";
    if !r.chance(6) {
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
            toml.push_str(&format!("[[route]]\npath = \"{path}\"\nowner = \"{o}\"\n"));
            if r.chance(2) {
                toml.push_str("switch = \"NOEVIA_RUST_X\"\n");
            }
            match r.below(30) {
                0 => toml.push_str("methods = [\"GET\", \"POST\"]\n"),
                1 => {}
                _ if !moved => {}
                _ => toml.push_str("methods = [\"GET\"]\n"),
            }
        }
        toml.push_str("[[route]]\npath = \"/api/model-manager/models/{id}\"\nowner = \"node\"\n");
        write(&root.join("rs/contracts/http/routes.toml"), &toml);
    }
    let literal = format!("const OWNED_ROUTES = Object.freeze(new Set(['GET {path}']));");
    let from_path = "const OWNED_ROUTES = Object.freeze(new Set(['GET '+PATH]));";
    let path_line = format!("const PATH = '{path}';");
    let marker = match r.below(22) {
        0 => None,
        1 => Some(format!("{path_line}\n{path_line}\n{from_path}\n")),
        2 => Some(format!("const PATH = '/api/models';\n{from_path}\n")),
        3 => Some(format!("{literal}\n{literal}\n")),
        4 => Some(format!("// {literal}\n")),
        5 => Some(format!("/*\n{literal}\n*/\n")),
        6 => Some(format!(
            "const OWNED_ROUTES = Object.freeze(new Set(['GET {path}', 'POST {path}']));\n"
        )),
        7 => Some(format!("  {literal}\n")),
        8 => Some(format!("{from_path}\n")),
        9 | 10 | 11 | 12 => Some(format!("'use strict';\n{path_line}\n{from_path}\n")),
        _ => Some(format!("'use strict';\n{literal}\n")),
    };
    if let Some(m) = marker {
        write(&root.join("core/server/rust-model-manager-scan.cjs"), &m);
    }
    match r.below(16) {
        0 => write(
            &root.join("core/server/routes/models.cjs"),
            "res.setHeader('X-Model-Scan', g);\n",
        ),
        1 => write(
            &root.join("core/server/routes/models.cjs"),
            "// const g = scanGeneration();\n",
        ),
        _ => {}
    }
    let mut models = String::new();
    for p in [
        "modelScanCache.snapshot()",
        "modelScanCache.publish(body,token)",
        "modelScanCache.refresh(token)",
    ] {
        match r.below(40) {
            0 => {}
            1 => models.push_str(&format!("// {p}\n")),
            _ => models.push_str(&format!("  return {p};\n")),
        }
    }
    write(&root.join("core/server/models.cjs"), &models);
    if !r.chance(10) {
        write(&root.join("core/server/rust-model-manager-scan-cache.cjs"), "'use strict';\n");
    }
    let mut index = String::from("'use strict';\n");
    if core_moved {
        index.push_str("const scan = require('./rust-model-manager-scan.cjs');\n");
    }
    if !r.chance(10) {
        index.push_str("const FRONT = String(process.env.NOEVIA_PROJECT_STORE_TOKEN || '');\n");
    }
    if !r.chance(6) {
        index.push_str("const cache = createModelManagerScanCache({\n");
        for _ in 0..r.below(5) {
            index.push_str("  models,\n");
        }
        index.push_str("  secret: FRONT,\n});\n");
    }
    match r.below(14) {
        0 => {}
        1 => index.push_str("// if (await modelManagerScanCache(req, res)) return;\n"),
        _ => index.push_str("if (await modelManagerScanCache(req, res)) return;\n"),
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
fn research_pairing_matches_the_shell() {
    // PASS, move together, rows, incomplete, refused, handlers, lease/bridge files, the two mounts, no
    // captured secret, the secret's order.
    compare(
        "research",
        "deep research ownership pairing",
        0xCA11_0000_0000_0004,
        11,
        &gen_research,
        &bridged::research_pairing,
    );
}

#[test]
fn task_reads_pairing_matches_the_shell() {
    // PASS, move together, rows, incomplete, refused, handlers, facts file, mount, secret.
    compare(
        "task_reads",
        "task read ownership pairing",
        0xCA11_0000_0000_0005,
        9,
        &gen_task_reads,
        &bridged::task_reads_pairing,
    );
}

#[test]
fn model_scan_pairing_matches_the_shell() {
    // PASS, move together, rows, incomplete, marker, cache policy, primitives, cache bridge, mount, secret.
    compare(
        "model_scan",
        "model scan read ownership pairing",
        0xCA11_0000_0000_0006,
        10,
        &gen_model_scan,
        &bridged::model_scan_pairing,
    );
}

#[test]
fn model_scan_marker_shapes() {
    let path_line = "const PATH = '/api/model-manager/models';";
    let literal = "const OWNED_ROUTES = Object.freeze(new Set(['GET /api/model-manager/models']));";
    let from_path = "const OWNED_ROUTES = Object.freeze(new Set(['GET '+PATH]));";
    let ok = |s: String| bridged::model_scan_marker(Some(s.as_bytes()));
    assert!(ok(format!("{literal}\n")));
    assert!(ok(format!("{path_line}\n{from_path}\n")));
    assert!(ok(format!("// {path_line}\n{path_line}\n{from_path}\n")));
    assert!(!ok(format!("{path_line}\n{path_line}\n{from_path}\n")));
    assert!(!ok(format!("{from_path}\n")));
    assert!(!ok(format!("{literal}\n{literal}\n")));
    assert!(!ok(format!("/* x\n{literal}\n*/\n")));
    assert!(!ok(format!("  {literal}\n")));
    assert!(!bridged::model_scan_marker(None));
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
