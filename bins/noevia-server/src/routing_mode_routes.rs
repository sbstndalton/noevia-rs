//! Native routing configuration; auth precedes method, product feature, body and admin policy.
use crate::{identity, reply, App};
use axum::{
    body::Body,
    http::{Request, Response, StatusCode},
    response::IntoResponse,
};
use http_body_util::BodyExt;
use server_routing_mode::{self as rm, input::Json};
use std::{
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::Duration,
};
struct Cancel(Arc<AtomicBool>);
impl Drop for Cancel {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Release);
    }
}
fn error(status: StatusCode, msg: &str) -> Response<Body> {
    reply::error(status, msg, true)
}
async fn bounded_body(mut body: Body) -> Result<Vec<u8>, ()> {
    let mut out = Vec::new();
    while let Some(frame) = body.frame().await {
        if let Ok(data) = frame.map_err(|_| ())?.into_data() {
            let take = data.len().min(rm::BODY_LIMIT + 1 - out.len());
            out.try_reserve(take).map_err(|_| ())?;
            out.extend_from_slice(data.get(..take).ok_or(())?);
            if out.len() > rm::BODY_LIMIT {
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
pub async fn serve(
    app: &Arc<App>,
    conn: crate::serve::Conn,
    req: Request<Body>,
    path: String,
) -> Response<Body> {
    if path != "/api/routing-mode" && path != "/api/routing-mode/allowed" {
        return app.proxy.forward(conn.peer, req).await;
    }
    let (parts, body) = req.into_parts();
    let Ok(permit) = Arc::clone(&app.routing_mode).try_acquire_owned() else {
        return error(
            StatusCode::SERVICE_UNAVAILABLE,
            "Routing settings unavailable",
        );
    };
    // Copies retained by each blocking operation prevent cancelled requests releasing admission
    // while their bounded jobs still run.
    let permit = Arc::new(permit);
    let cancel = Cancel(Arc::new(AtomicBool::new(false)));
    let creds = identity::creds_of(&parts);
    let layer = Arc::clone(&app.identity);
    let auth_permit = Arc::clone(&permit);
    let allowed_path = path == "/api/routing-mode/allowed";
    let now = identity::now_ms();
    let id = match tokio::task::spawn_blocking(move || {
        let _p = auth_permit;
        layer.gate_blocking(&creds, &path, now)
    })
    .await
    {
        Ok(Ok(id)) => id,
        Ok(Err(r)) => return r.into_response(),
        _ => return identity::Rejection::Unavailable.into_response(),
    };
    let admin = id.user.role == server_auth::Role::Admin;
    if parts.method != axum::http::Method::PUT
        && !(parts.method == axum::http::Method::GET && !allowed_path)
    {
        return error(StatusCode::METHOD_NOT_ALLOWED, "Method not allowed");
    }
    let a = Arc::clone(app);
    let p = Arc::clone(&permit);
    let enabled = match tokio::task::spawn_blocking(move || {
        let _p = p;
        a.identity
            .store_blocking()?
            .read(|r| rm::enabled(r, a.config.routing_feature_env))
    })
    .await
    {
        Ok(Ok(v)) => v,
        _ => {
            return error(
                StatusCode::SERVICE_UNAVAILABLE,
                "Routing settings unavailable",
            )
        }
    };
    if !enabled {
        return if parts.method == axum::http::Method::GET {
            reply::json_text(StatusCode::OK, "{\"enabled\":false}".into(), true)
        } else {
            error(
                StatusCode::CONFLICT,
                "Routing modes are turned off on this server.",
            )
        };
    }
    let parsed = if parts.method == axum::http::Method::PUT {
        match read_body(body, Duration::from_secs(10))
            .await
            .and_then(|b| Json::parse(&b).map_err(|_| ()))
        {
            Ok(j) => Some(j),
            Err(()) => return error(StatusCode::BAD_REQUEST, "invalid JSON"),
        }
    } else {
        None
    };
    if allowed_path && !admin {
        return error(StatusCode::FORBIDDEN, "Administrator required");
    }
    let a = Arc::clone(app);
    let c = Arc::clone(&cancel.0);
    let result = tokio::task::spawn_blocking(move || -> Result<serde_json::Value, rm::Error> {
        let _p = permit;
        let unavailable = || rm::Error {
            status: 503,
            message: "Routing settings unavailable".into(),
        };
        let sql_error = |e: server_store::StoreError| match e {
            server_store::StoreError::Sqlite(_) => rm::Error {
                status: 500,
                message: "Internal error".into(),
            },
            _ => unavailable(),
        };
        let cap = a.config.rust_routing_mode.ok_or_else(unavailable)?;
        if c.load(Ordering::Acquire) {
            return Err(unavailable());
        }
        if allowed_path {
            let j = parsed.ok_or_else(unavailable)?;
            let modes = rm::validate_allowed(&j)?;
            if c.load(Ordering::Acquire) {
                return Err(unavailable());
            }
            return rm::put_allowed(
                a.writes.writer().map_err(sql_error)?.as_ref(),
                cap,
                &id.user.id,
                modes,
                now,
            )
            .map_err(sql_error);
        }
        let file = a
            .routing_files
            .as_ref()
            .and_then(|v| v.as_ref().ok())
            .ok_or_else(unavailable)?;
        let store = a.identity.store_blocking().map_err(sql_error)?;
        let allowed = store.read(rm::allowed).map_err(sql_error)?;
        let setting = if let Some(j) = parsed {
            let next = rm::validate(&j, &allowed)?;
            file.write(&id.user.id, &next, cap, &c)
                .map_err(|_| unavailable())?;
            next
        } else {
            file.read(&id.user.id, &c).map_err(|_| unavailable())?
        };
        // Actual Node reads the allowed list again after writing the account setting.
        let allowed = store.read(rm::allowed).map_err(sql_error)?;
        Ok(setting.view(allowed, admin))
    })
    .await;
    match result {
        Ok(Ok(v)) => reply::json_text(StatusCode::OK, v.to_string(), true),
        Ok(Err(e)) => error(
            StatusCode::from_u16(e.status).unwrap_or(StatusCode::SERVICE_UNAVAILABLE),
            &e.message,
        ),
        _ => error(
            StatusCode::SERVICE_UNAVAILABLE,
            "Routing settings unavailable",
        ),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use bytes::Bytes;
    use http_body_util::channel::Channel;
    #[tokio::test]
    async fn bounded_stream_error_limit_and_deadline() {
        let (mut sender, body) = Channel::<Bytes, std::io::Error>::new(2);
        sender.send_data(Bytes::from_static(b"{}")).await.unwrap();
        sender.abort(std::io::Error::other("synthetic read error"));
        assert!(read_body(Body::new(body), Duration::from_secs(1))
            .await
            .is_err());
        let (mut sender, body) = Channel::<Bytes, std::io::Error>::new(2);
        sender
            .send_data(Bytes::from(vec![b' '; rm::BODY_LIMIT + 1]))
            .await
            .unwrap();
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
    async fn fifth_admission_refuses_and_permits_release() {
        let d = tempfile::tempdir().unwrap();
        let config = crate::config::Config::from_lookup(|k| match k {
            "UI_DATA_DIR" => Some(d.path().display().to_string()),
            "NOEVIA_LEGACY_UPSTREAM" => Some("http://127.0.0.1:9".into()),
            _ => None,
        })
        .unwrap();
        let app = crate::App::new(config);
        let permits = (0..4)
            .map(|_| Arc::clone(&app.routing_mode).try_acquire_owned().unwrap())
            .collect::<Vec<_>>();
        let conn = crate::serve::Conn {
            peer: "127.0.0.1:18021".parse().unwrap(),
            local: None,
        };
        let response = serve(
            &app,
            conn,
            Request::new(Body::empty()),
            "/api/routing-mode".into(),
        )
        .await;
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        drop(permits);
        assert_eq!(app.routing_mode.available_permits(), 4);
        let conn = crate::serve::Conn {
            peer: "127.0.0.1:18021".parse().unwrap(),
            local: None,
        };
        let response = serve(
            &app,
            conn,
            Request::new(Body::empty()),
            "/api/routing-mode".into(),
        )
        .await;
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(app.routing_mode.available_permits(), 4);
    }
}
