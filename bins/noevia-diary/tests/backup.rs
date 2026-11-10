//! Aborting the actual handler future must leave remote work covered until settlement.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]
use axum::{
    body::Body,
    http::{Request, Response},
};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn abort_hands_locks_to_settle_task_and_fence_waits() {
    let dir = tempfile::tempdir().unwrap();
    let uid = "12345678-1234-4234-8234-123456789abc";
    let root = dir.path().join("users").join(uid);
    std::fs::create_dir_all(&root).unwrap();
    let db = rusqlite::Connection::open(root.join("managed-diary.db")).unwrap();
    db.execute_batch("CREATE TABLE meta(key TEXT PRIMARY KEY,value TEXT); CREATE TABLE files(path TEXT,data BLOB,version TEXT); CREATE TABLE directories(path TEXT); CREATE TABLE backups(destination TEXT PRIMARY KEY,generation INTEGER,last_success REAL,retry_at REAL,failures INTEGER,error TEXT);").unwrap();
    for (k, v) in [
        ("tenant", uid),
        ("active", "1"),
        ("generation", "1"),
        ("settings", "{}"),
        ("due", "0"),
    ] {
        db.execute("INSERT INTO meta VALUES (?,?)", [k, v]).unwrap();
    }
    let bytes = b"synthetic";
    use sha2::Digest;
    let hash = format!("{:x}", sha2::Sha256::digest(bytes));
    db.execute(
        "INSERT INTO files VALUES ('a.md',?,?)",
        rusqlite::params![bytes, hash],
    )
    .unwrap();
    drop(db);
    let objects = Arc::new(Mutex::new(HashMap::<String, Vec<u8>>::new()));
    let entered = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let (o, e, r) = (objects.clone(), entered.clone(), release.clone());
    let dav = axum::Router::new().fallback(move |req: Request<Body>| {
        let (objects, entered, release) = (o.clone(), e.clone(), r.clone());
        async move {
            let path = req.uri().path().to_string();
            let method = req.method().as_str().to_string();
            match method.as_str() {
                "GET" => {
                    let found = objects.lock().unwrap().get(&path).cloned();
                    Response::builder()
                        .status(if found.is_some() { 200 } else { 404 })
                        .body(Body::from(found.unwrap_or_default()))
                        .unwrap()
                }
                "MKCOL" => Response::builder().status(201).body(Body::empty()).unwrap(),
                "PUT" => {
                    let bytes = axum::body::to_bytes(req.into_body(), 100000).await.unwrap();
                    entered.notify_one();
                    release.notified().await;
                    objects.lock().unwrap().insert(path, bytes.to_vec());
                    Response::builder().status(201).body(Body::empty()).unwrap()
                }
                _ => unreachable!(),
            }
        }
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let dav_task = tokio::spawn(async move {
        axum::serve(listener, dav).await.unwrap();
    });
    let cfg = serde_json::json!({"port":0,"control_token":"ctl","forward_secret":"f","data_root":dir.path(),"corpus":{},"llm":{"main":{"base_url":"http://127.0.0.1:9","setting_key":"A","fallback_setting_key":"B"},"aux":{"base_url":"http://127.0.0.1:9","setting_key":"C","fallback_setting_key":"D"}}});
    let app = Arc::new(noevia_diary::server::App::new(
        noevia_diary::config::parse(&cfg.to_string()).unwrap(),
    ));
    // The ASCII descriptor avoids introducing a second base64 implementation.
    use std::io::Write;
    // encode base64url with the existing dependency through the tenant assertion utility is
    // unnecessary: a test HTTP header can use a tiny deterministic encoder.
    let storage = serde_json::json!({"kind":"webdav","baseUrl":format!("http://{addr}/dav"),"username":"u","secret":"s"}).to_string();
    let alphabet = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut encoded = Vec::new();
    let (mut bits, mut count) = (0u32, 0u32);
    for b in storage.bytes() {
        bits = (bits << 8) | u32::from(b);
        count += 8;
        while count >= 6 {
            count -= 6;
            encoded
                .write_all(&[alphabet[((bits >> count) & 63) as usize]])
                .unwrap();
        }
    }
    if count > 0 {
        encoded.push(alphabet[((bits << (6 - count)) & 63) as usize]);
    }
    let request = Request::post("/api/storage-backup")
        .header("x-noevia-diary-forward", "f")
        .header("x-noevia-diary-writer", "1")
        .header("x-cowork-user-id", uid)
        .header("x-cowork-storage", String::from_utf8(encoded).unwrap())
        .body(Body::empty())
        .unwrap();
    let task = tokio::spawn(noevia_diary::server::handle(app.clone(), request));
    tokio::time::timeout(Duration::from_secs(5), entered.notified())
        .await
        .unwrap();
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    let lock = std::fs::OpenOptions::new()
        .append(true)
        .open(root.join("managed-backup.lock"))
        .unwrap();
    assert!(matches!(
        lock.try_lock(),
        Err(std::fs::TryLockError::WouldBlock)
    ));
    let app2 = app.clone();
    let fence = tokio::task::spawn_blocking(move || app2.writers.fence(uid));
    assert!(!fence.is_finished());
    release.notify_one();
    // The manifest is another immutable PUT, covered by the same exclusion.
    tokio::time::timeout(Duration::from_secs(5), entered.notified())
        .await
        .unwrap();
    assert!(matches!(
        lock.try_lock(),
        Err(std::fs::TryLockError::WouldBlock)
    ));
    assert!(!fence.is_finished());
    release.notify_one();
    tokio::time::timeout(Duration::from_secs(5), fence)
        .await
        .unwrap()
        .unwrap();
    lock.try_lock().unwrap();
    assert_eq!(objects.lock().unwrap().len(), 2);
    dav_task.abort();
    let _ = dav_task.await;
}
