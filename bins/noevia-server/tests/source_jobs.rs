//! sbstndalton/noevia#2191 round 6 `rust-source-jobs`: the front is the one owner of the source-job
//! store. Through the whole front, against a synthetic Node worker (an HTTP server in this test that
//! answers the synchronous route's `X-Noevia-Source-Work` NDJSON): a background upload is started
//! here, its operation sent to the worker with the client's session and body, its progress and
//! result written to `jobs/<uuid>.jsonl` and read back by the poll in core's words; a refusal before
//! `accepted` is passed on with no job left active; a worker gone before its result fails the job
//! with the restart wording; the two-active cap; the internal store routes Node used are gone.
//! Synthetic data only.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]

use noevia_server::config::Config;
use noevia_server::serve::{serve, Limits};
use server_store::rusqlite::Connection;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

const SCHEMA: &str = include_str!("../../../crates/server-store/tests/fixtures/node-schema.sql");
const CODE: &str = "synthetic-source-jobs-code";
const ORIGIN: &str = "http://127.0.0.1:18021";
const START: &str = "0123456789abcdef0123456789abcdef";

/// What the synthetic worker saw: (request line, header lines, body).
type Seen = Arc<Mutex<Vec<(String, Vec<String>, String)>>>;

/// The synthetic Node worker. The body's `mode` picks its answer.
async fn worker() -> (u16, Seen) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let seen: Seen = Arc::default();
    let log = Arc::clone(&seen);
    tokio::spawn(async move {
        loop {
            let Ok((mut s, _)) = listener.accept().await else {
                return;
            };
            let log = Arc::clone(&log);
            tokio::spawn(async move {
                let mut buf = Vec::new();
                let mut chunk = [0u8; 65536];
                let (head, body) = loop {
                    let n = s.read(&mut chunk).await.unwrap();
                    if n == 0 {
                        return;
                    }
                    buf.extend_from_slice(&chunk[..n]);
                    if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                        let head = String::from_utf8_lossy(&buf[..i]).into_owned();
                        let len: usize = head
                            .lines()
                            .find_map(|l| {
                                l.to_ascii_lowercase()
                                    .strip_prefix("content-length: ")
                                    .map(|v| v.trim().parse().unwrap())
                            })
                            .unwrap_or(0);
                        while buf.len() < i + 4 + len {
                            let n = s.read(&mut chunk).await.unwrap();
                            buf.extend_from_slice(&chunk[..n]);
                        }
                        break (
                            head,
                            String::from_utf8_lossy(&buf[i + 4..i + 4 + len]).into_owned(),
                        );
                    }
                };
                let mut lines = head.lines();
                let line = lines.next().unwrap().to_string();
                let headers: Vec<String> = lines.map(str::to_owned).collect();
                log.lock().unwrap().push((line, headers, body.clone()));
                let mode = serde_json::from_str::<serde_json::Value>(&body)
                    .ok()
                    .and_then(|v| v["mode"].as_str().map(str::to_owned))
                    .unwrap_or_default();
                let stream = "HTTP/1.1 200 OK\r\ncontent-type: application/x-ndjson\r\ncache-control: no-store\r\nconnection: close\r\n\r\n";
                match mode.as_str() {
                    "refuse" => {
                        let b =
                            r#"{"error":"Storage login rejected.","code":"storageLoginRejected"}"#;
                        let r = format!("HTTP/1.1 502 Bad Gateway\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{b}", b.len());
                        s.write_all(r.as_bytes()).await.unwrap();
                    }
                    "die" => {
                        s.write_all(
                            format!("{stream}{{\"accepted\":true}}\n{{\"stage\":\"Reading\"}}\n")
                                .as_bytes(),
                        )
                        .await
                        .unwrap();
                    }
                    "hold" => {
                        s.write_all(format!("{stream}{{\"accepted\":true}}\n").as_bytes())
                            .await
                            .unwrap();
                        tokio::time::sleep(Duration::from_secs(30)).await;
                    }
                    "fail" => {
                        s.write_all(format!("{stream}{{\"accepted\":true}}\n{{\"failed\":{{\"status\":422}}}}\n").as_bytes()).await.unwrap();
                    }
                    _ => {
                        s.write_all(format!("{stream}{{\"accepted\":true}}\n").as_bytes())
                            .await
                            .unwrap();
                        tokio::time::sleep(Duration::from_millis(50)).await;
                        s.write_all(b"{\"stage\":\"Creating upload folder\"}\n{\"stage\":\"Indexing extracted text\"}\n").await.unwrap();
                        tokio::time::sleep(Duration::from_millis(50)).await;
                        s.write_all("{\"done\":{\"status\":200,\"body\":{\"name\":\"notes.txt\",\"path\":\"Text/notes.txt\",\"bytes\":3,\"note\":\"\\ud800 caf\u{e9}\"}}}\n".as_bytes()).await.unwrap();
                    }
                }
                let _ = s.shutdown().await;
            });
        }
    });
    (port, seen)
}

