//! The project storage write pairing and the run of route pairings from the close-out routes to the evidence
//! recheck (`assemble-release.sh` at noevia f23eb731, lines 1101-1125 and 2726-3349), and the tool approval, framing
//! suggestion and toolbox picker pairings (1693-1825), council p13.
//!
//! Most of them have one shape: Core's refusal module (a fixed string, or an ERE through `code_has`, in
//! `core/server/index.cjs`) and the front's FEATURES capability move together, and when they have moved,
//! `rs/contracts/http/routes.toml` gives Rust exactly the listed paths under a per-path methods rule; some add
//! checks of Core's and the front's sources after that. [`RoutePair`] is that shape as data; the pairings with
//! more (the providers' re-seal and probe, the MCP views' discovery check, the model state reads and the
//! evidence recheck) are functions on top of it.
//!
//! Each `Err` holds the `die` text without the `assemble-release: ` prefix. The shell compares sorted route
//! lists (locale `sort`, or `LC_ALL=C sort`, joined by newlines or by spaces) with a sorted literal; for the
//! paths these pairings list that is the same as comparing the lists as multisets, which is what this does
//! (a path holding a space or a newline could make two different joined lists equal in the shell; routes.toml
//! has none).

use crate::bridged::{captured_secret, code_has, secret_near};
use crate::core_requires::{core_requires_file, Requires};
use crate::guard::{
    code_lines, grep_fixed, move_capability, project_routes_core, read_file, routes_toml_file,
    tab_fields, MoveKind,
};
use crate::model_pairings::{text_has, text_has_fixed};
use std::path::Path;

const INDEX: &str = "core/server/index.cjs";
const FRONT_LIB: &str = "rs/bins/noevia-server/src/lib.rs";
const CONTRACT: &str = "rs/contracts/http/routes.toml";

/// How Core's half is found in `core/server/index.cjs`.
#[derive(Clone, Copy, Debug)]
pub enum Core<'a> {
    /// `grep -qF NEEDLE index.cjs 2>/dev/null`.
    Fixed(&'a str),
    /// `code_has index.cjs ERE` (comments do not count).
    Ere(&'a str),
}

/// The awk's rule over `$4` (the `methods` column) of one selected row. Every rule also needs `$2 == "rust"`
/// and `$3 == "-"` (no switch).
#[derive(Clone, Copy, Debug)]
pub enum Want<'a> {
    /// `$4 == "-"`: no `methods` key, every method is Rust's.
    All,
    /// The runs of capitals in `$4` are exactly this set, each once (`if (m[i] in seen) exit 1`, `k == nw`,
    /// every wanted one seen).
    Exact(&'a [&'a str]),
    /// Every run of capitals is one of these and there are exactly this many (`if (m[i] != want) exit 1;
    /// k++` … `if (k != n) exit 1`): repeats count, as in the awk.
    Counted(&'a [&'a str], usize),
}

impl Want<'_> {
    pub fn allow(&self, methods: &[u8]) -> bool {
        let runs = || {
            methods
                .split(|c| !c.is_ascii_uppercase())
                .filter(|m| !m.is_empty())
        };
        match self {
            Want::All => methods == b"-",
            Want::Exact(set) => {
                let mut seen: Vec<&[u8]> = Vec::new();
                for m in runs() {
                    if seen.contains(&m) {
                        return false;
                    }
                    seen.push(m);
                }
                seen.len() == set.len() && set.iter().all(|w| seen.contains(&w.as_bytes()))
            }
            Want::Counted(allowed, n) => {
                let mut k = 0usize;
                for m in runs() {
                    if !allowed.iter().any(|a| a.as_bytes() == m) {
                        return false;
                    }
                    k += 1;
                }
                k == *n
            }
        }
    }
}

/// Which rows of the table the awk looks at.
#[derive(Clone, Copy, Debug)]
pub enum Select<'a> {
    /// `$1 == path` for one of these, each with its own rule.
    Paths(&'a [(&'a str, Want<'a>)]),
    /// `$1 ~ /^BASE(\/|$)/`: the base path and everything under it, one rule.
    Under(&'a str, Want<'a>),
}

impl Select<'_> {
    fn rule_for(&self, path: &[u8]) -> Option<Want<'_>> {
        match self {
            Select::Paths(paths) => paths
                .iter()
                .find(|(p, _)| p.as_bytes() == path)
                .map(|(_, w)| *w),
            Select::Under(base, w) => {
                let rest = path.strip_prefix(base.as_bytes())?;
                (rest.is_empty() || rest.starts_with(b"/")).then_some(*w)
            }
        }
    }
}

/// One route pairing.
#[derive(Clone, Copy, Debug)]
pub struct RoutePair<'a> {
    /// The refusal prefix, e.g. `decision settings ownership pairing`.
    pub label: &'a str,
    pub core: Core<'a>,
    pub feature: &'a str,
    pub select: Select<'a>,
    /// The refusal when a selected row breaks its rule (or routes.toml is refused).
    pub rule_text: &'a str,
    /// The paths the awk must print, each once (`… incomplete route ownership` otherwise).
    pub expected: &'a [&'a str],
}

fn front_has(tree: &Path, feature: &str) -> bool {
    move_capability(
        read_file(&tree.join(FRONT_LIB)).as_deref(),
        "FEATURES",
        feature,
        MoveKind::Features,
    )
}

fn core_has(tree: &Path, core: Core) -> bool {
    match core {
        Core::Fixed(needle) => grep_fixed(&tree.join(INDEX), needle),
        Core::Ere(ere) => code_has(read_file(&tree.join(INDEX)).as_deref(), ere),
    }
}

fn file_has(tree: &Path, rel: &str, ere: &str) -> bool {
    code_has(read_file(&tree.join(rel)).as_deref(), ere)
}

fn die(label: &str, text: &str) -> String {
    format!("{label}: {text}")
}

/// The owner half: Core's marker and the front's capability agree (`… Core and Rust must move together`).
/// `Ok(true)` when both have moved.
pub fn owner(tree: &Path, label: &str, core: Core, feature: &str) -> Result<bool, String> {
    let rs = front_has(tree, feature);
    if core_has(tree, core) != rs {
        return Err(die(label, "Core and Rust must move together"));
    }
    Ok(rs)
}

