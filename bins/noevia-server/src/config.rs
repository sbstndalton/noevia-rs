//! Environment, read with the same key names and meanings as core server/index.cjs:
//! UI_HOST (default 0.0.0.0), UI_PORT (default 8021), TRUST_PROXY (exactly "true" enables it),
//! STAMP_VERSION (the /api/ready version fallback). New keys: NOEVIA_LEGACY_UPSTREAM (the Node
//! server, loopback http only) and NOEVIA_WEB_DIST (the built web client, default ./dist, which
//! is /app/dist in the web image, the directory Node serves). COWORK_CODE_NET_ADDR as in core
//! code-net-guard.cjs (see code_net.rs; a malformed value stops startup). UI_DATA_DIR and the auth
//! keys (PUBLIC_ORIGIN, ADDITIONAL_TRUSTED_ORIGINS, UI_AUTH_TOKEN, LEGACY_AUTH_COMPAT,
//! NOEVIA_FEATURE_NATIVE_CLIENT_AUTH) as core index.cjs reads them, for identity.rs.
//! Rust always owns sign-in and the account and the tables they write (M3; Node always refuses
//! those writes), and the front read routes (sbstndalton/noevia#1071): their old deployment
//! switch NOEVIA_RUST_AUTH is retired, so UI_DATA_DIR and a valid auth environment are startup
//! requirements, and a leftover NOEVIA_RUST_AUTH value only earns a warning.

use std::net::IpAddr;
use std::path::PathBuf;

#[derive(Debug, Clone)]
pub struct Config {
    pub host: String,
    pub port: u16,
    pub trust_proxy: bool,
    pub upstream: Upstream,
    pub dist: PathBuf,
    pub stamp_version: Option<String>,
    /// COWORK_CODE_NET_ADDR, parsed as core code-net-guard.cjs does (empty: no guard).
    pub code_net: crate::code_net::CodeNetSpec,
    /// UI_DATA_DIR, where Node keeps cowork.db. Unset: no identity (every extraction is a 503);
    /// Node's own default (server/ui-data next to its code) is not guessed.
    pub data_dir: Option<PathBuf>,
    /// The auth environment, or why it is invalid.
    pub auth: Result<server_auth::AuthConfig, String>,
    /// Rust's ownership of sign-in and the account (M3), permanent since sbstndalton/noevia#1071.
    pub rust_auth: server_store::RustAuth,
    /// NOEVIA_RUST_PROJECTS=1 (M4).
    pub rust_projects: Option<server_store::RustProjects>,
    /// The permanent front read routes (sbstndalton/noevia#1071): always on (export: with
    /// NOEVIA_RUST_PROJECTS=1), and every prerequisite (UI_DATA_DIR, valid native auth, the
    /// compiled feature, a stats engine the front can read) is a startup requirement. Their old
    /// switches (RETIRED_SWITCHES) are not read.
    pub rust_usage: bool,
    pub rust_storage_reads: bool,
    /// The outbound storage list/read (#1368).
    pub rust_storage_browse: bool,
    pub rust_reasoning_settings: Option<server_store::RustReasoningSettings>,
    pub rust_routing_mode: Option<server_store::RustRoutingMode>,
    pub routing_feature_env: Option<bool>,
    /// GET /api/routing-default (M6, #1305).
    pub rust_routing_default: bool,
    /// GET /api/stats (M6, #1650): the engine it reads, configured from the keys core index.cjs
    /// read (MODEL_MANAGER_KIND, MODEL_MANAGER_BASE_URL / LEMONADE_BASE_URL / INFERENCE_BASE_URL,
    /// MODEL_MANAGER_API_KEY / INFERENCE_API_KEY / LEMONADE_API_KEY). Startup is refused where
    /// Node itself would not start (an unknown kind, an invalid llama.cpp router URL), for an
    /// `https` engine (the front has no TLS client) and for the Lemonade kind, whose MTP "last
    /// response" fallback was kept in Node's memory.
    pub rust_stats: Option<std::sync::Arc<server_stats::Engine>>,
    /// GET /api/export/conversations (M4, sbstndalton/noevia#1315), read from the user's files on
    /// disk: on with NOEVIA_RUST_PROJECTS=1, the free-chat list Node re-reads from disk (the
    /// owner's 2026-10-12 condition).
    pub rust_export: bool,
    /// NOEVIA_RUST_PROJECT_WRITES=1 (sbstndalton/noevia#1264): the project writes ported from
    /// Node so far. Refused without NOEVIA_RUST_PROJECTS=1 (they share its projects.json lock and
    /// write switch), without the compiled rust-project-writes feature and without UI_DATA_DIR;
    /// any other value is off.
    pub rust_project_writes: bool,
    /// NOEVIA_RUST_PROJECT_STORE=1 (sbstndalton/noevia#1264): the internal store Node writes
    /// projects.json and free-chats.json through (src/project_store.rs), on 127.0.0.1 at
    /// NOEVIA_PROJECT_STORE_PORT with the secret NOEVIA_PROJECT_STORE_TOKEN (at least 32 bytes).
    /// Refused without NOEVIA_RUST_PROJECTS=1, UI_DATA_DIR, the port or the secret; any other
    /// value is off.
    pub project_store: Option<ProjectStoreConfig>,
    /// GET /api/providers/chatgpt (M6, sbstndalton/noevia#1373). An invalid
    /// NOEVIA_FEATURE_CHATGPT_OAUTH refuses startup (Node does not start with one either).
    pub rust_chatgpt_status: Option<ChatgptStatus>,
    pub rust_sampling_settings: Option<server_store::RustSamplingSettings>,
    pub usage_clock: Result<server_usage::summary::Clock, server_usage::Fault>,
    /// Retired deployment switches still set in the environment (#1071): reported at startup,
    /// never read for ownership.
    pub retired: Vec<&'static str>,
    /// What the account routes read (PUBLIC_ORIGIN, WEBAUTHN_RP_ID, TRUST_PROXY, COWORK_DAV_PORT,
    /// UI_DATA_DIR); `None` without UI_DATA_DIR.
    pub account: Option<server_account::Settings>,
}

