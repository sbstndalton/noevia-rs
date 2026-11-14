//! The pre-launch checks: the selected front's `--features` against the installed Core's files and
//! exported markers. Every ownership moves as one pair; old/old (rollback) and new/new start, a mix
//! is refused before either process runs. Order and messages follow build/web-supervisor.sh, so the
//! first refusal a mixed image meets is the same one.

use crate::env::Env;

/// The permanent front read routes (#1071): every one, or nothing starts.
pub const FRONT_ROUTE_FEATURES: &[&str] = &[
    "rust-chatgpt-status",
    "rust-storage-reads",
    "rust-storage-browse",
    "rust-export",
    "rust-free-chats",
    "rust-usage",
    "rust-stats",
    "rust-routing-default",
    "rust-sampling-settings",
    "rust-reasoning-settings",
    "rust-routing-mode",
    "rust-projects",
    "rust-project-chats",
    "rust-project-writes",
    "project-store",
];

/// What the front reported with `--features`, one capability per line.
#[derive(Debug, Clone, Default)]
pub struct Features {
    lines: Vec<String>,
}

impl Features {
    pub fn parse(stdout: &str) -> Self {
        Self { lines: stdout.split('\n').map(str::to_owned).collect() }
    }

    /// `grep -qx`: one whole line equals the capability.
    pub fn has(&self, capability: &str) -> bool {
        self.lines.iter().any(|l| l == capability)
    }

    pub fn lines(&self) -> impl Iterator<Item = &str> {
        self.lines.iter().map(String::as_str)
    }
}

/// The result of one `node -e` probe: whether it exited 0, and what it printed.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NodeOut {
    pub ok: bool,
    pub stdout: String,
}

/// The installed Core as the checks see it (paths relative to the Core directory, the working
/// directory the script ran in). The real one reads files and runs `node`; tests use a fake.
pub trait CoreView {
    /// The file's text, or None when it is missing or unreadable (`grep … 2>/dev/null` fails).
    fn read(&self, rel: &str) -> Option<String>;
    /// `[ -e ]`.
    fn exists(&self, rel: &str) -> bool;
    /// `[ -f ]`.
    fn is_file(&self, rel: &str) -> bool;
    /// `node -e <script> <args…>` in the Core directory, stderr dropped, bounded in time.
    fn node(&self, script: &str, args: &[String]) -> NodeOut;
    /// `command -v node`.
    fn has_node(&self) -> bool;
    /// The absolute path of a Core file (for probes that take it as an argument).
    fn abs(&self, rel: &str) -> String;
}

fn contains(core: &dyn CoreView, rel: &str, needle: &str) -> bool {
    core.read(rel).is_some_and(|text| text.contains(needle))
}

fn requires(core: &dyn CoreView, rel: &str, module: &str) -> bool {
    contains(core, rel, &format!("require('./{module}')"))
}

/// `grep -vE '^[[:space:]]*(//|/\*|\*)'` then `grep -qF "require('./<module>')"`: the require must sit
/// on a line that does not start as a comment.
fn code_requires(core: &dyn CoreView, rel: &str, module: &str) -> bool {
    let Some(text) = core.read(rel) else { return false };
    let needle = format!("require('./{module}')");
    text.split('\n').any(|line| {
        let t = line.trim_start_matches([' ', '\t', '\x0b', '\x0c', '\r']);
        !(t.starts_with("//") || t.starts_with("/*") || t.starts_with('*')) && line.contains(&needle)
    })
}

/// A Core `require` of one module in `server/index.cjs` beside one front capability.
struct RequirePair {
    module: &'static str,
    capability: &'static str,
    label: &'static str,
}

/// The groups the script checks one by one before its PAIRS table, in its order, with its wording
/// ("<label> mismatch between noevia-server and noevia-core").
const EARLY_PAIRS: &[RequirePair] = &[
    RequirePair { module: "rust-diary.cjs", capability: "rust-diary", label: "Diary route ownership" },
    RequirePair { module: "rust-models.cjs", capability: "rust-model-reads", label: "model read route ownership" },
    RequirePair {
        module: "rust-project-reads.cjs",
        capability: "rust-project-reads",
        label: "project source read ownership",
    },
    RequirePair {
        module: "rust-project-lifecycle.cjs",
        capability: "rust-project-lifecycle",
        label: "project create/delete ownership",
    },
];

const MID_PAIRS: &[RequirePair] = &[
    RequirePair {
        module: "rust-project-files.cjs",
        capability: "rust-project-files",
        label: "project file delete ownership",
    },
    RequirePair {
        module: "rust-project-document-pages.cjs",
        capability: "rust-project-document-pages",
        label: "project document page reader ownership",
    },
    RequirePair {
        module: "rust-conversation-import.cjs",
        capability: "rust-conversation-import",
        label: "conversation import ownership",
    },
    RequirePair {
        module: "rust-project-documents.cjs",
        capability: "rust-project-documents",
        label: "project document upload ownership",
    },
];

