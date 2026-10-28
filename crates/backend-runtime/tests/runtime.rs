use backend_runtime::{
    approvals::{Approvals, Binding, Decision, Holder},
    chat::{FutureResult, Model, Runtime, Tool},
    journal::Journal,
    snapshot,
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
fn binding() -> Binding {
    Binding {
        user: "synthetic-user".into(),
        canonical_chat_id: "chat_1".into(),
        scope: "free".into(),
        holder: Holder::Free,
        generation: "0".into(),
        start: "1".repeat(32),
        tombstoned: false,
    }
}
fn control() -> backend_runtime::Result<Control> {
    Control::new(Duration::from_secs(5))
}
#[test]
fn journal_preserves_original_schema_replay_and_uncertain_retry(
) -> Result<(), Box<dyn std::error::Error>> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("index.db");
    let journal = Journal::open(&path)?;
    let first = uuid::Uuid::new_v4().to_string();
    let second = uuid::Uuid::new_v4().to_string();
    journal.enqueue_id(
        &first,
        "exchange",
        &json!({"xid":"one"}),
        "2026-02-02T01:00:00",
    )?;
    journal.enqueue_id(
        &second,
        "exchange_edit",
        &json!({"xid":"one","text":"corrected"}),
        "2026-01-01T01:00:00",
    )?;
    let pending = journal.unapplied(100)?;
    assert_eq!(
        pending.iter().map(|r| r.id.as_str()).collect::<Vec<_>>(),
        vec![first.as_str(), second.as_str()]
    );
    journal.enqueue_id(
        &first,
        "exchange",
        &json!({"xid":"one"}),
        "different retry time",
    )?;
    assert_eq!(journal.pending_count()?, 2);
    assert_eq!(
        journal.enqueue_id(&first, "exchange", &json!({"xid":"other"}), "time"),
        Err(Fault::Conflict)
    );
    assert_eq!(
        journal.unapplied_exchange_edit("one")?.map(|r| r.id),
        Some(second.clone())
    );
    assert_eq!(
        journal
            .unapplied_after_with_xid(journal.rowid_of(&first)?, "one")?
            .len(),
        1
    );
    journal.mark_failed(&first, &"é".repeat(600))?;
    assert_eq!(
        journal
            .unapplied(100)?
            .first()
            .and_then(|r| r.last_error.as_ref())
            .map(|s| s.chars().count()),
        Some(500)
    );
    journal.quarantine(&first, "quarantined: synthetic")?;
    assert_eq!(journal.quarantined_count()?, 1);
    journal.mark_applied(&second)?;
    assert_eq!(journal.pending_count()?, 0);
    assert!(journal.has_exchange()?);
    drop(journal);
    let reopened = Journal::open(&path)?;
    assert!(reopened.is_applied(&first)?);
    assert!(reopened.is_applied(&second)?);
    Ok(())
}
#[tokio::test]
async fn approvals_bind_owner_context_single_use_and_revoke(
) -> Result<(), Box<dyn std::error::Error>> {
    let approvals = Approvals::default();
    let context = binding();
    let ticket = approvals.ask(context.clone(), Duration::from_secs(1))?;
    let id = ticket.id.clone();
    assert_eq!(
        approvals.decide("other", &id, Decision::ApproveAll),
        Err(Fault::Missing)
    );
    assert!(!approvals.granted(&context)?);
    approvals.decide(&context.user, &id, Decision::ApproveAll)?;
    assert!(ticket.wait(&control()?).await?);
    assert!(approvals.granted(&context)?);
    assert_eq!(
        approvals.decide(&context.user, &id, Decision::Approve),
        Err(Fault::Missing)
    );
    let mut moved = context.clone();
    moved.generation = "1".into();
    moved.holder = Holder::Project {
        id: "project".into(),
    };
    assert!(!approvals.granted(&moved)?);
    let abandoned = approvals.ask(context.clone(), Duration::from_secs(1))?;
    let id = abandoned.id.clone();
    drop(abandoned);
    assert_eq!(
        approvals.decide(&context.user, &id, Decision::Approve),
        Err(Fault::Missing)
    );
    let timeout = approvals.ask(context, Duration::from_millis(1))?;
    assert!(!timeout.wait(&control()?).await?);
    Ok(())
}
#[test]
fn job_lease_fences_cancel_and_recovery_without_replaying_remote_effects(
) -> Result<(), Box<dyn std::error::Error>> {
    let dir = tempfile::tempdir()?;
    let db = StateStore::create(&dir.path().join("runtime.db"))?;
    let id = uuid::Uuid::new_v4().to_string();
    db.enqueue_job("one", &id, "research", &json!({"question":"synthetic"}))?;
    assert!(db.claim_job("two", &id, &control()?).is_err());
    let job = db.claim_job("one", &id, &control()?)?;
    assert!(db.cancel_job("one", &id)?);
    assert_eq!(
        db.complete_job("one", &job, &json!({}), &control()?),
        Err(Fault::Conflict)
    );
    assert!(db.claim_job("one", &id, &control()?).is_err());
    let next = uuid::Uuid::new_v4().to_string();
    db.enqueue_job("one", &next, "source", &json!({}))?;
    let claimed = db.claim_job("one", &next, &control()?)?;
    db.recover_interrupted()?;
    assert_eq!(
        db.complete_job("one", &claimed, &json!({}), &control()?),
        Err(Fault::Conflict)
    );
    assert!(db.claim_job("one", &next, &control()?).is_err());
    Ok(())
}
#[test]
fn complete_snapshot_restore_keeps_receipt_bytes_and_rejects_tenant_and_overwrite(
) -> Result<(), Box<dyn std::error::Error>> {
    let dir = tempfile::tempdir()?;
    let source = rusqlite::Connection::open(dir.path().join("managed.db"))?;
    let tenant = "01234567-89ab-4cde-8fab-0123456789ab";
    source.execute_batch("CREATE TABLE meta(key TEXT PRIMARY KEY,value TEXT NOT NULL);CREATE TABLE workspace_operation_receipts(id TEXT PRIMARY KEY,response BLOB NOT NULL);CREATE TABLE journal(id TEXT PRIMARY KEY,payload TEXT NOT NULL);")?;
    source.execute("INSERT INTO meta VALUES('tenant',?)", [tenant])?;
    let receipt = vec![0, 255, 0, 32, 10];
    source.execute(
        "INSERT INTO workspace_operation_receipts VALUES('operation',?)",
        [&receipt],
    )?;
    source.execute("INSERT INTO journal VALUES('intent','pending')", [])?;
    let backup = dir.path().join("snapshot.db");
    let metadata = snapshot::create(&source, &backup, &control()?)?;
    source.execute("DELETE FROM workspace_operation_receipts", [])?;
    let destination = dir.path().join("restored.db");
    snapshot::restore(&backup, &destination, &metadata, tenant, &control()?)?;
    let restored = rusqlite::Connection::open(&destination)?;
    let actual: Vec<u8> = restored.query_row(
        "SELECT response FROM workspace_operation_receipts",
        [],
        |r| r.get(0),
    )?;
    assert_eq!(actual, receipt);
    assert_eq!(
        restored.query_row("SELECT COUNT(*) FROM journal", [], |r| r.get::<_, i64>(0))?,
        1
    );
    assert_eq!(
        snapshot::restore(&backup, &destination, &metadata, tenant, &control()?),
        Err(Fault::Conflict)
    );
    assert_eq!(
        snapshot::restore(
            &backup,
            &dir.path().join("foreign.db"),
            &metadata,
            "11234567-89ab-4cde-8fab-0123456789ab",
            &control()?
        ),
        Err(Fault::Forbidden)
    );
    Ok(())
}
struct SequenceModel {
    count: AtomicUsize,
}
impl Model for SequenceModel {
    fn complete<'a>(
        &'a self,
        messages: &'a [Value],
        _: &'a [Value],
        _: &'a Control,
    ) -> FutureResult<'a, Value> {
        Box::pin(async move {
            if self.count.fetch_add(1, Ordering::SeqCst) == 0 {
                Ok(
                    json!({"role":"assistant","content":null,"tool_calls":[{"id":"tool-1","type":"function","function":{"name":"write_note","arguments":"{\"text\":\"synthetic\"}"}}]}),
                )
            } else {
                assert!(messages
                    .iter()
                    .any(|m| m.get("role") == Some(&json!("tool"))));
                Ok(json!({"role":"assistant","content":"Saved"}))
            }
        })
    }
}
struct WriteTool {
    count: Arc<AtomicUsize>,
}
impl Tool for WriteTool {
    fn schema(&self) -> Value {
        json!({"type":"object","properties":{"text":{"type":"string"}},"required":["text"]})
    }
    fn mutating(&self) -> bool {
        true
    }
    fn call<'a>(&'a self, _: &'a Binding, _: Value, _: &'a Control) -> FutureResult<'a, Value> {
        Box::pin(async move {
            self.count.fetch_add(1, Ordering::SeqCst);
            Ok(json!({"saved":true}))
        })
    }
}
#[tokio::test]
async fn actual_loop_requires_approval_then_persists_complete_transcript(
) -> Result<(), Box<dyn std::error::Error>> {
    let dir = tempfile::tempdir()?;
    let store = Arc::new(StateStore::create(&dir.path().join("runtime.db"))?);
    let context = binding();
    store.create_chat(&context)?;
    let writes = Arc::new(AtomicUsize::new(0));
    let mut tools = BTreeMap::<String, Arc<dyn Tool>>::new();
    tools.insert(
        "write_note".into(),
        Arc::new(WriteTool {
            count: Arc::clone(&writes),
        }),
    );
    let runtime = Runtime::new(
        Arc::clone(&store),
        Arc::new(SequenceModel {
            count: AtomicUsize::new(0),
        }),
        tools,
    )?;
    let mut events = Vec::new();
    let id = uuid::Uuid::new_v4().to_string();
    let result = runtime
        .run(
            context.clone(),
            &id,
            json!({"role":"user","content":"save"}),
            &control()?,
            |event| {
                if event.get("type") == Some(&json!("tool_pending")) {
                    assert_eq!(writes.load(Ordering::SeqCst), 0);
                    runtime.approvals.decide(
                        &context.user,
                        event
                            .get("id")
                            .and_then(Value::as_str)
                            .ok_or(Fault::Protocol)?,
                        Decision::Approve,
                    )?;
                }
                events.push(event);
                Ok(())
            },
        )
        .await?;
    assert_eq!(writes.load(Ordering::SeqCst), 1);
    assert_eq!(store.history(&context)?.1, result);
    assert_eq!(
        events.last().and_then(|v| v.get("type")),
        Some(&json!("done"))
    );
    Ok(())
}
#[tokio::test]
async fn moved_context_after_approval_prevents_tool_and_history_commit(
) -> Result<(), Box<dyn std::error::Error>> {
    let dir = tempfile::tempdir()?;
    let store = Arc::new(StateStore::create(&dir.path().join("runtime.db"))?);
    let context = binding();
    store.create_chat(&context)?;
    let writes = Arc::new(AtomicUsize::new(0));
    let mut tools = BTreeMap::<String, Arc<dyn Tool>>::new();
    tools.insert(
        "write_note".into(),
        Arc::new(WriteTool {
            count: Arc::clone(&writes),
        }),
    );
    let runtime = Runtime::new(
        Arc::clone(&store),
        Arc::new(SequenceModel {
            count: AtomicUsize::new(0),
        }),
        tools,
    )?;
    let id = uuid::Uuid::new_v4().to_string();
    let mut next = context.clone();
    next.generation = "1".into();
    next.holder = Holder::Project {
        id: "project".into(),
    };
    let result = runtime
        .run(
            context.clone(),
            &id,
            json!({"role":"user","content":"save"}),
            &control()?,
            |event| {
                if event.get("type") == Some(&json!("tool_pending")) {
                    runtime.approvals.decide(
                        &context.user,
                        event
                            .get("id")
                            .and_then(Value::as_str)
                            .ok_or(Fault::Protocol)?,
                        Decision::ApproveAll,
                    )?;
                    store.replace_context(&context, &next)?;
                }
                Ok(())
            },
        )
        .await;
    assert_eq!(result, Err(Fault::Conflict));
    assert_eq!(writes.load(Ordering::SeqCst), 0);
    assert!(store.history(&next)?.1.is_empty());
    Ok(())
}

