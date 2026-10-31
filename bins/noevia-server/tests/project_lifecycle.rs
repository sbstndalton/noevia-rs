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

#[path = "../../../crates/server-storage-reads/tests/support/dav_lab.rs"]
mod dav_lab;

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
    let mut app = noevia_server::App::new(Config::from_lookup(|k| env.get(k).cloned()).unwrap());
    // The test harness is not noevia-server: the owned storage child is the real binary.
    std::sync::Arc::get_mut(&mut app).unwrap().storage_child =
        Some(env!("CARGO_BIN_EXE_noevia-server").into());
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
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
    // The storage folder sweep was queued once and is the front's own job since round 3: with no
    // storage connected it removes nothing and is acknowledged (core afterDelete without a
    // connection), so it leaves the outbox; core never sees a sweep.
    let mut drained = false;
    for _ in 0..100 {
        let jobs = server_projects::outbox::jobs(&dir).unwrap();
        assert!(jobs.iter().all(|j| j.kind != "sweep" || j.project == "p1"));
        if jobs.iter().all(|j| j.kind != "sweep") {
            drained = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    assert!(drained, "the sweep job is run by the front");
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
        ("POST", "/api/projects/p1/browser"),
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
    // Round 3: the storage writes and the source file delete.
    assert!(noevia_server::FEATURES.contains(&"rust-project-storage"));
    assert!(noevia_server::FEATURES.contains(&"rust-project-files"));
}

/// The user's storage row: a synthetic WebDAV server (dav_lab), Basic `u:<secret>`.
fn connect_storage(f: &Front, port: u16, secret: &str) {
    let c = Connection::open(server_store::db_path(&f.data)).unwrap();
    c.execute(
        "INSERT INTO storage_connections(user_id,kind,base_url,bucket,region,username,secret,corpus_root,updated_at) VALUES(?1,'webdav',?2,'','us-east-1','u',?3,'Diary',1)",
        [f.user.clone(), format!("http://127.0.0.1:{port}/dav"), secret.to_owned()],
    )
    .unwrap();
}

/// #687 (round 3): with storage connected, a create-time text file is stored in the project's
/// folder (allocated now, attached as a source); a file that is not storable stays noevia's.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn create_with_storage_stores_its_text_files_in_the_project_folder() {
    if !cfg!(target_os = "linux") {
        eprintln!("skipped: the owned storage child runs only on Linux");
        return;
    }
    let f = front().await;
    publish_catalogue(&f);
    let (port, dav) = dav_lab::server();
    connect_storage(&f, port, "pw");
    std::fs::write(f.data.join("secrets.key"), vec![7u8; 32]).unwrap();
    let h = json_headers(&f);
    let body = r#"{"name":"Stored","files":[{"name":"notes.md","content":"hello storage"},{"name":"blob.bin","content":"x"},{"name":"sub/a.md","content":"y"}]}"#;
    let (status, _, text) = call(f.addr, "POST", "/api/projects", &h, body).await;
    assert_eq!(status, 200, "{text}");
    let made: serde_json::Value = serde_json::from_str(&text).unwrap();
    let id = made["id"].as_str().unwrap().to_string();
    let folder = format!("noevia projects/{id}");
    assert_eq!(made["reservedFolder"], folder.as_str());
    assert_eq!(made["projectFolder"], folder.as_str());
    assert_eq!(made["sourceFolders"], serde_json::json!([folder]));
    let full = format!("{folder}/Text/notes.md");
    assert_eq!(
        made["files"][0],
        serde_json::json!({"name": full, "attachment": {"id": hex_sha256("hello storage"), "bytes": 13, "group": "Text", "state": "ready"}, "source": folder, "textRef": hex_sha256("hello storage"), "chars": 13})
    );
    // Not storable: an Other-group file and a name with a folder stay plain.
    assert_eq!(made["files"][1]["name"], "blob.bin");
    assert_eq!(made["files"][2]["name"], "sub/a.md");
    assert!(made["files"][1].get("source").is_none());
    assert_eq!(projects(&f)[0], made);
    {
        let d = dav.lock().unwrap();
        assert_eq!(d.files.get(&full).unwrap().0, b"hello storage");
        assert!(d.dirs.contains("noevia projects") && d.dirs.contains(&folder));
        assert!(d.seen.iter().any(|(m, _)| m == "PUT"));
    }
    // The original is kept as an upload's is.
    let original = f
        .user_dir()
        .join("project-uploads")
        .join(hex_sha256(&id))
        .join(hex_sha256("hello storage"));
    assert_eq!(std::fs::read(original).unwrap(), b"hello storage");
    // The index jobs name the files as stored.
    let jobs = server_projects::outbox::jobs(&f.user_dir()).unwrap();
    let mut files: Vec<_> = jobs.iter().map(|j| j.file.clone().unwrap()).collect();
    files.sort();
    let mut want = vec![full.clone(), "blob.bin".to_string(), "sub/a.md".to_string()];
    want.sort();
    assert_eq!(files, want);

    // A second project: the reservation is its own folder.
    let (_, _, text) = call(
        f.addr,
        "POST",
        "/api/projects",
        &h,
        r#"{"name":"Two","files":[{"name":"b.txt","content":"b"}]}"#,
    )
    .await;
    let two: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert_ne!(two["projectFolder"], made["projectFolder"]);

    // Delete: the sweep runs in the front after the answer, removing only empty folders.
    dav.lock().unwrap().files.remove(&full);
    let (status, _, _) = call(f.addr, "DELETE", &format!("/api/projects/{id}"), &h, "").await;
    assert_eq!(status, 200);
    let mut swept = false;
    for _ in 0..100 {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        let gone = !dav.lock().unwrap().dirs.contains(&folder);
        let queued = server_projects::outbox::jobs(&f.user_dir())
            .unwrap()
            .iter()
            .any(|j| j.kind == "sweep");
        if gone && !queued {
            swept = true;
            break;
        }
    }
    assert!(
        swept,
        "the empty folder and its Text group are removed and the job acknowledged"
    );
    let d = dav.lock().unwrap();
    assert!(!d.dirs.contains(&format!("{folder}/Text")));
    assert!(
        d.dirs.contains("noevia projects"),
        "the root is never swept"
    );
    assert!(d.dirs.contains(two["projectFolder"].as_str().unwrap()));
}

