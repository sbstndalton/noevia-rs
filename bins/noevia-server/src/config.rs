//! Environment, read with the same key names and meanings as core server/index.cjs:
//! UI_HOST (default 0.0.0.0), UI_PORT (default 8021), TRUST_PROXY (exactly "true" enables it),
//! STAMP_VERSION (the /api/ready version fallback). New keys: NOEVIA_LEGACY_UPSTREAM (the Node
//! server, loopback http only) and NOEVIA_WEB_DIST (the built web client, default ./dist, which
//! is /app/dist in the web image, the directory Node serves). COWORK_CODE_NET_ADDR as in core
//! code-net-guard.cjs (see code_net.rs; a malformed value stops startup). UI_DATA_DIR and the auth
//! keys (PUBLIC_ORIGIN, ADDITIONAL_TRUSTED_ORIGINS, UI_AUTH_TOKEN, LEGACY_AUTH_COMPAT,
//! NOEVIA_FEATURE_NATIVE_CLIENT_AUTH) as core index.cjs reads them, for identity.rs; an invalid
//! auth value never stops the front (see identity.rs). NOEVIA_RUST_AUTH=1 (exactly) is the M3
//! deployment switch: Rust owns sign-in and the account and the tables they write (Node refuses
//! those writes with the same switch and NOEVIA_FRONT=rust); any other value is off.

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
    /// NOEVIA_RUST_AUTH=1.
    pub rust_auth: Option<server_store::RustAuth>,
    /// NOEVIA_RUST_PROJECTS=1 (M4); refused without NOEVIA_RUST_AUTH=1, because the front gates the
    /// routes it owns itself and Node's request gate no longer writes session upkeep then.
    pub rust_projects: Option<server_store::RustProjects>,
    /// M6 requires native identity/upkeep.
    pub rust_usage: bool,
    pub rust_storage_reads: bool,
    /// NOEVIA_RUST_STORAGE_BROWSE=1: the outbound storage list/read (#1368).
    pub rust_storage_browse: bool,
    pub rust_reasoning_settings: Option<server_store::RustReasoningSettings>,
    pub rust_routing_mode: Option<server_store::RustRoutingMode>,
    pub routing_feature_env: Option<bool>,
    /// NOEVIA_RUST_ROUTING_DEFAULT=1 (M6, #1305): refused without NOEVIA_RUST_AUTH=1 and without the
    /// compiled rust-routing-default feature; any other value is off.
    pub rust_routing_default: bool,
    /// NOEVIA_RUST_STATS=1 (M6, #1650): the engine GET /api/stats reads, configured from the keys
    /// core index.cjs reads (MODEL_MANAGER_KIND, MODEL_MANAGER_BASE_URL / LEMONADE_BASE_URL /
    /// INFERENCE_BASE_URL, MODEL_MANAGER_API_KEY / INFERENCE_API_KEY / LEMONADE_API_KEY). Refused
    /// without NOEVIA_RUST_AUTH=1 or the compiled rust-stats feature, where Node itself would not
    /// start (an unknown kind, an invalid llama.cpp router URL), for an `https` engine (the front has
    /// no TLS client) and for the Lemonade kind, whose MTP "last response" fallback is kept in
    /// Node's memory; any other value is off and none of those keys is read.
    pub rust_stats: Option<std::sync::Arc<server_stats::Engine>>,
    /// NOEVIA_RUST_EXPORT=1 (M4, sbstndalton/noevia#1315): GET /api/export/conversations, read from
    /// the user's files on disk. Refused without NOEVIA_RUST_PROJECTS=1 (and so NOEVIA_RUST_AUTH=1),
    /// without the compiled rust-export and rust-free-chats features (the free-chat list must be
    /// one Node re-reads from disk, the owner's 2026-10-12 condition) and without UI_DATA_DIR; any
    /// other value is off.
    pub rust_export: bool,
    /// NOEVIA_RUST_PROJECT_WRITES=1 (sbstndalton/noevia#1264): the project writes ported from
    /// Node so far. Refused without NOEVIA_RUST_PROJECTS=1 (they share its projects.json lock and
    /// write switch), without the compiled rust-project-writes feature and without UI_DATA_DIR;
    /// any other value is off.
    pub rust_project_writes: bool,
    /// NOEVIA_RUST_CHATGPT_STATUS=1 (M6, sbstndalton/noevia#1373): GET /api/providers/chatgpt.
    /// Refused without NOEVIA_RUST_AUTH=1, the compiled rust-chatgpt-status feature, UI_DATA_DIR
    /// and valid native auth, or with an invalid NOEVIA_FEATURE_CHATGPT_OAUTH (Node does not start
    /// with one either); any other value is off and none of those keys is read.
    pub rust_chatgpt_status: Option<ChatgptStatus>,
    pub rust_sampling_settings: Option<server_store::RustSamplingSettings>,
    pub usage_clock: Result<server_usage::summary::Clock, server_usage::Fault>,
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

