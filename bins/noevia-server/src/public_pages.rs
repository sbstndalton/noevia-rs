//! The public pages (sbstndalton/noevia#2190): `GET|HEAD /about` and `/privacy` (core
//! routes/public-pages.cjs, the home page and privacy policy Google requires for the app
//! registration behind "Connect Google Drive"), and `/device`, the native-app approval page (core
//! routes/device-auth.cjs `open`). Permanent, no switch; Node refuses them (core
//! rust-public-pages.cjs `RUST_PUBLIC_PAGES_OWNED`), so the web supervisor pairs this front's
//! `rust-public-pages` capability with that Core.
//!
//! - `/about`, `/privacy`: GET and HEAD only (routes.toml `methods`; Node's other methods fell
//!   through to its own chain, and still do). The exact bytes Node served (pages/*.html, written
//!   from core's `publicPage`), `text/html; charset=utf-8`, `public, max-age=300` and the security
//!   headers, before any session is looked at. Node matched the exact pathname, so `/about/` is
//!   not this page and goes to Node.
//! - `/device` and `/device/`, every method: while `features.nativeClientAuth` is off the JSON 404
//!   Node answered first; on, the web bundle's answer for the path (the SPA approval page), as the
//!   request chain gave it after that mount.

use crate::{reply, App};
use axum::body::Body;
use axum::http::{header, HeaderValue, Method, Request, Response, StatusCode};
use std::sync::Arc;

pub const ABOUT: &str = "/about";
pub const PRIVACY: &str = "/privacy";
pub const DEVICE: &str = "/device";

/// The routes this module answers (routes.toml).
pub const ROUTES: &[&str] = &[ABOUT, PRIVACY, DEVICE];

const ABOUT_HTML: &str = include_str!("pages/about.html");
const PRIVACY_HTML: &str = include_str!("pages/privacy.html");

/// core public-pages.cjs `publicPage(path)`: the exact pathname only.
pub fn page(path: &str) -> Option<&'static str> {
    match path {
        ABOUT => Some(ABOUT_HTML),
        PRIVACY => Some(PRIVACY_HTML),
        _ => None,
    }
}

/// core device-auth.cjs: `p === '/device' || p === '/device/'`.
pub fn is_device_page(path: &str) -> bool {
    path == DEVICE || path == "/device/"
}

/// The page reply index.cjs wrote: 200, HTML, five minutes' public cache, the security headers.
pub fn page_reply(method: &Method, html: &'static str) -> Response<Body> {
    let body = if method == Method::HEAD {
        Body::empty()
    } else {
        Body::from(html)
    };
    let mut res = Response::new(body);
    let h = res.headers_mut();
    reply::security_headers(h, false);
    h.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/html; charset=utf-8"),
    );
    h.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("public, max-age=300"),
    );
    res
}

pub async fn serve(
    app: &Arc<App>,
    conn: crate::serve::Conn,
    req: Request<Body>,
    path: String,
) -> Response<Body> {
    if let Some(html) = page(&path) {
        if req.method() == Method::GET || req.method() == Method::HEAD {
            return page_reply(req.method(), html);
        }
        return app.proxy.forward(conn.peer, req).await;
    }
    if !is_device_page(&path) {
        return app.proxy.forward(conn.peer, req).await;
    }
    let layer = Arc::clone(&app.identity);
    let enabled = tokio::task::spawn_blocking(move || {
        let auth = layer.authenticator()?;
        let store = layer.store_blocking().ok()?;
        store.read(|r| auth.native_client_auth(r)).ok()
    })
    .await
    .ok()
    .flatten();
    match enabled {
        // Node read the flag from its own memory and never failed here; an unreadable store is
        // this front's refusal, never the page.
        None => reply::error(
            StatusCode::SERVICE_UNAVAILABLE,
            "Sign-in is unavailable right now. Try again shortly.",
            false,
        ),
        Some(false) => reply::error(StatusCode::NOT_FOUND, "not found", false),
        Some(true) => {
            // `path` is the WHATWG pathname ("/device" or "/device/"), what Node's chain served
            // after this mount, so a dot-segment spelling gets the same page.
            let statics = app.statics.clone();
            let (method, headers) = (req.method().clone(), req.headers().clone());
            tokio::task::spawn_blocking(move || statics.serve(&method, &headers, &path))
                .await
                .unwrap_or_else(|_| {
                    reply::error(StatusCode::INTERNAL_SERVER_ERROR, "read error", false)
                })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_exact_pathnames_are_pages() {
        assert!(page(ABOUT).is_some_and(|p| p.contains("<title>About · noevia</title>")));
        assert!(page(PRIVACY).is_some_and(|p| p.contains("<title>Privacy policy · noevia</title>")));
        // What Google's app review reads (core web-address.test.cjs asserted the same).
        assert!(page(PRIVACY).is_some_and(|p| p.contains("<code>drive.file</code>")));
        for p in ["/about/", "/privacy/", "/About", "/about.html", "/"] {
            assert!(page(p).is_none(), "{p}");
        }
        assert!(is_device_page("/device") && is_device_page("/device/"));
        assert!(!is_device_page("/device/x") && !is_device_page("/devices"));
    }
}
