//! #2303: public calibration policy over the native worker (model_maint::calibration_facts), which
//! owns the job, its hold, controller and restore CAS. The facts it answers have the shape Core's
//! internal bridge (rust-calibration-routes-bridge.cjs) answered before the worker moved.
use crate::model_maint::calibration_worker::{Worker, OVER_LIMIT};
use crate::{identity, reply, App};
use axum::{
    body::Body,
    http::{header, HeaderValue, Method, Request, Response, StatusCode},
    response::IntoResponse,
};
use http_body_util::BodyExt;
use js_json::{wtf16, JValue};
use std::{sync::Arc, time::Duration};
pub const PATH: &str = "/api/models/calibration";
/// Core's bridge stopped every state operation after 14 s and answered 503; so does the front.
const DEADLINE: Duration = Duration::from_secs(14);
/// Core's bridge refused an answer over 8 MiB.
const MAX_BYTES: usize = 8 * 1024 * 1024;
const CONFIRM: &str =
    "Confirm that chat can pause and that Diary background jobs and other clients are stopped.";
const BUDGET: &str = "Choose a prompt time limit between 15 and 1800 seconds.";
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
fn changed() -> Response<Body> {
    error(
        409,
        "The calibration or model settings changed before launch. Try again.",
    )
}
fn response(status: StatusCode, value: &JValue) -> Response<Body> {
    let Some(text) = wtf16::stringify(value) else {
        return unavailable();
    };
    let mut res = reply::json_text(status, text, true);
    res.headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    res
}
fn opaque<'a>(facts: &'a JValue, key: &str) -> Option<&'a str> {
    facts.get(key).as_str().filter(|t| {
        t.len() == 64
            && t.bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    })
}
/// One state operation of the worker.
enum Op<'a> {
    Facts,
    Listing {
        model: &'a str,
        token: &'a str,
    },
    Prepare {
        model: &'a str,
        token: &'a str,
        row_token: &'a str,
    },
    Launch {
        model: &'a str,
        token: &'a str,
        row_token: &'a str,
        prepared_token: &'a str,
        plan: &'a JValue,
    },
}
/// The bridge's answer: `{ available: false }` without a worker, `{ available: true, ...facts }`
/// otherwise; `None` where it answered 503 (the worker threw, or the deadline passed).
async fn call(app: &App, op: Op<'_>) -> Option<JValue> {
    let Some(worker) = app.config.calibration.clone() else {
        return Some(JValue::obj([("available", JValue::Bool(false))]));
    };
    let facts = match op {
        Op::Facts => worker.state_facts(),
        Op::Listing { model, token } => {
            tokio::time::timeout(DEADLINE, worker.listing_facts(model, token))
                .await
                .ok()?
        }
        Op::Prepare {
            model,
            token,
            row_token,
        } => {
            let inference = inference(app).await;
            tokio::time::timeout(
                DEADLINE,
                worker.preparation_facts(model, token, row_token, inference),
            )
            .await
            .ok()?
            .ok()?
        }
        Op::Launch {
            model,
            token,
            row_token,
            prepared_token,
            plan,
        } => {
            let inference = inference(app).await;
            // A launch runs to its end even if the client leaves: the hold and the job's first save
            // are never cut in half. Past the deadline the client is told it is unavailable.
            let task = launch(
                worker,
                [model, token, row_token, prepared_token].map(str::to_owned),
                plan.clone(),
                inference,
            );
            tokio::time::timeout(DEADLINE, tokio::spawn(task))
                .await
                .ok()?
                .ok()?
                .ok()?
        }
    };
    let mut answer = vec![("available".into(), JValue::Bool(true))];
    if let JValue::Obj(fields) = facts {
        answer.extend(fields);
    }
    let answer = JValue::Obj(answer);
    (wtf16::stringify(&answer)?.len() <= MAX_BYTES).then_some(answer)
}
async fn launch(
    worker: Arc<Worker>,
    [model, token, row_token, prepared_token]: [String; 4],
    plan: JValue,
    inference: f64,
) -> Result<JValue, crate::model_maint::calibration_facts::Unavailable> {
    worker
        .compare_and_launch(
            &model,
            &token,
            &row_token,
            &prepared_token,
            &plan,
            inference,
        )
        .await
}
/// Core's inference budget for `conservativeFor` (0 when it cannot be read: the autoconfig budget).
async fn inference(app: &App) -> f64 {
    crate::model_state_routes::sizing_budget(app)
        .await
        .unwrap_or(0.0)
}
async fn body(mut body: Body) -> Result<JValue, Box<Response<Body>>> {
    let read = async {
        let mut bytes = Vec::new();
        while let Some(frame) = body.frame().await {
            let frame = frame.map_err(|_| Box::new(error(400, "invalid JSON")))?;
            if let Ok(data) = frame.into_data() {
                if data.len() > (1024usize * 1024).saturating_sub(bytes.len()) {
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
fn number(value: &JValue) -> f64 {
    match value {
        JValue::Undefined => f64::NAN,
        JValue::Null => 0.0,
        JValue::Bool(v) => {
            if *v {
                1.0
            } else {
                0.0
            }
        }
        JValue::Num(v) => *v,
        _ => js_json::to_js_string(value)
            .ok()
            .and_then(|s| {
                llamacpp_autoconfig::string_to_number(&s.encode_utf16().collect::<Vec<_>>()).ok()
            })
            .unwrap_or(f64::NAN),
    }
}
fn parse(input: &JValue) -> Result<(String, f64), &'static str> {
    // The route coerced the model before entering the actor's confirmation/budget checks.
    let model = if input.get("model").truthy() {
        js_json::to_js_string(input.get("model")).map_err(|_| "Choose an installed model.")?
    } else {
        String::new()
    };
    if input.get("confirmPause").as_bool() != Some(true) {
        return Err(CONFIRM);
    }
    let budget = if matches!(input.get("promptBudgetSeconds"), JValue::Undefined) {
        120.0
    } else {
        number(input.get("promptBudgetSeconds"))
    };
    if !budget.is_finite() || budget.fract() != 0.0 || !(15.0..=1800.0).contains(&budget) {
        return Err(BUDGET);
    }
    // A WTF-16 sentinel must never become a different selector on the UTF-8 command bridge.
    if model.chars().any(|c| wtf16::lone_unit(c).is_some()) || wtf16::len(&model) > 4096 {
        return Err("Choose an installed model.");
    }
    Ok((model, budget))
}
fn public_job(job: &JValue) -> Option<JValue> {
    if matches!(job, JValue::Null) {
        return Some(JValue::Null);
    }
    if !matches!(job, JValue::Obj(_))
        || job.get("id").as_str().is_none()
        || job.get("model").as_str().is_none()
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
                let v = job.get(key);
                (!matches!(v, JValue::Undefined)).then(|| ((*key).into(), v.clone()))
            })
            .collect(),
    ))
}
fn identity_ready(value: &JValue) -> bool {
    let JValue::Obj(fields) = value else {
        return false;
    };
    fields.len() == 6
        && fields.iter().all(|(k, _)| {
            ["arch", "quant", "artifact", "build", "profile", "hardware"].contains(&k.as_str())
        })
        && ["arch", "quant", "hardware"]
            .iter()
            .all(|k| matches!(value.get(k), JValue::Null | JValue::Str(_)))
        && value
            .get("artifact")
            .as_str()
            .is_some_and(|s| !s.is_empty())
        && opaque(value, "profile").is_some()
        && matches!(
            value.get("build"),
            JValue::Null | JValue::Str(_) | JValue::Obj(_)
        )
}
fn plan(facts: &JValue, budget: f64) -> Option<JValue> {
    let profile = facts.get("profile");
    let exists = profile.get("exists").as_bool()?;
    let JValue::Arr(fields) = profile.get("fields") else {
        return None;
    };
    if fields.iter().any(|field| field.as_str().is_none()) {
        return None;
    }
    let mut native = number(facts.get("row").get("meta").get("n_ctx_train"));
    if native == 0.0 || native.is_nan() {
        native = 0.0;
    }
    let conservative = facts.get("conservative");
    if !matches!(conservative, JValue::Null | JValue::Obj(_)) {
        return None;
    }
    if native == 0.0 && conservative.get("native").truthy() {
        native = number(conservative.get("native"));
    }
    if !native.is_finite() {
        return None;
    }
    let mut base = Vec::new();
    if !exists {
        if let JValue::Obj(values) = conservative.get("values") {
            for (key, value) in values {
                if key != "ctx-size" && fields.iter().any(|f| f.as_str() == Some(key.as_str())) {
                    base.push((key.clone(), value.clone()));
                }
            }
        }
        if !base
            .iter()
            .find(|(k, _)| k == "parallel")
            .is_some_and(|(_, v)| v.truthy())
        {
            base.retain(|(k, _)| k != "parallel");
            base.push(("parallel".into(), JValue::Str("1".into())));
        }
    }
    // The worker uses this same Rust ladder through its existing WASM ABI; no second candidate list.
    let ceiling = if native > 0.0 { native } else { 131072.0 };
    if !llamacpp_autoconfig::CTX_CANDIDATES
        .iter()
        .copied()
        .chain((native > 0.0).then_some(native))
        .any(|n| n >= 4096.0 && n <= ceiling)
    {
        return None;
    }
    Some(JValue::obj([
        ("promptBudgetSeconds", JValue::Num(budget)),
        ("native", JValue::Num(native)),
        ("base", JValue::Obj(base)),
    ]))
}
fn hold_failure(hold: &JValue) -> Response<Body> {
    let status = hold.get("status").as_num().unwrap_or(0.0) as u16;
    if status == 0 || status == 409 {
        return error(
            409,
            "Requests are in progress. Wait for them to finish, then start calibration.",
        );
    }
    let status =
        if !(400..=599).contains(&status) || status >= 500 || status == 401 || status == 403 {
            503
        } else {
            status
        };
    let message = hold
        .get("publicMessage")
        .as_str()
        .filter(|s| !s.is_empty())
        .unwrap_or("the inference gate refused the maintenance hold.");
    let mut value = JValue::obj([(
        "error",
        JValue::Str(format!("Calibration could not pause chat: {message}")),
    )]);
    if let (JValue::Obj(fields), JValue::Str(code)) = (&mut value, hold.get("code")) {
        fields.push(("code".into(), JValue::Str(code.clone())));
    }
    response(
        StatusCode::from_u16(status).unwrap_or(StatusCode::SERVICE_UNAVAILABLE),
        &value,
    )
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
    let (parts, input) = req.into_parts();
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
    let Some(facts) = call(app, Op::Facts).await else {
        return unavailable();
    };
    match facts.get("available").as_bool() {
        Some(false) => return error(404, "Native calibration is unavailable"),
        Some(true) => {}
        None => return unavailable(),
    }
    if parts.method == Method::GET {
        let Some(job) = public_job(facts.get("job")) else {
            return unavailable();
        };
        let mut result = vec![("job".into(), job)];
        let model = url::form_urlencoded::parse(parts.uri.query().unwrap_or("").as_bytes())
            .find(|(k, _)| k == "model")
            .map(|(_, v)| v.into_owned())
            .unwrap_or_default();
        if !model.is_empty() {
            let h = facts.get("history").get(&model);
            result.push((
                "history".into(),
                if h.truthy() {
                    h.clone()
                } else {
                    JValue::Arr(Vec::new())
                },
            ));
        }
        return response(StatusCode::OK, &JValue::Obj(result));
    }
    if parts.method != Method::POST {
        return error(405, "Method not allowed");
    }
    let parsed = match body(input).await {
        Ok(body) => body,
        Err(res) => return *res,
    };
    let (model, budget) = match parse(&parsed) {
        Ok(v) => v,
        Err(message) => return error(400, message),
    };
    let Some(facts) = call(app, Op::Facts).await else {
        return unavailable();
    };
    if facts.get("available").as_bool() != Some(true) {
        return unavailable();
    }
    if facts.get("starting").as_bool() == Some(true)
        || facts.get("job").get("status").as_str() == Some("running")
    {
        return error(409, "A calibration is already running.");
    }
    if facts.get("starting").as_bool().is_none() {
        return unavailable();
    }
    let Some(token) = opaque(&facts, "token") else {
        return unavailable();
    };
    let Some(listing) = call(
        app,
        Op::Listing {
            model: &model,
            token,
        },
    )
    .await
    else {
        return unavailable();
    };
    if listing.get("stale").as_bool() == Some(true) {
        return changed();
    }
    if listing.get("backend").as_bool() == Some(false) {
        return error(502, "The model server is not responding.");
    }
    if listing.get("backend").as_bool() != Some(true) {
        return unavailable();
    }
    if matches!(listing.get("row"), JValue::Null) {
        return error(404, "Choose an installed model.");
    }
    if !matches!(listing.get("row"), JValue::Obj(_))
        || listing.get("row").get("id").as_str() != Some(model.as_str())
    {
        return unavailable();
    }
    match listing.get("system").as_bool() {
        Some(true) => return error(400, "System routing model — not tuned"),
        Some(false) => {}
        None => return unavailable(),
    }
    let Some(row_token) = opaque(&listing, "rowToken") else {
        return unavailable();
    };
    let Some(prepared) = call(
        app,
        Op::Prepare {
            model: &model,
            token,
            row_token,
        },
    )
    .await
    else {
        return unavailable();
    };
    if prepared.get("stale").as_bool() == Some(true) {
        return changed();
    }
    if prepared.get("backend").as_bool() == Some(false) {
        return error(502, "The model server is not responding.");
    }
    if prepared.get("identityReady").as_bool() != Some(true)
        || !identity_ready(prepared.get("identity"))
    {
        return unavailable();
    }
    if prepared.get("row").get("id").as_str() != Some(model.as_str())
        || prepared.get("system").as_bool() != Some(false)
    {
        return unavailable();
    }
    if prepared.get("ladderReady").as_bool() != Some(true) {
        return response(StatusCode::SERVICE_UNAVAILABLE, &JValue::obj([("error", JValue::Str("Calibration is unavailable: the Rust context ladder could not be confirmed.".into())), ("code", JValue::Str("autoconfig_impl".into()))]));
    }
    let Some(plan) = plan(&prepared, budget) else {
        return unavailable();
    };
    let Some(prepared_token) = opaque(&prepared, "preparedToken") else {
        return unavailable();
    };
    let Some(result) = call(
        app,
        Op::Launch {
            model: &model,
            token,
            row_token,
            prepared_token,
            plan: &plan,
        },
    )
    .await
    else {
        return unavailable();
    };
    match result.get("applied").as_bool() {
        Some(true) => {}
        Some(false) => {
            return match result.get("reason").as_str() {
                Some("hold") => hold_failure(result.get("hold")),
                Some("stale") => changed(),
                // No gate hold, and the run could outlast the longest maintenance lease.
                Some("limit") => error(400, OVER_LIMIT),
                _ => unavailable(),
            };
        }
        None => return unavailable(),
    }
    let Some(job) = public_job(result.get("job")) else {
        return unavailable();
    };
    if matches!(job, JValue::Null) || job.get("model").as_str() != Some(model.as_str()) {
        return unavailable();
    }
    response(StatusCode::ACCEPTED, &job)
}
#[cfg(test)]
#[allow(clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    #[test]
    fn confirmation_budget_order_and_js_number_coercion() {
        for (wire, expected) in [
            (r#"{}"#, Err(CONFIRM)),
            (
                r#"{"confirmPause":true,"promptBudgetSeconds":null}"#,
                Err(BUDGET),
            ),
            (
                r#"{"confirmPause":true,"model":"synthetic","promptBudgetSeconds":[60]}"#,
                Ok(("synthetic".into(), 60.0)),
            ),
            (
                r#"{"confirmPause":true,"model":"synthetic"}"#,
                Ok(("synthetic".into(), 120.0)),
            ),
        ] {
            assert_eq!(parse(&wtf16::parse(wire).expect("wire")), expected);
        }
    }
    #[test]
    fn actual_node_confirmation_and_budget_refusal_fixture() {
        let fixture = wtf16::parse(include_str!(
            "../tests/fixtures/calibration-start-node.json"
        ))
        .expect("actual Node fixture");
        let JValue::Arr(cases) = fixture.get("cases") else {
            panic!("cases");
        };
        for case in cases {
            if let Err(message) = parse(case.get("input")) {
                assert_eq!(
                    case.get("result").get("body").get("error").as_str(),
                    Some(message)
                );
                assert_eq!(case.get("listingReads").as_num(), Some(0.0));
            }
        }
    }
    #[test]
    fn state_response_never_contains_original_profile_or_unknown_actor_fields() {
        let job=wtf16::parse(r#"{"id":"j","model":"synthetic","status":"interrupted","originalText":"private","unknown":"private"}"#).expect("job");
        let text = wtf16::stringify(&public_job(&job).expect("public")).expect("text");
        assert!(!text.contains("private"));
    }
}
