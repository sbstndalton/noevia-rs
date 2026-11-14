//! The front's own clients of Node (the reverse direction of legacy_proxy.rs), and how a front started
//! without Node builds them so that none of them dials.
//!
//! Every client of Node in this process is built from two keys of the environment: Node's loopback
//! address ([`BASE`], `NOEVIA_LEGACY_UPSTREAM`) and the per-start secret the web supervisor hands both
//! processes ([`SECRET`], `NOEVIA_PROJECT_STORE_TOKEN`, header `x-noevia-store-token`). Each of them already
//! has a path for "no Node session", taken when the secret is missing: no request is built and the caller
//! answers as for a Node that is down (see [`CLIENTS`] for each one's answer).
//!
//! A front alone (config.rs `front_only`, `NOEVIA_FRONT_ONLY=1`, set by the web supervisor's
//! `NOEVIA_SUPERVISOR_FRONT_ONLY` mode) builds them from [`lookup`], where both keys read as unset. So
//! nothing is sent to the reserved port the supervisor names as `NOEVIA_LEGACY_UPSTREAM` and the per-start
//! secret never leaves the process, whatever a client would otherwise ask Node. The config itself still
//! parses `NOEVIA_LEGACY_UPSTREAM` (a required key) and the front's project store still takes the secret
//! from the raw lookup: that listener is the front's, it answers callers and dials nobody.
//!
//! With `NOEVIA_FRONT_ONLY` unset (production) [`lookup`] passes every key through: nothing changes.
//!
//! The tests below pin the census: every source file that reads one of the two keys, and every file that
//! names a Node `/internal/` path in code, is listed here with how it reaches Node, so a new client of
//! Node cannot appear without saying what it does in a front alone.

/// Node's loopback address (`http://<loopback ip>:<port>`; config.rs `Upstream`).
pub const BASE: &str = "NOEVIA_LEGACY_UPSTREAM";
/// The per-start secret of the loopback calls between the front and Node.
pub const SECRET: &str = "NOEVIA_PROJECT_STORE_TOKEN";

/// The keys a client of Node may not see in a front alone.
pub const HIDDEN: [&str; 2] = [BASE, SECRET];

/// The lookup the front's clients of Node are built from: `get` itself beside Node, `get` without
/// [`HIDDEN`] in a front alone.
pub fn lookup<'a, G>(get: &'a G, front_only: bool) -> impl Fn(&str) -> Option<String> + 'a
where
    G: Fn(&str) -> Option<String> + ?Sized,
{
    move |key: &str| {
        if front_only && HIDDEN.contains(&key) {
            None
        } else {
            get(key)
        }
    }
}

/// How a client reaches Node.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Transport {
    /// research_routes.rs's loopback bridge (`node_call`, `node_call_text`, `node_open`), its
    /// `front_token()` for a module's own call, or its inference admission (engine-admission).
    Bridge,
    /// model_maint's own clients: Node's leases (node_lease.rs), the budget fence's bridge
    /// (budget_watch.rs) and the evidence recheck's admission.
    Maint,
    /// approval_revocation.rs's own client.
    Revoker,
    /// The legacy proxy (legacy_proxy.rs): `forward`, `relay`, the chat turn's relay.
    Proxy,
    /// embedding_background.rs: idles in a front alone (it never starts).
    Idle,
}

/// One client of Node: the file it lives in, how it reaches Node, and what it answers in a front alone.
#[derive(Debug, Clone, Copy)]
pub struct Client {
    pub file: &'static str,
    pub transport: Transport,
    pub alone: &'static str,
}

const DOWN: &str = "no request is built: the caller's answer for a Node that is down";
const BUSY: &str = "the admission refuses busy (503) without dialling";
const PROXY: &str = "the detached proxy answers 501 without connecting (legacy_proxy.rs)";
const IDLE: &str = "never starts";

const fn row(file: &'static str, transport: Transport, alone: &'static str) -> Client {
    Client {
        file,
        transport,
        alone,
    }
}

