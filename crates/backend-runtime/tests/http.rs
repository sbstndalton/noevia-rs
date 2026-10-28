#[path = "../../server-auth/tests/common/mod.rs"]
mod auth_fixture;
use backend_runtime::{
    api::Api,
    approvals::{Binding, Holder},
    chat::{FutureResult, Model, Runtime},
    state::StateStore,
    Control,
};
use serde_json::{json, Value};
use std::{
    collections::BTreeMap,
    net::SocketAddr,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
};
struct ModelReply(Arc<AtomicUsize>);
impl Model for ModelReply {
    fn complete<'a>(
        &'a self,
        _: &'a [Value],
        _: &'a [Value],
        _: &'a Control,
    ) -> FutureResult<'a, Value> {
        Box::pin(async move {
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok(json!({"role":"assistant","content":"Synthetic native reply"}))
        })
    }
}
#[tokio::test]
async fn actual_loopback_auth_csrf_and_native_chat_store() -> Result<(), Box<dyn std::error::Error>>
{
    let fixtures: Value = serde_json::from_str(include_str!(
        "../../server-auth/tests/fixtures/node-auth.v1.json"
    ))?;
    let scenario = fixtures
        .get("scenarios")
        .and_then(Value::as_array)
        .and_then(|a| a.first())
        .ok_or("scenario")?;
    let built = auth_fixture::build(scenario);
    let state = Arc::new(StateStore::create(
        &built.dir.path().join("native-state.db"),
    )?);
    let binding = Binding {
        user: "u-member".into(),
        canonical_chat_id: "chat1".into(),
        scope: "free".into(),
        holder: Holder::Free,
        generation: "0".into(),
        start: "1".repeat(32),
        tombstoned: false,
    };
    state.create_chat(&binding)?;
    let calls = Arc::new(AtomicUsize::new(0));
    let runtime = Arc::new(Runtime::new(
        Arc::clone(&state),
        Arc::new(ModelReply(Arc::clone(&calls))),
        BTreeMap::new(),
    )?);
    let api = Api::new(built.dir.path(), runtime, |key| {
        (key == "PUBLIC_ORIGIN").then(|| "https://noevia.example.test".into())
    })?;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;
    let task = tokio::spawn(async move {
        axum::serve(
            listener,
            api.router()
                .into_make_service_with_connect_info::<SocketAddr>(),
        )
        .await
    });
    let client = reqwest::Client::builder().no_proxy().build()?;
    let url = format!("http://{address}/api/chat");
    let body = json!({"chatId":"chat1","message":"synthetic test","spaceId":"free"}).to_string();
    let unauthorized = client
        .post(&url)
        .header("content-type", "application/json")
        .body(body.clone())
        .send()
        .await?;
    assert_eq!(unauthorized.status(), 401);
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    let cases = scenario
        .get("cases")
        .and_then(Value::as_array)
        .ok_or("cases")?;
    let credentials = cases
        .iter()
        .find(|c| {
            c.pointer("/expect/csrf") == Some(&Value::Bool(true))
                && c.pointer("/expect/authn/user/id") == Some(&json!("u-member"))
        })
        .and_then(|c| c.get("input"))
        .ok_or("credentials")?;
    let cookie = credentials
        .get("cookie")
        .and_then(Value::as_str)
        .ok_or("cookie")?;
    let csrf = credentials
        .get("csrf")
        .and_then(Value::as_str)
        .ok_or("csrf")?;
    let forbidden = client
        .post(&url)
        .header("cookie", cookie)
        .header("origin", "https://noevia.example.test")
        .header("content-type", "application/json")
        .body(body.clone())
        .send()
        .await?;
    assert_eq!(forbidden.status(), 403);
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    let reply = client
        .post(&url)
        .header("cookie", cookie)
        .header("x-csrf-token", csrf)
        .header("origin", "https://noevia.example.test")
        .header("content-type", "application/json")
        .body(body)
        .send()
        .await?;
    assert_eq!(reply.status(), 200);
    assert_eq!(
        reply
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok()),
        Some("text/event-stream")
    );
    let text = reply.text().await?;
    assert!(text.contains("Synthetic native reply"));
    assert!(text.contains("\"type\":\"done\""));
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(state.history(&binding)?.1.len(), 2);
    task.abort();
    Ok(())
}
