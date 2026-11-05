#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
mod support;
use embedding_worker::{
    media_policy,
    media_store::{digest, MediaItem},
    sqlite::SqlStore,
    Control,
};
use serde_json::{json, Value};
use server_projects::extract::{Engine, EngineError, Limits, OcrCaps};
use std::sync::atomic::{AtomicUsize, Ordering};
const USER: &str = "11111111-1111-4111-8111-111111111111";
const OTHER: &str = "22222222-2222-4222-8222-222222222222";
fn setup(file: Value) -> (tempfile::TempDir, SqlStore, std::path::PathBuf) {
    let (tmp, mut store) = support::seed();
    store.seed.next = None;
    store.seed.active.profile = "embeddinggemma-2".into();
    let auth = rusqlite::Connection::open(store.root.join("cowork.db")).unwrap();
    for (user, name) in [(USER, "one"), (OTHER, "two")] {
        auth.execute("INSERT INTO users(id,username,username_norm,display_name,role,password_hash,webauthn_user_id,created_at,updated_at) VALUES(?,?,?,'Synthetic','member','synthetic',?,0,0)",rusqlite::params![user,name,name,name]).unwrap();
        let dir = store.root.join("users").join(user);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("projects.json"),
            json!({"projects":[{"id":"p","files":[file]}]}).to_string(),
        )
        .unwrap();
        server_projects::outbox::enqueue(&dir, &[], 0).unwrap();
    }
    let dir = store.root.join("users").join(USER);
    (tmp, store, dir)
}
fn item() -> MediaItem {
    MediaItem {
        file: "a.png".into(),
        hash: format!("img:{}", "a".repeat(64)),
        label: "Image a.png".into(),
        meta: json!({"source":"a.png"}),
        path: "/synthetic/unused".into(),
        mime: "image/png".into(),
    }
}
#[test]
fn media_restart_is_idempotent_tenant_isolated_and_fenced_after_cancel_or_change() {
    let (_tmp, store, dir) = setup(json!({"name":"a.png"}));
    let ctl = Control::default();
    let target = store.seed.active.clone();
    let snap = store.media_snapshot(USER, "p").unwrap();
    store.create_media_index(&snap, &ctl).unwrap();
    let rows = store
        .reconcile_media(&snap, &[item()], &target, &ctl)
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(
        store
            .publish_media_rows(&snap, &target, &rows, &[vec![1., 2.]], 0, &ctl)
            .unwrap(),
        1
    );
    assert!(
        store
            .clone()
            .reconcile_media(&snap, &[item()], &target, &ctl)
            .unwrap()
            .is_empty(),
        "restart must keep the active vector and row"
    );
    let other = store.media_snapshot(OTHER, "p").unwrap();
    store.create_media_index(&other, &ctl).unwrap();
    assert_eq!(
        store
            .reconcile_media(&other, &[item()], &target, &ctl)
            .unwrap()
            .len(),
        1,
        "same names in another tenant have independent progress"
    );
    let cancelled = Control::default();
    cancelled.stop();
    assert!(store
        .publish_media_rows(&snap, &target, &rows, &[vec![2., 3.]], 0, &cancelled)
        .is_err());
    std::fs::write(
        dir.join("projects.json"),
        r#"{"projects":[{"id":"p","files":[]}]}"#,
    )
    .unwrap();
    assert!(store
        .publish_media_rows(&snap, &target, &rows, &[vec![2., 3.]], 0, &ctl)
        .is_err());
    let current = store.media_snapshot(USER, "p").unwrap();
    store.reconcile_media(&current, &[], &target, &ctl).unwrap();
    let db = rusqlite::Connection::open(dir.join("rag/p.db")).unwrap();
    assert_eq!(
        db.query_row("SELECT COUNT(*) FROM chunks WHERE kind='image'", [], |r| {
            r.get::<_, i64>(0)
        })
        .unwrap(),
        0
    );
}
#[test]
fn tail_resumes_pending_rows_without_recrossing_or_duplicating_the_head() {
    let head = "Synthetic head. ".repeat(20000);
    let file = json!({"name":"a.pdf","content":head,"document":{"availableVersion":"a".repeat(64),"indexing":"ready"}});
    let (_tmp, store, dir) = setup(file);
    let ctl = Control::default();
    server_projects::outbox::enqueue(
        &dir,
        &[server_projects::outbox::Job {
            kind: "index",
            project: "p",
            file: Some("a.pdf"),
            payload: None,
        }],
        1,
    )
    .unwrap();
    let claim = server_projects::outbox::claim_index(&dir, false, 2)
        .unwrap()
        .unwrap();
    store.index_source(USER, &claim, &ctl).unwrap().unwrap();
    let snapshot = store.media_snapshot(USER, "p").unwrap();
    let target = store.seed.active.clone();
    let head_piece = embedding_worker::outbox::chunks(&head).remove(0);
    let window = json!({"chunks":[{"body":head_piece,"page":1},{"body":"[Page 2]\nSynthetic tail first","page":2},{"body":"[Page 3]\nSynthetic tail second","page":3}],"fromPage":1,"toPage":3,"final":true});
    let (first, coverage) = store
        .prepare_tail(&snapshot, "a.pdf", &head, &window, &target, 1, &ctl)
        .unwrap();
    assert_eq!(first.len(), 1);
    assert_eq!(first[0].hash, digest("[Page 2]\nSynthetic tail first"));
    let (resumed, _) = store
        .clone()
        .prepare_tail(&snapshot, "a.pdf", &head, &window, &target, 1, &ctl)
        .unwrap();
    assert_eq!(
        resumed[0].id, first[0].id,
        "a crash before publication retries the same row"
    );
    store
        .publish_media_rows(&snapshot, &target, &resumed, &[vec![1., 2.]], 0, &ctl)
        .unwrap();
    assert_eq!(
        store
            .tail_coverage(&snapshot, "a.pdf", &target, &coverage)
            .unwrap(),
        (1, Some(2))
    );
    let (next, coverage) = store
        .prepare_tail(&snapshot, "a.pdf", &head, &window, &target, 1, &ctl)
        .unwrap();
    assert_eq!(next.len(), 1);
    assert_ne!(next[0].id, first[0].id);
    store
        .publish_media_rows(&snapshot, &target, &next, &[vec![2., 3.]], 0, &ctl)
        .unwrap();
    assert_eq!(
        store
            .tail_coverage(&snapshot, "a.pdf", &target, &coverage)
            .unwrap(),
        (0, None)
    );
    assert!(store
        .prepare_tail(&snapshot, "a.pdf", &head, &window, &target, 1, &ctl)
        .unwrap()
        .0
        .is_empty());
    let db = rusqlite::Connection::open(dir.join("rag/p.db")).unwrap();
    assert_eq!(
        db.query_row(
            "SELECT COUNT(*) FROM chunks WHERE COALESCE(meta,'') LIKE '{\"tail\":1,%'",
            [],
            |r| r.get::<_, i64>(0)
        )
        .unwrap(),
        2
    );
}
struct Unavailable(AtomicUsize);
impl Engine for Unavailable {
    fn docling_configured(&self) -> bool {
        true
    }
    fn docling(
        &self,
        _: &str,
        _: &[u8],
        _: bool,
        _: Option<(u32, u32)>,
    ) -> Result<js_json::JValue, EngineError> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Err(EngineError::new("Document extraction unavailable"))
    }
    fn docling_ranges(&self, _: usize) -> Option<bool> {
        Some(false)
    }
    fn ocr_configured(&self) -> bool {
        false
    }
    fn ocr_pages(
        &self,
        _: &[u8],
        _: &[u32],
        _: Option<u32>,
        _: Option<u64>,
    ) -> Result<Vec<js_json::JValue>, EngineError> {
        Err(EngineError::new("unavailable"))
    }
    fn ocr_caps(&self) -> Option<OcrCaps> {
        None
    }
    fn multimodal(&self) -> bool {
        true
    }
    fn busy_waits(&self) -> Vec<std::time::Duration> {
        vec![]
    }
}
fn document(completion: bool) -> (tempfile::TempDir, SqlStore, std::path::PathBuf, Value) {
    let bytes = b"%PDF-synthetic";
    let hash = server_projects::extract::sha_hex(bytes);
    let version = "a".repeat(64);
    let file = json!({"name":"a.pdf","content":"existing text","document":{"version":version,"byteHash":hash,"bytes":bytes.len(),"availableVersion":version,"extractor":"old","state":"ready","indexing":"ready"}});
    let (tmp, store, dir) = setup(file.clone());
    let docs = server_projects::lifecycle::documents_dir(&dir, "p");
    std::fs::create_dir_all(&docs).unwrap();
    std::fs::write(docs.join(format!("{hash}.pdf")), bytes).unwrap();
    let mut record = json!({"pageTexts":[{"number":1,"text":"existing text","status":"native"}],"text":"existing text","state":"ready"});
    if completion {
        record["completion"] = json!(server_projects::extract::COMPLETION);
    }
    std::fs::write(docs.join(format!("{version}.json")), record.to_string()).unwrap();
    (tmp, store, dir, file)
}
#[test]
fn node_completion_marker_survives_restart_without_an_engine_call() {
    let (_tmp, store, dir, file) = document(true);
    let engine = Unavailable(AtomicUsize::new(0));
    let ctl = Control::default();
    let snapshot = store.media_snapshot(USER, "p").unwrap();
    assert_eq!(
        store
            .document_step(&snapshot, &file, false, &engine, &Limits::from_env(), &ctl)
            .unwrap()
            .status,
        "complete"
    );
    let snapshot = store.clone().media_snapshot(USER, "p").unwrap();
    let file = &snapshot.project["files"][0];
    assert_eq!(
        file["document"]["completionSweep"],
        file["document"]["version"]
    );
    assert!(!media_policy::upload_candidate(file, "old"));
    assert_eq!(engine.0.load(Ordering::SeqCst), 0);
    assert!(dir.join("projects.json").exists());
}
#[test]
fn unavailable_figure_engine_keeps_progress_and_original_record_for_retry() {
    let (_tmp, store, dir, file) = document(false);
    let snapshot = store.media_snapshot(USER, "p").unwrap();
    let engine = Unavailable(AtomicUsize::new(0));
    let ctl = Control::default();
    let path = server_projects::lifecycle::documents_dir(&dir, "p")
        .join(format!("{}.json", "a".repeat(64)));
    let before = std::fs::read(&path).unwrap();
    let result = store
        .document_step(&snapshot, &file, true, &engine, &Limits::from_env(), &ctl)
        .unwrap();
    assert_eq!(result.status, "retry");
    assert!(result.unreachable);
    assert_eq!(std::fs::read(&path).unwrap(), before);
    assert_eq!(
        store.media_snapshot(USER, "p").unwrap().project,
        snapshot.project
    );
    assert!(engine.0.load(Ordering::SeqCst) > 0);
}
#[tokio::test]
async fn foreground_proof_refuses_tail_requests_and_unreachable_proof_fails_closed() {
    use embedding_worker::http::Http;
    use std::sync::Arc;
    let foreground = Arc::new(std::sync::atomic::AtomicBool::new(true));
    let observed = foreground.clone();
    let stub = support::server(move |path, _| {
        if path == "/internal/embedding-worker-owner" {
            let mut proof: Value = serde_json::from_str(support::PROOF).unwrap();
            proof["mediaWorkersStopped"] = true.into();
            proof["foregroundBusy"] = observed.load(Ordering::SeqCst).into();
            return (200, proof.to_string());
        }
        support::default_reply(path)
    })
    .await;
    let http = Http::new("", None, &stub.base, None).unwrap();
    assert!(!http
        .media_ready(&stub.base, "synthetic", support::START, true)
        .await
        .unwrap());
    assert!(
        http.media_ready(&stub.base, "synthetic", support::START, false)
            .await
            .unwrap(),
        "figure extraction does not yield to chat"
    );
    foreground.store(false, Ordering::SeqCst);
    assert!(http
        .media_ready(&stub.base, "synthetic", support::START, true)
        .await
        .unwrap());
    let denied = support::server(|_, _| (503, "{}".into())).await;
    assert!(!http
        .media_ready(&denied.base, "synthetic", support::START, true)
        .await
        .unwrap());
}
#[tokio::test]
async fn shutdown_drops_a_pending_image_request_without_publishing() {
    use embedding_worker::http::Http;
    use tokio::io::AsyncReadExt;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let (sent, received) = tokio::sync::oneshot::channel();
    let peer = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut b = [0u8; 4096];
        let _ = socket.read(&mut b).await;
        let _ = sent.send(());
        while socket.read(&mut b).await.unwrap_or(0) > 0 {}
    });
    let http = Http::new("", None, &base, None).unwrap();
    let control = Control::default();
    let stop = control.clone();
    let target = support::target("synthetic-image");
    let request = tokio::spawn(async move {
        http.media(
            &target,
            "synthetic-image",
            json!({"type":"image_url","image_url":{"url":"data:image/png;base64,eA=="}}),
            &control,
        )
        .await
    });
    tokio::time::timeout(std::time::Duration::from_secs(2), received)
        .await
        .unwrap()
        .unwrap();
    stop.stop();
    assert!(
        tokio::time::timeout(std::time::Duration::from_secs(2), request)
            .await
            .unwrap()
            .unwrap()
            .is_err()
    );
    tokio::time::timeout(std::time::Duration::from_secs(2), peer)
        .await
        .unwrap()
        .unwrap();
}

