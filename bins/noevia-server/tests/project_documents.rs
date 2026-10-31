//! sbstndalton/noevia#2191 round 5 `rust-project-documents` / `rust-document-extraction`:
//! `POST /api/projects/{id}/documents` through the whole front, against synthetic Docling and OCR
//! sidecars (HTTP servers in this test), with Node absent. Docling down: the document is stored
//! failed + queued, its job waits in `extraction-jobs.db`; Docling back: a worker pass extracts
//! it, replaces the record, queues the index job and empties the queue; a job a crash left
//! `running` is pending again at the next start. Synthetic data only.
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
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

const SCHEMA: &str = include_str!("../../../crates/server-store/tests/fixtures/node-schema.sql");
const CODE: &str = "synthetic-project-documents-code";
const ORIGIN: &str = "http://127.0.0.1:18021";

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

async fn front() -> Front {
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
    env.insert("NOEVIA_LEGACY_UPSTREAM", "http://127.0.0.1:9".into());
    env.insert("UI_PORT", "1".into());
    env.insert("UI_DATA_DIR", data.display().to_string());
    env.insert("NOEVIA_WEB_DIST", data.join("dist").display().to_string());
    env.insert("PUBLIC_ORIGIN", ORIGIN.into());
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
        r#"{{"setupCode":"{CODE}","username":"owner","password":"synthetic project-documents password","diaryEnabled":false}}"#
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
    std::fs::create_dir_all(data.join("users").join(&user)).unwrap();
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
    ]
}

/// What the synthetic sidecars saw: (path, selected headers, body length).
type Seen = Arc<Mutex<Vec<(String, Vec<(String, String)>, usize)>>>;

/// A synthetic sidecar's answer to `/extract`, from the request's selected headers.
type Reply = fn(&[(String, String)]) -> (u16, String);

/// A synthetic sidecar: `/health` answers `health`, `/extract` answers `reply(headers)`.
async fn sidecar(health: &'static str, reply: Reply) -> (String, Seen) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
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
                let (head, mut body) = loop {
                    let n = s.read(&mut chunk).await.unwrap();
                    if n == 0 {
                        return;
                    }
                    buf.extend_from_slice(&chunk[..n]);
                    if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                        break (
                            String::from_utf8_lossy(&buf[..i]).to_string(),
                            buf[i + 4..].to_vec(),
                        );
                    }
                };
                let mut lines = head.lines();
                let path = lines.next().unwrap().split(' ').nth(1).unwrap().to_string();
                let hs: Vec<(String, String)> = lines
                    .filter_map(|l| l.split_once(": "))
                    .map(|(k, v)| (k.to_ascii_lowercase(), v.to_string()))
                    .collect();
                let len: usize = hs
                    .iter()
                    .find(|(k, _)| k == "content-length")
                    .map(|(_, v)| v.parse().unwrap())
                    .unwrap_or(0);
                while body.len() < len {
                    let n = s.read(&mut chunk).await.unwrap();
                    if n == 0 {
                        break;
                    }
                    body.extend_from_slice(&chunk[..n]);
                }
                let picked: Vec<(String, String)> = hs
                    .iter()
                    .filter(|(k, _)| k.starts_with("x-") || k == "content-type")
                    .cloned()
                    .collect();
                log.lock()
                    .unwrap()
                    .push((path.clone(), picked.clone(), body.len()));
                let (status, text) = if path == "/health" {
                    (200, health.to_string())
                } else {
                    reply(&picked)
                };
                let resp = format!(
                    "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{text}",
                    text.len()
                );
                let _ = s.write_all(resp.as_bytes()).await;
            });
        }
    });
    (url, seen)
}

fn docling_reply(_: &[(String, String)]) -> (u16, String) {
    (
        200,
        r#"{"total":2,"pages":[{"number":1,"text":"Synthetic invoice total 42","status":"native","method":"docling"},{"number":2,"text":"","status":"failed","method":"docling"}]}"#.into(),
    )
}