/// The route half of a moved pair: the contract exists, every selected row keeps its rule, and the selected
/// paths are exactly `expected`.
pub fn routes(tree: &Path, p: &RoutePair) -> Result<(), String> {
    let contract = tree.join(CONTRACT);
    if !contract.is_file() {
        return Err(die(p.label, "missing route contract"));
    }
    let rt = routes_toml_file(&contract);
    if rt.error.is_some() {
        // set -o pipefail: a refused routes.toml fails the pipeline.
        return Err(die(p.label, p.rule_text));
    }
    let mut got: Vec<Vec<u8>> = Vec::new();
    for r in &rt.routes {
        let [path, owner, switch, methods] = tab_fields(r);
        let Some(want) = p.select.rule_for(&path) else {
            continue;
        };
        if owner != b"rust" || switch != b"-" || !want.allow(&methods) {
            return Err(die(p.label, p.rule_text));
        }
        got.push(path);
    }
    let mut want: Vec<Vec<u8>> = p.expected.iter().map(|s| s.as_bytes().to_vec()).collect();
    got.sort();
    want.sort();
    if got != want {
        return Err(die(p.label, "incomplete route ownership"));
    }
    Ok(())
}

/// A whole [`RoutePair`]: the owner half, then the routes when it has moved. `Ok(true)` when it has.
pub fn route_pair(tree: &Path, p: &RoutePair) -> Result<bool, String> {
    let moved = owner(tree, p.label, p.core, p.feature)?;
    if moved {
        routes(tree, p)?;
    }
    Ok(moved)
}

// ---------------------------------------------------------------------------------------------------------
// The project storage writes (script lines 1101-1125)

/// The project storage write pairing: Core loads `rust-project-storage.cjs` from `index.cjs`, or through the
/// `rust-index-outbox.cjs` that `index.cjs` loads, by a real `require` call ([`core_requires_file`]); the front
/// names `rust-project-storage` in FEATURES. A moved pair needs the project create/delete pair's front half
/// (`lifecycle_rs_owner`). `Ok(true)` when it has moved (the script's `project_storage_rs_owner`).
pub fn project_storage_pairing(tree: &Path, lifecycle_rs_owner: bool) -> Result<bool, String> {
    const LABEL: &str = "project storage write ownership pairing";
    let server = tree.join("core/server");
    let index = server.join("index.cjs");
    let direct = core_requires_file(&index, "./rust-project-storage.cjs");
    let outbox = core_requires_file(&index, "./rust-index-outbox.cjs");
    let chain = if outbox == Requires::Yes {
        core_requires_file(
            &server.join("rust-index-outbox.cjs"),
            "./rust-project-storage.cjs",
        )
    } else {
        Requires::No
    };
    if [direct, outbox, chain].contains(&Requires::Invalid) {
        return Err(die(LABEL, "Core wiring could not be read"));
    }
    let core = direct == Requires::Yes || chain == Requires::Yes;
    let rs = front_has(tree, "rust-project-storage");
    if core != rs {
        return Err(die(LABEL, "Core and Rust must move together"));
    }
    if rs && !lifecycle_rs_owner {
        return Err(die(LABEL, "needs the project create/delete pair"));
    }
    Ok(rs)
}

// ---------------------------------------------------------------------------------------------------------
// Tool approvals and the approval registry, the framing suggestion, the toolbox picker (1693-1825)

/// Tool approval decisions (#2282) and the approval registry (port contract S9): the approval POST is Rust's
/// with the new pair and Node's with the old; with the registry Core's pending-state bridge is gone, without
/// it the bridge is there, mounted and handed the front's captured secret.
pub fn tool_approval_pairings(tree: &Path) -> Result<(), String> {
    const TA: &str = "tool approval ownership pairing";
    const AR: &str = "approval registry ownership pairing";
    let ta = owner(
        tree,
        TA,
        Core::Fixed("require('./rust-tool-approvals.cjs')"),
        "rust-tool-approvals",
    )?;
    let index = code_lines(read_file(&tree.join(INDEX)).as_deref());
    let ar_core = text_has_fixed(&index, "require('./rust-approval-registry.cjs')");
    let ar = front_has(tree, "rust-approval-registry");
    if ar_core && !ar {
        return Err(die(
            AR,
            "Core requires rust-approval-registry.cjs (keeps no pending approvals) but the Rust front lacks the rust-approval-registry capability",
        ));
    }
    if ar && !ar_core {
        return Err(die(
            AR,
            "the Rust front has the rust-approval-registry capability but Core lacks require('./rust-approval-registry.cjs') (Core still holds the pending approvals)",
        ));
    }
    if ar && !ta {
        return Err(die(
            AR,
            "the front's rust-approval-registry needs the tool approval pair (rust-tool-approvals and Core's rust-tool-approvals.cjs refusal)",
        ));
    }
    if !tree.join(CONTRACT).is_file() {
        return Ok(());
    }
    let rows = generation_rows(tree, &["/api/tool-approvals/{approvalId}"], ta, POST1).map_err(|()| {
        die(
            TA,
            "POST alone must be Rust's with the new pair and Node's with the old",
        )
    })?;
    if !ta {
        return Ok(());
    }
    if rows.len() != 1 {
        return Err(die(TA, "missing route"));
    }
    let refused = project_routes_core(
        read_file(&tree.join("core/server/rust-tool-approvals.cjs")).as_deref(),
        "OWNED_ROUTES",
    );
    if refused != [b"POST /api/tool-approvals/{}".to_vec()] {
        return Err(die(TA, "Core must refuse exactly the approval POST"));
    }
    if tree.join("core/server/routes/approvals.cjs").is_file() {
        return Err(die(TA, "Core still carries the public handler"));
    }
    if ar {
        if !tree.join("core/server/rust-approval-registry.cjs").is_file() {
            return Err(die(
                TA,
                "Core (rust-approval-registry) is missing server/rust-approval-registry.cjs",
            ));
        }
        if tree.join("core/server/rust-tool-approval-bridge.cjs").exists() {
            return Err(die(
                TA,
                "Core (rust-approval-registry) still carries the pending state bridge server/rust-tool-approval-bridge.cjs",
            ));
        }
        if text_has(
            &index,
            r"toolApprovalBridge\(req, res\)|createToolApprovalBridge\(",
        ) {
            return Err(die(
                TA,
                "Core (rust-approval-registry) still mounts the pending state bridge",
            ));
        }
    } else {
        if !tree.join("core/server/rust-tool-approval-bridge.cjs").is_file() {
            return Err(die(TA, "missing pending state bridge"));
        }
        if !text_has(&index, r"toolApprovalBridge\(req, res\)") {
            return Err(die(TA, "missing bridge mount"));
        }
        if !secret_handed(tree, r"createToolApprovalBridge\(\{") {
            return Err(die(TA, "missing captured front secret"));
        }
    }
    Ok(())
}

