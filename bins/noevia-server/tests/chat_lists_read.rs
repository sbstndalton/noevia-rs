//! GET /api/chats/search (#1450, `rust-chat-search`) and GET /api/chats/{chatId}/export (#1471,
//! `rust-chat-export`) through the front: Node's gate first, the signed-in user's own files only, and
//! Node's answer byte for byte. Every case of tests/fixtures/node-chat-lists.v1.json (made by
//! tests/chat-lists-oracle.cjs from core's own workspace load, readHistory and routes/chat-lists.cjs at
//! tests/fixtures/chat-lists-oracle.core-ref, over synthetic tenants on disk) is laid out under the
//! signed-in user's directory, with another tenant's data under the same chat ids beside it, and asked
//! of the front: the status, the content type, cache control, disposition and length, and the body
//! must be Node's. Other methods and spellings still go to Node. Synthetic data only.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]

use base64::Engine as _;
use noevia_server::config::Config;
use noevia_server::serve::{serve, Limits};
use serde_json::Value;
use server_store::rusqlite::Connection;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

const SCHEMA: &str = include_str!("../../../crates/server-store/tests/fixtures/node-schema.sql");
const CODE: &str = "synthetic-chat-lists-code";
const ORIGIN: &str = "http://127.0.0.1:18021";

fn fixture() -> Value {
    let path = std::env::var("NOEVIA_CHAT_LISTS_FIXTURE").unwrap_or_else(|_| {
        concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/node-chat-lists.v1.json"
        )
        .to_string()
    });
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

struct Front {
    _data: tempfile::TempDir,
    data: PathBuf,
    addr: SocketAddr,
    jar: String,
    user: String,
}

/// One request: the status, the response head (lower-case) and the body bytes.
async fn call(
    addr: SocketAddr,
    method: &str,
    path: &str,
    cookie: Option<&str>,
) -> (u16, String, Vec<u8>) {
    let mut s = TcpStream::connect(addr).await.unwrap();
    let mut req = format!("{method} {path} HTTP/1.1\r\nhost: front.test\r\nconnection: close\r\n");
    if let Some(c) = cookie {
        req.push_str(&format!("cookie: {c}\r\n"));
    }
    if method != "GET" {
        req.push_str(&format!("origin: {ORIGIN}\r\ncontent-length: 0\r\n"));
    }
    req.push_str("\r\n");
    s.write_all(req.as_bytes()).await.unwrap();
    let mut out = Vec::new();
    s.read_to_end(&mut out).await.unwrap();
    let split = out.windows(4).position(|w| w == b"\r\n\r\n").unwrap();
    let head = String::from_utf8(out[..split].to_vec()).unwrap();
    let status = head[9..12].parse().unwrap();
    (status, head.to_ascii_lowercase(), out[split + 4..].to_vec())
}

fn header<'a>(head: &'a str, name: &str) -> Option<&'a str> {
    head.lines().find_map(|l| {
        l.strip_prefix(name)
            .and_then(|rest| rest.strip_prefix(": "))
            .map(str::trim_end)
    })
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
    // Node answers nothing here: nothing listens on the upstream, so a request the front sends on is
    // its 502.
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
        r#"{{"setupCode":"{CODE}","username":"owner","password":"synthetic chat lists password","diaryEnabled":false}}"#
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
    let jar = text
        .lines()
        .filter_map(|l| {
            l.strip_prefix("set-cookie: ")
                .map(|v| v.split(';').next().unwrap().to_string())
        })
        .collect::<Vec<_>>()
        .join("; ");
    let user: String = c
        .query_row("SELECT id FROM users", [], |r| r.get(0))
        .unwrap();
    Front {
        _data: dir,
        data,
        addr,
        jar,
        user,
    }
}

