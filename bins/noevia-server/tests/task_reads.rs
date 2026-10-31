//! sbstndalton/noevia#2257 `rust-task-reads`: the Code and Browser task reads through the whole front, with a
//! synthetic Node answering the loopback facts call. Plain TCP on 127.0.0.1; no model, no task runs.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]

use noevia_server::config::Config;
use noevia_server::serve::{serve, Limits};
use serde_json::{json, Value};
use server_store::rusqlite::Connection;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

const SCHEMA: &str = include_str!("../../../crates/server-store/tests/fixtures/node-schema.sql");
const CODE: &str = "synthetic-task-code";
const ORIGIN: &str = "http://127.0.0.1:18021";
const STORE_TOKEN: &str = "synthetic-store-token-0123456789abcdef";
const START: &str = "0123456789abcdef0123456789abcdef";
const TASK: &str = "11111111-1111-4111-8111-111111111111";
const OTHER: &str = "22222222-2222-4222-8222-222222222222";
const DONE: &str = "33333333-3333-4333-8333-333333333333";

type Answer = Arc<Mutex<(u16, String)>>;

struct Node {
    addr: SocketAddr,
    answer: Answer,
    seen: Arc<Mutex<Vec<String>>>,
}

async fn node() -> Node {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let answer: Answer = Arc::new(Mutex::new((200, facts().to_string())));
    let seen = Arc::new(Mutex::new(Vec::new()));
    let (a, s) = (Arc::clone(&answer), Arc::clone(&seen));
    tokio::spawn(async move {
        while let Ok((mut c, _)) = listener.accept().await {
            let (a, s) = (Arc::clone(&a), Arc::clone(&s));
            tokio::spawn(async move {
                let mut buf = Vec::new();
                let mut chunk = [0u8; 8192];
                loop {
                    let n = c.read(&mut chunk).await.unwrap_or(0);
                    if n == 0 {
                        return;
                    }
                    buf.extend_from_slice(&chunk[..n]);
                    let text = String::from_utf8_lossy(&buf).into_owned();
                    if let Some((h, b)) = text.split_once("\r\n\r\n") {
                        let want = h
                            .lines()
                            .find_map(|l| {
                                l.to_ascii_lowercase()
                                    .strip_prefix("content-length: ")
                                    .map(|v| v.parse::<usize>().unwrap())
                            })
                            .unwrap_or(0);
                        if b.len() >= want {
                            break;
                        }
                    }
                }
                s.lock()
                    .unwrap()
                    .push(String::from_utf8_lossy(&buf).to_ascii_lowercase());
                let (status, text) = a.lock().unwrap().clone();
                let out = format!("HTTP/1.1 {status} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{text}", text.len());
                let _ = c.write_all(out.as_bytes()).await;
            });
        }
    });
    Node { addr, answer, seen }
}

fn facts() -> Value {
    json!({"enabled": true, "facts": {
        "repositories": [{"id":"repo"}], "capabilities": ["read_repository"], "defaultCapabilities": ["read_repository"],
        "harnesses": [{"id":"pi","label":"pi","version":null}], "promptPreparation": [{"id":"direct"}], "sandboxed": true,
        "network": false, "networkState": "unavailable",
        "approvals": {TASK: {"id":"card-1","action":"edit_file","taskId":TASK}, OTHER: {"id":"card-2","action":"nope"}},
    }})
}

struct Front {
    _dir: tempfile::TempDir,
    data: PathBuf,
    addr: SocketAddr,
    jar: String,
    csrf: String,
    user: String,
    node: Node,
}

