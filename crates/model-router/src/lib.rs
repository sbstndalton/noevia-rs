#![forbid(unsafe_code)]
//! noevia-router: one OpenAI-compatible endpoint in front of several local inference engines, with
//! exactly one model loaded at a time.
//!
//! The request's `model` (or an alias) picks a `[[models]]` entry. When another model is loaded,
//! the router drains in-flight requests, stops the loaded backend (its stop command, SIGTERM then
//! SIGKILL of its process group, then removal of its named containers, verified gone), starts the
//! requested one, polls its health check within its `ready_timeout`, and proxies the request,
//! streamed or not. Idle models unload after their `ttl`. Every swap is logged with its timings to
//! `router-events.jsonl`. An optional `gpu_lock` file is flocked while a backend may be running.

pub mod backend;
pub mod config;
pub mod events;
pub mod gpu_lock;
pub mod http;
pub mod router;

pub use config::{Config, Model};
pub use router::Router;

use std::future::Future;
use std::time::Duration;

/// Serve on `listener` until `shutdown` resolves; then give in-flight requests the configured
/// grace, and stop the loaded backend.
pub async fn serve(
    listener: tokio::net::TcpListener,
    router: Router,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> std::io::Result<()> {
    let ticker = {
        let router = router.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_millis(500));
            loop {
                interval.tick().await;
                router.tick();
            }
        })
    };
    let (stop_tx, mut stop_rx) = tokio::sync::watch::channel(false);
    let app = http::app(router.clone());
    let server = tokio::spawn(async move {
        axum::serve(listener, app)
            .with_graceful_shutdown(async move {
                let _ = stop_rx.wait_for(|v| *v).await;
            })
            .await
    });
    shutdown.await;
    tracing::info!(
        "shutting down: draining for up to {:?}",
        router.config().shutdown_grace
    );
    router.begin_shutdown();
    let _ = stop_tx.send(true);
    let grace = router.config().shutdown_grace;
    let mut server = server;
    let served = match tokio::time::timeout(grace, &mut server).await {
        Ok(joined) => joined.unwrap_or(Ok(())),
        Err(_) => {
            server.abort();
            Ok(())
        }
    };
    ticker.abort();
    router.shutdown().await;
    served
}
