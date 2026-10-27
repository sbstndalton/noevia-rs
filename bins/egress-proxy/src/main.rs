//! egress-proxy: deny-by-default CONNECT + forward proxy for sandboxed coding tasks.
//!
//! Native profile (explicit; no legacy grant/key fallback):
//! `--native-control-v1 --control-key-file <absolute-path> --control-key-uid <uid>`
//! `--control-key-gid <positive-gid> --listen <specific-address:port>`.
//! Matching environment fields are CODE_EGRESS_CONTROL_KEY_FILE/_UID/_GID.
//! Missing/invalid current keys leave the native process UNREADY: only a fresh
//! authenticated Ready can install authority. `--capabilities` is side-effect-free
//! build metadata, not a readiness proof. Existing CLI below is the historical
//! legacy profile; selected production pairing remains separately qualified.
//!
//! ```text
//! egress-proxy [--grants <file.json>] [--grant-key-file <file>] [--listen <addr:port>]
//!              [--max-connections <n>] [--max-connections-per-task <n>] [--tunnel-idle-ms <ms>]
//! ```
//! Signed grants (noevia `docs/egress-grant-contract.md`): `--grant-key-file` /
//! `CODE_EGRESS_GRANT_KEY_FILE` names a file holding the 64-hex-digit HMAC key the web derives
//! and writes; `CODE_EGRESS_GRANT_KEY` (the hex itself) is the fallback. The file is re-read
//! every few seconds, so it may appear after the proxy starts. At least one of a grants file
//! or a grant key is required. `CODE_EGRESS_BIND` may be a host name (a compose network
//! alias): it is resolved once at start, so the proxy listens only on that network.
//! Environment fallbacks (the JS proxy's names): `CODE_EGRESS_GRANTS_FILE`,
//! `CODE_EGRESS_BIND` (default 127.0.0.1) + `CODE_EGRESS_PORT`, `CODE_EGRESS_MAX_CONNECTIONS` (default 256),
//! `CODE_EGRESS_MAX_CONNECTIONS_PER_TASK` (default 64; over it, 429), `CODE_EGRESS_TUNNEL_IDLE_MS` (default 600000: an idle CONNECT tunnel closes after 10 min).
//! The grants file is `{"token","task","domains":[...],"expiresIdleMs"?}` or an array of them.
//! Dark: noevia runs it only when the owner sets CODE_EGRESS_IMPL=rust (noevia#926).
#![forbid(unsafe_code)]

use std::net::SocketAddr;
use std::process::ExitCode;
use std::time::Duration;

use egress::GrantKey;
use egress_proxy::control::{engine::Engine, keyfile::KeyFile};
use egress_proxy::{parse_grants, stderr_log, KeySource, Limits, ProxyBuilder};
use std::{path::PathBuf, sync::Arc};

struct NativeProfile {
    path: PathBuf,
    uid: u32,
    gid: u32,
}
struct Args {
    native: Option<NativeProfile>,
    grants: Option<String>,
    key: Option<KeySource>,
    listen: SocketAddr,
    max_connections: usize,
    max_connections_per_task: usize,
    tunnel_idle: Duration,
}