#[test]
fn native_index_separates_models_dirty_documents_and_replaced_content(
) -> Result<(), Box<dyn std::error::Error>> {
    use backend_runtime::retrieval::{Index, Plan, Target};
    use sha2::{Digest, Sha256};
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("index.db");
    let index = Index::create(&path)?;
    let target = Target {
        model: "synthetic-a".into(),
        profile: "test".into(),
    };
    let other = Target {
        model: "synthetic-b".into(),
        profile: "test".into(),
    };
    let make = |text: &str, target: Target| {
        let hash = format!("{:x}", Sha256::digest(text.as_bytes()));
        let mut vectors = BTreeMap::new();
        vectors.insert(hash.clone(), vec![1.0, 0.0]);
        Plan {
            file: "2026-10.md".into(),
            chunks: vec![diary_corpus_parse::Chunk {
                day: "2026-10-01".into(),
                header: "test".into(),
                text: text.into(),
                content_hash: hash,
            }],
            target,
            vectors,
        }
    };
    let first = make("original", target.clone());
    assert_eq!(index.apply(&first, &control()?)?.embedded, 1);
    assert_eq!(
        index
            .search(&target, &[1.0, 0.0], 8, 0.3, &control()?)?
            .first()
            .map(|h| h.text.as_str()),
        Some("original")
    );
    assert!(index
        .search(&other, &[1.0, 0.0], 8, 0.3, &control()?)?
        .is_empty());
    index.apply(&make("original", other.clone()), &control()?)?;
    index.mark_dirty("2026-10.md")?;
    assert!(index
        .search(&target, &[1.0, 0.0], 8, 0.3, &control()?)?
        .is_empty());
    index.apply(&make("corrected", target.clone()), &control()?)?;
    assert_eq!(
        index
            .search(&target, &[1.0, 0.0], 8, 0.3, &control()?)?
            .first()
            .map(|h| h.text.as_str()),
        Some("corrected")
    );
    assert!(index
        .search(&other, &[1.0, 0.0], 8, 0.3, &control()?)?
        .is_empty());
    drop(index);
    let reopened = Index::open(&path)?;
    assert_eq!(
        reopened
            .search(&target, &[1.0, 0.0], 8, 0.3, &control()?)?
            .len(),
        1
    );
    let mut missing = make("pending correction", target.clone());
    missing.vectors.clear();
    reopened.mark_dirty(&missing.file)?;
    assert!(reopened.apply(&missing, &control()?)?.pending);
    assert!(reopened
        .search(&target, &[1.0, 0.0], 8, 0.3, &control()?)?
        .is_empty());
    Ok(())
}

