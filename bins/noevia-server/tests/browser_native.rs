//! Track R5 (shift 2): Browser task actions through the whole front (`noevia_server::serve`, the
//! router `App` mounts, `browser_routes::serve` behind it). A synthetic core on 127.0.0.1 answers the
//! task facts bridge and records every request it is sent. Unset, `NOEVIA_RUST_BROWSER` leaves every
//! Browser action with core; set, the front answers them in core's order, and refuses to write beside
//! a core that still runs Browser tasks itself. QA-named data only; no network beyond loopback.
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
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

const SCHEMA: &str = include_str!("../../../crates/server-store/tests/fixtures/node-schema.sql");
const STORE_TOKEN: &str = "synthetic-store-token-0123456789abcdef";
const START: &str = "0123456789abcdef0123456789abcdef";
const OWNER: &str = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa";
const MEMBER: &str = "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb";
const PROJECT: &str = "qa-project";
const UNKNOWN: &str = "0000000f-0000-4000-8000-00000000ffff";

fn hex(s: &str) -> String {
    Sha256::digest(s.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// What the synthetic core answers on the facts bridge, and every request it was sent.
#[derive(Clone)]
struct Core {
    facts: Arc<Mutex<Value>>,
    seen: Arc<Mutex<Vec<(String, String, Value)>>>,
}

async fn read_request(c: &mut TcpStream) -> Option<(String, String, Vec<u8>)> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 8192];
    loop {
        let n = c.read(&mut chunk).await.ok()?;
        if n == 0 {
            return None;
        }
        buf.extend_from_slice(&chunk[..n]);
        let Some(end) = buf.windows(4).position(|w| w == b"\r\n\r\n") else {
            continue;
        };
        let head = String::from_utf8_lossy(&buf[..end]).to_string();
        let want = head
            .lines()
            .find_map(|l| {
                l.to_ascii_lowercase()
                    .strip_prefix("content-length:")
                    .map(|v| v.trim().parse::<usize>().unwrap())
            })
            .unwrap_or(0);
        if buf.len() >= end + 4 + want {
            let mut first = head.lines().next()?.split(' ');
            let (m, p) = (first.next()?.to_string(), first.next()?.to_string());
            return Some((m, p, buf[end + 4..end + 4 + want].to_vec()));
        }
    }
}

async fn core(core: Core) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        while let Ok((mut c, _)) = listener.accept().await {
            let core = core.clone();
            tokio::spawn(async move {
                let Some((method, path, body)) = read_request(&mut c).await else {
                    return;
                };
                let body: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
                let text = if path == "/internal/tasks/facts" {
                    core.facts.lock().unwrap().to_string()
                } else {
                    json!({"error": "QA core answered this", "path": path}).to_string()
                };
                let status = if path == "/internal/tasks/facts" {
                    200
                } else {
                    418
                };
                core.seen.lock().unwrap().push((method, path, body));
                let out = format!("HTTP/1.1 {status} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{text}", text.len());
                let _ = c.write_all(out.as_bytes()).await;
            });
        }
    });
    addr
}