const FRAMING_SUGGEST: &str = "/api/chat-framing/suggest";

/// The framing suggestion (chat port S4): every method of the path is Rust's with the new pair and never
/// Rust's with the old; a paired Core refuses exactly its POST, makes no suggestion itself and serves the
/// front's bridge with the captured secret.
pub fn framing_suggest_pairing(tree: &Path) -> Result<(), String> {
    const LABEL: &str = "framing suggestion ownership pairing";
    let rs = owner(
        tree,
        LABEL,
        Core::Fixed("require('./rust-chat-framing-suggest.cjs')"),
        "rust-chat-framing-suggest",
    )?;
    let contract = tree.join(CONTRACT);
    if !rs && contract.is_file() {
        let rt = routes_toml_file(&contract);
        if rt.error.is_none()
            && rt.routes.iter().any(|r| {
                let [path, owner, _, _] = tab_fields(r);
                path == FRAMING_SUGGEST.as_bytes() && owner == b"rust"
            })
        {
            return Err(die(LABEL, "an old pair cannot give the suggestion to Rust"));
        }
    }
    if !rs {
        return Ok(());
    }
    routes(
        tree,
        &RoutePair {
            label: LABEL,
            core: Core::Fixed(""),
            feature: "",
            select: Select::Paths(&[(FRAMING_SUGGEST, Want::All)]),
            rule_text: "every method must belong to Rust",
            expected: &[FRAMING_SUGGEST],
        },
    )?;
    if !file_has(
        tree,
        "core/server/rust-chat-framing-suggest.cjs",
        "^const OWNED_ROUTES = Object[.]freeze[(]new Set[(][[]'POST /api/chat-framing/suggest'[]][)][)];$",
    ) {
        return Err(die(LABEL, "Core must refuse exactly this POST"));
    }
    if file_has(tree, "core/server/routes/chat-framing.cjs", "framing[.]suggest[(]") {
        return Err(die(
            LABEL,
            "Core still carries the public suggestion handler",
        ));
    }
    let index = code_lines(read_file(&tree.join(INDEX)).as_deref());
    if !text_has(&index, r"chatFramingSuggestBridge\(req, res\)") {
        return Err(die(LABEL, "missing bridge mount"));
    }
    if !secret_handed(tree, r"createSuggestBridge\(\{") {
        return Err(die(LABEL, "missing captured front secret"));
    }
    Ok(())
}

