//! The pairings whose Core side is a refusal module plus a state bridge (council p8): calibration cancel
//! (#2292), calibration start/read (#2303) and the code egress issuer (row V). Unlike the namespace
//! pairings in [`crate::guard`], the route rows are checked against whichever owner the pair names (Rust's
//! for every method with the new pair, Node's with the old), and the moved pair proves its Core wiring
//! with `code_has` regexes, `code_lines` and the captured front secret. Each is a port of its top-level
//! block in `assemble-release.sh`, compared with the verbatim block by `tests/guard_bridged.rs`.
//! Inert: nothing in the release path calls this yet.

use crate::guard::{
    code_lines, code_text, contains, grep_fixed, move_capability, project_routes_core, read_file,
    records, routes_toml_file, tab_fields, MoveKind,
};
use regex::bytes::{Regex, RegexBuilder};
use std::path::Path;

/// `grep -qE` with the script's `LC_ALL=C`: bytes, ASCII classes. `None` = a pattern grep would refuse too
/// (status 2, which every caller reads as "no match").
fn ere(pattern: &str) -> Option<Regex> {
    RegexBuilder::new(pattern).unicode(false).build().ok()
}

/// Lines as a here-string gives them to grep (`<<<"$x"`): every line of `text`, one empty line when empty.
fn here_lines(text: &[u8]) -> Vec<&[u8]> {
    let lines = records(text);
    if lines.is_empty() {
        vec![b"".as_slice()]
    } else {
        lines
    }
}

/// `code_has FILE ERE`: some line of [`code_text`] matches the extended regex. A missing file, or a pattern
/// that is not a regex, is false.
pub fn code_has(src: Option<&[u8]>, pattern: &str) -> bool {
    let (Some(text), Some(re)) = (code_text(src), ere(pattern)) else {
        return false;
    };
    here_lines(&text).iter().any(|l| re.is_match(l))
}

/// The name of the constant `index.cjs` captures the front's secret in, from its [`code_lines`]: the
/// first `const NAME = String(process.env.NOEVIA_PROJECT_STORE_TOKEN || '');` (the script's `sed -nE … | head -1`).
pub fn captured_secret(index_code: &[u8]) -> Option<Vec<u8>> {
    let re = ere(
        r"^[[:space:]]*const ([A-Za-z_][A-Za-z0-9_]*) = String\(process\.env\.NOEVIA_PROJECT_STORE_TOKEN \|\| ''\);",
    )?;
    records(index_code).into_iter().find_map(|l| {
        re.captures(l)
            .and_then(|c| c.get(1))
            .map(|m| m.as_bytes().to_vec())
    })
}

/// The script's awk: a line holding `secret: NAME` at most three lines after the latest line matching
/// `ctor` (the same line counts).
pub fn secret_near(index_code: &[u8], ctor: &str, secret: &[u8]) -> bool {
    let Some(re) = ere(ctor) else {
        return false;
    };
    let mut want = b"secret: ".to_vec();
    want.extend_from_slice(secret);
    let mut bridge = 0usize;
    for (i, l) in here_lines(index_code).iter().enumerate() {
        let nr = i + 1;
        if re.is_match(l) {
            bridge = nr;
        }
        if contains(l, &want) && bridge != 0 && nr - bridge <= 3 {
            return true;
        }
    }
    false
}

/// The route rows of one path for the pair's owner (`awk -v want=…` over the table): every row of `path` must
/// be Rust's for every method with no switch when `rust`, and Node's otherwise. `Ok` holds how many rows
/// there were; `Err(())` is the awk's `exit 1`. A table `routes_toml` refuses reads as empty (`|| x=""`).
fn owned_rows(tree: &Path, path: &str, rust: bool) -> Result<usize, ()> {
    let contract = tree.join("rs/contracts/http/routes.toml");
    let rt = routes_toml_file(&contract);
    if rt.error.is_some() {
        return Ok(0);
    }
    let mut n = 0;
    for r in &rt.routes {
        let [p, owner, switch, methods] = tab_fields(r);
        if p != path.as_bytes() {
            continue;
        }
        if rust && (owner != b"rust" || switch != b"-" || methods != b"-") {
            return Err(());
        }
        if !rust && owner != b"node" {
            return Err(());
        }
        n += 1;
    }
    Ok(n)
}

fn front_has(tree: &Path, feature: &str) -> bool {
    move_capability(
        read_file(&tree.join("rs/bins/noevia-server/src/lib.rs")).as_deref(),
        "FEATURES",
        feature,
        MoveKind::Features,
    )
}

fn core_file(tree: &Path, rel: &str) -> Option<Vec<u8>> {
    read_file(&tree.join("core/server").join(rel))
}

/// One calibration pairing (cancel or start/read).
#[derive(Clone, Copy, Debug)]
pub struct CalibrationPairing<'a> {
    pub label: &'a str,
    /// The refusal module, as `index.cjs` requires it (`rust-calibration-cancel.cjs`).
    pub module: &'a str,
    pub rs_feature: &'a str,
    /// The one path, Rust's for every method (also an ERE for `routes/models.cjs`, as the script uses it).
    pub path: &'a str,
    /// The refusal text's name for it (`the cancellation path`).
    pub path_text: &'a str,
    /// The policy function `llamacpp-calibration.cjs` must no longer define, and its refusal.
    pub policy: &'a str,
    pub policy_text: &'a str,
    /// The live actor primitives `llamacpp-manager.cjs` must expose (EREs, all of them).
    pub primitives: &'a [&'a str],
    pub bridge_file: &'a str,
    /// The mount `index.cjs` must hold (a fixed string) and the bridge's constructor (an ERE).
    pub mount: &'a str,
    pub ctor: &'a str,
}

