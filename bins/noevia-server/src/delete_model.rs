//! `POST /api/models/delete` (core `routes/models.cjs`, #336, #1660, #1661, #1695, #2115, #2219): unload a model,
//! delete its file through the engine, remove its models.ini section, and forget it everywhere.
//!
//! Order, as Core ran it: the body (400), the system-model refusal (400 `systemModel`), the sidecar refusal (409
//! `modelInUse`), management disabled (404), the router's own listing as a second system/sidecar check, the
//! existence check against the installed list (404, skipped when the list cannot be read), then unload + delete
//! (the unload is tried again once when the first delete is refused), then the section removal and the router
//! re-read under one hold (#2219). Only once the file is gone (or was already missing) does the section go.
//! After a delete the model's role references, last-loaded entry and identity cache are cleared, and Core's
//! cached reads are invalidated.
//!
//! Differences from Core, on purpose: a body of JSON `null` is `name required` (Core threw a TypeError); a
//! failure of the role clean-up after the file is gone is not an answer (Core answered an error and a retry then
//! got 404); a name that is not a string is `name required`.
//! Not bridged: Core's legacy folder-scan map (`modelScanCache`) has no op to clear it from here; the clear of the
//! role references invalidates Core's installed-list and scan reads.
use crate::App;
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::Duration;

pub const PATH: &str = "/api/models/delete";

const SYSTEM_MODEL_DELETE_REASON: &str = "System routing model — not deleted";
const SIDECAR_MODEL_DELETE_REASON: &str =
    "A running sidecar (embedding or reranking) depends on this model — not deleted";
const PRESETS_BUSY: &str = "Models are busy, so nothing was changed. Try again shortly.";
const NOT_CONFIRMED: &str = "Could not confirm this is not a system model. Try again.";
const GATE_CODES: [&str; 11] = [
    "inference_queue_full",
    "inference_queue_timeout",
    "engine_unavailable",
    "engine_switch_stuck",
    "lease_unknown",
    "model_in_use",
    "engine_busy",
    "gate_busy",
    "model_not_found",
    "admin_required",
    "maintenance_held",
];
const ENGINE_WAIT: Duration = Duration::from_secs(60);

/// The answer: status and JSON text, in Node's key order.
pub struct Answer {
    pub status: u16,
    pub body: String,
}

/// An object in the order given (serde's map would sort the keys).
fn object(pairs: &[(&str, Value)]) -> String {
    let fields: Vec<String> = pairs
        .iter()
        .map(|(k, v)| format!("{}:{v}", json!(k)))
        .collect();
    format!("{{{}}}", fields.join(","))
}

fn answer(status: u16, pairs: &[(&str, Value)]) -> Answer {
    Answer {
        status,
        body: object(pairs),
    }
}

fn refuse(status: u16, error: &str) -> Answer {
    answer(status, &[("error", json!(error))])
}

fn system_refusal() -> Answer {
    answer(
        400,
        &[
            ("error", json!(SYSTEM_MODEL_DELETE_REASON)),
            ("errorId", json!("systemModel")),
            ("service", json!("decision")),
        ],
    )
}

fn sidecar_refusal(app: &App, name: &str) -> Answer {
    answer(
        409,
        &[
            ("error", json!(SIDECAR_MODEL_DELETE_REASON)),
            ("errorId", json!("modelInUse")),
            ("service", json!(service_of(app, name))),
        ],
    )
}

/// `sidecarServiceOf`: the reranker when the deployment's reranker model is this one, else the embedding service.
fn service_of(app: &App, name: &str) -> &'static str {
    let env = &app.config.reranker.0;
    let on = env
        .get("NOEVIA_FEATURE_RAG_RERANK")
        .map(|v| v.trim().to_ascii_lowercase())
        .is_some_and(|v| matches!(v.as_str(), "1" | "true" | "on"));
    let model = env.get("RERANK_MODEL").map(|m| m.trim()).unwrap_or("");
    if on && !model.is_empty() && model == name.trim() {
        "reranker"
    } else {
        "embedding"
    }
}

fn slice300(text: &str) -> String {
    text.chars().take(300).collect()
}

/// `refusalOf`: a refusal from the inference gate as (status, code, error, retryable), or `None`.
fn refusal_of(status: u16, body: &Option<Value>) -> Option<(u16, String, String, bool)> {
    if !(400..=599).contains(&status) {
        return None;
    }
    let body = body.as_ref()?;
    let err = body.get("error");
    if let Some(e) = err.filter(|e| e.is_object()) {
        let code = e.get("code").and_then(Value::as_str)?;
        if e.get("type").and_then(Value::as_str) == Some("gate_error") && GATE_CODES.contains(&code)
        {
            let message = e
                .get("message")
                .and_then(Value::as_str)
                .filter(|m| !m.is_empty())
                .unwrap_or("The model engine refused the request.");
            return Some((
                status,
                code.to_owned(),
                slice300(message),
                e.get("retryable") == Some(&Value::Bool(true)),
            ));
        }
        return None;
    }
    let code = body.get("code").and_then(Value::as_str)?;
    let text = err.and_then(Value::as_str)?;
    GATE_CODES.contains(&code).then(|| {
        (
            status,
            code.to_owned(),
            slice300(text),
            body.get("retryable") == Some(&Value::Bool(true)),
        )
    })
}

/// `removeModel`: unload, then delete; a refused delete unloads once more and tries again.
struct Removed {
    ok: bool,
    status: u16,
    body: Option<Value>,
    unloaded: bool,
}

