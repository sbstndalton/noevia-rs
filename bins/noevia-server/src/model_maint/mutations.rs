//! Native public pull/delete lifecycle. Only shared Node facts, journals and caches remain
//! behind a token-authenticated bridge; it never executes an engine mutation or role policy.
use super::{
    activity::{MaintenanceActivity, NodeActivity},
    v8_json::{self, W},
};
use crate::{identity, reply, App};
use axum::{
    body::Body,
    http::{Method, Request, Response, StatusCode},
    response::IntoResponse,
};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use std::{sync::Arc, time::Duration};
pub const PULL: &str = "/api/models/pull";
pub const DELETE: &str = "/api/models/delete";
const SYSTEM: &str = "System routing model — not deleted";
const SIDECAR: &str =
    "A running sidecar (embedding or reranking) depends on this model — not deleted";
const BUSY: &str = "Models are busy, so nothing was changed. Try again shortly.";
fn error(status: u16, message: &str) -> Response<Body> {
    reply::error(
        StatusCode::from_u16(status).unwrap_or(StatusCode::SERVICE_UNAVAILABLE),
        message,
        true,
    )
}
fn answer(status: u16, body: Value) -> Response<Body> {
    reply::json_text(
        StatusCode::from_u16(status).unwrap_or(StatusCode::SERVICE_UNAVAILABLE),
        body.to_string(),
        true,
    )
}
fn ordered(status: u16, fields: Vec<(&str, W)>) -> Response<Body> {
    let body = W::Obj(
        fields
            .into_iter()
            .map(|(key, value)| (key.encode_utf16().collect(), value))
            .collect(),
    );
    reply::json_text(
        StatusCode::from_u16(status).unwrap_or(StatusCode::SERVICE_UNAVAILABLE),
        serialized(&body),
        true,
    )
}
fn unavailable() -> Response<Body> {
    error(503, "Model management is unavailable. Try again shortly.")
}
async fn bridge(app: &App, op: &str, body: Value) -> Option<Value> {
    let (status, bytes) = app
        .config
        .research
        .node_call(
            &format!("/internal/model-mutations/{op}"),
            &body,
            Duration::from_secs(15),
        )
        .await?;
    (status == 200)
        .then(|| serde_json::from_slice(&bytes).ok())
        .flatten()
}
fn serialized(w: &W) -> String {
    let mut text = String::new();
    v8_json::stringify(w, &mut text);
    text
}
async fn bridge_ok(app: &App, op: &str, body: Value) -> bool {
    bridge(app, op, body)
        .await
        .is_some_and(|v| v.get("ok").and_then(Value::as_bool) == Some(true))
}
async fn invalidate(app: &App) -> bool {
    bridge_ok(app, "invalidate", json!({})).await
}

