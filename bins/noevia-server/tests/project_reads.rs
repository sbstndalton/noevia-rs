//! sbstndalton/noevia#2191 `rust-project-reads`: `GET /api/projects/{id}/sources/text`,
//! `.../documents/original` and `.../uploads/original` through the whole front, with Node absent
//! (an unreachable upstream: anything proxied is a 502). Synthetic data only.
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
const CODE: &str = "synthetic-project-reads-code";
const ORIGIN: &str = "http://127.0.0.1:18021";

struct Front {
    _data: tempfile::TempDir,
    data: PathBuf,
    addr: SocketAddr,
    jar: String,
    csrf: String,
    user: String,
}

impl Front {
    fn user_dir(&self) -> PathBuf {
        self.data.join("users").join(&self.user)
    }
}

/// One HTTP/1.1 exchange at once: the status, the response head (lower-case names) and the body.
async fn call(
    addr: SocketAddr,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
    body: &str,
) -> (u16, String, String) {
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
    // Lossy: an original's bytes need not be UTF-8 (lengths are checked through content-length).
    let text = String::from_utf8_lossy(&out).into_owned();
    let status = text[9..12].parse().unwrap();
    let (head, body) = text.split_once("\r\n\r\n").unwrap();
    (status, head.to_ascii_lowercase(), body.to_string())
}

async fn front() -> Front {
    let f = front_new_account(false).await;
    // Node's workspace load made the directory on the user's first request.
    std::fs::create_dir_all(f.user_dir()).unwrap();
    f
}

/// A signed-in account that has made no request yet: its directory does not exist.
async fn front_new_account(writes: bool) -> Front {
    front_with_retired_value(writes.then_some("1")).await
}

async fn front_with_retired_value(value: Option<&str>) -> Front {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().to_path_buf();
    let c = Connection::open(server_store::db_path(&data)).unwrap();
    c.execute_batch(SCHEMA).unwrap();
    c.execute(
        "INSERT INTO settings VALUES('setup_code_hash', ?1)",
        [server_auth::js::digest(CODE)],
    )
    .unwrap();
    // Node answers nothing here: every route under test is the front's.
    let mut env: HashMap<&str, String> = HashMap::new();
    env.insert("NOEVIA_LEGACY_UPSTREAM", "http://127.0.0.1:9".into());
    env.insert("UI_PORT", "1".into());
    env.insert("UI_DATA_DIR", data.display().to_string());
    env.insert("NOEVIA_WEB_DIST", data.join("dist").display().to_string());
    env.insert("PUBLIC_ORIGIN", ORIGIN.into());
    if let Some(value) = value {
        env.insert("NOEVIA_RUST_PROJECT_WRITES", value.into());
        env.insert("NOEVIA_RUST_PROJECT_WRITES_CONFIRMED", value.into());
    }
    let app = noevia_server::App::new(Config::from_lookup(|k| env.get(k).cloned()).unwrap());
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(serve(
        listener,
        app,
        Limits::default(),
        std::future::pending(),
    ));
    // Sign the owner in (setup), as the corpus does.
    let setup = format!(
        r#"{{"setupCode":"{CODE}","username":"owner","password":"synthetic project-writes password","diaryEnabled":false}}"#
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
    assert_eq!(cookies.len(), 2, "{text}");
    let csrf = cookies[1].split_once('=').unwrap().1.to_string();
    let jar = cookies.join("; ");
    let user: String = c
        .query_row("SELECT id FROM users", [], |r| r.get(0))
        .unwrap();
    assert!(
        !data.join("users").exists(),
        "setup makes no user directory"
    );
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
    ]
}

