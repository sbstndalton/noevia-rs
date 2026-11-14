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

/// `grep -qE` in the C locale: bytes, ASCII classes. `None` = a pattern grep would refuse too (status 2,
/// which every caller reads as "no match"). The script does not export `LC_ALL`, so `code_has`, its `sed`
/// and its awk run in the caller's locale; for the ASCII-only Core sources they read, that is the same.
/// The parity tests pin `LC_ALL=C`.
pub(crate) fn ere(pattern: &str) -> Option<Regex> {
    let translated = posix_ere(pattern)?;
    RegexBuilder::new(&translated).unicode(false).build().ok()
}

/// A POSIX extended regex in the regex crate's syntax (council p10). Inside a bracket expression POSIX has
/// no escapes and no nesting: `\` and a `[` that does not open `[:class:]` are literals, and a `]` right
/// after the opening `[` or `[^` is a literal. The regex crate reads `[` there as a nested class, `\` as an
/// escape and `&&`, `--`, `~~` as set operations, so the guards' `[[]'GET …'[]]` failed to compile and read
/// as "no match" where grep matches. Those characters are escaped. Outside brackets a `{` that does not
/// start a valid interval is a literal for GNU grep, and `{,n}` is `{0,n}`. `[=x=]`, `[.x.]` and an
/// unclosed bracket are refused (`None`, grep's status 2), as no guard uses them.
pub fn posix_ere(pattern: &str) -> Option<String> {
    let c: Vec<char> = pattern.chars().collect();
    let at = |i: usize| c.get(i).copied();
    let mut out = String::with_capacity(pattern.len() + 8);
    let mut i = 0usize;
    while let Some(ch) = at(i) {
        match ch {
            '\\' => {
                out.push('\\');
                if let Some(n) = at(i + 1) {
                    out.push(n);
                }
                i += 2;
            }
            '[' => {
                out.push('[');
                i += 1;
                if at(i) == Some('^') {
                    out.push('^');
                    i += 1;
                }
                if at(i) == Some(']') {
                    out.push_str("\\]");
                    i += 1;
                }
                let mut prev_dash = false;
                loop {
                    let b = at(i)?;
                    let next = at(i + 1);
                    let mut dash = false;
                    match b {
                        ']' => {
                            out.push(']');
                            i += 1;
                            break;
                        }
                        '[' if next == Some(':') => {
                            let mut j = i + 2;
                            while !(at(j)? == ':' && at(j + 1) == Some(']')) {
                                j += 1;
                            }
                            for k in i..j + 2 {
                                out.push(at(k)?);
                            }
                            i = j + 2;
                        }
                        '[' if matches!(next, Some('=') | Some('.')) => return None,
                        '[' | '\\' | '&' | '~' => {
                            out.push('\\');
                            out.push(b);
                            i += 1;
                        }
                        '-' if prev_dash => {
                            out.push_str("\\-");
                            i += 1;
                        }
                        _ => {
                            dash = b == '-';
                            out.push(b);
                            i += 1;
                        }
                    }
                    prev_dash = dash;
                }
            }
            '{' => match interval(&c, i + 1) {
                Some((lo, hi, end)) => {
                    out.push('{');
                    out.push_str(if lo.is_empty() { "0" } else { &lo });
                    if let Some(hi) = hi {
                        out.push(',');
                        out.push_str(&hi);
                    }
                    out.push('}');
                    i = end;
                }
                None => {
                    out.push_str("\\{");
                    i += 1;
                }
            },
            '}' => {
                out.push_str("\\}");
                i += 1;
            }
            _ => {
                out.push(ch);
                i += 1;
            }
        }
    }
    Some(out)
}

