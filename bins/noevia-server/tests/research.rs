//! sbstndalton/noevia#2218 `rust-research`: deep research through the whole front, with a synthetic Node (its
//! admission lease and tool bridge) and a synthetic engine, both plain TCP on 127.0.0.1. No model and no web.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]

use noevia_server::config::Config;
use noevia_server::serve::{serve, Limits};
use serde_json::{json, Value};
use server_projects::research_jobs as jobs;
use server_store::rusqlite::Connection;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

const SCHEMA: &str = include_str!("../../../crates/server-store/tests/fixtures/node-schema.sql");
const CODE: &str = "synthetic-research-code";
const ORIGIN: &str = "http://127.0.0.1:18021";
const STORE_TOKEN: &str = "synthetic-store-token-0123456789abcdef";
const START: &str = "0123456789abcdef0123456789abcdef";
const LEASE: &str = "0123456789abcdef0123456789abcdef";
const UNREACHABLE: &str = "Research could not reach this server's search tools (the bridge to the web process did not answer). Try again shortly; if it keeps happening, ask an administrator to check the server.";

#[derive(Clone)]
enum Answer {
    Now(u16, String),
    /// A 302 to this location.
    Redirect(String),
    /// Holds the connection until the peer goes away, then records `what`.
    Hang(&'static str),
}

type Responder = Arc<dyn Fn(&str, &str, &str) -> Answer + Send + Sync>;

/// A plain HTTP stub: every request is logged `METHOD path headers-lowercased\n\nbody`.
struct Stub {
    addr: SocketAddr,
    log: Arc<Mutex<Vec<String>>>,
}

impl Stub {
    async fn start(respond: Responder) -> Stub {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let log = Arc::new(Mutex::new(Vec::new()));
        let seen = Arc::clone(&log);
        tokio::spawn(async move {
            while let Ok((mut s, _)) = listener.accept().await {
                let (respond, seen) = (Arc::clone(&respond), Arc::clone(&seen));
                tokio::spawn(async move {
                    let mut buf = Vec::new();
                    let mut chunk = [0u8; 8192];
                    let (head, body) = loop {
                        let n = s.read(&mut chunk).await.unwrap_or(0);
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
                                break (h.to_owned(), b.to_owned());
                            }
                        }
                    };
                    let line = head.lines().next().unwrap_or("").to_owned();
                    let mut parts = line.split(' ');
                    let (m, p) = (
                        parts.next().unwrap_or("").to_owned(),
                        parts.next().unwrap_or("").to_owned(),
                    );
                    seen.lock()
                        .unwrap()
                        .push(format!("{m} {p} {}\n\n{body}", head.to_ascii_lowercase()));
                    match respond(&m, &p, &body) {
                        Answer::Now(status, text) => {
                            let out = format!(
                                "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{text}",
                                text.len()
                            );
                            let _ = s.write_all(out.as_bytes()).await;
                        }
                        Answer::Redirect(to) => {
                            let out = format!(
                                "HTTP/1.1 302 Found\r\nlocation: {to}\r\ncontent-length: 0\r\nconnection: close\r\n\r\n"
                            );
                            let _ = s.write_all(out.as_bytes()).await;
                        }
                        Answer::Hang(what) => {
                            while s.read(&mut chunk).await.unwrap_or(0) > 0 {}
                            seen.lock().unwrap().push(what.to_owned());
                        }
                    }
                });
            }
        });
        Stub { addr, log }
    }
    fn seen(&self) -> Vec<String> {
        self.log.lock().unwrap().clone()
    }
    fn count(&self, starts: &str) -> usize {
        self.seen().iter().filter(|l| l.starts_with(starts)).count()
    }
}

fn sse(text: &str) -> String {
    format!(
        "data: {}\n\ndata: [DONE]\n\n",
        json!({"choices":[{"delta":{"content": text}}]})
    )
}

/// Node: lease granted, both tools there; extra behaviour by path.
fn node_answer(path: &str, body: &str) -> Answer {
    let ok = |v: Value| Answer::Now(200, v.to_string());
    if path == "/internal/inference-admission/lease" {
        return Answer::Now(201, json!({"lease": LEASE, "ttlMs": 30000}).to_string());
    }
    if path.ends_with("/renew") {
        return ok(json!({"ttlMs": 30000}));
    }
    if path.starts_with("/internal/inference-admission/lease/") {
        return Answer::Now(204, String::new());
    }
    match path {
        "/internal/research/availability" => {
            ok(json!({"reason": null, "lastLoaded": "synthetic-model"}))
        }
        "/internal/research/retrieve" => ok(json!({"hits": []})),
        "/internal/research/tool" => {
            if body.contains("tavily_search") {
                ok(
                    json!({"text": json!({"results":[{"url":"https://example.test/a","title":"A page"}]}).to_string()}),
                )
            } else {
                ok(json!({"text": "The tower is 50 metres tall. It opened in 1999."}))
            }
        }
        _ => Answer::Now(404, "{}".into()),
    }
}

fn lease_node() -> Responder {
    Arc::new(|_m, p, b| node_answer(p, b))
}

fn engine_saying(text: &'static str) -> Responder {
    Arc::new(move |_m, _p, _b| Answer::Now(200, sse(text)))
}

struct Front {
    _dir: tempfile::TempDir,
    data: PathBuf,
    addr: SocketAddr,
    jar: String,
    csrf: String,
    user: String,
    node: Stub,
    engine: Stub,
    app: Arc<noevia_server::App>,
}