impl Config {
    /// The deployment switches that are on.
    pub fn switches(&self) -> crate::routes::Switches {
        crate::routes::Switches {
            rust_auth: self.rust_auth.is_some(),
            rust_projects: self.rust_projects.is_some(),
            rust_usage: self.rust_usage,
            rust_storage_reads: self.rust_storage_reads,
            rust_storage_browse: self.rust_storage_browse,
            rust_sampling_settings: self.rust_sampling_settings.is_some(),
            rust_reasoning_settings: self.rust_reasoning_settings.is_some(),
            rust_routing_mode: self.rust_routing_mode.is_some(),
            rust_routing_default: self.rust_routing_default,
            rust_stats: self.rust_stats.is_some(),
            rust_export: self.rust_export,
            rust_project_writes: self.rust_project_writes,
            rust_chatgpt_status: self.rust_chatgpt_status.is_some(),
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
        let rust_auth =
            server_store::RustAuth::from_env_value(get(server_store::RustAuth::ENV).as_deref());
        let rust_projects = server_store::RustProjects::from_env_value(
            get(server_store::RustProjects::ENV).as_deref(),
        );
        if rust_projects.is_some() && rust_auth.is_none() {
            return Err("NOEVIA_RUST_PROJECTS=1 needs NOEVIA_RUST_AUTH=1".into());
        }
        let rust_usage = get("NOEVIA_RUST_USAGE").as_deref() == Some("1");
        if rust_usage && rust_auth.is_none() {
            return Err("NOEVIA_RUST_USAGE=1 needs NOEVIA_RUST_AUTH=1".into());
        }
        let rust_storage_reads = get("NOEVIA_RUST_STORAGE_READS").as_deref() == Some("1");
        if rust_storage_reads {
            if rust_auth.is_none() {
                return Err("NOEVIA_RUST_STORAGE_READS=1 needs NOEVIA_RUST_AUTH=1".into());
            }
            if !cfg!(feature = "rust-storage-reads") {
                return Err("NOEVIA_RUST_STORAGE_READS=1 needs compiled rust-storage-reads".into());
            }
            if get("UI_DATA_DIR").is_none_or(|v| v.is_empty())
                || server_auth::AuthConfig::from_lookup(&get).is_err()
            {
                return Err(
                    "NOEVIA_RUST_STORAGE_READS=1 needs UI_DATA_DIR and valid native auth".into(),
                );
            }
        }
        let rust_storage_browse = get("NOEVIA_RUST_STORAGE_BROWSE").as_deref() == Some("1");
        if rust_storage_browse {
            if rust_auth.is_none() {
                return Err("NOEVIA_RUST_STORAGE_BROWSE=1 needs NOEVIA_RUST_AUTH=1".into());
            }
            if !cfg!(feature = "rust-storage-browse") {
                return Err(
                    "NOEVIA_RUST_STORAGE_BROWSE=1 needs compiled rust-storage-browse".into(),
                );
            }
            if get("UI_DATA_DIR").is_none_or(|v| v.is_empty())
                || server_auth::AuthConfig::from_lookup(&get).is_err()
            {
                return Err(
                    "NOEVIA_RUST_STORAGE_BROWSE=1 needs UI_DATA_DIR and valid native auth".into(),
                );
            }
        }
        let rust_sampling_settings =
            if get(server_store::RustSamplingSettings::ENV).as_deref() == Some("1") {
                let auth =
                    rust_auth.ok_or("NOEVIA_RUST_SAMPLING_SETTINGS=1 needs NOEVIA_RUST_AUTH=1")?;
                server_store::RustSamplingSettings::from_env_value(Some("1"), auth)
            } else {
                None
            };
        let rust_reasoning_settings =
            if get(server_store::RustReasoningSettings::ENV).as_deref() == Some("1") {
                let auth =
                    rust_auth.ok_or("NOEVIA_RUST_REASONING_SETTINGS=1 needs NOEVIA_RUST_AUTH=1")?;
                if !cfg!(feature = "rust-reasoning-settings") {
                    return Err(
                        "NOEVIA_RUST_REASONING_SETTINGS=1 needs compiled rust-reasoning-settings"
                            .into(),
                    );
                }
                server_store::RustReasoningSettings::from_env_value(Some("1"), auth)
            } else {
                None
            };
        let rust_routing_mode = if get(server_store::RustRoutingMode::ENV).as_deref() == Some("1") {
            let auth = rust_auth.ok_or("NOEVIA_RUST_ROUTING_MODE=1 needs NOEVIA_RUST_AUTH=1")?;
            if !cfg!(feature = "rust-routing-mode") {
                return Err("NOEVIA_RUST_ROUTING_MODE=1 needs compiled rust-routing-mode".into());
            }
            server_store::RustRoutingMode::from_env_value(Some("1"), auth)
        } else {
            None
        };
        let rust_routing_default = if get("NOEVIA_RUST_ROUTING_DEFAULT").as_deref() == Some("1") {
            if rust_auth.is_none() {
                return Err("NOEVIA_RUST_ROUTING_DEFAULT=1 needs NOEVIA_RUST_AUTH=1".into());
            }
            if !cfg!(feature = "rust-routing-default") {
                return Err(
                    "NOEVIA_RUST_ROUTING_DEFAULT=1 needs compiled rust-routing-default".into(),
                );
            }
            true
        } else {
            false
        };
        let rust_stats = if get("NOEVIA_RUST_STATS").as_deref() == Some("1") {
            if rust_auth.is_none() {
                return Err("NOEVIA_RUST_STATS=1 needs NOEVIA_RUST_AUTH=1".into());
            }
            if !cfg!(feature = "rust-stats") {
                return Err("NOEVIA_RUST_STATS=1 needs compiled rust-stats".into());
            }
            let engine = server_stats::Engine::from_lookup(&get)
                .map_err(|e| format!("NOEVIA_RUST_STATS=1: {e}"))?;
            if engine.kind() == server_stats::Kind::Lemonade {
                return Err("NOEVIA_RUST_STATS=1 is not available with the Lemonade model manager: its MTP last-response fallback is recorded in Node".into());
            }
            Some(std::sync::Arc::new(engine))
        } else {
            None
        };
        let rust_export = if get("NOEVIA_RUST_EXPORT").as_deref() == Some("1") {
            if rust_projects.is_none() {
                return Err("NOEVIA_RUST_EXPORT=1 needs NOEVIA_RUST_PROJECTS=1".into());
            }
            if !cfg!(feature = "rust-export") {
                return Err("NOEVIA_RUST_EXPORT=1 needs compiled rust-export".into());
            }
            if !cfg!(feature = "rust-free-chats") {
                return Err("NOEVIA_RUST_EXPORT=1 needs compiled rust-free-chats".into());
            }
            if get("UI_DATA_DIR").filter(|d| !d.is_empty()).is_none() {
                return Err("NOEVIA_RUST_EXPORT=1 needs UI_DATA_DIR".into());
            }
            true
        } else {
            false
        };
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
        let rust_chatgpt_status = if get("NOEVIA_RUST_CHATGPT_STATUS").as_deref() == Some("1") {
            if rust_auth.is_none() {
                return Err("NOEVIA_RUST_CHATGPT_STATUS=1 needs NOEVIA_RUST_AUTH=1".into());
            }
            if !cfg!(feature = "rust-chatgpt-status") {
                return Err(
                    "NOEVIA_RUST_CHATGPT_STATUS=1 needs compiled rust-chatgpt-status".into(),
                );
            }
            if get("UI_DATA_DIR").is_none_or(|v| v.is_empty())
                || server_auth::AuthConfig::from_lookup(&get).is_err()
            {
                return Err(
                    "NOEVIA_RUST_CHATGPT_STATUS=1 needs UI_DATA_DIR and valid native auth".into(),
                );
            }
            let feature_env =
                server_routing_mode::feature_env(get("NOEVIA_FEATURE_CHATGPT_OAUTH").as_deref())
                    .map_err(|e| format!("NOEVIA_FEATURE_CHATGPT_OAUTH: {}", e.message))?;
            Some(ChatgptStatus {
                feature_env,
                previous_key_env: RedactedEnv(get("SECRETS_KEY_PREVIOUS")),
            })
        } else {
            None
        };
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
            auth: server_auth::AuthConfig::from_lookup(&get),
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
            rust_chatgpt_status,
            usage_clock: server_usage::summary::Clock::system(&get),
        })
    }
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

    #[test]
    fn node_defaults_and_names() {
        let c = cfg(&[("NOEVIA_LEGACY_UPSTREAM", "http://127.0.0.1:9021")]).unwrap();
        assert_eq!(
            (c.host.as_str(), c.port, c.trust_proxy),
            ("0.0.0.0", 8021, false)
        );
        let c = cfg(&[
            ("NOEVIA_LEGACY_UPSTREAM", "http://127.0.0.1:9021/"),
            ("UI_HOST", "127.0.0.1"),
            ("UI_PORT", "18021"),
            ("TRUST_PROXY", "true"),
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
        ])
        .unwrap();
        assert!(!c.trust_proxy);
        assert_eq!(c.upstream.authority(), "[::1]:9021");
        assert!(c.rust_auth.is_none());
        for (v, on) in [
            ("1", true),
            ("true", false),
            ("0", false),
            ("", false),
            (" 1", false),
        ] {
            let c = cfg(&[
                ("NOEVIA_LEGACY_UPSTREAM", "http://127.0.0.1:9021"),
                ("NOEVIA_RUST_AUTH", v),
            ])
            .unwrap();
            assert_eq!(c.rust_auth.is_some(), on, "{v:?}");
            assert_eq!(c.switches().rust_auth, on);
        }
    }

    #[test]
    fn rust_projects_needs_rust_auth() {
        let up = ("NOEVIA_LEGACY_UPSTREAM", "http://127.0.0.1:9021");
        let c = cfg(&[up, ("NOEVIA_RUST_AUTH", "1"), ("NOEVIA_RUST_PROJECTS", "1")]).unwrap();
        assert!(c.switches().rust_auth && c.switches().rust_projects);
        assert!(cfg(&[up, ("NOEVIA_RUST_PROJECTS", "1")]).is_err());
        for v in ["", "0", "true"] {
            let c = cfg(&[up, ("NOEVIA_RUST_AUTH", "1"), ("NOEVIA_RUST_PROJECTS", v)]).unwrap();
            assert!(!c.switches().rust_projects, "{v:?}");
            assert!(cfg(&[up, ("NOEVIA_RUST_PROJECTS", v)]).is_ok(), "{v:?}");
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
            assert!(cfg(&[("NOEVIA_LEGACY_UPSTREAM", bad)]).is_err(), "{bad}");
        }
        assert!(cfg(&[]).is_err());
        assert!(cfg(&[("NOEVIA_LEGACY_UPSTREAM", "http://127.0.0.1:8021")]).is_err());
        assert!(cfg(&[
            ("NOEVIA_LEGACY_UPSTREAM", "http://127.0.0.1:9021"),
            ("UI_PORT", "x")
        ])
        .is_err());
    }
    #[test]
    fn usage_needs_native_auth_and_defaults_off() {
        assert!(
            !cfg(&[("NOEVIA_LEGACY_UPSTREAM", "http://127.0.0.1:9021")])
                .unwrap()
                .rust_usage
        );
        assert!(cfg(&[
            ("NOEVIA_LEGACY_UPSTREAM", "http://127.0.0.1:9021"),
            ("NOEVIA_RUST_USAGE", "1")
        ])
        .is_err());
        let c = cfg(&[
            ("NOEVIA_LEGACY_UPSTREAM", "http://127.0.0.1:9021"),
            ("NOEVIA_RUST_USAGE", "1"),
            ("NOEVIA_RUST_AUTH", "1"),
        ])
        .unwrap();
        assert!(c.switches().rust_usage);
    }
    #[test]
    fn sampling_needs_native_auth_and_exact_switch_defaults_off() {
        for value in [None, Some("0"), Some("true"), Some(" 1")] {
            let mut env = vec![
                ("NOEVIA_LEGACY_UPSTREAM", "http://127.0.0.1:9021"),
                ("NOEVIA_RUST_AUTH", "1"),
            ];
            if let Some(value) = value {
                env.push(("NOEVIA_RUST_SAMPLING_SETTINGS", value));
            }
            assert!(cfg(&env).unwrap().rust_sampling_settings.is_none());
        }
        assert!(cfg(&[
            ("NOEVIA_LEGACY_UPSTREAM", "http://127.0.0.1:9021"),
            ("NOEVIA_RUST_SAMPLING_SETTINGS", "1")
        ])
        .is_err());
        assert!(
            cfg(&[
                ("NOEVIA_LEGACY_UPSTREAM", "http://127.0.0.1:9021"),
                ("NOEVIA_RUST_AUTH", "1"),
                ("NOEVIA_RUST_SAMPLING_SETTINGS", "1")
            ])
            .unwrap()
            .switches()
            .rust_sampling_settings
        );
    }
    #[test]
    fn reasoning_requires_exact_selector_auth_and_compiled_capability() {
        let base = [("NOEVIA_LEGACY_UPSTREAM", "http://127.0.0.1:9")];
        for value in ["", "0", "true", "01"] {
            let mut env = base.to_vec();
            env.push(("NOEVIA_RUST_REASONING_SETTINGS", value));
            assert!(cfg(&env).unwrap().rust_reasoning_settings.is_none());
        }
        assert!(cfg(&[
            ("NOEVIA_LEGACY_UPSTREAM", "http://127.0.0.1:9"),
            ("NOEVIA_RUST_REASONING_SETTINGS", "1")
        ])
        .is_err());
        let enabled = cfg(&[
            ("NOEVIA_LEGACY_UPSTREAM", "http://127.0.0.1:9"),
            ("NOEVIA_RUST_AUTH", "1"),
            ("NOEVIA_RUST_REASONING_SETTINGS", "1"),
        ]);
        if cfg!(feature = "rust-reasoning-settings") {
            assert!(enabled.unwrap().rust_reasoning_settings.is_some());
        } else {
            assert!(enabled
                .unwrap_err()
                .contains("compiled rust-reasoning-settings"));
        }
        assert_eq!(
            crate::FEATURES.contains(&"rust-reasoning-settings"),
            cfg!(feature = "rust-reasoning-settings")
        );
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod routing_tests {
    use super::*;
    #[test]
    fn routing_switch_is_exact_auth_required_compiled_and_product_independent() {
        for selector in [None, Some("0"), Some("true"), Some(" 1"), Some("1 ")] {
            let c = Config::from_lookup(|k| match k {
                "NOEVIA_LEGACY_UPSTREAM" => Some("http://127.0.0.1:9".into()),
                "NOEVIA_RUST_ROUTING_MODE" => selector.map(str::to_owned),
                _ => None,
            })
            .unwrap();
            assert!(c.rust_routing_mode.is_none());
        }
        assert!(Config::from_lookup(|k| match k {
            "NOEVIA_LEGACY_UPSTREAM" => Some("http://127.0.0.1:9".into()),
            "NOEVIA_RUST_ROUTING_MODE" => Some("1".into()),
            _ => None,
        })
        .is_err());
        let enabled = Config::from_lookup(|k| match k {
            "NOEVIA_LEGACY_UPSTREAM" => Some("http://127.0.0.1:9".into()),
            "NOEVIA_RUST_ROUTING_MODE" | "NOEVIA_RUST_AUTH" => Some("1".into()),
            _ => None,
        });
        if cfg!(feature = "rust-routing-mode") {
            assert!(enabled.unwrap().rust_routing_mode.is_some());
        } else {
            assert!(enabled.unwrap_err().contains("compiled rust-routing-mode"));
        }
        assert_eq!(
            crate::FEATURES.contains(&"rust-routing-mode"),
            cfg!(feature = "rust-routing-mode")
        );
        let invalid = Config::from_lookup(|k| match k {
            "NOEVIA_LEGACY_UPSTREAM" => Some("http://127.0.0.1:9".into()),
            "NOEVIA_RUST_ROUTING_MODE" | "NOEVIA_RUST_AUTH" => Some("1".into()),
            "NOEVIA_FEATURE_ROUTING_MODES" => Some("not-a-boolean".into()),
            _ => None,
        });
        assert!(invalid.is_err());
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod routing_default_tests {
    use super::*;

    fn lookup(pairs: &[(&str, &str)]) -> Result<Config, String> {
        let pairs: Vec<(String, String)> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        Config::from_lookup(|k| pairs.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone()))
    }

    #[test]
    fn routing_default_switch_is_exact_auth_required_and_compiled() {
        const UP: (&str, &str) = ("NOEVIA_LEGACY_UPSTREAM", "http://127.0.0.1:9");
        for value in [None, Some("0"), Some("true"), Some(" 1"), Some("1 ")] {
            let mut pairs = vec![UP];
            if let Some(v) = value {
                pairs.push(("NOEVIA_RUST_ROUTING_DEFAULT", v));
            }
            let c = lookup(&pairs).unwrap();
            assert!(!c.rust_routing_default);
            assert!(!c.switches().on("NOEVIA_RUST_ROUTING_DEFAULT"));
        }
        // Switched on without NOEVIA_RUST_AUTH is a startup refusal, never a silent Node answer.
        assert!(lookup(&[UP, ("NOEVIA_RUST_ROUTING_DEFAULT", "1")])
            .unwrap_err()
            .contains("NOEVIA_RUST_AUTH=1"));
        let enabled = lookup(&[
            UP,
            ("NOEVIA_RUST_AUTH", "1"),
            ("NOEVIA_RUST_ROUTING_DEFAULT", "1"),
        ]);
        if cfg!(feature = "rust-routing-default") {
            let c = enabled.unwrap();
            assert!(c.rust_routing_default);
            assert!(c.switches().on("NOEVIA_RUST_ROUTING_DEFAULT"));
        } else {
            assert!(enabled
                .unwrap_err()
                .contains("compiled rust-routing-default"));
        }
        assert_eq!(
            crate::FEATURES.contains(&"rust-routing-default"),
            cfg!(feature = "rust-routing-default")
        );
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod stats_tests {
    use super::*;

    fn lookup(pairs: &[(&str, &str)]) -> Result<Config, String> {
        let pairs: Vec<(String, String)> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        Config::from_lookup(|k| pairs.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone()))
    }

    const UP: (&str, &str) = ("NOEVIA_LEGACY_UPSTREAM", "http://127.0.0.1:9");
    const AUTH: (&str, &str) = ("NOEVIA_RUST_AUTH", "1");
    const ON: (&str, &str) = ("NOEVIA_RUST_STATS", "1");

    #[test]
    fn stats_switch_is_exact_default_off_and_reads_nothing_while_off() {
        for value in [None, Some("0"), Some("true"), Some(" 1"), Some("1 ")] {
            let mut pairs = vec![UP, AUTH, ("MODEL_MANAGER_KIND", "not-a-kind")];
            if let Some(v) = value {
                pairs.push(("NOEVIA_RUST_STATS", v));
            }
            // Off, an engine configuration Node would refuse is not the front's business.
            let c = lookup(&pairs).unwrap();
            assert!(c.rust_stats.is_none());
            assert!(!c.switches().on("NOEVIA_RUST_STATS"));
        }
    }

    #[test]
    fn stats_needs_auth_the_compiled_slice_and_an_engine_it_can_read() {
        assert!(lookup(&[UP, ON])
            .unwrap_err()
            .contains("NOEVIA_RUST_AUTH=1"));
        let enabled = lookup(&[
            UP,
            AUTH,
            ON,
            ("MODEL_MANAGER_KIND", "llamacpp"),
            ("MODEL_MANAGER_BASE_URL", "http://router:8080/v1"),
        ]);
        assert_eq!(
            crate::FEATURES.contains(&"rust-stats"),
            cfg!(feature = "rust-stats")
        );
        if !cfg!(feature = "rust-stats") {
            assert!(enabled.unwrap_err().contains("compiled rust-stats"));
            return;
        }
        let c = enabled.unwrap();
        assert!(c.switches().on("NOEVIA_RUST_STATS"));
        assert_eq!(
            c.rust_stats.as_ref().unwrap().kind(),
            server_stats::Kind::LlamaCpp
        );
        // No engine configured at all is the `none` kind, as in Node.
        let c = lookup(&[UP, AUTH, ON]).unwrap();
        assert_eq!(
            c.rust_stats.as_ref().unwrap().kind(),
            server_stats::Kind::None
        );
        for (pairs, words) in [
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
        ] {
            let mut all = vec![UP, AUTH, ON];
            all.extend(pairs);
            let err = lookup(&all).unwrap_err();
            assert!(err.contains(words), "{err}");
            assert!(err.starts_with("NOEVIA_RUST_STATS=1"), "{err}");
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
    const AUTH: (&str, &str) = ("NOEVIA_RUST_AUTH", "1");
    const PROJECTS: (&str, &str) = ("NOEVIA_RUST_PROJECTS", "1");
    const DATA: (&str, &str) = ("UI_DATA_DIR", "/data");
    const ON: (&str, &str) = ("NOEVIA_RUST_EXPORT", "1");

    #[test]
    fn export_switch_is_exact_and_default_off() {
        for value in [
            None,
            Some("0"),
            Some("true"),
            Some(" 1"),
            Some("1 "),
            Some(""),
        ] {
            let mut pairs = vec![UP, AUTH, PROJECTS, DATA];
            if let Some(v) = value {
                pairs.push(("NOEVIA_RUST_EXPORT", v));
            }
            let c = lookup(&pairs).unwrap();
            assert!(!c.rust_export);
            assert!(!c.switches().on("NOEVIA_RUST_EXPORT"));
        }
        // Off, nothing it needs is required.
        assert!(!lookup(&[UP]).unwrap().rust_export);
    }

    #[test]
    fn export_needs_projects_the_compiled_slices_and_a_data_dir() {
        let err = lookup(&[UP, ON, DATA]).unwrap_err();
        assert!(err.contains("NOEVIA_RUST_PROJECTS=1"), "{err}");
        let err = lookup(&[UP, AUTH, ON, DATA]).unwrap_err();
        assert!(err.contains("NOEVIA_RUST_PROJECTS=1"), "{err}");
        let enabled = lookup(&[UP, AUTH, PROJECTS, ON, DATA]);
        assert_eq!(
            crate::FEATURES.contains(&"rust-export"),
            cfg!(feature = "rust-export")
        );
        if !cfg!(feature = "rust-export") {
            assert!(enabled.unwrap_err().contains("compiled rust-export"));
            return;
        }
        if !cfg!(feature = "rust-free-chats") {
            assert!(enabled.unwrap_err().contains("compiled rust-free-chats"));
            return;
        }
        let c = enabled.unwrap();
        assert!(c.rust_export && c.switches().on("NOEVIA_RUST_EXPORT"));
        let err = lookup(&[UP, AUTH, PROJECTS, ON]).unwrap_err();
        assert!(err.contains("UI_DATA_DIR"), "{err}");
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
            let mut pairs = vec![UP, AUTH, PROJECTS, DATA];
            if let Some(v) = value {
                pairs.push(("NOEVIA_RUST_PROJECT_WRITES", v));
            }
            let c = lookup(&pairs).unwrap();
            assert!(c.switches().rust_projects);
            assert!(!c.rust_project_writes);
            assert!(!c.switches().on("NOEVIA_RUST_PROJECT_WRITES"));
        }
        assert!(!lookup(&[UP]).unwrap().rust_project_writes);
    }

    #[test]
    fn project_writes_need_projects_the_compiled_slice_and_a_data_dir() {
        let err = lookup(&[UP, WRITES, DATA]).unwrap_err();
        assert!(err.contains("NOEVIA_RUST_PROJECTS=1"), "{err}");
        let err = lookup(&[UP, AUTH, WRITES, DATA]).unwrap_err();
        assert!(err.contains("NOEVIA_RUST_PROJECTS=1"), "{err}");
        let enabled = lookup(&[UP, AUTH, PROJECTS, WRITES, DATA]);
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
        let err = lookup(&[UP, AUTH, PROJECTS, WRITES]).unwrap_err();
        assert!(err.contains("UI_DATA_DIR"), "{err}");
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod chatgpt_status_tests {
    use super::*;

    fn lookup(pairs: &[(&str, &str)]) -> Result<Config, String> {
        let pairs: Vec<(String, String)> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        Config::from_lookup(|k| pairs.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone()))
    }

    const UP: (&str, &str) = ("NOEVIA_LEGACY_UPSTREAM", "http://127.0.0.1:9");
    const AUTH: (&str, &str) = ("NOEVIA_RUST_AUTH", "1");
    const DATA: (&str, &str) = ("UI_DATA_DIR", "/data");
    const ON: (&str, &str) = ("NOEVIA_RUST_CHATGPT_STATUS", "1");

    #[test]
    fn chatgpt_status_switch_is_exact_and_default_off() {
        for value in [
            None,
            Some("0"),
            Some("true"),
            Some(" 1"),
            Some("1 "),
            Some(""),
        ] {
            let mut pairs = vec![UP, AUTH, DATA];
            if let Some(v) = value {
                pairs.push(("NOEVIA_RUST_CHATGPT_STATUS", v));
            }
            let c = lookup(&pairs).unwrap();
            assert!(c.rust_chatgpt_status.is_none());
            assert!(!c.switches().on("NOEVIA_RUST_CHATGPT_STATUS"));
        }
        // Off, an invalid product flag is not this switch's business.
        assert!(lookup(&[UP, ("NOEVIA_FEATURE_CHATGPT_OAUTH", "maybe")]).is_ok());
    }

    #[test]
    fn chatgpt_status_needs_auth_the_build_a_data_dir_and_a_valid_flag() {
        let err = lookup(&[UP, ON, DATA]).unwrap_err();
        assert!(err.contains("NOEVIA_RUST_AUTH=1"), "{err}");
        let enabled = lookup(&[UP, AUTH, ON, DATA]);
        assert_eq!(
            crate::FEATURES.contains(&"rust-chatgpt-status"),
            cfg!(feature = "rust-chatgpt-status")
        );
        if !cfg!(feature = "rust-chatgpt-status") {
            assert!(enabled
                .unwrap_err()
                .contains("compiled rust-chatgpt-status"));
            return;
        }
        let c = enabled.unwrap();
        assert!(c.switches().on("NOEVIA_RUST_CHATGPT_STATUS"));
        assert_eq!(c.rust_chatgpt_status.as_ref().unwrap().feature_env, None);
        let err = lookup(&[UP, AUTH, ON]).unwrap_err();
        assert!(err.contains("UI_DATA_DIR"), "{err}");
        let c = lookup(&[
            UP,
            AUTH,
            ON,
            DATA,
            ("NOEVIA_FEATURE_CHATGPT_OAUTH", " Off "),
        ])
        .unwrap();
        assert_eq!(
            c.rust_chatgpt_status.as_ref().unwrap().feature_env,
            Some(false)
        );
        let err = lookup(&[
            UP,
            AUTH,
            ON,
            DATA,
            ("NOEVIA_FEATURE_CHATGPT_OAUTH", "maybe"),
        ])
        .unwrap_err();
        assert!(err.contains("NOEVIA_FEATURE_CHATGPT_OAUTH"), "{err}");
        // The previous key never reaches Debug output.
        let c = lookup(&[
            UP,
            AUTH,
            ON,
            DATA,
            ("SECRETS_KEY_PREVIOUS", "synthetic-previous-key"),
        ])
        .unwrap();
        assert!(!format!("{c:?}").contains("synthetic-previous-key"));
    }
}
