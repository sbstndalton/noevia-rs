//! #2294: public cancellation policy; the live Node worker retains its controller and restoration.
use crate::{identity, reply, App};
use axum::{
    body::Body,
    http::{header, HeaderValue, Method, Request, Response, StatusCode},
    response::IntoResponse,
};
use js_json::{wtf16, JValue};
use std::sync::Arc;
pub const PATH: &str = "/api/models/autotune/cancel";
const FACTS: &str = "cancel/facts";
const ABORT: &str = "cancel/abort";
fn error(status: u16, message: &str) -> Response<Body> {
    reply::error(
        StatusCode::from_u16(status).unwrap_or(StatusCode::SERVICE_UNAVAILABLE),
        message,
        true,
    )
}
fn unavailable() -> Response<Body> {
    error(503, "Auto-tune state is unavailable. Try again shortly.")
}
async fn call(app: &App, path: &str, body: &serde_json::Value) -> Option<(u16, JValue)> {
    app.config.autotune.call(path, body).await
}
fn opaque<'a>(facts: &'a JValue, key: &str) -> Option<&'a str> {
    facts.get(key).as_str().filter(|t| {
        t.len() == 64
            && t.bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    })
}
/// The old publicJob's privacy rule is recursive, including dictionaries inside arrays. The
/// bridge independently excludes private profile/HTTP payloads; an unexpected HTTP config in a
/// malformed bridge answer is rejected rather than serialized. Model strings are response data,
/// never a selector, and stay lossless WTF-16 through the bridge and public serialization.
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
        || job.get("status").as_str() != Some("running")
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
pub async fn serve(
    app: &Arc<App>,
    conn: crate::serve::Conn,
    req: Request<Body>,
    path: String,
) -> Response<Body> {
    if path != PATH {
        return app.proxy.forward(conn.peer, req).await;
    }
    let (parts, _body) = req.into_parts();
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
    // Writes used Node's outer model-route admin message; safe methods reached the inner one.
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
    // Availability preceded the 405, even on a safe method. No body/model is read on this route.
    // A query or JSON 'model' never selects another person's or a replacement calibration job.
    let mut generation: Option<String> = None;
    for _ in 0..3 {
        let Some((200, facts)) = call(app, FACTS, &serde_json::json!({})).await else {
            return unavailable();
        };
        match facts.get("available").as_bool() {
            Some(false) => return error(404, "Auto-tune is unavailable"),
            Some(true) => {}
            None => return unavailable(),
        }
        if parts.method != Method::POST {
            return error(405, "Method not allowed");
        }
        let job = facts.get("job");
        if !matches!(job, JValue::Null) && !matches!(job, JValue::Obj(_)) {
            return unavailable();
        }
        if job.get("status").as_str() != Some("running") {
            return error(409, "No auto-tune is running.");
        }
        if public_job(job).is_none() {
            return unavailable();
        }
        let Some(current_generation) = opaque(&facts, "generation") else {
            return unavailable();
        };
        if generation
            .as_deref()
            .is_some_and(|first| first != current_generation)
        {
            return error(409, "The auto-tune changed before cancellation. Try again.");
        }
        generation = Some(current_generation.to_owned());
        let Some(token) = opaque(&facts, "token") else {
            return unavailable();
        };
        let Some((status, committed)) =
            call(app, ABORT, &serde_json::json!({"token": token})).await
        else {
            return unavailable();
        };
        if status == 409 && committed.get("applied").as_bool() == Some(false) {
            continue;
        }
        if status != 200 || committed.get("applied").as_bool() != Some(true) {
            return unavailable();
        }
        if opaque(&committed, "generation") != generation.as_deref() {
            return unavailable();
        }
        let Some(job) = public_job(committed.get("job")) else {
            return unavailable();
        };
        let Some(text) = wtf16::stringify(&job) else {
            return unavailable();
        };
        let mut res = reply::json_text(StatusCode::ACCEPTED, text, true);
        res.headers_mut()
            .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
        return res;
    }
    unavailable()
}
#[cfg(test)]
#[allow(clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    #[test]
    fn recursive_private_projection_matches_the_actual_old_node_oracle() {
        let JValue::Arr(cases) =
            wtf16::parse(include_str!("../tests/fixtures/autotune-cancel-node.json"))
                .expect("Node oracle")
        else {
            unreachable!()
        };
        for case in cases {
            if case.get("job").get("status").as_str() == Some("running") {
                let out = public_job(case.get("job")).expect("running job");
                let JValue::Obj(expected) = case.get("body") else {
                    unreachable!()
                };
                let JValue::Obj(actual) = &out else {
                    unreachable!()
                };
                assert_eq!(actual.len(), expected.len());
                for (key, value) in expected {
                    assert_eq!(out.get(key), value, "{key}");
                }
            } else {
                assert_eq!(case.get("status").as_num(), Some(409.0));
            }
        }
    }
    #[test]
    fn privacy_is_recursive_and_model_strings_round_trip_without_a_selector() {
        let job = wtf16::parse(r#"{"id":"j","model":"m\ud800","status":"running","originalText":"secret","unknown":"secret","models":[{"customVisible":"nested-public","beforeText":"secret","phases":[{"_private":"secret","steps":[{"lastRevision":"secret","originalRevision":"secret","answer":"visible"}]}]}]}"#).expect("fixture");
        let out = public_job(&job).expect("job");
        let text = wtf16::stringify(&out).expect("object");
        assert!(text.contains(r#"m\ud800"#));
        assert!(text.contains("visible"));
        assert!(text.contains("nested-public"));
        assert!(!text.contains("secret"));
        let bad = wtf16::parse(r#"{"id":"j","model":"m","status":"running","models":[{"headers":{"Authorization":"secret"}}]}"#).expect("HTTP config");
        assert!(public_job(&bad).is_none());
    }
}
