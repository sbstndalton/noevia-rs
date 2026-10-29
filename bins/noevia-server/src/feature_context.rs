//! Bounded control route on the EXISTING token-authenticated private store.
//! The shared App owns the controller; no second process identity is constructed here.
use crate::{
    project_store::{self, Store},
    upkeep::WriterLayer,
};
use axum::{
    body::Body,
    http::{HeaderValue, Request, Response, StatusCode},
};
use http_body_util::BodyExt;
use server_account::feature_context::{
    startup, wire, Cancellation, Control, Controller, FeatureRuntime,
};
use std::{
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::sync::Semaphore;

pub struct Layer {
    controller: Arc<Controller>,
    writes: Arc<WriterLayer>,
    start: String,
    trust_proxy: bool,
    admission: Arc<Semaphore>,
}
impl Layer {
    pub fn new(config: &crate::config::Config, writes: Arc<WriterLayer>) -> Option<Arc<Self>> {
        let start = config.start_id.clone()?;
        let runtime = Arc::new(FeatureRuntime::new().ok()?);
        Some(Arc::new(Self {
            controller: Arc::new(Controller::new(runtime, config.feature_context_env)),
            writes,
            start,
            trust_proxy: config.trust_proxy,
            admission: Arc::new(Semaphore::new(4)),
        }))
    }
    /// Public feature/device owners must clone this controller, not create another runtime.
    pub fn controller(&self) -> Arc<Controller> {
        Arc::clone(&self.controller)
    }
}
fn reply(status: StatusCode, body: &str) -> Response<Body> {
    let mut response = Response::new(Body::from(body.to_owned()));
    *response.status_mut() = status;
    response
        .headers_mut()
        .insert("content-type", HeaderValue::from_static("application/json"));
    response
        .headers_mut()
        .insert("cache-control", HeaderValue::from_static("no-store"));
    response
}
fn unavailable() -> Response<Body> {
    reply(
        StatusCode::SERVICE_UNAVAILABLE,
        r#"{"error":"Feature runtime context is unavailable.","code":"FEATURE_RUNTIME_CONTEXT_UNAVAILABLE"}"#,
    )
}
async fn read(mut body: Body) -> Result<Vec<u8>, StatusCode> {
    let mut bytes = Vec::new();
    while let Some(frame) = body.frame().await {
        let frame = frame.map_err(|_| StatusCode::BAD_REQUEST)?;
        if let Ok(data) = frame.into_data() {
            if data.len() > wire::BODY_LIMIT.saturating_sub(bytes.len()) {
                return Err(StatusCode::PAYLOAD_TOO_LARGE);
            }
            bytes.extend_from_slice(&data);
        }
    }
    Ok(bytes)
}
pub async fn serve(
    layer: Option<Arc<Layer>>,
    store: Store,
    request: Request<Body>,
) -> Response<Body> {
    let started = Instant::now();
    let deadline = started + Duration::from_millis(2200);
    // Reject before polling even an unbounded/pending body, and forbid ambiguous capabilities.
    if request
        .headers()
        .get_all(project_store::TOKEN_HEADER)
        .iter()
        .count()
        != 1
        || !project_store::authorised(&store, request.headers())
    {
        return reply(StatusCode::UNAUTHORIZED, r#"{"error":"unauthorised"}"#);
    }
    let Some(layer) = layer else {
        return unavailable();
    };
    if store.start_id.as_ref() != layer.start {
        return unavailable();
    }
    let permit = match Arc::clone(&layer.admission).try_acquire_owned() {
        Ok(permit) => permit,
        Err(_) => return unavailable(),
    };
    if request.headers().get_all("content-type").iter().count() != 1
        || !request
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .is_some_and(|s| {
                s.split(';')
                    .next()
                    .is_some_and(|s| s.trim().eq_ignore_ascii_case("application/json"))
            })
    {
        return reply(
            StatusCode::BAD_REQUEST,
            r#"{"error":"invalid content type"}"#,
        );
    }
    let bytes = match tokio::time::timeout_at(deadline.into(), read(request.into_body())).await {
        Ok(Ok(bytes)) => bytes,
        Ok(Err(status)) => return reply(status, r#"{"error":"invalid context body"}"#),
        Err(_) => return unavailable(),
    };
    let request = match wire::request_id(&bytes) {
        Ok(id) => id,
        Err(_) => {
            return reply(
                StatusCode::BAD_REQUEST,
                r#"{"error":"invalid context message"}"#,
            )
        }
    };
    let cancellation = Cancellation::default();
    let control = Control {
        request,
        deadline,
        cancelled: cancellation.signal(),
    };
    let worker = tokio::task::spawn_blocking(move || {
        // Retain admission through the real worker, including after handler timeout/drop.
        let _permit = permit;
        let writer = layer.writes.writer()?;
        wire::dispatch(
            &bytes,
            &layer.start,
            &layer.controller,
            &writer,
            &control,
            |tx, facts| {
                if facts.trust_proxy != layer.trust_proxy {
                    return Err(server_store::StoreError::NotReady(
                        "feature environment mismatch".into(),
                    ));
                }
                startup::initialize(tx, facts, crate::identity::now_ms())
            },
        )
    });
    let result = tokio::time::timeout_at(deadline.into(), worker).await;
    match result {
        Ok(Ok(Ok(text))) => reply(StatusCode::OK, &text),
        _ => unavailable(),
    }
    // cancellation drops on success, failure, timeout AND request-future cancellation.
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{config::Config, App};
    use axum::body::{Body, Bytes};
    use serde_json::{json, Value};
    use server_store::{db_path, rusqlite::Connection};
    use std::{
        pin::Pin,
        sync::atomic::{AtomicBool, Ordering},
        task::{Context, Poll},
    };
    use tower::ServiceExt;
    type Error = Box<dyn std::error::Error>;
    const START: &str = "11111111111111111111111111111111";
    const TOKEN: &str = "synthetic-store-capability-1234567890";
    fn fixture() -> Result<(tempfile::TempDir, Arc<App>, Store), Error> {
        let dir = tempfile::tempdir()?;
        let db = Connection::open(db_path(dir.path()))?;
        db.execute_batch(include_str!(
            "../../../crates/server-store/tests/fixtures/node-schema.sql"
        ))?;
        drop(db);
        let config = Config::from_lookup(|key| match key {
            "NOEVIA_LEGACY_UPSTREAM" => Some("http://127.0.0.1:9".into()),
            "UI_DATA_DIR" => Some(dir.path().to_string_lossy().into_owned()),
            "NOEVIA_START_ID" => Some(START.into()),
            "PUBLIC_ORIGIN" => Some("https://noevia.example.test".into()),
            _ => None,
        })?;
        let app = App::new(config);
        let store = Store {
            data_dir: dir.path().to_owned(),
            token: Arc::from(TOKEN),
            switch: server_store::RustProjects::owned(),
            start_id: Arc::from(START),
        };
        Ok((dir, app, store))
    }
    struct Pending(Arc<AtomicBool>);
    impl http_body::Body for Pending {
        type Data = Bytes;
        type Error = std::io::Error;
        fn poll_frame(
            self: Pin<&mut Self>,
            _: &mut Context<'_>,
        ) -> Poll<Option<Result<http_body::Frame<Bytes>, std::io::Error>>> {
            self.0.store(true, Ordering::SeqCst);
            Poll::Pending
        }
    }
    #[tokio::test]
    async fn capability_and_content_type_reject_before_body_poll() -> Result<(), Error> {
        let (_dir, app, store) = fixture()?;
        for mode in ["missing", "duplicate", "content-type"] {
            let polled = Arc::new(AtomicBool::new(false));
            let mut request = Request::post("/v1/features/context").header(
                "content-type",
                if mode == "content-type" {
                    "text/plain"
                } else {
                    "application/json"
                },
            );
            if mode != "missing" {
                request = request.header(project_store::TOKEN_HEADER, TOKEN);
            }
            if mode == "duplicate" {
                request = request.header(project_store::TOKEN_HEADER, TOKEN);
            }
            let request = request.body(Body::new(Pending(Arc::clone(&polled))))?;
            let response =
                project_store::router_with_context(store.clone(), app.feature_context.clone())
                    .oneshot(request)
                    .await?;
            assert_eq!(
                response.status(),
                if mode == "content-type" {
                    StatusCode::BAD_REQUEST
                } else {
                    StatusCode::UNAUTHORIZED
                }
            );
            assert!(!polled.load(Ordering::SeqCst));
        }
        Ok(())
    }
    #[tokio::test]
    async fn streamed_body_limit_refuses_without_installing_epoch() -> Result<(), Error> {
        let (dir, app, store) = fixture()?;
        let request = Request::post("/v1/features/context")
            .header(project_store::TOKEN_HEADER, TOKEN)
            .header("content-type", "application/json")
            .body(Body::from(vec![b' '; wire::BODY_LIMIT + 1]))?;
        let response = project_store::router_with_context(store, app.feature_context.clone())
            .oneshot(request)
            .await?;
        assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
        let db = Connection::open(db_path(dir.path()))?;
        assert_eq!(
            db.query_row(
                "SELECT count(*) FROM settings WHERE key=?1",
                [server_store::FEATURE_RUNTIME_INCARNATION_KEY],
                |r| r.get::<_, i64>(0)
            )?,
            0
        );
        Ok(())
    }
    struct OwnListener(tokio::task::AbortHandle);
    impl Drop for OwnListener {
        fn drop(&mut self) {
            self.0.abort();
        }
    }
    #[tokio::test]
    async fn real_private_tcp_registers_committed_startup_then_renews_and_releases_same_app(
    ) -> Result<(), Error> {
        let (dir, app, store) = fixture()?;
        let db = Connection::open(db_path(dir.path()))?;
        db.execute_batch("INSERT INTO settings VALUES('feature:astraReview','true'); INSERT INTO device_authorizations(device_code_hash,user_code_hash,client_name,ip,user_agent,created_at,expires_at,interval_ms,status) VALUES('d','u','synthetic','127.0.0.1','test',1,99999999,5000,'pending');")?;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let address = listener.local_addr()?;
        let router = project_store::router_with_context(store, app.feature_context.clone());
        let task = tokio::spawn(async move { axum::serve(listener, router).await });
        let _own = OwnListener(task.abort_handle());
        let client = reqwest::Client::builder()
            .no_proxy()
            .timeout(Duration::from_secs(4))
            .build()?;
        let url = format!("http://{address}/v1/features/context");
        let send = |body: Value| {
            client
                .post(&url)
                .header(project_store::TOKEN_HEADER, TOKEN)
                .header("content-type", "application/json")
                .body(body.to_string())
                .send()
        };
        let hello = send(
            json!({"op":"hello","request":"00000000-0000-4000-8000-000000000001","start":START}),
        )
        .await?;
        assert_eq!(hello.status(), 200);
        let hello: Value = serde_json::from_str(&hello.text().await?)?;
        let writer = app.writes.writer()?;
        let snapshot=writer.read(|r|Ok(json!({"sources":{"decision":server_account::feature_context::source_fingerprint(r,server_account::feature_context::ContextSource::Decision)?,"machines":server_account::feature_context::source_fingerprint(r,server_account::feature_context::ContextSource::InferenceMachines)?},"availability":{"stepSupervision":null,"toolGate":null,"chatFraming":null,"framingReasoner":null,"systemOneRouting":null}})))?;
        let mut body = json!({"op":"register","request":"00000000-0000-4000-8000-000000000004","start":START,"incarnation":hello.get("incarnation").ok_or("epoch")?,"boot":"00000000-0000-4000-8000-000000000002","session":"00000000-0000-4000-8000-000000000003","sequence":0,"facts":{"env":vec![Option::<bool>::None;23],"restart":[false,false],"browser":"available","code":false,"trustProxy":false},"snapshot":snapshot});
        for _ in 0..2 {
            let response = send(body.clone()).await?;
            assert_eq!(response.status(), 200);
            let response: Value = serde_json::from_str(&response.text().await?)?;
            assert_eq!(response.get("status"), Some(&json!("ready")));
        }
        assert_eq!(
            db.query_row("SELECT count(*) FROM device_authorizations", [], |r| r
                .get::<_, i64>(0))?,
            0
        );
        assert_eq!(
            db.query_row(
                "SELECT value FROM settings WHERE key='feature:plannerReview'",
                [],
                |r| r.get::<_, String>(0)
            )?,
            "true"
        );
        let object = body.as_object_mut().ok_or("object")?;
        object.remove("facts");
        object.insert("op".into(), json!("renew"));
        object.insert(
            "request".into(),
            json!("00000000-0000-4000-8000-000000000005"),
        );
        object.insert("sequence".into(), json!(1));
        assert_eq!(send(body.clone()).await?.status(), 200);
        let object = body.as_object_mut().ok_or("object")?;
        object.remove("snapshot");
        object.insert("op".into(), json!("release"));
        object.insert(
            "request".into(),
            json!("00000000-0000-4000-8000-000000000006"),
        );
        object.insert("sequence".into(), json!(0));
        assert_eq!(send(body).await?.status(), 200);
        writer.read(|r| {
            assert!(app
                .feature_context
                .as_ref()
                .ok_or_else(|| server_store::StoreError::NotReady("context".into()))?
                .controller()
                .admission(r)
                .is_err());
            Ok(())
        })?;
        drop(_own);
        let _ = task.await;
        Ok(())
    }
}
