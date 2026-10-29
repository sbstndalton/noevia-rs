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
struct ModelReply(Arc<AtomicUsize>, std::path::PathBuf);
impl Model for ModelReply {
    fn complete<'a>(
        &'a self,
        messages: &'a [Value],
        _: &'a [Value],
        _: &'a Control,
    ) -> FutureResult<'a, Value> {
        Box::pin(async move {
            let system = messages
                .first()
                .and_then(|v| v.get("content"))
                .and_then(Value::as_str)
                .ok_or(backend_runtime::Fault::Protocol)?;
            assert!(system.contains("Synthetic custom instruction"));
            assert!(system.contains("Synthetic memory"));
            assert!(system.contains("Calendar facts, computed by the server"));
            if self.0.fetch_add(1, Ordering::SeqCst) == 1 {
                let db = rusqlite::Connection::open(&self.1)?;
                db.execute(
                    "DELETE FROM sessions WHERE user_id='01234567-89ab-4cde-8fab-0123456789ab'",
                    [],
                )?;
            }
            Ok(json!({"role":"assistant","content":"Synthetic native reply"}))
        })
    }
}
#[tokio::test]
async fn actual_loopback_auth_csrf_and_native_chat_store() -> Result<(), Box<dyn std::error::Error>>
{
    let fixtures: Value = serde_json::from_str(
        &include_str!("../../server-auth/tests/fixtures/node-auth.v1.json")
            .replace("u-member", "01234567-89ab-4cde-8fab-0123456789ab"),
    )?;
    let scenario = fixtures
        .get("scenarios")
        .and_then(Value::as_array)
        .and_then(|a| a.first())
        .ok_or("scenario")?;
    let built = auth_fixture::build(scenario);
    let user_dir = built
        .dir
        .path()
        .join("users/01234567-89ab-4cde-8fab-0123456789ab");
    std::fs::create_dir_all(&user_dir)?;
    std::fs::write(
        user_dir.join("account-instructions.json"),
        json!({"text":"Synthetic custom instruction","style":"concise"}).to_string(),
    )?;
    std::fs::write(
        user_dir.join("account-memory.json"),
        json!({"memories":["Synthetic memory","Synthetic memory"]}).to_string(),
    )?;
    let state = Arc::new(StateStore::create(
        &built.dir.path().join("native-state.db"),
    )?);
    let binding = Binding {
        user: "01234567-89ab-4cde-8fab-0123456789ab".into(),
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
        Arc::new(ModelReply(
            Arc::clone(&calls),
            server_store::db_path(built.dir.path()),
        )),
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
                && c.pointer("/expect/authn/user/id")
                    == Some(&json!("01234567-89ab-4cde-8fab-0123456789ab"))
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
    let before = state.history(&binding)?;
    let revoked = client
        .post(&url)
        .header("cookie", cookie)
        .header("x-csrf-token", csrf)
        .header("origin", "https://noevia.example.test")
        .header("content-type", "application/json")
        .body(
            json!({"chatId":"chat1","message":"revoked during model call","spaceId":"free"})
                .to_string(),
        )
        .send()
        .await?;
    let events = revoked.text().await?;
    assert!(events.contains("Unauthorized"));
    assert!(!events.contains("Synthetic native reply"));
    assert_eq!(state.history(&binding)?, before);
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    task.abort();
    Ok(())
}
