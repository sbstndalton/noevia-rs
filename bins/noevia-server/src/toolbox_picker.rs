//! #2281: the toolbox picker is answered here; Node only supplies its live discovery/prefill facts.
use crate::{identity, reply, App};
use axum::{
    body::Body,
    http::{Method, Request, Response, StatusCode},
    response::IntoResponse,
};
use js_json::wtf16;
use std::{collections::HashSet, sync::Arc, time::Duration};

pub const PATH: &str = "/api/toolboxes";
const FACTS_PATH: &str = "/internal/toolbox-picker/facts";
const DEADLINE: Duration = Duration::from_secs(30);
const MAX_BYTES: usize = 8 * 1024 * 1024;
const UNAVAILABLE: &str = "The tool list is unavailable. Try again in a moment.";

fn error(status: StatusCode, message: &str) -> Response<Body> {
    reply::error(status, message, true)
}

pub async fn serve(
    app: &Arc<App>,
    conn: crate::serve::Conn,
    req: Request<Body>,
    path: String,
) -> Response<Body> {
    if path != PATH {
        return app.proxy.forward(conn.peer, req).await;
    }
    let (parts, _) = req.into_parts();
    let creds = identity::creds_of(&parts);
    let gate = Arc::clone(&app.identity);
    let id = match tokio::task::spawn_blocking(move || {
        gate.gate_blocking(&creds, PATH, identity::now_ms())
    })
    .await
    {
        Ok(Ok(id)) => id,
        Ok(Err(rejected)) => return rejected.into_response(),
        Err(_) => return identity::Rejection::Unavailable.into_response(),
    };
    if parts.method != Method::GET {
        return error(StatusCode::METHOD_NOT_ALLOWED, "method not allowed");
    }
    let Some((200, bytes)) = app
        .config
        .research
        .node_call(FACTS_PATH, &serde_json::json!({}), DEADLINE)
        .await
    else {
        return error(StatusCode::SERVICE_UNAVAILABLE, UNAVAILABLE);
    };
    if bytes.len() > MAX_BYTES {
        return error(StatusCode::SERVICE_UNAVAILABLE, UNAVAILABLE);
    }
    let Some(facts) = std::str::from_utf8(&bytes)
        .ok()
        .and_then(|s| wtf16::parse(s).ok())
    else {
        return error(StatusCode::SERVICE_UNAVAILABLE, UNAVAILABLE);
    };
    // The signed-in identity decides connector visibility; neither the bridge nor request parameters do.
    let (a, user, admin) = (
        Arc::clone(app),
        id.user.id.clone(),
        id.user.role == server_auth::Role::Admin,
    );
    let connected = match tokio::task::spawn_blocking(move || -> Result<HashSet<String>, ()> {
        let Some(drive) = crate::gdrive::of(&a) else {
            return Ok(HashSet::new());
        };
        let slot = drive.slot_for(&user, admin).map_err(|_| ())?;
        Ok(if drive.state(&slot).state == "connected" {
            HashSet::from(["gdrive".to_owned()])
        } else {
            HashSet::new()
        })
    })
    .await
    {
        Ok(Ok(c)) => c,
        _ => return error(StatusCode::SERVICE_UNAVAILABLE, UNAVAILABLE),
    };
    let built = tokio::task::spawn_blocking(move || {
        server_projects::toolbox_picker::compose(&facts, &connected)
    })
    .await;
    match built {
        Ok(Ok(body)) => match wtf16::stringify(&body) {
            Some(json) => reply::json_text(StatusCode::OK, json, true),
            None => error(StatusCode::SERVICE_UNAVAILABLE, UNAVAILABLE),
        },
        _ => error(StatusCode::SERVICE_UNAVAILABLE, UNAVAILABLE),
    }
}