/// The native ChatGPT status's startup inputs (#1373). Read once, as Node reads them once.
#[derive(Debug, Clone)]
pub struct ChatgptStatus {
    /// NOEVIA_FEATURE_CHATGPT_OAUTH, parsed as core features.cjs `parseEnv` (it overrides the
    /// admin setting while set).
    pub feature_env: Option<bool>,
    /// SECRETS_KEY_PREVIOUS (secrets.key.previous takes precedence), for the startup key snapshot.
    pub previous_key_env: RedactedEnv,
}

/// An environment value that may be key material: cloned with the config, never printed.
#[derive(Clone, Default)]
pub struct RedactedEnv(pub Option<String>);

impl std::fmt::Debug for RedactedEnv {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(if self.0.is_some() {
            "RedactedEnv(set)"
        } else {
            "RedactedEnv(unset)"
        })
    }
}

/// Where Node listens. Only `http://<loopback ip>:<port>` is accepted: the proxy forwards session
/// cookies and CSRF tokens, which must never leave the host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Upstream {
    pub ip: IpAddr,
    pub port: u16,
}

impl Upstream {
    pub fn authority(&self) -> String {
        match self.ip {
            IpAddr::V4(v4) => format!("{v4}:{}", self.port),
            IpAddr::V6(v6) => format!("[{v6}]:{}", self.port),
        }
    }

    pub fn parse(raw: &str) -> Result<Self, String> {
        let rest = raw
            .trim()
            .strip_prefix("http://")
            .ok_or("NOEVIA_LEGACY_UPSTREAM must be http://<loopback ip>:<port>")?;
        let rest = rest.strip_suffix('/').unwrap_or(rest);
        let (host, port) = rest
            .rsplit_once(':')
            .ok_or("NOEVIA_LEGACY_UPSTREAM needs a port")?;
        let host = host
            .strip_prefix('[')
            .and_then(|h| h.strip_suffix(']'))
            .unwrap_or(host);
        let ip: IpAddr = host
            .parse()
            .map_err(|_| "NOEVIA_LEGACY_UPSTREAM host must be a loopback IP address".to_string())?;
        if !ip.is_loopback() {
            return Err("NOEVIA_LEGACY_UPSTREAM must be a loopback address".into());
        }
        let port: u16 = port
            .parse()
            .ok()
            .filter(|p| *p != 0)
            .ok_or("NOEVIA_LEGACY_UPSTREAM port must be 1-65535")?;
        Ok(Upstream { ip, port })
    }
}

fn port_from(raw: Option<String>) -> Result<u16, String> {
    match raw.as_deref().map(str::trim) {
        // Node: Number(process.env.UI_PORT || 8021); an empty value means the default.
        None | Some("") => Ok(8021),
        Some(v) => v
            .parse::<u16>()
            .map_err(|_| format!("UI_PORT {v:?} is not a port number")),
    }
}