/// #2292 calibration cancellation.
pub const CALIBRATION_CANCEL: CalibrationPairing<'static> = CalibrationPairing {
    label: "calibration cancellation ownership pairing",
    module: "rust-calibration-cancel.cjs",
    rs_feature: "rust-calibration-cancel",
    path: "/api/models/calibration/cancel",
    path_text: "the cancellation path",
    policy: r"(^|[^A-Za-z0-9_$])function[[:space:]]+cancel[[:space:]]*\(",
    policy_text: "the public cancellation policy",
    primitives: &[
        "cancellationFacts: calibrator.cancellationFacts",
        "compareAndAbort: calibrator.compareAndAbort",
    ],
    bridge_file: "rust-calibration-cancel-bridge.cjs",
    mount: "calibrationCancelBridge(req, res)",
    ctor: r"createCalibrationCancelBridge\(\{",
};

/// #2303 calibration start/read.
pub const CALIBRATION_ROUTES: CalibrationPairing<'static> = CalibrationPairing {
    label: "calibration start/read ownership pairing",
    module: "rust-calibration-routes.cjs",
    rs_feature: "rust-calibration-routes",
    path: "/api/models/calibration",
    path_text: "the calibration path",
    policy: r"(async[[:space:]]+)?function[[:space:]]+(start|status)[[:space:]]*\(",
    policy_text: "the public start/status policy",
    primitives: &[
        "stateFacts:[[:space:]]*calibrator[.]stateFacts[[:space:]]*[,}]",
        "listingFacts:[[:space:]]*calibrator[.]listingFacts[[:space:]]*[,}]",
        "preparationFacts:[[:space:]]*calibrator[.]preparationFacts[[:space:]]*[,}]",
        "compareAndLaunch:[[:space:]]*calibrator[.]compareAndLaunch[[:space:]]*[,}]",
    ],
    bridge_file: "rust-calibration-routes-bridge.cjs",
    mount: "calibrationRoutesBridge(req, res)",
    ctor: r"createCalibrationRoutesBridge\(\{",
};

/// Run one calibration pairing over an assembled tree. `Err` holds the `die` text without the
/// `assemble-release: ` prefix.
pub fn calibration_pairing(tree: &Path, p: &CalibrationPairing) -> Result<(), String> {
    let die = |what: &str| Err(format!("{}: {what}", p.label));
    let marker = format!("require('./{}')", p.module);
    let core = grep_fixed(&tree.join("core/server/index.cjs"), &marker);
    let rs = front_has(tree, p.rs_feature);
    if core != rs {
        return die("Core and Rust must move together");
    }
    let mut rows = 0;
    if tree.join("rs/contracts/http/routes.toml").is_file() {
        match owned_rows(tree, p.path, rs) {
            Ok(n) => rows = n,
            Err(()) => return die("all methods must belong to the paired owner"),
        }
    }
    if !rs {
        return Ok(());
    }
    if rows != 1 {
        return die("incomplete route ownership");
    }
    let refused = project_routes_core(core_file(tree, p.module).as_deref(), "OWNED_ROUTES");
    if refused != [format!("ALL {}", p.path).into_bytes()] {
        return die(&format!("Core must refuse exactly {}", p.path_text));
    }
    if code_has(core_file(tree, "routes/models.cjs").as_deref(), p.path) {
        return die("Core still carries the public handler");
    }
    if code_has(core_file(tree, "llamacpp-calibration.cjs").as_deref(), p.policy) {
        return die(&format!("Core still carries {}", p.policy_text));
    }
    let manager = core_file(tree, "llamacpp-manager.cjs");
    if !p.primitives.iter().all(|re| code_has(manager.as_deref(), re)) {
        return die("Core does not expose the live actor primitives");
    }
    if !tree.join("core/server").join(p.bridge_file).is_file() {
        return die("missing state bridge");
    }
    let index = code_lines(core_file(tree, "index.cjs").as_deref());
    if !contains(&index, p.mount.as_bytes()) {
        return die("missing state bridge mount");
    }
    match captured_secret(&index) {
        Some(secret) if secret_near(&index, p.ctor, &secret) => Ok(()),
        _ => die("state bridge needs the front captured secret"),
    }
}

/// Row V: the code egress issuer pairing (Core's fail-closed client marker and the front's
/// `rust-code-egress-issuer`).
pub fn code_egress_pairing(tree: &Path) -> Result<(), String> {
    let label = "code egress issuer pairing";
    let core = code_has(
        core_file(tree, "code-egress-client.cjs").as_deref(),
        "^const CODE_EGRESS_CLIENT_MARKER = 'noevia-code-egress-client/1';",
    );
    let rs = front_has(tree, "rust-code-egress-issuer");
    if core != rs {
        return Err(format!(
            "{label}: Core ('noevia-code-egress-client/1') and Rust ('rust-code-egress-issuer') must move together"
        ));
    }
    if !core {
        return Ok(());
    }
    if tree.join("core/server/code-egress-control.cjs").exists() {
        return Err(format!(
            "{label}: Core still carries the Node issuer (server/code-egress-control.cjs)"
        ));
    }
    let egress = core_file(tree, "code-egress.cjs");
    if code_has(
        egress.as_deref(),
        "publishWebKey|createKeyKeeper|createControlClient|mintGrant|secrets[.]derive",
    ) {
        return Err(format!("{label}: Core still publishes keys or signs grants"));
    }
    if !code_has(egress.as_deref(), r"require\('[.]/code-egress-client[.]cjs'\)") {
        return Err(format!(
            "{label}: Core's egress setup does not use the native client"
        ));
    }
    Ok(())
}
