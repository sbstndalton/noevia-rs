//! noevia-server binary: reads the environment (see config.rs), listens on UI_HOST:UI_PORT.

use noevia_server::{config::Config, router, App};
use std::net::SocketAddr;
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
    let app = App::new(config);
    app.statics.warm();
    let service = router(app).into_make_service_with_connect_info::<SocketAddr>();
    let (stop_tx, stop_rx) = tokio::sync::watch::channel(false);
    let mut stop_wait = stop_rx.clone();
    let server = axum::serve(listener, service).with_graceful_shutdown(async move {
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
        r = server => match r {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("noevia-server: {e}");
                ExitCode::from(1)
            }
        },
        _ = drained => ExitCode::SUCCESS,
    }
}
