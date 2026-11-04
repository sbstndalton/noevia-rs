//! #2292: public cancellation policy over the native worker (model_maint::calibration_facts), which
//! owns the controller and the restore. Its facts have the shape Core's internal bridge
//! (rust-calibration-cancel-bridge.cjs) answered before the worker moved.
use crate::{identity, reply, App};
use axum::{
    body::Body,
    http::{header, HeaderValue, Method, Request, Response, StatusCode},
    response::IntoResponse,
};
use js_json::{wtf16, JValue};
use std::sync::Arc;
pub const PATH: &str = "/api/models/calibration/cancel";
const MAX_FACTS: usize = 8 * 1024 * 1024;
fn error(status: u16, message: &str) -> Response<Body> {
    reply::error(
        StatusCode::from_u16(status).unwrap_or(StatusCode::SERVICE_UNAVAILABLE),
        message,
        true,
    )
}
fn unavailable() -> Response<Body> {
    error(503, "Calibration state is unavailable. Try again shortly.")
}
/// The bridge's answer to `facts` (`token: None`) or `abort`: `{ available: false }` without a
/// worker; 409 for an abort that did not apply. `None` where it answered 503.
fn call(app: &App, token: Option<&str>) -> Option<(u16, JValue)> {
    let Some(worker) = app.config.calibration.as_ref() else {
        return Some((200, JValue::obj([("available", JValue::Bool(false))])));
    };
    let (status, answer) = match token {
        None => {
            let mut answer = vec![("available".into(), JValue::Bool(true))];
            if let JValue::Obj(fields) = worker.cancellation_facts() {
                answer.extend(fields);
            }
            (200, JValue::Obj(answer))
        }
        Some(token) => {
            let answer = worker.compare_and_abort(token);
            let status = if answer.get("applied").as_bool() == Some(false) {
                409
            } else {
                200
            };
            (status, answer)
        }
    };
    (wtf16::stringify(&answer)?.len() <= MAX_FACTS).then_some((status, answer))
}
fn opaque<'a>(facts: &'a JValue, key: &str) -> Option<&'a str> {
    facts.get(key).as_str().filter(|t| {
        t.len() == 64
            && t.bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    })
}
/// Raw worker facts are never forwarded wholesale. Original profile text and unknown actor fields
/// are private, even on a malformed or newer bridge. Strings remain WTF-16 end to end; a model
/// string is response data only, never an abort selector (the opaque token identifies the job).
fn public_job(job: &JValue) -> Option<JValue> {
    let JValue::Obj(_) = job else {
        return None;
    };
    if job.get("id").as_str().is_none_or(str::is_empty)
        || job.get("model").as_str().is_none_or(str::is_empty)
        || job.get("status").as_str() != Some("running")
        || !matches!(job.get("steps"), JValue::Arr(_))
    {
        return None;
    }
    const KEYS: &[&str] = &[
        "id",
        "model",
        "promptBudgetSeconds",
        "native",
        "base",
        "status",
        "phase",
        "startedAt",
        "steps",
        "memoryFloorGib",
        "memoryGuard",
        "originalRevision",
        "lastRevision",
        "result",
        "error",
        "restored",
        "finishedAt",
    ];
    Some(JValue::Obj(
        KEYS.iter()
            .filter_map(|key| {
                let value = job.get(key);
                (!matches!(value, JValue::Undefined)).then(|| ((*key).into(), value.clone()))
            })
            .collect(),
    ))
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
        let Some((200, facts)) = call(app, None) else {
            return unavailable();
        };
        match facts.get("available").as_bool() {
            Some(false) => return error(404, "Native calibration is unavailable"),
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
            return error(409, "No calibration is running.");
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
            return error(
                409,
                "The calibration changed before cancellation. Try again.",
            );
        }
        generation = Some(current_generation.to_owned());
        let Some(token) = opaque(&facts, "token") else {
            return unavailable();
        };
        let Some((status, committed)) = call(app, Some(token)) else {
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
    fn running_response_matches_the_actual_old_node_actor_fixtures() {
        let cases = wtf16::parse(include_str!(
            "../tests/fixtures/calibration-cancel-node.json"
        ))
        .expect("Node fixtures");
        let JValue::Arr(cases) = cases else {
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
    fn public_selection_drops_private_fields_and_preserves_lone_surrogates() {
        let job = wtf16::parse(r#"{"id":"j","model":"m\ud800","status":"running","steps":[],"originalText":"secret","unexpected":"secret"}"#).expect("fixture");
        let out = public_job(&job).expect("job");
        let text = wtf16::stringify(&out).expect("object");
        assert!(text.contains(r#"m\ud800"#));
        assert!(!text.contains("secret"));
    }
}