/// The toolbox picker (#2281), with `discovery_rs_owner` the MCP discovery pairing's front verdict: with it
/// the picker's facts are the front's own, without it Core serves the raw facts bridge.
pub fn toolbox_picker_pairing(tree: &Path, discovery_rs_owner: bool) -> Result<(), String> {
    const LABEL: &str = "toolbox picker ownership pairing";
    let rs = owner(
        tree,
        LABEL,
        Core::Fixed("require('./rust-toolbox-picker.cjs')"),
        "rust-toolbox-picker",
    )?;
    // `tp_rows` stays unset (empty) without a contract.
    let mut rows = Vec::new();
    if tree.join(CONTRACT).is_file() {
        rows = generation_rows(tree, &["/api/toolboxes"], rs, Want::All).map_err(|()| {
            die(LABEL, "all methods must belong to the paired owner")
        })?;
    }
    if !rs {
        return Ok(());
    }
    if rows.len() != 1 {
        return Err(die(LABEL, "incomplete route ownership"));
    }
    let refused = project_routes_core(
        read_file(&tree.join("core/server/rust-toolbox-picker.cjs")).as_deref(),
        "OWNED_ROUTES",
    );
    if refused != [b"ALL /api/toolboxes".to_vec()] {
        return Err(die(LABEL, "Core must refuse exactly the picker"));
    }
    if tree.join("core/server/routes/toolboxes.cjs").is_file()
        || tree.join("core/server/mcp-status.cjs").is_file()
    {
        return Err(die(
            LABEL,
            "Core still carries the public handler or status composition",
        ));
    }
    if file_has(tree, "core/server/toolboxes.cjs", "function toolboxSummaries") {
        return Err(die(LABEL, "Core still carries picker summary composition"));
    }
    if discovery_rs_owner {
        return Ok(());
    }
    if !tree.join("core/server/rust-toolbox-picker-facts.cjs").is_file() {
        return Err(die(LABEL, "missing raw facts bridge"));
    }
    let index = code_lines(read_file(&tree.join(INDEX)).as_deref());
    if !text_has_fixed(&index, "toolboxPickerFacts(req, res)") {
        return Err(die(LABEL, "missing facts mount"));
    }
    if !secret_handed(tree, r"createToolboxPickerFacts\(\{") {
        return Err(die(LABEL, "facts bridge needs the front captured secret"));
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------------------------
// The close-out routes (2726-2776)

/// `get_only_pair MARKER CAPABILITY LABEL ROUTE`, with `auto_roles_write` the auto-roles write pairing's
/// verdict (`auto_roles_write_owner = rust`): then `/api/auto-roles` is Rust's for exactly GET and PUT.
pub fn get_only_pair(
    tree: &Path,
    marker: &str,
    capability: &str,
    label: &str,
    route: &str,
    auto_roles_write: bool,
) -> Result<bool, String> {
    let needle = format!("require('./{marker}')");
    let label = format!("{label} ownership pairing");
    let want = if route == "/api/auto-roles" && auto_roles_write {
        Want::Exact(&["GET", "PUT"])
    } else {
        Want::Counted(&["GET"], 1)
    };
    let rule = format!("exactly GET of {route} must belong to Rust");
    let paths = [(route, want)];
    route_pair(
        tree,
        &RoutePair {
            label: &label,
            core: Core::Fixed(&needle),
            feature: capability,
            select: Select::Paths(&paths),
            rule_text: &rule,
            expected: &[route],
        },
    )
}

/// The five GET-only close-out pairs in the script's order (health, auto-roles read, workspace list, chat
/// search, chat download), then the chat search's and download's Core checks.
pub fn close_out_pairings(tree: &Path, auto_roles_write: bool) -> Result<(), String> {
    for (marker, capability, label, route) in [
        ("rust-health.cjs", "rust-health", "health check", "/api/health"),
        (
            "rust-auto-roles.cjs",
            "rust-auto-roles",
            "auto-roles read",
            "/api/auto-roles",
        ),
        (
            "rust-workspace.cjs",
            "rust-workspace",
            "workspace list",
            "/api/workspace",
        ),
        (
            "rust-chat-search.cjs",
            "rust-chat-search",
            "chat search",
            "/api/chats/search",
        ),
        (
            "rust-chat-export.cjs",
            "rust-chat-export",
            "chat download",
            "/api/chats/{chatId}/export",
        ),
    ] {
        get_only_pair(tree, marker, capability, label, route, auto_roles_write)?;
    }
    let lists = "core/server/routes/chat-lists.cjs";
    if grep_fixed(&tree.join(INDEX), "require('./rust-chat-search.cjs')") {
        const LABEL: &str = "chat search ownership pairing";
        if !file_has(
            tree,
            "core/server/rust-chat-search.cjs",
            "^const OWNED_ROUTES = Object[.]freeze[(]new Set[(][[]'GET /api/chats/search'[]][)][)];$",
        ) {
            return Err(die(LABEL, "Core must refuse exactly this GET"));
        }
        if file_has(tree, lists, "p === '/api/chats/search'") {
            return Err(die(LABEL, "Core still carries the public search handler"));
        }
    }
    if grep_fixed(&tree.join(INDEX), "require('./rust-chat-export.cjs')") {
        const LABEL: &str = "chat download ownership pairing";
        if !file_has(
            tree,
            "core/server/rust-chat-export.cjs",
            "^const OWNED_ROUTES = Object[.]freeze[(]new Set[(][[]'GET /api/chats/[{]chatId[}]/export'[]][)][)];$",
        ) {
            return Err(die(LABEL, "Core must refuse exactly this GET"));
        }
        if file_has(tree, lists, "chatMarkdown[(]") {
            return Err(die(LABEL, "Core still carries the public download handler"));
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------------------------
// Settings and storage (2777-2870)

pub const DECISION_SETTINGS: RoutePair<'static> = RoutePair {
    label: "decision settings ownership pairing",
    core: Core::Fixed("require('./rust-decision-settings.cjs')"),
    feature: "rust-decision-settings",
    select: Select::Paths(&[
        ("/api/admin/decision-settings", Want::All),
        ("/api/admin/decision-settings/test", Want::All),
    ]),
    rule_text: "all methods must belong to Rust",
    expected: &[
        "/api/admin/decision-settings",
        "/api/admin/decision-settings/test",
    ],
};

const POST1: Want<'static> = Want::Counted(&["POST"], 1);
const GET1: Want<'static> = Want::Counted(&["GET"], 1);
const PUT1: Want<'static> = Want::Counted(&["PUT"], 1);

pub const STORAGE_INTEGRATIONS: RoutePair<'static> = RoutePair {
    label: "storage integration ownership pairing",
    core: Core::Fixed("require('./rust-storage-integrations.cjs')"),
    feature: "rust-storage-integrations",
    select: Select::Paths(&[
        ("/api/integrations/storage/folder", POST1),
        ("/api/integrations/storage/test", POST1),
        ("/api/integrations/storage/nextcloud/start", POST1),
        ("/api/integrations/storage/nextcloud/poll", POST1),
        ("/api/integrations/storage/nextcloud/cancel", POST1),
    ]),
    rule_text: "exactly POST of all five routes must belong to Rust",
    expected: &[
        "/api/integrations/storage/folder",
        "/api/integrations/storage/test",
        "/api/integrations/storage/nextcloud/start",
        "/api/integrations/storage/nextcloud/poll",
        "/api/integrations/storage/nextcloud/cancel",
    ],
};

pub const VAULT_MIRROR: RoutePair<'static> = RoutePair {
    label: "vault mirror preference ownership pairing",
    core: Core::Fixed("require('./rust-vault-mirror-preferences.cjs')"),
    feature: "rust-vault-mirror-preferences",
    select: Select::Paths(&[("/api/chat-vault-mirror/preferences", Want::All)]),
    rule_text: "every method of the path must belong to Rust",
    expected: &["/api/chat-vault-mirror/preferences"],
};

pub const STORAGE_CONNECTION: RoutePair<'static> = RoutePair {
    label: "storage connection ownership pairing",
    core: Core::Fixed("require('./rust-storage-connection.cjs')"),
    feature: "rust-storage-connection",
    select: Select::Paths(&[("/api/integrations/storage", Want::Exact(&["GET", "PUT"]))]),
    rule_text: "exactly GET and PUT of the path must belong to Rust",
    expected: &["/api/integrations/storage"],
};

// ---------------------------------------------------------------------------------------------------------
// Providers (2871-2987)

pub const PROVIDER_REGISTRY: RoutePair<'static> = RoutePair {
    label: "provider registry ownership pairing",
    core: Core::Fixed("require('./rust-providers.cjs')"),
    feature: "rust-providers",
    select: Select::Paths(&[
        ("/api/providers", Want::Exact(&["GET", "POST"])),
        ("/api/providers/{id}", Want::Exact(&["PUT", "DELETE"])),
        ("/api/providers/chatgpt", Want::Exact(&["DELETE", "GET"])),
        ("/api/providers/test", Want::Exact(&["DELETE", "POST", "PUT"])),
    ]),
    rule_text: "exactly the registry's methods of the routes must belong to Rust",
    expected: &[
        "/api/providers",
        "/api/providers/chatgpt",
        "/api/providers/test",
        "/api/providers/{id}",
    ],
};

