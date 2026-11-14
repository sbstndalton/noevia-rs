//! M6 native reasoning default PUT only; identity and CSRF precede method/body policy.
//!
//! Track R12: with NOEVIA_RUST_SETTINGS_ROUTES=1 the dispatch (lib.rs) also hands GET here, answered as
//! core routes/reasoning-settings.cjs does (server-reasoning-settings `read`): the account default
//! (the settings row, `|| 'default'`), the project's resolved effort (`?projectId`, 404 for none) and
//! the provider mode. Unset, the GET is proxied to Node as before.
use crate::{identity, reply, App};
use axum::{
    body::Body,
    http::{Request, Response, StatusCode},
    response::IntoResponse,
};
use http_body_util::BodyExt;
use std::{sync::Arc, time::Duration};
fn error(status: StatusCode, msg: &str) -> Response<Body> {
    reply::error(status, msg, true)
}
async fn bounded_body(mut body: Body) -> Result<Vec<u8>, ()> {
    let mut out = Vec::new();
    while let Some(frame) = body.frame().await {
        if let Ok(data) = frame.map_err(|_| ())?.into_data() {
            let remaining = server_reasoning_settings::BODY_LIMIT + 1 - out.len();
            out.try_reserve(remaining.min(data.len())).map_err(|_| ())?;
            out.extend_from_slice(data.get(..remaining.min(data.len())).ok_or(())?);
            if out.len() > server_reasoning_settings::BODY_LIMIT {
                return Err(());
            }
        }
    }
    Ok(out)
}
async fn read_body(body: Body, deadline: Duration) -> Result<Vec<u8>, ()> {
    tokio::time::timeout(deadline, bounded_body(body))
        .await
        .map_err(|_| ())?
}
pub const ROUTE: &str = "/api/reasoning-settings";

pub async fn serve(
    app: &Arc<App>,
    conn: crate::serve::Conn,
    req: Request<Body>,
    path: String,
) -> Response<Body> {
    if path != "/api/reasoning-settings" {
        return app.proxy.forward(conn.peer, req).await;
    }
    let (parts, body) = req.into_parts();
    let Ok(permit) = Arc::clone(&app.reasoning_settings).try_acquire_owned() else {
        return error(
            StatusCode::SERVICE_UNAVAILABLE,
            "Reasoning settings unavailable",
        );
    };
    let creds = identity::creds_of(&parts);
    let layer = Arc::clone(&app.identity);
    let now = identity::now_ms();
    let id = match tokio::task::spawn_blocking(move || {
        layer.gate_blocking(&creds, "/api/reasoning-settings", now)
    })
    .await
    {
        Ok(Ok(id)) => id,
        Ok(Err(r)) => return r.into_response(),
        Err(_) => return identity::Rejection::Unavailable.into_response(),
    };
    if parts.method == axum::http::Method::GET && app.config.settings_routes {
        return get(app, &parts, id, now, permit).await;
    }
    if id.user.role != server_auth::Role::Admin {
        return error(StatusCode::FORBIDDEN, "Administrator required");
    }
    // Only PUT is owned. Never route capability reads or unrelated methods through this handler.
    if parts.method != axum::http::Method::PUT {
        return error(StatusCode::METHOD_NOT_ALLOWED, "Method not allowed");
    }
    let bytes = match read_body(body, Duration::from_secs(10)).await {
        Ok(bytes) => bytes,
        Err(()) => return error(StatusCode::BAD_REQUEST, "invalid JSON"),
    };
    let effort = match server_reasoning_settings::input::parse(&bytes) {
        Ok(effort) => effort,
        Err(server_reasoning_settings::input::Error::Json) => {
            return error(StatusCode::BAD_REQUEST, "invalid JSON")
        }
        Err(server_reasoning_settings::input::Error::Default) => {
            return error(
                StatusCode::BAD_REQUEST,
                "default must be default, low or high",
            )
        }
        Err(server_reasoning_settings::input::Error::Null) => {
            return error(StatusCode::INTERNAL_SERVER_ERROR, "Internal error")
        }
    };
    let app = Arc::clone(app);
    let result = tokio::task::spawn_blocking(
        move || -> Result<serde_json::Value, server_store::StoreError> {
            let _permit = permit;
            let token = app
                .config
                .rust_reasoning_settings
                .ok_or_else(|| server_store::StoreError::NotOwned("reasoning settings".into()))?;
            server_reasoning_settings::put(
                app.writes.writer()?.as_ref(),
                token,
                &id.user.id,
                effort,
                now,
            )
        },
    )
    .await;
    match result {
        Ok(Ok(v)) => reply::json_text(StatusCode::OK, v.to_string(), true),
        Ok(Err(server_store::StoreError::Sqlite(_))) => {
            error(StatusCode::INTERNAL_SERVER_ERROR, "Internal error")
        }
        _ => error(
            StatusCode::SERVICE_UNAVAILABLE,
            "Reasoning settings unavailable",
        ),
    }
}