fn parse_args() -> Result<Args, String> {
    let args = std::env::args_os()
        .skip(1)
        .map(|v| {
            v.into_string()
                .map_err(|_| "arguments must be UTF-8".to_owned())
        })
        .collect::<Result<Vec<_>, _>>()?;
    parse_options(args, &|key| std::env::var(key).ok())
}
fn parse_options(args: Vec<String>, env: &impl Fn(&str) -> Option<String>) -> Result<Args, String> {
    let mut grants = env("CODE_EGRESS_GRANTS_FILE");
    let mut native = false;
    let mut control_path = env("CODE_EGRESS_CONTROL_KEY_FILE");
    let mut control_uid = env("CODE_EGRESS_CONTROL_KEY_UID");
    let mut control_gid = env("CODE_EGRESS_CONTROL_KEY_GID");
    let mut key_file = env("CODE_EGRESS_GRANT_KEY_FILE").filter(|v| !v.trim().is_empty());
    let mut listen: Option<String> = None;
    let mut max_connections = env("CODE_EGRESS_MAX_CONNECTIONS");
    let mut per_task = env("CODE_EGRESS_MAX_CONNECTIONS_PER_TASK");
    let mut tunnel_idle = env("CODE_EGRESS_TUNNEL_IDLE_MS");
    let mut it = args.into_iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--native-control-v1" => native = true,
            "--control-key-file" => control_path = Some(value(&mut it)?),
            "--control-key-uid" => control_uid = Some(value(&mut it)?),
            "--control-key-gid" => control_gid = Some(value(&mut it)?),
            "--grants" => grants = Some(value(&mut it)?),
            "--grant-key-file" => key_file = Some(value(&mut it)?),
            "--listen" => listen = Some(value(&mut it)?),
            "--max-connections" => max_connections = Some(value(&mut it)?),
            "--max-connections-per-task" => per_task = Some(value(&mut it)?),
            "--tunnel-idle-ms" => tunnel_idle = Some(value(&mut it)?),
            "-h" | "--help" => {
                return Err(
                    "usage: egress-proxy --native-control-v1 --control-key-file <absolute> \
                     --control-key-uid <uid> --control-key-gid <gid> [--listen <addr:port>]; \
                     or --capabilities; legacy: [--grants <file.json>] [--grant-key-file <file>] \
                     [--listen <addr:port>] \
                     [--max-connections <n>] [--max-connections-per-task <n>] \
                     [--tunnel-idle-ms <ms>]"
                        .into(),
                )
            }
            _ => return Err("unknown argument".into()),
        }
    }
    let raw_key = env("CODE_EGRESS_GRANT_KEY").filter(|v| !v.trim().is_empty());
    let profile = if native {
        if grants.is_some() || key_file.is_some() || raw_key.is_some() {
            return Err("native control refuses legacy grants and keys".into());
        }
        let path = PathBuf::from(control_path.ok_or("missing native control key file")?);
        let uid = identity(control_uid.ok_or("missing native control key uid")?, false)?;
        let gid = identity(control_gid.ok_or("missing native control key gid")?, true)?;
        KeyFile::new(&path, uid, gid).map_err(|_| "invalid native key-file configuration")?;
        Some(NativeProfile { path, uid, gid })
    } else {
        if control_path.is_some() || control_uid.is_some() || control_gid.is_some() {
            return Err("native key profile requires explicit native mode".into());
        }
        None
    };
    let key = if profile.is_some() {
        None
    } else {
        match key_file {
            Some(path) => Some(KeySource::File(path.into())),
            None => raw_key
                .map(|hex| {
                    GrantKey::from_hex(hex.trim())
                        .map(KeySource::Static)
                        .map_err(|_| "invalid legacy grant key".to_owned())
                })
                .transpose()?,
        }
    };
    if profile.is_none() && grants.is_none() && key.is_none() {
        return Err("no grant authority configured".into());
    }
    let listen = match listen {
        Some(l) => l,
        None => {
            let port = env("CODE_EGRESS_PORT").ok_or("no listen address")?;
            let bind = env("CODE_EGRESS_BIND").unwrap_or_else(|| "127.0.0.1".into());
            let bind = resolve_bind(bind.trim())?;
            if bind.contains(':') {
                format!("[{}]:{}", bind, port.trim())
            } else {
                format!("{}:{}", bind, port.trim())
            }
        }
    };
    let listen = listen
        .parse::<SocketAddr>()
        .map_err(|_| "invalid listen address")?;
    let max_connections = match max_connections {
        None => Limits::default().max_connections,
        Some(n) => n
            .trim()
            .parse::<usize>()
            .ok()
            .filter(|&n| n > 0)
            .ok_or("max connections must be a positive integer")?,
    };
    let max_connections_per_task = match per_task {
        None => Limits::default().max_connections_per_task,
        Some(n) => n
            .trim()
            .parse::<usize>()
            .ok()
            .filter(|&n| n > 0)
            .ok_or("max connections per task must be a positive integer")?,
    };
    let tunnel_idle = match tunnel_idle {
        None => Limits::default().tunnel_idle_timeout,
        Some(n) => n
            .trim()
            .parse::<u64>()
            .ok()
            .filter(|&n| n > 0)
            .map(Duration::from_millis)
            .ok_or("tunnel idle timeout must be a positive integer (ms)")?,
    };
    if profile.is_some()
        && (listen.ip().is_unspecified() || max_connections > 256 || max_connections_per_task != 64)
    {
        return Err("unsupported native listener profile".into());
    }
    Ok(Args {
        native: profile,
        grants,
        key,
        listen,
        max_connections,
        max_connections_per_task,
        tunnel_idle,
    })
}

