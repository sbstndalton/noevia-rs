//! sbstndalton/noevia#2191 round 8 `rust-project-source-work`: the front runs the project source
//! operations itself, in the foreground and as the source jobs it owns (round 6). Through the whole
//! front, with no Node behind it (its upstream is a closed port): a background upload and a background sync
//! are started here, run in this process, their progress and result written to `jobs/<uuid>.jsonl` and read
//! back by the poll in core's words; the synchronous forms answer with the result; a refusal before the job
//! exists is a plain answer and starts nothing; the folder-import plan answers. Synthetic data only.
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
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

const SCHEMA: &str = include_str!("../../../crates/server-store/tests/fixtures/node-schema.sql");
const CODE: &str = "synthetic-source-jobs-code";
const ORIGIN: &str = "http://127.0.0.1:18021";
const START: &str = "0123456789abcdef0123456789abcdef";

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
    // Nothing listens here: a request that reached Node would fail, so none does.
    env.insert("NOEVIA_LEGACY_UPSTREAM", "http://127.0.0.1:9".to_string());
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

fn file_names(f: &Front) -> Vec<String> {
    let text =
        std::fs::read_to_string(f.data.join("users").join(&f.user).join("projects.json")).unwrap();
    let v: serde_json::Value = serde_json::from_str(&text).unwrap();
    v["projects"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["id"] == "p 1")
        .unwrap()["files"]
        .as_array()
        .unwrap()
        .iter()
        .map(|x| x["name"].as_str().unwrap().to_owned())
        .collect()
}

