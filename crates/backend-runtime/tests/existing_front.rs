#![cfg(target_os = "linux")]
#[path = "../../server-auth/tests/common/mod.rs"]
mod auth_fixture;
use backend_runtime::{
    api::Api,
    chat::{FutureResult, Model, Runtime},
    project_conversations::ProjectConversations,
    Control,
};
use serde_json::{json, Value};
use std::{collections::BTreeMap, sync::Arc};
const USER: &str = "01234567-89ab-4cde-8fab-0123456789ab";
const START: &str = "0123456789abcdef0123456789abcdef";
fn replace(v: &mut Value) {
    match v {
        Value::String(s) if s == "u-member" => *s = USER.into(),
        Value::Array(a) => {
            for v in a {
                replace(v)
            }
        }
        Value::Object(o) => {
            for v in o.values_mut() {
                replace(v)
            }
        }
        _ => (),
    }
}
struct Engine;
impl Model for Engine {
    fn complete<'a>(
        &'a self,
        messages: &'a [Value],
        _: &'a [Value],
        _: &'a Control,
    ) -> FutureResult<'a, Value> {
        Box::pin(async move {
            assert_eq!(
                messages
                    .iter()
                    .find(|v| v.get("role") == Some(&json!("user")))
                    .and_then(|v| v.get("content")),
                Some(&json!("existing question"))
            );
            Ok(json!({"role":"assistant","content":"native answer"}))
        })
    }
}
#[tokio::test]
async fn original_front_auth_history_and_native_execution_share_one_transcript(
) -> Result<(), Box<dyn std::error::Error>> {
    let fixture: Value = serde_json::from_str(include_str!(
        "../../server-auth/tests/fixtures/node-auth.v1.json"
    ))?;
    let mut scenario = fixture
        .get("scenarios")
        .and_then(Value::as_array)
        .and_then(|a| a.first())
        .ok_or("scenario")?
        .clone();
    replace(&mut scenario);
    let built = auth_fixture::build(&scenario);
    let data = built.dir.path();
    let user_dir = data.join("users").join(USER);
    std::fs::create_dir_all(&user_dir)?;
    std::fs::write(
        user_dir.join(server_projects::store::PROJECTS_FILE),
        r#"{"projects":[]}"#,
    )?;
    std::fs::write(
        user_dir.join(server_projects::chats::FREE_CHATS_FILE),
        r#"[{"id":"chat1","title":"existing title","updatedAt":1}]"#,
    )?;
    server_projects::journal::set_start_id(START);
    let history = vec![
        js_json::wtf16::parse(r#"{"role":"user","content":"existing question"}"#)
            .map_err(|_| "history")?,
    ];
    let context = server_projects::chat_move::grant_context(data, USER, "chat1", START)
        .map_err(|e| format!("{e:?}"))?;
    let original =
        server_projects::transcript::read(data, USER, "chat1").map_err(|e| format!("{e:?}"))?;
    server_projects::chat_move::write_bound_transcript(
        data,
        USER,
        &context,
        &history,
        &server_projects::transcript::Base::Revision(original.revision),
        || true,
    )
    .map_err(|e| format!("{e:?}"))?;
    let store = Arc::new(ProjectConversations::open(
        data,
        START,
        &data.join("native-turns.db"),
    )?);
    let runtime = Arc::new(Runtime::new(store, Arc::new(Engine), BTreeMap::new())?);
    let lookup = |key: &str| match key {
        "UI_DATA_DIR" => Some(data.display().to_string()),
        "NOEVIA_WEB_DIST" => Some(data.join("dist").display().to_string()),
        "PUBLIC_ORIGIN" => Some("https://noevia.example.test".into()),
        "NOEVIA_LEGACY_UPSTREAM" => Some("http://127.0.0.1:9".into()),
        "NOEVIA_START_ID" => Some(START.into()),
        _ => None,
    };
    let api = Api::new(data, runtime, lookup)?;
    let front = backend_runtime::frontend::assemble(
        noevia_server::config::Config::from_lookup(lookup)?,
        api,
    )?;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;
    let (stop, stopped) = tokio::sync::oneshot::channel();
    let task = tokio::spawn(noevia_server::serve::serve(
        listener,
        front,
        noevia_server::serve::Limits::default(),
        async move {
            let _ = stopped.await;
        },
    ));
    let credentials = scenario
        .get("cases")
        .and_then(Value::as_array)
        .ok_or("cases")?
        .iter()
        .find(|v| {
            v.pointer("/expect/csrf") == Some(&Value::Bool(true))
                && v.pointer("/expect/authn/user/id") == Some(&json!(USER))
        })
        .and_then(|v| v.get("input"))
        .ok_or("credentials")?;
    let cookie = credentials
        .get("cookie")
        .and_then(Value::as_str)
        .ok_or("cookie")?;
    let csrf = credentials
        .get("csrf")
        .and_then(Value::as_str)
        .ok_or("csrf")?;
    let client = reqwest::Client::builder().no_proxy().build()?;
    let reply = client
        .post(format!("http://{address}/api/chat"))
        .header("cookie", cookie)
        .header("x-csrf-token", csrf)
        .header("origin", "https://noevia.example.test")
        .header("content-type", "application/json")
        .body(json!({"chatId":"chat1","message":"follow up","spaceId":"free"}).to_string())
        .send()
        .await?;
    assert_eq!(reply.status(), 200);
    let events = reply.text().await?;
    assert!(events.contains("native answer"), "{events}");
    let saved =
        server_projects::transcript::read(data, USER, "chat1").map_err(|e| format!("{e:?}"))?;
    let encoded = js_json::wtf16::stringify(&saved.history).ok_or("encoded")?;
    let messages: Vec<Value> = serde_json::from_str(&encoded)?;
    assert_eq!(messages.len(), 3);
    assert_eq!(
        messages.last().and_then(|v| v.get("content")),
        Some(&json!("native answer"))
    );
    let public = client
        .get(format!("http://{address}/api/chats/chat1/history"))
        .header("cookie", cookie)
        .send()
        .await?;
    assert_eq!(public.status(), 200);
    assert!(public.text().await?.contains("native answer"));
    let _ = stop.send(());
    task.await?;
    Ok(())
}