/// The provider registry, its keys' re-seal (moves with the registry) and the provider probe (whose route
/// widens to PUT and DELETE with the registry), in the script's order.
pub fn provider_pairings(tree: &Path) -> Result<(), String> {
    let registry = route_pair(tree, &PROVIDER_REGISTRY)?;

    const RESEAL: &str = "provider key reseal ownership pairing";
    let reseal = owner(
        tree,
        RESEAL,
        Core::Fixed("require('./rust-providers-reseal.cjs')"),
        "rust-providers-reseal",
    )?;
    if reseal != registry {
        return Err(die(
            RESEAL,
            "it moves with the provider registry (a registry without it leaves a second writer of the provider files)",
        ));
    }
    if reseal {
        if !grep_fixed(
            &tree.join("rs/bins/noevia-server/src/project_store.rs"),
            "\"/v1/providers/reseal\"",
        ) {
            return Err(die(
                RESEAL,
                "the front does not serve /v1/providers/reseal",
            ));
        }
        if !grep_fixed(
            &tree.join("core/server/project-store-client.cjs"),
            "providersReseal",
        ) {
            return Err(die(
                RESEAL,
                "Core's store client does not call /v1/providers/reseal",
            ));
        }
    }

    let probe_rule = if registry {
        Want::Counted(&["POST", "PUT", "DELETE"], 3)
    } else {
        POST1
    };
    let probe_paths = [("/api/providers/test", probe_rule)];
    route_pair(
        tree,
        &RoutePair {
            label: "provider test ownership pairing",
            core: Core::Fixed("require('./rust-provider-test.cjs')"),
            feature: "rust-provider-test",
            select: Select::Paths(&probe_paths),
            rule_text: "exactly POST of the path must belong to Rust",
            expected: &["/api/providers/test"],
        },
    )?;
    Ok(())
}

pub const CHATGPT_SIGNIN: RoutePair<'static> = RoutePair {
    label: "ChatGPT sign-in ownership pairing",
    core: Core::Fixed("require('./rust-chatgpt-oauth.cjs')"),
    feature: "rust-chatgpt-oauth",
    select: Select::Paths(&[
        ("/api/providers/chatgpt/device", POST1),
        ("/api/providers/chatgpt/device/poll", POST1),
        ("/api/providers/chatgpt/device/cancel", POST1),
        ("/api/providers/chatgpt/models", GET1),
    ]),
    rule_text: "exactly the broker's methods of the routes must belong to Rust",
    expected: &[
        "/api/providers/chatgpt/device",
        "/api/providers/chatgpt/device/cancel",
        "/api/providers/chatgpt/device/poll",
        "/api/providers/chatgpt/models",
    ],
};

// ---------------------------------------------------------------------------------------------------------
// MCP (2988-3128)

pub const MCP_CREDENTIALS: RoutePair<'static> = RoutePair {
    label: "MCP credential ownership pairing",
    core: Core::Ere(r"require\('\./rust-mcp-credentials\.cjs'\)"),
    feature: "rust-mcp-credentials",
    select: Select::Paths(&[
        ("/api/mcp-keys/servers", Want::Exact(&["GET"])),
        ("/api/mcp-oauth/servers", Want::Exact(&["GET"])),
        ("/api/mcp-oauth/callback", Want::Exact(&["GET"])),
        ("/api/mcp-keys/{*}", Want::Exact(&["DELETE", "PUT"])),
        ("/api/mcp-oauth/{id}", Want::Exact(&["DELETE"])),
        ("/api/mcp-oauth/{id}/connect", Want::Exact(&["POST"])),
    ]),
    rule_text: "exactly the broker's methods of the routes must belong to Rust",
    expected: &[
        "/api/mcp-keys/servers",
        "/api/mcp-keys/{*}",
        "/api/mcp-oauth/callback",
        "/api/mcp-oauth/servers",
        "/api/mcp-oauth/{id}",
        "/api/mcp-oauth/{id}/connect",
    ],
};

/// The MCP credentials: the routes, then the front serves `/v1/mcp/*` and Core's store client calls the
/// three ops (code, not comments).
pub fn mcp_credentials_pairing(tree: &Path) -> Result<(), String> {
    let p = &MCP_CREDENTIALS;
    if !route_pair(tree, p)? {
        return Ok(());
    }
    if !file_has(tree, "rs/bins/noevia-server/src/project_store.rs", "/v1/mcp/") {
        return Err(die(p.label, "the front does not serve /v1/mcp/*"));
    }
    for op in ["mcpSession", "mcpKey", "mcpReseal"] {
        let ere = format!(r"(^|[^A-Za-z0-9_]){op}[[:space:]]*\(");
        if !file_has(tree, "core/server/project-store-client.cjs", &ere) {
            return Err(die(
                p.label,
                &format!("Core's store client does not call {op}"),
            ));
        }
    }
    Ok(())
}

/// `discovery_pair_ok`, with `discovery_rs_owner` the MCP discovery pairing's front verdict (`md_rs_owner`).
pub fn discovery_pair_ok(tree: &Path, discovery_rs_owner: bool) -> bool {
    if !file_has(tree, FRONT_LIB, "pub[[:space:]]+mod[[:space:]]+mcp_discovery") {
        return false;
    }
    if discovery_rs_owner {
        file_has(tree, INDEX, r"createChatFactsPublisher[[:space:]]*\(")
            && tree.join("core/server/toolbox-chat-facts.cjs").is_file()
    } else {
        file_has(tree, INDEX, r"createMcpDiscoveryPublisher[[:space:]]*\(")
            && tree.join("core/server/mcp-discovery-snapshot.cjs").is_file()
    }
}

pub const MCP_DIRECTORY: RoutePair<'static> = RoutePair {
    label: "MCP directory ownership pairing",
    core: Core::Ere(r"require\('\./rust-mcp-directory\.cjs'\)"),
    feature: "rust-mcp-directory",
    select: Select::Under("/api/admin/mcp-directory", Want::All),
    rule_text: "every method of the paths must belong to Rust",
    expected: &[
        "/api/admin/mcp-directory",
        "/api/admin/mcp-directory/custom",
        "/api/admin/mcp-directory/custom/preview",
        "/api/admin/mcp-directory/{*}",
        "/api/admin/mcp-directory/{*}/keys",
        "/api/admin/mcp-directory/{id}/oauth-client",
    ],
};