async fn remove_model(router: &crate::model_maint::router::Router, name: &str) -> Removed {
    let path = format!(
        "/models?model={}",
        crate::model_manager_scan::encode_component(name)
    );
    let delete = || async {
        router
            .request(reqwest::Method::DELETE, &path, None, None, ENGINE_WAIT)
            .await
    };
    let mut unloaded = router
        .unload(name, None)
        .await
        .map(|r| r.ok)
        .unwrap_or(false);
    let mut del = delete().await;
    if !matches!(&del, Ok(r) if r.ok) {
        if router
            .unload(name, None)
            .await
            .map(|r| r.ok)
            .unwrap_or(false)
        {
            unloaded = true;
        }
        del = delete().await;
    }
    match del {
        Ok(r) => Removed {
            ok: r.ok,
            status: r.status,
            body: r.body,
            unloaded,
        },
        Err(_) => Removed {
            ok: false,
            status: 502,
            body: None,
            unloaded,
        },
    }
}

/// `user` is the signed-in administrator (the roles cleared for that account are the ones reported).
pub async fn run(app: &Arc<App>, user: &str, raw: &str) -> Answer {
    // Core: JSON.parse, then `!body.name`. A `null` body threw there; here it is `name required` like any
    // other body without a name.
    let Ok(body) = serde_json::from_str::<Value>(raw) else {
        return refuse(400, "invalid JSON");
    };
    let Some(name) = body
        .get("name")
        .and_then(Value::as_str)
        .filter(|n| !n.is_empty())
    else {
        return refuse(400, "name required");
    };
    let confirm_system = body.get("confirmSystem") == Some(&Value::Bool(true));
    let roles = crate::model_kind::roles_beside(app.config.sizing.presets_path());
    if crate::model_kind::is_system_model(&roles, name) && !confirm_system {
        return system_refusal();
    }
    // The sidecar names are Core's; when it cannot say, nothing is deleted.
    let Some(sidecars) = crate::model_state_routes::sidecar_names(app).await else {
        return sidecar_refusal(app, name);
    };
    if sidecars.contains(name.trim()) {
        return sidecar_refusal(app, name);
    }
    let maint = Arc::clone(&app.config.maint);
    let loader = &app.config.model_manager_scan;
    let (true, Some(router)) = (
        app.config.sizing.kind() == server_stats::Kind::LlamaCpp,
        maint.router.as_ref(),
    ) else {
        return refuse(404, "model management is disabled");
    };
    // A neutral id can still point at Laya's weights: the router's --model path as well.
    let listing = router.models(None).await.ok().filter(|l| l.ok);
    let Some(rows) = listing
        .as_ref()
        .and_then(|l| crate::model_maint::router::rows(&l.body))
    else {
        return refuse(409, NOT_CONFIRMED);
    };
    if let Some(row) = rows
        .iter()
        .find(|m| m.get("id").and_then(Value::as_str) == Some(name))
    {
        let path = crate::model_state_routes::listing_row_path(row);
        if !confirm_system && crate::model_kind::is_system_model_at(&roles, name, path.as_deref()) {
            return system_refusal();
        }
    }
    // An unreadable installed list skips the existence check; the delete is attempted anyway.
    let installed = crate::model_state_routes::installed_cached(app).await.ok();
    let entry = installed
        .as_ref()
        .and_then(|l| l.iter().find(|m| m.get("name").as_str() == Some(name)));
    if installed.is_some() && entry.is_none() {
        return refuse(404, &format!("model not found: {name}"));
    }
    let missing_file = entry.is_some_and(|e| e.get("missingFile").as_bool() == Some(true));

    let removed = remove_model(router, name).await;
    // The section goes only once the file is gone (or was already missing), with the router's re-read under the
    // same hold; a refused hold keeps it (the card shows "File missing" and a retry or the prune finishes it).
    let file_gone = removed.ok || missing_file;
    let mut refused = false;
    let mut preset_removed = false;
    let mut reloaded = false;
    if file_gone && loader.configured() {
        let held = maint
            .write_presets_held(
                || async { loader.remove_section(name, false).await },
                |w| w.removed,
            )
            .await;
        match held {
            Ok((w, r)) => {
                preset_removed = w.removed;
                reloaded = r;
            }
            Err(_) => refused = true,
        }
    }
    if refused && !removed.ok {
        return answer(
            409,
            &[
                ("error", json!(PRESETS_BUSY)),
                ("code", json!("maintenance_held")),
                ("retryable", json!(true)),
                ("unloaded", json!(removed.unloaded)),
            ],
        );
    }
    if !(removed.ok || (missing_file && preset_removed)) {
        if let Some((status, code, error, retryable)) = refusal_of(removed.status, &removed.body) {
            return answer(
                status,
                &[
                    ("error", json!(error)),
                    ("code", json!(code)),
                    ("retryable", json!(retryable)),
                    ("unloaded", json!(removed.unloaded)),
                ],
            );
        }
        return answer(
            502,
            &[
                ("error", json!(format!("delete failed: {}", removed.status))),
                ("unloaded", json!(removed.unloaded)),
            ],
        );
    }
    // The model is gone: every auto-role that named it, the last-loaded default and the identity cache. A
    // failure here is not an answer (the file and the section are already gone).
    let cleared = match app.config.data_dir.as_deref() {
        Some(dir) => crate::auto_roles_routes::clear_role_references(app, dir, user, name)
            .await
            .unwrap_or_default(),
        None => Vec::new(),
    };
    crate::auto_roles_routes::forget_model(app, name).await;
    let mut pairs = vec![
        ("ok", json!(true)),
        ("unloaded", json!(removed.unloaded)),
        ("rolesCleared", json!(cleared)),
        ("presetRemoved", json!(preset_removed)),
    ];
    if refused {
        pairs.push(("presetBusy", json!(true)));
    }
    if preset_removed {
        pairs.push(("presetsReloaded", json!(reloaded)));
    }
    answer(200, &pairs)
}