#[derive(Clone)]
pub struct ProjectStoreConfig {
    pub port: u16,
    pub token: String,
}

impl std::fmt::Debug for ProjectStoreConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProjectStoreConfig")
            .field("port", &self.port)
            .finish_non_exhaustive()
    }
}

impl Config {
    /// The deployment switches that are on.
    pub fn switches(&self) -> crate::routes::Switches {
        crate::routes::Switches {
            rust_projects: self.rust_projects.is_some(),
            rust_project_writes: self.rust_project_writes,
        }
    }

    pub fn from_lookup(get: impl Fn(&str) -> Option<String>) -> Result<Self, String> {
        let host = get("UI_HOST")
            .filter(|h| !h.is_empty())
            .unwrap_or_else(|| "0.0.0.0".into());
        let port = port_from(get("UI_PORT"))?;
        let upstream = Upstream::parse(
            &get("NOEVIA_LEGACY_UPSTREAM").ok_or("NOEVIA_LEGACY_UPSTREAM is required")?,
        )?;
        if (upstream.port == port) && is_loopback_or_any(&host) {
            return Err("NOEVIA_LEGACY_UPSTREAM must not be this server's own port".into());
        }
        // sbstndalton/noevia#1071: sign-in, the account and the front read routes are permanent.
        // The front always owns them (Node no longer has them), so everything they need is a
        // startup requirement: a build without one of them, or an environment they cannot serve
        // from, refuses to start instead of leaving the route unanswered.
        let rust_auth = server_store::RustAuth::owned();
        let rust_projects = server_store::RustProjects::from_env_value(
            get(server_store::RustProjects::ENV).as_deref(),
        );
        let retired = retired_flags(&get);
        let rust_usage = true;
        let rust_storage_reads = true;
        let rust_storage_browse = true;
        let rust_sampling_settings =
            server_store::RustSamplingSettings::from_env_value(Some("1"), rust_auth);
        let rust_reasoning_settings =
            server_store::RustReasoningSettings::from_env_value(Some("1"), rust_auth);
        let rust_routing_mode = server_store::RustRoutingMode::from_env_value(Some("1"), rust_auth);
        let rust_routing_default = true;
        let engine =
            server_stats::Engine::from_lookup(&get).map_err(|e| format!("GET /api/stats: {e}"))?;
        if engine.kind() == server_stats::Kind::Lemonade {
            return Err("GET /api/stats is not available with the Lemonade model manager (its MTP last-response fallback was recorded in Node)".into());
        }
        let rust_stats = Some(std::sync::Arc::new(engine));
        // GET /api/export/conversations reads the free-chat list from disk, which is only the
        // list Node re-reads under NOEVIA_RUST_PROJECTS (the owner's 2026-10-12 condition): it is
        // the front's whenever that switch is on (the web supervisor requires it).
        let rust_export = rust_projects.is_some();
        let rust_project_writes = if get("NOEVIA_RUST_PROJECT_WRITES").as_deref() == Some("1") {
            if rust_projects.is_none() {
                return Err("NOEVIA_RUST_PROJECT_WRITES=1 needs NOEVIA_RUST_PROJECTS=1".into());
            }
            if !cfg!(feature = "rust-project-writes") {
                return Err(
                    "NOEVIA_RUST_PROJECT_WRITES=1 needs compiled rust-project-writes".into(),
                );
            }
            if get("UI_DATA_DIR").filter(|d| !d.is_empty()).is_none() {
                return Err("NOEVIA_RUST_PROJECT_WRITES=1 needs UI_DATA_DIR".into());
            }
            true
        } else {
            false
        };
        let project_store = if get("NOEVIA_RUST_PROJECT_STORE").as_deref() == Some("1") {
            if rust_projects.is_none() {
                return Err("NOEVIA_RUST_PROJECT_STORE=1 needs NOEVIA_RUST_PROJECTS=1".into());
            }
            if get("UI_DATA_DIR").filter(|d| !d.is_empty()).is_none() {
                return Err("NOEVIA_RUST_PROJECT_STORE=1 needs UI_DATA_DIR".into());
            }
            let port = get("NOEVIA_PROJECT_STORE_PORT")
                .and_then(|p| p.parse::<u16>().ok())
                .filter(|p| *p > 0)
                .ok_or("NOEVIA_RUST_PROJECT_STORE=1 needs NOEVIA_PROJECT_STORE_PORT (1-65535)")?;
            let token = get("NOEVIA_PROJECT_STORE_TOKEN")
                .filter(|t| t.len() >= crate::project_store::TOKEN_MIN && t.bytes().all(|b| b.is_ascii_graphic()))
                .ok_or("NOEVIA_RUST_PROJECT_STORE=1 needs NOEVIA_PROJECT_STORE_TOKEN (at least 32 visible ASCII characters)")?;
            Some(ProjectStoreConfig { port, token })
        } else {
            None
        };
        // Checked after the project switches so their own refusals come first.
        let missing: Vec<String> = FRONT_ROUTE_FEATURES
            .iter()
            .filter(|(_, compiled)| !compiled)
            .map(|(name, _)| format!("the front needs compiled {name}"))
            .collect();
        if !missing.is_empty() {
            return Err(missing.join("\n"));
        }
        if get("UI_DATA_DIR").is_none_or(|v| v.is_empty()) {
            return Err("UI_DATA_DIR is required: the front owns sign-in, the account and the front read routes (#1071)".into());
        }
        let auth = server_auth::AuthConfig::from_lookup(&get);
        if let Err(e) = &auth {
            return Err(format!(
                "invalid native auth environment (the front owns sign-in, #1071): {e}"
            ));
        }
        let feature_env =
            server_routing_mode::feature_env(get("NOEVIA_FEATURE_CHATGPT_OAUTH").as_deref())
                .map_err(|e| format!("NOEVIA_FEATURE_CHATGPT_OAUTH: {}", e.message))?;
        let rust_chatgpt_status = Some(ChatgptStatus {
            feature_env,
            previous_key_env: RedactedEnv(get("SECRETS_KEY_PREVIOUS")),
        });
        let routing_feature_env = if rust_routing_mode.is_some() {
            server_routing_mode::feature_env(get("NOEVIA_FEATURE_ROUTING_MODES").as_deref())
                .map_err(|e| e.message)?
        } else {
            None
        };
        Ok(Config {
            host,
            port,
            trust_proxy: get("TRUST_PROXY").as_deref() == Some("true"),
            upstream,
            dist: get("NOEVIA_WEB_DIST")
                .filter(|d| !d.is_empty())
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("dist")),
            stamp_version: get("STAMP_VERSION").filter(|v| !v.is_empty()),
            code_net: crate::code_net::CodeNetSpec::parse(
                &get("COWORK_CODE_NET_ADDR").unwrap_or_default(),
            )?,
            data_dir: get("UI_DATA_DIR")
                .filter(|d| !d.is_empty())
                .map(PathBuf::from),
            account: get("UI_DATA_DIR")
                .filter(|d| !d.is_empty())
                .map(|d| server_account::Settings::from_lookup(&get, PathBuf::from(d))),
            auth,
            rust_auth,
            rust_projects,
            rust_usage,
            rust_storage_reads,
            rust_storage_browse,
            rust_sampling_settings,
            rust_reasoning_settings,
            rust_routing_mode,
            routing_feature_env,
            rust_routing_default,
            rust_stats,
            rust_export,
            rust_project_writes,
            project_store,
            rust_chatgpt_status,
            usage_clock: server_usage::summary::Clock::system(&get),
            retired,
        })
    }
}