fn ocr_reply(h: &[(String, String)]) -> (u16, String) {
    assert!(
        h.iter().any(|(k, v)| k == "x-ocr-pages" && v == "[2]"),
        "{h:?}"
    );
    assert!(
        h.iter().any(|(k, v)| k == "x-ocr-dpi" && v == "400"),
        "{h:?}"
    );
    (
        200,
        r#"{"pages":[{"number":2,"text":"Scanned synthetic page"}]}"#.into(),
    )
}

fn refusal_reply(_: &[(String, String)]) -> (u16, String) {
    (422, "{}".into())
}

fn projects(f: &Front) -> serde_json::Value {
    let text =
        std::fs::read_to_string(f.data.join("users").join(&f.user).join("projects.json")).unwrap();
    serde_json::from_str(&text).unwrap()
}

fn queue(f: &Front) -> Vec<(String, String, i64)> {
    let file = f
        .data
        .join("users")
        .join(&f.user)
        .join("extraction-jobs.db");
    if !file.exists() {
        return Vec::new();
    }
    let c = Connection::open(file).unwrap();
    let mut st = c
        .prepare("SELECT file, state, attempts FROM jobs ORDER BY file")
        .unwrap();
    st.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        .unwrap()
        .map(Result::unwrap)
        .collect()
}

fn outbox_jobs(f: &Front) -> Vec<(String, String)> {
    let file = f.data.join("users").join(&f.user).join("index-outbox.db");
    let c = Connection::open(file).unwrap();
    let mut st = c
        .prepare("SELECT kind, file FROM jobs ORDER BY key")
        .unwrap();
    st.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .map(Result::unwrap)
        .collect()
}

