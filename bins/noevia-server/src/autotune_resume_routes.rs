//! Native resume policy; existing actor preparation and atomic launch retain live state ownership.
use crate::{identity, reply, App};
use axum::{
    body::Body,
    http::{header, HeaderValue, Method, Request, Response, StatusCode},
    response::IntoResponse,
};
use http_body_util::BodyExt;
use js_json::{wtf16, JValue};
use server_projects::autotune_resume::{self as policy, Fault};
use std::{sync::Arc, time::Duration};
pub const PATH: &str = "/api/models/autotune/resume";
const FACTS: &str = "/internal/autotune-resume/facts";
const PREPARE: &str = "/internal/autotune-resume/prepare";
const COMMIT: &str = "/internal/autotune-resume/commit";
const MAX: usize = 8 * 1024 * 1024;
fn error(status: u16, message: &str) -> Response<Body> {
    reply::error(
        StatusCode::from_u16(status).unwrap_or(StatusCode::SERVICE_UNAVAILABLE),
        message,
        true,
    )
}
fn fault(f: Fault) -> Response<Body> {
    error(f.status(), f.message())
}
fn stale() -> Response<Body> {
    error(409, "The auto-tune changed before resuming. Try again.")
}
fn opaque<'a>(facts: &'a JValue, key: &str) -> Option<&'a str> {
    facts.get(key).as_str().filter(|s| {
        s.len() == 64
            && s.bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    })
}
async fn call(app: &App, path: &str, body: &serde_json::Value) -> Option<(u16, JValue)> {
    let timeout = if path == PREPARE { 125 } else { 5 };
    let (status, bytes) = app
        .config
        .research
        .node_call(path, body, Duration::from_secs(timeout))
        .await?;
    if bytes.len() > MAX {
        return None;
    }
    Some((
        status,
        wtf16::parse(std::str::from_utf8(&bytes).ok()?).ok()?,
    ))
}
fn bridge_json(v: &JValue) -> Option<serde_json::Value> {
    serde_json::from_str(&wtf16::stringify(v)?).ok()
}
async fn input(mut body: Body) -> Result<JValue, Box<Response<Body>>> {
    let read = async {
        let mut bytes = Vec::new();
        while let Some(frame) = body.frame().await {
            let frame = frame.map_err(|_| Box::new(error(400, "invalid JSON")))?;
            if let Ok(data) = frame.into_data() {
                if data.len() > 1024usize.saturating_mul(1024).saturating_sub(bytes.len()) {
                    return Err(Box::new(error(413, "Request exceeds size limit")));
                }
                bytes.extend_from_slice(&data);
            }
        }
        if bytes.is_empty() {
            return Ok(JValue::Obj(Vec::new()));
        }
        wtf16::parse(&String::from_utf8_lossy(&bytes))
            .map_err(|_| Box::new(error(400, "invalid JSON")))
    };
    tokio::time::timeout(Duration::from_secs(120), read)
        .await
        .unwrap_or_else(|_| Err(Box::new(error(408, "Request timed out"))))
}
fn private_key(key: &str) -> bool {
    key.starts_with('_')
        || matches!(
            key,
            "originalText" | "lastRevision" | "originalRevision" | "beforeText"
        )
}
fn http_config(key: &str) -> bool {
    matches!(
        key,
        "headers"
            | "authorization"
            | "apiKey"
            | "api_key"
            | "accessToken"
            | "refreshToken"
            | "httpConfig"
            | "requestConfig"
            | "baseUrl"
            | "url"
    )
}
fn public_tree(value: &JValue) -> Option<JValue> {
    match value {
        JValue::Obj(fields) => {
            let mut selected = Vec::new();
            for (key, value) in fields {
                if private_key(key) {
                    continue;
                }
                if http_config(key) {
                    return None;
                }
                selected.push((key.clone(), public_tree(value)?));
            }
            Some(JValue::Obj(selected))
        }
        JValue::Arr(values) => Some(JValue::Arr(
            values.iter().map(public_tree).collect::<Option<Vec<_>>>()?,
        )),
        other => Some(other.clone()),
    }
}
fn public_job(job: &JValue) -> Option<JValue> {
    if !matches!(job, JValue::Obj(_))
        || job.get("id").as_str().is_none_or(str::is_empty)
        || job.get("model").as_str().is_none_or(str::is_empty)
        || !matches!(
            job.get("status").as_str(),
            Some("running" | "passed" | "failed" | "cancelled" | "interrupted")
        )
    {
        return None;
    }
    const KEYS: &[&str] = &[
        "id",
        "model",
        "bulk",
        "mode",
        "promptBudgetSeconds",
        "status",
        "phase",
        "startedAt",
        "log",
        "models",
        "queueProgress",
        "finishedAt",
        "error",
        "failedPhase",
        "failedStep",
        "restored",
        "skipped",
        "waiting",
        "result",
    ];
    let mut fields = Vec::new();
    for key in KEYS {
        let value = job.get(key);
        if !matches!(value, JValue::Undefined) {
            fields.push(((*key).to_owned(), value.clone()));
        }
    }
    public_tree(&JValue::Obj(fields))
}
fn public_response(status: u16, result: &JValue) -> Response<Body> {
    let Some(job) = public_job(result.get("publicJob")) else {
        return fault(Fault::State);
    };
    let Some(body) = wtf16::stringify(&job) else {
        return fault(Fault::State);
    };
    let mut response = reply::json_text(
        StatusCode::from_u16(status).unwrap_or(StatusCode::ACCEPTED),
        body,
        true,
    );
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}
fn stage_result(status: u16, result: &JValue, generation: &str) -> Result<(), Box<Response<Body>>> {
    if result.get("fault").as_str() == Some("reload") {
        return Err(Box::new(error(
            409,
            "The model server has not re-read the restored settings yet. Try again shortly.",
        )));
    }
    if status == 409 && result.get("applied").as_bool() == Some(false) {
        return Err(Box::new(stale()));
    }
    if status != 200 || result.get("applied").as_bool() != Some(true) {
        return Err(Box::new(fault(Fault::State)));
    }
    if opaque(result, "generation") != Some(generation) {
        return Err(Box::new(stale()));
    }
    Ok(())
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
    let (parts, body) = req.into_parts();
    let creds = identity::creds_of(&parts);
    let gate = Arc::clone(&app.identity);
    let id = match tokio::task::spawn_blocking(move || {
        gate.gate_blocking(&creds, PATH, identity::now_ms())
    })
    .await
    {
        Ok(Ok(id)) => id,
        Ok(Err(r)) => return r.into_response(),
        Err(_) => return identity::Rejection::Unavailable.into_response(),
    };
    if id.user.role != server_auth::Role::Admin {
        return error(
            403,
            if parts.method == Method::GET || parts.method == Method::HEAD {
                "Administrator required for shared model profiles"
            } else {
                "administrator required"
            },
        );
    }
    let Some((200, mut facts)) = call(app, FACTS, &serde_json::json!({})).await else {
        return fault(Fault::State);
    };
    match facts.get("available").as_bool() {
        Some(false) => return error(404, "Auto-tune is unavailable"),
        Some(true) => {}
        None => return fault(Fault::State),
    }
    if parts.method != Method::POST {
        return error(405, "Method not allowed");
    }
    let body = match input(body).await {
        Ok(b) => b,
        Err(r) => return *r,
    };
    if let Err(e) = policy::validate(body.get("confirmPause"), &facts) {
        return fault(e);
    }
    let Some(generation) = opaque(&facts, "generation").map(str::to_owned) else {
        return fault(Fault::State);
    };
    let Some(token) = opaque(&facts, "token") else {
        return fault(Fault::State);
    };
    let Some((status, listing)) = call(
        app,
        PREPARE,
        &serde_json::json!({"token":token,"stage":"listing"}),
    )
    .await
    else {
        return fault(Fault::State);
    };
    if let Err(r) = stage_result(status, &listing, &generation) {
        return *r;
    }
    if let Err(e) = policy::validate(body.get("confirmPause"), &listing) {
        return fault(e);
    }
    let filter = match policy::filter(&listing, listing.get("models")) {
        Ok(p) => p,
        Err(e) => return fault(e),
    };
    let mut rows = match listing.get("models") {
        JValue::Arr(a) => a.clone(),
        _ => return fault(Fault::State),
    };
    facts = listing;
    if let Some(plan) = filter {
        let Some(token) = opaque(&facts, "token") else {
            return fault(Fault::State);
        };
        let Some(patch) = bridge_json(&plan) else {
            return fault(Fault::State);
        };
        let Some((status, result)) = call(
            app,
            COMMIT,
            &serde_json::json!({"token":token,"plan":patch}),
        )
        .await
        else {
            return fault(Fault::State);
        };
        if let Err(r) = stage_result(status, &result, &generation) {
            return *r;
        }
        if plan.get("complete").as_bool() == Some(true) {
            return public_response(200, &result);
        }
        let JValue::Arr(kept) = plan.get("keep") else {
            return fault(Fault::State);
        };
        rows = kept
            .iter()
            .filter_map(|n| {
                if let JValue::Num(n) = n {
                    rows.get(*n as usize).cloned()
                } else {
                    None
                }
            })
            .collect();
        if rows.len() != kept.len() {
            return fault(Fault::State);
        }
        facts = result;
    }
    let selectors = match policy::selectors(&facts, &rows) {
        Ok(s) => s,
        Err(e) => return fault(e),
    };
    let Some(token) = opaque(&facts, "token") else {
        return fault(Fault::State);
    };
    let Some(selectors_json) = bridge_json(&selectors) else {
        return fault(Fault::State);
    };
    let Some((status, prepared)) = call(
        app,
        PREPARE,
        &serde_json::json!({"token":token,"stage":"identities","selectors":selectors_json}),
    )
    .await
    else {
        return fault(Fault::State);
    };
    if let Err(r) = stage_result(status, &prepared, &generation) {
        return *r;
    }
    if let Err(e) = policy::validate(body.get("confirmPause"), &prepared) {
        return fault(e);
    }
    if let Err(e) = policy::identities(&prepared, &selectors, prepared.get("identities")) {
        return fault(e);
    }
    let plan = match policy::launch(&prepared) {
        Ok(p) => p,
        Err(e) => return fault(e),
    };
    let Some(token) = opaque(&prepared, "token") else {
        return fault(Fault::State);
    };
    let Some(patch) = bridge_json(&plan) else {
        return fault(Fault::State);
    };
    let Some((status, result)) = call(
        app,
        COMMIT,
        &serde_json::json!({"token":token,"plan":patch}),
    )
    .await
    else {
        return fault(Fault::State);
    };
    // Launch intentionally creates a new run identity, so only its exact conditional-write answer is checked here.
    if status == 409 && result.get("applied").as_bool() == Some(false) {
        return stale();
    }
    if status != 200 || result.get("applied").as_bool() != Some(true) {
        return fault(Fault::State);
    }
    public_response(202, &result)
}
