//! M4 (NOEVIA_RUST_PROJECTS=1, with NOEVIA_RUST_AUTH=1): the front answers a project's image
//! routes itself (crates/server-projects `assets`) instead of proxying them. The request passes
//! Node's gate first (server-auth: session, origin + CSRF for writes, device-token limits), here
//! on the WHATWG pathname Node routes on; the user it returns is the only one whose files are
//! touched. A path the routes.toml pattern matches but Node's own route regex would not (a
//! trailing slash, an empty segment) still goes to Node, as before.

use crate::identity::{self, Rejection};
use crate::reply;
use crate::App;
use axum::body::Body;
use axum::http::{HeaderName, HeaderValue, Request, Response, StatusCode};
use axum::response::IntoResponse;
use server_projects::{assets, Ctx, Reply};
use std::sync::Arc;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Route {
    /// POST /api/projects/{id}/assets
    Upload { project: String },
    /// GET or DELETE /api/projects/{id}/assets/{assetId}
    One { project: String, asset: String },
}

/// Node's matchers, `^\/api\/projects\/([^/]+)\/assets$` and `...\/assets\/([^/]+)$`, with the
/// methods its handlers answer; `None` sends the request on to Node.
pub fn route(method: &str, path: &str) -> Option<Route> {
    let rest = path.strip_prefix("/api/projects/")?;
    let parts: Vec<&str> = rest.split('/').collect();
    match (method, parts.as_slice()) {
        ("POST", [project, "assets"]) if !project.is_empty() => Some(Route::Upload {
            project: (*project).to_string(),
        }),
        ("GET" | "DELETE", [project, "assets", asset])
            if !project.is_empty() && !asset.is_empty() =>
        {
            Some(Route::One {
                project: (*project).to_string(),
                asset: (*asset).to_string(),
            })
        }
        _ => None,
    }
}

fn respond(r: Reply) -> Response<Body> {
    let status = StatusCode::from_u16(r.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    match r.body {
        server_projects::Body::Json(text) => reply::json_text(status, text, true),
        server_projects::Body::Bytes { headers, bytes } => {
            let mut res = Response::new(Body::from(bytes));
            *res.status_mut() = status;
            let h = res.headers_mut();
            reply::security_headers(h, true);
            for (k, v) in headers {
                // The route's own headers win, as in Node's writeHead (its sandbox CSP).
                let (Ok(k), Ok(v)) = (
                    HeaderName::from_bytes(k.as_bytes()),
                    HeaderValue::from_str(&v),
                ) else {
                    return reply::error(StatusCode::INTERNAL_SERVER_ERROR, "Internal error", true);
                };
                h.insert(k, v);
            }
            res
        }
    }
}

/// One request for a Rust-owned project route; `path` is the WHATWG pathname.
pub async fn serve(
    app: &Arc<App>,
    conn: crate::serve::Conn,
    req: Request<Body>,
    path: String,
) -> Response<Body> {
    let (parts, body) = req.into_parts();
    let Some(route) = route(parts.method.as_str(), &path) else {
        return app
            .proxy
            .forward(conn.peer, Request::from_parts(parts, body))
            .await;
    };
    let (Some(switch), Some(data_dir)) = (app.config.rust_projects, app.config.data_dir.clone())
    else {
        return Rejection::Unavailable.into_response();
    };
    let creds = identity::creds_of(&parts);
    let layer = Arc::clone(&app.identity);
    let now = identity::now_ms();
    let gate_path = path.clone();
    let authn =
        match tokio::task::spawn_blocking(move || layer.gate_blocking(&creds, &gate_path, now))
            .await
        {
            Ok(Ok(id)) => id,
            Ok(Err(rejection)) => return rejection.into_response(),
            Err(_) => return Rejection::Unavailable.into_response(),
        };
    let upload = match &route {
        Route::Upload { .. } => match crate::account_routes::read_body(body, assets::BODY_LIMIT)
            .await
        {
            Ok(b) => Some(b),
            Err(()) => return reply::error(StatusCode::BAD_REQUEST, "invalid request body", true),
        },
        Route::One { .. } => None,
    };
    let method = parts.method.clone();
    let user = authn.user.id;
    let done = tokio::task::spawn_blocking(move || {
        let ctx = Ctx {
            data_dir: &data_dir,
            user_id: &user,
            switch,
            now_ms: now,
        };
        match (route, upload) {
            (Route::Upload { project }, Some(b)) => assets::post(ctx, &project, &b.bytes, b.over),
            (Route::One { project, asset }, _) if method == "DELETE" => {
                assets::delete(ctx, &project, &asset)
            }
            (Route::One { project, asset }, _) => assets::get(ctx, &project, &asset),
            (Route::Upload { .. }, None) => Reply::internal(),
        }
    })
    .await;
    match done {
        Ok(r) => respond(r),
        Err(_) => reply::error(StatusCode::INTERNAL_SERVER_ERROR, "Internal error", true),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nodes_matchers_and_methods() {
        assert_eq!(
            route("POST", "/api/projects/p%201/assets"),
            Some(Route::Upload {
                project: "p%201".into()
            })
        );
        assert_eq!(
            route("DELETE", "/api/projects/p/assets/img-1"),
            Some(Route::One {
                project: "p".into(),
                asset: "img-1".into()
            })
        );
        for (m, p) in [
            ("POST", "/api/projects/p/assets/"),
            ("POST", "/api/projects//assets"),
            ("GET", "/api/projects/p/assets"),
            ("GET", "/api/projects/p/assets/"),
            ("GET", "/api/projects/p/assets/a/b"),
            ("PUT", "/api/projects/p/assets/a"),
            ("POST", "/api/projects/p/assets/a"),
            ("GET", "/api/projectsx/p/assets/a"),
        ] {
            assert_eq!(route(m, p), None, "{m} {p}");
        }
    }
}