async fn front(with_node: bool) -> Front {
    let node = node().await;
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
    let upstream = if with_node {
        format!("http://{}", node.addr)
    } else {
        "http://127.0.0.1:9".to_owned()
    };
    env.insert("NOEVIA_LEGACY_UPSTREAM", upstream);
    env.insert("UI_PORT", "1".into());
    env.insert("UI_DATA_DIR", data.display().to_string());
    env.insert("NOEVIA_WEB_DIST", data.join("dist").display().to_string());
    env.insert("PUBLIC_ORIGIN", ORIGIN.into());
    env.insert("NOEVIA_START_ID", START.into());
    env.insert("NOEVIA_FEATURE_NATIVE_CLIENT_AUTH", "true".into());
    env.insert("TRUST_PROXY", "true".into());
    env.insert("NOEVIA_PROJECT_STORE_TOKEN", STORE_TOKEN.into());
    let mut app = noevia_server::App::new(Config::from_lookup(|k| env.get(k).cloned()).unwrap());
    Arc::get_mut(&mut app).unwrap().storage_child =
        Some(env!("CARGO_BIN_EXE_noevia-server").into());
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(serve(
        listener,
        app,
        Limits::default(),
        std::future::pending(),
    ));
    let setup = format!(
        r#"{{"setupCode":"{CODE}","username":"owner","password":"synthetic task password","diaryEnabled":false}}"#
    );
    let mut s = TcpStream::connect(addr).await.unwrap();
    let req = format!("POST /api/setup/complete HTTP/1.1\r\nhost: front.test\r\nconnection: close\r\norigin: {ORIGIN}\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n{setup}", setup.len());
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
    let f = Front {
        _dir: dir,
        data,
        addr,
        jar,
        csrf,
        user,
        node,
    };
    let home = f.data.join("users").join(&f.user);
    std::fs::create_dir_all(home.join("jobs")).unwrap();
    let project = |id: &str, name: &str| {
        format!(
            r#"{{"id":"{id}","name":"{name}","goal":"g","instructions":"i","pinned":false,"archived":false,"sourceFolders":[],"memories":[],"files":[],"routing":"auto","modes":["chat"],"toolboxes":["core"],"toolsMode":"auto","chats":[],"createdAt":1,"updatedAt":2}}"#
        )
    };
    std::fs::write(
        home.join("projects.json"),
        format!(
            r#"{{"projects":[{},{},{}]}}"#,
            project("p1", "Alpha"),
            project("p2", "Beta"),
            project("cowork-diary-extras", "Internal")
        ),
    )
    .unwrap();
    f.journal(TASK, "code", "p1", &["job.started", "approval.requested"]);
    f.journal(OTHER, "code", "cowork-diary-extras", &["job.started"]);
    f.journal(DONE, "code", "p2", &["job.started", "job.completed"]);
    f
}

