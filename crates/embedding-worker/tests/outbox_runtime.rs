#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
mod support;
use embedding_worker::{fill::Store, http::Http, runtime, Index, Options};
use std::sync::{
    atomic::{AtomicBool, AtomicUsize, Ordering},
    Arc,
};
use support::*;
fn queued_media_source(
    file: serde_json::Value,
) -> (
    tempfile::TempDir,
    embedding_worker::sqlite::SqlStore,
    std::path::PathBuf,
) {
    let (tmp, mut store) = seed();
    store.seed.next = None;
    let user = "11111111-1111-4111-8111-111111111111";
    let auth = rusqlite::Connection::open(store.root.join("cowork.db")).unwrap();
    auth.execute("INSERT INTO users(id,username,username_norm,display_name,role,password_hash,webauthn_user_id,created_at,updated_at) VALUES(?,'notify','notify','Synthetic','member','synthetic','notify',0,0)",[user]).unwrap();
    let dir = store.root.join("users").join(user);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("projects.json"),
        serde_json::json!({"projects":[{"id":"p","files":[file]}]}).to_string(),
    )
    .unwrap();
    server_projects::outbox::enqueue(
        &dir,
        &[server_projects::outbox::Job {
            kind: "index",
            project: "p",
            file: Some("a.md"),
            payload: None,
        }],
        1,
    )
    .unwrap();
    // Exercise the last permitted text attempt: media failure must never exhaust it.
    rusqlite::Connection::open(dir.join("index-outbox.db"))
        .unwrap()
        .execute("UPDATE jobs SET attempts=4", [])
        .unwrap();
    (tmp, store, dir)
}
fn outbox_ready_reply(path: &str) -> (u16, String) {
    if path == "/internal/embedding-worker-owner" {
        let mut proof: serde_json::Value = serde_json::from_str(PROOF).unwrap();
        proof["indexOutboxStopped"] = true.into();
        proof["foregroundBusy"] = false.into();
        return (200, proof.to_string());
    }
    default_reply(path)
}
async fn media_notification_preserves_text(fail: bool) {
    let text = (0..400)
        .map(|n| format!("unique synthetic paragraph {n:04}. "))
        .collect::<String>();
    let (_tmp, store, dir) = queued_media_source(serde_json::json!({"name":"a.md","content":text}));
    let verify = store.clone();
    let notices = Arc::new(AtomicUsize::new(0));
    let prepared = Arc::new(AtomicBool::new(false));
    let (seen, ready, path) = (notices.clone(), prepared.clone(), dir.join("rag/p.db"));
    let stub = server(move |route, _| {
        if route == "/internal/embedding-worker-owner/source-changed" {
            seen.fetch_add(1, Ordering::SeqCst);
            let rows = rusqlite::Connection::open_with_flags(
                &path,
                rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
            )
            .ok()
            .and_then(|db| {
                db.query_row("SELECT COUNT(*) FROM chunks WHERE file='a.md'", [], |r| {
                    r.get::<_, i64>(0)
                })
                .ok()
            })
            .unwrap_or(0);
            ready.store(rows > 0, Ordering::SeqCst);
            if fail {
                return (
                    503,
                    r#"{"error":"private credential must not be recorded"}"#.into(),
                );
            }
        }
        outbox_ready_reply(route)
    })
    .await;
    let http = Http::new(&stub.base, None, &stub.base, None).unwrap();
    let worker = runtime::start(
        store,
        http,
        stub.base.clone(),
        "synthetic-token".into(),
        START.into(),
        Options::default(),
        None,
    )
    .unwrap();
    let completed = tokio::time::timeout(std::time::Duration::from_secs(10), async {
        while !server_projects::outbox::jobs(&dir).unwrap().is_empty() {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await
    .is_ok();
    // A new two-second drain must not retry a dropped media notification.
    if completed {
        tokio::time::sleep(std::time::Duration::from_millis(2200)).await;
    }
    // Join even on the failing baseline, before asserting, so an expected red
    // result cannot leave background tasks behind during runtime teardown.
    worker.shutdown().await;
    assert!(
        completed,
        "text job must complete on its last attempt even when media notification fails"
    );
    assert_eq!(
        notices.load(Ordering::SeqCst),
        1,
        "notify once, independently of text success"
    );
    assert!(
        prepared.load(Ordering::SeqCst),
        "prepare source before media notification"
    );
    let index = Index {
        user: Some("11111111-1111-4111-8111-111111111111".into()),
        project: "p".into(),
    };
    assert_eq!(
        verify.remaining(&index, &target("active")).unwrap(),
        0,
        "text vectors must finish despite media failure"
    );
    let db = rusqlite::Connection::open(dir.join("rag/p.db")).unwrap();
    assert!(
        db.query_row(
            "SELECT COUNT(*) FROM chunks WHERE file='a.md' AND direct=0",
            [],
            |r| r.get::<_, i64>(0)
        )
        .unwrap()
            > 1
    );
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn media_notification_is_sent_once_after_source_preparation() {
    media_notification_preserves_text(false).await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn failed_media_notification_does_not_fail_text_or_retry_notification() {
    media_notification_preserves_text(true).await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn failed_text_job_persists_a_bounded_sanitized_diagnostic() {
    let (_tmp, store, dir) =
        queued_media_source(serde_json::json!({"name":"a.md","textRef":"a".repeat(64)}));
    let stub = server(|path, _| outbox_ready_reply(path)).await;
    let http = Http::new(&stub.base, None, &stub.base, None).unwrap();
    let worker = runtime::start(
        store,
        http,
        stub.base.clone(),
        "synthetic-token".into(),
        START.into(),
        Options::default(),
        None,
    )
    .unwrap();
    let db = rusqlite::Connection::open(dir.join("index-outbox.db")).unwrap();
    wait_until(|| {
        db.query_row("SELECT state='failed' FROM jobs", [], |r| {
            r.get::<_, bool>(0)
        })
        .unwrap_or(false)
    })
    .await;
    worker.shutdown().await;
    let error: String = db
        .query_row("SELECT last_error FROM jobs", [], |r| r.get(0))
        .expect("failed text row must retain a diagnostic");
    let error: serde_json::Value = serde_json::from_str(&error).unwrap();
    assert_eq!(error["operation"], "prepare");
    assert_eq!(error["job"].as_str().unwrap().len(), 16);
    assert!(error["reason"].as_str().unwrap().len() < 64);
    assert!(!error.to_string().contains("a.md"));
    assert_eq!(
        db.query_row("SELECT attempts FROM jobs", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        5
    );
}