#[test]
fn legacy_projects_without_an_outbox_can_backfill_their_first_image() {
    let (_tmp, store, dir) = setup(json!({"name":"a.png"}));
    std::fs::remove_file(dir.join("index-outbox.db")).unwrap();
    let snapshot = store.media_snapshot(USER, "p").unwrap();
    let control = Control::default();
    store.create_media_index(&snapshot, &control).unwrap();
    assert_eq!(
        store
            .reconcile_media(&snapshot, &[item()], &store.seed.active, &control)
            .unwrap()
            .len(),
        1
    );
}

#[test]
fn source_hints_survive_a_new_generation_and_recover_after_a_crash() {
    let folder = tempfile::tempdir().unwrap();
    let path = folder.path().join(format!("{}.json", "a".repeat(64)));
    let first = json!({"start":"one","user":USER,"project":"p","sequence":1});
    std::fs::write(&path, first.to_string()).unwrap();
    let claims = embedding_worker::media_runtime::claim_requests(folder.path(), "one");
    assert_eq!(claims.len(), 1);
    assert!(!path.exists());
    let next = json!({"start":"one","user":USER,"project":"p","sequence":2});
    std::fs::write(&path, next.to_string()).unwrap();
    std::fs::remove_file(&claims[0].0).unwrap();
    assert!(
        path.exists(),
        "finishing an older pass must not delete a newer hint"
    );
    let claims = embedding_worker::media_runtime::claim_requests(folder.path(), "one");
    assert_eq!(claims[0].1["sequence"], 2);
    let recovered = embedding_worker::media_runtime::claim_requests(folder.path(), "one");
    assert_eq!(recovered.len(), 1);
    assert_eq!(recovered[0].1["sequence"], 2);
    assert!(
        embedding_worker::media_runtime::claim_requests(folder.path(), "other-start").is_empty()
    );
}
struct Ready(AtomicUsize);
impl Engine for Ready {
    fn docling_configured(&self) -> bool {
        true
    }
    fn docling(
        &self,
        _: &str,
        _: &[u8],
        _: bool,
        _: Option<(u32, u32)>,
    ) -> Result<js_json::JValue, EngineError> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(js_json::wtf16::parse(r#"{"total":1,"pages":[{"number":1,"text":"Synthetic readable page","status":"native","method":"docling"}]}"#).unwrap())
    }
    fn docling_ranges(&self, _: usize) -> Option<bool> {
        Some(false)
    }
    fn ocr_configured(&self) -> bool {
        false
    }
    fn ocr_pages(
        &self,
        _: &[u8],
        _: &[u32],
        _: Option<u32>,
        _: Option<u64>,
    ) -> Result<Vec<js_json::JValue>, EngineError> {
        Err(EngineError::new("unavailable"))
    }
    fn ocr_caps(&self) -> Option<OcrCaps> {
        None
    }
    fn multimodal(&self) -> bool {
        true
    }
    fn busy_waits(&self) -> Vec<std::time::Duration> {
        vec![]
    }
}
#[test]
fn figure_refresh_publishes_native_extraction_and_queues_the_existing_index_owner() {
    let (_tmp, store, dir, file) = document(false);
    let engine = Ready(AtomicUsize::new(0));
    let control = Control::default();
    let snapshot = store.media_snapshot(USER, "p").unwrap();
    assert_eq!(
        store
            .document_step(
                &snapshot,
                &file,
                true,
                &engine,
                &Limits::from_env(),
                &control
            )
            .unwrap()
            .status,
        "refreshed"
    );
    let snapshot = store.media_snapshot(USER, "p").unwrap();
    let updated = &snapshot.project["files"][0];
    assert!(updated["document"]["extractor"]
        .as_str()
        .unwrap()
        .contains(":figures-v1"));
    assert!(updated["content"]
        .as_str()
        .unwrap()
        .contains("Synthetic readable page"));
    assert!(!media_policy::needs_figures(updated, &Value::Null));
    assert_eq!(
        server_projects::outbox::claim_index(&dir, false, 3)
            .unwrap()
            .unwrap()
            .file
            .as_deref(),
        Some("a.pdf")
    );
    assert_eq!(engine.0.load(Ordering::SeqCst), 1);
    assert!(
        store.media_snapshot(OTHER, "p").unwrap().project["files"][0]["document"]["extractor"]
            == "old"
    );
    assert_eq!(
        std::fs::read_dir(dir.join("source-staging"))
            .unwrap()
            .count(),
        0,
        "staging is removed after publication"
    );
}

#[test]
fn restart_sweeps_only_owned_abandoned_media_staging() {
    let dir = tempfile::tempdir().unwrap();
    let owned = dir.path().join(format!("media-{}", "a".repeat(64)));
    let other = dir.path().join("other-staging");
    std::fs::create_dir(&owned).unwrap();
    std::fs::write(owned.join("synthetic"), b"unfinished").unwrap();
    std::fs::create_dir(&other).unwrap();
    embedding_worker::media_runtime::sweep_staging(dir.path());
    assert!(!owned.exists());
    assert!(other.exists());
}

#[test]
fn renewing_authority_never_revives_a_pass_that_lost_coverage() {
    let (_tmp, store, _dir) = setup(json!({"name":"a.png"}));
    let owner = Control::default();
    let old = owner.pass();
    let snapshot = store.media_snapshot(USER, "p").unwrap();
    store.create_media_index(&snapshot, &old).unwrap();
    let rows = store
        .reconcile_media(&snapshot, &[item()], &store.seed.active, &old)
        .unwrap();
    owner.suspend();
    owner.resume();
    assert!(!owner.suspended());
    assert!(old.suspended());
    assert!(store
        .publish_media_rows(
            &snapshot,
            &store.seed.active,
            &rows,
            &[vec![1., 2.]],
            0,
            &old
        )
        .is_err());
    let successor = owner.pass();
    assert!(!successor.suspended());
    assert_eq!(
        store
            .publish_media_rows(
                &snapshot,
                &store.seed.active,
                &rows,
                &[vec![1., 2.]],
                0,
                &successor
            )
            .unwrap(),
        1
    );
}
