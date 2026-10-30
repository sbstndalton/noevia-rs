//! sbstndalton/noevia#2191 round 2 `rust-project-lifecycle`: `POST /api/projects` and `DELETE
//! /api/projects/{projectId}` through the whole front, with Node absent (an unreachable upstream:
//! anything proxied is a 502), and the index outbox they leave for core. Synthetic data only.
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
const CODE: &str = "synthetic-project-lifecycle-code";
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

fn hex_sha256(s: &str) -> String {
    use sha2::Digest;
    sha2::Sha256::digest(s.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// The catalogue core publishes (core server/project-catalogue.cjs).
fn publish_catalogue(f: &Front) {
    std::fs::write(
        f.data.join("project-catalogue.json"),
        r#"{"version":1,"selectable":["core","project-docs","web-search"],"defaults":["core"],"icons":["folder","book"]}"#,
    )
    .unwrap();
}

fn projects(f: &Front) -> serde_json::Value {
    let text = std::fs::read_to_string(f.user_dir().join("projects.json")).unwrap();
    serde_json::from_str::<serde_json::Value>(&text).unwrap()["projects"].clone()
}

fn json_headers(f: &Front) -> Vec<(&str, &str)> {
    let mut h = headers(f);
    h.push(("content-type", "application/json"));
    h
}

#[tokio::test]
async fn create_answers_nodes_refusals_before_anything_is_written() {
    let f = front().await;
    // Signed out, then without the CSRF token: the gate's answers.
    let (status, _, _) = call(f.addr, "POST", "/api/projects", &[("origin", ORIGIN)], "{}").await;
    assert_eq!(status, 401);
    let (status, _, _) = call(
        f.addr,
        "POST",
        "/api/projects",
        &[("cookie", f.jar.as_str()), ("origin", ORIGIN)],
        r#"{"name":"x"}"#,
    )
    .await;
    assert_eq!(status, 403);
    let h = json_headers(&f);
    // Core has not published its catalogue yet: the create waits for it (10 s), then answers 503.
    let started = std::time::Instant::now();
    let (status, _, _) = call(f.addr, "POST", "/api/projects", &h, r#"{"name":"x"}"#).await;
    assert_eq!(status, 503);
    assert!(started.elapsed() >= std::time::Duration::from_secs(9));
    // Published while a create waits: that create is served.
    let waiting = tokio::spawn({
        let (addr, h) = (
            f.addr,
            h.iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect::<Vec<_>>(),
        );
        async move {
            let h: Vec<(&str, &str)> = h.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
            call(addr, "POST", "/api/projects", &h, r#"{"name":"late"}"#)
                .await
                .0
        }
    });
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    publish_catalogue(&f);
    assert_eq!(waiting.await.unwrap(), 200);
    std::fs::remove_file(f.user_dir().join("projects.json")).unwrap();
    std::fs::remove_file(f.user_dir().join("index-outbox.db")).ok();
    for (body, status, error) in [
        ("{", 400, "invalid JSON"),
        (r#"{"name":"  "}"#, 400, "name required"),
        (
            r#"{"name":"a","reasoningEffort":"max"}"#,
            400,
            "Invalid reasoning effort",
        ),
        (
            r#"{"name":"a","modes":["x"]}"#,
            400,
            "modes may only contain chat, cowork, code",
        ),
        (
            r#"{"name":"a","icon":"rocket"}"#,
            400,
            "Choose a supported project icon.",
        ),
    ] {
        let (got, _, text) = call(f.addr, "POST", "/api/projects", &h, body).await;
        assert_eq!(got, status, "{body}");
        let v: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(v["error"], error, "{body}");
    }
    // http.cjs readBody's 1 MiB.
    let big = format!(r#"{{"name":"{}"}}"#, "x".repeat(1024 * 1024));
    let (status, _, text) = call(f.addr, "POST", "/api/projects", &h, &big).await;
    assert_eq!(
        (status, text.as_str()),
        (413, r#"{"error":"Request exceeds size limit"}"#)
    );
    assert!(
        !f.user_dir().join("projects.json").exists(),
        "nothing written"
    );
    assert!(!f.user_dir().join("index-outbox.db").exists());
}

#[tokio::test]
async fn create_saves_the_project_first_and_queues_its_files_once() {
    let f = front().await;
    publish_catalogue(&f);
    std::fs::write(
        f.user_dir().join("projects.json"),
        r#"{"projects":[{"id":"old","name":"Old","files":[],"chats":[]}]}"#,
    )
    .unwrap();
    let h = json_headers(&f);
    let body = r##"{"name":" Synthetic notes ","goal":"g","icon":"book","color":"#ABCDEF","files":[{"name":"a.md","content":"alpha"},{"name":"b.md","content":"beta"},{"name":7,"content":"x"}]}"##;
    let (status, _, text) = call(f.addr, "POST", "/api/projects", &h, body).await;
    assert_eq!(status, 200, "{text}");
    let made: serde_json::Value = serde_json::from_str(&text).unwrap();
    let id = made["id"].as_str().unwrap().to_string();
    assert!(id.starts_with("proj-"));
    assert_eq!(made["name"], "Synthetic notes");
    assert_eq!(made["color"], "#abcdef");
    assert_eq!(
        made["toolboxes"],
        serde_json::json!(["core", "project-docs"])
    );
    assert_eq!(made["docsToolboxDefaulted"], true);
    assert_eq!(made["routing"], "auto");
    assert_eq!(made["toolsMode"], "auto");
    // #1806: the texts are in their own files; the entry keeps the pointer (no text in the answer).
    assert_eq!(
        made["files"][0],
        serde_json::json!({"name": "a.md", "textRef": hex_sha256("alpha"), "chars": 5})
    );
    assert_eq!(made["files"].as_array().unwrap().len(), 2);
    assert!(!text.contains("alpha") && !text.contains("beta"));
    let stored_text = f
        .user_dir()
        .join("project-sources")
        .join(&id)
        .join(format!("{}.txt", hex_sha256("beta")));
    assert_eq!(std::fs::read_to_string(stored_text).unwrap(), "beta");
    assert!(
        made.get("reservedFolder").is_none(),
        "local storage reserves nothing"
    );
    // Put first, the old project kept; the answer is what is stored.
    let stored = projects(&f);
    assert_eq!(stored[0], made);
    assert_eq!(stored[1]["id"], "old");
    // One index job per file, pending, at one generation.
    let jobs = server_projects::outbox::jobs(&f.user_dir()).unwrap();
    let files: Vec<_> = jobs.iter().map(|j| j.file.clone().unwrap()).collect();
    assert_eq!(files, vec!["a.md", "b.md"]);
    assert!(jobs
        .iter()
        .all(|j| j.kind == "index" && j.project == id && j.state == "pending"));
    // A Manual routing preference is the new project's default.
    std::fs::write(
        f.user_dir().join("preferences.json"),
        r#"{"defaultRouting":"manual"}"#,
    )
    .unwrap();
    let (_, _, text) = call(
        f.addr,
        "POST",
        "/api/projects",
        &h,
        r#"{"name":"m","toolboxes":["web-search","nope"]}"#,
    )
    .await;
    let m: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert_eq!(m["routing"], "manual");
    assert_eq!(m["toolboxes"], serde_json::json!(["web-search"]));
    assert_eq!(m["toolsMode"], "manual");
    assert_eq!(projects(&f).as_array().unwrap().len(), 3);
}

/// A project with everything a delete removes: stored files, an image, a RAG index, a chat with
/// its transcript, and that chat's attachments project with its own files.
fn seed_full(f: &Front) {
    let dir = f.user_dir();
    std::fs::write(
        dir.join("projects.json"),
        r#"{"projects":[{"id":"p1","name":"Doomed","projectFolder":"noevia projects/p1","files":[],"chats":[{"id":"c1","title":"t"}]},{"id":"cowork-chat-context-c1","name":"Chat attachments c1","files":[],"chats":[]},{"id":"keep","name":"Kept","files":[],"chats":[]}]}"#,
    )
    .unwrap();
    for id in ["p1", "cowork-chat-context-c1", "keep"] {
        let up = dir.join("project-uploads").join(hex_sha256(id));
        std::fs::create_dir_all(&up).unwrap();
        std::fs::write(up.join("blob"), "u").unwrap();
        let docs = dir.join("project-documents").join(hex_sha256(id));
        std::fs::create_dir_all(&docs).unwrap();
        std::fs::write(docs.join("v.json"), "{}").unwrap();
        let assets = dir.join("project-assets").join(id);
        std::fs::create_dir_all(&assets).unwrap();
        std::fs::write(assets.join("img1"), "png").unwrap();
        std::fs::create_dir_all(dir.join("rag")).unwrap();
        for s in ["db", "db-wal", "db-shm"] {
            std::fs::write(dir.join("rag").join(format!("{id}.{s}")), "sqlite").unwrap();
        }
    }
    std::fs::write(
        dir.join("history-c1.json"),
        r#"{"history":[{"role":"user","content":"hi"}]}"#,
    )
    .unwrap();
}

#[tokio::test]
async fn delete_purges_the_project_its_chats_and_their_attachments_but_nothing_else() {
    let f = front().await;
    seed_full(&f);
    let dir = f.user_dir();
    // A symlink planted in an asset directory is removed as a link; its target stays.
    let outside = f.data.join("outside.txt");
    std::fs::write(&outside, "keep me").unwrap();
    std::os::unix::fs::symlink(&outside, dir.join("project-assets").join("p1").join("link"))
        .unwrap();
    let h = headers(&f);
    let (status, _, text) = call(f.addr, "DELETE", "/api/projects/p1", &h, "").await;
    assert_eq!((status, text.as_str()), (200, r#"{"ok":true}"#));
    let ids: Vec<_> = projects(&f)
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["id"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(
        ids,
        vec!["keep"],
        "the project and its chat's attachments project are gone"
    );
    for id in ["p1", "cowork-chat-context-c1"] {
        assert!(
            !dir.join("project-uploads").join(hex_sha256(id)).exists(),
            "{id} uploads"
        );
        assert!(
            !dir.join("project-documents").join(hex_sha256(id)).exists(),
            "{id} documents"
        );
        assert!(!dir.join("project-assets").join(id).exists(), "{id} assets");
        for s in ["db", "db-wal", "db-shm"] {
            assert!(
                !dir.join("rag").join(format!("{id}.{s}")).exists(),
                "{id}.{s}"
            );
        }
        assert!(
            server_projects::outbox::tombstoned(&dir, id)
                .unwrap()
                .is_some(),
            "{id} tombstone"
        );
    }
    assert_eq!(std::fs::read_to_string(&outside).unwrap(), "keep me");
    // The kept project's files are untouched.
    assert!(dir
        .join("project-uploads")
        .join(hex_sha256("keep"))
        .join("blob")
        .exists());
    assert!(dir.join("rag").join("keep.db").exists());
    // The chat is tombstoned and its transcript removed.
    assert!(!dir.join("history-c1.json").exists());
    let tombstones = std::fs::read_to_string(dir.join("deleted-chats.json")).unwrap();
    assert!(tombstones.contains("c1"));
    // The storage folder sweep is queued for core, once.
    let jobs = server_projects::outbox::jobs(&dir).unwrap();
    let sweeps: Vec<_> = jobs
        .iter()
        .filter(|j| j.kind == "sweep")
        .map(|j| j.project.clone())
        .collect();
    assert_eq!(sweeps, vec!["p1"]);
    // Again: no such project.
    let (status, _, text) = call(f.addr, "DELETE", "/api/projects/p1", &h, "").await;
    assert_eq!(
        (status, text.as_str()),
        (404, r#"{"error":"no such project"}"#)
    );
    // A bad escape is Node's URIError.
    let (status, _, _) = call(f.addr, "DELETE", "/api/projects/%E0%A4%A", &h, "").await;
    assert_eq!(status, 500);
}

#[tokio::test]
async fn delete_is_tenant_scoped_and_other_methods_stay_nodes() {
    let f = front().await;
    seed_full(&f);
    let (member_id, member) = insert_member_session(&f);
    std::fs::create_dir_all(f.data.join("users").join(&member_id)).unwrap();
    let mh = vec![
        ("cookie", member.as_str()),
        ("x-csrf-token", "synthetic-member-csrf-token"),
        ("origin", ORIGIN),
    ];
    let (status, _, text) = call(f.addr, "DELETE", "/api/projects/p1", &mh, "").await;
    assert_eq!(
        (status, text.as_str()),
        (404, r#"{"error":"no such project"}"#)
    );
    assert_eq!(
        projects(&f).as_array().unwrap().len(),
        3,
        "the owner's projects are untouched"
    );
    // Signed out, and without CSRF.
    let (status, _, _) = call(
        f.addr,
        "DELETE",
        "/api/projects/p1",
        &[("origin", ORIGIN)],
        "",
    )
    .await;
    assert_eq!(status, 401);
    let (status, _, _) = call(
        f.addr,
        "DELETE",
        "/api/projects/p1",
        &[("cookie", f.jar.as_str()), ("origin", ORIGIN)],
        "",
    )
    .await;
    assert_eq!(status, 403);
    assert_eq!(projects(&f).as_array().unwrap().len(), 3);
    let h = headers(&f);
    for (m, path) in [
        ("GET", "/api/projects"),
        ("PUT", "/api/projects/p1"),
        ("GET", "/api/projects/p1"),
        ("POST", "/api/projects/p1/config"),
    ] {
        let (status, _, _) = call(f.addr, m, path, &h, "").await;
        assert_eq!(status, 502, "{m} {path} goes to Node");
    }
}

/// #1914: a delete whose save fails answers the save failure and removes nothing.
#[tokio::test]
async fn a_failed_save_removes_nothing() {
    use std::os::unix::fs::PermissionsExt;
    let f = front().await;
    seed_full(&f);
    let dir = f.user_dir();
    // The projects.json lock and its temp file cannot be made in a read-only directory.
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o500)).unwrap();
    let (status, _, text) = call(f.addr, "DELETE", "/api/projects/p1", &headers(&f), "").await;
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
    assert_eq!(
        (status, text.as_str()),
        (
            500,
            r#"{"error":"could not delete the project; nothing was changed"}"#
        )
    );
    assert_eq!(projects(&f).as_array().unwrap().len(), 3);
    assert!(dir.join("rag").join("p1.db").exists());
    assert!(dir.join("history-c1.json").exists());
    assert!(dir
        .join("project-uploads")
        .join(hex_sha256("p1"))
        .join("blob")
        .exists());
}

#[test]
fn the_build_lists_rust_project_lifecycle() {
    assert!(noevia_server::FEATURES.contains(&"rust-project-lifecycle"));
}