fn insert_member_session(f: &Front) -> (String, String) {
    let user = "22222222-2222-4222-8222-222222222222";
    let token = "synthetic-member-session-token";
    let csrf = "synthetic-member-csrf-token";
    let c = Connection::open(server_store::db_path(&f.data)).unwrap();
    c.execute(
        "INSERT INTO users(id,username,username_norm,display_name,role,password_hash,webauthn_user_id,created_at,updated_at) VALUES(?1,'member','member','Synthetic member','member','x',?1,1,1)",
        [user],
    )
    .unwrap();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64;
    c.execute(
        "INSERT INTO sessions(id_hash,user_id,csrf_hash,created_at,last_seen_at,expires_at,user_agent,ip) VALUES(?1,?2,?3,?4,?4,?5,'synthetic','127.0.0.1')",
        (server_auth::js::digest(token), user, server_auth::js::digest(csrf), now, now + 86_400_000),
    )
    .unwrap();
    (
        user.to_string(),
        format!("cowork_session={token}; cowork_csrf={csrf}"),
    )
}

const REF: &str = "1111111111111111111111111111111111111111111111111111111111111111";
const PDF_HASH: &str = "2222222222222222222222222222222222222222222222222222222222222222";
const UPLOAD_ID: &str = "3333333333333333333333333333333333333333333333333333333333333333";

