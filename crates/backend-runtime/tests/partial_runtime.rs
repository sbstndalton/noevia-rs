#![cfg(target_os = "linux")]
use backend_runtime::{
    chat::{FutureResult, Model, Runtime},
    project_conversations::ProjectConversations,
    state::Conversations,
    Control,
};
use serde_json::{json, Value};
use std::{
    collections::BTreeMap,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    time::Duration,
};
const USER: &str = "01234567-89ab-4cde-8fab-0123456789ab";
const START: &str = "0123456789abcdef0123456789abcdef";
struct Slow {
    round: AtomicUsize,
    saved: Arc<tokio::sync::Notify>,
}
impl Model for Slow {
    fn complete<'a>(
        &'a self,
        _: &'a [Value],
        _: &'a [Value],
        _: &'a Control,
    ) -> FutureResult<'a, Value> {
        Box::pin(async { Ok(json!({"role":"assistant","content":"finished"})) })
    }
    fn complete_stream<'a>(
        &'a self,
        _: &'a [Value],
        _: &'a [Value],
        _: &'a Control,
        emit: &'a mut (dyn FnMut(Value) -> backend_runtime::Result<()> + Send),
    ) -> FutureResult<'a, Value> {
        Box::pin(async move {
            if self.round.fetch_add(1, Ordering::SeqCst) == 0 {
                emit(json!({"type":"delta","text":"survives "}))?;
                tokio::time::sleep(Duration::from_millis(4050)).await;
                emit(json!({"type":"delta","text":"abort"}))?;
                self.saved.notify_one();
                std::future::pending::<()>().await;
            }
            emit(json!({"type":"delta","text":"finished"}))?;
            Ok(json!({"role":"assistant","content":"finished"}))
        })
    }
}
#[tokio::test]
async fn original_partial_is_running_then_offered_after_abort_and_cleared_after_success(
) -> Result<(), Box<dyn std::error::Error>> {
    let dir = tempfile::tempdir()?;
    let user = dir.path().join("users").join(USER);
    std::fs::create_dir_all(&user)?;
    std::fs::write(
        user.join(server_projects::store::PROJECTS_FILE),
        r#"{"projects":[]}"#,
    )?;
    std::fs::write(
        user.join(server_projects::chats::FREE_CHATS_FILE),
        r#"[{"id":"chat1","title":"test"}]"#,
    )?;
    server_projects::journal::set_start_id(START);
    let store = Arc::new(ProjectConversations::open(
        dir.path(),
        START,
        &dir.path().join("turns.db"),
    )?);
    let binding = store.context(USER, "chat1")?;
    let saved = Arc::new(tokio::sync::Notify::new());
    let runtime = Arc::new(Runtime::new(
        Arc::clone(&store),
        Arc::new(Slow {
            round: AtomicUsize::new(0),
            saved: Arc::clone(&saved),
        }),
        BTreeMap::new(),
    )?);
    let worker = Arc::clone(&runtime);
    let context = binding.clone();
    let task = tokio::spawn(async move {
        worker
            .run(
                context,
                &uuid::Uuid::new_v4().to_string(),
                json!({"role":"user","content":"first"}),
                &Control::new(Duration::from_secs(10))?,
                |_| Ok(()),
            )
            .await
    });
    tokio::time::timeout(Duration::from_secs(8), saved.notified()).await?;
    let file = server_projects::partial::file_for(&user, "chat1");
    let running = js_json::wtf16::parse(&std::fs::read_to_string(&file)?).map_err(|_| "parse")?;
    assert_eq!(
        server_projects::partial::state(&running, START),
        server_projects::partial::State::Running
    );
    task.abort();
    assert!(task.await.is_err());
    let interrupted =
        js_json::wtf16::parse(&std::fs::read_to_string(&file)?).map_err(|_| "parse")?;
    assert_eq!(
        server_projects::partial::state(&interrupted, START),
        server_projects::partial::State::Interrupted
    );
    assert_eq!(interrupted.get("content").as_str(), Some("survives abort"));
    runtime
        .run(
            binding,
            &uuid::Uuid::new_v4().to_string(),
            json!({"role":"user","content":"second"}),
            &Control::new(Duration::from_secs(5))?,
            |_| Ok(()),
        )
        .await?;
    assert!(!file.exists());
    Ok(())
}