/// A storage that refuses the saved login: the create still succeeds, its files stay noevia's
/// plain-named copies and no folder is attached (core ensureProjectFolder failing).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn create_keeps_its_files_when_storage_refuses() {
    if !cfg!(target_os = "linux") {
        eprintln!("skipped: the owned storage child runs only on Linux");
        return;
    }
    let f = front().await;
    publish_catalogue(&f);
    let (port, dav) = dav_lab::server();
    connect_storage(&f, port, "wrong");
    std::fs::write(f.data.join("secrets.key"), vec![7u8; 32]).unwrap();
    let h = json_headers(&f);
    let (status, _, text) = call(
        f.addr,
        "POST",
        "/api/projects",
        &h,
        r#"{"name":"R","files":[{"name":"n.md","content":"n"}]}"#,
    )
    .await;
    assert_eq!(status, 200, "{text}");
    let made: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert!(made.get("projectFolder").is_none());
    assert!(made["reservedFolder"].is_string());
    assert_eq!(made["files"][0]["name"], "n.md");
    assert!(!text.contains("wrong"));
    let d = dav.lock().unwrap();
    assert!(d.files.is_empty() && d.dirs.is_empty());
    assert_eq!(
        d.seen.len(),
        1,
        "one refused MKCOL, never retried: {:?}",
        d.seen
    );
}