#[cfg(unix)]
#[test]
fn snapshot_refuses_symlinks_and_non_regular_sources() -> Result<(), Box<dyn std::error::Error>> {
    let dir = tempfile::tempdir()?;
    let bytes = dir.path().join("owned-bytes");
    std::fs::write(&bytes, b"synthetic original")?;
    let link = dir.path().join("link");
    std::os::unix::fs::symlink(&bytes, &link)?;
    assert!(snapshot::digest(&link, &control()?).is_err());
    assert!(snapshot::digest(dir.path(), &control()?).is_err());
    assert!(
        snapshot::create(&rusqlite::Connection::open_in_memory()?, &link, &control()?).is_err()
    );
    assert_eq!(std::fs::read(&bytes)?, b"synthetic original");
    Ok(())
}

#[cfg(unix)]
#[test]
fn native_database_owners_refuse_symlinks_and_keep_new_files_private(
) -> Result<(), Box<dyn std::error::Error>> {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir()?;
    let state = dir.path().join("state.db");
    let _state = backend_runtime::state::StateStore::create(&state)?;
    let index = dir.path().join("index.db");
    let _index = backend_runtime::retrieval::Index::create(&index)?;
    let journal = dir.path().join("journal.db");
    let _journal = backend_runtime::journal::Journal::open(&journal)?;
    for path in [&state, &index, &journal] {
        assert_eq!(std::fs::metadata(path)?.permissions().mode() & 0o777, 0o600);
        let alias = path.with_extension("link");
        std::os::unix::fs::symlink(path, &alias)?;
        assert!(backend_runtime::state::StateStore::open(&alias).is_err());
        assert!(backend_runtime::retrieval::Index::open(&alias).is_err());
        assert!(backend_runtime::journal::Journal::open(&alias).is_err());
    }
    assert!(backend_runtime::state::StateStore::create(&state).is_err());
    assert!(backend_runtime::retrieval::Index::create(&index).is_err());
    Ok(())
}