const UPLOAD: &str = r#"{"name":"notes.txt","dataBase64":"YWJj","organized":true}"#;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_background_upload_runs_in_the_front_and_its_job_reads_back_in_cores_words() {
    let f = front().await;
    let h = headers(&f);
    // The project 404, the gate and the body come first: no job exists for them.
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
    let (status, body) = call(
        f.addr,
        "POST",
        "/api/projects/p%201/upload?background=1",
        &h,
        "{nope",
    )
    .await;
    assert_eq!(
        (status, body.as_str()),
        (400, r#"{"error":"invalid JSON"}"#)
    );
    assert!(journals(&f).is_empty(), "a refusal leaves no job");

    // Accepted: 202 { poll }; the operation runs here and its result is the route's answer.
    let (status, body) = call(
        f.addr,
        "POST",
        "/api/projects/p%201/upload?background=1",
        &h,
        UPLOAD,
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
    let done = poll_until_done(&f, &poll).await;
    assert_eq!(done["status"], 200, "{done}");
    assert_eq!(done["body"]["name"], "notes.txt");
    assert_eq!(done["body"]["path"], "notes.txt");
    assert_eq!(done["body"]["bytes"], 3);
    assert_eq!(file_names(&f), vec!["notes.txt"]);
    // The journal: created, started, the stage the operation reported, the terminal row.
    let journal = journals(&f);
    assert_eq!(journal.len(), 1);
    assert!(journal[0].contains("job.completed"), "{}", journal[0]);
    // The operation was listed for core while it ran, and ended ready.
    let ops = std::fs::read_to_string(
        f.data
            .join("users")
            .join(&f.user)
            .join("source-file-ops.json"),
    )
    .unwrap();
    assert!(
        ops.contains(r#""name":"notes.txt""#) && ops.contains(r#""state":"ready""#),
        "{ops}"
    );
    // The same name again is the route's own 409, as the job's result (status 409, not a failed job).
    let (_, body) = call(
        f.addr,
        "POST",
        "/api/projects/p%201/upload?background=1",
        &h,
        UPLOAD,
    )
    .await;
    let poll = serde_json::from_str::<serde_json::Value>(&body).unwrap()["poll"]
        .as_str()
        .unwrap()
        .to_owned();
    let again = poll_until_done(&f, &poll).await;
    assert_eq!(again["status"], 409);
    assert_eq!(again["body"]["code"], "sourceExists");
    // Another project's job is not readable through this one.
    let other = poll.replace("p%201", "p2");
    let get = [("cookie", f.jar.as_str())];
    let (status, _) = call(f.addr, "GET", &other, &get, "").await;
    assert_eq!(status, 404);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_synchronous_forms_answer_with_their_result_and_a_background_sync_is_a_job() {
    let f = front().await;
    let h = headers(&f);
    // The foreground upload: the result is the answer, a 202 never.
    let (status, body) = call(f.addr, "POST", "/api/projects/p%201/upload", &h, UPLOAD).await;
    assert_eq!(status, 200, "{body}");
    let v: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(
        (v["name"].as_str(), v["bytes"].as_u64()),
        (Some("notes.txt"), Some(3))
    );
    // The older form, without `organized`.
    let (status, body) = call(
        f.addr,
        "POST",
        "/api/projects/p%201/upload",
        &h,
        r#"{"name":"older.md","dataBase64":"YWJj"}"#,
    )
    .await;
    assert_eq!(status, 200, "{body}");
    // A sync of a project with no attached folders: its files are the list, nothing skipped.
    let (status, body) = call(f.addr, "POST", "/api/projects/p%201/sources/sync", &h, "{}").await;
    assert_eq!(status, 200, "{body}");
    let v: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["files"].as_array().unwrap().len(), 2);
    assert_eq!(v["skipped"], serde_json::json!([]));
    // And as a job; an unknown project is a plain 404 for both forms.
    let (status, body) = call(
        f.addr,
        "POST",
        "/api/projects/p%201/sources/sync?background=1",
        &h,
        "{}",
    )
    .await;
    assert_eq!(status, 202, "{body}");
    let poll = serde_json::from_str::<serde_json::Value>(&body).unwrap()["poll"]
        .as_str()
        .unwrap()
        .to_owned();
    let done = poll_until_done(&f, &poll).await;
    assert_eq!(done["status"], 200);
    assert_eq!(done["body"]["files"].as_array().unwrap().len(), 2);
    let (status, body) = call(f.addr, "POST", "/api/projects/ghost/sources/sync", &h, "{}").await;
    assert_eq!(
        (status, body.as_str()),
        (404, r#"{"error":"no such project"}"#)
    );
    // Sources of a folder that cannot be read (no storage connected) are refused as core refused them.
    let udir = f.data.join("users").join(&f.user);
    std::fs::write(
        udir.join("projects.json"),
        r#"{"projects":[{"id":"p 1","name":"Synthetic","files":[],"chats":[],"sourceFolders":["docs"]}]}"#,
    )
    .unwrap();
    let (status, body) = call(f.addr, "POST", "/api/projects/p%201/sources/sync", &h, "{}").await;
    assert_eq!(
        (status, body.as_str()),
        (
            400,
            r#"{"error":"no browsable storage connection is configured"}"#
        )
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_folder_import_plan_is_the_fronts_and_opens_a_ticket() {
    let f = front().await;
    let h = headers(&f);
    let plan = r#"{"entries":[{"path":"docs/a.md","size":3},{"path":".git/config","size":2}]}"#;
    let (status, body) = call(
        f.addr,
        "POST",
        "/api/projects/p%201/folder-import/plan",
        &h,
        plan,
    )
    .await;
    assert_eq!(status, 200, "{body}");
    let v: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["accepted"][0]["path"], "docs/a.md");
    assert_eq!(v["accepted"][0]["change"], "new");
    assert_eq!(v["skipped"][0]["code"], "system");
    let id = v["importId"].as_str().unwrap();
    assert_eq!(id.len(), 32);
    // The ticket admits the planned path under its id, and only that.
    let up = |rel: &str| {
        format!(
            r#"{{"name":"x.md","organized":true,"dataBase64":"YWJj","relativePath":"{rel}","importId":"{id}"}}"#
        )
    };
    let (status, body) = call(
        f.addr,
        "POST",
        "/api/projects/p%201/upload",
        &h,
        &up("docs/b.md"),
    )
    .await;
    assert_eq!(
        (status, body.contains("importNotPlanned")),
        (409, true),
        "{body}"
    );
    let (status, body) = call(
        f.addr,
        "POST",
        "/api/projects/p%201/upload",
        &h,
        &up("docs/a.md"),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    let (status, body) = call(
        f.addr,
        "POST",
        "/api/projects/ghost/folder-import/plan",
        &h,
        plan,
    )
    .await;
    assert_eq!(
        (status, body.as_str()),
        (404, r#"{"error":"no such project"}"#)
    );
    let (status, body) = call(
        f.addr,
        "POST",
        "/api/projects/p%201/folder-import/plan",
        &h,
        r#"{"entries":"no"}"#,
    )
    .await;
    assert_eq!(
        (status, body.as_str()),
        (400, r#"{"error":"entries must be a list"}"#)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_client_that_leaves_cannot_strand_the_job_and_the_slots_are_freed() {
    let f = front().await;
    let h = headers(&f);
    // The client sends the start and goes away without reading the answer.
    for n in 0..6 {
        let body = format!(r#"{{"name":"gone{n}.txt","dataBase64":"YWJj","organized":true}}"#);
        let mut s = TcpStream::connect(f.addr).await.unwrap();
        let mut req = format!(
            "POST /api/projects/p%201/upload?background=1 HTTP/1.1\r\nhost: front.test\r\nconnection: close\r\ncontent-length: {}\r\n",
            body.len()
        );
        for (k, v) in &h {
            req.push_str(&format!("{k}: {v}\r\n"));
        }
        req.push_str("\r\n");
        req.push_str(&body);
        s.write_all(req.as_bytes()).await.unwrap();
        drop(s);
        // The two-active cap is 429 while jobs run; these finish at once, so each start is accepted or,
        // while two are still running, told to wait: either way nothing stays active for good.
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
    for _ in 0..100 {
        let names = file_names(&f);
        if names.len() >= 2 && journals(&f).iter().all(|j| j.contains("job.completed")) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(
        journals(&f).iter().all(|j| j.contains("job.completed")),
        "{:?}",
        journals(&f)
    );
    let (status, body) = call(
        f.addr,
        "POST",
        "/api/projects/p%201/upload?background=1",
        &h,
        UPLOAD,
    )
    .await;
    assert_eq!(status, 202, "a slot is free again: {body}");
}

fn listed_states(f: &Front) -> String {
    std::fs::read_to_string(
        f.data
            .join("users")
            .join(&f.user)
            .join("source-file-ops.json"),
    )
    .unwrap_or_default()
}

/// Re-review N1: a client that disconnects from a synchronous sync leaves the operation running, and its
/// listing says so (running, then ready), instead of failed while the work goes on.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_client_that_leaves_a_foreground_sync_does_not_fail_its_listing_while_it_runs() {
    let f = front().await;
    let h = headers(&f);
    // The project's source lock is held, so the sync waits (its foreground wait is 30 s) once it is listed.
    let lock = server_projects::project_documents::source_lock(&f.user, "p 1");
    let held = lock.lock().unwrap();
    let mut s = TcpStream::connect(f.addr).await.unwrap();
    let mut req = String::from("POST /api/projects/p%201/sources/sync HTTP/1.1\r\nhost: front.test\r\nconnection: close\r\ncontent-length: 2\r\n");
    for (k, v) in &h {
        req.push_str(&format!("{k}: {v}\r\n"));
    }
    req.push_str("\r\n{}");
    s.write_all(req.as_bytes()).await.unwrap();
    for _ in 0..100 {
        if listed_states(&f).contains(r#""state":"running""#) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(listed_states(&f).contains(r#""state":"running""#));
    drop(s);
    tokio::time::sleep(Duration::from_millis(500)).await;
    let while_running = listed_states(&f);
    assert!(
        while_running.contains(r#""state":"running""#) && !while_running.contains("failed"),
        "{while_running}"
    );
    drop(held);
    for _ in 0..200 {
        if listed_states(&f).contains(r#""state":"ready""#) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let after = listed_states(&f);
    assert!(
        after.contains(r#""state":"ready""#) && !after.contains("failed"),
        "{after}"
    );
}

/// A second person's session: the owner's projects are not theirs, whichever source route they use.
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
    std::fs::create_dir_all(f.data.join("users").join(user)).unwrap();
    (
        user.to_string(),
        format!("cowork_session={token}; cowork_csrf={csrf}"),
    )
}

/// Two users: a project id of the owner's is a 404 for the other person on the upload, the sync (both
/// forms) and the folder-import plan, and nothing of the owner's changes or is written for them. (The
/// OCR-front harness holds the same check against a separate front process.)
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn another_persons_project_is_a_404_on_every_source_work_route() {
    let f = front().await;
    let (member, cookie) = insert_member_session(&f);
    let h = [
        ("cookie", cookie.as_str()),
        ("x-csrf-token", "synthetic-member-csrf-token"),
        ("origin", ORIGIN),
        ("content-type", "application/json"),
    ];
    let mine = headers(&f);
    for (path, body, expected) in [
        ("/api/projects/p%201/upload", UPLOAD, "no such project"),
        (
            "/api/projects/p%201/upload?background=1",
            UPLOAD,
            "project not found",
        ),
        ("/api/projects/p%201/sources/sync", "{}", "no such project"),
        (
            "/api/projects/p%201/sources/sync?background=1",
            "{}",
            "project not found",
        ),
        (
            "/api/projects/p%201/folder-import/plan",
            r#"{"entries":[{"path":"a.md","size":1}]}"#,
            "no such project",
        ),
    ] {
        let (status, text) = call(f.addr, "POST", path, &h, body).await;
        assert_eq!(status, 404, "{path}: {text}");
        assert!(text.contains(expected), "{path}: {text}");
    }
    assert!(
        file_names(&f).is_empty(),
        "the owner's project was not touched"
    );
    assert!(journals(&f).is_empty());
    let theirs = f.data.join("users").join(&member);
    assert!(
        !theirs.join("jobs").exists(),
        "no job was made for the member"
    );
    assert!(!theirs.join("project-uploads").exists() && !theirs.join("projects.json").exists());
    // Whatever was listed for them (in their own folder) is over.
    let ops = std::fs::read_to_string(theirs.join("source-file-ops.json")).unwrap_or_default();
    assert!(!ops.contains(r#""state":"running""#), "{ops}");
    // And the owner's own request still works.
    let (status, _) = call(f.addr, "POST", "/api/projects/p%201/upload", &mine, UPLOAD).await;
    assert_eq!(status, 200);
}

/// A saved storage login the front cannot open (its credential key is missing) is not "no storage": a sync
/// of a project with attached folders says so (503), where it used to answer as if the person had no
/// storage connected (400), and an upload does not quietly keep the file only in noevia.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_saved_storage_login_that_cannot_be_read_is_a_503_not_no_storage() {
    let f = front().await;
    let h = headers(&f);
    let c = Connection::open(server_store::db_path(&f.data)).unwrap();
    c.execute(
        "INSERT INTO storage_connections(user_id,kind,base_url,bucket,region,username,secret,corpus_root,updated_at) VALUES(?1,'webdav','http://127.0.0.1:9/dav','','us-east-1','u','not-an-envelope','',1)",
        [&f.user],
    )
    .unwrap();
    let udir = f.data.join("users").join(&f.user);
    std::fs::write(
        udir.join("projects.json"),
        r#"{"projects":[{"id":"p 1","name":"Synthetic","files":[],"chats":[],"sourceFolders":["docs"]}]}"#,
    )
    .unwrap();
    let (status, body) = call(f.addr, "POST", "/api/projects/p%201/sources/sync", &h, "{}").await;
    assert_eq!(status, 503, "{body}");
    assert!(body.contains("could not be read"), "{body}");
    let (status, body) = call(
        f.addr,
        "POST",
        "/api/projects/p%201/sources/sync?background=1",
        &h,
        "{}",
    )
    .await;
    assert_eq!(status, 202, "{body}");
    let poll = serde_json::from_str::<serde_json::Value>(&body).unwrap()["poll"]
        .as_str()
        .unwrap()
        .to_owned();
    let done = poll_until_done(&f, &poll).await;
    assert_eq!(done["status"], 503, "{done}");
}

#[test]
fn the_front_names_the_capability_beside_the_source_jobs() {
    assert!(noevia_server::FEATURES.contains(&"rust-project-source-work"));
    assert!(noevia_server::FEATURES.contains(&"rust-source-jobs"));
}
