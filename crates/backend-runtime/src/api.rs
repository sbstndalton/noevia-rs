//! Candidate HTTP integration. Existing native account/auth handlers are reused directly.
//! The execution store must be populated explicitly; existing project-state migration,
//! non-basic chat modes, provider routing and the other route families are not adopted here.
use crate::{chat::Runtime, Control, Fault, Result};
use axum::{
    body::{to_bytes, Body, Bytes},
    extract::{ConnectInfo, State},
    http::{HeaderValue, Request, Response, StatusCode},
    Router,
};
use futures_core::Stream;
use serde_json::{json, Value};
use server_auth::{Authenticator, Creds, Refusal};
use server_store::{RustAuth, Writer};
use std::{
    net::SocketAddr,
    path::Path,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::sync::{mpsc, Semaphore};
pub struct Api {
    account: Arc<server_account::Account>,
    auth: Arc<Authenticator>,
    writer: Arc<Writer>,
    pub runtime: Arc<Runtime>,
    admission: Arc<Semaphore>,
    chat_admission: Arc<Semaphore>,
    diary: Option<Arc<crate::diary_http::DiaryHttp>>,
}
fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|v| i64::try_from(v.as_millis()).ok())
        .unwrap_or(i64::MAX)
}
impl Api {
    pub fn new(
        data_dir: &Path,
        runtime: Arc<Runtime>,
        get: impl Fn(&str) -> Option<String>,
    ) -> Result<Arc<Self>> {
        Self::with_diary(data_dir, runtime, get, None)
    }
    pub fn with_diary(
        data_dir: &Path,
        runtime: Arc<Runtime>,
        get: impl Fn(&str) -> Option<String>,
        diary: Option<Arc<crate::diary_http::DiaryHttp>>,
    ) -> Result<Arc<Self>> {
        let config = server_auth::AuthConfig::from_lookup(&get).map_err(|_| Fault::Invalid)?;
        let auth = Arc::new(Authenticator::new(config));
        let ownership = RustAuth::owned();
        let writer = Arc::new(Writer::open(data_dir, ownership).map_err(|_| Fault::Unavailable)?);
        let account = Arc::new(server_account::Account::new(
            Arc::clone(&writer),
            Arc::clone(&auth),
            server_account::Settings::from_lookup(get, data_dir.to_owned()),
            ownership,
            now(),
        ));
        Ok(Arc::new(Self {
            account,
            auth,
            writer,
            runtime,
            diary,
            admission: Arc::new(Semaphore::new(16)),
            chat_admission: Arc::new(Semaphore::new(4)),
        }))
    }
    /// Owned by the candidate listener task; dropping/aborting it ends scheduling.
    pub fn start_replay_worker(self: &Arc<Self>) -> tokio::task::JoinHandle<()> {
        let api = Arc::clone(self);
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(30));
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                interval.tick().await;
                let Some(diary) = api.diary.clone() else {
                    return;
                };
                let writer = Arc::clone(&api.writer);
                let result = tokio::task::spawn_blocking(move || {
                    diary.replay_pending(&writer, &Control::new(Duration::from_secs(20))?)
                })
                .await;
                if !matches!(result, Ok(Ok(_))) {
                    eprintln!("native Diary replay deferred; pending intent retained");
                }
            }
        })
    }
    pub fn dav_service(
        &self,
        data: &Path,
        scope: server_dav::Scope,
        config: server_dav::Config,
        protection: Option<diary_workspace_transaction::ProtectionConfigDto>,
    ) -> Result<Arc<server_dav::DavService<crate::dav::DavBackend>>> {
        let tenants = self.diary.as_ref().ok_or(Fault::Unavailable)?.corpora();
        let store = Arc::new(server_store::Store::open(data).map_err(|_| Fault::Unavailable)?);
        let mut backend =
            crate::dav::DavBackend::new(store, Arc::clone(&self.writer), tenants, scope)?;
        if let Some(protection) = protection {
            backend = backend.with_workspace(protection);
        }
        Ok(Arc::new(server_dav::DavService::new(
            Arc::new(backend),
            config,
        )))
    }
    pub async fn serve_request(
        self: Arc<Self>,
        peer: SocketAddr,
        request: Request<Body>,
    ) -> Response<Body> {
        handle(State(self), ConnectInfo(peer), request).await
    }
    pub fn router(self: Arc<Self>) -> Router {
        Router::new().fallback(handle).with_state(self)
    }
}
fn response(status: u16, value: Value) -> Response<Body> {
    let mut reply = Response::new(Body::from(value.to_string()));
    *reply.status_mut() = StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    reply.headers_mut().insert(
        "content-type",
        HeaderValue::from_static("application/json; charset=utf-8"),
    );
    reply
        .headers_mut()
        .insert("cache-control", HeaderValue::from_static("no-store"));
    reply.headers_mut().insert(
        "x-content-type-options",
        HeaderValue::from_static("nosniff"),
    );
    reply
}
fn failure(fault: Fault) -> Response<Body> {
    let (status, message) = match fault {
        Fault::Invalid => (400, "Invalid request"),
        Fault::Unauthorized => (401, "unauthorized"),
        Fault::Forbidden => (403, "Forbidden"),
        Fault::Missing => (404, "Not found"),
        Fault::Conflict => (409, "The operation conflicts with current state"),
        Fault::Capacity => (429, "Operation capacity exceeded"),
        Fault::Cancelled => (409, "Operation cancelled"),
        Fault::Deadline => (504, "Operation deadline exceeded"),
        _ => (503, "The operation is unavailable"),
    };
    response(status, json!({"error":message}))
}
async fn handle(
    State(api): State<Arc<Api>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    request: Request<Body>,
) -> Response<Body> {
    let permit = match Arc::clone(&api.admission).try_acquire_owned() {
        Ok(p) => p,
        Err(_) => return failure(Fault::Capacity),
    };
    let path = request.uri().path().to_owned();
    let method = request.method().as_str().to_owned();
    let query = request.uri().query().unwrap_or("").to_owned();
    let creds = Creds::from_headers(
        &method,
        request
            .headers()
            .iter()
            .map(|(name, value)| (name.as_str(), value.as_bytes())),
    );
    let content_type = request
        .headers()
        .get("content-type")
        .map(|v| server_auth::request::latin1(v.as_bytes()))
        .unwrap_or_default();
    let user_agent = request
        .headers()
        .get("user-agent")
        .map(|v| server_auth::request::latin1(v.as_bytes()))
        .unwrap_or_default();
    let body = if matches!(method.as_str(), "GET" | "HEAD" | "OPTIONS") {
        Vec::new()
    } else {
        match tokio::time::timeout(
            Duration::from_secs(15),
            to_bytes(request.into_body(), server_account::BODY_LIMIT),
        )
        .await
        {
            Ok(Ok(bytes)) => bytes.to_vec(),
            _ => return failure(Fault::Invalid),
        }
    };
    let account_request = server_account::Request {
        method: method.clone(),
        path: path.clone(),
        creds: creds.clone(),
        user_agent,
        content_type,
        client_ip: peer.ip().to_string(),
        body: server_account::Body {
            bytes: body.clone(),
            over: false,
        },
    };
    let account = Arc::clone(&api.account);
    let outcome =
        tokio::task::spawn_blocking(move || account.handle_with_clock(&account_request, now)).await;
    match outcome {
        Ok(server_account::Outcome::Reply(reply)) => {
            let mut result = response(
                reply.status,
                serde_json::from_str(&reply.body)
                    .unwrap_or_else(|_| json!({"error":"Internal error"})),
            );
            for cookie in reply.cookies {
                if let Ok(value) = HeaderValue::from_str(&cookie) {
                    result.headers_mut().append("set-cookie", value);
                }
            }
            for (name, value) in reply.headers {
                if let (Ok(name), Ok(value)) = (
                    axum::http::header::HeaderName::from_bytes(name.as_bytes()),
                    HeaderValue::from_str(&value),
                ) {
                    result.headers_mut().append(name, value);
                }
            }
            return result;
        }
        Ok(server_account::Outcome::Pass) => {}
        Err(_) => return failure(Fault::Unavailable),
    }
    let writer = Arc::clone(&api.writer);
    let auth = Arc::clone(&api.auth);
    let gate_path = path.clone();
    let gate_creds = creds.clone();
    let identity = match tokio::task::spawn_blocking(move || {
        writer.read(|r| auth.gate_in(r, &gate_creds, &gate_path, now()))
    })
    .await
    {
        Ok(Ok(Ok(identity))) => identity,
        Ok(Ok(Err(Refusal::Unauthorized))) => return failure(Fault::Unauthorized),
        Ok(Ok(Err(_))) => return failure(Fault::Forbidden),
        _ => return failure(Fault::Unavailable),
    };
    if crate::diary_http::DiaryHttp::handles(&path) {
        let Some(diary) = api.diary.clone() else {
            return failure(Fault::Unavailable);
        };
        let writer = Arc::clone(&api.writer);
        let auth = Arc::clone(&api.auth);
        let user = identity.user.id.clone();
        return match tokio::task::spawn_blocking(move || {
            diary.handle(
                &writer,
                &auth,
                &creds,
                &user,
                &method,
                &path,
                &query,
                &body,
                now(),
            )
        })
        .await
        {
            Ok(Ok((status, value))) => response(status, value),
            Ok(Err(e)) => failure(e),
            Err(_) => failure(Fault::Unavailable),
        };
    }
    if let Some(id) = path
        .strip_prefix("/api/tool-approvals/")
        .filter(|id| !id.is_empty() && !id.contains('/'))
    {
        if method != "POST" {
            return response(405, json!({"error":"method not allowed"}));
        }
        let body: Value = match serde_json::from_slice(&body) {
            Ok(v) => v,
            Err(_) => return response(400, json!({"error":"invalid JSON"})),
        };
        let decision = match body
            .get("decision")
            .and_then(Value::as_str)
            .map(crate::approvals::Decision::parse)
        {
            Some(Ok(v)) => v,
            _ => {
                return response(
                    400,
                    json!({"error":"decision must be 'approve', 'deny' or 'approve_all'"}),
                )
            }
        };
        return match api
            .runtime
            .approvals
            .decide(&identity.user.id, id, decision)
        {
            Ok(()) => response(200, json!({"ok":true})),
            Err(Fault::Missing) => response(404, json!({"error":"no such pending approval"})),
            Err(e) => failure(e),
        };
    }
    if path != "/api/chat" {
        return response(404, json!({"error":"Not found"}));
    }
    if method != "POST" {
        return response(405, json!({"error":"method not allowed"}));
    }
    let body: Value = match serde_json::from_slice(&body) {
        Ok(Value::Object(v)) => Value::Object(v),
        Ok(_) => return response(400, json!({"error":"request body must be a JSON object"})),
        Err(_) => return response(400, json!({"error":"invalid JSON"})),
    };
    // Explicitly refuse unsupported semantics instead of silently dropping a pinned skill,
    // a Diary request, an attachment, Cowork mode, or per-turn tool permission selections.
    if body.get("mode").is_some_and(|v| v != "chat")
        || body.get("spaceId").is_some_and(|v| v != "free")
        || [
            "skill",
            "turnToolboxes",
            "turnImageIds",
            "compactOnly",
            "projectId",
        ]
        .iter()
        .any(|k| body.get(k).is_some_and(|v| !v.is_null()))
    {
        return response(
            409,
            json!({"error":"This native candidate does not implement this chat mode or context yet"}),
        );
    }
    let Some(message) = body
        .get("message")
        .and_then(Value::as_str)
        .filter(|v| !v.is_empty())
    else {
        return response(400, json!({"error":"message required"}));
    };
    let Some(chat) = body.get("chatId").and_then(Value::as_str) else {
        return response(400, json!({"error":"chatId required"}));
    };
    let session_store = Arc::new(crate::session_conversations::SessionConversations::new(
        Arc::clone(&api.runtime.store),
        Arc::clone(&api.writer),
        Arc::clone(&api.auth),
        creds,
        identity.user.id.clone(),
        now,
    ));
    let runtime = Arc::new(api.runtime.with_conversations(session_store.clone()));
    let binding = match runtime.store.context(&identity.user.id, chat) {
        Ok(v) => v,
        Err(e) => return failure(e),
    };
    if binding.holder != crate::approvals::Holder::Free || binding.scope != "free" {
        return response(
            409,
            json!({"error":"Project instruction/context assembly is not implemented by this candidate"}),
        );
    }
    // Long-lived chats must not consume the request permits needed to approve them.
    drop(permit);
    let chat_permit = match Arc::clone(&api.chat_admission).try_acquire_owned() {
        Ok(p) => p,
        Err(_) => return failure(Fault::Capacity),
    };
    let control = match Control::new(Duration::from_secs(300)) {
        Ok(v) => v,
        Err(e) => return failure(e),
    };
    let turn = uuid::Uuid::new_v4().to_string();
    let user = json!({"role":"user","content":message});
    let owned = control.clone();
    let (sender, receiver) = mpsc::channel::<Bytes>(64);
    let job = tokio::spawn(async move {
        let _permit = chat_permit;
        let result = runtime
            .run(binding, &turn, user, &owned, |event| {
                session_store.check()?;
                sender
                    .try_send(Bytes::from(format!("data: {event}\n\n")))
                    .map_err(|_| Fault::Cancelled)
            })
            .await;
        if let Err(error) = result {
            let _ = sender.try_send(Bytes::from(format!(
                "data: {}\n\n",
                json!({"type":"error","text":error.to_string()})
            )));
        }
    });
    let stream = Events {
        receiver,
        control,
        job,
    };
    let mut reply = Response::new(Body::from_stream(stream));
    reply.headers_mut().insert(
        "content-type",
        HeaderValue::from_static("text/event-stream"),
    );
    reply.headers_mut().insert(
        "cache-control",
        HeaderValue::from_static("no-cache, no-store"),
    );
    reply
        .headers_mut()
        .insert("x-accel-buffering", HeaderValue::from_static("no"));
    reply
}
struct Events {
    receiver: mpsc::Receiver<Bytes>,
    control: Control,
    job: tokio::task::JoinHandle<()>,
}
impl Stream for Events {
    type Item = std::result::Result<Bytes, std::io::Error>;
    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.get_mut().receiver.poll_recv(cx).map(|v| v.map(Ok))
    }
}
impl Drop for Events {
    fn drop(&mut self) {
        self.control.cancel();
        self.job.abort();
    }
}