fn hex_sha256(s: &str) -> String {
    use sha2::Digest;
    sha2::Sha256::digest(s.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// One project with a text source (external), an inline source, a PDF and an upload; the
/// stored files as Node's ingest leaves them.
fn seed(f: &Front) {
    let dir = f.user_dir();
    let projects = format!(
        r#"{{"projects":[{{"id":"p1","name":"Synthetic","files":[{{"name":"notes.md","textRef":"{REF}","chars":5}},{{"name":"inline.md","content":"in😀"}},{{"name":"Synthetic/Documents/report one.pdf","content":"","document":{{"byteHash":"{PDF_HASH}","state":"ready"}}}},{{"name":"Synthetic/Other/data é.bin","content":"","attachment":{{"id":"{UPLOAD_ID}","state":"stored"}}}},{{"name":"bad.pdf","document":{{"byteHash":"../../../../etc/passwd"}}}}],"chats":[]}},{{"id":"cowork-chat-context-c9","name":"ctx","routing":"manual","files":[{{"name":"a.md","content":"x"}}],"chats":[]}}]}}"#
    );
    std::fs::write(dir.join("projects.json"), projects).unwrap();
    let text = dir.join("project-sources").join("p1");
    std::fs::create_dir_all(&text).unwrap();
    std::fs::write(text.join(format!("{REF}.txt")), "hello").unwrap();
    let docs = dir.join("project-documents").join(hex_sha256("p1"));
    std::fs::create_dir_all(&docs).unwrap();
    std::fs::write(docs.join(format!("{PDF_HASH}.pdf")), b"%PDF-1.7 synthetic").unwrap();
    let ups = dir.join("project-uploads").join(hex_sha256("p1"));
    std::fs::create_dir_all(&ups).unwrap();
    std::fs::write(ups.join(UPLOAD_ID), [0u8, 1, 2, 255]).unwrap();
}

fn get_headers(f: &Front) -> Vec<(&str, &str)> {
    vec![("cookie", f.jar.as_str())]
}

#[tokio::test]
async fn source_text_is_answered_natively_with_node_status_codes() {
    let f = front().await;
    seed(&f);
    let h = get_headers(&f);
    // Failing before #2191: Node answered this; with Node absent it was a proxy 502.
    let (status, head, body) = call(
        f.addr,
        "GET",
        "/api/projects/p1/sources/text?name=notes.md",
        &h,
        "",
    )
    .await;
    assert_eq!(
        (status, body.as_str()),
        (200, r#"{"name":"notes.md","content":"hello","chars":5}"#)
    );
    assert!(head.contains("cache-control: no-store"), "{head}");
    let (status, _, body) = call(
        f.addr,
        "GET",
        "/api/projects/p1/sources/text?name=inline.md",
        &h,
        "",
    )
    .await;
    // chars is UTF-16 code units, as `content.length`.
    assert_eq!(
        (status, body.as_str()),
        (200, r#"{"name":"inline.md","content":"in😀","chars":4}"#)
    );
    let (status, _, body) = call(f.addr, "GET", "/api/projects/p1/sources/text", &h, "").await;
    assert_eq!(
        (status, body.as_str()),
        (400, r#"{"error":"a source name is required"}"#)
    );
    let (status, _, body) = call(
        f.addr,
        "GET",
        "/api/projects/p1/sources/text?name=nope",
        &h,
        "",
    )
    .await;
    assert_eq!(
        (status, body.as_str()),
        (404, r#"{"error":"no such source"}"#)
    );
    let (status, _, body) = call(
        f.addr,
        "GET",
        "/api/projects/zz/sources/text?name=a.md",
        &h,
        "",
    )
    .await;
    assert_eq!(
        (status, body.as_str()),
        (404, r#"{"error":"no such project"}"#)
    );
    // An internal project is hidden, but getProject's self-heal is still saved, as in Node.
    let (status, _, body) = call(
        f.addr,
        "GET",
        "/api/projects/cowork-chat-context-c9/sources/text?name=a.md",
        &h,
        "",
    )
    .await;
    assert_eq!(
        (status, body.as_str()),
        (404, r#"{"error":"no such project"}"#)
    );
    let stored = std::fs::read_to_string(f.user_dir().join("projects.json")).unwrap();
    let saved: serde_json::Value = serde_json::from_str(&stored).unwrap();
    assert_eq!(saved["projects"][1]["routing"], "auto");
    // Missing text: its own code, and the pointer is kept.
    std::fs::remove_file(
        f.user_dir()
            .join("project-sources/p1")
            .join(format!("{REF}.txt")),
    )
    .unwrap();
    let (status, _, body) = call(
        f.addr,
        "GET",
        "/api/projects/p1/sources/text?name=notes.md",
        &h,
        "",
    )
    .await;
    assert_eq!(
        (status, body.as_str()),
        (
            404,
            r#"{"error":"The text of this source is missing. Re-upload or refresh it.","code":"SOURCE_TEXT_MISSING","name":"notes.md"}"#
        )
    );
    assert!(std::fs::read_to_string(f.user_dir().join("projects.json"))
        .unwrap()
        .contains(REF));
    // A malformed escape in the id is Node's URIError: a 500.
    let (status, _, _) = call(
        f.addr,
        "GET",
        "/api/projects/%E0%A4%A/sources/text?name=x",
        &h,
        "",
    )
    .await;
    assert_eq!(status, 500);
}

#[tokio::test]
async fn originals_are_served_with_node_headers_and_refuse_traversal() {
    let f = front().await;
    seed(&f);
    let h = get_headers(&f);
    let (status, head, body) = call(
        f.addr,
        "GET",
        "/api/projects/p1/documents/original?name=Synthetic%2FDocuments%2Freport+one.pdf",
        &h,
        "",
    )
    .await;
    assert_eq!(
        (status, body.as_str()),
        (200, "%PDF-1.7 synthetic"),
        "{head}"
    );
    assert!(head.contains("content-type: application/pdf"), "{head}");
    assert!(
        head.contains("content-disposition: attachment; filename*=utf-8''report%20one.pdf"),
        "{head}"
    );
    assert!(head.contains("cache-control: private, no-store"), "{head}");
    assert!(head.contains("content-length: 18"), "{head}");
    let (status, _, body) = call(
        f.addr,
        "GET",
        "/api/projects/p1/documents/original?name=notes.md",
        &h,
        "",
    )
    .await;
    assert_eq!(
        (status, body.as_str()),
        (404, r#"{"error":"no such document"}"#)
    );
    // A byteHash that is not a hash never names a path.
    let unavailable = r#"{"error":"Saved document data is unavailable; upload or refresh again."}"#;
    let (status, _, body) = call(
        f.addr,
        "GET",
        "/api/projects/p1/documents/original?name=bad.pdf",
        &h,
        "",
    )
    .await;
    assert_eq!((status, body.as_str()), (422, unavailable));
    // A symlink in place of the stored PDF is not followed out of the tenant directory.
    let docs = f
        .user_dir()
        .join("project-documents")
        .join(hex_sha256("p1"));
    let outside = f.data.join("outside.pdf");
    std::fs::write(&outside, b"OUTSIDE SECRET").unwrap();
    std::fs::remove_file(docs.join(format!("{PDF_HASH}.pdf"))).unwrap();
    std::os::unix::fs::symlink(&outside, docs.join(format!("{PDF_HASH}.pdf"))).unwrap();
    let (status, _, body) = call(
        f.addr,
        "GET",
        "/api/projects/p1/documents/original?name=Synthetic/Documents/report%20one.pdf",
        &h,
        "",
    )
    .await;
    assert_eq!((status, body.as_str()), (422, unavailable));

    let (status, head, body) = call(
        f.addr,
        "GET",
        "/api/projects/p1/uploads/original?name=Synthetic/Other/data%20%C3%A9.bin",
        &h,
        "",
    )
    .await;
    assert_eq!(status, 200, "{head}{body}");
    assert!(
        head.contains("content-type: application/octet-stream"),
        "{head}"
    );
    assert!(
        head.contains("content-disposition: attachment; filename*=utf-8''data%20%c3%a9.bin"),
        "{head}"
    );
    assert!(head.contains("content-length: 4"), "{head}");
    let (status, _, body) = call(
        f.addr,
        "GET",
        "/api/projects/p1/uploads/original?name=notes.md",
        &h,
        "",
    )
    .await;
    assert_eq!(
        (status, body.as_str()),
        (404, r#"{"error":"No such original"}"#)
    );
    let ups = f.user_dir().join("project-uploads").join(hex_sha256("p1"));
    std::fs::remove_file(ups.join(UPLOAD_ID)).unwrap();
    std::os::unix::fs::symlink(&outside, ups.join(UPLOAD_ID)).unwrap();
    let (status, _, body) = call(
        f.addr,
        "GET",
        "/api/projects/p1/uploads/original?name=Synthetic/Other/data%20%C3%A9.bin",
        &h,
        "",
    )
    .await;
    assert_eq!(
        (status, body.as_str()),
        (404, r#"{"error":"No such original"}"#)
    );
    assert!(!body.contains("OUTSIDE"));
}

#[tokio::test]
async fn signed_out_other_tenant_and_other_methods_are_not_served() {
    let f = front().await;
    seed(&f);
    for path in [
        "/api/projects/p1/sources/text?name=notes.md",
        "/api/projects/p1/documents/original?name=bad.pdf",
        "/api/projects/p1/uploads/original?name=x",
    ] {
        let (status, _, _) = call(f.addr, "GET", path, &[], "").await;
        assert_eq!(status, 401, "signed out {path}");
    }
    // Another account sees only its own (empty) workspace.
    let (_, member) = insert_member_session(&f);
    let mh = vec![("cookie", member.as_str())];
    let (status, _, body) = call(
        f.addr,
        "GET",
        "/api/projects/p1/sources/text?name=notes.md",
        &mh,
        "",
    )
    .await;
    assert_eq!(
        (status, body.as_str()),
        (404, r#"{"error":"no such project"}"#)
    );
    let (status, _, body) = call(
        f.addr,
        "GET",
        "/api/projects/p1/uploads/original?name=Synthetic/Other/data%20%C3%A9.bin",
        &mh,
        "",
    )
    .await;
    assert_eq!(
        (status, body.as_str()),
        (404, r#"{"error":"No such original"}"#)
    );
    // Other methods and other `{*}/original` segments stay Node's (unreachable here: 502).
    let h = headers(&f);
    for (m, path) in [
        ("POST", "/api/projects/p1/sources/text"),
        ("DELETE", "/api/projects/p1/uploads/original"),
        ("GET", "/api/projects/p1/other/original?name=x"),
        ("POST", "/api/projects/p1/documents/pages?name=x"),
    ] {
        let (status, _, _) = call(f.addr, m, path, &h, "").await;
        assert_eq!(status, 502, "{m} {path} goes to Node");
    }
}

#[test]
fn the_build_lists_rust_project_reads() {
    assert!(noevia_server::FEATURES.contains(&"rust-project-reads"));
    assert!(noevia_server::FEATURES.contains(&"rust-project-document-pages"));
}

/// sbstndalton/noevia#2191 round 4 (`rust-project-document-pages`): every answer equals Node's,
/// byte for byte, as core's own `docRead` + document-sources.cjs `readPages` gave them on the same
/// synthetic stored results (fixtures/document-pages-oracle.json, written by
/// fixtures/document-pages-oracle.cjs against core e10b48b, the last with the Node route): page
/// ranges, the UTF-16 offset cut through a surrogate pair, JS `Number()`
/// of the query values, 400/404/422 refusals, a leading BOM and a missing result.
/// Failing before round 4: the GET went to Node (absent here: 502).
#[tokio::test]
async fn document_pages_answer_as_node_does() {
    let f = front().await;
    let oracle: serde_json::Value =
        serde_json::from_str(include_str!("fixtures/document-pages-oracle.json")).unwrap();
    let dir = f.user_dir();
    std::fs::write(
        dir.join("projects.json"),
        oracle["project"].as_str().unwrap(),
    )
    .unwrap();
    let docs = dir.join("project-documents").join(hex_sha256("p1"));
    std::fs::create_dir_all(&docs).unwrap();
    for (version, text) in oracle["versions"].as_object().unwrap() {
        std::fs::write(docs.join(format!("{version}.json")), text.as_str().unwrap()).unwrap();
    }
    let h = get_headers(&f);
    let cases = oracle["cases"].as_array().unwrap();
    assert!(cases.len() >= 20);
    for case in cases {
        let query = case[0].as_str().unwrap();
        let path = format!("/api/projects/p1/documents/pages{query}");
        let (status, head, body) = call(f.addr, "GET", &path, &h, "").await;
        assert_eq!(
            (u64::from(status), body.as_str()),
            (case[1].as_u64().unwrap(), case[2].as_str().unwrap()),
            "{query}"
        );
        assert!(head.contains("cache-control: no-store"), "{head}");
    }
    // A symlinked result is not followed (stricter than Node): its 422.
    let v = "a".repeat(64);
    let outside = f.data.join("outside.json");
    std::fs::rename(docs.join(format!("{v}.json")), &outside).unwrap();
    std::os::unix::fs::symlink(&outside, docs.join(format!("{v}.json"))).unwrap();
    let (status, _, body) = call(
        f.addr,
        "GET",
        "/api/projects/p1/documents/pages?name=Synthetic%2F%22report%22.pdf",
        &h,
        "",
    )
    .await;
    assert_eq!(
        (status, body.as_str()),
        (
            422,
            r#"{"error":"Saved document data is unavailable; upload or refresh again."}"#
        )
    );
    // Signed out 401; another account sees only its own workspace (no such project: 404).
    let (status, _, _) = call(
        f.addr,
        "GET",
        "/api/projects/p1/documents/pages?name=bom.pdf",
        &[],
        "",
    )
    .await;
    assert_eq!(status, 401);
    let (_, member) = insert_member_session(&f);
    let (status, _, body) = call(
        f.addr,
        "GET",
        "/api/projects/p1/documents/pages?name=bom.pdf",
        &[("cookie", member.as_str())],
        "",
    )
    .await;
    assert_eq!(
        (status, body.as_str()),
        (404, r#"{"error":"no such document"}"#)
    );
    // A bad escape in the id is Node's URIError (500).
    let (status, _, _) = call(f.addr, "GET", "/api/projects/%E0/documents/pages", &h, "").await;
    assert_eq!(status, 500);
}