/// sbstndalton/noevia#2191 round 3 (`rust-project-files`): DELETE /api/projects/{id}/files, Node's
/// order and answers, the storage delete through the owned child, then the list, the prune and the
/// RAG removal queued for core.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_source_file_delete_removes_it_from_storage_then_the_project() {
    if !cfg!(target_os = "linux") {
        eprintln!("skipped: the owned storage child runs only on Linux");
        return;
    }
    let f = front().await;
    publish_catalogue(&f);
    let h = json_headers(&f);
    // No browsable storage yet: Node's answers in Node's order.
    std::fs::write(
        f.user_dir().join("projects.json"),
        r#"{"projects":[{"id":"loc","name":"L","sourceFolders":["s"],"files":[],"chats":[]}]}"#,
    )
    .unwrap();
    let del = |id: &str| format!("/api/projects/{id}/files");
    for (path, body, want) in [
        (
            del("nope"),
            r#"{"path":"s/a.md"}"#,
            (404, r#"{"error":"no such project"}"#),
        ),
        (
            del("loc"),
            "null",
            (400, r#"{"error":"request body must be a JSON object"}"#),
        ),
        (del("loc"), "{", (400, r#"{"error":"invalid JSON"}"#)),
        (del("loc"), "", (400, r#"{"error":"a path is required"}"#)),
        (
            del("loc"),
            r#"{"path":0}"#,
            (400, r#"{"error":"a path is required"}"#),
        ),
        (
            del("loc"),
            r#"{"path":"s/../a.md"}"#,
            (
                400,
                r#"{"error":"That file is not in any folder attached to this project, so it cannot be deleted from here."}"#,
            ),
        ),
        (
            del("loc"),
            r#"{"path":"s/a.md"}"#,
            (400, r#"{"error":"no browsable storage connected"}"#),
        ),
        (del("%E0%A4%A"), "{}", (500, "")),
    ] {
        let (status, _, text) = call(f.addr, "DELETE", &path, &h, body).await;
        assert_eq!(status, want.0, "{path} {body}: {text}");
        if !want.1.is_empty() {
            assert_eq!(text, want.1, "{path} {body}");
        }
    }
    // Signed out and without the CSRF token: the gate's answers.
    let (status, _, _) = call(f.addr, "DELETE", &del("loc"), &[("origin", ORIGIN)], "{}").await;
    assert_eq!(status, 401);

    // With storage: a stored file, then its delete.
    let (port, dav) = dav_lab::server();
    connect_storage(&f, port, "pw");
    std::fs::write(f.data.join("secrets.key"), vec![7u8; 32]).unwrap();
    let (_, _, text) = call(f.addr, "POST", "/api/projects", &h, r#"{"name":"S","files":[{"name":"gone.md","content":"bye"},{"name":"keep.md","content":"stay"}]}"#).await;
    let made: serde_json::Value = serde_json::from_str(&text).unwrap();
    let id = made["id"].as_str().unwrap().to_string();
    let folder = made["projectFolder"].as_str().unwrap().to_string();
    let target = format!("{folder}/Text/gone.md");
    // A folder of its own is refused by storage-client's check, nothing removed.
    let (status, _, text) = call(
        f.addr,
        "DELETE",
        &del(&id),
        &h,
        &format!(r#"{{"path":"{folder}/Text"}}"#),
    )
    .await;
    assert_eq!(
        (status, text.as_str()),
        (
            400,
            format!(r#"{{"error":"refusing to delete \"{folder}/Text\": it is a folder"}}"#)
                .as_str()
        )
    );
    let (status, _, text) = call(
        f.addr,
        "DELETE",
        &del(&id),
        &h,
        &format!(r#"{{"path":"{target}"}}"#),
    )
    .await;
    assert_eq!(
        (status, text.as_str()),
        (200, format!(r#"{{"ok":true,"path":"{target}"}}"#).as_str())
    );
    assert!(!dav.lock().unwrap().files.contains_key(&target));
    assert!(dav
        .lock()
        .unwrap()
        .files
        .contains_key(&format!("{folder}/Text/keep.md")));
    let stored = projects(&f);
    let p = stored
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["id"] == id.as_str())
        .unwrap();
    let names: Vec<_> = p["files"]
        .as_array()
        .unwrap()
        .iter()
        .map(|x| x["name"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(names, vec![format!("{folder}/Text/keep.md")]);
    let uploads = f.user_dir().join("project-uploads").join(hex_sha256(&id));
    assert!(
        !uploads.join(hex_sha256("bye")).exists(),
        "its original is pruned"
    );
    assert!(uploads.join(hex_sha256("stay")).exists());
    let jobs = server_projects::outbox::jobs(&f.user_dir()).unwrap();
    assert!(jobs.iter().any(|j| j.kind == "unindex"
        && j.project == id
        && j.file.as_deref() == Some(target.as_str())));
    // Gone already: storage says missing, the list has nothing to drop: still Node's 200.
    let (status, _, _) = call(
        f.addr,
        "DELETE",
        &del(&id),
        &h,
        &format!(r#"{{"path":"{folder}/x.md"}}"#),
    )
    .await;
    assert_eq!(status, 200);
    // A refused login is core's storageLoginRejected (502), never the credential.
    let c = Connection::open(server_store::db_path(&f.data)).unwrap();
    c.execute("UPDATE storage_connections SET secret = 'wrong'", [])
        .unwrap();
    let (status, _, text) = call(
        f.addr,
        "DELETE",
        &del(&id),
        &h,
        &format!(r#"{{"path":"{folder}/Text/keep.md"}}"#),
    )
    .await;
    assert_eq!(status, 502, "{text}");
    assert!(text.contains(r#""code":"storageLoginRejected""#) && !text.contains("wrong"));
    assert!(dav
        .lock()
        .unwrap()
        .files
        .contains_key(&format!("{folder}/Text/keep.md")));
}

// ── sbstndalton/noevia#2191 round 4 (`rust-conversation-import`) ─────────────────────────────

const EXPORT_HEAD: &str = r#"{"format":"noevia-conversations-v1","chats":["#;

fn history_of(f: &Front, chat: &str) -> String {
    let now = server_projects::transcript::read(&f.data, &f.user, chat).unwrap();
    assert!(now.exists, "{chat} has a transcript");
    js_json::wtf16::stringify(&now.history).unwrap()
}

fn audits(f: &Front) -> Vec<String> {
    let c = Connection::open(server_store::db_path(&f.data)).unwrap();
    let mut q = c
        .prepare("SELECT detail FROM audit_events WHERE action='import.conversations' AND actor_user_id=?1 AND target_user_id=?1 ORDER BY id")
        .unwrap();
    q.query_map([&f.user], |r| r.get::<_, String>(0))
        .unwrap()
        .map(Result::unwrap)
        .collect()
}

fn free_ids(f: &Front) -> Vec<String> {
    let text = std::fs::read_to_string(f.user_dir().join("free-chats.json")).unwrap_or("[]".into());
    serde_json::from_str::<serde_json::Value>(&text)
        .unwrap()
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["id"].as_str().unwrap().to_string())
        .collect()
}

/// Node's answers and files, end to end: refusals before anything is written, free and project
/// chats (a project matched by name, another created through the round-2 create), the audit row,
/// an idempotent re-import, a pre-existing unlisted transcript never overwritten.
/// Failing before round 4: the POST went to Node (absent here: 502).
#[tokio::test]
async fn import_adds_chats_as_node_does_and_never_overwrites() {
    let f = front().await;
    publish_catalogue(&f);
    let dir = f.user_dir();
    std::fs::write(
        dir.join("projects.json"),
        r#"{"projects":[{"id":"p-work","name":" Work ","files":[],"chats":[{"id":"c-old","title":"Old","updatedAt":1}]},{"id":"cowork-chat-context-c9","name":"fresh","files":[],"chats":[]}]}"#,
    )
    .unwrap();
    // An unlisted transcript already holds c-taken (a chat whose list save never landed).
    server_projects::transcript::write(
        &f.data,
        &f.user,
        "c-taken",
        &[js_json::wtf16::parse(r#"{"role":"user","content":"mine"}"#).unwrap()],
        &server_projects::transcript::Base::Any,
        None,
    )
    .unwrap();
    let h = json_headers(&f);
    let post = |body: String| {
        let h = h.clone();
        async move { call(f.addr, "POST", "/api/import/conversations", &h, &body).await }
    };
    let not_export =
        r#"{"error":"Choose a conversations.json from a noevia conversations export."}"#;
    for body in ["{", "[]", r#"{"format":"x","chats":[]}"#] {
        let (status, _, text) = post(body.into()).await;
        assert_eq!((status, text.as_str()), (400, not_export), "{body}");
    }
    assert!(
        !dir.join("conversation-imports").exists(),
        "a refusal writes nothing"
    );
    let export = format!(
        "{EXPORT_HEAD}{}]}}",
        [
            r#"{"id":"c-old","title":"Old"}"#,
            r#"{"id":"c-free","title":"Free one","updatedAt":5,"history":[{"role":"user","content":"hi","reasoning":"secret"},{"role":"assistant","content":"yo","failure":" broke ","toolCalls":[{"name":"web","args":{}}]}]}"#,
            r#"{"id":"c-work","title":"In work","updatedAt":6,"project":{"name":"work"},"history":[{"role":"user","content":"w"}]}"#,
            r#"{"id":"c-new","title":"","updatedAt":7,"project":{"name":"Fresh"},"history":[]}"#,
            r#"{"id":"c-taken","title":"Taken","updatedAt":8,"history":[{"role":"user","content":"theirs"}]}"#,
        ]
        .join(",")
    );
    let (status, _, text) = post(export.clone()).await;
    assert_eq!(
        (status, text.as_str()),
        (
            200,
            r#"{"imported":4,"skipped":[{"title":"Old","reason":"already in this account"}],"projectsCreated":1}"#
        )
    );
    assert_eq!(
        history_of(&f, "c-free"),
        // The store bounds it as chat-sources.cjs sanitizeHistory does (`failure` last), the
        // same transcript Node's import published through it.
        r#"[{"role":"user","content":"hi"},{"role":"assistant","content":"yo","toolCalls":[{"name":"web","status":"done"}],"failure":"broke"}]"#
    );
    assert_eq!(
        history_of(&f, "c-taken"),
        r#"[{"role":"user","content":"mine"}]"#,
        "never overwritten"
    );
    let free = free_ids(&f);
    assert!(
        free.contains(&"c-free".to_string()) && !free.contains(&"c-taken".to_string()),
        "{free:?}"
    );
    let renamed = free
        .iter()
        .find(|id| id.starts_with("c-") && *id != "c-free")
        .unwrap()
        .clone();
    assert_eq!(
        history_of(&f, &renamed),
        r#"[{"role":"user","content":"theirs"}]"#
    );
    let all = projects(&f);
    let work = all
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["id"] == "p-work")
        .unwrap();
    let ids: Vec<&str> = work["chats"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, ["c-work", "c-old"]);
    // "Fresh" is a free chat's context project (internal): a new project is made, not matched.
    let fresh = all
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["name"] == "Fresh")
        .unwrap();
    assert!(fresh["id"].as_str().unwrap().starts_with("proj-"));
    assert_eq!(fresh["chats"][0]["id"], "c-new");
    assert_eq!(fresh["toolboxes"], serde_json::json!([]));
    assert_eq!(
        std::fs::read_dir(dir.join("conversation-imports"))
            .unwrap()
            .count(),
        0
    );
    assert_eq!(
        audits(&f),
        [r#"{"imported":4,"skipped":1,"projectsCreated":1}"#]
    );
    // sbstndalton/noevia#2196: the same file again adds nothing, not even the chat whose id was
    // taken (it went in under a new id; the ledger knows which) and nothing stored changes.
    let stored = |file: &str| std::fs::read(dir.join(file)).unwrap_or_default();
    let before = (stored("projects.json"), stored("free-chats.json"));
    let (status, _, text) = post(export).await;
    assert_eq!(status, 200);
    let again: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert_eq!(
        (
            again["imported"].as_u64(),
            again["skipped"].as_array().unwrap().len(),
            again["projectsCreated"].as_u64()
        ),
        (Some(0), 5, Some(0))
    );
    assert_eq!(audits(&f).len(), 2);
    assert_eq!(before, (stored("projects.json"), stored("free-chats.json")));
}

/// sbstndalton/noevia#2196: importing twice leaves one copy; a changed chat keeps the existing copy
/// and says so; a chat without an id is known by its content (changed content is a new chat); a
/// deleted id comes back once; a chat the user then deletes can be restored.
#[tokio::test]
async fn import_twice_leaves_one_copy() {
    let f = front().await;
    publish_catalogue(&f);
    let dir = f.user_dir();
    std::fs::write(dir.join("deleted-chats.json"), "[]").unwrap();
    let h = json_headers(&f);
    let post = |body: String| {
        let h = h.clone();
        async move {
            let (status, _, text) =
                call(f.addr, "POST", "/api/import/conversations", &h, &body).await;
            assert_eq!(status, 200, "{text}");
            serde_json::from_str::<serde_json::Value>(&text).unwrap()
        }
    };
    let listed = || -> Vec<String> {
        let text = std::fs::read_to_string(dir.join("free-chats.json")).unwrap();
        let list: serde_json::Value = serde_json::from_str(&text).unwrap();
        let mut ids: Vec<String> = list
            .as_array()
            .unwrap()
            .iter()
            .map(|c| c["id"].as_str().unwrap().to_string())
            .collect();
        ids.sort();
        ids
    };
    let chat = |id: &str, content: &str| {
        format!(
            r#"{{{id}"title":"T","updatedAt":5,"history":[{{"role":"user","content":"{content}"}}]}}"#
        )
    };
    let export = |chats: &[String]| format!("{EXPORT_HEAD}{}]}}", chats.join(","));
    let first = export(&[chat(r#""id":"c-one","#, "hello"), chat("", "no id")]);
    let got = post(first.clone()).await;
    assert_eq!(got["imported"], 2);
    let ids = listed();
    assert_eq!(ids.len(), 2);

    let got = post(first.clone()).await;
    assert_eq!(
        (
            got["imported"].as_u64(),
            got["skipped"].as_array().unwrap().len()
        ),
        (Some(0), 2)
    );
    assert_eq!(listed(), ids);

    // Changed under the same id: the existing chat stays as it was, and the answer says so.
    let revision = || {
        server_projects::transcript::read(&f.data, &f.user, "c-one")
            .unwrap()
            .revision
    };
    let saved = revision();
    let got = post(export(&[chat(r#""id":"c-one","#, "edited")])).await;
    assert_eq!(got["imported"], 0);
    assert!(got["skipped"][0]["reason"]
        .as_str()
        .unwrap()
        .contains("differs"));
    assert_eq!(listed(), ids);
    assert_eq!(saved, revision());
    // No id, other content: nothing says it is the same chat, so it is a new one, once.
    let edited = export(&[chat("", "no id, edited")]);
    assert_eq!(post(edited.clone()).await["imported"], 1);
    assert_eq!(post(edited).await["imported"], 0);
    assert_eq!(listed().len(), 3);

    // The ledger holds ids and hashes, none of the chat's words.
    let ledger = std::fs::read_to_string(dir.join("imported-chats.json")).unwrap();
    assert!(!ledger.contains("hello") && ledger.contains("id:c-one"));

    // A tombstoned id imports under a new id, once; deleting that copy allows one restore.
    std::fs::write(dir.join("deleted-chats.json"), r#"["c-gone"]"#).unwrap();
    let gone = export(&[chat(r#""id":"c-gone","#, "bye")]);
    assert_eq!(post(gone.clone()).await["imported"], 1);
    assert_eq!(post(gone.clone()).await["imported"], 0);
    let list = std::fs::read_to_string(dir.join("free-chats.json")).unwrap();
    let mut list: serde_json::Value = serde_json::from_str(&list).unwrap();
    list.as_array_mut()
        .unwrap()
        .retain(|c| c["title"] != "T" || ids.contains(&c["id"].as_str().unwrap().to_string()));
    std::fs::write(dir.join("free-chats.json"), list.to_string()).unwrap();
    assert_eq!(post(gone).await["imported"], 1);
}

/// A journal Node left (crashed after staging, before its lists) is finished by the next import,
/// file for file as conversation-import.cjs wrote it; a deleted chat is reported, not imported.
#[tokio::test]
async fn import_finishes_a_journal_node_left() {
    let f = front().await;
    publish_catalogue(&f);
    let dir = f.user_dir();
    std::fs::write(dir.join("projects.json"), r#"{"projects":[]}"#).unwrap();
    std::fs::write(dir.join("deleted-chats.json"), r#"["c-gone"]"#).unwrap();
    let journal = dir.join("conversation-imports").join("a".repeat(64));
    std::fs::create_dir_all(&journal).unwrap();
    std::fs::write(
        journal.join("0.json"),
        r#"{"history":[{"role":"user","content":"left"}]}"#,
    )
    .unwrap();
    std::fs::write(journal.join("1.json"), r#"{"history":[]}"#).unwrap();
    std::fs::write(
        journal.join("manifest.json"),
        r#"{"version":1,"plan":{"freeChats":[{"id":"c-left","title":"Left","updatedAt":3},{"id":"c-gone","title":"Gone","updatedAt":2}],"projectChats":[],"skipped":[],"imported":2},"files":[{"id":"c-left","stage":"0.json"},{"id":"c-gone","stage":"1.json"}],"projectsCreated":0}"#,
    )
    .unwrap();
    // A staging directory without a manifest is discarded.
    std::fs::create_dir_all(dir.join("conversation-imports").join("b".repeat(64))).unwrap();
    let (status, _, text) = call(
        f.addr,
        "POST",
        "/api/import/conversations",
        &json_headers(&f),
        &format!("{EXPORT_HEAD}]}}"),
    )
    .await;
    assert_eq!(
        (status, text.as_str()),
        (200, r#"{"imported":0,"skipped":[],"projectsCreated":0}"#)
    );
    assert_eq!(free_ids(&f), ["c-left"]);
    assert_eq!(
        history_of(&f, "c-left"),
        r#"[{"role":"user","content":"left"}]"#
    );
    assert_eq!(
        std::fs::read_dir(dir.join("conversation-imports"))
            .unwrap()
            .count(),
        0
    );
    assert_eq!(
        audits(&f),
        [
            r#"{"imported":1,"skipped":1,"projectsCreated":0}"#,
            r#"{"imported":0,"skipped":0,"projectsCreated":0}"#
        ]
    );
}

/// Signed out 401; another account's import lands in its own lists only; other methods are Node's.
#[tokio::test]
async fn import_is_tenant_scoped_and_post_only() {
    let f = front().await;
    publish_catalogue(&f);
    std::fs::write(f.user_dir().join("projects.json"), r#"{"projects":[]}"#).unwrap();
    let body = format!(r#"{EXPORT_HEAD}{{"id":"c-m","title":"M","history":[]}}]}}"#);
    let (status, _, _) = call(
        f.addr,
        "POST",
        "/api/import/conversations",
        &[("origin", ORIGIN)],
        &body,
    )
    .await;
    assert_eq!(status, 401);
    let (member, cookie) = insert_member_session(&f);
    std::fs::create_dir_all(f.data.join("users").join(&member)).unwrap();
    let csrf = "synthetic-member-csrf-token";
    let mh = [
        ("cookie", cookie.as_str()),
        ("x-csrf-token", csrf),
        ("origin", ORIGIN),
        ("content-type", "application/json"),
    ];
    let (status, _, text) = call(f.addr, "POST", "/api/import/conversations", &mh, &body).await;
    assert_eq!(
        (status, text.as_str()),
        (200, r#"{"imported":1,"skipped":[],"projectsCreated":0}"#)
    );
    assert!(f
        .data
        .join("users")
        .join(&member)
        .join("free-chats.json")
        .exists());
    assert!(free_ids(&f).is_empty(), "the admin's lists are untouched");
    for m in ["GET", "PUT"] {
        let (status, _, _) = call(f.addr, m, "/api/import/conversations", &headers(&f), "").await;
        assert_eq!(status, 502, "{m} goes to Node");
    }
    assert!(noevia_server::FEATURES.contains(&"rust-conversation-import"));
}