/// Every file under `src/` that calls Node, keyed by path relative to `src/`.
#[rustfmt::skip]
pub const CLIENTS: &[Client] = &[
    // research_routes.rs's bridge and its users.
    row("research_routes.rs", Transport::Bridge, DOWN),
    row("auto_roles_routes.rs", Transport::Bridge, DOWN),
    row("autotune_cancel_routes.rs", Transport::Bridge, DOWN),
    row("autotune_clear_routes.rs", Transport::Bridge, DOWN),
    row("autotune_read_routes.rs", Transport::Bridge, DOWN),
    row("autotune_resume_routes.rs", Transport::Bridge, DOWN),
    row("autotune_runner.rs", Transport::Bridge, DOWN),
    row("autotune_settings_routes.rs", Transport::Bridge, DOWN),
    row("autotune_untuned_routes.rs", Transport::Bridge, DOWN),
    row("chat_framing_suggest.rs", Transport::Bridge, DOWN),
    row("chat_mirror.rs", Transport::Bridge, DOWN),
    row("chat_native.rs", Transport::Bridge, DOWN),
    row("code_routes.rs", Transport::Bridge, DOWN),
    row("idle_unload_read.rs", Transport::Bridge, DOWN),
    row("machine_service.rs", Transport::Bridge, DOWN),
    row("model_assignments.rs", Transport::Bridge, DOWN),
    row("model_downloads_read.rs", Transport::Bridge, DOWN),
    row("model_manager_scan.rs", Transport::Bridge, DOWN),
    row("model_state_routes.rs", Transport::Bridge, DOWN),
    row("model_writes_routes.rs", Transport::Bridge, DOWN),
    row("preset_read_routes.rs", Transport::Bridge, DOWN),
    row("task_routes.rs", Transport::Bridge, DOWN),
    row("chat_admission.rs", Transport::Bridge, BUSY),
    // model_maint's own clients.
    row("model_maint/mod.rs", Transport::Maint, DOWN),
    row("model_maint/node_lease.rs", Transport::Maint, DOWN),
    row("model_maint/budget_watch.rs", Transport::Maint, DOWN),
    row("model_maint/calibration_worker.rs", Transport::Maint, DOWN),
    row("model_maint/evidence_recheck.rs", Transport::Maint, BUSY),
    // The rest.
    row("approval_revocation.rs", Transport::Revoker, DOWN),
    row("legacy_proxy.rs", Transport::Proxy, PROXY),
    row("chat_turn.rs", Transport::Proxy, DOWN),
    // Only the Cowork start's path, relayed by chat_turn.rs.
    row("chat_edge.rs", Transport::Proxy, DOWN),
    row("embedding_background.rs", Transport::Idle, IDLE),
];

/// The files allowed to read [`BASE`] or [`SECRET`] by their literal names (outside tests): this module
/// and the config, which parses the address and gives the project store its secret.
pub const LITERAL_READERS: &[&str] = &["node_link.rs", "config.rs"];

