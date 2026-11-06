//! noevia-diary: the Rust Diary server process (port contract D4). Python's lifespan starts it on
//! 127.0.0.1:8011 and forwards the routes named in DIARY_RUST_ROUTES to it; every other route,
//! and every request this side declines, stays with Python.

pub mod assertion;
pub mod backup;
pub mod config;
pub mod corpus;
pub mod health;
pub mod managed;
pub mod py;
pub mod server;
pub mod workspace;
pub mod write;
pub mod writes;

/// The `noevia-diary --version` line (image provenance): the crate version and the noevia-rs ref
/// the binary was built from (`NOEVIA_RS_REF` at build time, else `unknown`).
pub fn version_line() -> String {
    version_line_for(option_env!("NOEVIA_RS_REF"))
}

fn version_line_for(rs_ref: Option<&str>) -> String {
    let rs_ref = rs_ref.filter(|r| !r.is_empty()).unwrap_or("unknown");
    format!(
        "noevia-diary {} noevia-rs {rs_ref}",
        env!("CARGO_PKG_VERSION")
    )
}

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

#[cfg(test)]
mod version_tests {
    #[test]
    fn version_line_names_the_crate_version_and_the_noevia_rs_ref() {
        let v = env!("CARGO_PKG_VERSION");
        assert_eq!(
            super::version_line_for(Some("0123456789abcdef0123456789abcdef01234567")),
            format!("noevia-diary {v} noevia-rs 0123456789abcdef0123456789abcdef01234567")
        );
        assert_eq!(
            super::version_line_for(None),
            format!("noevia-diary {v} noevia-rs unknown")
        );
        assert_eq!(
            super::version_line_for(Some("")),
            format!("noevia-diary {v} noevia-rs unknown")
        );
        assert!(!super::version_line().contains('\n'));
    }
}
