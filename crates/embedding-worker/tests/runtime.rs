#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
mod support;
use embedding_worker::{fill::Store, http::Http, runtime, Index, Options};
use std::sync::{
    atomic::{AtomicBool, AtomicUsize, Ordering},
    Arc,
};
use support::*;
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn regression_p2_transient_batch_proof_failure_resumes_without_restart() {
    let (dir, store) = seed();
    let service_ready = Arc::new(AtomicBool::new(false));
    let sr = service_ready.clone();
    let once = Arc::new(AtomicBool::new(true));
    let fail = Arc::new(AtomicBool::new(false));
    let embeds = Arc::new(AtomicUsize::new(0));
    let (o, f, e) = (once.clone(), fail.clone(), embeds.clone());
    let stub = server(move |path, _| {
        if path == "/api/v1/embedding-service" {
            sr.store(true, Ordering::SeqCst);
        }
        if path == "/v1/models" && o.swap(false, Ordering::SeqCst) {
            f.store(true, Ordering::SeqCst);
        }
        if path == "/internal/embedding-worker-owner" && f.swap(false, Ordering::SeqCst) {
            return (404, "{}".into());
        }
        if path == "/v1/embeddings" {
            e.fetch_add(1, Ordering::SeqCst);
        }
        default_reply(path)
    })
    .await;
    let verify = store.clone();
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
    wait_until(|| service_ready.load(Ordering::SeqCst)).await;
    signal(dir.path(), 1);
    wait_until(|| {
        std::fs::read(dir.path().join("embedding-worker-status.json"))
            .ok()
            .and_then(|b| serde_json::from_slice::<serde_json::Value>(&b).ok())
            .is_some_and(|v| {
                v.get("ownershipUnconfirmedSince")
                    .and_then(serde_json::Value::as_u64)
                    .is_some()
            })
    })
    .await;
    wait_until(|| {
        verify
            .remaining(
                &Index {
                    user: None,
                    project: "p".into(),
                },
                &target("next"),
            )
            .ok()
            == Some(0)
    })
    .await;
    wait_until(|| {
        std::fs::read(dir.path().join("embedding-worker-status.json"))
            .ok()
            .and_then(|b| serde_json::from_slice::<serde_json::Value>(&b).ok())
            .is_some_and(|v| {
                v.get("ownershipStatus").and_then(serde_json::Value::as_str) == Some("confirmed")
            })
    })
    .await;
    assert!(embeds.load(Ordering::SeqCst) >= 2);
    worker.shutdown().await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn regression_p3_revoked_proof_after_embedding_prevents_publication() {
    let (dir, store) = seed();
    let verify = store.clone();
    let revoked = Arc::new(AtomicBool::new(false));
    let r = revoked.clone();
    let stub = server(move |path, _| {
        if path == "/v1/embeddings" {
            r.store(true, Ordering::SeqCst);
        }
        if path == "/internal/embedding-worker-owner" && r.load(Ordering::SeqCst) {
            return (404, "{}".into());
        }
        default_reply(path)
    })
    .await;
    signal(dir.path(), 1);
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
    wait_until(|| {
        std::fs::read(dir.path().join("embedding-worker-status.json"))
            .ok()
            .and_then(|b| serde_json::from_slice::<serde_json::Value>(&b).ok())
            .is_some_and(|v| v.get("lastPass").is_some_and(|p| !p.is_null()))
    })
    .await;
    assert!(revoked.load(Ordering::SeqCst));
    assert_eq!(
        verify
            .remaining(
                &Index {
                    user: None,
                    project: "p".into()
                },
                &target("active")
            )
            .unwrap(),
        1
    );
    worker.shutdown().await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn regression_p3_proof_is_rechecked_between_provider_fallback_singles() {
    let (dir, store) = seed();
    let verify = store.clone();
    let db = rusqlite::Connection::open(dir.path().join("rag/p.db")).unwrap();
    db.execute("INSERT INTO chunks VALUES(2,'second.txt',0,'second synthetic text','hash2',NULL,NULL,0,'text',NULL)",[]).unwrap();
    drop(db);
    let revoked = Arc::new(AtomicBool::new(false));
    let calls = Arc::new(AtomicUsize::new(0));
    let (r, c) = (revoked.clone(), calls.clone());
    let stub = server(move |path, body| {
        if path == "/internal/embedding-worker-owner" && r.load(Ordering::SeqCst) {
            return (404, "{}".into());
        }
        if path == "/v1/embeddings" {
            c.fetch_add(1, Ordering::SeqCst);
            let v: serde_json::Value = serde_json::from_str(body).unwrap();
            if v.get("input")
                .and_then(serde_json::Value::as_array)
                .is_some_and(|a| a.len() > 1)
            {
                return (200, "{\"data\":[]}".into());
            }
            r.store(true, Ordering::SeqCst);
        }
        default_reply(path)
    })
    .await;
    signal(dir.path(), 1);
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
    wait_until(|| {
        std::fs::read(dir.path().join("embedding-worker-status.json"))
            .ok()
            .and_then(|b| serde_json::from_slice::<serde_json::Value>(&b).ok())
            .is_some_and(|v| v.get("lastPass").is_some_and(|p| !p.is_null()))
    })
    .await;
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    assert_eq!(
        verify
            .remaining(
                &Index {
                    user: None,
                    project: "p".into()
                },
                &target("active")
            )
            .unwrap(),
        2
    );
    worker.shutdown().await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn runtime_start_migrates_real_temp_vec0_without_model_loader() {
    let (dir, store) = seed();
    let verify = store.clone();
    let loader_requests = Arc::new(AtomicUsize::new(0));
    let loader = loader_requests.clone();
    let stub = server(move |path, _| {
        if path == "/api/v1/embedding-service" {
            loader.fetch_add(1, Ordering::SeqCst);
        }
        default_reply(path)
    })
    .await;
    signal(dir.path(), 1);
    let http = Http::new("", None, &stub.base, None).unwrap();
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
    wait_until(|| {
        verify
            .remaining(
                &Index {
                    user: None,
                    project: "p".into(),
                },
                &target("next"),
            )
            .ok()
            == Some(0)
    })
    .await;
    assert_eq!(loader_requests.load(Ordering::SeqCst), 0);
    worker.shutdown().await;
}

#[cfg(target_os = "linux")]
#[allow(dead_code)]
#[path = "../../../tools/qualification/process.rs"]
mod process;
#[cfg(target_os = "linux")]
#[test]
fn owned_migration_process_child() {
    if std::env::var("NOEVIA_EMBED_FIXTURE_CHILD").as_deref() != Ok("yes") {
        return;
    }
    let root = std::path::PathBuf::from(std::env::var_os("NOEVIA_EMBED_FIXTURE_ROOT").unwrap());
    let base = std::env::var("NOEVIA_EMBED_FIXTURE_HTTP").unwrap();
    let db = rusqlite::Connection::open_with_flags(
        root.join("cowork.db"),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .unwrap();
    let count: i64 = db
        .query_row("SELECT COUNT(*) FROM users", [], |r| r.get(0))
        .unwrap();
    assert_eq!(count, 0, "child fixture must have no accounts");
    drop(db);
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap()
        .block_on(async move {
            let store = embedding_worker::sqlite::SqlStore {
                root,
                rag_dir: None,
                seed: embedding_worker::Config {
                    version: 0,
                    active: target("active"),
                    next: Some(target("next")),
                    previous: None,
                },
                open_vec: embedding_worker::sqlite::open_vec_required,
            };
            let http = Http::new(&base, None, &base, None).unwrap();
            let _owner = runtime::start(
                store,
                http,
                base,
                "synthetic-token".into(),
                START.into(),
                Options::default(),
                None,
            )
            .unwrap();
            std::future::pending::<()>().await;
        });
}
#[cfg(target_os = "linux")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn kill_between_embedding_and_publication_leaves_no_vectors_and_recovers() {
    let (dir, store) = seed();
    let service_ready = Arc::new(AtomicBool::new(false));
    let sr = service_ready.clone();
    let embedded = Arc::new(AtomicBool::new(false));
    let blocked = Arc::new(AtomicBool::new(false));
    let release = Arc::new(AtomicBool::new(false));
    let (e, b, r) = (embedded.clone(), blocked.clone(), release.clone());
    let stub = server(move |path, _| {
        if path == "/api/v1/embedding-service" {
            sr.store(true, Ordering::SeqCst);
        }
        if path == "/v1/embeddings" {
            e.store(true, Ordering::SeqCst);
        }
        if path == "/internal/embedding-worker-owner"
            && e.load(Ordering::SeqCst)
            && !r.load(Ordering::SeqCst)
        {
            b.store(true, Ordering::SeqCst);
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
            while !r.load(Ordering::SeqCst) && std::time::Instant::now() < deadline {
                std::thread::sleep(std::time::Duration::from_millis(2));
            }
        }
        default_reply(path)
    })
    .await;
    let mut child = std::process::Command::new(std::env::current_exe().unwrap());
    child
        .args(["--exact", "owned_migration_process_child", "--nocapture"])
        .env("NOEVIA_EMBED_FIXTURE_CHILD", "yes")
        .env("NOEVIA_EMBED_FIXTURE_ROOT", dir.path())
        .env("NOEVIA_EMBED_FIXTURE_HTTP", &stub.base);
    let (send, recv) = std::sync::mpsc::channel();
    let runner = std::thread::spawn(move || {
        process::bounded_observed(
            &mut child,
            vec![],
            std::time::Duration::from_secs(18),
            |pid| {
                send.send(pid).unwrap();
            },
            |_| {},
        )
        .unwrap()
    });
    let pid = recv
        .recv_timeout(std::time::Duration::from_secs(2))
        .unwrap();
    wait_until(|| service_ready.load(Ordering::SeqCst)).await;
    signal(dir.path(), 1);
    wait_until(|| blocked.load(Ordering::SeqCst)).await;
    let owned = rustix::process::Pid::from_raw(i32::try_from(pid).unwrap()).unwrap();
    rustix::process::kill_process(owned, rustix::process::Signal::KILL).unwrap();
    release.store(true, Ordering::SeqCst);
    let output = runner.join().unwrap();
    assert!(!output.status.success());
    let index = Index {
        user: None,
        project: "p".into(),
    };
    assert_eq!(
        store.remaining(&index, &target("active")).unwrap(),
        1,
        "no vector may survive the kill-before-publication boundary"
    );
    let verify = store.clone();
    let http = Http::new("", None, &stub.base, None).unwrap();
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
    wait_until(|| verify.remaining(&index, &target("next")).ok() == Some(0)).await;
    worker.shutdown().await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn regression_p1_lock_contender_recovers_when_draining_owner_releases() {
    let (dir, store) = seed();
    let verify = store.clone();
    let lock = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(dir.path().join("embedding-worker.lock"))
        .unwrap();
    lock.try_lock().unwrap();
    let requests = Arc::new(AtomicUsize::new(0));
    let seen = requests.clone();
    let stub = server(move |path, _| {
        seen.fetch_add(1, Ordering::SeqCst);
        default_reply(path)
    })
    .await;
    signal(dir.path(), 1);
    let http = Http::new(&stub.base, None, &stub.base, None).unwrap();
    let owner = runtime::start(
        store,
        http,
        stub.base.clone(),
        "synthetic-token".into(),
        START.into(),
        Options::default(),
        None,
    )
    .expect("lock contention must leave an idle retrying owner");
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    assert_eq!(requests.load(Ordering::SeqCst), 0);
    drop(lock);
    wait_until(|| {
        verify
            .remaining(
                &Index {
                    user: None,
                    project: "p".into(),
                },
                &target("next"),
            )
            .ok()
            == Some(0)
    })
    .await;
    owner.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn heartbeat_stays_fresh_during_thirty_second_pass_without_loader() {
    let (dir, store) = seed();
    let embedding = Arc::new(AtomicBool::new(false));
    let observed = embedding.clone();
    let stub = server(move |path, _| {
        if path == "/v1/embeddings" {
            observed.store(true, Ordering::Release);
            std::thread::sleep(std::time::Duration::from_secs(30));
        }
        default_reply(path)
    })
    .await;
    let http = Http::new("", None, &stub.base, None).unwrap();
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
    let snapshot = || -> serde_json::Value {
        std::fs::read(dir.path().join("embedding-worker-status.json"))
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default()
    };
    wait_until(|| snapshot()["configValid"] == true).await;
    signal(dir.path(), 1);
    wait_until(|| embedding.load(Ordering::Acquire)).await;
    let mut previous = snapshot()["at"].as_u64().unwrap();
    for _ in 0..7 {
        tokio::time::sleep(std::time::Duration::from_secs(4)).await;
        let status = snapshot();
        let at = status["at"].as_u64().unwrap();
        assert!(at > previous, "heartbeat stalled during embedding");
        assert!(status["lastPass"].is_null());
        assert!(status["service"].is_null());
        previous = at;
    }
    worker.shutdown().await;
}

#[tokio::test]
async fn cancellation_sleeps_until_notified_and_wakes_all_waiters() {
    use std::{future::Future, task::Poll};
    let control = embedding_worker::Control::default();
    let polls = Arc::new(AtomicUsize::new(0));
    let mut tasks = vec![];
    for _ in 0..2 {
        let control = control.clone();
        let polls = polls.clone();
        tasks.push(tokio::spawn(async move {
            let future = control.cancelled();
            tokio::pin!(future);
            std::future::poll_fn(|cx| {
                polls.fetch_add(1, Ordering::SeqCst);
                future.as_mut().poll(cx)
            })
            .await;
        }));
    }
    tokio::task::yield_now().await;
    tokio::time::sleep(std::time::Duration::from_millis(60)).await;
    assert_eq!(
        polls.load(Ordering::SeqCst),
        2,
        "cancellation polled without a notification"
    );
    control.suspend();
    for task in tasks {
        tokio::time::timeout(std::time::Duration::from_millis(100), task)
            .await
            .unwrap()
            .unwrap();
    }
    control.resume();
    control.stop();
    assert!(
        std::future::poll_fn(|cx| {
            let f = control.cancelled();
            tokio::pin!(f);
            Poll::Ready(f.poll(cx).is_ready())
        })
        .await
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn index_outbox_yields_to_slow_foreground_without_spending_attempts() {
    let (_tmp, mut store) = seed();
    store.seed.next = None;
    let user = "11111111-1111-4111-8111-111111111111";
    let auth = rusqlite::Connection::open(store.root.join("cowork.db")).unwrap();
    auth.execute("INSERT INTO users(id,username,username_norm,display_name,role,password_hash,webauthn_user_id,created_at,updated_at) VALUES(?,'outbox','outbox','Synthetic','member','synthetic','outbox',0,0)",[user]).unwrap();
    let dir = store.root.join("users").join(user);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("projects.json"),
        r#"{"projects":[{"id":"p","files":[{"name":"a.md","content":"synthetic source"}]}]}"#,
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
    let foreground = Arc::new(AtomicBool::new(true));
    let busy = foreground.clone();
    let proofs = Arc::new(AtomicUsize::new(0));
    let seen = proofs.clone();
    let stub = server(move |path, _| {
        if path == "/internal/embedding-worker-owner" {
            seen.fetch_add(1, Ordering::SeqCst);
            let mut proof: serde_json::Value = serde_json::from_str(PROOF).unwrap();
            proof["indexOutboxStopped"] = true.into();
            proof["foregroundBusy"] = busy.load(Ordering::SeqCst).into();
            return (200, proof.to_string());
        }
        default_reply(path)
    })
    .await;
    let http = Http::new("", None, &stub.base, None).unwrap();
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
    wait_until(|| proofs.load(Ordering::SeqCst) > 0).await;
    tokio::time::sleep(std::time::Duration::from_millis(2200)).await;
    assert_eq!(
        server_projects::outbox::jobs(&dir).unwrap()[0].state,
        "pending"
    );
    let db = rusqlite::Connection::open(dir.join("index-outbox.db")).unwrap();
    assert_eq!(
        db.query_row("SELECT attempts FROM jobs", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        0
    );
    foreground.store(false, Ordering::SeqCst);
    wait_until(|| server_projects::outbox::jobs(&dir).unwrap().is_empty()).await;
    worker.shutdown().await;
    let db = rusqlite::Connection::open(dir.join("rag/p.db")).unwrap();
    assert_eq!(
        db.query_row("SELECT body FROM chunks", [], |r| r.get::<_, String>(0))
            .unwrap(),
        "synthetic source"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn native_config_publication_reaches_background_without_node_kick() {
    let (dir, mut store) = seed();
    store.seed.next = None;
    let verify = store.clone();
    let stub = server(|path, _| default_reply(path)).await;
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
    wait_until(|| {
        std::fs::read(dir.path().join("embedding-worker-status.json"))
            .ok()
            .and_then(|b| serde_json::from_slice::<serde_json::Value>(&b).ok())
            .is_some_and(|v| v.get("configValid") == Some(&serde_json::Value::Bool(true)))
    })
    .await;
    let before = verify.config().unwrap();
    embedding_worker::config::publish(
        dir.path(),
        &verify.seed,
        &before,
        embedding_worker::config::Change::SetNext(target("next")),
    )
    .unwrap();
    wait_until(|| {
        verify
            .remaining(
                &Index {
                    user: None,
                    project: "p".into(),
                },
                &target("next"),
            )
            .ok()
            == Some(0)
    })
    .await;
    assert_eq!(verify.config().unwrap().next, Some(target("next")));
    worker.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn active_only_publication_kicks_backfill_without_node() {
    let (dir, mut store) = seed();
    store.seed.next = None;
    let verify = store.clone();
    let stub = server(|path, _| default_reply(path)).await;
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
    wait_until(|| {
        std::fs::read(dir.path().join("embedding-worker-status.json"))
            .ok()
            .and_then(|b| serde_json::from_slice::<serde_json::Value>(&b).ok())
            .is_some_and(|v| v.get("configValid") == Some(&serde_json::Value::Bool(true)))
    })
    .await;
    let before = verify.config().unwrap();
    embedding_worker::config::publish(
        dir.path(),
        &verify.seed,
        &before,
        embedding_worker::config::Change::SetNext(target("next")),
    )
    .unwrap();
    let next = verify.config().unwrap();
    embedding_worker::config::publish(
        dir.path(),
        &verify.seed,
        &next,
        embedding_worker::config::Change::Switch,
    )
    .unwrap();
    wait_until(|| {
        verify
            .remaining(
                &Index {
                    user: None,
                    project: "p".into(),
                },
                &target("next"),
            )
            .ok()
            == Some(0)
    })
    .await;
    assert_eq!(verify.config().unwrap().active, target("next"));
    worker.shutdown().await;
}