/// The files that build a client of Node from [`BASE`] or [`SECRET`] (by these constants). Each takes the
/// lookup it reads them from as an argument; config.rs and main.rs hand them [`lookup`].
pub const CONSTANT_READERS: &[&str] = &[
    "config.rs",
    "research_routes.rs",
    "model_maint/mod.rs",
    "model_maint/budget_watch.rs",
    "model_maint/evidence_recheck.rs",
];

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;
    use std::path::{Path, PathBuf};

    fn env(pairs: &'static [(&'static str, &'static str)]) -> impl Fn(&str) -> Option<String> {
        move |k: &str| {
            pairs
                .iter()
                .find(|(n, _)| *n == k)
                .map(|(_, v)| (*v).to_owned())
        }
    }

    const ENV: &[(&str, &str)] = &[
        (BASE, "http://127.0.0.1:9021"),
        (SECRET, "synthetic-store-token-0123456789abcdef"),
        ("UI_DATA_DIR", "/data"),
        ("INFERENCE_BASE_URL", "http://127.0.0.1:11434"),
    ];

    #[test]
    fn beside_node_every_key_passes_through() {
        let get = env(ENV);
        let node = lookup(&get, false);
        for (k, v) in ENV {
            assert_eq!(node(k).as_deref(), Some(*v), "{k}");
        }
        assert_eq!(node("UNSET_KEY"), None);
    }

    #[test]
    fn a_front_alone_hides_exactly_nodes_address_and_secret() {
        let get = env(ENV);
        let alone = lookup(&get, true);
        assert_eq!(alone(BASE), None);
        assert_eq!(alone(SECRET), None);
        assert_eq!(alone("UI_DATA_DIR").as_deref(), Some("/data"));
        assert_eq!(
            alone("INFERENCE_BASE_URL").as_deref(),
            Some("http://127.0.0.1:11434")
        );
        // Names that only look alike are other keys.
        let near: &'static [(&str, &str)] = &[
            ("NOEVIA_PROJECT_STORE_TOKEN_PREVIOUS", "x"),
            ("noevia_legacy_upstream", "y"),
            ("NOEVIA_PROJECT_STORE_PORT", "19021"),
        ];
        let get = env(near);
        let alone = lookup(&get, true);
        for (k, v) in near {
            assert_eq!(alone(k).as_deref(), Some(*v), "{k}");
        }
    }

    #[test]
    fn the_lookup_works_over_a_trait_object_and_a_closure() {
        let raw = |k: &str| (k == SECRET).then(|| "s".to_owned());
        let dynamic: &dyn Fn(&str) -> Option<String> = &raw;
        assert_eq!(lookup(dynamic, false)(SECRET).as_deref(), Some("s"));
        assert_eq!(lookup(dynamic, true)(SECRET), None);
        assert_eq!(lookup(&raw, true)(SECRET), None);
    }

    // ---- The census, read from the sources ----

    fn src() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("src")
    }

    fn files(dir: &Path, out: &mut Vec<PathBuf>) {
        let mut entries: Vec<_> = std::fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().path())
            .collect();
        entries.sort();
        for p in entries {
            if p.is_dir() {
                files(&p, out);
            } else if p.extension().is_some_and(|e| e == "rs") {
                out.push(p);
            }
        }
    }

    /// Each file's code lines (relative path, line): comments dropped, and everything from the first
    /// `#[cfg(...test...)]` on dropped (this crate keeps its unit tests at the end of a file).
    fn code() -> Vec<(String, Vec<String>)> {
        let root = src();
        let mut all = Vec::new();
        files(&root, &mut all);
        all.iter()
            .map(|p| {
                let rel = p
                    .strip_prefix(&root)
                    .unwrap()
                    .to_string_lossy()
                    .replace('\\', "/");
                let text = std::fs::read_to_string(p).unwrap();
                let lines = text
                    .lines()
                    .take_while(|l| {
                        let t = l.trim();
                        !(t.starts_with("#[cfg(") && t.contains("test"))
                    })
                    .filter(|l| {
                        let t = l.trim_start();
                        !t.starts_with("//")
                    })
                    .map(str::to_owned)
                    .collect();
                (rel, lines)
            })
            .collect()
    }

    fn with(needle: &str) -> BTreeSet<String> {
        code()
            .into_iter()
            .filter(|(_, lines)| lines.iter().any(|l| l.contains(needle)))
            .map(|(f, _)| f)
            .collect()
    }

    #[test]
    fn the_census_has_one_row_per_file_and_every_file_exists() {
        let mut seen = BTreeSet::new();
        for c in CLIENTS {
            assert!(seen.insert(c.file), "{} twice", c.file);
            assert!(src().join(c.file).is_file(), "{} is not under src/", c.file);
            assert!(!c.alone.is_empty(), "{}", c.file);
        }
        for f in LITERAL_READERS.iter().chain(CONSTANT_READERS) {
            assert!(src().join(f).is_file(), "{f} is not under src/");
        }
    }

    #[test]
    fn only_the_config_and_this_module_name_the_two_keys_literally() {
        for key in HIDDEN {
            let literal = format!("\"{key}\"");
            let readers = with(&literal);
            let allowed: BTreeSet<String> =
                LITERAL_READERS.iter().map(|s| (*s).to_owned()).collect();
            let extra: Vec<_> = readers.difference(&allowed).collect();
            assert!(
                extra.is_empty(),
                "{key} is read by its literal name outside the census (use node_link::{} through node_link::lookup): {extra:?}",
                if key == BASE { "BASE" } else { "SECRET" }
            );
        }
    }

    #[test]
    fn every_reader_of_the_constants_is_pinned() {
        let mut readers = with("node_link::BASE");
        readers.extend(with("node_link::SECRET"));
        let pinned: BTreeSet<String> = CONSTANT_READERS.iter().map(|s| (*s).to_owned()).collect();
        assert_eq!(
            readers, pinned,
            "a new reader of Node's address or secret: build it from node_link::lookup and list it in CONSTANT_READERS"
        );
    }

    #[test]
    fn every_file_naming_a_node_internal_path_in_code_is_a_census_row() {
        let callers: BTreeSet<String> = with("\"/internal/")
            .into_iter()
            .filter(|f| f != "node_link.rs")
            .collect();
        let census: BTreeSet<String> = CLIENTS.iter().map(|c| c.file.to_owned()).collect();
        let missing: Vec<_> = callers.difference(&census).collect();
        assert!(
            missing.is_empty(),
            "files that name a Node /internal/ path but are not in node_link::CLIENTS: {missing:?}"
        );
    }

    #[test]
    fn the_bridge_rows_reach_node_only_through_the_bridge() {
        // A Bridge row reaches Node through research_routes.rs's client (or its admission or secret),
        // never with a client of its own built from Node's keys.
        let own: BTreeSet<String> = CONSTANT_READERS.iter().map(|s| (*s).to_owned()).collect();
        for c in CLIENTS.iter().filter(|c| c.transport == Transport::Bridge) {
            if c.file == "research_routes.rs" {
                continue;
            }
            assert!(
                !own.contains(c.file),
                "{} builds its own client of Node",
                c.file
            );
        }
    }

    /// What a row's file must use (in code) to reach Node the way its row says.
    fn markers(t: Transport) -> &'static [&'static str] {
        match t {
            Transport::Bridge => &[
                ".node_call(",
                ".node_call_text(",
                ".node_open(",
                "front_token()",
                ".admission()",
                "fn bridge_post_text(",
                "autotune_runner::call(",
            ],
            Transport::Maint => &[
                "NodeLeases",
                "NodeBridge",
                "Admission",
                ".node.",
                "node_lease::",
            ],
            Transport::Revoker => &["x-noevia-store-token"],
            Transport::Proxy => &["proxy.", "relay(", "COWORK_PATH"],
            Transport::Idle => &["front_only"],
        }
    }

    #[test]
    fn every_row_reaches_node_the_way_it_says() {
        let code = code();
        for c in CLIENTS {
            let (_, lines) = code.iter().find(|(f, _)| f == c.file).unwrap();
            let wanted = markers(c.transport);
            assert!(
                lines.iter().any(|l| wanted.iter().any(|m| l.contains(m))),
                "{} ({:?}) uses none of {wanted:?}",
                c.file,
                c.transport
            );
        }
    }
}