struct Front {
    _data: tempfile::TempDir,
    data: PathBuf,
    addr: SocketAddr,
    jar: String,
    csrf: String,
    user: String,
}

async fn call(
    addr: SocketAddr,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
    body: &str,
) -> (u16, String) {
    let mut s = TcpStream::connect(addr).await.unwrap();
    let mut req = format!(
        "{method} {path} HTTP/1.1\r\nhost: front.test\r\nconnection: close\r\ncontent-length: {}\r\n",
        body.len()
    );
    for (k, v) in headers {
        req.push_str(&format!("{k}: {v}\r\n"));
    }
    req.push_str("\r\n");
    req.push_str(body);
    s.write_all(req.as_bytes()).await.unwrap();
    let mut out = Vec::new();
    s.read_to_end(&mut out).await.unwrap();
    let text = String::from_utf8_lossy(&out).into_owned();
    let status = text[9..12].parse().unwrap();
    let (_, body) = text.split_once("\r\n\r\n").unwrap();
    (status, body.to_string())
}

async fn front(node: u16) -> Front {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().to_path_buf();
    let c = Connection::open(server_store::db_path(&data)).unwrap();
    c.execute_batch(SCHEMA).unwrap();
    c.execute(
        "INSERT INTO settings VALUES('setup_code_hash', ?1)",
        [server_auth::js::digest(CODE)],
    )
    .unwrap();
    let mut env: HashMap<&str, String> = HashMap::new();
    env.insert("NOEVIA_LEGACY_UPSTREAM", format!("http://127.0.0.1:{node}"));
    env.insert("UI_PORT", "1".into());
    env.insert("UI_DATA_DIR", data.display().to_string());
    env.insert("NOEVIA_WEB_DIST", data.join("dist").display().to_string());
    env.insert("PUBLIC_ORIGIN", ORIGIN.into());
    env.insert("NOEVIA_START_ID", START.into());
    let app = noevia_server::App::new(Config::from_lookup(|k| env.get(k).cloned()).unwrap());
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(serve(
        listener,
        app,
        Limits::default(),
        std::future::pending(),
    ));
    let setup = format!(
        r#"{{"setupCode":"{CODE}","username":"owner","password":"synthetic source-jobs password","diaryEnabled":false}}"#
    );
    let mut s = TcpStream::connect(addr).await.unwrap();
    let req = format!(
        "POST /api/setup/complete HTTP/1.1\r\nhost: front.test\r\nconnection: close\r\norigin: {ORIGIN}\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n{setup}",
        setup.len()
    );
    s.write_all(req.as_bytes()).await.unwrap();
    let mut out = Vec::new();
    s.read_to_end(&mut out).await.unwrap();
    let text = String::from_utf8(out).unwrap();
    assert!(text.starts_with("HTTP/1.1 201"), "{text}");
    let cookies: Vec<String> = text
        .lines()
        .filter_map(|l| {
            l.strip_prefix("set-cookie: ")
                .map(|v| v.split(';').next().unwrap().to_string())
        })
        .collect();
    let csrf = cookies[1].split_once('=').unwrap().1.to_string();
    let jar = cookies.join("; ");
    let user: String = c
        .query_row("SELECT id FROM users", [], |r| r.get(0))
        .unwrap();
    let udir = data.join("users").join(&user);
    std::fs::create_dir_all(&udir).unwrap();
    std::fs::write(
        udir.join("projects.json"),
        r#"{"projects":[{"id":"p 1","name":"Synthetic","files":[],"chats":[]},{"id":"p2","name":"Other","files":[],"chats":[]}]}"#,
    )
    .unwrap();
    Front {
        _data: dir,
        data,
        addr,
        jar,
        csrf,
        user,
    }
}

fn headers(f: &Front) -> Vec<(&str, &str)> {
    vec![
        ("cookie", f.jar.as_str()),
        ("x-csrf-token", f.csrf.as_str()),
        ("origin", ORIGIN),
        ("content-type", "application/json"),
        ("x-noevia-source-work", "0"),
    ]
}