pub async fn serve(
    app: &Arc<App>,
    conn: crate::serve::Conn,
    req: Request<Body>,
    path: String,
) -> Response<Body> {
    if ![PULL, DELETE].contains(&path.as_str()) || req.method() != Method::POST {
        return app.proxy.forward(conn.peer, req).await;
    }
    let (parts, body) = req.into_parts();
    let creds = identity::creds_of(&parts);
    let layer = Arc::clone(&app.identity);
    let gate_path = path.clone();
    let gate_creds = creds.clone();
    let now = identity::now_ms();
    let id = match tokio::task::spawn_blocking(move || {
        layer.gate_blocking(&gate_creds, &gate_path, now)
    })
    .await
    {
        Ok(Ok(id)) => id,
        Ok(Err(r)) => return r.into_response(),
        Err(_) => return identity::Rejection::Unavailable.into_response(),
    };
    if id.user.role != server_auth::Role::Admin {
        return error(403, "administrator required");
    }
    let Some(data_dir) = app.config.data_dir.as_ref() else {
        return identity::Rejection::Unavailable.into_response();
    };
    if let Err(rejected) =
        crate::project_routes::ensure_account(app, data_dir, &creds, &path, now, &id.user.id, true)
            .await
    {
        return *rejected;
    }
    let bytes = match tokio::time::timeout(Duration::from_secs(30), async {
        let mut body = body;
        let mut bytes = Vec::new();
        while let Some(frame) = body.frame().await {
            let frame = frame.map_err(|_| ())?;
            if let Ok(data) = frame.into_data() {
                if data.len() > 1024usize.saturating_mul(1024).saturating_sub(bytes.len()) {
                    return Err(());
                }
                bytes.extend_from_slice(&data);
            }
        }
        Ok(bytes)
    })
    .await
    {
        Ok(Ok(v)) => v,
        _ => return error(413, "Request exceeds size limit"),
    };
    let raw = String::from_utf8_lossy(&bytes).into_owned();
    // Once an engine mutation starts it owns its task, so caller departure cannot drop its
    // active registration while the engine still executes it. Every call has a deadline.
    let app = Arc::clone(app);
    let job = tokio::spawn(async move {
        if !invalidate(&app).await {
            return unavailable();
        }
        let response = if path == PULL {
            pull(&app, &raw).await
        } else {
            delete(&app, &raw, &id.user.id).await
        };
        let _ = invalidate(&app).await;
        response
    });
    job.await.unwrap_or_else(|_| error(500, "Internal error"))
}
async fn pull(app: &Arc<App>, raw: &str) -> Response<Body> {
    let plan =
        match super::pull::route_prepare(raw, app.config.models.kind != server_stats::Kind::None) {
            Ok(v) => v,
            Err(a) => {
                return reply::json_text(
                    StatusCode::from_u16(a.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
                    a.body,
                    true,
                )
            }
        };
    let Some(router) = &app.config.maint.router else {
        return error(404, "model management is disabled");
    };
    let activity = match NodeActivity(Arc::clone(app)).enter().await {
        Ok(g) => g,
        Err(e) => return error(e.status, &e.message),
    };
    if let Err(a) = plan.validate() {
        return reply::json_text(
            StatusCode::from_u16(a.status).unwrap_or(StatusCode::BAD_GATEWAY),
            a.body,
            true,
        );
    }
    let checkpoint: Value = match serde_json::from_str(&serialized(&plan.checkpoint)) {
        Ok(v) => v,
        Err(_) => return error(500, "Internal error"),
    };
    if !bridge_ok(
        app,
        "download",
        json!({"ticket":activity.ticket(),"checkpoint":checkpoint,"state":"requested"}),
    )
    .await
    {
        return unavailable();
    }
    if let Err(e) = activity.confirm(Duration::from_secs(120)).await {
        return error(e.status, &e.message);
    }
    let result = router.pull_exact(plan.request_body()).await;
    if result.as_ref().is_ok_and(|(ok, _, _)| !ok)
        && !bridge_ok(
            app,
            "download",
            json!({"ticket":activity.ticket(),"checkpoint":checkpoint,"state":"rejected"}),
        )
        .await
    {
        return unavailable();
    }
    activity.leave().await;
    match result {
        Ok((ok, status, body)) => {
            let a = plan.finish(ok, status, body);
            reply::json_text(
                StatusCode::from_u16(a.status).unwrap_or(StatusCode::BAD_GATEWAY),
                a.body,
                true,
            )
        }
        Err(_) => error(500, "Internal error"),
    }
}
fn protection(facts: &Value, name: &str) -> Option<Response<Body>> {
    let sidecar = facts
        .get("sidecarNames")?
        .as_array()?
        .iter()
        .any(|v| v.as_str() == Some(name.trim()));
    if facts.get("protectionUnknown").and_then(Value::as_bool) == Some(true) || sidecar {
        let service = if facts
            .get("rerankerNames")?
            .as_array()?
            .iter()
            .any(|v| v.as_str() == Some(name.trim()))
        {
            "reranker"
        } else {
            "embedding"
        };
        return Some(answer(
            409,
            json!({"error":SIDECAR,"errorId":"modelInUse","service":service}),
        ));
    }
    None
}
async fn delete(app: &Arc<App>, raw: &str, user: &str) -> Response<Body> {
    let mut body = match v8_json::parse(&raw.encode_utf16().collect::<Vec<_>>()) {
        Ok(v) => v,
        Err(_) => return error(400, "invalid JSON"),
    };
    if matches!(body, W::Null) {
        return error(500, "Internal error");
    }
    if !body.get("name").truthy() {
        return error(400, "name required");
    }
    let name_value = body.take("name");
    let string_name = matches!(name_value, W::Str(_));
    let name_units = match v8_json::to_js_string(&name_value) {
        Ok(v) => v,
        Err(_) => return error(500, "Internal error"),
    };
    let name = match String::from_utf16(&name_units) {
        Ok(v) => v,
        Err(_) => return unknown_name(&name_value),
    };
    let confirm = matches!(body.get("confirmSystem"), W::Bool(true));
    let roles = crate::model_kind::roles_beside(app.config.sizing.presets_path());
    if !confirm && crate::model_kind::is_system_model(&roles, &name) {
        return answer(
            400,
            json!({"error":SYSTEM,"errorId":"systemModel","service":"decision"}),
        );
    }
    let Some(facts) = bridge(app, "protection", json!({})).await else {
        return unavailable();
    };
    if !facts.get("sidecarNames").is_some_and(Value::is_array)
        || !facts.get("rerankerNames").is_some_and(Value::is_array)
        || !facts
            .get("protectionUnknown")
            .is_some_and(Value::is_boolean)
    {
        return unavailable();
    }
    if let Some(refusal) = protection(&facts, &name) {
        return refusal;
    }
    if app.config.models.kind == server_stats::Kind::None {
        return error(404, "model management is disabled");
    }
    let maint = &app.config.maint;
    let Some(router) = &maint.router else {
        return unavailable();
    };
    let listing = match router.models(None).await {
        Ok(r) if r.ok => r,
        _ => {
            return error(
                409,
                "Could not confirm this is not a system model. Try again.",
            )
        }
    };
    let Some(rows) = super::router::rows(&listing.body) else {
        return error(
            409,
            "Could not confirm this is not a system model. Try again.",
        );
    };
    let mut matched = None;
    for row in rows {
        // Core's find() short-circuits: only a null encountered before the match throws.
        if row.is_null() {
            return error(500, "Internal error");
        }
        let same = match &name_value {
            W::Str(units) => row
                .get("id")
                .and_then(Value::as_str)
                .is_some_and(|id| id.encode_utf16().eq(units.iter().copied())),
            W::Num(n) => row.get("id").and_then(Value::as_f64) == Some(*n),
            W::Bool(b) => row.get("id").and_then(Value::as_bool) == Some(*b),
            _ => false,
        };
        if same {
            matched = Some(row);
            break;
        }
    }
    if let Some(row) = matched {
        let args = row.pointer("/status/args").and_then(Value::as_array);
        let file = args
            .and_then(|a| {
                a.iter()
                    .position(|s| matches!(s.as_str(), Some("--model" | "-m")))
                    .and_then(|i| a.get(i + 1))
            })
            .and_then(Value::as_str);
        if !confirm && crate::model_kind::is_system_model_at(&roles, &name, file) {
            return answer(
                400,
                json!({"error":SYSTEM,"errorId":"systemModel","service":"decision"}),
            );
        }
    }
    if !string_name {
        return unknown_name(&name_value);
    }
    let installed = crate::model_state_routes::installed_list(
        app,
        &app.config.sizing,
        &app.config.sizing.listing().await,
    )
    .await;
    let missing = match &installed {
        Ok(list) => {
            let Some(entry) = list.iter().find(|m| m.get("name").as_str() == Some(&name)) else {
                return error(404, &format!("model not found: {name}"));
            };
            entry.get("missingFile").as_bool() == Some(true)
        }
        Err(crate::model_state_routes::ListFault::Engine) => false,
        Err(_) => return unavailable(),
    };
    let activity = match NodeActivity(Arc::clone(app)).enter().await {
        Ok(g) => g,
        Err(e) => return error(e.status, &e.message),
    };
    // Re-read assignments immediately before the engine operation; never trust a cached snapshot.
    let Some(facts) = bridge(app, "protection", json!({})).await else {
        return unavailable();
    };
    if !facts.get("sidecarNames").is_some_and(Value::is_array)
        || !facts.get("rerankerNames").is_some_and(Value::is_array)
        || !facts
            .get("protectionUnknown")
            .is_some_and(Value::is_boolean)
    {
        return unavailable();
    }
    if let Some(r) = protection(&facts, &name) {
        return r;
    }
    let removed = match router.remove_model_registered(&name, &activity).await {
        Ok(r) => r,
        Err(e) => return error(e.status, &e.message),
    };
    activity.leave().await;
    let file_gone = removed.reply.ok || missing;
    let mut preset_removed = false;
    let mut reloaded = false;
    let mut preset_busy = false;
    if file_gone && app.config.models.model_loader {
        match maint
            .acquire("model-delete-presets", Duration::from_secs(300), false)
            .await
        {
            Ok(hold) => {
                if hold.excluded() && hold.may_start(Duration::from_secs(15)) {
                    preset_removed = remove_section(app, &name, &hold).await;
                    if preset_removed && hold.excluded() {
                        let r = maint.reload_inner(router, &hold, false).await;
                        reloaded = (200..300).contains(&r.status);
                    }
                } else {
                    preset_busy = true;
                }
                hold.release().await;
            }
            Err(super::HoldError::Busy) => preset_busy = true,
            Err(super::HoldError::Gate { status: 409, .. }) => preset_busy = true,
            Err(_) => return unavailable(),
        }
    }
    if preset_busy && !removed.reply.ok {
        return ordered(
            409,
            vec![
                ("error", W::str(BUSY)),
                ("code", W::str("maintenance_held")),
                ("retryable", W::Bool(true)),
                ("unloaded", W::Bool(removed.unloaded)),
            ],
        );
    }
    if !(removed.reply.ok || (missing && preset_removed)) {
        if let Some(r) = refusal(&removed.reply, removed.unloaded) {
            return r;
        }
        return answer(
            502,
            json!({"error":format!("delete failed: {}",removed.reply.status),"unloaded":removed.unloaded}),
        );
    }
    let Some(data_dir) = app.config.data_dir.clone() else {
        return unavailable();
    };
    let role_app = Arc::clone(app);
    let role_user = user.to_owned();
    let role_name = name.clone();
    let cleared = match tokio::task::spawn_blocking(move || {
        crate::auto_roles_routes::clear_all(&role_app.auto_roles, &data_dir, &role_user, &role_name)
    })
    .await
    {
        Ok(Ok(v)) => v,
        _ => return unavailable(),
    };
    if !bridge_ok(app, "commit", json!({"name":name})).await {
        return unavailable();
    }
    if let Ok(mut cache) = maint.identities.lock() {
        cache.retain(|(id, _, _)| id != &name);
    }
    let mut result = vec![
        ("ok", W::Bool(true)),
        ("unloaded", W::Bool(removed.unloaded)),
        (
            "rolesCleared",
            W::Arr(cleared.iter().map(|role| W::str(role)).collect()),
        ),
        ("presetRemoved", W::Bool(preset_removed)),
    ];
    if preset_busy {
        result.push(("presetBusy", W::Bool(true)));
    }
    if preset_removed {
        result.push(("presetsReloaded", W::Bool(reloaded)));
    }
    ordered(200, result)
}
async fn remove_section(app: &App, name: &str, hold: &super::Hold) -> bool {
    if matches!(name, "." | "..") {
        return false;
    }
    let loader = &app.config.model_manager_scan;
    let Some((200, listing)) = loader
        .mutation_request(
            reqwest::Method::GET,
            "/api/v1/sections",
            None,
            Duration::from_secs(15),
        )
        .await
    else {
        return false;
    };
    if !listing
        .get("sections")
        .and_then(Value::as_array)
        .is_some_and(|s| {
            s.iter().any(|v| {
                v.as_str() == Some(name) || v.get("name").and_then(Value::as_str) == Some(name)
            })
        })
    {
        return false;
    }
    if !hold.excluded() || !hold.may_start(Duration::from_secs(15)) {
        return false;
    }
    let mut path = format!(
        "/api/v1/sections/{}",
        crate::gdrive::encode_uri_component(name)
    );
    let revision = listing.get("revision").cloned().unwrap_or(Value::Null);
    let revision_text = revision.to_string();
    let Ok(revision) = v8_json::parse(&revision_text.encode_utf16().collect::<Vec<_>>()) else {
        return false;
    };
    if revision.truthy() {
        let Ok(units) = v8_json::to_js_string(&revision) else {
            return false;
        };
        let Ok(revision) = String::from_utf16(&units) else {
            return false;
        };
        path.push_str("?baseRevision=");
        path.push_str(&crate::gdrive::encode_uri_component(&revision));
    }
    loader
        .mutation_request(
            reqwest::Method::DELETE,
            &path,
            None,
            Duration::from_secs(15),
        )
        .await
        .is_some_and(|(s, _)| (200..300).contains(&s))
}
fn refusal(r: &super::router::ExactReply, unloaded: bool) -> Option<Response<Body>> {
    let e = r.body.get("error");
    let gate: Vec<u16> = "gate_error".encode_utf16().collect();
    let nested = e.get("type").units() == Some(gate.as_slice());
    let code = String::from_utf16(
        if nested {
            e.get("code")
        } else {
            r.body.get("code")
        }
        .units()?,
    )
    .ok()?;
    if !(400..600).contains(&r.status)
        || ![
            "model_in_use",
            "engine_busy",
            "gate_busy",
            "model_not_found",
            "admin_required",
            "maintenance_held",
        ]
        .contains(&code.as_str())
    {
        return None;
    }
    let mut message = if nested {
        let message = e.get("message");
        if message.truthy() {
            match v8_json::to_js_string(message) {
                Ok(s) => s,
                Err(_) => return Some(error(500, "Internal error")),
            }
        } else {
            "The model engine refused the request."
                .encode_utf16()
                .collect()
        }
    } else {
        e.units()?.to_vec()
    };
    message.truncate(300); // JavaScript slice counts UTF-16 code units, including a dangling half.
    let retryable = matches!(
        if nested {
            e.get("retryable")
        } else {
            r.body.get("retryable")
        },
        W::Bool(true)
    );
    let body = W::Obj(vec![
        ("error".encode_utf16().collect(), W::Str(message)),
        ("code".encode_utf16().collect(), W::str(&code)),
        ("retryable".encode_utf16().collect(), W::Bool(retryable)),
        ("unloaded".encode_utf16().collect(), W::Bool(unloaded)),
    ]);
    Some(reply::json_text(
        StatusCode::from_u16(r.status).ok()?,
        serialized(&body),
        true,
    ))
}

fn unknown_name(value: &W) -> Response<Body> {
    let mut units: Vec<u16> = "model not found: ".encode_utf16().collect();
    units.extend(v8_json::to_js_string(value).unwrap_or_default());
    let mut text = String::from("{\"error\":");
    v8_json::quote(&units, &mut text);
    text.push('}');
    reply::json_text(StatusCode::NOT_FOUND, text, true)
}
