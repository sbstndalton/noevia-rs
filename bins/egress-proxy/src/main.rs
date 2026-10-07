//! egress-proxy: deny-by-default CONNECT + forward proxy for sandboxed coding tasks.
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
use egress_proxy::{parse_grants, stderr_log, KeySource, Limits, ProxyBuilder};

struct Args {
    grants: Option<String>,
    key: Option<KeySource>,
    listen: SocketAddr,
    max_connections: usize,
    max_connections_per_task: usize,
    tunnel_idle: Duration,
}

fn parse_args() -> Result<Args, String> {
    let mut grants = std::env::var("CODE_EGRESS_GRANTS_FILE").ok();
    let mut key_file = std::env::var("CODE_EGRESS_GRANT_KEY_FILE")
        .ok()
        .filter(|v| !v.trim().is_empty());
    let mut listen: Option<String> = None;
    let mut max_connections = std::env::var("CODE_EGRESS_MAX_CONNECTIONS").ok();
    let mut per_task = std::env::var("CODE_EGRESS_MAX_CONNECTIONS_PER_TASK").ok();
    let mut tunnel_idle = std::env::var("CODE_EGRESS_TUNNEL_IDLE_MS").ok();
    let mut it = std::env::args().skip(1);
    while let Some(a) = it.next() {
        match a.as_str() {
            "--grants" => grants = it.next(),
            "--grant-key-file" => key_file = it.next(),
            "--listen" => listen = it.next(),
            "--max-connections" => max_connections = it.next(),
            "--max-connections-per-task" => per_task = it.next(),
            "--tunnel-idle-ms" => tunnel_idle = it.next(),
            "-h" | "--help" => {
                return Err(
                    "usage: egress-proxy [--grants <file.json>] [--grant-key-file <file>] \
                     [--listen <addr:port>] \
                     [--max-connections <n>] [--max-connections-per-task <n>] \
                     [--tunnel-idle-ms <ms>]"
                        .into(),
                )
            }
            other => return Err(format!("unknown argument {other:?}")),
        }
    }
    let key = match key_file {
        Some(path) => Some(KeySource::File(path.into())),
        None => match std::env::var("CODE_EGRESS_GRANT_KEY") {
            Ok(hex) if !hex.trim().is_empty() => Some(KeySource::Static(
                GrantKey::from_hex(hex.trim())
                    .map_err(|_| "CODE_EGRESS_GRANT_KEY must be 64 hex digits".to_owned())?,
            )),
            _ => None,
        },
    };
    if grants.is_none() && key.is_none() {
        return Err(
            "no grants: give --grants / CODE_EGRESS_GRANTS_FILE or a grant key \
                    (--grant-key-file / CODE_EGRESS_GRANT_KEY_FILE / CODE_EGRESS_GRANT_KEY)"
                .into(),
        );
    }
    let listen = match listen {
        Some(l) => l,
        None => {
            let port = std::env::var("CODE_EGRESS_PORT")
                .map_err(|_| "no listen address (--listen or CODE_EGRESS_PORT)")?;
            let bind = std::env::var("CODE_EGRESS_BIND").unwrap_or_else(|_| "127.0.0.1".into());
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
        .map_err(|e| format!("listen address {listen:?}: {e}"))?;
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
    Ok(Args {
        grants,
        key,
        listen,
        max_connections,
        max_connections_per_task,
        tunnel_idle,
    })
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
        .map_err(|e| format!("CODE_EGRESS_BIND {bind:?}: {e}"))?
        .next()
        .map(|a| a.ip().to_string())
        .ok_or_else(|| format!("CODE_EGRESS_BIND {bind:?} has no address"))
}

fn main() -> ExitCode {
    let args = match parse_args() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("egress-proxy: {e}");
            return ExitCode::from(2);
        }
    };
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
