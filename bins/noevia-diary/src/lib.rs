//! noevia-diary: the Rust Diary server process (port contract D4). Python's lifespan starts it on
//! 127.0.0.1:8011 and forwards the routes named in DIARY_RUST_ROUTES to it; every other route,
//! and every request this side declines, stays with Python.

pub mod assertion;
pub mod config;
pub mod corpus;
pub mod health;
pub mod managed;
pub mod py;
pub mod server;
pub mod workspace;
pub mod write;
pub mod writes;

use std::sync::Arc;

/// Serve on `listener` until the process is stopped.
pub async fn serve(
    listener: tokio::net::TcpListener,
    app: Arc<server::App>,
) -> std::io::Result<()> {
    let router = axum::Router::new().fallback(move |req: axum::http::Request<axum::body::Body>| {
        let app = app.clone();
        async move { server::handle(app, req).await }
    });
    axum::serve(listener, router).await
}