fn seed(data: &Path) {
    let db = Connection::open(server_store::db_path(data)).unwrap();
    db.execute_batch(SCHEMA).unwrap();
    let now = noevia_server::identity::now_ms();
    for (uid, name, role) in [
        (OWNER, "QA Owner", "admin"),
        (MEMBER, "QA Member", "member"),
    ] {
        db.execute(
            "INSERT INTO users(id,username,username_norm,display_name,role,password_hash,webauthn_user_id,created_at,updated_at) VALUES(?1,?2,lower(?2),?2,?3,'x',?1,1,1)",
            (uid, name, role),
        )
        .unwrap();
        db.execute(
            "INSERT INTO sessions VALUES(?1,?2,?3,?4,?4,?5,'ua','127.0.0.1')",
            (
                hex(&format!("qa-token-{uid}")),
                uid,
                hex("qa-csrf"),
                now,
                now + 86_400_000,
            ),
        )
        .unwrap();
        let home = data.join("users").join(uid);
        std::fs::create_dir_all(&home).unwrap();
        std::fs::write(
            home.join("projects.json"),
            format!(r#"{{"projects":[{{"id":"{PROJECT}","name":"QA project","goal":"g","instructions":"i","pinned":false,"archived":false,"sourceFolders":[],"memories":[],"files":[],"routing":"auto","modes":["chat"],"toolboxes":["core"],"toolsMode":"auto","chats":[],"createdAt":1,"updatedAt":2}}]}}"#),
        )
        .unwrap();
    }
}

struct Rig {
    _dir: tempfile::TempDir,
    data: PathBuf,
    addr: SocketAddr,
    core: Core,
}

async fn rig(switch: bool, facts: Value) -> Rig {
    let dir = tempfile::tempdir().unwrap();
    let base = std::fs::canonicalize(dir.path()).unwrap();
    let data = base.join("data");
    std::fs::create_dir_all(&data).unwrap();
    seed(&data);
    let c = Core {
        facts: Arc::new(Mutex::new(facts)),
        seen: Arc::default(),
    };
    let core_addr = core(c.clone()).await;
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let mut env: HashMap<&str, String> = HashMap::from([
        ("NOEVIA_LEGACY_UPSTREAM", format!("http://{core_addr}")),
        ("UI_PORT", "1".into()),
        ("UI_DATA_DIR", data.display().to_string()),
        ("NOEVIA_WEB_DIST", data.join("dist").display().to_string()),
        ("PUBLIC_ORIGIN", format!("http://{addr}")),
        ("NOEVIA_START_ID", START.into()),
        ("NOEVIA_PROJECT_STORE_TOKEN", STORE_TOKEN.into()),
        ("PATH", std::env::var("PATH").unwrap_or_default()),
        ("HOME", base.display().to_string()),
    ]);
    if switch {
        env.insert("NOEVIA_RUST_BROWSER", "1".into());
    }
    let app = noevia_server::App::new(Config::from_lookup(|k| env.get(k).cloned()).unwrap());
    tokio::spawn(serve(
        listener,
        app,
        Limits::default(),
        std::future::pending(),
    ));
    Rig {
        _dir: dir,
        data,
        addr,
        core: c,
    }
}

impl Rig {
    async fn call(&self, user: &str, method: &str, path: &str, body: Option<&str>) -> (u16, Value) {
        let body = body.unwrap_or("");
        let mut s = TcpStream::connect(self.addr).await.unwrap();
        let req = format!(
            "{method} {path} HTTP/1.1\r\nHost: {a}\r\nConnection: close\r\nCookie: cowork_session=qa-token-{user}; cowork_csrf=qa-csrf\r\nOrigin: http://{a}\r\nX-CSRF-Token: qa-csrf\r\nContent-Type: application/json\r\nContent-Length: {n}\r\n\r\n{body}",
            a = self.addr,
            n = body.len()
        );
        s.write_all(req.as_bytes()).await.unwrap();
        let mut out = Vec::new();
        let _ = s.read_to_end(&mut out).await;
        let end = out.windows(4).position(|w| w == b"\r\n\r\n").unwrap();
        let head = String::from_utf8_lossy(&out[..end]).to_ascii_lowercase();
        let status = head[9..12].parse().unwrap();
        (
            status,
            serde_json::from_slice(&out[end + 4..]).unwrap_or(Value::Null),
        )
    }

    fn browser(tail: &str) -> String {
        format!("/api/projects/{PROJECT}/browser{tail}")
    }

    /// The requests core was sent that were not the facts bridge.
    fn forwarded(&self) -> Vec<(String, String)> {
        self.core
            .seen
            .lock()
            .unwrap()
            .iter()
            .filter(|(_, p, _)| p != "/internal/tasks/facts")
            .map(|(m, p, _)| (m.clone(), p.clone()))
            .collect()
    }

    fn journals(&self) -> usize {
        std::fs::read_dir(self.data.join("users").join(OWNER).join("jobs")).map_or(0, |d| d.count())
    }
}

#[tokio::test]
async fn unset_every_browser_action_stays_with_core() {
    let rig = rig(false, json!({"enabled": true, "owner": "front"})).await;
    let start = r#"{"domains":["shop.example.test"]}"#;
    assert_eq!(
        rig.call(OWNER, "POST", &Rig::browser(""), Some(start))
            .await
            .0,
        418
    );
    assert_eq!(
        rig.call(
            OWNER,
            "POST",
            &Rig::browser(&format!("/{UNKNOWN}/cancel")),
            None
        )
        .await
        .0,
        418
    );
    assert_eq!(
        rig.forwarded(),
        vec![
            ("POST".to_string(), Rig::browser("")),
            (
                "POST".to_string(),
                Rig::browser(&format!("/{UNKNOWN}/cancel"))
            ),
        ]
    );
    assert_eq!(rig.journals(), 0);
}

#[tokio::test]
async fn set_the_front_answers_in_cores_order() {
    let rig = rig(true, json!({"enabled": true, "owner": "front"})).await;
    let start = r#"{"domains":["shop.example.test"]}"#;
    // The member: the flag is on, the role is not (the bridge answers a member `{enabled: true}`).
    *rig.core.facts.lock().unwrap() = json!({"enabled": true});
    assert_eq!(
        rig.call(MEMBER, "POST", &Rig::browser(""), Some(start))
            .await,
        (403, json!({"error": "Administrator required"}))
    );
    *rig.core.facts.lock().unwrap() = json!({"enabled": false});
    assert_eq!(
        rig.call(OWNER, "POST", &Rig::browser(""), Some(start))
            .await,
        (
            404,
            json!({"error": "Browser mode is not enabled on this server."})
        )
    );
    *rig.core.facts.lock().unwrap() = json!({"enabled": true, "owner": "front"});
    assert_eq!(
        rig.call(OWNER, "POST", "/api/projects/nope/browser", Some(start))
            .await,
        (404, json!({"error": "project not found"}))
    );
    assert_eq!(
        rig.call(OWNER, "PUT", &Rig::browser(""), Some(start)).await,
        (405, json!({"error": "method not allowed"}))
    );
    assert_eq!(
        rig.call(OWNER, "POST", &Rig::browser(""), Some("{bad"))
            .await,
        (400, json!({"error": "invalid JSON"}))
    );
    assert_eq!(
        rig.call(OWNER, "POST", &Rig::browser(""), Some(r#"{"domains":[]}"#))
            .await,
        (
            400,
            json!({"error": "A browser task needs at least one domain it may reach."})
        )
    );
    // No egress proxy on this deployment: refused before any job is made, as core does.
    assert_eq!(
        rig.call(OWNER, "POST", &Rig::browser(""), Some(start))
            .await,
        (
            503,
            json!({"error": "Browser tasks need the egress proxy (D15) configured on this server."})
        )
    );
    for action in ["act", "approve", "finish", "cancel"] {
        assert_eq!(
            rig.call(
                OWNER,
                "POST",
                &Rig::browser(&format!("/{UNKNOWN}/{action}")),
                Some("{}")
            )
            .await,
            (404, json!({"error": "Task not found"})),
            "{action}"
        );
    }
    // The reads stay task_routes.rs's, with the front's own facts.
    let (status, state) = rig.call(OWNER, "GET", &Rig::browser(""), None).await;
    assert_eq!(status, 200, "{state}");
    assert_eq!(
        rig.forwarded(),
        Vec::<(String, String)>::new(),
        "nothing reached core but the facts bridge"
    );
    assert_eq!(rig.journals(), 0);
}

#[tokio::test]
async fn set_beside_a_core_that_still_runs_browser_tasks_it_writes_nothing() {
    let rig = rig(true, json!({"enabled": true, "facts": {"approvals": {}}})).await;
    assert_eq!(
        rig.call(
            OWNER,
            "POST",
            &Rig::browser(""),
            Some(r#"{"domains":["shop.example.test"]}"#)
        )
        .await,
        (
            503,
            json!({"error": "Browser tasks are unavailable right now. Try again shortly."})
        )
    );
    assert_eq!(rig.forwarded(), Vec::<(String, String)>::new());
    assert_eq!(rig.journals(), 0);
}
