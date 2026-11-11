//! The routing table: `[router]`, `[defaults]` and `[[models]]` of a TOML file compatible with the
//! prototype's `router.toml`. `{dir}` in `cmd`, `stop`, `cwd` and `gpu_lock` is the config file's
//! folder; a leading `~/` in `gpu_lock` and `logdir` is `$HOME`.

use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Container names the router must never stop or remove: production's, whatever a config says.
const PROTECTED_PREFIXES: [&str; 1] = ["noevia-"];
const PROTECTED_INFIXES: [&str; 1] = ["inference"];

#[derive(Debug)]
pub struct ConfigError(pub String);

impl std::fmt::Display for ConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for ConfigError {}

fn err<T>(message: impl Into<String>) -> Result<T, ConfigError> {
    Err(ConfigError(message.into()))
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawRouter {
    host: Option<String>,
    port: Option<u16>,
    logdir: Option<String>,
    allow_non_loopback: Option<bool>,
    gpu_lock: Option<String>,
    gpu_lock_timeout: Option<f64>,
    docker: Option<String>,
    max_body_mb: Option<u64>,
    shutdown_grace: Option<f64>,
}

/// One `[[models]]` entry, or `[defaults]` (every key optional there).
#[derive(Debug, Default, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawModel {
    name: Option<String>,
    engine: Option<String>,
    cmd: Option<String>,
    proxy: Option<String>,
    health: Option<String>,
    health_expect: Option<String>,
    stop: Option<String>,
    containers: Option<Vec<String>>,
    aliases: Option<Vec<String>>,
    upstream_model: Option<String>,
    ready_timeout: Option<f64>,
    stop_timeout: Option<f64>,
    ttl: Option<f64>,
    env: Option<BTreeMap<String, toml::Value>>,
    cwd: Option<String>,
    enabled: Option<bool>,
    unlisted: Option<bool>,
    notes: Option<String>,
    devices: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawConfig {
    #[serde(default)]
    router: RawRouter,
    #[serde(default)]
    defaults: RawModel,
    #[serde(default)]
    models: Vec<RawModel>,
}

/// One routable model: how to start it, reach it, check it and stop it.
#[derive(Debug, Clone)]
pub struct Model {
    pub name: String,
    /// `llamacpp`, `strata`, ...: informational; routing is by name.
    pub engine: String,
    /// Start command, run by `sh -c` in its own process group; it must stay in the foreground.
    pub cmd: String,
    /// Upstream base URL, e.g. `http://127.0.0.1:18190`.
    pub proxy: String,
    /// Health path, or `None` for `health = "none"` (ready as soon as it is started).
    pub health: Option<String>,
    /// Substring the health body must contain (empty: any 200).
    pub health_expect: String,
    /// Optional stop command, run before the process group is signalled. `{pid}` is the process
    /// group id.
    pub stop: String,
    /// Docker containers that must be verified gone after a stop.
    pub containers: Vec<String>,
    pub aliases: Vec<String>,
    /// Rewrite the request's `model` to this (empty: leave it).
    pub upstream_model: String,
    pub ready_timeout: Duration,
    pub stop_timeout: Duration,
    /// Idle time before an unload; `None` never unloads.
    pub ttl: Option<Duration>,
    pub env: Vec<(String, String)>,
    pub cwd: PathBuf,
    /// Hidden from `GET /v1/models`, still routable.
    pub unlisted: bool,
    pub notes: String,
    pub devices: String,
}

#[derive(Debug, Clone)]
pub struct Config {
    pub host: String,
    pub port: u16,
    pub logdir: PathBuf,
    pub allow_non_loopback: bool,
    /// A file flocked (exclusive) for as long as a backend may be running.
    pub gpu_lock: Option<PathBuf>,
    pub gpu_lock_timeout: Duration,
    /// The docker CLI used to verify and remove containers.
    pub docker: String,
    pub max_body_bytes: usize,
    /// How long in-flight requests may run after SIGTERM before the backend is stopped anyway.
    pub shutdown_grace: Duration,
    /// Models in file order (disabled entries dropped).
    pub models: Vec<Model>,
    /// Every name and alias -> index into `models`.
    pub names: BTreeMap<String, usize>,
}

impl Config {
    pub fn resolve(&self, name: &str) -> Option<&Model> {
        self.names.get(name).and_then(|i| self.models.get(*i))
    }

    pub fn load(path: &Path) -> Result<Config, ConfigError> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| ConfigError(format!("{}: {e}", path.display())))?;
        let base = std::fs::canonicalize(path)
            .ok()
            .and_then(|p| p.parent().map(Path::to_path_buf))
            .unwrap_or_else(|| PathBuf::from("."));
        Config::parse(&text, &base)
    }

    /// Parse a config whose `{dir}` is `base`.
    pub fn parse(text: &str, base: &Path) -> Result<Config, ConfigError> {
        let raw: RawConfig = toml::from_str(text).map_err(|e| ConfigError(e.to_string()))?;
        let dir = base.to_string_lossy().into_owned();
        let sub = |s: &str| s.replace("{dir}", &dir);
        let r = raw.router;
        let mut models = Vec::new();
        let mut names = BTreeMap::new();
        for m in raw.models {
            let d = &raw.defaults;
            let name = m.name.clone().unwrap_or_default();
            if name.trim().is_empty() {
                return err("a [[models]] entry has no name");
            }
            if !m.enabled.or(d.enabled).unwrap_or(true) {
                continue;
            }
            let pick = |a: &Option<String>, b: &Option<String>| a.clone().or_else(|| b.clone());
            let cmd = pick(&m.cmd, &d.cmd).unwrap_or_default();
            if cmd.trim().is_empty() {
                return err(format!("model {name:?}: no cmd"));
            }
            let proxy = pick(&m.proxy, &d.proxy).unwrap_or_default();
            match url::Url::parse(&proxy) {
                Ok(u) if u.scheme() == "http" && u.host_str().is_some() => {}
                _ => {
                    return err(format!(
                        "model {name:?}: proxy {proxy:?} is not an http:// URL"
                    ))
                }
            }
            let health = pick(&m.health, &d.health).unwrap_or_else(|| "/health".into());
            let health = if health.eq_ignore_ascii_case("none") {
                None
            } else if health.starts_with('/') {
                Some(health)
            } else {
                return err(format!(
                    "model {name:?}: health {health:?} must start with / or be \"none\""
                ));
            };
            let secs = |key: &str, v: Option<f64>, default: f64, zero_ok: bool| {
                let v = v.unwrap_or(default);
                if !v.is_finite() || v < 0.0 || (!zero_ok && v == 0.0) {
                    return err(format!("model {name:?}: {key} = {v} is out of range"));
                }
                Duration::try_from_secs_f64(v)
                    .map_err(|e| ConfigError(format!("model {name:?}: {key}: {e}")))
            };
            let ready_timeout = secs(
                "ready_timeout",
                m.ready_timeout.or(d.ready_timeout),
                600.0,
                false,
            )?;
            let stop_timeout = secs(
                "stop_timeout",
                m.stop_timeout.or(d.stop_timeout),
                60.0,
                false,
            )?;
            let ttl = secs("ttl", m.ttl.or(d.ttl), 0.0, true)?;
            let containers = m
                .containers
                .clone()
                .or_else(|| d.containers.clone())
                .unwrap_or_default();
            for c in &containers {
                if c.is_empty()
                    || !c
                        .chars()
                        .all(|ch| ch.is_ascii_alphanumeric() || "_.-".contains(ch))
                {
                    return err(format!(
                        "model {name:?}: container name {c:?} is not a plain docker name"
                    ));
                }
                if PROTECTED_PREFIXES.iter().any(|p| c.starts_with(p))
                    || PROTECTED_INFIXES.iter().any(|p| c.contains(p))
                {
                    return err(format!("model {name:?}: container {c:?} looks like a production container; the router never removes those"));
                }
            }
            let mut env = Vec::new();
            for (k, v) in d.env.iter().flatten().chain(m.env.iter().flatten()) {
                let v = match v {
                    toml::Value::String(s) => s.clone(),
                    other => other.to_string(),
                };
                env.retain(|(ek, _): &(String, String)| ek != k);
                env.push((k.clone(), v));
            }
            let cwd = pick(&m.cwd, &d.cwd)
                .filter(|c| !c.is_empty())
                .map(|c| sub(&c))
                .unwrap_or_else(|| dir.clone());
            let model = Model {
                name: name.clone(),
                engine: pick(&m.engine, &d.engine).unwrap_or_default(),
                cmd: sub(&cmd),
                proxy: proxy.trim_end_matches('/').to_string(),
                health,
                health_expect: pick(&m.health_expect, &d.health_expect).unwrap_or_default(),
                stop: sub(&pick(&m.stop, &d.stop).unwrap_or_default()),
                containers,
                aliases: m
                    .aliases
                    .clone()
                    .or_else(|| d.aliases.clone())
                    .unwrap_or_default(),
                upstream_model: pick(&m.upstream_model, &d.upstream_model).unwrap_or_default(),
                ready_timeout,
                stop_timeout,
                ttl: (!ttl.is_zero()).then_some(ttl),
                env,
                cwd: PathBuf::from(cwd),
                unlisted: m.unlisted.or(d.unlisted).unwrap_or(false),
                notes: pick(&m.notes, &d.notes).unwrap_or_default(),
                devices: pick(&m.devices, &d.devices).unwrap_or_default(),
            };
            let index = models.len();
            for n in std::iter::once(&model.name).chain(model.aliases.iter()) {
                if n.trim().is_empty() {
                    return err(format!("model {name:?}: empty alias"));
                }
                if names.insert(n.clone(), index).is_some() {
                    return err(format!("model name {n:?} used twice"));
                }
            }
            models.push(model);
        }
        let home = |s: String| match (s.strip_prefix("~/"), std::env::var_os("HOME")) {
            (Some(rest), Some(h)) => PathBuf::from(h).join(rest),
            _ => PathBuf::from(s),
        };
        let lock_timeout = r.gpu_lock_timeout.unwrap_or(30.0);
        let grace = r.shutdown_grace.unwrap_or(10.0);
        let gpu_lock_timeout = Duration::try_from_secs_f64(lock_timeout)
            .map_err(|e| ConfigError(format!("gpu_lock_timeout: {e}")))?;
        let shutdown_grace = Duration::try_from_secs_f64(grace)
            .map_err(|e| ConfigError(format!("shutdown_grace: {e}")))?;
        let max_body_mb = r.max_body_mb.unwrap_or(64);
        Ok(Config {
            host: r.host.unwrap_or_else(|| "127.0.0.1".into()),
            port: r.port.unwrap_or(18090),
            logdir: r
                .logdir
                .map(|l| home(sub(&l)))
                .unwrap_or_else(|| base.join("logs")),
            allow_non_loopback: r.allow_non_loopback.unwrap_or(false),
            gpu_lock: r.gpu_lock.filter(|g| !g.is_empty()).map(|g| home(sub(&g))),
            gpu_lock_timeout,
            docker: r.docker.unwrap_or_else(|| "docker".into()),
            max_body_bytes: usize::try_from(max_body_mb.saturating_mul(1 << 20))
                .unwrap_or(usize::MAX),
            shutdown_grace,
            models,
            names,
        })
    }
}

