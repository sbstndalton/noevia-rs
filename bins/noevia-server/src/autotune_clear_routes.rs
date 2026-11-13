//! #2293: native finished-job dismissal policy, with the live Node actor as sole state writer.
use crate::{identity, reply, App};
use axum::{
    body::Body,
    http::{header, HeaderValue, Method, Request, Response, StatusCode},
    response::IntoResponse,
};
use js_json::{wtf16, JValue};
use std::{sync::Arc, time::Duration};
pub const PATH: &str = "/api/models/autotune/clear";
const FACTS: &str = "/internal/autotune-clear/facts";
const CLEAR: &str = "/internal/autotune-clear/clear";
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
    let (status, bytes) =
        crate::autotune_runner::call(app, path, body, Duration::from_secs(5)).await?;
    if bytes.len() > 8192 {
        return None;
    }
    Some((
        status,
        wtf16::parse(std::str::from_utf8(&bytes).ok()?).ok()?,
    ))
}
fn token(facts: &JValue) -> Option<&str> {
    facts.get("token").as_str().filter(|t| {
        t.len() == 64
            && t.bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    })
}
/// Original guard order: active/starting, absent/nonfinished, restored-settings reload pending.
fn refusal(facts: &JValue) -> Result<Option<&'static str>, ()> {
    let starting = facts.get("starting").as_bool().ok_or(())?;
    let present = facts.get("jobPresent").as_bool().ok_or(())?;
    let reload = facts.get("reloadPending").as_bool().ok_or(())?;
    let status = facts.get("status");
    if starting || status.as_str() == Some("running") {
        return Ok(Some("Auto-tune is running. Cancel it before clearing."));
    }
    if !present
        || !matches!(
            status.as_str(),
            Some("failed" | "cancelled" | "interrupted" | "passed")
        )
    {
        return Ok(Some("There is no finished auto-tune to clear."));
    }
    if reload {
        return Ok(Some(
            "The model server has not re-read the restored settings yet. Try again shortly.",
        ));
    }
    Ok(None)
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
    match refusal(&facts) {
        Ok(Some(message)) => return error(409, message),
        Ok(None) => {}
        Err(()) => return unavailable(),
    }
    let Some(token) = token(&facts) else {
        return unavailable();
    };
    let Some((status, result)) = call(app, CLEAR, &serde_json::json!({ "token": token })).await
    else {
        return unavailable();
    };
    if status == 409 && result.get("applied").as_bool() == Some(false) {
        // Never reselect a replacement job after this snapshot loses its generation.
        return error(409, "The auto-tune changed before dismissal. Try again.");
    }
    if status != 200 || result.get("applied").as_bool() != Some(true) {
        return unavailable();
    }
    let mut response = reply::json_text(StatusCode::OK, "{\"cleared\":true}".into(), true);
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    #[test]
    fn guard_order_matches_immutable_actual_node_clear_oracle() {
        let cases = wtf16::parse(include_str!("../tests/fixtures/autotune-clear-node.json"))
            .expect("Node oracle");
        let JValue::Arr(cases) = cases else {
            panic!("cases")
        };
        for case in cases {
            let input = case.get("input");
            let job = input.get("job");
            let facts = JValue::obj([
                ("starting", input.get("starting").clone()),
                ("jobPresent", JValue::Bool(!matches!(job, JValue::Null))),
                ("status", job.get("status").clone()),
                (
                    "reloadPending",
                    JValue::Bool(job.get("_reloadPending").as_bool() == Some(true)),
                ),
            ]);
            let refusal = refusal(&facts).expect("raw facts");
            let expected = case.get("answer").get("body").get("error");
            assert_eq!(refusal, expected.as_str());
        }
    }
}