pub const PERMITTED_TOOLS: RoutePair<'static> = RoutePair {
    label: "permitted tools ownership pairing",
    core: Core::Ere(r"require\('\./rust-toolboxes-permitted\.cjs'\)"),
    feature: "rust-toolboxes-permitted",
    select: Select::Paths(&[("/api/toolboxes/permitted", Want::All)]),
    rule_text: "every method of the path must belong to Rust",
    expected: &["/api/toolboxes/permitted"],
};

pub const CONNECTORS: RoutePair<'static> = RoutePair {
    label: "connectors ownership pairing",
    core: Core::Ere(r"require\('\./rust-connectors\.cjs'\)"),
    feature: "rust-connectors",
    select: Select::Paths(&[
        ("/api/connectors", GET1),
        ("/api/connectors/nextcloud/policy", PUT1),
    ]),
    rule_text: "exactly GET /api/connectors and PUT /api/connectors/nextcloud/policy must belong to Rust",
    expected: &["/api/connectors", "/api/connectors/nextcloud/policy"],
};

const DISCOVERY_TEXT: &str = "Core does not publish the discovery snapshot the front reads";

/// The admin MCP directory, the permitted-tools view and the connectors list, in the script's order.
pub fn mcp_views_pairings(tree: &Path, discovery_rs_owner: bool) -> Result<(), String> {
    let d = &MCP_DIRECTORY;
    if route_pair(tree, d)? {
        if !discovery_pair_ok(tree, discovery_rs_owner) {
            return Err(die(d.label, DISCOVERY_TEXT));
        }
        if file_has(
            tree,
            "core/server/directory-mcp.cjs",
            "(INSERT[[:space:]]+(OR[[:space:]]+[A-Za-z]+[[:space:]]+)?INTO|REPLACE[[:space:]]+INTO|UPDATE|DELETE[[:space:]]+FROM)[[:space:]]+.?directory_mcp_servers",
        ) {
            return Err(die(d.label, "Core still writes directory_mcp_servers"));
        }
        if !file_has(
            tree,
            "core/server/rust-mcp-credentials.cjs",
            "directory_mcp_servers",
        ) {
            return Err(die(
                d.label,
                "Core does not treat directory_mcp_servers as the front's",
            ));
        }
        if !file_has(
            tree,
            "rs/bins/noevia-server/src/mcp_broker.rs",
            "directory_mcp_servers",
        ) {
            return Err(die(d.label, "the front does not re-seal the shared keys"));
        }
    }
    for p in [&PERMITTED_TOOLS, &CONNECTORS] {
        if route_pair(tree, p)? && !discovery_pair_ok(tree, discovery_rs_owner) {
            return Err(die(p.label, DISCOVERY_TEXT));
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------------------------
// Provider models, Google Drive, off-site backup, model sizing (3129-3233)

pub const PROVIDER_MODELS: RoutePair<'static> = RoutePair {
    label: "provider model list ownership pairing",
    core: Core::Fixed("require('./rust-provider-models.cjs')"),
    feature: "rust-provider-models",
    select: Select::Paths(&[("/api/providers/{providerId}/models{*?}", Want::All)]),
    rule_text: "the route must belong to Rust for every method",
    expected: &["/api/providers/{providerId}/models{*?}"],
};

pub const GOOGLE_DRIVE: RoutePair<'static> = RoutePair {
    label: "Google Drive ownership pairing",
    core: Core::Fixed("require('./rust-google-drive.cjs')"),
    feature: "rust-google-drive",
    select: Select::Paths(&[
        ("/api/connectors/gdrive/connect", POST1),
        ("/api/connectors/gdrive/disconnect", POST1),
        ("/api/connectors/gdrive/policy", PUT1),
        ("/api/connectors/gdrive/backup-copy", PUT1),
    ]),
    rule_text: "exactly the connector writes' methods of the routes must belong to Rust",
    expected: &[
        "/api/connectors/gdrive/backup-copy",
        "/api/connectors/gdrive/connect",
        "/api/connectors/gdrive/disconnect",
        "/api/connectors/gdrive/policy",
    ],
};

pub const OFFSITE_BACKUP: RoutePair<'static> = RoutePair {
    label: "off-site backup ownership pairing",
    core: Core::Fixed("require('./rust-offsite-backup.cjs')"),
    feature: "rust-offsite-backup",
    select: Select::Under("/api/admin/offsite-backup", Want::All),
    rule_text: "every method of the paths must belong to Rust",
    expected: &[
        "/api/admin/offsite-backup",
        "/api/admin/offsite-backup/copy",
        "/api/admin/offsite-backup/google/connect",
        "/api/admin/offsite-backup/google/disconnect",
        "/api/admin/offsite-backup/new-store",
        "/api/admin/offsite-backup/recovery-key",
        "/api/admin/offsite-backup/run",
        "/api/admin/offsite-backup/verify",
    ],
};

/// The off-site backup: the routes, then Core refuses the new-store path too.
pub fn offsite_backup_pairing(tree: &Path) -> Result<(), String> {
    let p = &OFFSITE_BACKUP;
    if route_pair(tree, p)?
        && !grep_fixed(
            &tree.join("core/server/rust-offsite-backup.cjs"),
            "'/api/admin/offsite-backup/new-store'",
        )
    {
        return Err(die(p.label, "Core does not refuse the new-store path"));
    }
    Ok(())
}

pub const MODEL_SIZING: RoutePair<'static> = RoutePair {
    label: "model sizing ownership pairing",
    core: Core::Fixed("require('./rust-model-sizing.cjs')"),
    feature: "rust-model-sizing",
    select: Select::Paths(&[
        ("/api/models/estimate", GET1),
        ("/api/models/preset/suggest", GET1),
    ]),
    rule_text: "exactly GET of both routes must belong to Rust",
    expected: &["/api/models/estimate", "/api/models/preset/suggest"],
};

// ---------------------------------------------------------------------------------------------------------
// Model state reads, maintenance and the evidence recheck (3234-3349)