fn unavailable() -> Response<Body> {
    error(
        StatusCode::SERVICE_UNAVAILABLE,
        "Reasoning settings unavailable",
    )
}

/// A provider file for [`server_reasoning_settings::read::provider`].
fn provider_file(path: &std::path::Path) -> server_reasoning_settings::read::File {
    use server_reasoning_settings::read::File;
    match std::fs::read(path) {
        Ok(bytes) => File::Bytes(bytes),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => File::Missing,
        Err(_) => File::Unreadable,
    }
}

/// `GET /api/reasoning-settings` (NOEVIA_RUST_SETTINGS_ROUTES=1).
async fn get(
    app: &Arc<App>,
    parts: &axum::http::request::Parts,
    id: server_auth::Identity,
    now: i64,
    permit: tokio::sync::OwnedSemaphorePermit,
) -> Response<Body> {
    use server_reasoning_settings::read;
    let project_id = read::project_param(parts.uri.query());
    let Some(data_dir) = app.config.data_dir.clone() else {
        return unavailable();
    };
    let app = Arc::clone(app);
    let result = tokio::task::spawn_blocking(move || -> Response<Body> {
        let _permit = permit;
        let Ok(store) = app.identity.store_blocking() else {
            return unavailable();
        };
        // `db.prepare("SELECT value FROM settings WHERE key='reasoning_effort_default'").get()?.value || 'default'`;
        // only the native PUT writes the row, always as text.
        let Ok(stored) = store.read(|r| {
            r.row(
                "SELECT CASE WHEN typeof(value)='text' THEN value ELSE NULL END FROM settings WHERE key=?1",
                [server_reasoning_settings::KEY],
                |row| row.get::<_, Option<String>>(0),
            )
        }) else {
            return unavailable();
        };
        let global = stored.flatten().filter(|v| !v.is_empty()).unwrap_or_else(|| "default".into());
        let project = match project_id {
            None => None,
            Some(pid) => {
                let ctx = server_projects::Ctx {
                    data_dir: &data_dir,
                    user_id: &id.user.id,
                    switch: app.config.rust_projects,
                    now_ms: now,
                };
                match server_projects::chats::get_project(ctx, &pid) {
                    Ok(Some(p)) => Some(p),
                    Ok(None) => return error(StatusCode::NOT_FOUND, "no such project"),
                    // A list Node's load reads as empty (not JSON, `projects` not a list): no project.
                    Err(_)
                        if server_projects::store::user_dir(&data_dir, &id.user.id)
                            .is_ok_and(|d| crate::routing_default_routes::unreadable(&d)) =>
                    {
                        return error(StatusCode::NOT_FOUND, "no such project")
                    }
                    Err(reply) => return crate::project_routes::respond(reply),
                }
            }
        };
        let defaults = &app.config.reasoning_defaults;
        let wanted = project
            .as_ref()
            .map(|p| p.get("provider"))
            .filter(|v| v.truthy())
            .cloned()
            .unwrap_or_else(|| js_json::JValue::Str(defaults.id.clone()));
        let Ok(user_dir) = server_projects::store::user_dir(&data_dir, &id.user.id) else {
            return unavailable();
        };
        let shared = provider_file(&data_dir.join(server_store::ROOT_PROVIDER_FILE));
        let private = provider_file(&user_dir.join(server_projects::provider_rows::FILE));
        let Ok(provider) = read::provider(defaults, &shared, &private, &wanted) else {
            return unavailable();
        };
        let effort = read::resolve_effort(project.as_ref(), &global);
        let model = project
            .as_ref()
            .map(|p| p.get("model"))
            .filter(|v| v.truthy())
            .cloned()
            .unwrap_or_else(|| js_json::JValue::Str(String::new()));
        let Ok(mode) = read::mode_for(provider.as_ref(), &model, &effort, defaults) else {
            return error(StatusCode::INTERNAL_SERVER_ERROR, "Internal error");
        };
        let admin = id.user.role == server_auth::Role::Admin;
        reply::json_text(
            StatusCode::OK,
            read::answer(&global, &effort, mode, admin),
            true,
        )
    })
    .await;
    result.unwrap_or_else(|_| unavailable())
}