/// Loopback hosts the router may bind without `allow_non_loopback`.
pub fn is_loopback(host: &str) -> bool {
    matches!(host, "127.0.0.1" | "localhost" | "::1")
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::indexing_slicing)]
    use super::*;

    fn parse(t: &str) -> Result<Config, ConfigError> {
        Config::parse(t, Path::new("/cfg"))
    }

    const ONE: &str = r#"
[defaults]
ready_timeout = 20
[[models]]
name = "a"
aliases = ["x"]
cmd = "exec {dir}/run.sh"
proxy = "http://127.0.0.1:9/"
"#;

    #[test]
    fn defaults_dir_and_aliases() {
        let c = parse(ONE).unwrap();
        let m = c.resolve("x").unwrap();
        assert_eq!(m.name, "a");
        assert_eq!(m.cmd, "exec /cfg/run.sh");
        assert_eq!(m.proxy, "http://127.0.0.1:9");
        assert_eq!(m.health.as_deref(), Some("/health"));
        assert_eq!(m.ready_timeout, Duration::from_secs(20));
        assert_eq!(m.stop_timeout, Duration::from_secs(60));
        assert_eq!(m.ttl, None);
        assert_eq!(c.logdir, PathBuf::from("/cfg/logs"));
        assert_eq!(c.port, 18090);
    }

    #[test]
    fn rejects_unknown_keys_collisions_and_production_containers() {
        assert!(parse(&format!("{ONE}bogus = 1\n"))
            .unwrap_err()
            .0
            .contains("bogus"));
        let dup = format!(
            "{ONE}[[models]]\nname = \"x\"\ncmd = \"true\"\nproxy = \"http://127.0.0.1:1\"\n"
        );
        assert!(parse(&dup).unwrap_err().0.contains("used twice"));
        for c in ["noevia-inference-llama-1", "my-inference"] {
            let t = format!("{ONE}containers = [\"{c}\"]\n");
            assert!(parse(&t).unwrap_err().0.contains("production"), "{c}");
        }
        assert!(parse(&format!("{ONE}containers = [\"a;rm\"]\n")).is_err());
        assert!(parse(&format!("{ONE}ready_timeout = 0\n")).is_err());
        assert!(parse(&ONE.replace("http://127.0.0.1:9/", "127.0.0.1:9")).is_err());
    }

    #[test]
    fn disabled_dropped_health_none_env_and_lock() {
        let t = format!(
            "[router]\ngpu_lock = \"{{dir}}/b.lock\"\n{ONE}health = \"none\"\nttl = 5\nenv = {{ A = \"1\", B = 2 }}\n\
             [[models]]\nname = \"off\"\nenabled = false\ncmd = \"false\"\nproxy = \"http://127.0.0.1:1\"\n"
        );
        let c = parse(&t).unwrap();
        assert!(c.resolve("off").is_none());
        let m = c.resolve("a").unwrap();
        assert_eq!(m.health, None);
        assert_eq!(m.ttl, Some(Duration::from_secs(5)));
        assert_eq!(
            m.env,
            vec![("A".into(), "1".into()), ("B".into(), "2".into())]
        );
        assert_eq!(c.gpu_lock, Some(PathBuf::from("/cfg/b.lock")));
    }

    #[test]
    fn prototype_router_toml_parses() {
        let c = parse(include_str!("../tests/fixtures/router.toml")).unwrap();
        assert_eq!(c.models.len(), 7, "the disabled placeholder is dropped");
        assert_eq!(c.resolve("tiered-a").unwrap().name, "tiered-a-iq2");
        assert_eq!(
            c.resolve("tiered-a-iq2").unwrap().ready_timeout,
            Duration::from_secs(900)
        );
        assert_eq!(
            c.resolve("dense-9b").unwrap().containers,
            vec!["nrouter-llama".to_string()]
        );
    }
}