/// An interval body starting at `c[from]` (after the `{`): `n`, `n,`, `n,m` or `,m`, then `}`. Returns
/// the low bound (maybe empty), the high part (`Some("")` for `n,`) and the index after the `}`.
fn interval(c: &[char], from: usize) -> Option<(String, Option<String>, usize)> {
    let digits = |mut i: usize| {
        let mut s = String::new();
        while let Some(d) = c.get(i).filter(|d| d.is_ascii_digit()) {
            s.push(*d);
            i += 1;
        }
        (s, i)
    };
    let (lo, mut i) = digits(from);
    let mut hi = None;
    if c.get(i) == Some(&',') {
        let (h, j) = digits(i + 1);
        hi = Some(h);
        i = j;
    }
    if lo.is_empty() && hi.as_deref().is_none_or(str::is_empty) {
        return None;
    }
    (c.get(i) == Some(&'}')).then_some((lo, hi, i + 1))
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
    if !p
        .primitives
        .iter()
        .all(|re| code_has(manager.as_deref(), re))
    {
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

/// The five paths of deep research as Node's matcher took them (the block's awk `$1 ~ /…/`).
fn research_path(p: &[u8]) -> bool {
    let Some(rest) = p.strip_prefix(b"/api/projects/{projectId}/research".as_slice()) else {
        return false;
    };
    matches!(
        rest,
        b"" | b"/plan" | b"/{job}" | b"/{job}/cancel" | b"/{job}/save"
    )
}

/// The five refusals Core's `rust-research.cjs` must hold (params collapsed, byte order).
pub const RESEARCH_REFUSED: &[&str] = &[
    "ALL /api/projects/{}/research",
    "ALL /api/projects/{}/research/plan",
    "ALL /api/projects/{}/research/{}",
    "ALL /api/projects/{}/research/{}/cancel",
    "ALL /api/projects/{}/research/{}/save",
];

/// The block's last awk over `code_lines index.cjs`: the secret is captured (`cap`) before the first
/// `clientFromEnv(` (`cut`), never read from the environment after it (`late`), and handed to both the
/// admission routes and the research bridge within three lines of their constructors.
pub fn research_secret_order(index_code: &[u8], secret: &[u8]) -> bool {
    let (Some(cut_re), Some(env_re), Some(adm_re), Some(bridge_re)) = (
        ere(r"clientFromEnv\("),
        ere(r"process\.env\.NOEVIA_PROJECT_STORE_TOKEN"),
        ere(r"createAdmissionRoutes\(\{"),
        ere(r"createResearchBridge\(\{"),
    ) else {
        return false;
    };
    let mut capture = b"const ".to_vec();
    capture.extend_from_slice(secret);
    capture.extend_from_slice(b" = String(process.env.NOEVIA_PROJECT_STORE_TOKEN");
    let mut handed = b"secret: ".to_vec();
    handed.extend_from_slice(secret);
    let (mut cut, mut cap, mut adm, mut bridge) = (0usize, 0usize, 0usize, 0usize);
    let (mut late, mut adm_ok, mut bridge_ok) = (false, false, false);
    for (i, l) in here_lines(index_code).iter().enumerate() {
        let nr = i + 1;
        if cut == 0 && cut_re.is_match(l) {
            cut = nr;
        }
        if cap == 0 && contains(l, &capture) {
            cap = nr;
            continue; // awk `next`
        }
        if cut != 0 && nr > cut && env_re.is_match(l) {
            late = true;
        }
        if adm_re.is_match(l) {
            adm = nr;
        }
        if bridge_re.is_match(l) {
            bridge = nr;
        }
        if contains(l, &handed) {
            if adm != 0 && nr - adm <= 3 {
                adm_ok = true;
            }
            if bridge != 0 && nr - bridge <= 3 {
                bridge_ok = true;
            }
        }
    }
    cap != 0 && cut != 0 && cap < cut && !late && adm_ok && bridge_ok
}

/// #2218 deep research: Core's `rust-research.cjs` and the front's `rust-research`.
pub fn research_pairing(tree: &Path) -> Result<(), String> {
    let die = |what: &str| Err(format!("deep research ownership pairing: {what}"));
    let core = grep_fixed(
        &tree.join("core/server/index.cjs"),
        "require('./rust-research.cjs')",
    );
    let rs = front_has(tree, "rust-research");
    if core != rs {
        return die("Core and Rust must move together");
    }
    let contract = tree.join("rs/contracts/http/routes.toml");
    if !contract.is_file() {
        // An absent table is the route-table check's to report.
        return Ok(());
    }
    let rt = routes_toml_file(&contract);
    let mut paths: Vec<Vec<u8>> = Vec::new();
    if rt.error.is_none() {
        for r in &rt.routes {
            let [p, owner, switch, methods] = tab_fields(r);
            if !research_path(&p) {
                continue;
            }
            let ok = if rs {
                owner == b"rust" && switch == b"-" && methods == b"-"
            } else {
                owner == b"node"
            };
            if !ok {
                return die(
                    "its routes must be Rust's for every method with the new pair, and Node's with the old",
                );
            }
            paths.push(p);
        }
    }
    if !rs {
        return Ok(());
    }
    paths.sort();
    paths.dedup();
    if paths.len() != 5 {
        return die("incomplete route ownership");
    }
    let refused = project_routes_core(
        core_file(tree, "rust-research.cjs").as_deref(),
        "OWNED_ROUTES",
    );
    let want: Vec<Vec<u8>> = RESEARCH_REFUSED
        .iter()
        .map(|s| s.as_bytes().to_vec())
        .collect();
    if refused != want {
        return die("Core must refuse exactly the five paths the front answers");
    }
    let server = tree.join("core/server");
    if server.join("routes/research.cjs").exists()
        || server.join("research-service.cjs").exists()
    {
        return die("Core still carries the research handlers");
    }
    if !server.join("inference-admission-lease.cjs").is_file()
        || !server.join("rust-research-bridge.cjs").is_file()
    {
        return die(
            "Core does not offer the front its admission lease and tool bridge",
        );
    }
    let index = code_lines(core_file(tree, "index.cjs").as_deref());
    let mounted = |re: &str| {
        ere(re).is_some_and(|re| here_lines(&index).iter().any(|l| re.is_match(l)))
    };
    if !mounted(r"inferenceAdmission\.handle\(req, res\)") {
        return die("Core does not mount the admission lease route");
    }
    if !mounted(r"researchBridge\(req, res\)") {
        return die("Core does not mount the research bridge");
    }
    let Some(secret) = captured_secret(&index) else {
        return die(
            "Core does not capture the front's secret before its project-store client deletes it",
        );
    };
    if !research_secret_order(&index, &secret) {
        return die(
            "Core does not hand its admission lease and research bridge the front's secret (captured before clientFromEnv, never read from process.env after it)",
        );
    }
    Ok(())
}

/// The four project task reads of the task read pairing (`$1 ~ /…(code|browser)(\/\{task\})?$/`).
fn task_read_path(p: &[u8]) -> bool {
    let Some(rest) = p.strip_prefix(b"/api/projects/{projectId}/".as_slice()) else {
        return false;
    };
    matches!(
        rest,
        b"code" | b"browser" | b"code/{task}" | b"browser/{task}"
    )
}

/// The five refusals Core's `rust-task-reads.cjs` must hold (params collapsed, byte order).
pub const TASK_READS_REFUSED: &[&str] = &[
    "ALL /api/code/active",
    "GET /api/projects/{}/browser",
    "GET /api/projects/{}/browser/{}",
    "GET /api/projects/{}/code",
    "GET /api/projects/{}/code/{}",
];

/// `grep -qE PATTERN FILE 2>/dev/null` over the raw file (comments included): a missing file is false.
fn file_matches(path: &Path, pattern: &str) -> bool {
    let (Some(src), Some(re)) = (read_file(path), ere(pattern)) else {
        return false;
    };
    records(&src).iter().any(|l| re.is_match(l))
}

/// #2257 Code and Browser task reads: Core's `rust-task-reads.cjs` and the front's `rust-task-reads`.
pub fn task_reads_pairing(tree: &Path) -> Result<(), String> {
    let die = |what: &str| Err(format!("task read ownership pairing: {what}"));
    let core = grep_fixed(
        &tree.join("core/server/index.cjs"),
        "require('./rust-task-reads.cjs')",
    );
    let rs = front_has(tree, "rust-task-reads");
    if core != rs {
        return die("Core and Rust must move together");
    }
    let contract = tree.join("rs/contracts/http/routes.toml");
    if !contract.is_file() {
        // An absent table is the route-table check's to report.
        return Ok(());
    }
    let rt = routes_toml_file(&contract);
    let mut paths: Vec<Vec<u8>> = Vec::new();
    if rt.error.is_none() {
        for r in &rt.routes {
            let [p, owner, switch, methods] = tab_fields(r);
            let ok = if p == b"/api/code/active" {
                if rs {
                    owner == b"rust" && switch == b"-" && methods == b"-"
                } else {
                    owner == b"node"
                }
            } else if task_read_path(&p) {
                if rs {
                    let runs: Vec<&[u8]> = methods
                        .split(|c| !c.is_ascii_uppercase())
                        .filter(|m| !m.is_empty())
                        .collect();
                    owner == b"rust" && switch == b"-" && runs == [b"GET".as_slice()]
                } else {
                    owner == b"node"
                }
            } else {
                continue;
            };
            if !ok {
                return die(
                    "the active list must be Rust's whole and the four project reads Rust's for GET alone with the new pair, and Node's with the old",
                );
            }
            paths.push(p);
        }
    }
    if !rs {
        return Ok(());
    }
    paths.sort();
    paths.dedup();
    if paths.len() != 5 {
        return die("incomplete route ownership");
    }
    let refused = project_routes_core(
        core_file(tree, "rust-task-reads.cjs").as_deref(),
        "OWNED_ROUTES",
    );
    let want: Vec<Vec<u8>> = TASK_READS_REFUSED
        .iter()
        .map(|s| s.as_bytes().to_vec())
        .collect();
    if refused != want {
        return die("Core must refuse exactly the five reads the front answers");
    }
    let server = tree.join("core/server");
    for file in ["routes/code.cjs", "routes/browser.cjs"] {
        if file_matches(&server.join(file), r"service\.(list|listActive|get)\(") {
            return die("Core still carries the task read handlers");
        }
    }
    if !server.join("rust-task-facts.cjs").is_file() {
        return die("Core does not offer the front its task facts");
    }
    let index = code_lines(core_file(tree, "index.cjs").as_deref());
    let mounted = ere(r"taskFactsBridge\(req, res\)")
        .is_some_and(|re| here_lines(&index).iter().any(|l| re.is_match(l)));
    if !mounted {
        return die("Core does not mount the task facts bridge");
    }
    match captured_secret(&index) {
        Some(secret) if secret_near(&index, r"createTaskFactsBridge\(\{", &secret) => Ok(()),
        _ => die("Core does not hand its task facts bridge the front's captured secret"),
    }
}

/// The block's awk over Core's `rust-model-manager-scan.cjs` (raw lines, `//` lines and `/* … */` blocks
/// skipped): exactly one `OWNED_ROUTES` declaration, and it is either the literal GET of the path or
/// `'GET '+PATH` with exactly one `PATH` declaration naming the path. The last declaration of each wins,
/// as in the awk. A missing file is refused (awk cannot open it).
pub fn model_scan_marker(src: Option<&[u8]>) -> bool {
    let Some(src) = src else {
        return false;
    };
    let path = "/api/model-manager/models";
    let path_line = format!("const PATH = '{path}';");
    let literal_line = format!("const OWNED_ROUTES = Object.freeze(new Set(['GET {path}']));");
    let from_path_line = "const OWNED_ROUTES = Object.freeze(new Set(['GET '+PATH]));";
    let (mut block, mut paths, mut markers) = (false, 0usize, 0usize);
    let (mut correct_path, mut literal, mut from_path) = (false, false, false);
    for line in records(src) {
        if block {
            if contains(line, b"*/") {
                block = false;
            }
            continue;
        }
        let start = line
            .iter()
            .position(|c| !c.is_ascii_whitespace() && *c != 0x0b)
            .unwrap_or(line.len());
        let rest = line.get(start..).unwrap_or_default();
        if rest.starts_with(b"//") {
            continue;
        }
        if rest.starts_with(b"/*") {
            if !contains(line, b"*/") {
                block = true;
            }
            continue;
        }
        if line.starts_with(b"const PATH = ") {
            paths += 1;
            correct_path = line == path_line.as_bytes();
        }
        if line.starts_with(b"const OWNED_ROUTES = ") {
            markers += 1;
            literal = line == literal_line.as_bytes();
            from_path = line == from_path_line.as_bytes();
        }
    }
    markers == 1 && (literal || (from_path && paths == 1 && correct_path))
}

/// #2304 model scan read: Core's `rust-model-manager-scan.cjs` and the front's `rust-model-manager-scan`;
/// only the exact GET of `/api/model-manager/models` moves.
pub fn model_scan_pairing(tree: &Path) -> Result<(), String> {
    let die = |what: &str| Err(format!("model scan read ownership pairing: {what}"));
    let path = "/api/model-manager/models";
    let core = grep_fixed(
        &tree.join("core/server/index.cjs"),
        "require('./rust-model-manager-scan.cjs')",
    );
    let rs = front_has(tree, "rust-model-manager-scan");
    if core != rs {
        return die("Core and Rust must move together");
    }
    let mut rows = 0usize;
    let contract = tree.join("rs/contracts/http/routes.toml");
    if contract.is_file() {
        let rt = routes_toml_file(&contract);
        if rt.error.is_none() {
            for r in &rt.routes {
                let [p, owner, switch, methods] = tab_fields(r);
                if p != path.as_bytes() {
                    continue;
                }
                let ok = if rs {
                    owner == b"rust" && switch == b"-" && methods == b"GET"
                } else {
                    owner == b"node"
                };
                if !ok {
                    return die("only GET belongs to the paired owner");
                }
                rows += 1;
            }
        }
    }
    if !rs {
        return Ok(());
    }
    if rows != 1 {
        return die("incomplete route ownership");
    }
    if !model_scan_marker(core_file(tree, "rust-model-manager-scan.cjs").as_deref()) {
        return die("Core must refuse exactly this GET");
    }
    if code_has(
        core_file(tree, "routes/models.cjs").as_deref(),
        "X-Model-Scan|scanGeneration",
    ) {
        return die("Core still carries the public cache policy");
    }
    let models = core_file(tree, "models.cjs");
    for primitive in [
        r"modelScanCache[.]snapshot\(\)",
        r"modelScanCache[.]publish\(body,token\)",
        r"modelScanCache[.]refresh\(token\)",
    ] {
        if !code_has(models.as_deref(), primitive) {
            return die("existing shared cache primitives required");
        }
    }
    if !tree
        .join("core/server/rust-model-manager-scan-cache.cjs")
        .is_file()
    {
        return die("missing cache bridge");
    }
    let index = code_lines(core_file(tree, "index.cjs").as_deref());
    if !contains(&index, b"modelManagerScanCache(req, res)") {
        return die("missing cache bridge mount");
    }
    match captured_secret(&index) {
        Some(secret) if secret_near(&index, r"createModelManagerScanCache\(\{", &secret) => Ok(()),
        _ => die("cache bridge needs the front captured secret"),
    }
}