/// The script's PAIRS heredoc: "<label> route ownership mismatch …".
pub const ROUTE_PAIRS: &[(&str, &str, &str)] = &[
    ("rust-public-pages.cjs", "rust-public-pages", "public page"),
    ("rust-plugin-directory.cjs", "rust-plugin-directory", "plugin directory"),
    ("rust-decision-settings.cjs", "rust-decision-settings", "decision settings"),
    ("rust-storage-integrations.cjs", "rust-storage-integrations", "storage integration"),
    ("rust-vault-mirror-preferences.cjs", "rust-vault-mirror-preferences", "vault mirror preference"),
    ("rust-storage-connection.cjs", "rust-storage-connection", "storage connection"),
    ("rust-provider-test.cjs", "rust-provider-test", "provider test"),
    ("rust-chatgpt-oauth.cjs", "rust-chatgpt-oauth", "ChatGPT sign-in"),
    ("rust-provider-models.cjs", "rust-provider-models", "provider model list"),
    ("rust-providers.cjs", "rust-providers", "provider registry"),
    ("rust-providers-reseal.cjs", "rust-providers-reseal", "provider key reseal"),
    ("rust-google-drive.cjs", "rust-google-drive", "Google Drive"),
    ("rust-offsite-backup.cjs", "rust-offsite-backup", "off-site backup"),
    ("rust-mcp-credentials.cjs", "rust-mcp-credentials", "MCP credential"),
    ("rust-health.cjs", "rust-health", "health check"),
    ("rust-auto-roles.cjs", "rust-auto-roles", "auto-roles read"),
    ("rust-workspace.cjs", "rust-workspace", "workspace list"),
    ("rust-chat-search.cjs", "rust-chat-search", "chat search"),
    ("rust-chat-export.cjs", "rust-chat-export", "chat download"),
    ("rust-diary-routes.cjs", "rust-diary-routes", "Diary connector and recovery"),
    ("rust-diary-local-exchange.cjs", "rust-diary-local-exchange", "Diary local exchange"),
    ("rust-mcp-directory.cjs", "rust-mcp-directory", "MCP directory"),
    ("rust-toolbox-picker.cjs", "rust-toolbox-picker", "toolbox picker"),
    ("rust-toolboxes-permitted.cjs", "rust-toolboxes-permitted", "permitted tools"),
    ("rust-connectors.cjs", "rust-connectors", "connectors"),
    ("rust-project-config.cjs", "rust-project-config", "project configuration"),
    ("rust-admin-users.cjs", "rust-admin-users", "account deletion"),
    ("rust-research.cjs", "rust-research", "deep research"),
    ("rust-task-reads.cjs", "rust-task-reads", "task read"),
    ("rust-autotune-read.cjs", "rust-autotune-read", "auto-tune status GET"),
    ("rust-idle-unload-read.cjs", "rust-idle-unload-read", "idle unload read"),
    ("rust-model-assignments.cjs", "rust-model-assignments", "model assignments read"),
    ("rust-model-downloads-read.cjs", "rust-model-downloads-read", "model download GET"),
    ("rust-preset-read.cjs", "rust-preset-read", "preset GET"),
    ("rust-autotune-resume.cjs", "rust-autotune-resume", "auto-tune resume"),
    ("rust-autotune-cancel.cjs", "rust-autotune-cancel", "auto-tune cancellation"),
    ("rust-autotune-clear.cjs", "rust-autotune-clear", "autotune dismissal"),
    ("rust-model-manager-scan.cjs", "rust-model-manager-scan", "model folder scan read"),
    ("rust-calibration-cancel.cjs", "rust-calibration-cancel", "calibration cancellation"),
    ("rust-autotune-untuned.cjs", "rust-autotune-untuned", "untuned discovery"),
    ("rust-calibration-routes.cjs", "rust-calibration-routes", "calibration start/read"),
    ("rust-autotune-settings.cjs", "rust-autotune-settings", "auto-tune settings"),
    ("rust-tool-approvals.cjs", "rust-tool-approvals", "tool approval decision"),
    ("rust-model-evidence-recheck.cjs", "rust-model-evidence-recheck", "model evidence recheck"),
    ("rust-model-state-reads.cjs", "rust-model-state-reads", "model state read"),
    ("rust-chat-framing-suggest.cjs", "rust-chat-framing-suggest", "framing suggestion"),
];

const INDEX: &str = "server/index.cjs";

