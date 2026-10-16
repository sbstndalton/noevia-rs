//! GET /api/export/conversations through the front (#1315, permanent under NOEVIA_RUST_PROJECTS=1 since #1071): Node's gate
//! first, the signed-in user's own files only, the audit row before the download, Node's headers,
//! Node's 413 and a 500 when the audit cannot be written. The ZIP's bytes are
//! `server_export::route::export`'s, which crates/server-export/tests/route.rs holds to Node's own
//! answers. Synthetic data only.
#![cfg(all(feature = "rust-export", feature = "rust-free-chats"))]
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
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

const SCHEMA: &str = include_str!("../../../crates/server-store/tests/fixtures/node-schema.sql");
const CODE: &str = "synthetic-export-code";
const ORIGIN: &str = "http://127.0.0.1:18021";
const OTHER: &str = "bbbbbbbb-2222-4222-8222-bbbbbbbbbbbb";

struct Front {
    _data: tempfile::TempDir,
    data: PathBuf,
    addr: SocketAddr,
    jar: String,
    user: String,
}

impl Front {
    fn db(&self) -> Connection {
        Connection::open(server_store::db_path(&self.data)).unwrap()
    }
    fn audits(&self) -> Vec<(String, String, String, String)> {
        let c = self.db();
        let mut st = c
            .prepare("SELECT actor_user_id,target_user_id,action,detail FROM audit_events WHERE action='export.conversations' ORDER BY id")
            .unwrap();
        st.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
            .unwrap()
            .map(Result::unwrap)
            .collect()
    }
    fn write(&self, user: &str, name: &str, text: &str) {
        let dir = self.data.join("users").join(user);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(name), text).unwrap();
    }
}

/// One GET: the status, the response head (lower-case) and the body bytes.
async fn get(addr: SocketAddr, path: &str, cookie: Option<&str>) -> (u16, String, Vec<u8>) {
    call(addr, "GET", path, cookie).await
}

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
    req.push_str("\r\n");
    s.write_all(req.as_bytes()).await.unwrap();
    let mut out = Vec::new();
    s.read_to_end(&mut out).await.unwrap();
    let split = out.windows(4).position(|w| w == b"\r\n\r\n").unwrap();
    let head = String::from_utf8(out[..split].to_vec()).unwrap();
    let status = head[9..12].parse().unwrap();
    (status, head.to_ascii_lowercase(), out[split + 4..].to_vec())
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
    // Node answers nothing here (nothing listens on the upstream).
    let mut env: HashMap<&str, String> = HashMap::new();
    env.insert("NOEVIA_LEGACY_UPSTREAM", "http://127.0.0.1:9".into());
    env.insert("UI_PORT", "1".into());
    env.insert("UI_DATA_DIR", data.display().to_string());
    env.insert("NOEVIA_WEB_DIST", data.join("dist").display().to_string());
    env.insert("PUBLIC_ORIGIN", ORIGIN.into());
    env.insert("NOEVIA_RUST_PROJECTS", "1".into());
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
        r#"{{"setupCode":"{CODE}","username":"owner","password":"synthetic export password","diaryEnabled":false}}"#
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
    let f = Front {
        _data: dir,
        data,
        addr,
        jar,
        user,
    };
    // Another tenant's files sit beside the owner's, under the same chat ids.
    f.write(
        OTHER,
        "free-chats.json",
        r#"[{"id":"c1","title":"OTHER TENANT TITLE"}]"#,
    );
    f.write(
        OTHER,
        "history-c1.json",
        r#"{"history":[{"role":"user","content":"OTHER TENANT CONTENT"}]}"#,
    );
    f
}

/// The clock reading the export was built at, from its README.md ("Exported <ISO>.").
fn exported_at(zip: &[u8]) -> i64 {
    let text = String::from_utf8_lossy(zip);
    let at = text.split("Exported ").nth(1).unwrap();
    let iso = &at[..24];
    chrono_ms(iso)
}

