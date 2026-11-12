#![forbid(unsafe_code)]
//! noevia-router: one OpenAI-compatible endpoint, one local model loaded at a time.
//!
//!     noevia-router --config router.toml [--host H] [--port P] [--logdir D] [--preload MODEL]
//!     noevia-router --config router.toml --check     # validate, print the names, exit
//!
//! Loopback only unless `[router] allow_non_loopback = true`. SIGTERM/SIGINT drain in-flight
//! requests for `shutdown_grace` seconds, then stop the loaded backend.

use model_router::{config, Config, Router};
use std::path::PathBuf;

const USAGE: &str = "usage: noevia-router --config FILE [--host H] [--port P] [--logdir DIR] [--preload MODEL] [--check]";

struct Args {
    config: PathBuf,
    host: Option<String>,
    port: Option<u16>,
    logdir: Option<PathBuf>,
    preload: Option<String>,
    check: bool,
}

fn parse_args() -> Result<Args, String> {
    let mut it = std::env::args().skip(1);
    let mut args = Args {
        config: PathBuf::from("router.toml"),
        host: None,
        port: None,
        logdir: None,
        preload: None,
        check: false,
    };
    while let Some(a) = it.next() {
        let mut value = |name: &str| {
            it.next()
                .ok_or_else(|| format!("{name} needs a value\n{USAGE}"))
        };
        match a.as_str() {
            "--config" => args.config = PathBuf::from(value("--config")?),
            "--host" => args.host = Some(value("--host")?),
            "--port" => {
                args.port = Some(
                    value("--port")?
                        .parse()
                        .map_err(|e| format!("--port: {e}"))?,
                )
            }
            "--logdir" => args.logdir = Some(PathBuf::from(value("--logdir")?)),
            "--preload" => args.preload = Some(value("--preload")?),
            "--check" => args.check = true,
            "-h" | "--help" => return Err(USAGE.into()),
            other => return Err(format!("unknown argument {other:?}\n{USAGE}")),
        }
    }
    Ok(args)
}

async fn terminated() {
    let term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate());
    match term {
        Ok(mut term) => {
            tokio::select! { _ = term.recv() => {}, _ = tokio::signal::ctrl_c() => {} }
        }
        Err(_) => {
            let _ = tokio::signal::ctrl_c().await;
        }
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .init();
    let args = parse_args()?;
    let mut cfg = Config::load(&args.config)?;
    if args.check {
        for m in &cfg.models {
            println!("{}\t{}\t{}", m.name, m.engine, m.aliases.join(","));
        }
        return Ok(());
    }
    if let Some(h) = args.host {
        cfg.host = h;
    }
    if let Some(p) = args.port {
        cfg.port = p;
    }
    if let Some(l) = args.logdir {
        cfg.logdir = l;
    }
    if !config::is_loopback(&cfg.host) && !cfg.allow_non_loopback {
        return Err(format!(
            "refusing to bind {}: loopback only unless [router] allow_non_loopback = true",
            cfg.host
        )
        .into());
    }
    let listener = tokio::net::TcpListener::bind((cfg.host.as_str(), cfg.port)).await?;
    let names: Vec<String> = cfg.models.iter().map(|m| m.name.clone()).collect();
    tracing::info!(
        "noevia-router on http://{} models={names:?}",
        listener.local_addr()?
    );
    let router = Router::new(cfg)?;
    if let Some(name) = args.preload {
        match router.config().names.get(&name).copied() {
            Some(i) => {
                let r = router.clone();
                tokio::spawn(async move {
                    if let Err(e) = r.acquire(i).await {
                        tracing::warn!("preload {name} failed: {e}");
                    }
                });
            }
            None => tracing::warn!("preload: unknown model {name:?}"),
        }
    }
    model_router::serve(listener, router, terminated()).await?;
    Ok(())
}
