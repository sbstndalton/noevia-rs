//! noevia-server binary: reads the environment (see config.rs), listens on UI_HOST:UI_PORT.

use noevia_server::{config::Config, serve, App};
use std::process::ExitCode;
use std::time::Duration;

/// How long in-flight requests (a streaming chat) get after SIGTERM before the process exits.
const DRAIN: Duration = Duration::from_secs(5);

async fn terminated() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        match signal(SignalKind::terminate()) {
            Ok(mut term) => {
                tokio::select! {
                    _ = term.recv() => {}
                    _ = tokio::signal::ctrl_c() => {}
                }
            }
            Err(_) => {
                let _ = tokio::signal::ctrl_c().await;
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

#[tokio::main]
async fn main() -> ExitCode {
    // build/web-supervisor.sh asks before choosing this front (sbstndalton/noevia#1246).
    if std::env::args().nth(1).as_deref() == Some("--features") {
        for f in noevia_server::FEATURES {
            println!("{f}");
        }
        return ExitCode::SUCCESS;
    }
    let config = match Config::from_lookup(|k| std::env::var(k).ok()) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("noevia-server: {e}");
            return ExitCode::from(2);
        }
    };
    let listener = match tokio::net::TcpListener::bind((config.host.as_str(), config.port)).await {
        Ok(l) => l,
        Err(e) => {
            eprintln!(
                "noevia-server: cannot listen on {}:{}: {e}",
                config.host, config.port
            );
            return ExitCode::from(1);
        }
    };
    if !config.dist.join("index.html").is_file() {
        eprintln!(
            "noevia-server: warning: {} has no index.html; the web client will not load",
            config.dist.display()
        );
    }
    println!(
        "noevia-server listening on http://{}:{} (legacy upstream {}, trust proxy {})",
        config.host,
        config.port,
        config.upstream.authority(),
        config.trust_proxy
    );
    let rust_auth = config.rust_auth.is_some();
    let app = App::new(config);
    app.statics.warm();
    // Without NOEVIA_RUST_AUTH no route needs an identity, so a refusal is only reported. With it
    // Rust owns sign-in: a schema newer than this build stops the front (identity.rs).
    let layer = std::sync::Arc::clone(&app.identity);
    // The Argon2 decoy is built once; build it before the first request can time it.
    match tokio::task::spawn_blocking(move || {
        if !server_auth::password::warm() {
            eprintln!("noevia-server: warning: Argon2 decoy unavailable");
        }
        layer.status()
    })
    .await
    {
        Ok(noevia_server::identity::Status::Ready) => {
            println!("noevia-server: identity ready (read-only cowork.db)")
        }
        Ok(noevia_server::identity::Status::Unavailable(why)) => {
            println!("noevia-server: identity unavailable for now: {why}")
        }
        Ok(noevia_server::identity::Status::Refused(why)) => {
            if rust_auth {
                eprintln!("noevia-server: NOEVIA_RUST_AUTH=1 but identity is refused: {why}");
                return ExitCode::from(1);
            }
            eprintln!("noevia-server: warning: identity refused: {why}")
        }
        Err(_) => eprintln!("noevia-server: warning: identity check did not run"),
    }
    if rust_auth {
        let writes = std::sync::Arc::clone(&app.writes);
        match tokio::task::spawn_blocking(move || writes.writer().map(|_| ())).await {
            Ok(Ok(())) => println!(
                "noevia-server: NOEVIA_RUST_AUTH=1, Rust owns sign-in and the account tables"
            ),
            Ok(Err(e)) => {
                println!("noevia-server: NOEVIA_RUST_AUTH=1, cowork.db writer not open yet: {e}")
            }
            Err(_) => eprintln!("noevia-server: warning: writer check did not run"),
        }
    }
    let (stop_tx, stop_rx) = tokio::sync::watch::channel(false);
    let mut stop_wait = stop_rx.clone();
    let server = serve::serve(listener, app, serve::Limits::default(), async move {
        let mut rx = stop_rx;
        let _ = rx.wait_for(|s| *s).await;
    });
    tokio::spawn(async move {
        terminated().await;
        let _ = stop_tx.send(true);
    });
    let drained = async {
        let _ = stop_wait.wait_for(|s| *s).await;
        tokio::time::sleep(DRAIN).await;
    };
    tokio::select! {
        () = server => ExitCode::SUCCESS,
        _ = drained => ExitCode::SUCCESS,
    }
}