fn value(it: &mut impl Iterator<Item = String>) -> Result<String, String> {
    it.next()
        .filter(|v| !v.starts_with("--"))
        .ok_or_else(|| "missing argument value".into())
}
fn identity(raw: String, positive: bool) -> Result<u32, String> {
    let n = raw
        .parse::<u32>()
        .map_err(|_| "invalid native key identity")?;
    if raw != n.to_string() || n == u32::MAX || (positive && n == 0) {
        return Err("invalid native key identity".into());
    }
    Ok(n)
}
fn capabilities() -> serde_json::Value {
    let supported = cfg!(unix);
    let capabilities = if supported {
        vec![egress::control::CAPABILITY]
    } else {
        Vec::new()
    };
    serde_json::json!({"version":1,"probe":"source-capability","capabilities":capabilities,"nativeControl":supported,"sessionLeaseMs":15000,"renewalMs":5000,"readiness":"requires-authenticated-ready"})
}
async fn run_native(args: Args, profile: NativeProfile) -> ExitCode {
    let provider = match KeyFile::new(&profile.path, profile.uid, profile.gid) {
        Ok(v) => v,
        Err(_) => {
            eprintln!("invalid native key-file configuration");
            return ExitCode::from(2);
        }
    };
    let engine = match Engine::waiting(Arc::new(provider)) {
        Ok(v) => Arc::new(v),
        Err(_) => {
            eprintln!("native controller unavailable");
            return ExitCode::FAILURE;
        }
    };
    let listener = match tokio::net::TcpListener::bind(args.listen).await {
        Ok(v) => v,
        Err(_) => {
            eprintln!("native listener unavailable");
            return ExitCode::FAILURE;
        }
    };
    let log = stderr_log();
    log(
        serde_json::json!({"event":"egress.listening","bind":listener.local_addr().ok().map(|v|v.to_string()),"mode":egress::control::CAPABILITY,"authority":"requires-authenticated-ready"}),
    );
    let limits = Limits {
        max_connections: args.max_connections,
        max_connections_per_task: args.max_connections_per_task,
        tunnel_idle_timeout: args.tunnel_idle,
        ..Limits::default()
    };
    let proxy = ProxyBuilder::new(vec![]).limits(limits).log(log).build();
    match proxy.serve_native(listener, engine).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(_) => {
            eprintln!("native listener stopped");
            ExitCode::FAILURE
        }
    }
}

/// An IP stays as is; a name (a compose alias) is resolved once to its first address, so the
/// proxy binds one network only. A name that does not resolve is an error, never 0.0.0.0.
fn resolve_bind(bind: &str) -> Result<String, String> {
    if bind.parse::<std::net::IpAddr>().is_ok() {
        return Ok(bind.to_owned());
    }
    use std::net::ToSocketAddrs;
    (bind, 0u16)
        .to_socket_addrs()
        .map_err(|_| "bind name is unavailable".to_owned())?
        .next()
        .map(|a| a.ip().to_string())
        .ok_or_else(|| "bind name has no address".to_owned())
}

