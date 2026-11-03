//! Preset GET uses the same parsed snapshot and revision as Node's conditional writer.
use std::time::Duration;

use axum::{body::Body, http::StatusCode, response::Response};
use js_json::JValue;

use crate::{reply, App};

pub const PATH: &str = "/api/models/preset";
const FACTS: &str = "/internal/preset-read/facts";
const MAX_FACTS_BYTES: usize = 8 * 1024 * 1024;

fn unavailable() -> Response<Body> {
    reply::error(
        StatusCode::SERVICE_UNAVAILABLE,
        "Preset state is unavailable. Try again shortly.",
        true,
    )
}

pub async fn serve(app: &App, admin: bool, model: Option<String>) -> Response<Body> {
    if !admin {
        return reply::error(
            StatusCode::FORBIDDEN,
            "Administrator required for shared model profiles",
            true,
        );
    }
    let model = model.unwrap_or_default();
    let Some((200, bytes)) = app
        .config
        .research
        .node_call(
            FACTS,
            &serde_json::json!({ "model": model }),
            Duration::from_secs(5),
        )
        .await
    else {
        return unavailable();
    };
    if bytes.len() > MAX_FACTS_BYTES {
        return unavailable();
    }
    let Some(facts) = std::str::from_utf8(&bytes)
        .ok()
        .and_then(|s| js_json::parse(s).ok())
    else {
        return unavailable();
    };
    let (status, reason) = match facts.get("kind").as_str() {
        Some("preset") => {
            let revision = facts.get("revision");
            let exists = facts.get("exists");
            let options = facts.get("options");
            let defaults = facts.get("defaults");
            let fields = facts.get("fields");
            if !revision.as_str().is_some_and(|s| {
                s.len() == 64
                    && s.bytes()
                        .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
            }) || exists.as_bool().is_none()
                || !options.is_object()
                || !defaults.is_object()
                || !matches!(fields, JValue::Arr(values) if values.iter().all(|v| v.as_str().is_some()))
            {
                return unavailable();
            }
            let view = JValue::obj([
                ("model", JValue::Str(model)),
                ("revision", revision.clone()),
                ("exists", exists.clone()),
                ("options", options.clone()),
                ("defaults", defaults.clone()),
                ("fields", fields.clone()),
            ]);
            return match js_json::stringify(&view) {
                Some(body) => reply::json_text(StatusCode::OK, body, true),
                None => unavailable(),
            };
        }
        Some("unavailable") => (StatusCode::NOT_FOUND, "Native presets are unavailable"),
        Some("unconfigured") => (
            StatusCode::NOT_IMPLEMENTED,
            "Native preset editing is not exposed by this llama.cpp adapter",
        ),
        Some("nonregular") => (StatusCode::CONFLICT, "Preset path must be a regular file"),
        Some("duplicate") => (
            StatusCode::CONFLICT,
            "Duplicate preset sections require operator repair",
        ),
        Some("oversized") => (
            StatusCode::PAYLOAD_TOO_LARGE,
            "Preset file exceeds the editor limit",
        ),
        Some("read-failed") => (StatusCode::INTERNAL_SERVER_ERROR, "Internal error"),
        _ => return unavailable(),
    };
    reply::error(status, reason, true)
}