impl Front {
    fn journal(&self, id: &str, kind: &str, project: &str, events: &[&str]) {
        let mut text = format!(
            r#"{{"job":"{id}","seq":1,"type":"job.created","at":1000,"data":{{"kind":"{kind}","projectId":"{project}","capabilities":["read_repository"]}}}}"#
        );
        text.push('\n');
        text.push_str(&format!(r#"{{"job":"{id}","seq":2,"type":"checkpoint.created","at":1001,"data":{{"task":"Synthetic task","domains":["x.test"]}}}}"#));
        text.push('\n');
        for (i, e) in events.iter().enumerate() {
            text.push_str(&format!(
                r#"{{"job":"{id}","seq":{},"type":"{e}","at":{},"data":{{}}}}"#,
                i + 3,
                1002 + i
            ));
            text.push('\n');
        }
        std::fs::write(
            self.data
                .join("users")
                .join(&self.user)
                .join("jobs")
                .join(format!("{id}.jsonl")),
            text,
        )
        .unwrap();
    }
    fn set_role(&self, role: &str) {
        let c = Connection::open(server_store::db_path(&self.data)).unwrap();
        c.execute("UPDATE users SET role=?1", [role]).unwrap();
    }
    async fn call(&self, method: &str, path: &str) -> (u16, Value) {
        call(self.addr, method, path, &self.jar, &self.csrf).await
    }
    fn set_node(&self, status: u16, body: Value) {
        *self.node.answer.lock().unwrap() = (status, body.to_string());
    }
}

async fn call(addr: SocketAddr, method: &str, path: &str, jar: &str, csrf: &str) -> (u16, Value) {
    let mut s = TcpStream::connect(addr).await.unwrap();
    let req = format!("{method} {path} HTTP/1.1\r\nhost: front.test\r\nconnection: close\r\norigin: {ORIGIN}\r\ncookie: {jar}\r\nx-csrf-token: {csrf}\r\ncontent-length: 0\r\n\r\n");
    s.write_all(req.as_bytes()).await.unwrap();
    let mut out = Vec::new();
    s.read_to_end(&mut out).await.unwrap();
    let text = String::from_utf8_lossy(&out).into_owned();
    let status = text.split(' ').nth(1).unwrap().parse().unwrap();
    let body = text.split_once("\r\n\r\n").map_or("", |(_, b)| b);
    (status, serde_json::from_str(body).unwrap_or(Value::Null))
}

#[tokio::test]
async fn the_active_list_is_the_accounts_live_visible_tasks_with_their_waiting_action() {
    let f = front(true).await;
    let (s, v) = f.call("GET", "/api/code/active").await;
    assert_eq!(s, 200, "{v}");
    assert_eq!(
        v["total"], 1,
        "a finished task and an internal project's are left out: {v}"
    );
    let row = &v["tasks"][0];
    assert_eq!(row["id"], TASK);
    assert_eq!(row["projectId"], "p1");
    assert_eq!(row["projectName"], "Alpha");
    assert_eq!(row["title"], "Synthetic task");
    assert_eq!(row["status"], "waiting_approval");
    assert_eq!(row["approvalAction"], "edit_file");
    assert_eq!(row["updatedAt"], 1003);
    // The right question was put to Node, with its secret.
    let asked = f.node.seen.lock().unwrap().last().unwrap().clone();
    assert!(asked.starts_with("post /internal/tasks/facts "), "{asked}");
    assert!(asked.contains(&format!("x-noevia-store-token: {STORE_TOKEN}")));
    assert!(asked.contains(&format!("\"user\":\"{}\"", f.user)));
    assert!(asked.contains("\"surface\":\"code\"") && asked.contains("\"admin\":true"));
    // Other methods: core's 405, past the flag and the role.
    assert_eq!(f.call("POST", "/api/code/active").await.0, 405);
}

#[tokio::test]
async fn a_projects_state_and_task_answer_as_node_did_and_keep_tenant_scope() {
    let f = front(true).await;
    let (s, v) = f.call("GET", "/api/projects/p1/code").await;
    assert_eq!(s, 200, "{v}");
    for key in [
        "repositories",
        "capabilities",
        "defaultCapabilities",
        "harnesses",
        "promptPreparation",
        "sandboxed",
        "network",
        "networkState",
        "tasks",
    ] {
        assert!(v.get(key).is_some(), "{key}: {v}");
    }
    assert!(
        v.get("approvals").is_none(),
        "the card map stays on the bridge"
    );
    assert_eq!(v["networkState"], "unavailable");
    assert_eq!(v["tasks"].as_array().unwrap().len(), 1);
    assert_eq!(v["tasks"][0]["approval"]["id"], "card-1");
    let (s, v) = f
        .call("GET", &format!("/api/projects/p1/code/{TASK}"))
        .await;
    assert_eq!(s, 200, "{v}");
    assert_eq!(v["approval"]["action"], "edit_file");
    assert_eq!(v["task"], "Synthetic task");
    // Another project's task, a missing one and a missing project.
    let (s, v) = f
        .call("GET", &format!("/api/projects/p2/code/{TASK}"))
        .await;
    assert_eq!((s, v["error"].as_str()), (404, Some("Task not found")));
    let (s, v) = f
        .call(
            "GET",
            "/api/projects/p1/code/44444444-4444-4444-8444-444444444444",
        )
        .await;
    assert_eq!((s, v["error"].as_str()), (404, Some("Task not found")));
    let (s, v) = f.call("GET", "/api/projects/nope/code").await;
    assert_eq!((s, v["error"].as_str()), (404, Some("project not found")));
    let (s, v) = f
        .call("GET", &format!("/api/projects/nope/browser/{TASK}"))
        .await;
    assert_eq!((s, v["error"].as_str()), (404, Some("project not found")));
    // A malformed project id is the 500 Node's decodeURIComponent gave.
    assert_eq!(f.call("GET", "/api/projects/%ZZ/code").await.0, 500);
    // A browser read does not show a code task.
    let (s, v) = f
        .call("GET", &format!("/api/projects/p1/browser/{TASK}"))
        .await;
    assert_eq!((s, v["error"].as_str()), (404, Some("Task not found")));
    // An unreadable task on a get: Node's 409, and the list keeps going.
    std::fs::write(
        f.data
            .join("users")
            .join(&f.user)
            .join("jobs")
            .join(format!("{TASK}.jsonl")),
        "bad\nbad\n",
    )
    .unwrap();
    let (s, v) = f
        .call("GET", &format!("/api/projects/p1/code/{TASK}"))
        .await;
    assert_eq!(
        (s, v["error"].as_str()),
        (409, Some("Unreadable job journal; review required"))
    );
    let (s, v) = f.call("GET", "/api/projects/p1/code").await;
    assert_eq!(s, 200);
    // The whole stub, as core's list answers one: an id and a status, nothing else (#2270).
    assert_eq!(v["tasks"][0], json!({"id": TASK, "status": "unreadable"}));
    assert_eq!(v["tasks"].as_array().unwrap().len(), 1, "{v}");
}

#[tokio::test]
async fn the_browser_state_lists_browser_tasks_only() {
    let f = front(true).await;
    let id = "55555555-5555-4555-8555-555555555555";
    f.journal(id, "browser", "p1", &["job.started"]);
    f.set_node(200, json!({"enabled": true, "facts": {"capabilities":["open_browser"],"network":true,"networkState":"ready","approvals":{id:{"id":"b1","action":"click"}}}}));
    let (s, v) = f.call("GET", "/api/projects/p1/browser").await;
    assert_eq!(s, 200, "{v}");
    assert_eq!(v["capabilities"][0], "open_browser");
    assert_eq!(v["tasks"].as_array().unwrap().len(), 1, "{v}");
    assert_eq!(v["tasks"][0]["id"], id);
    assert_eq!(v["tasks"][0]["domains"][0], "x.test");
    assert_eq!(v["tasks"][0]["approval"]["id"], "b1");
    assert_eq!(
        f.call("GET", &format!("/api/projects/p1/browser/{id}"))
            .await
            .1["approval"]["id"],
        "b1"
    );
}

#[tokio::test]
async fn the_flag_comes_first_then_the_role_and_a_signed_out_read_never_reaches_node() {
    let f = front(true).await;
    // Signed out and a bad session: the gate, before anything.
    let before = f.node.seen.lock().unwrap().len();
    assert_eq!(
        call(f.addr, "GET", "/api/code/active", "x=y", "t").await.0,
        401
    );
    assert_eq!(
        call(f.addr, "GET", "/api/projects/p1/code", "x=y", "t")
            .await
            .0,
        401
    );
    assert_eq!(f.node.seen.lock().unwrap().len(), before);
    // A member: Node's flag answer, then 403, and the question says it is not an administrator.
    f.set_role("member");
    f.set_node(200, json!({"enabled": true}));
    for path in [
        "/api/code/active",
        "/api/projects/p1/code",
        "/api/projects/p1/browser",
    ] {
        let (s, v) = f.call("GET", path).await;
        assert_eq!(
            (s, v["error"].as_str()),
            (403, Some("Administrator required")),
            "{path}"
        );
    }
    assert!(f
        .node
        .seen
        .lock()
        .unwrap()
        .last()
        .unwrap()
        .contains("\"admin\":false"));
    // Off: 404 for everyone, in each surface's words.
    f.set_node(200, json!({"enabled": false}));
    let (s, v) = f.call("GET", "/api/code/active").await;
    assert_eq!(
        (s, v["error"].as_str()),
        (404, Some("Code mode is not enabled on this server."))
    );
    let (s, v) = f.call("GET", "/api/projects/nope/browser").await;
    assert_eq!(
        (s, v["error"].as_str()),
        (404, Some("Browser mode is not enabled on this server."))
    );
}

#[tokio::test]
async fn no_usable_answer_from_node_is_a_503_never_a_list_without_its_approvals() {
    let f = front(true).await;
    for (status, body) in [
        (404, json!({"error": "Not found"})),
        (503, json!({"error": "Task facts are unavailable."})),
        (200, json!({"enabled": true})),
        (200, json!({"nothing": 1})),
    ] {
        f.set_node(status, body.clone());
        let (s, _) = f.call("GET", "/api/projects/p1/code").await;
        assert_eq!(s, 503, "{status} {body}");
    }
    let absent = front(false).await;
    let (s, v) = absent.call("GET", "/api/code/active").await;
    assert_eq!(s, 503);
    assert!(v["error"].as_str().unwrap().contains("unavailable"));
}

#[tokio::test]
async fn the_task_writes_stay_with_node() {
    let f = front(false).await;
    for (m, p) in [
        ("POST", "/api/projects/p1/code".to_owned()),
        ("POST", format!("/api/projects/p1/code/{TASK}/approve")),
        ("POST", format!("/api/projects/p1/code/{TASK}/cancel")),
        ("GET", format!("/api/projects/p1/code/{TASK}/approve")),
        ("POST", "/api/projects/p1/browser".to_owned()),
        ("POST", format!("/api/projects/p1/browser/{TASK}/act")),
    ] {
        assert_eq!(
            f.call(m, &p).await.0,
            502,
            "{m} {p}: goes to Node, absent here"
        );
    }
}
