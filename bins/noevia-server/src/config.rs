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
    pub rust_reasoning_settings: Option<server_store::RustReasoningSettings>,
    pub rust_routing_mode: Option<server_store::RustRoutingMode>,
    pub routing_feature_env: Option<bool>,
    /// NOEVIA_RUST_ROUTING_DEFAULT=1 (M6, #1305): refused without NOEVIA_RUST_AUTH=1 and without the
    /// compiled rust-routing-default feature; any other value is off.
    pub rust_routing_default: bool,
    pub rust_sampling_settings: Option<server_store::RustSamplingSettings>,
    pub usage_clock: Result<server_usage::summary::Clock, server_usage::Fault>,
    /// What the account routes read (PUBLIC_ORIGIN, WEBAUTHN_RP_ID, TRUST_PROXY, COWORK_DAV_PORT,
    /// UI_DATA_DIR); `None` without UI_DATA_DIR.
    pub account: Option<server_account::Settings>,
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
            rust_sampling_settings: self.rust_sampling_settings.is_some(),
            rust_reasoning_settings: self.rust_reasoning_settings.is_some(),
            rust_routing_mode: self.rust_routing_mode.is_some(),
            rust_routing_default: self.rust_routing_default,
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
            rust_sampling_settings,
            rust_reasoning_settings,
            rust_routing_mode,
            routing_feature_env,
            rust_routing_default,
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
