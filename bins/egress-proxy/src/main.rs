//! egress-proxy: deny-by-default CONNECT + forward proxy for sandboxed coding tasks.
//!
//! ```text
//! egress-proxy --grants <file.json> [--listen <addr:port>] [--max-connections <n>]
//!              [--max-connections-per-task <n>] [--tunnel-idle-ms <ms>]
//! ```
//! Environment fallbacks (the JS proxy's names): `CODE_EGRESS_GRANTS_FILE`,
//! `CODE_EGRESS_BIND` (default 127.0.0.1) + `CODE_EGRESS_PORT`, `CODE_EGRESS_MAX_CONNECTIONS` (default 256),
//! `CODE_EGRESS_MAX_CONNECTIONS_PER_TASK` (default 64; over it, 429), `CODE_EGRESS_TUNNEL_IDLE_MS` (default 600000: an idle CONNECT tunnel closes after 10 min).
//! The grants file is `{"token","task","domains":[...],"expiresIdleMs"?}` or an array of them.
//! Dark: nothing deploys this yet; the web-side signed grant contract is a later slice.
#![forbid(unsafe_code)]

use std::net::SocketAddr;
use std::process::ExitCode;
use std::time::Duration;

use egress_proxy::{parse_grants, stderr_log, Limits, ProxyBuilder};

struct Args {
    grants: String,
    listen: SocketAddr,
    max_connections: usize,
    max_connections_per_task: usize,
    tunnel_idle: Duration,
}

fn parse_args() -> Result<Args, String> {
    let mut grants = std::env::var("CODE_EGRESS_GRANTS_FILE").ok();
    let mut listen: Option<String> = None;
    let mut max_connections = std::env::var("CODE_EGRESS_MAX_CONNECTIONS").ok();
    let mut per_task = std::env::var("CODE_EGRESS_MAX_CONNECTIONS_PER_TASK").ok();
    let mut tunnel_idle = std::env::var("CODE_EGRESS_TUNNEL_IDLE_MS").ok();
    let mut it = std::env::args().skip(1);
    while let Some(a) = it.next() {
        match a.as_str() {
            "--grants" => grants = it.next(),
            "--listen" => listen = it.next(),
            "--max-connections" => max_connections = it.next(),
            "--max-connections-per-task" => per_task = it.next(),
            "--tunnel-idle-ms" => tunnel_idle = it.next(),
            "-h" | "--help" => {
                return Err(
                    "usage: egress-proxy --grants <file.json> [--listen <addr:port>] \
                     [--max-connections <n>] [--max-connections-per-task <n>] \
                     [--tunnel-idle-ms <ms>]"
                        .into(),
                )
            }
            other => return Err(format!("unknown argument {other:?}")),
        }
    }
    let grants = grants.ok_or("no grants file (--grants or CODE_EGRESS_GRANTS_FILE)")?;
    let listen = match listen {
        Some(l) => l,
        None => {
            let port = std::env::var("CODE_EGRESS_PORT")
                .map_err(|_| "no listen address (--listen or CODE_EGRESS_PORT)")?;
            let bind = std::env::var("CODE_EGRESS_BIND").unwrap_or_else(|_| "127.0.0.1".into());
            if bind.contains(':') {
                format!("[{}]:{}", bind.trim(), port.trim())
            } else {
                format!("{}:{}", bind.trim(), port.trim())
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
        listen,
        max_connections,
        max_connections_per_task,
        tunnel_idle,
    })
}

fn main() -> ExitCode {
    let args = match parse_args() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("egress-proxy: {e}");
            return ExitCode::from(2);
        }
    };
    let grants = match std::fs::read_to_string(&args.grants)
        .map_err(|e| format!("grants file {:?}: {e}", args.grants))
        .and_then(|t| parse_grants(&t))
    {
        Ok(g) => g,
        Err(e) => {
            eprintln!("egress-proxy: {e}");
            return ExitCode::from(2);
        }
    };
    let sweep_every = grants
        .iter()
        .map(|g| g.idle_ttl_ms)
        .min()
        .unwrap_or(60_000)
        .clamp(100, 60_000);
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
        log(serde_json::json!({ "event": "egress.listening", "bind": bound, "grants": grants.len() }));
        let limits = Limits {
            max_connections: args.max_connections,
            max_connections_per_task: args.max_connections_per_task,
            tunnel_idle_timeout: args.tunnel_idle,
            ..Limits::default()
        };
        let proxy = ProxyBuilder::new(grants).limits(limits).log(log).build();
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