// The Core probes, verbatim from the script (each runs in the Core directory).
const MACHINES_PROBE: &str = "try { const m = require('./server/rust-inference-machines.cjs'); if (m.MACHINES_CLIENT_MARKER === 'noevia-inference-machines-client/1') process.stdout.write('rust'); } catch {}";
const AUTO_ROLES_PROBE: &str = r#"
    const fs = require("node:fs"), vm = require("node:vm");
    const src = fs.readFileSync(process.argv[1], "utf8");
    let value;
    try {
      // Invocation and export access both run inside the timed evaluation.
      value = vm.runInNewContext(
        "(function () {" +
        "  const stub = new Proxy(function () {}, { get: () => stub, apply: () => stub, construct: () => stub });" +
        "  const module = { exports: {} };" +
        "  (function (require, module, exports) {" + src + "\n})(() => stub, module, module.exports);" +
        "  const v = module.exports.RUST_AUTO_ROLES_WRITE_OWNED;" +
        "  return v === undefined ? undefined : typeof v === \"string\" ? v : null;" +
        "})()", {}, { timeout: 2000 });
    } catch { process.stdout.write("invalid"); process.exit(0); }
    if (value === undefined) { process.stdout.write("invalid"); process.exit(0); }
    const markers = src.match(/["\x27]rust-auto-roles-write[^"\x27\r\n]*["\x27]/g) || [];
    process.stdout.write(value === "rust-auto-roles-write" && markers.length === 1 ? "rust" : "invalid");
  "#;
const EMBEDDING_PROBE: &str = "try { const m = require('./server/rust-embedding-background.cjs'); if (m.RUST_EMBEDDING_OWNED === 'rust-embedding-background-v1') process.stdout.write('rust'); } catch {}";
const CHAT_MIRROR_PROBE: &str = "try { const m = require('./server/rust-chat-mirror.cjs'); if (m.RUST_CHAT_MIRROR_OWNED === 'rust-chat-mirror-v1') process.stdout.write('rust'); } catch {}";
const SOURCE_STARTUP_PROBE: &str = r#"
    const fs=require("node:fs"),vm=require("node:vm");
    try {
      const src=fs.readFileSync(process.argv[1],"utf8");
      const value=vm.runInNewContext("const module={exports:{}};"+src+"\nmodule.exports.RUST_SOURCE_STARTUP_OWNED",{}, {timeout:2000});
      process.stdout.write(value === "rust-source-startup-v1" ? "v1" : "invalid");
    } catch { process.stdout.write("invalid"); }
  "#;
const DAV_LISTENER_PROBE: &str = r#"
    const fs=require("node:fs"),vm=require("node:vm");
    try {
      const src=fs.readFileSync(process.argv[1],"utf8");
      const value=vm.runInNewContext("const module={exports:{}};"+src+"\nmodule.exports.RUST_DAV_LISTENER_OWNED",{}, {timeout:2000});
      process.stdout.write(value === "rust-dav-listener-v1" ? "v1" : "invalid");
    } catch { process.stdout.write("invalid"); }
  "#;
const WEB_READ_PROBE: &str = "try { const m = require('./server/rust-web-address.cjs'); if (m.WEB_ADDRESS_READ_OWNER === 'noevia-web-address-read-rust/1') process.stdout.write('rust'); } catch {}";
const WEB_WRITE_PROBE: &str = "try { const m = require('./server/rust-web-address.cjs'); if (m.WEB_ADDRESS_WRITE_OWNER === 'noevia-web-address-write-rust/1') process.stdout.write('rust'); } catch {}";
const DIARY_BACKUP_PROBE: &str = "try { const m = require('./server/rust-diary.cjs'); if (m.DIARY_BACKUP_WORKER_OWNER === 'noevia-diary-backup-worker-rust/1') process.stdout.write('rust'); } catch {}";
const BUDGET_WATCH_PROBE: &str = "try { const m = require('./server/rust-budget-watch.cjs'); if (m.BUDGET_WATCH_OWNER === 'noevia-budget-watch-rust/1') process.stdout.write('v1'); else if (m.BUDGET_WATCH_OWNER === 'noevia-budget-watch-rust/2') process.stdout.write('v2'); else if (m.BUDGET_WATCH_OWNER === 'noevia-budget-watch-rust/3') process.stdout.write('v3'); } catch {}";
const CALIBRATION_PROBE: &str = "try { const m = require('./server/rust-calibration-routes.cjs'); if (m.CALIBRATION_WORKER_OWNER === 'noevia-calibration-worker-rust/1') process.stdout.write('v1'); else if (m.CALIBRATION_WORKER_OWNER === 'noevia-calibration-worker-rust/2') process.stdout.write('v2'); } catch {}";
const SOURCE_CLIENT_PROBE: &str =
    "const c=require(process.argv[1]); if(c.SOURCE_JOBS_CLIENT_MARKER) process.stdout.write(c.SOURCE_JOBS_CLIENT_MARKER)";

/// `$(node -e … 2>/dev/null || true)`: whatever it printed, success or not.
fn probe(core: &dyn CoreView, script: &str) -> String {
    core.node(script, &[]).stdout
}

/// A probe guarded by `|| x=invalid`: a failed or timed-out run reads as invalid.
fn probe_file(core: &dyn CoreView, script: &str, rel: &str) -> String {
    let out = core.node(script, &[core.abs(rel)]);
    if out.ok {
        out.stdout
    } else {
        "invalid".to_owned()
    }
}

/// What the checks hand to the launch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Paired {
    /// The front strips client copies of X-Noevia-Client-Ip, so Node may believe it.
    pub client_ip_confirmed: bool,
    /// The front's absolute path when it advertises legacy-root-claim.
    pub legacy_claim_bin: Option<String>,
    /// Core's DAV listener generation ("none", "v1").
    pub dav_listener_core: String,
}

/// A refusal before either process starts: printed to stderr, exit 2.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refusal(pub String);

fn refuse<T>(msg: impl Into<String>) -> Result<T, Refusal> {
    Err(Refusal(format!("web-supervisor: {}", msg.into())))
}

fn mismatch(label: &str) -> String {
    format!("{label} mismatch between noevia-server and noevia-core; update the paired image")
}

fn pair(core_rust: bool, front_rust: bool, label: &str) -> Result<(), Refusal> {
    if core_rust != front_rust {
        return refuse(mismatch(label));
    }
    Ok(())
}

/// Every check after the front binary was found, in the script's order. `warn` gets the
/// non-fatal notes as they happen.
pub fn check(
    env: &Env,
    bin: &str,
    features: &Features,
    core: &dyn CoreView,
    warn: &mut dyn FnMut(String),
) -> Result<Paired, Refusal> {
    // #1246: with the code network configured only the front can refuse its requests.
    if !env.get("COWORK_CODE_NET_ADDR").is_empty() && !features.has("code-net-guard") {
        return refuse("COWORK_CODE_NET_ADDR is set but this noevia-server has no code-net-guard; update the image");
    }
    // #1578: Node believes the client address header only behind a front that strips client copies.
    let client_ip_confirmed = features.has("client-address-header");
    if !client_ip_confirmed {
        warn("web-supervisor: this noevia-server has no client-address-header; Node ignores X-Noevia-Client-Ip and sees every client as the front".to_owned());
    }
    if !features.has("rust-auth") {
        return refuse("this noevia-server has no rust-auth; Node no longer answers sign-in (sbstndalton/noevia#1071), so update the image");
    }
    let missing: Vec<&str> = FRONT_ROUTE_FEATURES.iter().copied().filter(|f| !features.has(f)).collect();
    if !missing.is_empty() {
        let list: String = missing.iter().map(|f| format!(" {f}")).collect();
        return refuse(format!(
            "this noevia-server lacks the front route feature(s):{list}; Node no longer answers those routes (sbstndalton/noevia#1071), so update the image"
        ));
    }
    pair(contains(core, INDEX, ".createReadOnlyFeatures("), features.has("rust-feature-owner"), "feature ownership")?;
    for p in EARLY_PAIRS {
        pair(requires(core, INDEX, p.module), features.has(p.capability), p.label)?;
    }
    // Round 3: either index.cjs requires the storage split, or it loads the outbox view that does.
    let storage_core = code_requires(core, INDEX, "rust-project-storage.cjs")
        || (code_requires(core, INDEX, "rust-index-outbox.cjs")
            && code_requires(core, "server/rust-index-outbox.cjs", "rust-project-storage.cjs"));
    pair(storage_core, features.has("rust-project-storage"), "project storage write ownership")?;
    for p in MID_PAIRS {
        pair(requires(core, INDEX, p.module), features.has(p.capability), p.label)?;
    }
    for (module, capability, label) in ROUTE_PAIRS {
        pair(requires(core, INDEX, module), features.has(capability), &format!("{label} route ownership"))?;
    }
    // #1638: Core's native-owner clients export their marker (loaded from routes, not index.cjs).
    pair(
        probe(core, MACHINES_PROBE) == "rust",
        features.has("rust-inference-machines"),
        "inference machines route ownership",
    )?;
    // F1: the exact export beside exactly one compiled capability line.
    let auto_core = if !core.is_file("server/rust-auto-roles-write.cjs") {
        "none".to_owned()
    } else if !core.has_node() {
        "invalid".to_owned()
    } else {
        probe_file(core, AUTO_ROLES_PROBE, "server/rust-auto-roles-write.cjs")
    };
    let mut auto_front = "none";
    for line in features.lines().filter(|l| l.contains("rust-auto-roles-write")) {
        auto_front = if line == "rust-auto-roles-write" && auto_front == "none" { "rust" } else { "invalid" };
    }
    if auto_core == "invalid" || auto_front == "invalid" || auto_core != auto_front {
        return refuse("auto-roles write route ownership mismatch or ambiguous capability marker; update the paired image");
    }
    pair(requires(core, INDEX, "rust-model-sizing.cjs"), features.has("rust-model-sizing"), "model sizing route ownership")?;
    pair(
        requires(core, INDEX, "rust-model-maintenance.cjs"),
        features.has("rust-model-maintenance"),
        "model maintenance route ownership",
    )?;
    // Embedding background: a new front beside an old Core stays idle; a new Core needs the front.
    let embedding_core = probe(core, EMBEDDING_PROBE) == "rust";
    if requires(core, INDEX, "rust-embedding-background.cjs") && !embedding_core {
        return refuse("invalid embedding background ownership marker");
    }
    if embedding_core && !features.has("rust-embedding-background-v1") {
        return refuse("embedding background requires rust-embedding-background-v1; update the paired image");
    }
    let chat_mirror_core = probe(core, CHAT_MIRROR_PROBE) == "rust";
    if requires(core, INDEX, "rust-chat-mirror.cjs") && !chat_mirror_core {
        return refuse("invalid chat mirror ownership marker");
    }
    if chat_mirror_core != features.has("rust-chat-mirror-v1") {
        return refuse("chat mirror schedule ownership mismatch between noevia-server and noevia-core (rust-chat-mirror-v1); update the paired image");
    }
    let generation = |rel: &str, script: &str| {
        if core.is_file(rel) {
            probe_file(core, script, rel)
        } else {
            "none".to_owned()
        }
    };
    let source_startup_core = generation("server/rust-source-startup.cjs", SOURCE_STARTUP_PROBE);
    let source_startup_front = if features.has("rust-source-startup-v1") { "v1" } else { "none" };
    if source_startup_core == "invalid" || source_startup_core != source_startup_front {
        return refuse("source startup ownership mismatch; update the paired image");
    }
    let dav_listener_core = generation("server/rust-dav-listener.cjs", DAV_LISTENER_PROBE);
    let dav_listener_front = if features.has("rust-dav-listener-v1") { "v1" } else { "none" };
    if dav_listener_core == "invalid" || dav_listener_core != dav_listener_front {
        return refuse("DAV listener ownership mismatch; update the paired image");
    }
    // One direction only: an older Core beside a baseline front is shared mode and starts.
    if contains(core, "server/preset-reload.cjs", "'noevia-preset-baseline-bridge/1'")
        && !features.has("rust-preset-baseline")
    {
        return refuse("this noevia-core asks the front for the preset reload baseline, but this noevia-server has no rust-preset-baseline; update the paired image");
    }
    pair(probe(core, WEB_READ_PROBE) == "rust", features.has("rust-web-address-read"), "web-address GET ownership")?;
    pair(probe(core, WEB_WRITE_PROBE) == "rust", features.has("rust-web-address-write"), "web-address POST ownership")?;
    pair(
        probe(core, DIARY_BACKUP_PROBE) == "rust",
        features.has("rust-diary-backup-worker"),
        "diary backup worker ownership",
    )?;
    let mut budget_front = "";
    for (cap, generation) in [("rust-budget-watch", "v1"), ("rust-budget-watch-v2", "v2"), ("rust-budget-watch-v3", "v3")] {
        if features.has(cap) {
            budget_front = generation;
        }
    }
    if budget_front != probe(core, BUDGET_WATCH_PROBE) {
        return refuse(mismatch("budget watch ownership"));
    }
    let mut calibration_front = "";
    for (cap, generation) in [("rust-calibration-worker", "v1"), ("rust-calibration-worker-v2", "v2")] {
        if features.has(cap) {
            calibration_front = generation;
        }
    }
    if calibration_front != probe(core, CALIBRATION_PROBE) {
        return refuse(mismatch("calibration worker ownership"));
    }
    check_source_jobs(features, core)?;
    let source_work_core = requires(core, INDEX, "rust-project-source-work.cjs");
    pair(source_work_core, features.has("rust-project-source-work"), "project source operations ownership")?;
    if source_work_core {
        for worker_file in ["source-work.cjs", "folder-import.cjs", "pdf-reduce.cjs"] {
            if core.exists(&format!("server/{worker_file}")) {
                return refuse(format!("project source operations ownership mismatch between noevia-server and noevia-core (Core still carries server/{worker_file}); update the paired image"));
            }
        }
        let answers_worker = ["server/routes/projects.cjs", INDEX]
            .iter()
            .any(|rel| core.read(rel).is_some_and(|t| t.to_ascii_lowercase().contains("x-noevia-source-work")));
        if answers_worker {
            return refuse("project source operations ownership mismatch between noevia-server and noevia-core (Core still answers the worker call); update the paired image");
        }
    }
    pair(
        requires(core, "server/large-document-lock.cjs", "large-document-file-lock.cjs"),
        features.has("rust-large-document-host-lock"),
        "large-document lock",
    )?;
    // #2027: only the selected front, by absolute path, never an inherited one.
    let legacy_claim_bin = if features.has("legacy-root-claim") {
        if !bin.starts_with('/') {
            return refuse("legacy-root-claim requires an absolute NOEVIA_SERVER_BIN");
        }
        Some(bin.to_owned())
    } else {
        None
    };
    Ok(Paired { client_ip_confirmed, legacy_claim_bin, dav_listener_core })
}

/// Source job journals (#2191 round 6): Node's client export, the front's store, then the route owner.
fn check_source_jobs(features: &Features, core: &dyn CoreView) -> Result<(), Refusal> {
    const CLIENT: &str = "server/project-store-client.cjs";
    const CANONICAL: &str = "noevia-source-jobs-client/1";
    let marker = core.node(SOURCE_CLIENT_PROBE, &[core.abs(CLIENT)]).stdout;
    let declares = core
        .read(CLIENT)
        .is_some_and(|t| t.contains("SOURCE_JOBS_CLIENT_MARKER") || t.contains("sourceJobsFor("))
        || contains(core, "server/source-jobs.cjs", "workspace.sourceJobs");
    if declares && marker != CANONICAL {
        return refuse("core declares a source job store client but does not export its canonical capability");
    }
    if !marker.is_empty() && marker != CANONICAL {
        return refuse("unknown source job client capability");
    }
    let store = features.has("noevia-source-jobs-store/1");
    if !marker.is_empty() && !store {
        return refuse("core source job client requires noevia-source-jobs-store/1 from this noevia-server");
    }
    if marker.is_empty() && store {
        return refuse("noevia-server source job store requires a paired core source job client");
    }
    pair(requires(core, INDEX, "rust-source-jobs.cjs"), features.has("rust-source-jobs"), "source job ownership")
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    /// A Core made of in-memory files; each node probe answers by the module it names.
    #[derive(Default)]
    struct Fake {
        files: HashMap<String, String>,
        probes: HashMap<&'static str, NodeOut>,
    }

    impl Fake {
        fn file(mut self, rel: &str, text: &str) -> Self {
            self.files.insert(rel.to_owned(), text.to_owned());
            self
        }
        fn probe(mut self, module: &'static str, out: &str) -> Self {
            self.probes.insert(module, NodeOut { ok: true, stdout: out.to_owned() });
            self
        }
    }

    impl CoreView for Fake {
        fn read(&self, rel: &str) -> Option<String> {
            self.files.get(rel).cloned()
        }
        fn exists(&self, rel: &str) -> bool {
            self.files.contains_key(rel)
        }
        fn is_file(&self, rel: &str) -> bool {
            self.files.contains_key(rel)
        }
        fn node(&self, script: &str, args: &[String]) -> NodeOut {
            let haystack = format!("{script} {}", args.join(" "));
            self.probes.iter().find(|(m, _)| haystack.contains(*m)).map(|(_, o)| o.clone()).unwrap_or_default()
        }
        fn has_node(&self) -> bool {
            true
        }
        fn abs(&self, rel: &str) -> String {
            format!("/core/{rel}")
        }
    }

    const BASE: &str = "code-net-guard rust-auth client-address-header rust-chatgpt-status rust-storage-reads rust-storage-browse rust-export rust-free-chats rust-usage rust-stats rust-routing-default rust-sampling-settings rust-reasoning-settings rust-routing-mode rust-projects rust-project-chats rust-project-writes project-store";

    fn features(extra: &str) -> Features {
        Features::parse(&format!("{BASE} {extra}").split_whitespace().collect::<Vec<_>>().join("\n"))
    }

    fn run(core: &Fake, f: &Features, env: &Env) -> Result<Paired, Refusal> {
        check(env, "/usr/local/bin/noevia-server", f, core, &mut |_| {})
    }

    fn refusal(core: &Fake, f: &Features) -> String {
        run(core, f, &Env::default()).err().map(|r| r.0).unwrap_or_default()
    }

    #[test]
    fn an_old_core_beside_a_base_front_starts() {
        let p = run(&Fake::default(), &features(""), &Env::default()).unwrap();
        assert!(p.client_ip_confirmed);
        assert_eq!(p.legacy_claim_bin, None);
        assert_eq!(p.dav_listener_core, "none");
    }

    #[test]
    fn features_are_whole_lines() {
        let f = Features::parse("rust-auth-extra\nrust-auth \n");
        assert!(!f.has("rust-auth"));
        assert!(Features::parse("a\nrust-auth\n").has("rust-auth"));
    }

    #[test]
    fn every_missing_route_feature_is_named_in_order() {
        let f = Features::parse("code-net-guard\nrust-auth\nrust-usage");
        let err = refusal(&Fake::default(), &f);
        assert!(err.contains("lacks the front route feature(s): rust-chatgpt-status rust-storage-reads"), "{err}");
        assert!(!err.contains(" rust-usage "), "{err}");
        assert!(err.ends_with("so update the image"));
    }

    #[test]
    fn code_net_guard_is_required_only_with_the_code_network() {
        let f = Features::parse(&BASE.replace("code-net-guard ", "").replace(' ', "\n"));
        assert!(run(&Fake::default(), &f, &Env::default()).is_ok());
        let env = Env::from_pairs([("COWORK_CODE_NET_ADDR", "egress")]);
        let err = run(&Fake::default(), &f, &env).err().unwrap().0;
        assert!(err.contains("no code-net-guard"));
    }

    #[test]
    fn a_missing_client_address_header_only_warns() {
        let f = Features::parse(&BASE.replace("client-address-header ", "").replace(' ', "\n"));
        let mut notes = Vec::new();
        let p = check(&Env::default(), "/f", &f, &Fake::default(), &mut |w| notes.push(w)).unwrap();
        assert!(!p.client_ip_confirmed);
        assert_eq!(notes.len(), 1);
        assert!(notes[0].contains("no client-address-header"));
    }

    #[test]
    fn every_route_pair_refuses_both_mixes_and_accepts_the_match() {
        for (module, capability, label) in ROUTE_PAIRS {
            let core = Fake::default().file(INDEX, &format!("  if (require('./{module}').refuseOwned(req, res)) return;"));
            let want = format!("{label} route ownership mismatch between noevia-server and noevia-core");
            assert!(refusal(&core, &features("")).contains(&want), "{module}: core only");
            assert!(run(&core, &features(capability), &Env::default()).is_ok(), "{module}: pair");
            assert!(refusal(&Fake::default(), &features(capability)).contains(&want), "{module}: front only");
        }
        for p in EARLY_PAIRS.iter().chain(MID_PAIRS) {
            let core = Fake::default().file(INDEX, &format!("require('./{}')", p.module));
            assert!(refusal(&core, &features("")).contains(p.label), "{}", p.module);
            assert!(run(&core, &features(p.capability), &Env::default()).is_ok(), "{}", p.module);
            assert!(refusal(&Fake::default(), &features(p.capability)).contains(p.label), "{}", p.module);
        }
    }

    #[test]
    fn the_storage_split_counts_through_the_outbox_view_on_code_lines_only() {
        let chained = Fake::default()
            .file(INDEX, "const o = require('./rust-index-outbox.cjs');")
            .file("server/rust-index-outbox.cjs", "const s = require('./rust-project-storage.cjs');");
        assert!(run(&chained, &features("rust-project-storage"), &Env::default()).is_ok());
        let commented = Fake::default().file(INDEX, "  // require('./rust-project-storage.cjs')\n * require('./rust-project-storage.cjs')");
        assert!(run(&commented, &features(""), &Env::default()).is_ok());
        assert!(refusal(&commented, &features("rust-project-storage")).contains("project storage write ownership"));
        let broken = Fake::default().file(INDEX, "require('./rust-index-outbox.cjs')");
        assert!(refusal(&broken, &features("rust-project-storage")).contains("project storage write ownership"));
    }

    #[test]
    fn exported_markers_pair_with_their_capability() {
        let cases: &[(&str, &str, &str, &str)] = &[
            ("rust-inference-machines.cjs", "rust", "rust-inference-machines", "inference machines route ownership"),
            ("rust-web-address.cjs", "rust", "rust-web-address-read", "web-address GET ownership"),
            ("rust-diary.cjs", "rust", "rust-diary-backup-worker", "diary backup worker ownership"),
            ("rust-budget-watch.cjs", "v2", "rust-budget-watch-v2", "budget watch ownership"),
            ("rust-calibration-routes.cjs", "v2", "rust-calibration-worker-v2", "calibration worker ownership"),
        ];
        for (module, out, capability, label) in cases {
            let core = Fake::default().probe(module, out);
            assert!(run(&core, &features(capability), &Env::default()).is_ok(), "{module}");
            assert!(refusal(&core, &features("")).contains(label), "{module}");
            assert!(refusal(&Fake::default(), &features(capability)).contains(label), "{module}");
        }
        // The newest generation listed wins; a v1 Core beside a v1+v3 front is a mismatch.
        let core = Fake::default().probe("rust-budget-watch.cjs", "v1");
        assert!(refusal(&core, &features("rust-budget-watch rust-budget-watch-v3")).contains("budget watch"));
    }

    #[test]
    fn embedding_background_is_one_directional() {
        // A new front beside an old Core stays idle and starts.
        assert!(run(&Fake::default(), &features("rust-embedding-background-v1"), &Env::default()).is_ok());
        let core = Fake::default().probe("rust-embedding-background.cjs", "rust");
        assert!(refusal(&core, &features("")).contains("requires rust-embedding-background-v1"));
        let required = Fake::default().file(INDEX, "require('./rust-embedding-background.cjs')");
        assert!(refusal(&required, &features("rust-embedding-background-v1")).contains("invalid embedding background"));
    }

    #[test]
    fn auto_roles_write_needs_one_exact_capability_line() {
        let core = Fake::default().file("server/rust-auto-roles-write.cjs", "x").probe("rust-auto-roles-write.cjs", "rust");
        assert!(run(&core, &features("rust-auto-roles-write"), &Env::default()).is_ok());
        assert!(refusal(&core, &features("rust-auto-roles-write rust-auto-roles-write")).contains("ambiguous"));
        assert!(refusal(&core, &features("rust-auto-roles-write-v2")).contains("ambiguous"));
        assert!(refusal(&Fake::default(), &features("rust-auto-roles-write")).contains("ambiguous"));
        let invalid = Fake::default().file("server/rust-auto-roles-write.cjs", "x").probe("rust-auto-roles-write.cjs", "invalid");
        assert!(refusal(&invalid, &features("rust-auto-roles-write")).contains("ambiguous"));
    }

    #[test]
    fn generations_compare_exactly_and_a_failed_probe_is_invalid() {
        let core = Fake::default().file("server/rust-dav-listener.cjs", "x").probe("rust-dav-listener.cjs", "v1");
        let p = run(&core, &features("rust-dav-listener-v1"), &Env::default()).unwrap();
        assert_eq!(p.dav_listener_core, "v1");
        assert!(refusal(&core, &features("")).contains("DAV listener ownership mismatch"));
        let mut failed = Fake::default().file("server/rust-source-startup.cjs", "x");
        failed.probes.insert("rust-source-startup.cjs", NodeOut { ok: false, stdout: "v1".to_owned() });
        assert!(refusal(&failed, &features("rust-source-startup-v1")).contains("source startup ownership mismatch"));
    }

    #[test]
    fn source_job_client_and_store_pair() {
        let canonical = "noevia-source-jobs-client/1";
        let core = Fake::default().file("server/project-store-client.cjs", "SOURCE_JOBS_CLIENT_MARKER").probe("project-store-client.cjs", canonical);
        assert!(run(&core, &features("noevia-source-jobs-store/1"), &Env::default()).is_ok());
        assert!(refusal(&core, &features("")).contains("requires noevia-source-jobs-store/1"));
        assert!(refusal(&Fake::default(), &features("noevia-source-jobs-store/1")).contains("requires a paired core source job client"));
        let declared = Fake::default().file("server/source-jobs.cjs", "workspace.sourceJobs");
        assert!(refusal(&declared, &features("")).contains("does not export its canonical capability"));
        let unknown = Fake::default().probe("project-store-client.cjs", "noevia-source-jobs-client/2");
        assert!(refusal(&unknown, &features("noevia-source-jobs-store/1")).contains("unknown source job client capability"));
    }

    #[test]
    fn source_work_core_must_have_dropped_the_worker_path() {
        let base = Fake::default().file(INDEX, "require('./rust-project-source-work.cjs')");
        assert!(run(&base, &features("rust-project-source-work"), &Env::default()).is_ok());
        let carries = Fake::default().file(INDEX, "require('./rust-project-source-work.cjs')").file("server/pdf-reduce.cjs", "");
        assert!(refusal(&carries, &features("rust-project-source-work")).contains("Core still carries server/pdf-reduce.cjs"));
        let answers = Fake::default()
            .file(INDEX, "require('./rust-project-source-work.cjs')")
            .file("server/routes/projects.cjs", "headers['X-Noevia-Source-Work']");
        assert!(refusal(&answers, &features("rust-project-source-work")).contains("Core still answers the worker call"));
    }

    #[test]
    fn preset_baseline_and_large_lock() {
        let bridge = Fake::default().file("server/preset-reload.cjs", "const B = 'noevia-preset-baseline-bridge/1';");
        assert!(refusal(&bridge, &features("")).contains("has no rust-preset-baseline"));
        assert!(run(&bridge, &features("rust-preset-baseline"), &Env::default()).is_ok());
        assert!(run(&Fake::default(), &features("rust-preset-baseline"), &Env::default()).is_ok(), "shared mode");
        let host = Fake::default().file("server/large-document-lock.cjs", "require('./large-document-file-lock.cjs')");
        assert!(refusal(&host, &features("")).contains("large-document lock mismatch"));
        assert!(run(&host, &features("rust-large-document-host-lock"), &Env::default()).is_ok());
    }

    #[test]
    fn legacy_claim_needs_an_absolute_front_path() {
        let f = features("legacy-root-claim");
        let p = run(&Fake::default(), &f, &Env::default()).unwrap();
        assert_eq!(p.legacy_claim_bin.as_deref(), Some("/usr/local/bin/noevia-server"));
        let err = check(&Env::default(), "./noevia-server", &f, &Fake::default(), &mut |_| {}).err().unwrap().0;
        assert!(err.contains("requires an absolute NOEVIA_SERVER_BIN"));
    }
}
