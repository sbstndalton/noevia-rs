//! M4 (permanent under NOEVIA_RUST_PROJECTS=1 and NOEVIA_RUST_AUTH=1 since #1071,
//! sbstndalton/noevia#1315): the front answers `GET /api/export/conversations` itself, as core
//! routes/export.cjs does:
//!
//! 1. Node's request gate first (server-auth: session, origin, device-token limits) on the exact
//!    pathname Node matches; a refused request reads nothing.
//! 2. The signed-in user's own files only ([`server_export::route::export`], from the user id the
//!    gate returned), built at one clock reading (`when`) that also names the file.
//! 3. A download is audited before it is sent: `export.conversations`, actor and target the user,
//!    detail `{"bytes":N}`, `created_at` = now, appended to cowork.db's audit_events as core
//!    auth.cjs `audit` appends it. A failed append is Node's 500 and nothing is sent.
//! 4. 200 `application/zip` with Node's Content-Length, Content-Disposition and Cache-Control; a
//!    too-large export is Node's 413 `{"error": …}`, neither audited nor sent; a failure is 500.
//!
//! Every other method (Node's 405), a trailing slash and every other path stay Node's. Exports run
//! one at a time (Node builds them synchronously, one at a time too). An export holds its ZIP once,
//! as the chunks it was made in, which the body sends in order (#1824).

use crate::identity;
use crate::reply;
use crate::App;
use axum::body::Body;
use axum::http::{header, HeaderValue, Method, Request, Response, StatusCode};
use axum::response::IntoResponse;
use server_export::route::{export, Outcome};
use std::sync::Arc;

/// The route (exact pathname; routes.toml).
pub const ROUTE: &str = "/api/export/conversations";

/// The audit action core routes/export.cjs records.
pub const AUDIT_ACTION: &str = "export.conversations";

/// What the blocking part produced.
enum Done {
    Zip {
        chunks: Vec<Vec<u8>>,
        len: usize,
        day: String,
    },
    TooLarge(String),
    Internal,
}

/// The audit row (core auth.cjs `audit(action, actor, actor, { bytes })`).
fn audit(app: &App, user: &str, bytes: usize) -> Result<(), server_store::StoreError> {
    let detail = format!("{{\"bytes\":{bytes}}}");
    let at = identity::now_ms();
    app.writes.writer()?.write(|tx| {
        tx.execute(
            "INSERT INTO audit_events(actor_user_id,target_user_id,action,detail,created_at) VALUES(?1,?2,?3,?4,?5)",
            server_store::rusqlite::params![user, user, AUDIT_ACTION, detail, at],
        )?;
        Ok(())
    })
}

/// The ZIP's chunks as a body, sent in order and dropped as they go, never joined (#1824).
struct Chunks {
    rest: std::collections::VecDeque<Vec<u8>>,
    left: u64,
}

impl Drop for Chunks {
    /// Sent or abandoned (a closed connection): the ZIP is freed, so give the heap back (#1833).
    fn drop(&mut self) {
        self.rest.clear();
        server_export::route::return_memory();
    }
}

impl http_body::Body for Chunks {
    type Data = bytes::Bytes;
    type Error = std::convert::Infallible;

    fn poll_frame(
        mut self: std::pin::Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Result<http_body::Frame<Self::Data>, Self::Error>>> {
        let next = self.rest.pop_front();
        std::task::Poll::Ready(next.map(|c| {
            self.left = self.left.saturating_sub(c.len() as u64);
            Ok(http_body::Frame::data(bytes::Bytes::from(c)))
        }))
    }

    fn is_end_stream(&self) -> bool {
        self.rest.is_empty()
    }

    fn size_hint(&self) -> http_body::SizeHint {
        http_body::SizeHint::with_exact(self.left)
    }
}

/// The 200 reply: index.cjs's security headers, then routes/export.cjs's writeHead.
fn download(chunks: Vec<Vec<u8>>, len: usize, day: &str) -> Response<Body> {
    let Ok(disposition) = HeaderValue::from_str(&format!(
        "attachment; filename=\"noevia-conversations-{day}.zip\""
    )) else {
        return reply::error(StatusCode::INTERNAL_SERVER_ERROR, "Internal error", true);
    };
    let mut res = Response::new(Body::new(Chunks {
        rest: chunks.into_iter().filter(|c| !c.is_empty()).collect(),
        left: len as u64,
    }));
    let h = res.headers_mut();
    reply::security_headers(h, true);
    h.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/zip"),
    );
    h.insert(header::CONTENT_LENGTH, HeaderValue::from(len));
    h.insert(header::CONTENT_DISPOSITION, disposition);
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    res
}

/// One request for the export route; `path` is the WHATWG pathname.
pub async fn serve(
    app: &Arc<App>,
    conn: crate::serve::Conn,
    req: Request<Body>,
    path: String,
) -> Response<Body> {
    // Node matches the exact pathname and answers other methods with 405 itself.
    if !app.config.rust_export || path != ROUTE || req.method() != Method::GET {
        return app.proxy.forward(conn.peer, req).await;
    }
    let Some(data_dir) = app.config.data_dir.clone() else {
        return identity::Rejection::Unavailable.into_response();
    };
    let (parts, _body) = req.into_parts();
    let creds = identity::creds_of(&parts);
    let layer = Arc::clone(&app.identity);
    let now = identity::now_ms();
    let authn =
        match tokio::task::spawn_blocking(move || layer.gate_blocking(&creds, ROUTE, now)).await {
            Ok(Ok(id)) => id,
            Ok(Err(rejection)) => return rejection.into_response(),
            Err(_) => return identity::Rejection::Unavailable.into_response(),
        };
    let Ok(permit) = Arc::clone(&app.export).acquire_owned().await else {
        return reply::error(StatusCode::INTERNAL_SERVER_ERROR, "Internal error", true);
    };
    let user = authn.user.id;
    let worker = Arc::clone(app);
    let done = tokio::task::spawn_blocking(move || {
        let _permit = permit;
        let when = identity::now_ms();
        let done = match export(&data_dir, &user, when) {
            Outcome::Zip { chunks, len, day } => match audit(&worker, &user, len) {
                Ok(()) => return Done::Zip { chunks, len, day },
                Err(_) => Done::Internal,
            },
            Outcome::TooLarge(message) => Done::TooLarge(message),
            Outcome::Internal => Done::Internal,
        };
        // Nothing is sent: what the export built is freed (a sent ZIP is trimmed by its body).
        server_export::route::return_memory();
        done
    })
    .await;
    match done {
        Ok(Done::Zip { chunks, len, day }) => download(chunks, len, &day),
        Ok(Done::TooLarge(message)) => reply::error(StatusCode::PAYLOAD_TOO_LARGE, &message, true),
        Ok(Done::Internal) | Err(_) => {
            reply::error(StatusCode::INTERNAL_SERVER_ERROR, "Internal error", true)
        }
    }
}