/// The awk over `printf '%s\n' "$table"` with `-v want=…`, where a table `routes_toml` refuses reads as empty
/// (`… 2>/dev/null)" || x=""`): each row of one of `paths` keeps `rust_rule` (with `$2 == "rust"`, `$3 == "-"`)
/// when `rust`, and `$2 == "node"` otherwise. `Ok` holds the printed paths; `Err(())` is the awk's `exit 1`.
fn generation_rows(
    tree: &Path,
    paths: &[&str],
    rust: bool,
    rust_rule: Want,
) -> Result<Vec<Vec<u8>>, ()> {
    let contract = tree.join(CONTRACT);
    let rt = routes_toml_file(&contract);
    if rt.error.is_some() {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    for r in &rt.routes {
        let [path, owner, switch, methods] = tab_fields(r);
        if !paths.iter().any(|p| p.as_bytes() == path.as_slice()) {
            continue;
        }
        if rust && (owner != b"rust" || switch != b"-" || !rust_rule.allow(&methods)) {
            return Err(());
        }
        if !rust && owner != b"node" {
            return Err(());
        }
        out.push(path);
    }
    Ok(out)
}

/// Core's `index.cjs` hands the bridge built by `ctor` (an ERE) the front's captured secret (within three lines).
fn secret_handed(tree: &Path, ctor: &str) -> bool {
    let index = code_lines(read_file(&tree.join(INDEX)).as_deref());
    match captured_secret(&index) {
        Some(secret) => secret_near(&index, ctor, &secret),
        None => false,
    }
}

/// GET inference-budget and GET installed (#2262). Unlike the others, the rows are checked for the old pair
/// too (Node's), whenever routes.toml exists.
pub fn model_state_reads_pairing(tree: &Path) -> Result<(), String> {
    const LABEL: &str = "model state read ownership pairing";
    let rs = owner(
        tree,
        LABEL,
        Core::Fixed("require('./rust-model-state-reads.cjs')"),
        "rust-model-state-reads",
    )?;
    if !tree.join(CONTRACT).is_file() {
        return Ok(());
    }
    const PATHS: [&str; 2] = ["/api/models/inference-budget", "/api/models/installed"];
    let rows = generation_rows(tree, &PATHS, rs, GET1).map_err(|()| {
        die(
            LABEL,
            "exactly GET of both routes must be Rust's with the new pair, and Node's with the old",
        )
    })?;
    if !rs {
        return Ok(());
    }
    // `printf '%s\n' "$rows" | sort -u | wc -l` = 2: both paths, whatever the repeats.
    if !PATHS
        .iter()
        .all(|p| rows.iter().any(|r| r.as_slice() == p.as_bytes()))
    {
        return Err(die(LABEL, "incomplete route ownership"));
    }
    if !grep_fixed(
        &tree.join("core/server/rust-model-state-reads.cjs"),
        "OWNED_PATHS = Object.freeze(['/api/models/inference-budget', '/api/models/installed'])",
    ) {
        return Err(die(
            LABEL,
            "Core must refuse exactly the two reads the front answers",
        ));
    }
    if raw_has(
        tree,
        "core/server/routes/models.cjs",
        r"inferenceEstimates|await modelsInstalled\(\)\)",
    ) {
        return Err(die(LABEL, "Core still carries the read handlers"));
    }
    if !tree.join("core/server/rust-model-facts.cjs").is_file() {
        return Err(die(LABEL, "Core does not offer the front its model facts"));
    }
    let index = code_lines(read_file(&tree.join(INDEX)).as_deref());
    if !text_has(&index, r"modelFactsBridge\(req, res\)") {
        return Err(die(LABEL, "Core does not mount the model facts bridge"));
    }
    if !secret_handed(tree, r"createModelFactsBridge\(\{") {
        return Err(die(
            LABEL,
            "Core does not hand its model facts bridge the front's captured secret",
        ));
    }
    Ok(())
}

/// `grep -qE ERE FILE 2>/dev/null` over the raw file (comments count); a missing file is false.
fn raw_has(tree: &Path, rel: &str, ere: &str) -> bool {
    match read_file(&tree.join(rel)) {
        Some(src) => text_has(&src, ere),
        None => false,
    }
}

pub const MODEL_MAINTENANCE: RoutePair<'static> = RoutePair {
    label: "model maintenance ownership pairing",
    core: Core::Fixed("require('./rust-model-maintenance.cjs')"),
    feature: "rust-model-maintenance",
    select: Select::Paths(&[
        ("/api/models/presets/reload", POST1),
        ("/api/models/evidence", GET1),
        ("/api/models/evidence/import", POST1),
    ]),
    rule_text: "exactly the one method of each route must belong to Rust",
    expected: &[
        "/api/models/presets/reload",
        "/api/models/evidence",
        "/api/models/evidence/import",
    ],
};

const RECHECK: &str = "/api/models/evidence/recheck";