/// The deployment switches retired by sbstndalton/noevia#1071: the routes they moved are always
/// the front's. A leftover value only earns a warning.
pub const RETIRED_SWITCHES: &[&str] = &[
    "NOEVIA_RUST_AUTH",
    "NOEVIA_RUST_CHATGPT_STATUS",
    "NOEVIA_RUST_STORAGE_READS",
    "NOEVIA_RUST_STORAGE_BROWSE",
    "NOEVIA_RUST_EXPORT",
    "NOEVIA_RUST_USAGE",
    "NOEVIA_RUST_STATS",
    "NOEVIA_RUST_ROUTING_DEFAULT",
    "NOEVIA_RUST_SAMPLING_SETTINGS",
    "NOEVIA_RUST_REASONING_SETTINGS",
    "NOEVIA_RUST_ROUTING_MODE",
];

/// The cargo features the permanent front routes need, and whether this build has each. The
/// web supervisor checks the same names in `--features` before starting either process.
pub const FRONT_ROUTE_FEATURES: &[(&str, bool)] = &[
    ("rust-chatgpt-status", cfg!(feature = "rust-chatgpt-status")),
    ("rust-storage-reads", cfg!(feature = "rust-storage-reads")),
    ("rust-storage-browse", cfg!(feature = "rust-storage-browse")),
    ("rust-export", cfg!(feature = "rust-export")),
    ("rust-free-chats", cfg!(feature = "rust-free-chats")),
    (
        "rust-routing-default",
        cfg!(feature = "rust-routing-default"),
    ),
    ("rust-stats", cfg!(feature = "rust-stats")),
    (
        "rust-reasoning-settings",
        cfg!(feature = "rust-reasoning-settings"),
    ),
    ("rust-routing-mode", cfg!(feature = "rust-routing-mode")),
];

