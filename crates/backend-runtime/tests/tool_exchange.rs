use backend_runtime::{
    approvals::{Binding, Decision, Holder},
    chat::{FutureResult, Model, Runtime, Tool},
    state::StateStore,
    Control, Fault,
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
struct Batch(AtomicUsize);
impl Model for Batch {
    fn complete<'a>(
        &'a self,
        _: &'a [Value],
        _: &'a [Value],
        _: &'a Control,
    ) -> FutureResult<'a, Value> {
        Box::pin(async move {
            if self.0.fetch_add(1, Ordering::SeqCst) > 0 {
                return Ok(json!({"role":"assistant","content":"done"}));
            }
            let calls=[("read","{\"a\":1,\"b\":2}"),("read","{\"b\":2,\"a\":1}"),("write","{}"),("read","{\"a\":1,\"b\":2}"),("write","{}")].into_iter().enumerate().map(|(i,(name,args))|json!({"id":format!("call{i}"),"type":"function","function":{"name":name,"arguments":args}})).collect::<Vec<_>>();
            Ok(json!({"role":"assistant","tool_calls":calls}))
        })
    }
}
struct Count {
    count: Arc<AtomicUsize>,
    write: bool,
}
impl Tool for Count {
    fn schema(&self) -> Value {
        json!({"type":"object"})
    }
    fn mutating(&self) -> bool {
        self.write
    }
    fn call<'a>(&'a self, _: &'a Binding, _: Value, _: &'a Control) -> FutureResult<'a, Value> {
        Box::pin(async move {
            let n = self.count.fetch_add(1, Ordering::SeqCst) + 1;
            if self.write {
                Err(Fault::Transport)
            } else {
                Ok(Value::String(format!("read{n}")))
            }
        })
    }
}
#[tokio::test]
async fn original_turn_cache_deduplicates_uncertain_writes_and_invalidates_reads(
) -> Result<(), Box<dyn std::error::Error>> {
    let dir = tempfile::tempdir()?;
    let store = Arc::new(StateStore::create(&dir.path().join("state.db"))?);
    let binding = Binding {
        user: "synthetic".into(),
        canonical_chat_id: "one".into(),
        scope: "free".into(),
        holder: Holder::Free,
        generation: "0".into(),
        start: "a".repeat(32),
        tombstoned: false,
    };
    store.create_chat(&binding)?;
    let reads = Arc::new(AtomicUsize::new(0));
    let writes = Arc::new(AtomicUsize::new(0));
    let mut tools: BTreeMap<String, Arc<dyn Tool>> = BTreeMap::new();
    tools.insert(
        "read".into(),
        Arc::new(Count {
            count: Arc::clone(&reads),
            write: false,
        }),
    );
    tools.insert(
        "write".into(),
        Arc::new(Count {
            count: Arc::clone(&writes),
            write: true,
        }),
    );
    let runtime = Runtime::new(store, Arc::new(Batch(AtomicUsize::new(0))), tools)?;
    let mut approvals = 0;
    let mut outputs = Vec::new();
    runtime
        .run(
            binding,
            &uuid::Uuid::new_v4().to_string(),
            json!({"role":"user","content":"synthetic"}),
            &Control::new(Duration::from_secs(5))?,
            |event| {
                if event.get("type") == Some(&json!("tool_pending")) {
                    approvals += 1;
                    runtime.approvals.decide(
                        "synthetic",
                        event
                            .get("id")
                            .and_then(Value::as_str)
                            .ok_or(Fault::Protocol)?,
                        Decision::Approve,
                    )?;
                }
                if event.get("type") == Some(&json!("tool_result")) {
                    outputs.push(event);
                }
                Ok(())
            },
        )
        .await?;
    assert_eq!(reads.load(Ordering::SeqCst), 2);
    assert_eq!(writes.load(Ordering::SeqCst), 1);
    assert_eq!(approvals, 1);
    assert_eq!(outputs.len(), 5);
    assert_eq!(
        outputs.first().and_then(|v| v.get("text")),
        outputs.get(1).and_then(|v| v.get("text"))
    );
    assert_eq!(
        outputs.get(2).and_then(|v| v.get("text")),
        outputs.get(4).and_then(|v| v.get("text"))
    );
    assert_ne!(
        outputs.first().and_then(|v| v.get("text")),
        outputs.get(3).and_then(|v| v.get("text"))
    );
    Ok(())
}

struct ExternalProbe(AtomicUsize);
impl Model for ExternalProbe {
    fn complete<'a>(
        &'a self,
        _: &'a [Value],
        schemas: &'a [Value],
        _: &'a Control,
    ) -> FutureResult<'a, Value> {
        Box::pin(async move {
            assert!(schemas
                .iter()
                .all(|s| s.pointer("/function/name") != Some(&json!("diary_read"))));
            if self.0.fetch_add(1, Ordering::SeqCst) > 0 {
                return Ok(json!({"role":"assistant","content":"no private data"}));
            }
            Ok(json!({"role":"assistant","tool_calls":[
                {"id":"private","type":"function","function":{"name":"diary_read","arguments":"{}"}},
                {"id":"storage","type":"function","function":{"name":"nc_webdav_read_file","arguments":"{\"path\":\"diary/note.md\"}"}}
            ]}))
        })
    }
}
#[tokio::test]
async fn external_engine_cannot_force_diary_or_unknown_storage_root(
) -> Result<(), Box<dyn std::error::Error>> {
    let dir = tempfile::tempdir()?;
    let store = Arc::new(StateStore::create(&dir.path().join("state.db"))?);
    let binding = Binding {
        user: "synthetic".into(),
        canonical_chat_id: "private".into(),
        scope: "free".into(),
        holder: Holder::Free,
        generation: "0".into(),
        start: "a".repeat(32),
        tombstoned: false,
    };
    store.create_chat(&binding)?;
    let calls = Arc::new(AtomicUsize::new(0));
    let mut tools: BTreeMap<String, Arc<dyn Tool>> = BTreeMap::new();
    for name in ["diary_read", "nc_webdav_read_file"] {
        tools.insert(
            name.into(),
            Arc::new(Count {
                count: Arc::clone(&calls),
                write: false,
            }),
        );
    }
    let runtime = Runtime::new(store, Arc::new(ExternalProbe(AtomicUsize::new(0))), tools)?;
    runtime
        .run(
            binding,
            &uuid::Uuid::new_v4().to_string(),
            json!({"role":"user","content":"synthetic"}),
            &Control::new(Duration::from_secs(5))?,
            |_| Ok(()),
        )
        .await?;
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    Ok(())
}
