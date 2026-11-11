#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
use super::*;
use crate::test_support as support;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
const USER: &str = "11111111-1111-4111-8111-111111111111";
struct EngineStub(AtomicUsize);
impl Engine for EngineStub {
    fn docling_configured(&self) -> bool {
        true
    }
    fn docling(
        &self,
        _: &str,
        _: &[u8],
        _: bool,
        _: Option<(u32, u32)>,
    ) -> Result<js_json::JValue, extract::EngineError> {
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
    ) -> Result<Vec<js_json::JValue>, extract::EngineError> {
        Err(extract::EngineError::new("unavailable"))
    }
    fn ocr_caps(&self) -> Option<extract::OcrCaps> {
        None
    }
    fn multimodal(&self) -> bool {
        true
    }
    fn busy_waits(&self) -> Vec<Duration> {
        vec![]
    }
}
fn file(name: &str, version: char, extractor: &str) -> Value {
    let bytes = format!("%PDF-synthetic {name}");
    json!({"name":name,"content":"existing text","document":{"version":version.to_string().repeat(64),"byteHash":extract::sha_hex(bytes.as_bytes()),"bytes":bytes.len(),"availableVersion":version.to_string().repeat(64),"extractor":extractor,"state":"ready","indexing":"ready"}})
}
fn seed(
    files: &[Value],
    originals: bool,
) -> (
    tempfile::TempDir,
    crate::sqlite::SqlStore,
    std::path::PathBuf,
) {
    let (tmp, mut store) = support::seed();
    store.seed.next = None;
    let auth = rusqlite::Connection::open(store.root.join("cowork.db")).unwrap();
    auth.execute("INSERT INTO users(id,username,username_norm,display_name,role,password_hash,webauthn_user_id,created_at,updated_at) VALUES(?1,?1,?1,'Synthetic','member','synthetic',?1,0,0)",[USER]).unwrap();
    let dir = store.root.join("users").join(USER);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("projects.json"),
        json!({"projects":[{"id":"p","files":files}]}).to_string(),
    )
    .unwrap();
    server_projects::outbox::enqueue(&dir, &[], 0).unwrap();
    let docs = server_projects::lifecycle::documents_dir(&dir, "p");
    std::fs::create_dir_all(&docs).unwrap();
    for f in files {
        std::fs::write(docs.join(format!("{}.json", f["document"]["version"].as_str().unwrap())), json!({"pageTexts":[{"number":1,"text":"existing text","status":"native"}],"text":"existing text","state":"ready"}).to_string()).unwrap();
    }
    if originals {
        for file in files {
            let bytes = format!("%PDF-synthetic {}", file["name"].as_str().unwrap());
            std::fs::write(
                docs.join(format!("{}.pdf", extract::sha_hex(bytes.as_bytes()))),
                bytes.as_bytes(),
            )
            .unwrap();
        }
    }
    (tmp, store, dir)
}
async fn host(store: crate::sqlite::SqlStore) -> (NativeHost, support::Server) {
    let server = support::server(|path, _| {
        if path == "/internal/embedding-worker-owner" {
            return (200, json!({"protocol":"rust-embedding-worker-v1","start":support::START,"nodeWorkersStopped":true,"mediaWorkersStopped":true,"foregroundBusy":false}).to_string());
        }
        support::default_reply(path)
    }).await;
    let host = crate::runtime::media_test_host(
        store,
        crate::http::Http::new("", None, &server.base, None).unwrap(),
        server.base.clone(),
    );
    (host, server)
}
fn worker<'a>(
    host: &'a NativeHost,
    state: &'a Mutex<Value>,
    engine: Arc<EngineStub>,
) -> Worker<'a> {
    Worker {
        host,
        control: Control::default(),
        state,
        engine,
        cfg: json!({"perPass":1,"perHour":480,"perDocument":1,"uploads":2,"batch":1,"duty":1,"minPauseMs":0}),
        spent: VecDeque::new(),
        backoff: BTreeMap::new(),
    }
}
struct EnvRestore(Vec<(&'static str, Option<std::ffi::OsString>)>);
impl EnvRestore {
    fn set(entries: &[(&'static str, &str)]) -> Self {
        Self(
            entries
                .iter()
                .map(|(key, value)| {
                    let old = std::env::var_os(key);
                    std::env::set_var(key, value);
                    (*key, old)
                })
                .collect(),
        )
    }
}
impl Drop for EnvRestore {
    fn drop(&mut self) {
        for (key, value) in self.0.drain(..).rev() {
            if let Some(value) = value {
                std::env::set_var(key, value);
            } else {
                std::env::remove_var(key);
            }
        }
    }
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn figure_pass_refreshes_two_documents_after_the_first_changes_the_project() {
    let _env = EnvRestore::set(&[
        ("NOEVIA_MULTIMODAL_RAG", "1"),
        ("NOEVIA_MULTIMODAL_FIGURE_SWEEP_MAX", "2"),
        ("NOEVIA_MULTIMODAL_RAG_INTERVAL_MS", "0"),
        ("EMBEDDING_REINDEX_DUTY", "1"),
    ]);
    let (_tmp, store, _) = seed(
        &[
            file("a.pdf", 'a', "old"),
            file("b.pdf", 'b', "old"),
            file("c.pdf", 'c', "old"),
        ],
        true,
    );
    let (host, _server) = host(store).await;
    let engine = Arc::new(EngineStub(AtomicUsize::new(0)));
    let state = Mutex::new(Value::Null);
    worker(&host, &state, engine.clone()).figures().await;
    assert_eq!(
        engine.0.load(Ordering::SeqCst),
        2,
        "one successful refresh must not invalidate later documents"
    );
    assert_eq!(
        host.store.media_snapshot(USER, "p").unwrap().project["files"][2]["document"]["extractor"],
        "old",
        "the configured two-document budget remains enforced"
    );
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn upload_pass_uses_its_two_document_budget() {
    let extractor = extract::extractor_version(true);
    let (_tmp, store, _) = seed(
        &[
            file("a.pdf", 'a', &extractor),
            file("b.pdf", 'b', &extractor),
            file("c.pdf", 'c', &extractor),
        ],
        false,
    );
    let (host, _server) = host(store).await;
    let state = Mutex::new(Value::Null);
    let engine = Arc::new(EngineStub(AtomicUsize::new(0)));
    worker(&host, &state, engine.clone()).documents().await;
    let snap = host.store.media_snapshot(USER, "p").unwrap();
    let marked = snap.project["files"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|f| f["document"]["completionSweep"] == f["document"]["version"])
        .count();
    assert_eq!(
        marked, 2,
        "the first progress marker must not postpone the second upload"
    );
    assert_eq!(engine.0.load(Ordering::SeqCst), 0);
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tail_coverage_does_not_postpone_a_later_upload() {
    let extractor = extract::extractor_version(true);
    let mut first = file("a.pdf", 'a', &extractor);
    let full = "A".repeat(210000);
    let head = format!("[Page 1]\n{}", &full[..200000]);
    first["content"] = json!(head);
    first["document"]["completionSweep"] = first["document"]["version"].clone();
    let (_tmp, store, dir) = seed(&[first, file("b.pdf", 'b', &extractor)], false);
    let docs = server_projects::lifecycle::documents_dir(&dir, "p");
    std::fs::write(docs.join(format!("{}.json","a".repeat(64))),json!({"pageTexts":[{"number":1,"status":"native","text":full},{"number":2,"status":"native","text":"later tail"}]}).to_string()).unwrap();
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
    store
        .index_source(USER, &claim, &Control::default())
        .unwrap()
        .unwrap();
    // Cache both tail windows before the pass: it must advance their coverage
    // and then complete the later upload without any HTTP timing dependency.
    let snapshot = store.media_snapshot(USER, "p").unwrap();
    let target = store.seed.active.clone();
    let record = read(&docs.join(format!("{}.json", "a".repeat(64)))).unwrap();
    for from in [1, 2] {
        let window = media_policy::tail_window(
            &head,
            &record,
            &json!({"fromPage":from,"maxChars":3600}),
            crate::outbox::chunks,
        );
        let (rows, _) = store
            .prepare_tail(
                &snapshot,
                "a.pdf",
                &head,
                &window,
                &target,
                64,
                &Control::default(),
            )
            .unwrap();
        store
            .publish_media_rows(
                &snapshot,
                &target,
                &rows,
                &vec![vec![1.0, 0.0]; rows.len()],
                0,
                &Control::default(),
            )
            .unwrap();
    }

    let (host, _server) = host(store).await;
    let state = Mutex::new(Value::Null);
    worker(&host, &state, Arc::new(EngineStub(AtomicUsize::new(0))))
        .documents()
        .await;
    let snap = host.store.media_snapshot(USER, "p").unwrap();
    assert_eq!(
        snap.project["files"][0]["document"]["searchable"]["version"],
        "a".repeat(64),
        "earlier tail really advanced"
    );
    assert_eq!(
        snap.project["files"][0]["document"]["searchable"]["complete"], true,
        "cached tail coverage really completed"
    );
    assert_eq!(
        snap.project["files"][1]["document"]["completionSweep"],
        "b".repeat(64)
    );
}
#[tokio::test]
async fn resident_tail_record_excludes_a_second_large_operation() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("record.json");
    let lock = tmp.path().join(extract::LARGE_LOCK_FILE);
    std::fs::write(
        &path,
        json!({"pageTexts":[{"number":1,"text":"x".repeat(26*1024*1024)}]}).to_string(),
    )
    .unwrap();
    let competing_lock = lock.clone();
    with_tail_record(path, extract::LARGE_BYTES + 1, Some(lock), Control::default(), move |record| {
        assert!(record["pageTexts"][0]["text"].as_str().unwrap().len() > extract::LARGE_BYTES);
        // A second process uses this same nonblocking BEGIN IMMEDIATE protocol.
        // No elapsed-time assertion: the retained record must still own the lock.
        let contender = rusqlite::Connection::open(competing_lock).unwrap();
        contender.busy_timeout(Duration::ZERO).unwrap();
        let result = contender.execute_batch("BEGIN IMMEDIATE");
        assert!(matches!(result, Err(rusqlite::Error::SqliteFailure(error, _)) if matches!(error.code, rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked)), "a second large operation overlapped a resident tail record");
        Ok(())
    }).await.unwrap().unwrap();
}
#[tokio::test]
async fn cancellation_while_waiting_for_a_large_record_keeps_the_runtime_responsive_and_releases_it(
) {
    use std::future::Future as _;
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("record.json");
    std::fs::write(&path, "{}").unwrap();
    let (held_tx, held_rx) = tokio::sync::oneshot::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let foreground = std::thread::spawn(move || {
        let _hold = extract::large_lock(extract::LARGE_BYTES + 1, None).unwrap();
        held_tx.send(()).unwrap();
        release_rx.recv().unwrap();
    });
    held_rx.await.unwrap();
    let invoked = Arc::new(AtomicBool::new(false));
    let observed = invoked.clone();
    {
        let pending = with_tail_record(
            path,
            extract::LARGE_BYTES + 1,
            None,
            Control::default(),
            move |_| {
                observed.store(true, Ordering::SeqCst);
                Ok(())
            },
        );
        tokio::pin!(pending);
        // Poll once while the foreground operation holds the permit. A blocking
        // acquire here would stop this runtime before it could cancel the wait.
        std::future::poll_fn(|cx| {
            assert!(pending.as_mut().poll(cx).is_pending());
            std::task::Poll::Ready(())
        })
        .await;
    }
    release_tx.send(()).unwrap();
    tokio::task::spawn_blocking(move || foreground.join().unwrap())
        .await
        .unwrap();
    tokio::task::spawn_blocking(|| {
        let _hold = extract::large_lock(extract::LARGE_BYTES + 1, None).unwrap();
    })
    .await
    .unwrap();
    assert!(
        !invoked.load(Ordering::SeqCst),
        "a cancelled waiter must never read/process the record"
    );
}
#[test]
fn changed_or_deleted_document_never_reaches_an_engine() {
    for deleted in [false, true] {
        let old = file("a.pdf", 'a', "old");
        let (_tmp, store, dir) = seed(std::slice::from_ref(&old), true);
        let snapshot = store.media_snapshot(USER, "p").unwrap();
        let mut changed = old.clone();
        changed["content"] = json!("new source");
        std::fs::write(
            dir.join("projects.json"),
            json!({"projects":[{"id":"p","files":if deleted{vec![]}else{vec![changed]}}]})
                .to_string(),
        )
        .unwrap();
        let engine = EngineStub(AtomicUsize::new(0));
        assert!(store
            .document_step(
                &snapshot,
                &old,
                true,
                &engine,
                &Limits::from_env(),
                &Control::default()
            )
            .is_err());
        assert_eq!(engine.0.load(Ordering::SeqCst), 0);
    }
}
