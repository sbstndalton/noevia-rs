//! Native idle timer writes and conservative missing-preset cleanup.
//! Jobs own the maintenance reservation from acquisition through the last side effect. A
//! disconnected requester hands the settlement job over, as for the existing native reload.
use crate::{identity, model_maint::HoldError, reply, App};
use axum::{
    body::Body,
    http::{Method, Request, Response, StatusCode},
    response::IntoResponse,
};
use js_json::{wtf16, JValue};
use serde_json::{json, Value};
use std::{sync::Arc, time::Duration};

pub const IDLE: &str = "/api/models/idle-unload";
pub const PRUNE: &str = "/api/models/prune-missing";
const EFFECTS: &str = "/internal/model-writes/effects";
const INVALID: &str = "Choose Never or a time from 5 minutes to 24 hours.";
const BUSY: &str = "Models are busy, so nothing was changed. Try again shortly.";
const SCAN: &str = "The model folder looks unavailable, so nothing was removed.";
const STEP: Duration = Duration::from_secs(30);

pub fn owned(method: &Method, path: &str) -> bool {
    (path == IDLE && method == Method::PUT) || (path == PRUNE && method == Method::POST)
}
fn answer(status: u16, body: Value) -> Response<Body> {
    reply::json(
        StatusCode::from_u16(status).unwrap_or(StatusCode::SERVICE_UNAVAILABLE),
        &body,
        true,
    )
}
fn error(status: u16, text: &str) -> Response<Body> {
    answer(status, json!({"error":text}))
}
fn unavailable() -> Response<Body> {
    error(
        503,
        "Model write authority is unavailable. Try again shortly.",
    )
}
fn idle_error(status: u16, text: &str, id: &str) -> Response<Body> {
    answer(status, json!({"error":text,"errorId":id}))
}
fn integer(v: &JValue) -> Option<f64> {
    v.as_num()
        .filter(|n| n.is_finite() && *n >= 0.0 && n.fract() == 0.0)
}
fn valid_seconds(v: &JValue) -> bool {
    matches!(v, JValue::Null)
        || integer(v).is_some_and(|n| n == 0.0 || (300.0..=86400.0).contains(&n))
}
// Validate all JSON with the existing ECMAScript scanner, but materialize only
// the shallow setting field. Ignored deep fields and overflow numbers remain valid JSON.
fn input_seconds(bytes: &[u8]) -> Result<JValue, ()> {
    use prompt_framing::json::{parse_utf8, Value as Input};
    let parsed = parse_utf8(if bytes.is_empty() { b"{}" } else { bytes }, 2).ok_or(())?;
    Ok(match parsed.get("seconds") {
        Some(Input::Null) => JValue::Null,
        Some(Input::Num(n)) => JValue::Num(*n),
        _ => JValue::Undefined,
    })
}
fn view(v: &JValue) -> Option<Value> {
    let seconds = integer(v.get("seconds"))?;
    let source = v
        .get("source")
        .as_str()
        .filter(|s| ["setting", "env", "default"].contains(s))?;
    Some(json!({"seconds":seconds,"source":source,
        "fallbackSeconds":integer(v.get("fallbackSeconds")),
        "fallbackSource":v.get("fallbackSource").as_str().filter(|s| ["env","default"].contains(s)),
        "defaultSeconds":integer(v.get("defaultSeconds")).unwrap_or(1800.0),
        "minSeconds":integer(v.get("minSeconds")).unwrap_or(300.0),
        "maxSeconds":integer(v.get("maxSeconds")).unwrap_or(86400.0)}))
}
type IdleFault = (u16, &'static str, &'static str);
async fn idle_call(
    app: &App,
    method: reqwest::Method,
    body: Option<String>,
) -> Result<(Value, Option<String>), IdleFault> {
    let response = if method == reqwest::Method::PUT {
        app.config
            .model_manager_scan
            .idle_settling(body.unwrap_or_default())
            .await
    } else {
        app.config
            .model_manager_scan
            .write_call(method, "/api/v1/idle-unload", body)
            .await
    };
    let (status, body) = response
        .map(|(s, b)| (s.as_u16(), b))
        .unwrap_or((502, None));
    match status {
        404 | 405 => {
            return Err((
                404,
                "This model manager cannot change the idle unload time yet.",
                "idleUnloadUnavailable",
            ))
        }
        400 => return Err((400, INVALID, "idleUnloadInvalid")),
        503 => {
            return Err((
                503,
                "The model manager has no place to keep this setting.",
                "idleUnloadUnavailable",
            ))
        }
        200..=299 => {}
        _ => {
            return Err((
                502,
                "The model manager did not answer. Try again in a moment.",
                "idleUnloadEngine",
            ))
        }
    }
    let unexpected = (
        502,
        "The model manager gave an unexpected answer.",
        "idleUnloadEngine",
    );
    let body = body.ok_or(unexpected)?;
    let next = view(&body).ok_or(unexpected)?;
    Ok((
        next,
        body.get("preset")
            .as_str()
            .filter(|s| ["written", "unchanged", "failed"].contains(s))
            .map(str::to_owned),
    ))
}
async fn effects(app: &App, body: Value) -> Option<JValue> {
    let (status, bytes) = app
        .config
        .research
        .node_call(EFFECTS, &body, Duration::from_secs(5))
        .await?;
    if status != 200 || bytes.len() > 65536 {
        return None;
    }
    let result = wtf16::parse(std::str::from_utf8(&bytes).ok()?).ok()?;
    (result.get("ok").as_bool() == Some(true)).then_some(result)
}
async fn audit(app: Arc<App>, actor: String, detail: Value) -> bool {
    tokio::task::spawn_blocking(move || {
        app.writes.writer()?.write(|tx| {
            tx.execute("INSERT INTO audit_events(actor_user_id,target_user_id,action,detail,created_at) VALUES(?1,?1,'models.idle_unload',?2,?3)",
                server_store::rusqlite::params![actor, detail.to_string(), identity::now_ms()])?;
            Ok(())
        })
    }).await.is_ok_and(|r| r.is_ok())
}
async fn idle(app: Arc<App>, seconds: JValue, actor: String) -> Response<Body> {
    // Invalidation is an acknowledgment from the remaining Node in-memory reader, not an executor.
    if effects(&app, json!({"op":"invalidate"})).await.is_none() {
        return unavailable();
    }
    let previous = idle_call(&app, reqwest::Method::GET, None)
        .await
        .ok()
        .and_then(|(v, _)| v.get("seconds").cloned());
    let maint = &app.config.maint;
    let hold = if maint.router.is_some() {
        Some(
            match maint.acquire("", Duration::from_secs(300), false).await {
                Ok(h) => h,
                Err(HoldError::Busy) => return idle_error(
                    409,
                    "Models are busy, so the idle unload time was not changed. Try again shortly.",
                    "idleUnloadBusy",
                ),
                Err(_) => return unavailable(),
            },
        )
    } else {
        None
    };
    let out = {
        let work = async {
            if let Some(h) = hold.as_ref() {
                if !h.confirm(STEP).await {
                    return unavailable();
                }
            }
            let Some(seconds) = wtf16::stringify(&seconds) else {
                return unavailable();
            };
            let (mut next, preset) = match idle_call(
                &app,
                reqwest::Method::PUT,
                Some(format!("{{\"seconds\":{seconds}}}")),
            )
            .await
            {
                Ok(v) => v,
                Err((status, message, id)) => return idle_error(status, message, id),
            };
            if let Some(h) = hold.as_ref() {
                if !h.confirm(Duration::from_secs(135)).await {
                    return unavailable();
                }
            }
            let reload = if preset.as_deref() != Some("failed") {
                if let (Some(router), Some(hold)) = (maint.router.as_ref(), hold.as_ref()) {
                    Some(maint.reload_inner(&router.settling(), hold, false).await)
                } else {
                    None
                }
            } else {
                None
            };
            let applied_reload = reload
                .as_ref()
                .is_some_and(|r| (200..300).contains(&r.status));
            let reload_body = reload
                .as_ref()
                .and_then(|r| serde_json::from_str::<Value>(&r.body).ok());
            let apply = if preset.as_deref() == Some("failed") {
                json!({"state":"failed"})
            } else if applied_reload {
                json!({"state":"applied"})
            } else if let Some(loaded) = reload_body
                .as_ref()
                .and_then(|b| b.get("loaded"))
                .and_then(Value::as_array)
            {
                json!({"state":"pending","loaded":loaded.iter().filter(|v| v.is_string()).collect::<Vec<_>>()})
            } else {
                json!({"state":"pending"})
            };
            let effective = if applied_reload {
                app.config
                    .research
                    .node_call(
                        "/internal/idle-unload-read/facts",
                        &json!({}),
                        Duration::from_secs(10),
                    )
                    .await
                    .filter(|(s, _)| *s == 200)
                    .and_then(|(_, bytes)| serde_json::from_slice::<Value>(&bytes).ok())
                    .and_then(|v| v.get("seconds").cloned())
            } else {
                None
            };
            if !audit(Arc::clone(&app), actor, json!({"seconds":next.get("seconds"),"source":next.get("source"),"previous":previous})).await { return unavailable(); }
            let applied = applied_reload
                && effective.as_ref().and_then(Value::as_f64)
                    == next.get("seconds").and_then(Value::as_f64);
            let Some(fields) = next.as_object_mut() else {
                return unavailable();
            };
            fields.insert("apply".into(), apply);
            fields.insert("applied".into(), json!(applied));
            answer(200, next)
        };
        tokio::pin!(work);
        loop {
            tokio::select! {
                result = &mut work => break result,
                _ = tokio::time::sleep(Duration::from_secs(1)) => {
                    // Keep the existing grant's renewer alive through settlement. Never
                    // acquire a replacement or authorize a next effect after coverage loss.
                    if let Some(h) = hold.as_ref() { h.cover(Duration::from_secs(300)); }
                }
            }
        }
    };
    if let Some(hold) = hold {
        hold.release().await;
    }
    if effects(&app, json!({"op":"invalidate"})).await.is_none() {
        return unavailable();
    }
    out
}
fn candidates(rows: &[JValue]) -> Result<Vec<String>, ()> {
    let ordinary: Vec<_> = rows
        .iter()
        .filter(|m| {
            m.get("system").as_bool() != Some(true)
                && m.get("servedElsewhere").as_bool() != Some(true)
                && m.get("sidecarProtected").as_bool() != Some(true)
                && (matches!(m.get("source"), JValue::Null | JValue::Undefined)
                    || m.get("source").as_str() == Some("preset"))
        })
        .collect();
    let missing: Vec<String> = ordinary
        .iter()
        .filter(|m| m.get("missingFile").as_bool() == Some(true))
        .filter_map(|m| m.get("name").as_str().map(str::to_owned))
        .collect();
    if missing.len() * 2 > ordinary.len() {
        return Err(());
    }
    Ok(missing)
}
// Match encodeURIComponent and the historical truthy revision coercion. A malformed
// revision must never silently turn a conditional delete into an unconditional one.
fn section_path(name: &str, revision: &JValue) -> Option<String> {
    if name.is_empty() {
        return None;
    }
    let encode = |text: &str| {
        if text.chars().any(|c| wtf16::lone_unit(c).is_some()) {
            return None;
        }
        Some(crate::plugin_directory_routes::encode_uri_component(text))
    };
    let name = encode(name)?;
    let query = if revision.truthy() {
        let revision = js_json::to_js_string(revision).ok()?;
        format!("?baseRevision={}", encode(&revision)?)
    } else {
        String::new()
    };
    Some(format!("/api/v1/sections/{name}/missing-fenced{query}"))
}
async fn prune(app: Arc<App>, actor: String) -> Response<Body> {
    let loader = &app.config.model_manager_scan;
    if app.config.sizing.kind() == server_stats::Kind::None {
        return error(404, "model management is disabled");
    }
    if effects(&app, json!({"op":"invalidate"})).await.is_none() {
        return unavailable();
    }
    let scan = loader
        .write_call(reqwest::Method::GET, "/api/v1/models", None)
        .await;
    if !scan.is_some_and(|(s, b)| {
        s.is_success()
            && matches!(b.as_ref().map(|v|v.get("models")),Some(JValue::Arr(a)) if !a.is_empty())
    }) {
        return answer(409, json!({"error":SCAN,"code":"scanUntrusted"}));
    }
    let sizing = &app.config.sizing;
    let listing = sizing.listing().await;
    let rows = match crate::model_state_routes::installed_list(&app, sizing, &listing).await {
        Ok(rows) => rows,
        Err(crate::model_state_routes::ListFault::Engine) => {
            return error(502, "Could not list installed models.")
        }
        Err(crate::model_state_routes::ListFault::Node) => return unavailable(),
    };
    let names = match candidates(&rows) {
        Ok(v) => v,
        Err(()) => return answer(409, json!({"error":SCAN,"code":"scanUntrusted"})),
    };
    if names.is_empty() {
        return answer(200, json!({"removed":[],"kept":[],"reloaded":false}));
    }
    let maint = &app.config.maint;
    let hold = match maint.acquire("", Duration::from_secs(3600), false).await {
        Ok(h) => h,
        Err(HoldError::Busy) => {
            return answer(
                409,
                json!({"error":BUSY,"code":"maintenance_held","retryable":true}),
            )
        }
        Err(_) => return unavailable(),
    };
    let mut removed = Vec::new();
    let mut kept = Vec::new();
    let mut lost = false;
    let deadline = std::time::Instant::now() + Duration::from_secs(3600);
    for name in names {
        if std::time::Instant::now() + STEP + STEP >= deadline || !hold.confirm(STEP + STEP).await {
            lost = true;
            break;
        }
        let sections = loader
            .write_call(reqwest::Method::GET, "/api/v1/sections", None)
            .await;
        let section = sections.and_then(|(s, b)| s.is_success().then_some(b).flatten());
        let eligible = section.as_ref().is_some_and(|s| matches!(s.get("sections"),JValue::Arr(a) if a.iter().any(|v| v.get("name").as_str()==Some(name.as_str()) && v.get("hasFile").as_bool()==Some(false))));
        if !hold.confirm(STEP).await {
            lost = true;
            break;
        }
        let did_remove = if eligible {
            let revision = section
                .as_ref()
                .map(|s| s.get("revision"))
                .unwrap_or(&JValue::Undefined);
            match section_path(&name, revision) {
                // A new path prevents older authorities from admitting an unfenced
                // delete. This authority refuses writes until publication is fenced;
                // its 503 (or a transport failure) is never a successful "kept" result.
                Some(path) => match loader
                    .write_call(reqwest::Method::DELETE, &path, None)
                    .await
                {
                    Some((status, _)) if status.is_success() => true,
                    Some((status, _)) if status.is_server_error() => {
                        lost = true;
                        break;
                    }
                    None => {
                        lost = true;
                        break;
                    }
                    _ => false,
                },
                None => false,
            }
        } else {
            false
        };
        if did_remove {
            removed.push(name);
        } else {
            kept.push(name);
        }
    }
    let reloaded = if !removed.is_empty()
        && !lost
        && std::time::Instant::now() + Duration::from_secs(135) < deadline
        && hold.confirm(Duration::from_secs(135)).await
    {
        if let Some(router) = maint.router.as_ref() {
            (200..300).contains(&maint.reload_inner(router, &hold, false).await.status)
        } else {
            false
        }
    } else {
        false
    };
    // Existing Node role/last-loaded/identity consumers receive only the names actually removed.
    let acknowledged = !lost
        && hold.confirm(Duration::from_secs(5)).await
        && effects(&app, json!({"op":"removed","names":removed,"user":actor}))
            .await
            .is_some();
    hold.release().await;
    if lost || !acknowledged {
        return unavailable();
    }
    let body = JValue::obj([
        (
            "removed",
            JValue::Arr(removed.into_iter().map(JValue::Str).collect()),
        ),
        (
            "kept",
            JValue::Arr(kept.into_iter().map(JValue::Str).collect()),
        ),
        ("reloaded", JValue::Bool(reloaded)),
    ]);
    reply::json_text(
        StatusCode::OK,
        wtf16::stringify(&body).unwrap_or_else(|| "null".into()),
        true,
    )
}
pub async fn serve(app: &Arc<App>, req: Request<Body>, path: String) -> Response<Body> {
    let (parts, body) = req.into_parts();
    let creds = identity::creds_of(&parts);
    let gate = Arc::clone(&app.identity);
    let id = match tokio::task::spawn_blocking(move || {
        gate.gate_blocking(&creds, &path, identity::now_ms())
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
    if parts.method == Method::PUT {
        if !app.config.model_manager_scan.configured() {
            return idle_error(
                404,
                "The idle unload setting is unavailable",
                "idleUnloadUnavailable",
            );
        }
        let bytes = match axum::body::to_bytes(body, 1024 * 1024).await {
            Ok(b) => b,
            Err(_) => return error(400, "invalid JSON"),
        };
        let seconds = match input_seconds(&bytes) {
            Ok(v) => v,
            Err(()) => return error(400, "invalid JSON"),
        };
        if !valid_seconds(&seconds) {
            return answer(
                400,
                json!({"error":INVALID,"errorId":"idleUnloadInvalid","minSeconds":300,"maxSeconds":86400}),
            );
        }
        let app = Arc::clone(app);
        // No request cancellation can strand Maint's acquisition reservation. The job, including
        // acquisition, is transferred to this bounded task before taking any permit.
        let job = tokio::spawn(async move { idle(app, seconds, id.user.id).await });
        // Dropping a JoinHandle detaches, whereas timing out the effect future
        // would drop its hold while the authority's blocking write survives.
        match tokio::time::timeout(STEP, job).await {
            Ok(Ok(response)) => response,
            _ => unavailable(),
        }
    } else {
        let app = Arc::clone(app);
        tokio::spawn(async move { prune(app, id.user.id).await })
            .await
            .unwrap_or_else(|_| unavailable())
    }
}
#[cfg(test)]
#[allow(clippy::indexing_slicing)]
mod tests {
    use super::*;
    #[test]
    fn validation_and_conservative_selection() {
        let deep = format!(
            "{{\"seconds\":900,\"ignored\":{}0{}}}",
            "[".repeat(512),
            "]".repeat(512)
        );
        assert_eq!(input_seconds(deep.as_bytes()), Ok(JValue::Num(900.0)));
        assert_eq!(
            input_seconds(br#"{"seconds":900,"ignored":1e400}"#),
            Ok(JValue::Num(900.0))
        );
        assert!(input_seconds(br#"{"seconds":900,"ignored":[}"#).is_err());

        assert_eq!(
            section_path("a!(x)~", &JValue::Num(3.0)).as_deref(),
            Some("/api/v1/sections/a!(x)~/missing-fenced?baseRevision=3")
        );
        assert_eq!(
            section_path("a", &JValue::Bool(false)).as_deref(),
            Some("/api/v1/sections/a/missing-fenced")
        );
        assert!(section_path("a", &JValue::obj([("toString", JValue::Num(1.0))])).is_none());
        let lone = wtf16::lone_char(0xD800)
            .map(|c| c.to_string())
            .unwrap_or_default();
        assert!(section_path(&lone, &JValue::Null).is_none());

        for text in ["null", "0", "300", "86400"] {
            assert!(valid_seconds(
                &wtf16::parse(text).unwrap_or(JValue::Undefined)
            ));
        }
        for text in ["true", "\"300\"", "299", "86401", "1.5", "{}"] {
            assert!(!valid_seconds(
                &wtf16::parse(text).unwrap_or(JValue::Undefined)
            ));
        }
        assert!(!valid_seconds(&JValue::Undefined));
        let rows=wtf16::parse(r#"[{"name":"gone","missingFile":true},{"name":"ok"},{"name":"side","missingFile":true,"sidecarProtected":true}]"#).unwrap_or(JValue::Null);
        if let JValue::Arr(rows) = rows {
            assert_eq!(candidates(&rows), Ok(vec!["gone".into()]));
            assert!(candidates(&rows[..1]).is_err());
        } else {
            assert_eq!(rows, JValue::Arr(vec![]));
        }
    }
}
