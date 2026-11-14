//! #2286: the public 1×1 image recheck. Live admission stays with Node's engine arbiter;
//! the request, bounded response, identity comparison and evidence write are native.
use super::evidence_live::Failure;
use crate::{identity, reply, App};
use axum::{
    body::Body,
    http::{Request, Response, StatusCode},
    response::IntoResponse,
};
use engine_admission::{Admission, Ask, Lease};
use js_json::JValue;
use model_stream::Cancel;
use serde_json::{json, Value};
use std::{sync::Arc, time::Duration};

pub const PATH: &str = "/api/models/evidence/recheck";
const PIXEL: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg==";
const PROJECTOR: &str = "The inference server needs a multimodal projector (mmproj) configured.";
const UNREACHABLE: &str = "The image probe could not reach the inference server or timed out. Retry when the server is available.";
const LOST: &str =
    "The engine admission lease ended before the image probe completed. Try again shortly.";
const CHANGED: &str = "The model configuration changed during the image probe. Retry the recheck.";
const CATEGORY: &str =
    "Only image input can be rechecked here; use Measure context for context capacity.";
const LIMIT: usize = 64 * 1024;

pub struct Recheck {
    admission: Arc<Admission>,
    slots: Arc<tokio::sync::Semaphore>,
}
impl std::fmt::Debug for Recheck {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Recheck").finish_non_exhaustive()
    }
}
impl Recheck {
    pub fn new(get: &impl Fn(&str) -> Option<String>) -> Result<Self, String> {
        Ok(Self {
            slots: Arc::new(tokio::sync::Semaphore::new(1)),
            admission: Arc::new(Admission::new(
                &get(crate::node_link::BASE).unwrap_or_default(),
                get(crate::node_link::SECRET).as_deref(),
            )?),
        })
    }
    async fn probe(
        router: &super::router::Router,
        model: &str,
        lease: &Lease,
    ) -> Result<bool, Failure> {
        let request = async {
            let payload = json!({"model":model,"max_tokens":1,"messages":[{"role":"user","content":[{"type":"text","text":"ok"},{"type":"image_url","image_url":{"url":format!("data:image/png;base64,{PIXEL}")}}]}]});
            let req = router.image_probe_request(&payload, lease.headers());
            let mut res = req.send().await.map_err(|_| unavailable(UNREACHABLE))?;
            let status = res.status();
            // fetch(... redirect:'error') never follows or accepts a redirect.
            if status.is_redirection() && res.headers().contains_key(reqwest::header::LOCATION) {
                return Err(unavailable(UNREACHABLE));
            }
            let mut bytes = Vec::new();
            while let Some(chunk) = res.chunk().await.map_err(|_| unavailable(UNREACHABLE))? {
                let room = LIMIT.saturating_sub(bytes.len());
                bytes.extend(chunk.iter().take(room));
                if chunk.len() > room {
                    // A normal max_tokens:1 success is tiny. A truncated success is uncertain;
                    // a rejected response needs only the same capped error prefix Node read.
                    if status.is_success() {
                        return Err(unavailable(UNREACHABLE));
                    }
                    break;
                }
            }
            if status.is_success() {
                if serde_json::from_slice::<Value>(&bytes)
                    .ok()
                    .and_then(|v| v.get("error").map(|e| !e.is_null()))
                    .unwrap_or(false)
                {
                    return Err(unavailable("The inference server returned an error for the image probe. Check model configuration and server availability."));
                }
                return Ok(true);
            }
            let body = String::from_utf8_lossy(&bytes).to_ascii_lowercase();
            if body.contains("mmproj") || body.contains("projector") {
                return Ok(false);
            }
            Err(unavailable(&format!("The inference server rejected the image probe (HTTP {}). Check model configuration and server availability.",status.as_u16())))
        };
        tokio::select! {
            biased;
            _ = lease.lost().cancelled() => Err(unavailable(LOST)),
            result = tokio::time::timeout(Duration::from_secs(20), request) => result.unwrap_or_else(|_| Err(unavailable(UNREACHABLE))),
        }
    }
}
fn unavailable(message: &str) -> Failure {
    Failure {
        status: 503,
        message: message.into(),
    }
}
async fn admin(
    app: &Arc<App>,
    creds: server_auth::Creds,
) -> Result<server_auth::Identity, Box<Response<Body>>> {
    let layer = Arc::clone(&app.identity);
    match tokio::task::spawn_blocking(move || layer.gate_current_blocking(&creds, PATH)).await {
        Ok(Ok(id)) if id.user.role == server_auth::Role::Admin => Ok(id),
        Ok(Ok(_)) => Err(Box::new(reply::error(
            StatusCode::FORBIDDEN,
            "administrator required",
            true,
        ))),
        Ok(Err(r)) => Err(Box::new(r.into_response())),
        Err(_) => Err(Box::new(identity::Rejection::Unavailable.into_response())),
    }
}
pub async fn serve(app: &Arc<App>, req: Request<Body>) -> Response<Body> {
    let (parts, body) = req.into_parts();
    let creds = identity::creds_of(&parts);
    let actor = match admin(app, creds.clone()).await {
        Ok(id) => id,
        Err(r) => return *r,
    };
    if app.config.maint.router.is_none() {
        return reply::error(
            StatusCode::NOT_FOUND,
            "Qualification evidence needs the native engine",
            true,
        );
    }
    // readJson(...).catch(()=>null): malformed/oversized JSON becomes the same Choose a model.
    let parsed = match super::body_text(body, 1024 * 1024, Duration::from_secs(10)).await {
        Ok(raw) => js_json::wtf16::parse(&raw).ok(),
        Err(_) => None,
    };
    let model = parsed
        .as_ref()
        .and_then(|b| b.get("model").as_str())
        .unwrap_or("");
    // Router model identifiers must be real UTF-8; never send the WTF-16 surrogate mapping as a different model.
    if model.is_empty()
        || js_json::wtf16::len(model) > 200
        || model
            .chars()
            .any(|c| js_json::wtf16::lone_unit(c).is_some())
    {
        return reply::error(StatusCode::BAD_REQUEST, "Choose a model", true);
    }
    if parsed.as_ref().and_then(|b| b.get("category").as_str()) != Some("vision") {
        return reply::error(StatusCode::BAD_REQUEST, CATEGORY, true);
    }
    let (app, model) = (Arc::clone(app), model.to_owned());
    let Ok(permit) = Arc::clone(&app.config.maint.recheck.slots).try_acquire_owned() else {
        return reply::error(
            StatusCode::SERVICE_UNAVAILABLE,
            "An image probe is already running. Try again shortly.",
            true,
        );
    };
    // The operation owns its lease even if the browser leaves. All inference is in this task;
    // lease loss cancels/drops the HTTP response and never files a qualification.
    tokio::spawn(async move {
        let _permit = permit;
        let fresh = match admin(&app, creds.clone()).await {
            Ok(id) => id,
            Err(r) => return *r,
        };
        if fresh.user.id != actor.user.id {
            return reply::error(StatusCode::UNAUTHORIZED, "unauthorized", true);
        }
        let maint = Arc::clone(&app.config.maint);
        let cancel = Cancel::new();
        let lease = match maint
            .recheck
            .admission
            .enter(Ask::Engine { model: &model }, &cancel)
            .await
        {
            Ok(lease) => lease,
            Err(e) => return super::failure(unavailable(&e.message)),
        };
        let result = async {
            if let Err(r) = admin(&app, creds.clone()).await {
                return Err(*r);
            }
            let measured = maint
                .compute_identity(&model)
                .await
                .map_err(super::failure)?;
            let router = maint.router.as_ref().ok_or_else(|| {
                super::failure(unavailable(
                    "Qualification evidence needs the native engine",
                ))
            })?;
            let supported = Recheck::probe(router, &model, &lease)
                .await
                .map_err(super::failure)?;
            if let Err(r) = admin(&app, creds).await {
                return Err(*r);
            }
            if lease.lost().is_cancelled() {
                return Err(super::failure(unavailable(LOST)));
            }
            let current = maint
                .compute_identity(&model)
                .await
                .map_err(super::failure)?;
            if measured.as_ref().map(|i| &i.hash) != current.as_ref().map(|i| &i.hash) {
                return Err(super::failure(unavailable(CHANGED)));
            }
            if lease.lost().is_cancelled() {
                return Err(super::failure(unavailable(LOST)));
            }
            if let Some(identity) = measured {
                let record = JValue::obj([
                    ("category", JValue::Str("vision".into())),
                    (
                        "result",
                        JValue::Str(if supported { "passed" } else { "failed" }.into()),
                    ),
                    ("value", JValue::Null),
                    (
                        "suite",
                        JValue::obj([
                            ("name", JValue::Str("vision-probe".into())),
                            ("version", JValue::Num(1.0)),
                        ]),
                    ),
                    ("source", JValue::Str("recheck".into())),
                    (
                        "limitations",
                        JValue::Arr(vec![JValue::Str(
                            if supported {
                                "1×1 image accepted; not an accuracy test"
                            } else {
                                PROJECTOR
                            }
                            .into(),
                        )]),
                    ),
                ]);
                maint
                    .file_record(&model, &record, &identity, identity::now_ms())
                    .await
                    .map_err(super::failure)?;
            }
            maint.evidence_body(&model).await.map_err(super::failure)
        }
        .await;
        lease.release().await;
        match result {
            Ok(body) => super::text(200, body),
            Err(response) => response,
        }
    })
    .await
    .unwrap_or_else(|_| reply::error(StatusCode::INTERNAL_SERVER_ERROR, "Internal error", true))
}