fn chrono_ms(iso: &str) -> i64 {
    // YYYY-MM-DDTHH:MM:SS.mmmZ, UTC.
    let n = |a: usize, b: usize| iso[a..b].parse::<i64>().unwrap();
    let (y, mo, d) = (n(0, 4), n(5, 7), n(8, 10));
    let (h, mi, s, ms) = (n(11, 13), n(14, 16), n(17, 19), n(20, 23));
    // Days from civil (Howard Hinnant's algorithm).
    let y2 = if mo <= 2 { y - 1 } else { y };
    let era = y2.div_euclid(400);
    let yoe = y2 - era * 400;
    let mp = (mo + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    ((days * 24 + h) * 60 + mi) * 60_000 + s * 1000 + ms
}

#[tokio::test(flavor = "multi_thread")]
async fn a_signed_in_export_is_the_users_own_audited_and_nodes_bytes() {
    let f = front().await;
    f.write(
        &f.user,
        "free-chats.json",
        r#"[{"id":"c1","title":"Mine","updatedAt":1800000000000}]"#,
    );
    f.write(
        &f.user,
        "history-c1.json",
        r#"{"history":[{"role":"user","content":"MY CONTENT"},{"role":"assistant","content":"ok","reasoning":"SECRET_REASONING"}]}"#,
    );
    let (status, head, body) = get(f.addr, "/api/export/conversations", Some(&f.jar)).await;
    assert_eq!(status, 200, "{head}");
    let when = exported_at(&body);
    let want = server_export::route::export(&f.data, &f.user, when);
    let server_export::route::Outcome::Zip { chunks, day, .. } = want else {
        panic!("{want:?}");
    };
    let bytes = chunks.concat();
    assert_eq!(body, bytes);
    for line in [
        "content-type: application/zip".to_string(),
        format!("content-length: {}", bytes.len()),
        format!("content-disposition: attachment; filename=\"noevia-conversations-{day}.zip\""),
        "cache-control: no-store".to_string(),
        "x-noevia-api: 1".to_string(),
        "x-content-type-options: nosniff".to_string(),
        "strict-transport-security: max-age=15552000".to_string(),
    ] {
        assert!(head.lines().any(|l| l == line), "{line} missing in {head}");
    }
    let text = String::from_utf8_lossy(&body);
    assert!(text.contains("MY CONTENT"));
    assert!(!text.contains("OTHER TENANT"));
    assert!(!text.contains("SECRET_REASONING"));
    assert_eq!(
        f.audits(),
        vec![(
            f.user.clone(),
            f.user.clone(),
            "export.conversations".into(),
            format!("{{\"bytes\":{}}}", bytes.len())
        )]
    );
    // Nothing names the other tenant either.
    assert!(!text.contains(OTHER));
}

#[tokio::test(flavor = "multi_thread")]
async fn signed_out_is_refused_before_anything_is_read_or_audited() {
    let f = front().await;
    for cookie in [None, Some("cowork_session=not-a-session")] {
        let (status, head, body) = get(f.addr, "/api/export/conversations", cookie).await;
        assert_eq!(status, 401, "{head}");
        assert!(!String::from_utf8_lossy(&body).contains("OTHER TENANT"));
    }
    assert!(f.audits().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn too_large_is_nodes_413_neither_audited_nor_sent() {
    let f = front().await;
    let many = format!("[{}]", vec![r#"{"id":"c"}"#; 19_999].join(","));
    f.write(&f.user, "free-chats.json", &many);
    let (status, head, body) = get(f.addr, "/api/export/conversations", Some(&f.jar)).await;
    assert_eq!(status, 413, "{head}");
    assert_eq!(
        String::from_utf8(body).unwrap(),
        r#"{"error":"Your conversations are too large to export in one file (the limit is 64 MB and 20,000 chats). Delete conversations you no longer need and try again."}"#
    );
    assert!(head.lines().any(|l| l == "content-type: application/json"));
    assert!(head.lines().any(|l| l == "cache-control: no-store"));
    assert!(f.audits().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn an_audit_that_cannot_be_written_is_a_500_and_nothing_is_sent() {
    let f = front().await;
    f.write(
        &f.user,
        "free-chats.json",
        r#"[{"id":"c1","title":"Mine"}]"#,
    );
    f.db()
        .execute_batch("CREATE TRIGGER export_audit_abort BEFORE INSERT ON audit_events WHEN NEW.action='export.conversations' BEGIN SELECT RAISE(ABORT,'synthetic audit refusal'); END;")
        .unwrap();
    let (status, head, body) = get(f.addr, "/api/export/conversations", Some(&f.jar)).await;
    assert_eq!(status, 500, "{head}");
    assert_eq!(body, br#"{"error":"Internal error"}"#);
    assert!(f.audits().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_broken_store_file_is_a_500_not_a_smaller_export() {
    let f = front().await;
    f.write(&f.user, "projects.json", r#"{"projects":{"0":{"id":"p"}}}"#);
    let (status, _, body) = get(f.addr, "/api/export/conversations", Some(&f.jar)).await;
    assert_eq!(status, 500);
    assert_eq!(body, br#"{"error":"Internal error"}"#);
    assert!(f.audits().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn other_methods_and_paths_stay_nodes() {
    let f = front().await;
    // Node is down in this harness: a forwarded request is the proxy's 502, never an export.
    for (m, p) in [
        ("POST", "/api/export/conversations"),
        ("HEAD", "/api/export/conversations"),
        ("GET", "/api/export/conversations/"),
    ] {
        let (status, head, _) = call(f.addr, m, p, Some(&f.jar)).await;
        assert_ne!(status, 200, "{m} {p}: {head}");
        assert!(!head.contains("application/zip"), "{m} {p}: {head}");
    }
    assert!(f.audits().is_empty());
}