fn b64(bytes: &[u8]) -> String {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

async fn worker_pass(f: &Front) -> usize {
    let data = f.data.clone();
    let engine = noevia_server::extraction::HttpEngine::new(
        tokio::runtime::Handle::current(),
        Some(data.clone()),
    )
    .unwrap();
    tokio::task::spawn_blocking(move || {
        noevia_server::extraction::pass(&data, engine.as_ref(), server_store::RustProjects::owned())
    })
    .await
    .unwrap()
}

/// Makes every queued job due now (the backoff is tested in the queue's unit tests).
fn due_now(f: &Front) {
    let file = f
        .data
        .join("users")
        .join(&f.user)
        .join("extraction-jobs.db");
    Connection::open(file)
        .unwrap()
        .execute("UPDATE jobs SET next_at = 0", [])
        .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn documents_are_extracted_by_docling_and_queued_while_it_is_down() {
    let f = front().await;
    let h = headers(&f);
    // A project as Node's store writes it (synthetic).
    let id = "p-docs".to_string();
    std::fs::write(
        f.data.join("users").join(&f.user).join("projects.json"),
        r#"{"projects":[{"id":"p-docs","name":"Docs","files":[],"chats":[],"toolboxes":["core"],"updatedAt":1}]}"#,
    )
    .unwrap();
    let url = format!("/api/projects/{id}/documents");
    let pdf = b"%PDF-1.4 synthetic invoice".to_vec();

    // Refusals, in Node's words, before anything is written.
    assert_eq!(
        call(f.addr, "POST", "/api/projects/nope/documents", &h, "{}").await,
        (404, r#"{"error":"no such project"}"#.into())
    );
    assert_eq!(
        call(f.addr, "POST", &url, &h, "not json").await,
        (400, r#"{"error":"invalid JSON"}"#.into())
    );
    assert_eq!(call(f.addr, "POST", &url, &h, "null").await.0, 500);
    assert_eq!(
        call(f.addr, "POST", &url, &h, r#"{"name":"notes.txt"}"#).await,
        (
            400,
            r#"{"error":"notes.txt is not a supported document (PDF)."}"#.into()
        )
    );
    assert_eq!(
        call(f.addr, "POST", &url, &h, r#"{}"#).await,
        (
            400,
            r#"{"error":"that file is not a supported document (PDF)."}"#.into()
        )
    );
    assert_eq!(
        call(
            f.addr,
            "POST",
            &url,
            &h,
            r#"{"name":"a.pdf","dataBase64":""}"#
        )
        .await,
        (400, r#"{"error":"document data was empty"}"#.into())
    );
    assert_eq!(
        call(f.addr, "POST", &url, &[("origin", ORIGIN)], "{}")
            .await
            .0,
        401
    );
    assert!(queue(&f).is_empty());

    // 1. Docling not reachable: stored failed, queued, nothing extracted.
    std::env::set_var("DOCLING_BASE_URL", "http://127.0.0.1:9");
    std::env::remove_var("OCR_BASE_URL");
    let upload = format!(r#"{{"name":"invoice.pdf","dataBase64":"{}"}}"#, b64(&pdf));
    let (s, body) = call(f.addr, "POST", &url, &h, &upload).await;
    assert_eq!(s, 200, "{body}");
    let answer: serde_json::Value = serde_json::from_str(&body).unwrap();
    // #2237: one state, in words a person can read; "fetch failed" is not shown.
    assert_eq!(answer["document"]["state"], "queued");
    assert_eq!(
        answer["document"]["error"],
        "Waiting for document reading to come back. The file is saved and will be read automatically."
    );
    assert_eq!(answer["document"]["errorId"], "extractionWaiting");
    assert!(!body.contains("fetch failed"), "{body}");
    assert_eq!(answer["document"]["extraction"], "queued");
    assert_eq!(answer["document"]["indexing"], "unavailable");
    assert_eq!(answer["characters"], 0);
    assert_eq!(queue(&f), vec![("invoice.pdf".into(), "pending".into(), 1)]);
    let byte_hash = answer["document"]["byteHash"].as_str().unwrap().to_string();
    let version = answer["document"]["version"].as_str().unwrap().to_string();
    let docs = f.data.join("users").join(&f.user).join("project-documents");
    let folder = std::fs::read_dir(&docs)
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    assert_eq!(
        std::fs::read(folder.join(format!("{byte_hash}.pdf"))).unwrap(),
        pdf
    );
    let stored: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(folder.join(format!("{version}.json"))).unwrap(),
    )
    .unwrap();
    assert_eq!(stored["retryable"], true);

    // The same name again without `replace`: Node's 409.
    let (s, body) = call(f.addr, "POST", &url, &h, &upload).await;
    assert_eq!(s, 409, "{body}");
    assert!(body.contains(r#""code":"sourceExists""#));

    // A pass while Docling is still down: the job waits with a longer backoff.
    due_now(&f);
    assert_eq!(worker_pass(&f).await, 1);
    assert_eq!(queue(&f), vec![("invoice.pdf".into(), "pending".into(), 2)]);

    // 2. A crash left the job running: the next start makes it pending again.
    let qfile = f
        .data
        .join("users")
        .join(&f.user)
        .join("extraction-jobs.db");
    Connection::open(&qfile)
        .unwrap()
        .execute("UPDATE jobs SET state = 'running'", [])
        .unwrap();
    assert_eq!(
        worker_pass(&f).await,
        0,
        "a running job is not claimed twice"
    );
    assert_eq!(noevia_server::extraction::recover(&f.data), 1);

    // 3. Docling and OCR back: the pass extracts, retries page 2 through OCR, stores and indexes.
    let (docling, docling_seen) = sidecar(r#"{"ranges":true}"#, docling_reply).await;
    let (ocr, ocr_seen) = sidecar(r#"{"ranges":true,"dpis":[400]}"#, ocr_reply).await;
    std::env::set_var("DOCLING_BASE_URL", &docling);
    std::env::set_var("OCR_BASE_URL", &ocr);
    due_now(&f);
    assert_eq!(worker_pass(&f).await, 1);
    assert!(queue(&f).is_empty(), "{:?}", queue(&f));
    let seen = docling_seen.lock().unwrap().clone();
    let extract = seen.iter().find(|(p, _, _)| p == "/extract").unwrap();
    assert!(extract
        .1
        .contains(&("x-document-name".into(), "invoice.pdf".into())));
    assert!(extract
        .1
        .contains(&("content-type".into(), "application/octet-stream".into())));
    assert_eq!(extract.2, pdf.len());
    assert!(ocr_seen
        .lock()
        .unwrap()
        .iter()
        .any(|(p, _, _)| p == "/extract"));
    let p = projects(&f);
    let file = &p["projects"][0]["files"][0];
    assert_eq!(file["name"], "invoice.pdf");
    assert_eq!(file["document"]["state"], "ready");
    assert_eq!(file["document"]["indexing"], "pending");
    assert!(file["document"].get("extraction").is_none());
    assert_eq!(
        file["document"]["pageStatus"][1]["retry"]["outcome"],
        "recovered"
    );
    assert!(
        file["textRef"].is_string(),
        "the text is in its own file: {file}"
    );
    assert!(file.get("content").is_none());
    assert!(outbox_jobs(&f).contains(&("index".into(), "invoice.pdf".into())));
    // The same bytes and extractor are the same version: the retryable record was replaced.
    let stored: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(folder.join(format!("{version}.json"))).unwrap(),
    )
    .unwrap();
    assert_eq!(stored["retryable"], false);
    assert_eq!(stored["completion"], "ranges-ocr-retry-v1");

    // 4. A new upload with Docling up is read at once: ready, nothing queued.
    let upload2 = format!(
        r#"{{"name":"second.pdf","dataBase64":"{}"}}"#,
        b64(b"%PDF-1.4 second synthetic")
    );
    let (s, body) = call(f.addr, "POST", &url, &h, &upload2).await;
    assert_eq!(s, 200, "{body}");
    let answer: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(answer["document"]["state"], "ready");
    assert_eq!(answer["pages"], 2);
    assert!(answer["characters"].as_u64().unwrap() > 0);
    assert!(answer["document"].get("extraction").is_none());
    assert!(queue(&f).is_empty());

    // 5. A refusal (422) is final: stored failed, never queued.
    let (refuse, _) = sidecar(r#"{"ranges":true}"#, refusal_reply).await;
    std::env::set_var("DOCLING_BASE_URL", &refuse);
    let upload3 = format!(
        r#"{{"name":"broken.pdf","dataBase64":"{}"}}"#,
        b64(b"%PDF-1.4 unreadable synthetic")
    );
    let (s, body) = call(f.addr, "POST", &url, &h, &upload3).await;
    assert_eq!(s, 200, "{body}");
    let answer: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(answer["document"]["state"], "failed");
    assert_eq!(
        answer["document"]["error"],
        "This document could not be read; the original is stored."
    );
    assert!(answer["document"].get("extraction").is_none());
    assert!(queue(&f).is_empty());

    // 6. Replacing a readable document while Docling is down keeps the last readable text (stale).
    std::env::set_var("DOCLING_BASE_URL", "http://127.0.0.1:9");
    let replace = format!(
        r#"{{"name":"second.pdf","replace":true,"dataBase64":"{}"}}"#,
        b64(b"%PDF-1.4 second synthetic, edited")
    );
    let (s, body) = call(f.addr, "POST", &url, &h, &replace).await;
    assert_eq!(s, 200, "{body}");
    let answer: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(answer["document"]["stale"], true);
    assert_eq!(answer["document"]["state"], "queued");
    assert_eq!(answer["document"]["extraction"], "queued");
    assert!(answer["characters"].as_u64().unwrap() > 0);
    assert_eq!(queue(&f).len(), 1);
    std::env::remove_var("DOCLING_BASE_URL");
    std::env::remove_var("OCR_BASE_URL");
}