fn retired_flags(get: &impl Fn(&str) -> Option<String>) -> Vec<&'static str> {
    RETIRED_SWITCHES
        .iter()
        .copied()
        .filter(|k| get(k).is_some())
        .collect()
}

fn is_loopback_or_any(host: &str) -> bool {
    match host.parse::<IpAddr>() {
        Ok(ip) => ip.is_loopback() || ip.is_unspecified(),
        Err(_) => host == "localhost",
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn cfg(pairs: &[(&str, &str)]) -> Result<Config, String> {
        let m: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        Config::from_lookup(|k| m.get(k).cloned())
    }

    const DATA: (&str, &str) = ("UI_DATA_DIR", "/data");

    #[test]
    fn node_defaults_and_names() {
        let c = cfg(&[("NOEVIA_LEGACY_UPSTREAM", "http://127.0.0.1:9021"), DATA]).unwrap();
        assert_eq!(
            (c.host.as_str(), c.port, c.trust_proxy),
            ("0.0.0.0", 8021, false)
        );
        let c = cfg(&[
            ("NOEVIA_LEGACY_UPSTREAM", "http://127.0.0.1:9021/"),
            ("UI_HOST", "127.0.0.1"),
            ("UI_PORT", "18021"),
            ("TRUST_PROXY", "true"),
            DATA,
        ])
        .unwrap();
        assert_eq!(
            (c.host.as_str(), c.port, c.trust_proxy),
            ("127.0.0.1", 18021, true)
        );
        // Only the exact string "true" turns it on, as in Node.
        let c = cfg(&[
            ("NOEVIA_LEGACY_UPSTREAM", "http://[::1]:9021"),
            ("TRUST_PROXY", "TRUE"),
            DATA,
        ])
        .unwrap();
        assert!(!c.trust_proxy);
        assert_eq!(c.upstream.authority(), "[::1]:9021");
    }

    /// sbstndalton/noevia#1071: Rust always owns sign-in and the account. NOEVIA_RUST_AUTH is
    /// retired: whatever it says (or when it is unset) the ownership is the same, and a leftover
    /// value is only reported.
    #[test]
    fn rust_auth_is_unconditional_and_its_old_switch_is_retired() {
        for v in [None, Some("1"), Some("0"), Some(""), Some("true")] {
            let mut pairs = vec![("NOEVIA_LEGACY_UPSTREAM", "http://127.0.0.1:9021"), DATA];
            if let Some(v) = v {
                pairs.push(("NOEVIA_RUST_AUTH", v));
            }
            let c = cfg(&pairs).unwrap();
            assert_eq!(c.rust_auth, server_store::RustAuth::owned(), "{v:?}");
            assert!(c.auth.is_ok());
            assert_eq!(
                c.retired.contains(&"NOEVIA_RUST_AUTH"),
                v.is_some(),
                "{v:?}"
            );
            assert!(crate::FEATURES.contains(&"rust-auth"));
        }
    }

    #[test]
    fn rust_auth_refuses_to_start_without_a_data_dir_or_valid_auth() {
        let up = ("NOEVIA_LEGACY_UPSTREAM", "http://127.0.0.1:9021");
        for pairs in [
            vec![up],
            vec![up, ("UI_DATA_DIR", "")],
            vec![up, ("NOEVIA_RUST_AUTH", "1")],
        ] {
            let err = cfg(&pairs).unwrap_err();
            assert!(err.contains("UI_DATA_DIR is required"), "{err}");
        }
        let err = cfg(&[up, DATA, ("NOEVIA_FEATURE_NATIVE_CLIENT_AUTH", "maybe")]).unwrap_err();
        assert!(err.contains("invalid native auth environment"), "{err}");
    }

    #[test]
    fn rust_projects_no_longer_needs_the_retired_auth_switch() {
        let up = ("NOEVIA_LEGACY_UPSTREAM", "http://127.0.0.1:9021");
        let c = cfg(&[up, DATA, ("NOEVIA_RUST_PROJECTS", "1")]).unwrap();
        assert!(c.switches().rust_projects);
        for v in ["", "0", "true"] {
            let c = cfg(&[up, DATA, ("NOEVIA_RUST_PROJECTS", v)]).unwrap();
            assert!(!c.switches().rust_projects, "{v:?}");
        }
    }

    #[test]
    fn refuses_bad_upstreams() {
        for bad in [
            "http://10.0.0.5:9021",
            "https://127.0.0.1:9021",
            "http://localhost:9021",
            "http://127.0.0.1",
            "http://127.0.0.1:0",
            "127.0.0.1:9021",
        ] {
            assert!(
                cfg(&[("NOEVIA_LEGACY_UPSTREAM", bad), DATA]).is_err(),
                "{bad}"
            );
        }
        assert!(cfg(&[DATA]).is_err());
        assert!(cfg(&[("NOEVIA_LEGACY_UPSTREAM", "http://127.0.0.1:8021"), DATA]).is_err());
        assert!(cfg(&[
            ("NOEVIA_LEGACY_UPSTREAM", "http://127.0.0.1:9021"),
            ("UI_PORT", "x"),
            DATA
        ])
        .is_err());
    }
}

/// sbstndalton/noevia#1071: the front read routes have no switch of their own. They are always
/// the front's (the export: with NOEVIA_RUST_PROJECTS=1), and everything they need is a startup
/// requirement.
#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod front_routes_tests {
    use super::*;

    fn lookup(pairs: &[(&str, &str)]) -> Result<Config, String> {
        let pairs: Vec<(String, String)> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        Config::from_lookup(|k| pairs.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone()))
    }

    const UP: (&str, &str) = ("NOEVIA_LEGACY_UPSTREAM", "http://127.0.0.1:9");
    const PROJECTS: (&str, &str) = ("NOEVIA_RUST_PROJECTS", "1");
    const DATA: (&str, &str) = ("UI_DATA_DIR", "/data");

    fn all_compiled() -> bool {
        FRONT_ROUTE_FEATURES.iter().all(|(_, on)| *on)
    }

    fn owned(c: &Config) -> [bool; 10] {
        [
            c.rust_usage,
            c.rust_storage_reads,
            c.rust_storage_browse,
            c.rust_sampling_settings.is_some(),
            c.rust_reasoning_settings.is_some(),
            c.rust_routing_mode.is_some(),
            c.rust_routing_default,
            c.rust_stats.is_some(),
            c.rust_export,
            c.rust_chatgpt_status.is_some(),
        ]
    }

    #[test]
    fn the_default_build_lists_every_front_route_feature() {
        for (name, compiled) in FRONT_ROUTE_FEATURES {
            assert_eq!(crate::FEATURES.contains(name), *compiled, "{name}");
        }
        // rust-auth, rust-usage and rust-sampling-settings are in every build.
        assert!(crate::FEATURES.contains(&"rust-auth"));
        assert!(crate::FEATURES.contains(&"rust-usage"));
        assert!(crate::FEATURES.contains(&"rust-sampling-settings"));
    }

    #[test]
    fn the_front_owns_every_read_route_without_any_switch() {
        let enabled = lookup(&[UP, PROJECTS, DATA]);
        if !all_compiled() {
            let err = enabled.unwrap_err();
            for (name, compiled) in FRONT_ROUTE_FEATURES {
                assert_eq!(
                    err.contains(&format!("needs compiled {name}")),
                    !compiled,
                    "{err}"
                );
            }
            return;
        }
        let c = enabled.unwrap();
        assert_eq!(owned(&c), [true; 10]);
        assert!(c.retired.is_empty());
        assert_eq!(
            c.rust_stats.as_ref().unwrap().kind(),
            server_stats::Kind::None
        );
        // Without NOEVIA_RUST_PROJECTS the export stays off; the rest is unchanged.
        let c = lookup(&[UP, DATA]).unwrap();
        assert_eq!(owned(&c)[..8], [true; 8]);
        assert!(!c.rust_export && c.rust_chatgpt_status.is_some());
    }

    #[test]
    fn a_leftover_retired_switch_never_changes_ownership() {
        if !all_compiled() {
            return;
        }
        for value in ["0", "1", "", "true"] {
            let mut on = vec![UP, PROJECTS, DATA];
            for name in RETIRED_SWITCHES {
                on.push((name, value));
            }
            let c = lookup(&on).unwrap();
            assert_eq!(owned(&c), [true; 10], "{value:?}");
            assert_eq!(c.retired, RETIRED_SWITCHES.to_vec());
        }
    }

    #[test]
    fn the_front_refuses_to_start_without_a_data_dir_or_valid_auth() {
        if !all_compiled() {
            return;
        }
        for pairs in [vec![UP], vec![UP, ("UI_DATA_DIR", "")]] {
            let err = lookup(&pairs).unwrap_err();
            assert!(err.contains("UI_DATA_DIR is required"), "{err}");
        }
        let err = lookup(&[UP, DATA, ("NOEVIA_FEATURE_NATIVE_CLIENT_AUTH", "maybe")]).unwrap_err();
        assert!(err.contains("invalid native auth environment"), "{err}");
    }

    #[test]
    fn stats_engine_problems_refuse_startup() {
        let bad: [(Vec<(&str, &str)>, &str); 5] = [
            (
                vec![("MODEL_MANAGER_KIND", "ollama")],
                "unsupported MODEL_MANAGER_KIND",
            ),
            (
                vec![
                    ("MODEL_MANAGER_KIND", "llamacpp"),
                    ("MODEL_MANAGER_BASE_URL", "http://u:p@r:1"),
                ],
                "Invalid llama.cpp router URL",
            ),
            (
                vec![
                    ("MODEL_MANAGER_KIND", "llamacpp"),
                    ("MODEL_MANAGER_BASE_URL", "https://r:1"),
                ],
                "plain http",
            ),
            (
                vec![("LEMONADE_BASE_URL", "http://lemonade:8000")],
                "Lemonade",
            ),
            (
                vec![
                    ("MODEL_MANAGER_KIND", "Lemonade"),
                    ("MODEL_MANAGER_BASE_URL", "http://l:1"),
                ],
                "Lemonade",
            ),
        ];
        for (pairs, words) in bad {
            let mut on = vec![UP, DATA];
            on.extend(pairs);
            let err = lookup(&on).unwrap_err();
            assert!(err.contains(words), "{err}");
            assert!(err.contains("/api/stats"), "{err}");
        }
        if all_compiled() {
            let c = lookup(&[
                UP,
                DATA,
                ("MODEL_MANAGER_KIND", "llamacpp"),
                ("MODEL_MANAGER_BASE_URL", "http://router:8080/v1"),
            ])
            .unwrap();
            assert_eq!(
                c.rust_stats.as_ref().unwrap().kind(),
                server_stats::Kind::LlamaCpp
            );
        }
    }

    #[test]
    fn product_flags_are_validated_at_startup() {
        for (key, words) in [
            (
                "NOEVIA_FEATURE_CHATGPT_OAUTH",
                "NOEVIA_FEATURE_CHATGPT_OAUTH",
            ),
            ("NOEVIA_FEATURE_ROUTING_MODES", ""),
        ] {
            if all_compiled() {
                let err = lookup(&[UP, DATA, (key, "maybe")]).unwrap_err();
                assert!(err.contains(words), "{key}: {err}");
            }
        }
        if all_compiled() {
            let c = lookup(&[UP, DATA, ("NOEVIA_FEATURE_CHATGPT_OAUTH", " Off ")]).unwrap();
            assert_eq!(
                c.rust_chatgpt_status.as_ref().unwrap().feature_env,
                Some(false)
            );
            // The previous key never reaches Debug output.
            let c =
                lookup(&[UP, DATA, ("SECRETS_KEY_PREVIOUS", "synthetic-previous-key")]).unwrap();
            assert!(!format!("{c:?}").contains("synthetic-previous-key"));
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod export_tests {
    use super::*;

    fn lookup(pairs: &[(&str, &str)]) -> Result<Config, String> {
        let pairs: Vec<(String, String)> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        Config::from_lookup(|k| pairs.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone()))
    }

    const UP: (&str, &str) = ("NOEVIA_LEGACY_UPSTREAM", "http://127.0.0.1:9");
    const PROJECTS: (&str, &str) = ("NOEVIA_RUST_PROJECTS", "1");
    const DATA: (&str, &str) = ("UI_DATA_DIR", "/data");
    const STORE: (&str, &str) = ("NOEVIA_RUST_PROJECT_STORE", "1");
    const STORE_PORT: (&str, &str) = ("NOEVIA_PROJECT_STORE_PORT", "19021");
    const STORE_TOKEN: (&str, &str) = (
        "NOEVIA_PROJECT_STORE_TOKEN",
        "synthetic-store-token-0123456789abcdef",
    );

    #[test]
    fn project_store_is_exact_default_off_and_needs_projects_port_and_secret() {
        for v in [None, Some("0"), Some("true"), Some(" 1"), Some("")] {
            let mut pairs = vec![UP, PROJECTS, DATA, STORE_PORT, STORE_TOKEN];
            if let Some(v) = v {
                pairs.push(("NOEVIA_RUST_PROJECT_STORE", v));
            }
            assert!(lookup(&pairs).unwrap().project_store.is_none(), "{v:?}");
        }
        let c = lookup(&[UP, PROJECTS, DATA, STORE, STORE_PORT, STORE_TOKEN]).unwrap();
        assert_eq!(c.project_store.as_ref().unwrap().port, 19021);
        assert!(
            !format!("{:?}", c.project_store).contains("synthetic-store-token"),
            "the secret is never printed"
        );
        for (missing, pairs) in [
            (
                "NOEVIA_RUST_PROJECTS=1",
                vec![UP, DATA, STORE, STORE_PORT, STORE_TOKEN],
            ),
            (
                "UI_DATA_DIR",
                vec![UP, PROJECTS, STORE, STORE_PORT, STORE_TOKEN],
            ),
            (
                "NOEVIA_PROJECT_STORE_PORT",
                vec![UP, PROJECTS, DATA, STORE, STORE_TOKEN],
            ),
            (
                "NOEVIA_PROJECT_STORE_TOKEN",
                vec![UP, PROJECTS, DATA, STORE, STORE_PORT],
            ),
            (
                "NOEVIA_PROJECT_STORE_TOKEN",
                vec![
                    UP,
                    PROJECTS,
                    DATA,
                    STORE,
                    STORE_PORT,
                    ("NOEVIA_PROJECT_STORE_TOKEN", "short"),
                ],
            ),
        ] {
            let err = lookup(&pairs).unwrap_err();
            assert!(err.contains(missing), "{missing}: {err}");
        }
    }

    const WRITES: (&str, &str) = ("NOEVIA_RUST_PROJECT_WRITES", "1");

    // #1264: NOEVIA_RUST_PROJECTS=1 (live) never moves the ported writes; only this switch does.
    #[test]
    fn project_writes_switch_is_exact_default_off_and_separate_from_projects() {
        for value in [
            None,
            Some("0"),
            Some("true"),
            Some(" 1"),
            Some("1 "),
            Some(""),
        ] {
            let mut pairs = vec![UP, PROJECTS, DATA];
            if let Some(v) = value {
                pairs.push(("NOEVIA_RUST_PROJECT_WRITES", v));
            }
            let c = lookup(&pairs).unwrap();
            assert!(c.switches().rust_projects);
            assert!(!c.rust_project_writes);
            assert!(!c.switches().on("NOEVIA_RUST_PROJECT_WRITES"));
        }
        assert!(!lookup(&[UP, DATA]).unwrap().rust_project_writes);
    }

    #[test]
    fn project_writes_need_projects_the_compiled_slice_and_a_data_dir() {
        let err = lookup(&[UP, WRITES, DATA]).unwrap_err();
        assert!(err.contains("NOEVIA_RUST_PROJECTS=1"), "{err}");
        let err = lookup(&[UP, WRITES, DATA]).unwrap_err();
        assert!(err.contains("NOEVIA_RUST_PROJECTS=1"), "{err}");
        let enabled = lookup(&[UP, PROJECTS, WRITES, DATA]);
        assert_eq!(
            crate::FEATURES.contains(&"rust-project-writes"),
            cfg!(feature = "rust-project-writes")
        );
        if !cfg!(feature = "rust-project-writes") {
            assert!(enabled
                .unwrap_err()
                .contains("compiled rust-project-writes"));
            return;
        }
        let c = enabled.unwrap();
        assert!(c.rust_project_writes && c.switches().on("NOEVIA_RUST_PROJECT_WRITES"));
        let err = lookup(&[UP, PROJECTS, WRITES]).unwrap_err();
        assert!(err.contains("UI_DATA_DIR"), "{err}");
    }
}
