//! `POST /api/models/prune-missing` (core `routes/models.cjs` `pruneMissingPresets`, #1695/#1698): remove the
//! models.ini section of every model whose file is gone, then ask the router to re-read the file.
//!
//! A section goes only when the folder scan says its file is missing (`missingFile`) and the model manager's own
//! listing says it has none (`hasFile: false`); Laya, sidecar models and presets served elsewhere are never
//! touched. The scan and the manager's listing come from the same folder, so a folder that is briefly gone would
//! look like every model deleted: the prune then refuses (409 `scanUntrusted`) when the scan lists no file or when
//! more than half of the ordinary presets would go. Idempotent: a second run finds nothing to do.
//!
//! The scan is taken from the manager now, never from Core's cache (a cached scan could be the glitch itself).
//! After the removals each name's role references, last-loaded entry and identity cache are cleared, as Core did.
use crate::App;
use js_json::JValue;
use serde_json::json;
use std::sync::Arc;

pub const PATH: &str = "/api/models/prune-missing";

const SCAN_UNTRUSTED: &str = "The model folder looks unavailable, so nothing was removed.";
const LIST_FAILED: &str = "Could not list installed models.";

/// The answer: status and JSON text, in Node's key order.
pub struct Answer {
    pub status: u16,
    pub body: String,
}

/// Node's key order: `error`, then `code`.
fn refuse(status: u16, error: &str, code: Option<&str>) -> Answer {
    let body = match code {
        Some(code) => format!(r#"{{"error":{},"code":{}}}"#, json!(error), json!(code)),
        None => format!(r#"{{"error":{}}}"#, json!(error)),
    };
    Answer { status, body }
}

/// An ordinary preset: not Laya, not a sidecar's model, not served elsewhere, a preset (or no source).
fn ordinary(row: &JValue) -> bool {
    row.get("system").as_bool() != Some(true)
        && row.get("servedElsewhere").as_bool() != Some(true)
        && row.get("sidecarProtected").as_bool() != Some(true)
        && match row.get("source") {
            JValue::Null | JValue::Undefined => true,
            s => s.as_str() == Some("preset"),
        }
}

/// `user` is the signed-in administrator (the roles cleared for that account are the ones Core reported).
pub async fn run(app: &Arc<App>, user: &str) -> Answer {
    let loader = &app.config.model_manager_scan;
    let Some(scan) = loader.fresh_scan().await.filter(|s| !s.is_empty()) else {
        return refuse(409, SCAN_UNTRUSTED, Some("scanUntrusted"));
    };
    let Ok(installed) = crate::model_state_routes::installed_now(app, &scan).await else {
        return refuse(502, LIST_FAILED, None);
    };
    let ordinary: Vec<&JValue> = installed.iter().filter(|m| ordinary(m)).collect();
    let candidates: Vec<String> = ordinary
        .iter()
        .filter(|m| m.get("missingFile").as_bool() == Some(true))
        .filter_map(|m| m.get("name").as_str().map(str::to_owned))
        .collect();
    if !candidates.is_empty() && candidates.len() * 2 > ordinary.len() {
        return refuse(409, SCAN_UNTRUSTED, Some("scanUntrusted"));
    }
    if candidates.is_empty() {
        return Answer {
            status: 200,
            body: r#"{"removed":[],"kept":[],"reloaded":false}"#.to_owned(),
        };
    }
    // The sections go and the router re-reads models.ini inside one hold (#2219).
    let sweep = async {
        let (mut removed, mut kept) = (Vec::new(), Vec::new());
        for name in candidates {
            if loader.remove_section(&name, true).await.removed {
                removed.push(name);
            } else {
                kept.push(name);
            }
        }
        (removed, kept)
    };
    let ((removed, kept), reloaded) = match app
        .config
        .maint
        .write_presets_held(|| sweep, |(removed, _)| !removed.is_empty())
        .await
    {
        Ok(done) => done,
        Err(o) => {
            return Answer {
                status: o.status,
                body: o.body,
            }
        }
    };
    if let Some(data_dir) = app.config.data_dir.as_deref() {
        for name in &removed {
            // Core awaits clearRoleReferences; the section is gone either way, so a failure is not an answer.
            let _ =
                crate::auto_roles_routes::clear_role_references(app, data_dir, user, name).await;
        }
    }
    for name in &removed {
        crate::auto_roles_routes::forget_model(app, name).await;
    }
    Answer {
        status: 200,
        body: format!(
            r#"{{"removed":{},"kept":{},"reloaded":{reloaded}}}"#,
            json!(removed),
            json!(kept)
        ),
    }
}