fn main() -> ExitCode {
    if std::env::args_os()
        .skip(1)
        .eq([std::ffi::OsString::from("--capabilities")])
    {
        println!("{}", capabilities());
        return ExitCode::SUCCESS;
    }
    let mut args = match parse_args() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("egress-proxy: {e}");
            return ExitCode::from(2);
        }
    };
    if let Some(profile) = args.native.take() {
        let runtime = match tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
        {
            Ok(v) => v,
            Err(_) => {
                eprintln!("native runtime unavailable");
                return ExitCode::FAILURE;
            }
        };
        return runtime.block_on(run_native(args, profile));
    }
    let grants = match &args.grants {
        None => Vec::new(),
        Some(path) => match std::fs::read_to_string(path)
            .map_err(|e| format!("grants file {path:?}: {e}"))
            .and_then(|t| parse_grants(&t))
        {
            Ok(g) => g,
            Err(e) => {
                eprintln!("egress-proxy: {e}");
                return ExitCode::from(2);
            }
        },
    };
    let sweep_every = grants
        .iter()
        .map(|g| g.idle_ttl_ms)
        .min()
        .unwrap_or(60_000)
        .min(if args.key.is_some() { 5_000 } else { 60_000 })
        .clamp(100, 60_000);
    let signed = args.key.is_some();
    let runtime = match tokio::runtime::Runtime::new() {
        Ok(r) => r,
        Err(e) => {
            eprintln!("egress-proxy: runtime: {e}");
            return ExitCode::FAILURE;
        }
    };
    runtime.block_on(async move {
        let listener = match tokio::net::TcpListener::bind(args.listen).await {
            Ok(l) => l,
            Err(e) => {
                eprintln!(
                    "{}",
                    serde_json::json!({ "event": "egress.bind_failed", "error": e.to_string() })
                );
                return ExitCode::FAILURE;
            }
        };
        let log = stderr_log();
        let bound = listener.local_addr().map(|a| a.to_string()).ok();
        log(
            serde_json::json!({ "event": "egress.listening", "bind": bound,
            "grants": grants.len(), "signedGrants": signed }),
        );
        let limits = Limits {
            max_connections: args.max_connections,
            max_connections_per_task: args.max_connections_per_task,
            tunnel_idle_timeout: args.tunnel_idle,
            ..Limits::default()
        };
        let mut builder = ProxyBuilder::new(grants).limits(limits).log(log);
        if let Some(k) = args.key {
            builder = builder.grant_key(k);
        }
        let proxy = builder.build();
        let sweeper = proxy.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_millis(sweep_every));
            loop {
                tick.tick().await;
                sweeper.sweep();
            }
        });
        proxy.serve(listener).await;
        ExitCode::SUCCESS
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    fn options(items: &[&str], variables: &[(&str, &str)]) -> Result<Args, String> {
        parse_options(items.iter().map(|v| v.to_string()).collect(), &|name| {
            variables
                .iter()
                .find(|(k, _)| *k == name)
                .map(|(_, v)| v.to_string())
        })
    }
    #[cfg(unix)]
    #[test]
    fn native_profile_requires_explicit_mode_and_canonical_identity() {
        let args = options(
            &[
                "--native-control-v1",
                "--control-key-file",
                "/tmp/control.key",
                "--control-key-uid",
                "0",
                "--control-key-gid",
                "60101",
                "--listen",
                "127.0.0.1:0",
            ],
            &[],
        )
        .ok();
        assert!(args.as_ref().is_some_and(|a| a
            .native
            .as_ref()
            .is_some_and(|n| n.uid == 0 && n.gid == 60101)
            && a.key.is_none()
            && a.grants.is_none()));
        assert!(options(
            &[
                "--control-key-file",
                "/tmp/control.key",
                "--listen",
                "127.0.0.1:1"
            ],
            &[]
        )
        .is_err());
        for raw in ["+0", "00", " 0", "4294967295", "4294967296", "-1"] {
            assert!(identity(raw.into(), false).is_err());
        }
        assert!(identity("0".into(), true).is_err());
    }
    #[test]
    fn native_mode_never_falls_back_to_legacy_or_missing_argument_values() {
        let base = [
            "--native-control-v1",
            "--control-key-file",
            "/tmp/control.key",
            "--control-key-uid",
            "0",
            "--control-key-gid",
            "60101",
            "--listen",
            "127.0.0.1:1",
        ];
        for key in [
            "CODE_EGRESS_GRANTS_FILE",
            "CODE_EGRESS_GRANT_KEY_FILE",
            "CODE_EGRESS_GRANT_KEY",
        ] {
            assert!(options(&base, &[(key, "synthetic-private-input")]).is_err());
        }
        assert!(options(
            &["--native-control-v1", "--control-key-file"],
            &[("CODE_EGRESS_GRANT_KEY", "07")]
        )
        .is_err());
        assert!(options(
            &["--grant-key-file", "--native-control-v1"],
            &[("CODE_EGRESS_GRANT_KEY", "07")]
        )
        .is_err());
    }
    #[cfg(unix)]
    #[test]
    fn native_config_refuses_wildcard_and_unsupported_resource_profile() {
        let base = [
            "--native-control-v1",
            "--control-key-file",
            "/tmp/control.key",
            "--control-key-uid",
            "0",
            "--control-key-gid",
            "60101",
        ];
        for addr in ["0.0.0.0:3001", "[::]:3001"] {
            let mut args = base.to_vec();
            args.extend(["--listen", addr]);
            assert!(options(&args, &[]).is_err());
        }
        for (flag, value) in [
            ("--max-connections", "257"),
            ("--max-connections-per-task", "65"),
        ] {
            let mut args = base.to_vec();
            args.extend(["--listen", "127.0.0.1:1", flag, value]);
            assert!(options(&args, &[]).is_err());
        }
    }
    #[test]
    fn capability_probe_is_static_metadata_and_errors_redact_input() {
        let probe = capabilities();
        assert_eq!(
            probe.get("nativeControl"),
            Some(&serde_json::json!(cfg!(unix)))
        );
        assert_eq!(
            probe.get("readiness"),
            Some(&serde_json::json!("requires-authenticated-ready"))
        );
        let err = options(&["synthetic-private-input"], &[]).err();
        assert!(err.is_some_and(|e| !e.contains("synthetic-private-input")));
    }
    #[test]
    fn historical_legacy_profile_is_not_implicitly_native() {
        let parsed = options(
            &["--grants", "/tmp/oracle.json", "--listen", "127.0.0.1:1"],
            &[],
        )
        .ok();
        assert!(parsed.is_some_and(|a| a.native.is_none() && a.grants.is_some()));
    }
}