/// The user's directory holding exactly `files` (a string is UTF-8 text, `{hex}` raw bytes, `{dir}` a
/// directory), as the oracle laid it out.
fn lay_out(dir: &Path, files: &Value) {
    if dir.exists() {
        for entry in std::fs::read_dir(dir).unwrap() {
            let p = entry.unwrap().path();
            if p.is_dir() {
                std::fs::remove_dir_all(&p).unwrap();
            } else {
                std::fs::remove_file(&p).unwrap();
            }
        }
    }
    std::fs::create_dir_all(dir).unwrap();
    for (name, spec) in files.as_object().unwrap() {
        let target = dir.join(name);
        if let Some(text) = spec.as_str() {
            std::fs::write(&target, text).unwrap();
        } else if let Some(hex) = spec.get("hex").and_then(Value::as_str) {
            let bytes: Vec<u8> = (0..hex.len())
                .step_by(2)
                .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap())
                .collect();
            std::fs::write(&target, bytes).unwrap();
        } else if spec.get("dir").is_some() {
            std::fs::create_dir_all(&target).unwrap();
        } else {
            panic!("unknown file spec {spec}");
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn every_oracle_case_is_nodes_answer_through_the_front() {
    let fx = fixture();
    assert_eq!(fx["v"], 1);
    let f = front().await;
    let other = f
        .data
        .join("users")
        .join(fx["users"]["b"].as_str().unwrap());
    lay_out(&other, &fx["other"]);
    let mine = f.data.join("users").join(&f.user);
    let cases = fx["cases"].as_array().unwrap();
    assert!(cases.len() >= 95, "{}", cases.len());
    // The oracle reads zone-less Date strings in UTC; the front reads them in its own zone, as Node
    // does in the same container. Those few cases need this process to run in UTC too (CI does).
    let utc = server_export::js_date::parse("2026-01-01T00:00") == Some(1_767_225_600_000.0);
    // noevia#2345: CI must run this in UTC (a skip there would pass silently), and two cases
    // without "-local-" in their id are zone-dependent too: "-000000-01-01" (not ISO, so the
    // legacy parser reads it as a local date) and [2026,1,1] (joined to "2026,1,1", also local).
    assert!(
        utc || std::env::var_os("CI").is_none(),
        "CI runs this test outside UTC: its zone-dependent cases would be skipped"
    );
    const ZONED: [&str; 2] = ["export-date-minus-zero-year", "export-date-array"];
    let mut failures = Vec::new();
    for c in cases {
        let id = c["id"].as_str().unwrap();
        if (id.contains("-local-") || ZONED.contains(&id)) && !utc {
            eprintln!("skipped {id}: this process does not run in UTC");
            continue;
        }
        lay_out(&mine, &c["files"]);
        let (status, head, body) =
            call(f.addr, "GET", c["path"].as_str().unwrap(), Some(&f.jar)).await;
        let node = &c["node"];
        let want = base64::engine::general_purpose::STANDARD
            .decode(node["body"].as_str().unwrap())
            .unwrap();
        let mut wrong = Vec::new();
        if u64::from(status) != node["status"].as_u64().unwrap() {
            wrong.push(format!("status {status} != {}", node["status"]));
        }
        for h in [
            "content-type",
            "cache-control",
            "content-disposition",
            "content-length",
        ] {
            let got = header(&head, h);
            let expected = node["headers"][h].as_str().map(str::to_ascii_lowercase);
            // Node's json() leaves the length to the transport; the front always sends one.
            if h == "content-length" && expected.is_none() {
                continue;
            }
            if got != expected.as_deref() {
                wrong.push(format!("{h} {got:?} != {expected:?}"));
            }
        }
        if body != want {
            wrong.push(format!(
                "body {:?} != {:?}",
                String::from_utf8_lossy(&body),
                String::from_utf8_lossy(&want)
            ));
        }
        // The other tenant's text never reaches this user's answer.
        assert!(
            !String::from_utf8_lossy(&body).contains("B SECRET"),
            "{id}: another tenant's data"
        );
        if !wrong.is_empty() {
            failures.push(format!("{id}: {}", wrong.join("; ")));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
    // Nothing was written in the user's directory but the case files (the routes only read).
    let last = cases.last().unwrap();
    let names: Vec<String> = std::fs::read_dir(&mine)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    for n in &names {
        assert!(last["files"].get(n).is_some(), "the read wrote {n}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn signed_out_other_methods_and_spellings_are_not_answered_here() {
    let f = front().await;
    let dir = f.data.join("users").join(&f.user);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("free-chats.json"),
        r#"[{"id":"c1","title":"Mine"}]"#,
    )
    .unwrap();
    std::fs::write(
        dir.join("history-c1.json"),
        r#"{"history":[{"role":"user","content":"needle"}]}"#,
    )
    .unwrap();
    // Node's gate first: signed out is its 401, nothing read.
    for path in ["/api/chats/search?q=needle", "/api/chats/c1/export"] {
        let (status, head, body) = call(f.addr, "GET", path, None).await;
        assert_eq!(status, 401, "{path} {head}");
        assert!(!String::from_utf8_lossy(&body).contains("needle"));
        let (status, _, body) = call(f.addr, "GET", path, Some(&f.jar)).await;
        assert_eq!(status, 200, "{path}");
        assert!(!body.is_empty());
    }
    // Every other method and spelling is Node's (nothing listens there: the proxy's 502).
    for (method, path) in [
        ("POST", "/api/chats/search"),
        ("HEAD", "/api/chats/search"),
        ("GET", "/api/chats/search/"),
        ("POST", "/api/chats/c1/export"),
        ("DELETE", "/api/chats/c1/export"),
        ("GET", "/api/chats/c1/export/"),
        ("GET", "/api/chats//export"),
        ("GET", "/api/chats/a/b/export"),
    ] {
        let (status, head, _) = call(f.addr, method, path, Some(&f.jar)).await;
        assert_eq!(status, 502, "{method} {path} {head}");
    }
}

#[test]
fn the_oracle_core_is_pinned_and_the_capabilities_are_listed() {
    let pin = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/chat-lists-oracle.core-ref"
    ))
    .unwrap();
    let pin = pin.trim();
    assert!(
        pin.len() == 40 && pin.bytes().all(|b| b.is_ascii_hexdigit()),
        "{pin}"
    );
    for cap in ["rust-chat-search", "rust-chat-export"] {
        assert!(noevia_server::FEATURES.contains(&cap), "{cap}");
    }
}