async fn front(flag: Option<&str>, node: Responder, engine: Responder) -> Front {
    let (node, engine) = (Stub::start(node).await, Stub::start(engine).await);
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().to_path_buf();
    let c = Connection::open(server_store::db_path(&data)).unwrap();
    c.execute_batch(SCHEMA).unwrap();
    c.execute(
        "INSERT INTO settings VALUES('setup_code_hash', ?1)",
        [server_auth::js::digest(CODE)],
    )
    .unwrap();
    let flag_name = server_features::spec(server_features::Feature::DeepResearch).env;
    let mut env: HashMap<&str, String> = HashMap::new();
    env.insert("NOEVIA_LEGACY_UPSTREAM", format!("http://{}", node.addr));
    env.insert("UI_PORT", "1".into());
    env.insert("UI_DATA_DIR", data.display().to_string());
    env.insert("NOEVIA_WEB_DIST", data.join("dist").display().to_string());
    env.insert("PUBLIC_ORIGIN", ORIGIN.into());
    env.insert("NOEVIA_START_ID", START.into());
    env.insert("NOEVIA_FEATURE_NATIVE_CLIENT_AUTH", "true".into());
    env.insert("TRUST_PROXY", "true".into());
    env.insert("NOEVIA_PROJECT_STORE_TOKEN", STORE_TOKEN.into());
    env.insert("INFERENCE_BASE_URL", format!("http://{}", engine.addr));
    if let Some(v) = flag {
        env.insert(flag_name, v.into());
    }
    let mut app = noevia_server::App::new(Config::from_lookup(|k| env.get(k).cloned()).unwrap());
    Arc::get_mut(&mut app).unwrap().storage_child =
        Some(env!("CARGO_BIN_EXE_noevia-server").into());
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let kept = Arc::clone(&app);
    tokio::spawn(serve(
        listener,
        app,
        Limits::default(),
        std::future::pending(),
    ));
    let setup = format!(
        r#"{{"setupCode":"{CODE}","username":"owner","password":"synthetic research password","diaryEnabled":false}}"#
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
    let f = Front {
        _dir: dir,
        data,
        addr,
        jar,
        csrf,
        user,
        node,
        engine,
        app: kept,
    };
    seed_project(&f.user_dir());
    f
}

impl Front {
    fn user_dir(&self) -> PathBuf {
        self.data.join("users").join(&self.user)
    }
    async fn call(&self, method: &str, path: &str, body: &str) -> (u16, Value) {
        call(self.addr, method, path, &self.jar, &self.csrf, body).await
    }
    async fn plan(&self) -> (u16, Value) {
        self.call(
            "POST",
            "/api/projects/p1/research/plan",
            r#"{"question":"How tall is the tower?"}"#,
        )
        .await
    }
    /// Starts a job and waits until `done` says so of its view.
    async fn job_until(&self, id: &str, done: impl Fn(&Value) -> bool) -> Value {
        for _ in 0..400 {
            let (s, v) = self
                .call("GET", &format!("/api/projects/p1/research/{id}"), "")
                .await;
            assert_eq!(s, 200, "{v}");
            if done(&v) {
                return v;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        panic!("the job never got there");
    }
}

fn seed_project(dir: &std::path::Path) {
    std::fs::create_dir_all(dir).unwrap();
    std::fs::write(
        dir.join("projects.json"),
        r#"{"projects":[{"id":"p1","name":"Base","goal":"g","instructions":"i","pinned":false,"archived":false,"sourceFolders":[],"memories":[],"files":[],"routing":"auto","modes":["chat"],"toolboxes":["core"],"toolsMode":"auto","chats":[],"createdAt":1,"updatedAt":2}]}"#,
    )
    .unwrap();
}

async fn call(
    addr: SocketAddr,
    method: &str,
    path: &str,
    jar: &str,
    csrf: &str,
    body: &str,
) -> (u16, Value) {
    let mut s = TcpStream::connect(addr).await.unwrap();
    let req = format!(
        "{method} {path} HTTP/1.1\r\nhost: front.test\r\nconnection: close\r\ncookie: {jar}\r\nx-csrf-token: {csrf}\r\norigin: {ORIGIN}\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n{body}",
        body.len()
    );
    s.write_all(req.as_bytes()).await.unwrap();
    let mut out = Vec::new();
    s.read_to_end(&mut out).await.unwrap();
    let text = String::from_utf8_lossy(&out).into_owned();
    let status = text[9..12].parse().unwrap();
    let (_, b) = text.split_once("\r\n\r\n").unwrap();
    (status, serde_json::from_str(b).unwrap_or(Value::Null))
}

async fn settle(what: impl Fn() -> bool) -> bool {
    for _ in 0..400 {
        if what() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    false
}

fn set_machine_row(f: &Front, base_url: &str, model: &str) {
    let c = Connection::open(server_store::db_path(&f.data)).unwrap();
    c.execute(
        "INSERT OR REPLACE INTO settings(key,value) VALUES('inference:machines', ?1)",
        [machine_row(base_url, model)],
    )
    .unwrap();
}

/// core inference-machines.cjs's row, with `base_url` as scheme://host:port.
fn machine_row(base_url: &str, model: &str) -> String {
    let (address, port) = base_url.rsplit_once(':').unwrap();
    json!({
        "version": 1,
        "machines": [{"id":"m1","name":"Synthetic Pi","address":address}],
        "assignments": {"chatSmart": {"machine":"m1","port":port.parse::<u32>().unwrap(),"model":model}}
    })
    .to_string()
}

#[tokio::test]
async fn off_is_absent_and_a_flag_that_is_on_serves_admins_alone() {
    let off = front(None, lease_node(), engine_saying("x")).await;
    let (s, v) = off.call("GET", "/api/projects/p1/research", "").await;
    assert_eq!(
        (s, v),
        (
            404,
            json!({"error":"Deep research is not enabled on this server."})
        )
    );
    assert_eq!(off.node.seen().len(), 0, "nothing reached Node");

    let f = front(Some("true"), lease_node(), engine_saying("x")).await;
    let (s, v) = f.call("GET", "/api/projects/p1/research", "").await;
    assert_eq!(s, 200, "{v}");
    assert_eq!(v["budget"]["maxWebCalls"], 12, "{v}");
    assert_eq!(v["available"], true);
    assert_eq!(v["reason"], Value::Null);
    assert_eq!(v["jobs"], json!([]));

    // A member, and a device token on an administrator's account (which acts as a member).
    let member = "22222222-2222-4222-8222-222222222222";
    let c = Connection::open(server_store::db_path(&f.data)).unwrap();
    c.execute("INSERT INTO users(id,username,username_norm,display_name,role,password_hash,webauthn_user_id,created_at,updated_at) VALUES(?1,'member','member','Synthetic member','member','x',?1,1,1)", [member]).unwrap();
    let now = noevia_server::identity::now_ms();
    c.execute(
        "INSERT INTO sessions(id_hash,user_id,csrf_hash,created_at,last_seen_at,expires_at,user_agent,ip) VALUES(?1,?2,?3,?4,?4,?5,'synthetic','127.0.0.1')",
        (server_auth::js::digest("member-token"), member, server_auth::js::digest("member-csrf"), now, now + 86_400_000),
    ).unwrap();
    seed_project(&f.data.join("users").join(member));
    let before = f.node.seen().len();
    for (m, p) in [
        ("GET", "/api/projects/p1/research"),
        ("POST", "/api/projects/p1/research"),
        ("POST", "/api/projects/p1/research/plan"),
    ] {
        let (s, v) = call(
            f.addr,
            m,
            p,
            "cowork_session=member-token; cowork_csrf=member-csrf",
            "member-csrf",
            "{}",
        )
        .await;
        assert_eq!(
            (s, v),
            (403, json!({"error":"Administrator required"})),
            "{m} {p}"
        );
    }
    assert_eq!(f.node.seen().len(), before, "a member reaches nothing");
    // A device token on the administrator's own account: it acts as a member (core `authn.user.role`).
    c.execute(
        "INSERT INTO device_grants(id,user_id,client_name,created_at,last_used_at,expires_at,ip,user_agent) VALUES('synthetic-grant',?1,'Synthetic',?2,?2,?3,'127.0.0.1','ua')",
        (f.user.as_str(), now, now + 86_400_000),
    )
    .unwrap();
    c.execute(
        "INSERT INTO device_tokens(token_hash,grant_id,kind,created_at,expires_at) VALUES(?1,'synthetic-grant','access',?2,?3)",
        (server_auth::js::digest("nva_synthetic-device-token"), now, now + 86_400_000),
    )
    .unwrap();
    let mut s = TcpStream::connect(f.addr).await.unwrap();
    s.write_all(b"GET /api/projects/p1/research HTTP/1.1\r\nhost: front.test\r\nconnection: close\r\nauthorization: Bearer nva_synthetic-device-token\r\n\r\n")
        .await
        .unwrap();
    let mut out = Vec::new();
    s.read_to_end(&mut out).await.unwrap();
    let text = String::from_utf8_lossy(&out).into_owned();
    assert!(text.starts_with("HTTP/1.1 403"), "{text}");
    assert!(text.contains("Administrator required"), "{text}");
    // With no session at all.
    let (s, _) = call(f.addr, "GET", "/api/projects/p1/research", "", "", "").await;
    assert_eq!(s, 401);
    // Another account's project id is a missing project here, not that project.
    let (s, v) = f.call("GET", "/api/projects/qa-other/research", "").await;
    assert_eq!((s, v), (404, json!({"error":"project not found"})));
}

#[tokio::test]
async fn method_and_shape_answers_are_nodes() {
    let f = front(Some("true"), lease_node(), engine_saying("x")).await;
    let job = "0123abcd-0123-4abc-8def-0123456789ab";
    let na = json!({"error":"method not allowed"});
    for (m, p) in [
        (
            "GET".to_owned(),
            "/api/projects/p1/research/plan".to_owned(),
        ),
        (
            "POST".to_owned(),
            "/api/projects/p1/research/plan/cancel".to_owned(),
        ),
        ("PUT".to_owned(), "/api/projects/p1/research".to_owned()),
        (
            "POST".to_owned(),
            format!("/api/projects/p1/research/{job}"),
        ),
        (
            "GET".to_owned(),
            format!("/api/projects/p1/research/{job}/cancel"),
        ),
        (
            "GET".to_owned(),
            format!("/api/projects/p1/research/{job}/save"),
        ),
    ] {
        let (s, v) = f.call(&m, &p, "").await;
        assert_eq!((s, &v), (405, &na), "{m} {p}");
    }
    let (s, v) = f
        .call("GET", &format!("/api/projects/p1/research/{job}"), "")
        .await;
    assert_eq!(s, 404, "{v}");
    // Bad body and bad question words.
    let (s, v) = f.call("POST", "/api/projects/p1/research", "{").await;
    assert_eq!((s, v), (400, json!({"error":"invalid JSON"})));
    let (s, v) = f
        .call("POST", "/api/projects/p1/research", r#"{"question":"  "}"#)
        .await;
    assert_eq!(
        (s, v),
        (400, json!({"error":"Write a research question first."}))
    );
    let long = format!(r#"{{"question":"{}"}}"#, "x".repeat(2001));
    let (s, v) = f.call("POST", "/api/projects/p1/research", &long).await;
    assert_eq!(
        (s, v),
        (
            400,
            json!({"error":"Keep the research question under 2000 characters."})
        )
    );
    // Node's old base bug: `/research/cancel` on the old matcher started a job.
    let (s, _) = f
        .call(
            "POST",
            "/api/projects/p1/research/cancel",
            r#"{"question":"q"}"#,
        )
        .await;
    assert_eq!(s, 404);
    let (_, v) = f.call("GET", "/api/projects/p1/research", "").await;
    assert_eq!(v["jobs"], json!([]));
}

#[tokio::test]
async fn availability_comes_from_node_and_a_missing_node_refuses() {
    let words = "Web search is not configured on this server.";
    let node: Responder = Arc::new(move |_m, p, b| {
        if p == "/internal/research/availability" {
            Answer::Now(
                200,
                json!({"reason": words, "lastLoaded": null}).to_string(),
            )
        } else {
            node_answer(p, b)
        }
    });
    let f = front(Some("true"), node, engine_saying("x")).await;
    let (s, v) = f.call("GET", "/api/projects/p1/research", "").await;
    assert_eq!(s, 200);
    assert_eq!(
        (&v["available"], &v["reason"]),
        (&json!(false), &json!(words))
    );
    let (s, v) = f.plan().await;
    assert_eq!((s, v), (409, json!({"error": words})));
    assert_eq!(f.engine.seen().len(), 0, "no engine call");

    let nothing: Responder = Arc::new(|_m, _p, _b| Answer::Now(500, "x".into()));
    let f = front(Some("true"), nothing, engine_saying("x")).await;
    let (_, v) = f.call("GET", "/api/projects/p1/research", "").await;
    assert_eq!(v["available"], false, "unknown is not available: {v}");
    assert_eq!(
        v["reason"], UNREACHABLE,
        "a bridge that fails is not 'not configured': {v}"
    );
    // Tools there but no model anywhere: the pick-a-model words.
    let none: Responder = Arc::new(|_m, p, b| {
        if p == "/internal/research/availability" {
            Answer::Now(200, json!({"reason": null, "lastLoaded": null}).to_string())
        } else {
            node_answer(p, b)
        }
    });
    let f = front(Some("true"), none, engine_saying("x")).await;
    let (_, v) = f.call("GET", "/api/projects/p1/research", "").await;
    assert_eq!(
        v["reason"],
        "Pick a Smart model for Auto routing, or load a model, before starting research."
    );
}

#[tokio::test]
async fn a_plan_runs_under_a_lease_streams_and_gives_the_lease_back() {
    let f = front(
        Some("true"),
        lease_node(),
        engine_saying(r#"{"subQuestions":["Height?","Age?","Place?"]}"#),
    )
    .await;
    let (s, v) = f.plan().await;
    assert_eq!(s, 200, "{v}");
    assert!(v["subQuestions"].is_array(), "{v}");
    let seen = f.engine.seen();
    assert_eq!(seen.len(), 1);
    assert!(
        seen[0].starts_with("POST /v1/chat/completions"),
        "{}",
        seen[0]
    );
    let body: Value = serde_json::from_str(seen[0].split_once("\n\n").unwrap().1).unwrap();
    assert_eq!(
        (&body["model"], &body["stream"]),
        (&json!("synthetic-model"), &json!(true))
    );
    assert_eq!(f.node.count("POST /internal/inference-admission/lease"), 1);
    let enter = f
        .node
        .seen()
        .into_iter()
        .find(|l| l.starts_with("POST /internal/inference-admission/lease"))
        .unwrap();
    assert!(enter.contains(&format!("x-noevia-store-token: {STORE_TOKEN}")));
    assert!(enter.contains(r#"{"model":"synthetic-model"}"#), "{enter}");
    assert!(
        settle(|| f.node.count(&format!(
            "DELETE /internal/inference-admission/lease/{LEASE}"
        )) == 1)
        .await,
        "released"
    );
}

#[tokio::test]
async fn a_refused_lease_sends_nothing_to_the_engine_and_keeps_its_words() {
    let node: Responder = Arc::new(|_m, p, b| {
        if p == "/internal/inference-admission/lease" {
            Answer::Now(
                409,
                json!({"error":"Over the inference budget.","code":"overBudget"}).to_string(),
            )
        } else {
            node_answer(p, b)
        }
    });
    let f = front(Some("true"), node, engine_saying("x")).await;
    let (s, v) = f.plan().await;
    assert_eq!(s, 500, "{v}");
    assert_eq!(v["error"], "Over the inference budget.");
    assert_eq!(f.engine.seen().len(), 0);
}

#[tokio::test]
async fn a_client_that_leaves_a_plan_ends_the_engine_call_and_the_lease() {
    let hang: Responder = Arc::new(|_m, _p, _b| Answer::Hang("engine-dropped"));
    let f = front(Some("true"), lease_node(), hang).await;
    let mut s = TcpStream::connect(f.addr).await.unwrap();
    let body = r#"{"question":"How tall?"}"#;
    let req = format!(
        "POST /api/projects/p1/research/plan HTTP/1.1\r\nhost: front.test\r\ncookie: {}\r\nx-csrf-token: {}\r\norigin: {ORIGIN}\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n{body}",
        f.jar, f.csrf, body.len()
    );
    s.write_all(req.as_bytes()).await.unwrap();
    assert!(
        settle(|| f.engine.count("POST /v1/chat/completions") == 1).await,
        "the engine was asked"
    );
    drop(s);
    assert!(
        settle(|| f.engine.count("engine-dropped") == 1).await,
        "the engine's connection closed"
    );
    assert!(
        settle(|| f.node.count("DELETE /internal/inference-admission/lease/") == 1).await,
        "the lease came back: {:?}",
        f.node.seen()
    );
}

/// A research job from the POST to its end, with the engine answering with a citing sentence.
#[tokio::test]
async fn a_job_runs_to_its_end_after_the_client_left_and_keeps_every_cap() {
    let f = front(
        Some("true"),
        lease_node(),
        engine_saying("The tower is 50 metres tall [S1]."),
    )
    .await;
    let (s, v) = f
        .call(
            "POST",
            "/api/projects/p1/research",
            r#"{"question":"How tall is the tower?"}"#,
        )
        .await;
    assert_eq!(s, 202, "{v}");
    assert_eq!(v["status"], "running");
    let id = v["id"].as_str().unwrap().to_owned();
    // The connection above is closed already: the job is its own task.
    let done = f.job_until(&id, |j| j["status"] != "running").await;
    assert_eq!(done["status"], "completed", "{done}");
    assert!(
        done["result"]["markdown"]
            .as_str()
            .unwrap()
            .contains("tower"),
        "{done}"
    );
    assert_eq!(done["result"]["sources"], 1, "{done}");
    assert!(done.get("artifacts").is_some());
    // Excerpts are stored, never in the public view.
    assert!(!done.to_string().contains("excerpts"));
    // One search, in research's shape, as this user; no other tool name.
    for line in f
        .node
        .seen()
        .iter()
        .filter(|l| l.starts_with("POST /internal/research/tool"))
    {
        let body: Value = serde_json::from_str(line.split_once("\n\n").unwrap().1).unwrap();
        assert_eq!(body["user"], f.user.as_str());
        assert!(["tavily_search", "tavily_extract"].contains(&body["name"].as_str().unwrap()));
    }
    // Every lease back.
    let f2 = &f;
    assert!(
        settle(|| f2.node.count("POST /internal/inference-admission/lease")
            == f2.node.count("DELETE /internal/inference-admission/lease/"))
        .await,
        "{:?}",
        f.node.seen()
    );
}

#[tokio::test]
async fn one_active_job_a_cancel_stops_the_engine_and_a_late_cancel_is_a_read() {
    let hang: Responder = Arc::new(|_m, _p, _b| Answer::Hang("engine-dropped"));
    let f = front(Some("true"), lease_node(), hang).await;
    let (s, v) = f
        .call(
            "POST",
            "/api/projects/p1/research",
            r#"{"question":"How tall?"}"#,
        )
        .await;
    assert_eq!(s, 202, "{v}");
    let id = v["id"].as_str().unwrap().to_owned();
    assert!(
        settle(|| f.engine.count("POST /v1/chat/completions") >= 1).await,
        "{:?}",
        f.node.seen()
    );
    let (s, v) = f
        .call(
            "POST",
            "/api/projects/p1/research",
            r#"{"question":"Another?"}"#,
        )
        .await;
    assert_eq!(s, 409, "{v}");
    let (s, v) = f
        .call(
            "POST",
            &format!("/api/projects/p1/research/{id}/cancel"),
            "",
        )
        .await;
    assert_eq!(s, 200, "{v}");
    let done = f.job_until(&id, |j| j["status"] != "running").await;
    assert_eq!(done["status"], "cancelled", "{done}");
    assert!(
        settle(|| f.engine.count("engine-dropped") == 1).await,
        "the engine was told"
    );
    assert!(
        settle(|| f.node.count("DELETE /internal/inference-admission/lease/") == 1).await,
        "lease back"
    );
    let (s, again) = f
        .call(
            "POST",
            &format!("/api/projects/p1/research/{id}/cancel"),
            "",
        )
        .await;
    assert_eq!((s, &again["status"]), (200, &json!("cancelled")));
    // A project that is not the job's sees no such job.
    let dir = f.user_dir();
    let text = std::fs::read_to_string(dir.join("projects.json")).unwrap();
    std::fs::write(dir.join("projects.json"), text.replace(r#"{"projects":["#, r#"{"projects":[{"id":"p2","name":"Two","goal":"g","instructions":"i","pinned":false,"archived":false,"sourceFolders":[],"memories":[],"files":[],"routing":"auto","modes":["chat"],"toolboxes":["core"],"toolsMode":"auto","chats":[],"createdAt":1,"updatedAt":2},"#)).unwrap();
    let (s, _) = f
        .call("GET", &format!("/api/projects/p2/research/{id}"), "")
        .await;
    assert_eq!(s, 404);
}

#[tokio::test]
async fn a_lost_lease_stops_the_step() {
    // Renewals say the lease ended: the step must not keep the engine.
    let hang: Responder = Arc::new(|_m, _p, _b| Answer::Hang("engine-dropped"));
    let node: Responder = Arc::new(|_m, p, b| {
        if p == "/internal/inference-admission/lease" {
            return Answer::Now(201, json!({"lease": LEASE, "ttlMs": 300}).to_string());
        }
        if p.ends_with("/renew") {
            return Answer::Now(404, "{}".into());
        }
        node_answer(p, b)
    });
    let f = front(Some("true"), node, hang).await;
    // Long before the step's own 120 s deadline.
    let (s, v) = tokio::time::timeout(Duration::from_secs(20), f.plan())
        .await
        .expect("a lost lease ends the step at once");
    assert!(s >= 500, "{s} {v}");
    assert_eq!(f.engine.count("engine-dropped"), 1);
}

#[tokio::test]
async fn an_engine_answer_past_the_cap_and_an_overlong_stream_are_refused() {
    let big = "x".repeat(5 * 1024 * 1024);
    let engine: Responder = Arc::new(move |_m, _p, _b| Answer::Now(200, sse(&big)));
    let f = front(Some("true"), lease_node(), engine).await;
    let (s, v) = f.plan().await;
    assert_eq!(s, 500, "{v}");
    assert!(
        settle(|| f.node.count("DELETE /internal/inference-admission/lease/") == 1).await,
        "lease back after the refusal"
    );
}

#[tokio::test]
async fn smart_on_a_machine_runs_there_keyless_and_the_row_is_only_read() {
    let f = front(Some("true"), lease_node(), engine_saying("local")).await;
    let machine = Stub::start(engine_saying(
        r#"{"subQuestions":["Height?","Age?","Place?"]}"#,
    ))
    .await;
    let url = format!("http://{}", machine.addr);
    set_machine_row(&f, &url, "machine-model");
    let row = || -> String {
        let c = Connection::open(server_store::db_path(&f.data)).unwrap();
        c.query_row(
            "SELECT value FROM settings WHERE key='inference:machines'",
            [],
            |r| r.get(0),
        )
        .unwrap()
    };
    let before = row();
    let (s, v) = f.plan().await;
    assert_eq!(s, 200, "{v}");
    assert_eq!(f.engine.seen().len(), 0, "the local engine is not used");
    let seen = machine.seen();
    assert_eq!(seen.len(), 1);
    assert!(
        !seen[0].contains("authorization"),
        "no local key: {}",
        seen[0]
    );
    let enter = f
        .node
        .seen()
        .into_iter()
        .find(|l| l.starts_with("POST /internal/inference-admission/lease"))
        .unwrap();
    assert!(
        enter.contains(r#""machine":"m1""#) && enter.contains(r#""role":"smart""#),
        "{enter}"
    );
    assert_eq!(row(), before);
}

#[tokio::test]
async fn a_machine_address_off_this_network_never_gets_a_connection_and_redirects_are_refused() {
    let f = front(Some("true"), lease_node(), engine_saying("local")).await;
    // Link-local (the cloud metadata range) is refused before any socket, with core's words.
    set_machine_row(&f, "http://169.254.169.254:8080", "machine-model");
    let (s, v) = f.plan().await;
    assert_eq!(s, 500, "{v}");
    assert!(v["error"].as_str().unwrap().contains("Synthetic Pi"), "{v}");
    assert_eq!(f.engine.seen().len(), 0);
    assert_eq!(
        f.node.count("POST /internal/inference-admission/lease"),
        0,
        "no turn counted"
    );
    // A redirecting machine: the redirect is not followed.
    let target = Stub::start(engine_saying(r#"["x"]"#)).await;
    let there = format!("http://{}/v1/chat/completions", target.addr);
    let redirecting =
        Stub::start(Arc::new(move |_m, _p, _b| Answer::Redirect(there.clone()))).await;
    set_machine_row(&f, &format!("http://{}", redirecting.addr), "machine-model");
    let (s, _) = f.plan().await;
    assert_eq!(s, 500);
    assert_eq!(target.seen().len(), 0, "nothing followed");
}

/// A job a previous start left behind, written by the store itself (the journal old Core wrote through the same routes).
#[tokio::test]
async fn a_job_of_an_earlier_start_reads_as_interrupted_and_its_report_can_be_saved() {
    let f = front(Some("true"), lease_node(), engine_saying("x")).await;
    let old_start = "fedcba9876543210fedcba9876543210";
    let ctx = || jobs::Context {
        data_dir: &f.data,
        start_id: old_start,
    };
    let owner = || jobs::Owner {
        user: &f.user,
        start: old_start,
        born: "1",
        owner: "0123abcd-0123-4abc-8def-012345678900",
    };
    jobs::recover(ctx(), owner()).unwrap();
    let (id, run) = jobs::start(
        ctx(),
        owner(),
        "p1",
        "0123abcd-0123-4abc-8def-012345678901",
        "Old question?",
        "skipped",
        &json!(["Old question?"]),
    )
    .unwrap();
    let result = json!({
        "question":"Old question?","partial":true,"sections":1,"questions":1,"citationValidity":1,
        "citations":{"total":1,"valid":1},"claims":{"total":1,"supported":1,"flagged":0,"dropped":0,"uncited":0},
        "webCalls":1,"markdown":"# Old report\n","dropped":[],"flagged":[],
        "sources":[{"id":1,"kind":"web","url":"https://example.test/a","file":null,"title":"A","retrievedAt":1,"excerpts":[],"sentences":[]}]
    });
    jobs::report(ctx(), owner(), &id, &run, &result).unwrap();
    let (s, v) = f.call("GET", "/api/projects/p1/research", "").await;
    assert_eq!(s, 200, "{v}");
    let row = &v["jobs"][0];
    assert_eq!(row["id"], id.as_str());
    // A job that held a finished report when its process went: core's failed, and saveable.
    assert_eq!(row["status"], "failed", "{row}");
    assert_eq!(row["canSavePartial"], true, "{row}");
    assert_eq!(row["result"]["markdown"], "# Old report\n");
    let (s, saved) = f
        .call("POST", &format!("/api/projects/p1/research/{id}/save"), "")
        .await;
    assert_eq!(s, 200, "{saved}");
    let names: Vec<&str> = saved["artifacts"]
        .as_array()
        .unwrap()
        .iter()
        .map(|a| a.as_str().unwrap())
        .collect();
    assert_eq!(names.len(), 2, "{saved}");
    assert!(names[0].ends_with("old-question.md"), "{names:?}");
    assert!(names[1].ends_with("old-question.sources.json"), "{names:?}");
    assert_eq!(saved["canSavePartial"], false);
    let projects = std::fs::read_to_string(f.user_dir().join("projects.json")).unwrap();
    assert!(projects.contains("old-question.md"), "{projects}");
    // Saved once; a second save says so and writes nothing more.
    let (s, again) = f
        .call("POST", &format!("/api/projects/p1/research/{id}/save"), "")
        .await;
    assert!(s >= 400, "{s} {again}");
}

#[tokio::test]
async fn the_local_engine_never_redirects_a_step() {
    let target = Stub::start(engine_saying(r#"{"subQuestions":["a","b","c"]}"#)).await;
    let there = format!("http://{}/v1/chat/completions", target.addr);
    let f = front(
        Some("true"),
        lease_node(),
        Arc::new(move |_m, _p, _b| Answer::Redirect(there.clone())),
    )
    .await;
    let (s, v) = f.plan().await;
    assert_eq!(s, 500, "{v}");
    assert_eq!(target.seen().len(), 0, "nothing followed");
}

/// The failure that hid behind "Web search is not configured" (review H1, L2): Node answering the front's own calls
/// with its 404 for an unknown path, as it did when it was built with an empty secret.
#[tokio::test]
async fn a_bridge_that_refuses_the_front_is_unreachable_not_unconfigured() {
    let refuse: Responder = Arc::new(|_m, p, b| {
        if p.starts_with("/internal/research/") {
            Answer::Now(404, json!({"error": "Not found"}).to_string())
        } else {
            node_answer(p, b)
        }
    });
    let f = front(Some("true"), refuse, engine_saying("x")).await;
    let (s, v) = f.call("GET", "/api/projects/p1/research", "").await;
    assert_eq!(s, 200);
    assert_eq!(
        (&v["available"], &v["reason"]),
        (&json!(false), &json!(UNREACHABLE))
    );
    let (s, v) = f.plan().await;
    assert_eq!((s, v), (409, json!({"error": UNREACHABLE})));

    // Availability fine, but the tool route refuses: the job says what happened, in the same words.
    let tools_refused: Responder = Arc::new(|_m, p, b| {
        if p == "/internal/research/tool" || p == "/internal/research/retrieve" {
            Answer::Now(404, json!({"error": "Not found"}).to_string())
        } else {
            node_answer(p, b)
        }
    });
    let f = front(Some("true"), tools_refused, engine_saying("x")).await;
    let (s, v) = f
        .call(
            "POST",
            "/api/projects/p1/research",
            r#"{"question":"How tall is the tower?"}"#,
        )
        .await;
    assert_eq!(s, 202, "{v}");
    let id = v["id"].as_str().unwrap().to_owned();
    let done = f.job_until(&id, |j| j["status"] != "running").await;
    assert_eq!(done["status"], "failed", "{done}");
    assert!(
        !done.to_string().contains("not configured") && !done.to_string().contains("Not found"),
        "{done}"
    );
    assert!(
        done["error"]
            .as_str()
            .unwrap()
            .contains("could not reach this server's search tools"),
        "{done}"
    );
}

/// A POST whose client leaves while the job store is busy (review M1). The store call is held on the owner lock; the
/// client goes away; the lock is let go. The job either was never made or is running to its end: it is never a
/// `running` row with nothing behind it.
#[tokio::test]
async fn a_client_that_leaves_while_the_store_is_busy_cannot_strand_the_job() {
    let f = front(
        Some("true"),
        lease_node(),
        engine_saying("The tower is 50 metres tall [S1]."),
    )
    .await;
    // The first read recovers the account's old jobs and makes the owner file the lock sits beside.
    let (s, v) = f.call("GET", "/api/projects/p1/research", "").await;
    assert_eq!(s, 200, "{v}");
    let lock = server_projects::lock::path_for(&f.user_dir().join("research-jobs-owner.json"));
    let held = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&lock)
        .unwrap();
    held.lock().unwrap();

    let mut client = TcpStream::connect(f.addr).await.unwrap();
    let body = r#"{"question":"How tall is the tower?"}"#;
    let req = format!(
        "POST /api/projects/p1/research HTTP/1.1\r\nhost: front.test\r\ncookie: {}\r\nx-csrf-token: {}\r\norigin: {ORIGIN}\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n{body}",
        f.jar, f.csrf, body.len()
    );
    client.write_all(req.as_bytes()).await.unwrap();
    // The store call is waiting on the lock (it gives up after 3 s). The client leaves, and the server notices.
    tokio::time::sleep(Duration::from_millis(600)).await;
    drop(client);
    tokio::time::sleep(Duration::from_millis(600)).await;
    drop(held);

    // The start goes through (or was never made). A job that exists must be run, to its end.
    let mut job = None;
    for _ in 0..120 {
        let (_, v) = f.call("GET", "/api/projects/p1/research", "").await;
        if let Some(j) = v["jobs"].as_array().and_then(|a| a.first()) {
            job = j["id"].as_str().map(str::to_owned);
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    if let Some(id) = job {
        let done = f.job_until(&id, |j| j["status"] != "running").await;
        assert_eq!(done["status"], "completed", "{done}");
        assert_eq!(f.app.config.research.running(), 0);
    }
    // And the project's one slot is free either way.
    let (s, v) = f
        .call(
            "POST",
            "/api/projects/p1/research",
            r#"{"question":"Another?"}"#,
        )
        .await;
    assert_eq!(s, 202, "{v}");
}

/// A job task that ends without reaching its own last lines (a panic does that; here it is stopped from outside,
/// which the runtime reports the same way) leaves no `running` row and no taken slot (review L1).
#[tokio::test]
async fn a_job_task_that_dies_frees_its_slot_and_its_row() {
    let hang: Responder = Arc::new(|_m, _p, _b| Answer::Hang("engine-dropped"));
    let f = front(Some("true"), lease_node(), hang).await;
    let (s, v) = f
        .call(
            "POST",
            "/api/projects/p1/research",
            r#"{"question":"How tall?"}"#,
        )
        .await;
    assert_eq!(s, 202, "{v}");
    let id = v["id"].as_str().unwrap().to_owned();
    assert!(
        settle(|| f.engine.count("POST /v1/chat/completions") >= 1).await,
        "the job reached the engine"
    );
    assert_eq!(f.app.config.research.running(), 1);
    assert_eq!(f.app.config.research.abort_jobs(), 1);
    let done = f.job_until(&id, |j| j["status"] != "running").await;
    assert_ne!(done["status"], "running", "{done}");
    assert!(
        settle(|| f.app.config.research.running() == 0).await,
        "the job left the live set"
    );
    let (s, v) = f
        .call(
            "POST",
            "/api/projects/p1/research",
            r#"{"question":"Again?"}"#,
        )
        .await;
    assert_eq!(s, 202, "the project's slot is free again: {v}");
}

/// The public replacement of the removed `tests/research_jobs.rs` cross-account case: a second
/// administrator with a project of the same id sees none of the first account's jobs. Research is
/// scoped by account first and by project id second, so equal ids must not meet (sbstndalton/noevia#2241).
#[tokio::test]
async fn a_second_account_with_the_same_project_id_cannot_read_list_cancel_or_save_the_first_ones_job(
) {
    let f = front(
        Some("true"),
        lease_node(),
        engine_saying("The tower is 50 metres tall [S1]."),
    )
    .await;
    let (s, v) = f
        .call(
            "POST",
            "/api/projects/p1/research",
            r#"{"question":"How tall is the tower?"}"#,
        )
        .await;
    assert_eq!(s, 202, "{v}");
    let id = v["id"].as_str().unwrap().to_owned();
    let done = f.job_until(&id, |j| j["status"] != "running").await;
    assert_eq!(done["status"], "completed", "{done}");

    // A second administrator, whose own project has the same id.
    let other = "33333333-3333-4333-8333-333333333333";
    let c = Connection::open(server_store::db_path(&f.data)).unwrap();
    c.execute("INSERT INTO users(id,username,username_norm,display_name,role,password_hash,webauthn_user_id,created_at,updated_at) VALUES(?1,'second','second','Synthetic second admin','admin','x',?1,1,1)", [other]).unwrap();
    let now = noevia_server::identity::now_ms();
    c.execute(
        "INSERT INTO sessions(id_hash,user_id,csrf_hash,created_at,last_seen_at,expires_at,user_agent,ip) VALUES(?1,?2,?3,?4,?4,?5,'synthetic','127.0.0.1')",
        (server_auth::js::digest("second-token"), other, server_auth::js::digest("second-csrf"), now, now + 86_400_000),
    ).unwrap();
    seed_project(&f.data.join("users").join(other));
    let jar = "cowork_session=second-token; cowork_csrf=second-csrf";
    let as_second = |method: &'static str, path: String, body: &'static str| {
        let addr = f.addr;
        async move { call(addr, method, &path, jar, "second-csrf", body).await }
    };

    // The project is theirs, so the list is answered, and it is empty.
    let (s, v) = as_second("GET", "/api/projects/p1/research".into(), "").await;
    assert_eq!(s, 200, "{v}");
    assert_eq!(
        v["jobs"],
        json!([]),
        "the first account's job is not listed: {v}"
    );
    assert!(!v.to_string().contains(&id), "{v}");
    // The job's own id is unknown to them: read, cancel and save are all the 404.
    let missing = json!({"error":"No such research job."});
    for (method, tail) in [("GET", ""), ("POST", "/cancel"), ("POST", "/save")] {
        let (s, v) = as_second(
            method,
            format!("/api/projects/p1/research/{id}{tail}"),
            "{}",
        )
        .await;
        assert_eq!((s, &v), (404, &missing), "{method} {tail}: {v}");
    }
    // The first account still has it, untouched by the attempts.
    let (s, v) = f
        .call("GET", &format!("/api/projects/p1/research/{id}"), "")
        .await;
    assert_eq!((s, v["status"].as_str()), (200, Some("completed")), "{v}");
    let (_, list) = f.call("GET", "/api/projects/p1/research", "").await;
    assert_eq!(list["jobs"].as_array().map(Vec::len), Some(1), "{list}");
}

/// #2254: a report cannot silently imply it searched the contents of documents still awaiting extraction.
#[tokio::test]
async fn queued_documents_are_reported_as_unavailable_without_inventing_evidence() {
    let f = front(Some("true"), lease_node(), engine_saying("The tower is 50 metres tall [S1].")).await;
    let file = f.user_dir().join("projects.json");
    let mut projects: Value = serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
    projects["projects"][0]["files"] = json!([
        {"name":"private-title-do-not-inject.pdf","content":" ","document":{"state":"queued"}},
        {"name":"other.pdf","document":{"state":"queued"}},
        {"name":"previous.pdf","content":"Previous readable text", "document":{"state":"queued"}},
        {"name":"failed.pdf","content":"", "document":{"state":"failed"}},
        {"name":"ready.txt","content":"Readable", "document":{"state":"ready"}}
    ]);
    std::fs::write(file, projects.to_string()).unwrap();
    let (s, v) = f.call("POST", "/api/projects/p1/research", r#"{"question":"How tall is the tower?"}"#).await;
    assert_eq!(s, 202, "{v}");
    let id = v["id"].as_str().unwrap();
    let done = f.job_until(id, |j| j["status"] != "running").await;
    assert_eq!(done["status"], "completed", "{done}");
    let md = done["result"]["markdown"].as_str().unwrap();
    assert!(md.contains("2 project documents were waiting to be read"), "{md}");
    assert!(md.contains("not covered by this report"));
    assert!(!md.contains("private-title-do-not-inject"));
    assert_eq!(done["result"]["sources"], 1, "the notice creates no evidence");
    assert!(done["artifacts"].as_array().is_some_and(|a| !a.is_empty()));
}
