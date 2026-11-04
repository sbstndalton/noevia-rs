//! Exact GET download status; the selected engine adapter still owns its tracker.
use std::time::Duration;

use axum::{body::Body, http::StatusCode, response::Response};
use js_json::{wtf16, JValue};

use crate::{reply, App};

pub const PATH: &str = "/api/models/downloads";
const FACTS: &str = "/internal/model-downloads-read/facts";

fn row_field(row: &JValue, key: &str) -> JValue {
    if matches!(row.get("infinite"), JValue::Arr(keys) if keys.iter().any(|v| v.as_str() == Some(key)))
    {
        // The sign does not affect truthiness or the final JSON representation.
        JValue::Num(f64::INFINITY)
    } else {
        row.get("fields").get(key).clone()
    }
}

fn first_truthy(row: &JValue, keys: &[&str]) -> JValue {
    keys.iter()
        .map(|key| row_field(row, key))
        .find(JValue::truthy)
        .unwrap_or_else(|| JValue::Str(String::new()))
}

fn view(facts: &JValue) -> Option<JValue> {
    let rows = [facts.get("direct"), facts.get("jobs"), facts.get("downloads")]
        .into_iter()
        .find(|value| value.truthy());
    let rows = match rows {
        Some(JValue::Arr(rows)) => rows,
        None => return Some(JValue::Arr(Vec::new())),
        Some(_) => return None,
    };
    rows.iter()
        .map(|row| {
            if !row.get("fields").is_object() {
                return None;
            }
            let progress = row_field(row, "percent")
                .as_num()
                .map(|n| n / 100.0)
                .or_else(|| row_field(row, "progress").as_num());
            Some(JValue::obj([
                ("id", first_truthy(row, &["id", "job_id"])),
                ("model", first_truthy(row, &["model_name", "model", "checkpoint"])),
                ("progress", progress.map_or(JValue::Null, JValue::Num)),
                ("status", first_truthy(row, &["status", "state"])),
            ]))
        })
        .collect::<Option<Vec<_>>>()
        .map(JValue::Arr)
}

fn unavailable() -> Response<Body> {
    reply::error(StatusCode::BAD_GATEWAY, "Download status unavailable", true)
}

pub async fn serve(app: &App) -> Response<Body> {
    let Some((200, bytes)) = app
        .config
        .research
        .node_call(FACTS, &serde_json::json!({}), Duration::from_secs(10))
        .await
    else {
        return unavailable();
    };
    let Some(facts) = std::str::from_utf8(&bytes)
        .ok()
        .and_then(|text| wtf16::parse(text).ok())
    else {
        return unavailable();
    };
    let result = match facts.get("kind").as_str() {
        Some("disabled") => Some(JValue::Arr(Vec::new())),
        Some("listing") => view(&facts),
        Some("fault") => None,
        _ => return unavailable(),
    };
    match result.as_ref().and_then(wtf16::stringify) {
        Some(body) => reply::json_text(StatusCode::OK, body, true),
        None => reply::error(StatusCode::INTERNAL_SERVER_ERROR, "Internal error", true),
    }
}