/// The model maintenance routes (#2210), then the evidence recheck (#2286), which needs them.
pub fn model_evidence_pairings(tree: &Path) -> Result<(), String> {
    let m = &MODEL_MAINTENANCE;
    let maint = route_pair(tree, m)?;
    let recheck_front = front_has(tree, "rust-model-evidence-recheck");
    if maint && !recheck_front {
        // `routes_toml … | awk '$1 == RECHECK && $2 == "rust" { f = 1 } END { exit !f }'` under pipefail: a
        // refused table fails the pipeline (no refusal); it was read already for the routes above.
        let rt = routes_toml_file(&tree.join(CONTRACT));
        if rt.error.is_none()
            && rt.routes.iter().any(|r| {
                let [path, owner, _, _] = tab_fields(r);
                path == RECHECK.as_bytes() && owner == b"rust"
            })
        {
            return Err(die(m.label, "the recheck stays Node's"));
        }
    }

    const LABEL: &str = "evidence recheck ownership pairing";
    let rs = owner(
        tree,
        LABEL,
        Core::Fixed("require('./rust-model-evidence-recheck.cjs')"),
        "rust-model-evidence-recheck",
    )?;
    if !tree.join(CONTRACT).is_file() {
        return Ok(());
    }
    let rows = generation_rows(tree, &[RECHECK], rs, POST1).map_err(|()| {
        die(
            LABEL,
            "POST alone must be native with the new pair and Node's with the old",
        )
    })?;
    if !rs {
        return Ok(());
    }
    if !maint {
        return Err(die(LABEL, "native evidence store required"));
    }
    // `printf '%s\n' "$rows" | wc -w` = 1: one row (the path has no blank).
    if rows.len() != 1 {
        return Err(die(LABEL, "missing route"));
    }
    let refused = project_routes_core(
        read_file(&tree.join("core/server/rust-model-evidence-recheck.cjs")).as_deref(),
        "OWNED_ROUTES",
    );
    if refused != [b"POST /api/models/evidence/recheck".to_vec()] {
        return Err(die(LABEL, "exact POST refusal required"));
    }
    if raw_has(
        tree,
        "core/server/routes/models.cjs",
        "createVisionProbe|source:[[:space:]]*.recheck.",
    ) {
        return Err(die(LABEL, "public Node probe still present"));
    }
    if !tree
        .join("core/server/inference-admission-lease.cjs")
        .is_file()
    {
        return Err(die(LABEL, "live inference admission required"));
    }
    let index = code_lines(read_file(&tree.join(INDEX)).as_deref());
    if !text_has(&index, r"inferenceAdmission\.handle\(req, res\)") {
        return Err(die(LABEL, "inference admission must be mounted"));
    }
    if !secret_handed(tree, r"createAdmissionRoutes\(\{") {
        return Err(die(
            LABEL,
            "captured inference admission secret required",
        ));
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------------------------
// By name

/// The CLI names of this module's pairings, in the script's order (`project-storage` first: line 1101).
pub const NAMES: &[&str] = &[
    "project-storage",
    "tool-approvals",
    "framing-suggest",
    "toolbox-picker",
    "close-out",
    "decision-settings",
    "storage-integrations",
    "vault-mirror",
    "storage-connection",
    "providers",
    "chatgpt-signin",
    "mcp-credentials",
    "mcp-views",
    "provider-models",
    "google-drive",
    "offsite-backup",
    "model-sizing",
    "model-state-reads",
    "model-evidence",
];

/// The verdicts of earlier pairings some of these take: the project create/delete pair's front half, the
/// auto-roles write pair (`auto_roles_write_owner = rust`) and the MCP discovery pair's front half.
#[derive(Clone, Copy, Debug, Default)]
pub struct Earlier {
    pub lifecycle_rs_owner: bool,
    pub auto_roles_write: bool,
    pub discovery_rs_owner: bool,
}

/// Run one pairing by its CLI name; `None` for a name that is not one of [`NAMES`].
pub fn by_name(tree: &Path, name: &str, earlier: Earlier) -> Option<Result<(), String>> {
    Some(match name {
        "project-storage" => project_storage_pairing(tree, earlier.lifecycle_rs_owner).map(|_| ()),
        "tool-approvals" => tool_approval_pairings(tree),
        "framing-suggest" => framing_suggest_pairing(tree),
        "toolbox-picker" => toolbox_picker_pairing(tree, earlier.discovery_rs_owner),
        "close-out" => close_out_pairings(tree, earlier.auto_roles_write),
        "decision-settings" => route_pair(tree, &DECISION_SETTINGS).map(|_| ()),
        "storage-integrations" => route_pair(tree, &STORAGE_INTEGRATIONS).map(|_| ()),
        "vault-mirror" => route_pair(tree, &VAULT_MIRROR).map(|_| ()),
        "storage-connection" => route_pair(tree, &STORAGE_CONNECTION).map(|_| ()),
        "providers" => provider_pairings(tree),
        "chatgpt-signin" => route_pair(tree, &CHATGPT_SIGNIN).map(|_| ()),
        "mcp-credentials" => mcp_credentials_pairing(tree),
        "mcp-views" => mcp_views_pairings(tree, earlier.discovery_rs_owner),
        "provider-models" => route_pair(tree, &PROVIDER_MODELS).map(|_| ()),
        "google-drive" => route_pair(tree, &GOOGLE_DRIVE).map(|_| ()),
        "offsite-backup" => offsite_backup_pairing(tree),
        "model-sizing" => route_pair(tree, &MODEL_SIZING).map(|_| ()),
        "model-state-reads" => model_state_reads_pairing(tree),
        "model-evidence" => model_evidence_pairings(tree),
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn want_rules_follow_the_awk() {
        assert!(Want::All.allow(b"-"));
        assert!(!Want::All.allow(b"GET"));
        let gp = Want::Exact(&["GET", "PUT"]);
        assert!(gp.allow(b"GET,PUT"));
        assert!(gp.allow(b"[\"PUT\", \"GET\"]"));
        assert!(!gp.allow(b"GET,GET,PUT"));
        assert!(!gp.allow(b"GET"));
        assert!(!gp.allow(b"GET,PUT,POST"));
        assert!(!gp.allow(b"-"));
        assert!(GET1.allow(b"GET"));
        assert!(!GET1.allow(b"GET,GET"));
        assert!(!GET1.allow(b"Get"));
        assert!(!GET1.allow(b"-"));
        // The probe with the registry: repeats count, as in the awk.
        let probe = Want::Counted(&["POST", "PUT", "DELETE"], 3);
        assert!(probe.allow(b"POST,PUT,DELETE"));
        assert!(probe.allow(b"POST,POST,PUT"));
        assert!(!probe.allow(b"POST,PUT"));
        assert!(!probe.allow(b"POST,PUT,GET"));
    }

    #[test]
    fn under_selects_the_base_and_below_only() {
        let s = Select::Under("/api/admin/offsite-backup", Want::All);
        assert!(s.rule_for(b"/api/admin/offsite-backup").is_some());
        assert!(s.rule_for(b"/api/admin/offsite-backup/run").is_some());
        assert!(s.rule_for(b"/api/admin/offsite-backup/").is_some());
        assert!(s.rule_for(b"/api/admin/offsite-backups").is_none());
        assert!(s.rule_for(b"/api/admin/offsite").is_none());
        assert!(s.rule_for(b"x/api/admin/offsite-backup").is_none());
    }
}