#[cfg(all(test, feature = "rust-reasoning-settings"))]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use bytes::Bytes;
    use http_body_util::channel::Channel;
    #[tokio::test]
    async fn reasoning_body_read_error_caps_and_deadline() {
        let (mut sender, body) = Channel::<Bytes, std::io::Error>::new(2);
        sender
            .send_data(Bytes::from_static(b"{\"default\":\"high\"}"))
            .await
            .unwrap();
        sender.abort(std::io::Error::other("synthetic body error"));
        assert!(read_body(Body::new(body), Duration::from_secs(1))
            .await
            .is_err());
        let (mut sender, body) = Channel::<Bytes, std::io::Error>::new(2);
        sender
            .send_data(Bytes::from(vec![
                b'a';
                server_reasoning_settings::BODY_LIMIT
            ]))
            .await
            .unwrap();
        sender.send_data(Bytes::from_static(b"x")).await.unwrap();
        drop(sender);
        assert!(read_body(Body::new(body), Duration::from_secs(1))
            .await
            .is_err());
        let (_sender, body) = Channel::<Bytes, std::io::Error>::new(1);
        assert!(read_body(Body::new(body), Duration::from_millis(5))
            .await
            .is_err());
    }
    #[tokio::test]
    async fn reasoning_admission_refuses_fifth_and_releases_on_exit() {
        let d = tempfile::tempdir().unwrap();
        let config = crate::config::Config::from_lookup(|key| match key {
            "UI_DATA_DIR" => Some(d.path().display().to_string()),
            "NOEVIA_LEGACY_UPSTREAM" => Some("http://127.0.0.1:9".into()),
            "NOEVIA_RUST_AUTH" | "NOEVIA_RUST_SAMPLING_SETTINGS" => Some("1".into()),
            _ => None,
        })
        .unwrap();
        let app = Arc::new(App::new(config));
        let permits = (0..4)
            .map(|_| {
                Arc::clone(&app.reasoning_settings)
                    .try_acquire_owned()
                    .unwrap()
            })
            .collect::<Vec<_>>();
        let conn = crate::serve::Conn {
            peer: "127.0.0.1:18021".parse().unwrap(),
            local: None,
        };
        let response = serve(
            &app,
            conn,
            Request::new(Body::empty()),
            "/api/reasoning-settings".into(),
        )
        .await;
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&bytes).unwrap(),
            serde_json::json!({"error":"Reasoning settings unavailable"})
        );
        drop(permits);
        assert_eq!(app.reasoning_settings.available_permits(), 4);
        let conn = crate::serve::Conn {
            peer: "127.0.0.1:18021".parse().unwrap(),
            local: None,
        };
        let _ = serve(
            &app,
            conn,
            Request::new(Body::empty()),
            "/api/reasoning-settings".into(),
        )
        .await;
        assert_eq!(app.reasoning_settings.available_permits(), 4);
    }
}