async fn poll_until_done(f: &Front, poll: &str) -> serde_json::Value {
    let get = [("cookie", f.jar.as_str())];
    for _ in 0..200 {
        let (status, body) = call(f.addr, "GET", poll, &get, "").await;
        assert_eq!(status, 200, "{body}");
        // JSON as JSON.stringify writes it (a lone surrogate stays escaped).
        let v = server_projects::source_jobs::parse_js(body.as_bytes()).unwrap();
        if v["done"] == true {
            return v;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("the job never finished");
}

fn journals(f: &Front) -> Vec<String> {
    let dir = f.data.join("users").join(&f.user).join("jobs");
    let mut out: Vec<String> = std::fs::read_dir(dir)
        .map(|d| {
            d.map(|e| std::fs::read_to_string(e.unwrap().path()).unwrap())
                .collect()
        })
        .unwrap_or_default();
    out.sort();
    out
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_background_upload_runs_as_the_fronts_own_job() {
    let (node, seen) = worker().await;
    let f = front(node).await;
    let h = headers(&f);
    // The project 404 and the gate come first; no job, no worker call.
    let (status, body) = call(
        f.addr,
        "POST",
        "/api/projects/ghost/upload?background=1",
        &h,
        "{}",
    )
    .await;
    assert_eq!(
        (status, body.as_str()),
        (404, r#"{"error":"project not found"}"#)
    );
    let (status, _) = call(
        f.addr,
        "POST",
        "/api/projects/p%201/upload?background=1",
        &[],
        "{}",
    )
    .await;
    assert_eq!(status, 401);
    assert!(seen.lock().unwrap().is_empty());

    // Accepted: 202 { poll }, the worker got the synchronous route with the session and body.
    let up = r#"{"name":"notes.txt","dataBase64":"YWJj","organized":true}"#;
    let (status, body) = call(
        f.addr,
        "POST",
        "/api/projects/p%201/upload?background=1",
        &h,
        up,
    )
    .await;
    assert_eq!(status, 202, "{body}");
    let poll = serde_json::from_str::<serde_json::Value>(&body).unwrap()["poll"]
        .as_str()
        .unwrap()
        .to_owned();
    assert!(
        poll.starts_with("/api/projects/p%201/source-jobs/"),
        "{poll}"
    );
    let id = poll.rsplit('/').next().unwrap().to_owned();
    {
        let seen = seen.lock().unwrap();
        let (line, hs, b) = &seen[0];
        assert_eq!(line, "POST /api/projects/p%201/upload HTTP/1.1");
        assert_eq!(b, up);
        assert!(hs
            .iter()
            .any(|l| l.eq_ignore_ascii_case(&format!("cookie: {}", f.jar))));
        assert!(hs
            .iter()
            .any(|l| l.eq_ignore_ascii_case(&format!("x-csrf-token: {}", f.csrf))));
        let work: Vec<_> = hs
            .iter()
            .filter(|l| l.to_ascii_lowercase().starts_with("x-noevia-source-work:"))
            .collect();
        assert_eq!(
            work,
            vec!["x-noevia-source-work: 1"],
            "the client's own copy is replaced"
        );
    }
    // Another project's poll path, and no session: not this job.
    let (status, _) = call(
        f.addr,
        "GET",
        &format!("/api/projects/p2/source-jobs/{id}"),
        &[("cookie", f.jar.as_str())],
        "",
    )
    .await;
    assert_eq!(status, 404);
    let (status, _) = call(f.addr, "GET", &poll, &[], "").await;
    assert_eq!(status, 401);
    let done = poll_until_done(&f, &poll).await;
    assert_eq!(done["status"], 200);
    assert_eq!(done["body"]["path"], "Text/notes.txt");
    // The result as the store has kept it since it became its writer (keys in the store's order).
    let (_, raw) = call(f.addr, "GET", &poll, &[("cookie", f.jar.as_str())], "").await;
    assert!(
        raw.starts_with(
            r#"{"done":true,"status":200,"body":{"bytes":3,"name":"notes.txt","note":"\ud800 caf"#
        ),
        "{raw}"
    );
    let journal = journals(&f).join("\n");
    assert!(
        journal.contains(r#""n":1,"stage":"Creating upload folder""#),
        "{journal}"
    );
    assert!(
        journal.contains(r#""n":2,"stage":"Indexing extracted text""#),
        "{journal}"
    );
    assert!(journal.contains(r#""type":"job.completed""#), "{journal}");
    assert!(
        journal.contains(r#"\ud800"#),
        "a lone surrogate is kept as JSON.stringify wrote it"
    );

    // A refusal before `accepted` is passed on as it is, and its job never stays active.
    let (status, body) = call(
        f.addr,
        "POST",
        "/api/projects/p%201/sources/sync?background=1",
        &h,
        r#"{"mode":"refuse"}"#,
    )
    .await;
    assert_eq!(
        (status, body.as_str()),
        (
            502,
            r#"{"error":"Storage login rejected.","code":"storageLoginRejected"}"#
        )
    );
    // A worker gone before its result, and a failed operation.
    let (status, body) = call(
        f.addr,
        "POST",
        "/api/projects/p%201/sources/sync?background=1",
        &h,
        r#"{"mode":"die"}"#,
    )
    .await;
    assert_eq!(status, 202);
    let died = serde_json::from_str::<serde_json::Value>(&body).unwrap()["poll"]
        .as_str()
        .unwrap()
        .to_owned();
    assert_eq!(
        poll_until_done(&f, &died).await,
        serde_json::json!({"done":true,"status":503,"body":{"error":"The server restarted before processing finished; refresh or re-upload to retry."}})
    );
    let (status, body) = call(
        f.addr,
        "POST",
        "/api/projects/p%201/upload?background=1",
        &h,
        r#"{"mode":"fail"}"#,
    )
    .await;
    assert_eq!(status, 202);
    let failed = serde_json::from_str::<serde_json::Value>(&body).unwrap()["poll"]
        .as_str()
        .unwrap()
        .to_owned();
    assert_eq!(
        poll_until_done(&f, &failed).await,
        serde_json::json!({"done":true,"status":422,"body":{"error":"Source processing failed; refresh or re-upload to retry."}})
    );

    // Two running jobs are the cap; the third start is core's 429 and reaches no worker.
    let before = seen.lock().unwrap().len();
    for _ in 0..2 {
        let (status, _) = call(
            f.addr,
            "POST",
            "/api/projects/p2/upload?background=1",
            &h,
            r#"{"mode":"hold"}"#,
        )
        .await;
        assert_eq!(status, 202);
    }
    let (status, body) = call(
        f.addr,
        "POST",
        "/api/projects/p2/upload?background=1",
        &h,
        r#"{"mode":"hold"}"#,
    )
    .await;
    assert_eq!(
        (status, body.as_str()),
        (
            429,
            r#"{"error":"Source processing is busy; wait for the current operation and retry."}"#
        )
    );
    assert_eq!(seen.lock().unwrap().len(), before + 2);
    // A running job's poll.
    let (_, raw) = call(
        f.addr,
        "GET",
        &poll.replace(&id, "11111111-1111-4111-8111-111111111111"),
        &[("cookie", f.jar.as_str())],
        "",
    )
    .await;
    assert_eq!(
        raw,
        r#"{"error":"Processing status expired or the server restarted; refresh or re-upload to retry."}"#
    );

    // The synchronous form is Node's: proxied with the client's worker header dropped.
    let (status, _) = call(
        f.addr,
        "POST",
        "/api/projects/p%201/upload",
        &h,
        r#"{"mode":"refuse"}"#,
    )
    .await;
    assert_eq!(status, 502);
    let seen = seen.lock().unwrap();
    let (line, hs, _) = seen.last().unwrap();
    assert_eq!(line, "POST /api/projects/p%201/upload HTTP/1.1");
    assert!(!hs
        .iter()
        .any(|l| l.to_ascii_lowercase().starts_with("x-noevia-source-work:")));
}

#[tokio::test]
async fn the_internal_source_store_routes_are_gone() {
    use axum::{body::Body, http::Request};
    use tower::ServiceExt;
    let root = tempfile::tempdir().unwrap();
    let app = noevia_server::project_store::router(noevia_server::project_store::Store {
        data_dir: root.path().into(),
        token: "synthetic-source-store-token-0000000".into(),
        switch: server_store::RustProjects::owned(),
        start_id: START.into(),
    });
    for op in ["recover", "start", "abandon", "progress", "finish", "read"] {
        let res = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(format!("/v1/source-jobs/{op}"))
                    .header(
                        noevia_server::project_store::TOKEN_HEADER,
                        "synthetic-source-store-token-0000000",
                    )
                    .body(Body::from("{}"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), 404, "{op}");
    }
    assert!(!noevia_server::FEATURES.contains(&"noevia-source-jobs-store/1"));
    assert!(noevia_server::FEATURES.contains(&"rust-source-jobs"));
}
