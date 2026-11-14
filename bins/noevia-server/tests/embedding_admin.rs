//! Track R11 (S10b): `/api/embedding` through the whole front, with `NOEVIA_RUST_EMBEDDING_ADMIN`
//! unset (relayed to a synthetic Node) and `rust` (answered by the front over a temp data root).
//! Scripted local listeners stand in for Node, the embeddings server, the model manager and the
//! Diary sidecar; the native worker is represented by the status file it would write. QA-named
//! data, loopback addresses only; no engine, no model loads.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]

use embedding_worker::{fill::Store as _, sqlite::SqlStore, Control, Index};
use noevia_server::{
    config::Config,
    serve::{serve, Limits},
    App,
};
use serde_json::{json, Value};
use server_store::rusqlite::{params, Connection};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

const A: &str = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa";
const B: &str = "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb";
const ADMIN: &str = "Cookie: cowork_session=admin-token; cowork_csrf=csrf\r\n";
const MEMBER: &str = "Cookie: cowork_session=member-token; cowork_csrf=csrf\r\n";
const SECRET: &str = "synthetic-store-token-0123456789abcdef";
const START: &str = "0123456789abcdef0123456789abcdef";
const LOADER_TOKEN: &str = "qa-loader-token";
const DIARY_TOKEN: &str = "qa-diary-token";

fn hex(s: &str) -> String {
    Sha256::digest(s.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn now() -> u64 {
    noevia_server::identity::now_ms() as u64
}

struct Answer {
    status: u16,
    head: String,
    body: Value,
}

async fn call(addr: SocketAddr, method: &str, path: &str, who: &str, body: &str) -> Answer {
    let mut s = TcpStream::connect(addr).await.unwrap();
    s.write_all(
        format!(
            "{method} {path} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n{who}Origin: http://{addr}\r\nX-CSRF-Token: csrf\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        )
        .as_bytes(),
    )
    .await
    .unwrap();
    let mut bytes = Vec::new();
    tokio::time::timeout(Duration::from_secs(30), s.read_to_end(&mut bytes))
        .await
        .unwrap()
        .unwrap();
    let text = String::from_utf8(bytes).unwrap();
    let (head, b) = text.split_once("\r\n\r\n").unwrap();
    Answer {
        status: head.split_whitespace().nth(1).unwrap().parse().unwrap(),
        head: head.to_ascii_lowercase(),
        body: serde_json::from_str(b).unwrap_or(Value::Null),
    }
}

async fn post(addr: SocketAddr, body: Value) -> Answer {
    call(addr, "POST", "/api/embedding", ADMIN, &body.to_string()).await
}

fn seed(dir: &Path) -> Connection {
    let db = Connection::open(dir.join("cowork.db")).unwrap();
    db.execute_batch(include_str!(
        "../../../crates/server-store/tests/fixtures/node-schema.sql"
    ))
    .unwrap();
    let at = noevia_server::identity::now_ms();
    for (uid, role, token) in [(A, "admin", "admin-token"), (B, "member", "member-token")] {
        db.execute("INSERT INTO users(id,username,username_norm,display_name,role,password_hash,webauthn_user_id,created_at,updated_at) VALUES(?1,?1,?1,'QA',?2,'x',?1,1,1)", (uid, role)).unwrap();
        db.execute(
            "INSERT INTO sessions VALUES(?1,?2,?3,?4,?4,?5,'ua','127.0.0.1')",
            (hex(token), uid, hex("csrf"), at, at + 86_400_000),
        )
        .unwrap();
    }
    db
}

type Seen = Arc<Mutex<Vec<String>>>;

/// An HTTP/1.1 stub: `route(request)` picks the reply; every request is kept.
async fn stub(route: impl Fn(&str) -> (u16, String) + Send + Sync + 'static) -> (String, Seen) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
    let seen: Seen = Arc::default();
    let (kept, route) = (Arc::clone(&seen), Arc::new(route));
    tokio::spawn(async move {
        while let Ok((mut c, _)) = listener.accept().await {
            let (kept, route) = (Arc::clone(&kept), Arc::clone(&route));
            tokio::spawn(async move {
                let mut buf = Vec::new();
                let mut chunk = [0u8; 8192];
                let request = loop {
                    let Ok(n) = c.read(&mut chunk).await else {
                        return;
                    };
                    if n == 0 {
                        return;
                    }
                    buf.extend_from_slice(&chunk[..n]);
                    let t = String::from_utf8_lossy(&buf).into_owned();
                    if let Some((h, b)) = t.split_once("\r\n\r\n") {
                        let want = h
                            .lines()
                            .find_map(|l| {
                                l.to_ascii_lowercase()
                                    .strip_prefix("content-length: ")
                                    .map(|v| v.parse::<usize>().unwrap())
                            })
                            .unwrap_or(0);
                        if b.len() >= want {
                            break t;
                        }
                    }
                };
                kept.lock().unwrap().push(request.clone());
                let (code, text) = route(&request);
                let out = format!("HTTP/1.1 {code} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{text}", text.len());
                let _ = c.write_all(out.as_bytes()).await;
            });
        }
    });
    (base, seen)
}

fn body_of(request: &str) -> Value {
    serde_json::from_str(request.split_once("\r\n\r\n").unwrap().1).unwrap()
}

/// An embeddings server listing `qa-embed-a` and `qa-embed-b`; every input embeds as `vector`.
async fn embeddings(vector: Value) -> (String, Seen) {
    stub(move |req| {
        if req.starts_with("GET /v1/models ") {
            return (
                200,
                json!({"data": [{"id": "qa-embed-a"}, {"id": "qa-embed-b"}]}).to_string(),
            );
        }
        if req.starts_with("POST /v1/embeddings ") {
            let n = body_of(req)["input"].as_array().unwrap().len();
            let data: Vec<Value> = (0..n)
                .map(|i| json!({"index": i, "embedding": vector}))
                .collect();
            return (200, json!({ "data": data }).to_string());
        }
        (404, "{}".into())
    })
    .await
}

fn file(model: &str) -> String {
    format!("qa/{model}.gguf")
}

/// A model manager: the embedding service it runs (`loads`: whether a newly served model ever
/// reports loaded) and its installed models.
async fn loader(loads: bool) -> (String, Seen) {
    let served = Arc::new(Mutex::new(vec!["qa-embed-a".to_string()]));
    let loaded = Arc::new(Mutex::new(vec!["qa-embed-a".to_string()]));
    stub(move |req| {
        let state = |served: &[String], loaded: &[String]| {
            let models: Vec<Value> = served
                .iter()
                .map(|m| json!({"model": m, "file": file(m)}))
                .collect();
            json!({"managed": true, "enabled": !served.is_empty(), "models": models, "previous": [], "running": {"loaded": loaded}}).to_string()
        };
        if req.starts_with("PUT /api/v1/embedding-service ") {
            let wanted: Vec<String> = body_of(req)["models"]
                .as_array()
                .unwrap()
                .iter()
                .map(|m| m["model"].as_str().unwrap().to_string())
                .collect();
            *served.lock().unwrap() = wanted.clone();
            let mut l = loaded.lock().unwrap();
            l.retain(|m| wanted.contains(m));
            return (200, state(&wanted, &l));
        }
        if req.starts_with("GET /api/v1/embedding-service ") {
            let s = served.lock().unwrap().clone();
            if loads {
                *loaded.lock().unwrap() = s.clone();
            }
            return (200, state(&s, &loaded.lock().unwrap()));
        }
        if req.starts_with("GET /api/v1/models ") {
            return (200, json!([{"modelId": "qa-embed-b", "file": file("qa-embed-b")}, {"modelId": "qa-embed-c", "file": "qa/qa-embed-c.bin"}]).to_string());
        }
        (404, "{}".into())
    })
    .await
}

/// A Diary sidecar whose embedding settings use `qa-embed-a` at `base`.
async fn diary(base: &str) -> (String, Seen) {
    let base = base.to_string();
    stub(move |req| {
        if req.starts_with("GET /api/internal/embedding ") {
            return (200, json!({"active": {"model": "qa-embed-a", "profile": "nomic-v1", "base_url": base}, "next": null, "previous": null}).to_string());
        }
        if req.starts_with("POST /api/internal/embedding ") {
            return (200, json!({"ok": true, "action": body_of(req)["action"]}).to_string());
        }
        (404, "{}".into())
    })
    .await
}

struct World {
    dir: tempfile::TempDir,
    db: Connection,
    addr: SocketAddr,
    node: Seen,
    embed: String,
}

async fn world(on: bool, extra: &[(&str, String)]) -> World {
    let dir = tempfile::tempdir().unwrap();
    let db = seed(dir.path());
    let (node, node_seen) = stub(|_| (202, r#"{"relayed":true}"#.to_owned())).await;
    let (embed, _) = embeddings(json!([0.25, 0.5, 0.75])).await;
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let mut env: HashMap<String, String> = [
        ("NOEVIA_LEGACY_UPSTREAM", node),
        ("UI_DATA_DIR", dir.path().display().to_string()),
        ("PUBLIC_ORIGIN", format!("http://{addr}")),
        ("NOEVIA_PROJECT_STORE_TOKEN", SECRET.to_owned()),
        ("NOEVIA_START_ID", START.to_owned()),
        ("EMBEDDING_MODEL", "qa-embed-a".to_owned()),
        ("EMBEDDING_BASE_URL", embed.clone()),
        ("DIARY_BASE_URL", "http://127.0.0.1:9".to_owned()),
        ("DIARY_AUTH_TOKEN", DIARY_TOKEN.to_owned()),
        (
            "DIARY_TENANT_KEY",
            "qa-tenant-key-0123456789abcdef0123456789".to_owned(),
        ),
        ("MODEL_LOADER_TOKEN", LOADER_TOKEN.to_owned()),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_owned(), v))
    .collect();
    if on {
        env.insert("NOEVIA_RUST_EMBEDDING_ADMIN".into(), "rust".into());
    }
    for (k, v) in extra {
        env.insert((*k).to_owned(), v.clone());
    }
    let mut config = Config::from_lookup(|k| env.get(k).cloned()).unwrap();
    config.embedding_admin.wait_ms = 400;
    config.embedding_admin.poll_ms = 20;
    tokio::spawn(serve(
        listener,
        App::new(config),
        Limits::default(),
        std::future::pending(),
    ));
    World {
        dir,
        db,
        addr,
        node: node_seen,
        embed,
    }
}

impl World {
    fn path(&self, name: &str) -> std::path::PathBuf {
        self.dir.path().join(name)
    }
    fn saved(&self) -> Option<Value> {
        std::fs::read(self.path("embedding-config.json"))
            .ok()
            .map(|b| serde_json::from_slice(&b).unwrap())
    }
    fn target(&self, model: &str) -> Value {
        json!({"baseUrl": self.embed, "model": model, "profile": "nomic-v1"})
    }
    fn save(&self, active: &str, next: Option<&str>, previous: Option<&str>) {
        let t = |m: Option<&str>| m.map_or(Value::Null, |m| self.target(m));
        let state = json!({"version": 1, "active": self.target(active), "next": t(next), "previous": t(previous)});
        std::fs::write(self.path("embedding-config.json"), state.to_string()).unwrap();
    }
    fn kicked(&self) -> Option<u64> {
        let v: Value = serde_json::from_slice(
            &std::fs::read(self.path("embedding-worker-request.json")).ok()?,
        )
        .unwrap();
        assert_eq!(v["start"], START);
        v["sequence"].as_u64()
    }
    fn audits(&self, action: &str) -> Vec<(String, Option<String>, Value)> {
        let mut q = self
            .db
            .prepare("SELECT actor_user_id, target_user_id, detail FROM audit_events WHERE action=?1 ORDER BY id")
            .unwrap();
        q.query_map([action], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get::<_, String>(2)?))
        })
        .unwrap()
        .map(|r| {
            let (a, t, d) = r.unwrap();
            (a, t, serde_json::from_str(&d).unwrap())
        })
        .collect()
    }
    /// The config lock is free (no request left it held).
    fn lock_free(&self) -> bool {
        let lock = std::fs::File::open(self.path("embedding-config.json.lock"));
        lock.is_err() || lock.unwrap().try_lock().is_ok()
    }
    /// The native worker's status file, `service` its view of the embedding service.
    fn worker_status(&self, service: Option<Value>) {
        let at = now();
        let s = json!({"protocol": "rust-embedding-worker-v1", "start": START, "at": at, "running": false, "lastPass": null, "nextServed": null, "configValid": true, "configInvalid": false, "service": service, "serviceAt": at});
        std::fs::write(self.path("embedding-worker-status.json"), s.to_string()).unwrap();
    }
    fn store(&self) -> SqlStore {
        SqlStore {
            rag_dir: None,
            root: self.dir.path().into(),
            seed: embedding_worker::runtime::seed(|_| None).unwrap(),
            open_vec: embedding_worker::sqlite::open_vec_required,
        }
    }
    /// A project index of `A` with two embeddable chunks, `slot`'s vectors for both.
    fn index_with(&self, slot: &str) -> Index {
        embedding_worker::sqlite_vec_loader::register().unwrap();
        let dir = self.dir.path().join("users").join(A);
        std::fs::create_dir_all(dir.join("rag")).unwrap();
        std::fs::write(
            dir.join("projects.json"),
            json!({"projects": [{"id": "qa-project"}]}).to_string(),
        )
        .unwrap();
        server_projects::outbox::enqueue(&dir, &[], 0).unwrap();
        let path = dir.join("rag/qa-project.db");
        if !path.exists() {
            let db = Connection::open(&path).unwrap();
            db.execute_batch("PRAGMA journal_mode=WAL;CREATE TABLE chunks(id INTEGER PRIMARY KEY,file TEXT NOT NULL,chunk_no INTEGER NOT NULL,body TEXT NOT NULL,content_hash TEXT NOT NULL,updated_at TEXT NOT NULL DEFAULT (datetime('now')),version TEXT,direct INTEGER,kind TEXT,meta TEXT);CREATE UNIQUE INDEX idx_chunks_file_hash ON chunks(file,content_hash);CREATE TABLE vec_meta(key TEXT PRIMARY KEY,value TEXT)").unwrap();
            for id in [1, 2] {
                db.execute("INSERT INTO chunks(id,file,chunk_no,body,content_hash,direct,kind) VALUES(?,'qa.md',?,'qa synthetic chunk',?,0,'text')", params![id, id, format!("qa-hash{id}")]).unwrap();
            }
        }
        let index = Index {
            user: Some(A.into()),
            project: "qa-project".into(),
        };
        let target = embedding_worker::Target {
            profile: "nomic-v1".into(),
            model: slot.into(),
            base_url: Some(self.embed.clone()),
        };
        let store = self.store();
        let version = store.config().map(|c| c.version).unwrap_or(0);
        let rows = store
            .missing(&index, &target, 4, &Default::default())
            .unwrap()
            .unwrap();
        let vectors = vec![vec![1.0, 0.0]; rows.len()];
        store
            .publish(
                &index,
                &target,
                &rows,
                &vectors,
                version,
                &Control::default(),
            )
            .unwrap();
        index
    }
    fn slot(&self, index: &Index, model: &str) -> Option<(usize, usize)> {
        let target = embedding_worker::Target {
            profile: "nomic-v1".into(),
            model: model.into(),
            base_url: None,
        };
        let path = self.store().index_path(index).unwrap();
        let db = embedding_worker::sqlite::open_vec_required(&path).unwrap();
        let key = format!("slot:{}", embedding_worker::sqlite::slot_id(&target));
        let found: Option<String> = db
            .query_row("SELECT value FROM vec_meta WHERE key=?", [key], |r| {
                r.get(0)
            })
            .ok();
        found.map(|_| self.store().coverage(index, &target).unwrap())
    }
}

fn admin(w: &World) -> (&'static str, SocketAddr) {
    (ADMIN, w.addr)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn switch_unset_every_method_is_node_s() {
    let w = world(false, &[]).await;
    for method in ["GET", "POST", "PUT", "DELETE"] {
        let a = call(
            w.addr,
            method,
            "/api/embedding",
            ADMIN,
            r#"{"action":"finish"}"#,
        )
        .await;
        assert_eq!(
            (a.status, a.body),
            (202, json!({"relayed": true})),
            "{method}"
        );
    }
    assert_eq!(w.node.lock().unwrap().len(), 4);
    assert_eq!(w.saved(), None);
    assert!(w.audits("embedding.finish").is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn under_the_switch_every_method_is_the_front_s() {
    let w = world(true, &[]).await;
    let anonymous = call(w.addr, "GET", "/api/embedding", "", "").await;
    // core's gate refuses before the route (whose own `Sign in required` is never reached).
    assert_eq!(
        (anonymous.status, anonymous.body),
        (401, json!({"error": "unauthorized"}))
    );
    for method in ["GET", "POST", "DELETE"] {
        let a = call(w.addr, method, "/api/embedding", MEMBER, "{}").await;
        assert_eq!(
            (a.status, &a.body),
            (403, &json!({"error": "Administrator required"})),
            "{method}"
        );
        assert!(a.head.contains("cache-control: no-store"));
    }
    let (who, addr) = admin(&w);
    for method in ["PUT", "DELETE", "PATCH", "OPTIONS"] {
        let a = call(addr, method, "/api/embedding", who, "{}").await;
        assert_eq!(
            (a.status, &a.body),
            (405, &json!({"error": "Method not allowed"})),
            "{method}"
        );
        assert!(a.head.contains("cache-control: no-store"));
    }
    let bad = call(addr, "POST", "/api/embedding", who, "{not json").await;
    assert_eq!(
        (bad.status, bad.body),
        (400, json!({"error": "invalid JSON"}))
    );
    let big = format!(
        "{{\"action\":\"finish\",\"pad\":\"{}\"}}",
        "q".repeat(1024 * 1024)
    );
    let bad = call(addr, "POST", "/api/embedding", who, &big).await;
    assert_eq!(
        (bad.status, bad.body),
        (400, json!({"error": "invalid JSON"}))
    );
    let unknown = post(addr, json!({"action": "qa-unknown"})).await;
    assert_eq!(unknown.status, 400);
    assert!(unknown.body["error"]
        .as_str()
        .unwrap()
        .starts_with("action must be one of"));
    // The WHATWG pathname is the route's too.
    let dotted = call(addr, "GET", "/api/x/../embedding", MEMBER, "").await;
    assert_eq!(dotted.status, 403);
    // A path beside it stays Node's.
    let beside = call(addr, "GET", "/api/embedding/qa", who, "").await;
    assert_eq!(beside.status, 202);
    assert_eq!(w.node.lock().unwrap().len(), 1);
    assert!(w.lock_free());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn get_reports_the_projects_the_diary_and_the_service() {
    let (diary_base, diary_seen) = diary("").await;
    let w = world(true, &[("DIARY_BASE_URL", diary_base)]).await;
    let index = w.index_with("qa-embed-a");
    // No worker status and no Diary call yet: the status is stale, no service.
    let service = json!({"managed": true, "enabled": true, "models": [{"model": "qa-embed-a", "file": file("qa-embed-a")}], "previous": [], "running": {"loaded": ["qa-embed-a"]}});
    w.worker_status(Some(service.clone()));
    let a = call(w.addr, "GET", "/api/embedding", ADMIN, "").await;
    assert_eq!(a.status, 200, "{}", a.body);
    assert!(a.head.contains("cache-control: no-store"));
    let p = &a.body["projects"];
    assert_eq!(p["active"], w.target("qa-embed-a"));
    assert_eq!(
        (&p["next"], &p["previous"], &p["source"]),
        (&Value::Null, &Value::Null, &json!("environment"))
    );
    assert_eq!(
        p["coverage"]["active"],
        json!({"indexes": 1, "chunks": 2, "vectors": 2, "unavailable": 0, "complete": true, "ready": true})
    );
    assert_eq!(
        (&p["stale"], &p["configValid"], &p["running"]),
        (&json!(false), &json!(true), &json!(false))
    );
    assert_eq!((&p["duty"], &p["batch"]), (&json!(0.5), &json!(4)));
    assert_eq!(a.body["diary"]["active"]["model"], "qa-embed-a");
    let mut shown = service.clone();
    shown["stale"] = json!(false);
    assert_eq!(a.body["service"], shown);
    // The Diary is asked with the service bearer only: no tenant, assertion or storage credential.
    let asked = diary_seen.lock().unwrap()[0].to_ascii_lowercase();
    assert!(asked.starts_with("get /api/internal/embedding "));
    assert!(asked.contains(&format!("authorization: bearer {DIARY_TOKEN}")));
    assert!(!asked.contains("x-cowork-"), "{asked}");
    // A worker status of another start is not current: stale, and no service.
    std::fs::write(w.path("embedding-worker-status.json"), json!({"protocol": "rust-embedding-worker-v1", "start": "ffffffffffffffffffffffffffffffff", "at": now()}).to_string()).unwrap();
    let w2 = world(true, &[]).await;
    std::fs::copy(
        w.path("embedding-worker-status.json"),
        w2.path("embedding-worker-status.json"),
    )
    .unwrap();
    let b = call(w2.addr, "GET", "/api/embedding", ADMIN, "").await;
    assert_eq!(b.body["projects"]["stale"], true);
    assert!(b.body.get("service").is_none());
    let _ = index;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn set_next_switch_rollback_and_finish_save_the_file_kick_and_audit() {
    let w = world(true, &[]).await;
    let addr = w.addr;
    let set = post(addr, json!({"action": "set-next", "scope": "projects", "target": {"baseUrl": w.embed, "model": "qa-embed-b"}})).await;
    assert_eq!(set.status, 200, "{}", set.body);
    assert_eq!(set.body["projects"]["next"], w.target("qa-embed-b"));
    assert!(set.body.get("diary").is_none());
    assert_eq!(
        w.saved().unwrap(),
        json!({"version": 1, "active": w.target("qa-embed-a"), "next": w.target("qa-embed-b"), "previous": null})
    );
    // Node's two-space JSON, mode 0600.
    let text = std::fs::read_to_string(w.path("embedding-config.json")).unwrap();
    assert!(text.starts_with("{\n  \"version\": 1,"));
    use std::os::unix::fs::PermissionsExt;
    let mode = std::fs::metadata(w.path("embedding-config.json"))
        .unwrap()
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o600);
    let first = w.kicked().unwrap();
    assert!(first >= now() - 60_000);
    assert_eq!(
        w.audits("embedding.set-next"),
        vec![(
            A.to_string(),
            None,
            json!({"scope": "projects", "model": "qa-embed-b", "profile": null, "force": false, "projects": "ok", "diary": "skipped"})
        )]
    );
    assert!(w.lock_free());

    let switched = post(addr, json!({"action": "switch", "scope": "projects"})).await;
    assert_eq!(switched.status, 200, "{}", switched.body);
    assert_eq!(
        w.saved().unwrap(),
        json!({"version": 1, "active": w.target("qa-embed-b"), "next": null, "previous": w.target("qa-embed-a")})
    );
    assert!(w.kicked().unwrap() > first);
    let back = post(addr, json!({"action": "rollback", "scope": "projects"})).await;
    assert_eq!(back.status, 200, "{}", back.body);
    assert_eq!(
        w.saved().unwrap(),
        json!({"version": 1, "active": w.target("qa-embed-a"), "next": null, "previous": w.target("qa-embed-b")})
    );
    let done = post(addr, json!({"action": "finish", "scope": "projects"})).await;
    assert_eq!(done.status, 200, "{}", done.body);
    assert_eq!(done.body["projects"]["source"], "saved");
    assert_eq!(
        w.saved().unwrap(),
        json!({"version": 1, "active": w.target("qa-embed-a"), "next": null, "previous": null})
    );
    for action in ["switch", "rollback", "finish"] {
        let rows = w.audits(&format!("embedding.{action}"));
        assert_eq!(rows.len(), 1, "{action}");
        assert_eq!(rows[0].2["projects"], "ok");
    }
    // Nothing was relayed, and the lock is free after every change.
    assert!(w.node.lock().unwrap().is_empty());
    assert!(w.lock_free());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn clear_next_and_rollback_drop_the_abandoned_vectors_after_the_hold() {
    let w = world(true, &[]).await;
    w.save("qa-embed-a", Some("qa-embed-c"), None);
    let index = w.index_with("qa-embed-c");
    let kept = w.index_with("qa-embed-a");
    assert_eq!(w.slot(&index, "qa-embed-c"), Some((2, 2)));
    let cleared = post(w.addr, json!({"action": "clear-next", "scope": "projects"})).await;
    assert_eq!(cleared.status, 200, "{}", cleared.body);
    assert_eq!(cleared.body["projects"]["next"], Value::Null);
    assert_eq!(
        w.slot(&index, "qa-embed-c"),
        None,
        "the abandoned next's slot is dropped"
    );
    assert_eq!(
        w.slot(&kept, "qa-embed-a"),
        Some((2, 2)),
        "the active slot stays"
    );
    assert!(w.lock_free());

    // A rollback drops the next it abandons too, and keeps both the targets it swaps.
    w.save("qa-embed-b", Some("qa-embed-c"), Some("qa-embed-a"));
    w.index_with("qa-embed-c");
    w.index_with("qa-embed-b");
    let back = post(w.addr, json!({"action": "rollback", "scope": "projects"})).await;
    assert_eq!(back.status, 200, "{}", back.body);
    assert_eq!(
        w.saved().unwrap(),
        json!({"version": 1, "active": w.target("qa-embed-a"), "next": null, "previous": w.target("qa-embed-b")})
    );
    assert_eq!(w.slot(&index, "qa-embed-c"), None);
    assert_eq!(w.slot(&index, "qa-embed-b"), Some((2, 2)));
    assert_eq!(w.slot(&index, "qa-embed-a"), Some((2, 2)));
    // Finish then drops the previous model's vectors.
    let done = post(w.addr, json!({"action": "finish", "scope": "projects"})).await;
    assert_eq!(done.status, 200, "{}", done.body);
    assert_eq!(w.slot(&index, "qa-embed-b"), None);
    assert_eq!(w.slot(&index, "qa-embed-a"), Some((2, 2)));
    assert!(w.lock_free());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn refusals_change_nothing_and_release_the_lock() {
    let w = world(true, &[]).await;
    let addr = w.addr;
    let public = post(addr, json!({"action": "set-next", "target": {"baseUrl": "http://8.8.8.8:8080", "model": "qa-embed-b"}})).await;
    assert_eq!(public.status, 400);
    assert_eq!(
        public.body,
        json!({"error": "The embeddings server must be on this network: it receives the text of every project and the Diary (8.8.8.8 is not on this network (8.8.8.8 is a public address)).", "code": "publicEndpoint"})
    );
    let unserved = post(addr, json!({"action": "set-next", "scope": "projects", "target": {"baseUrl": w.embed, "model": "qa-embed-z"}})).await;
    assert_eq!(unserved.status, 409);
    assert_eq!(
        unserved.body,
        json!({"error": "The embeddings server does not serve qa-embed-z.", "code": "modelNotServed", "served": ["qa-embed-a", "qa-embed-b"]})
    );
    let none = post(addr, json!({"action": "switch", "scope": "projects"})).await;
    assert_eq!(none.status, 409);
    assert_eq!(none.body["projects"]["code"], "noNext");
    assert_eq!(w.saved(), None);
    assert_eq!(w.kicked(), None);
    assert_eq!(w.audits("embedding.switch")[0].2["projects"], "refused");
    assert!(w.lock_free());

    // Another writer holds the config lock: the change waits, then refuses; nothing is written.
    let held = server_projects::lock::acquire(&w.path("embedding-config.json")).unwrap();
    let busy = post(addr, json!({"action": "set-next", "scope": "projects", "target": {"baseUrl": w.embed, "model": "qa-embed-b"}})).await;
    assert_eq!(busy.status, 500);
    assert_eq!(
        busy.body["projects"],
        json!({"error": "Embedding config lock unavailable", "status": 500})
    );
    assert_eq!(w.saved(), None);
    drop(held);
    let ok = post(addr, json!({"action": "set-next", "scope": "projects", "target": {"baseUrl": w.embed, "model": "qa-embed-b"}})).await;
    assert_eq!(ok.status, 200, "{}", ok.body);
    assert!(w.lock_free());

    // A damaged file turns embedding off and refuses every change; the file is left alone.
    std::fs::write(w.path("embedding-config.json"), "{qa damaged").unwrap();
    let damaged = post(addr, json!({"action": "set-next", "scope": "projects", "target": {"baseUrl": w.embed, "model": "qa-embed-b"}})).await;
    assert_eq!(damaged.status, 409);
    assert_eq!(damaged.body["projects"]["code"], "badState");
    let status = call(addr, "GET", "/api/embedding", ADMIN, "").await;
    assert_eq!(status.body["projects"]["active"]["profile"], "unavailable");
    assert!(status.body["projects"]["damaged"]
        .as_str()
        .unwrap()
        .contains("cannot be read (not valid JSON)"));
    assert_eq!(
        std::fs::read_to_string(w.path("embedding-config.json")).unwrap(),
        "{qa damaged"
    );
    assert!(w.lock_free());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_seeded_public_address_is_refused_as_at_start_up() {
    let w = world(
        true,
        &[("EMBEDDING_BASE_URL", "http://8.8.4.4:8080".into())],
    )
    .await;
    let a = call(w.addr, "GET", "/api/embedding", ADMIN, "").await;
    assert_eq!(a.body["projects"]["active"]["profile"], "unavailable");
    assert_eq!(a.body["projects"]["damaged"], "EMBEDDING_BASE_URL http://8.8.4.4:8080 refused: 8.8.4.4 is not on this network (8.8.4.4 is a public address)");
    // core's own answer here: its setNext reaches the configuration outside the hold.
    let set = post(w.addr, json!({"action": "set-next", "scope": "projects", "target": {"baseUrl": w.embed, "model": "qa-embed-b"}})).await;
    assert_eq!(set.status, 500);
    assert_eq!(
        set.body["projects"],
        json!({"error": "Embedding configuration requires the publication lock", "status": 500})
    );
    assert_eq!(w.saved(), None);
    assert!(w.lock_free());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_diary_half_goes_to_the_sidecar_and_an_unreachable_diary_is_a_207() {
    let w = world(true, &[]).await;
    // DIARY_BASE_URL is a closed loopback port.
    let get = call(w.addr, "GET", "/api/embedding", ADMIN, "").await;
    assert_eq!(
        get.body["diary"],
        json!({"error": "The Diary could not be reached (fetch failed)."})
    );
    let set = post(w.addr, json!({"action": "set-next", "target": {"baseUrl": w.embed, "diaryBaseUrl": w.embed, "model": "qa-embed-b"}})).await;
    assert_eq!(set.status, 207, "{}", set.body);
    assert_eq!(
        set.body["diary"],
        json!({"error": "The Diary could not be reached (fetch failed)."})
    );
    assert_eq!(set.body["projects"]["next"]["model"], "qa-embed-b");
    assert_eq!(w.audits("embedding.set-next")[0].2["diary"], "refused");

    let (diary_base, diary_seen) = diary("").await;
    let w = world(true, &[("DIARY_BASE_URL", diary_base)]).await;
    let set = post(w.addr, json!({"action": "set-next", "scope": "diary", "force": true, "target": {"diaryBaseUrl": w.embed, "model": "qa-embed-b", "profile": "nomic-v1"}})).await;
    assert_eq!(set.status, 200, "{}", set.body);
    assert_eq!(
        set.body,
        json!({"diary": {"ok": true, "action": "set-next"}})
    );
    let sent = diary_seen.lock().unwrap().last().cloned().unwrap();
    assert!(sent.starts_with("POST /api/internal/embedding "));
    assert!(!sent.to_ascii_lowercase().contains("x-cowork-"));
    assert_eq!(
        body_of(&sent),
        json!({"action": "set-next", "target": {"model": "qa-embed-b", "profile": "nomic-v1", "base_url": w.embed}, "force": true})
    );
    assert_eq!(
        w.saved(),
        None,
        "a Diary-only change leaves the projects' file alone"
    );
}

/// A world whose active model is on the embedding service the model manager runs.
async fn service_world(loads: bool, diary_on: bool) -> (World, Seen) {
    let (loader_base, loader_seen) = loader(loads).await;
    let (embed, _) = embeddings(json!([0.25, 0.5, 0.75])).await;
    let mut extra = vec![
        ("MODEL_LOADER_URL", loader_base),
        ("EMBEDDING_SERVICE_URL", embed.clone()),
        ("EMBEDDING_BASE_URL", embed.clone()),
    ];
    if diary_on {
        extra.push(("DIARY_BASE_URL", diary(&embed).await.0));
    }
    let mut w = world(true, &extra).await;
    w.embed = embed;
    w.worker_status(Some(json!({"managed": true, "enabled": true, "models": [{"model": "qa-embed-a", "file": file("qa-embed-a")}], "previous": [], "running": {"loaded": ["qa-embed-a"]}})));
    (w, loader_seen)
}

fn puts(seen: &Seen) -> Vec<Value> {
    seen.lock()
        .unwrap()
        .iter()
        .filter(|r| r.starts_with("PUT /api/v1/embedding-service "))
        .map(|r| {
            assert!(r
                .to_ascii_lowercase()
                .contains(&format!("x-model-loader-token: {LOADER_TOKEN}")));
            body_of(r)
        })
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn service_switch_loads_the_model_then_sets_next_everywhere_and_a_cancel_shrinks_it() {
    let (w, loader_seen) = service_world(true, true).await;
    let a = post(
        w.addr,
        json!({"action": "service-switch", "model": "qa-embed-b"}),
    )
    .await;
    assert_eq!(a.status, 200, "{}", a.body);
    assert_eq!(
        puts(&loader_seen),
        vec![
            json!({"models": [{"model": "qa-embed-a", "file": file("qa-embed-a")}, {"model": "qa-embed-b", "file": file("qa-embed-b")}]})
        ]
    );
    assert_eq!(a.body["projects"]["next"], w.target("qa-embed-b"));
    assert_eq!(a.body["diary"], json!({"ok": true, "action": "set-next"}));
    assert_eq!(
        a.body["service"]["running"]["loaded"],
        json!(["qa-embed-a", "qa-embed-b"])
    );
    assert_eq!(w.saved().unwrap()["next"], w.target("qa-embed-b"));
    assert_eq!(w.audits("embedding.set-next")[0].2["projects"], "ok");
    assert!(w.kicked().is_some());
    // Cancelling it everywhere serves only what is still configured.
    let c = post(w.addr, json!({"action": "clear-next"})).await;
    assert_eq!(c.status, 200, "{}", c.body);
    assert_eq!(
        puts(&loader_seen).last().unwrap(),
        &json!({"models": [{"model": "qa-embed-a", "file": file("qa-embed-a")}]})
    );
    // A model the manager does not list, one whose file is not a GGUF, none at all.
    let same = post(
        w.addr,
        json!({"action": "service-switch", "model": "qa-embed-a"}),
    )
    .await;
    assert_eq!(
        (same.status, &same.body["code"]),
        (404, &json!("modelNotInstalled"))
    );
    let other = post(
        w.addr,
        json!({"action": "service-switch", "model": "qa-embed-c"}),
    )
    .await;
    assert_eq!(
        (other.status, &other.body["code"]),
        (404, &json!("modelNotInstalled"))
    );
    let empty = post(w.addr, json!({"action": "service-switch"})).await;
    assert_eq!(
        (empty.status, empty.body),
        (400, json!({"error": "service-switch needs a model"}))
    );
    assert!(w.lock_free());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_model_that_does_not_load_in_time_is_rolled_back_and_nothing_is_set() {
    let (w, loader_seen) = service_world(false, true).await;
    let a = post(
        w.addr,
        json!({"action": "service-switch", "model": "qa-embed-b"}),
    )
    .await;
    assert_eq!(a.status, 504, "{}", a.body);
    assert_eq!(a.body["code"], "serviceNotLoaded");
    assert_eq!(a.body["error"], "The embedding service did not load qa-embed-b in time. Nothing was re-indexed; try again or pick another model.");
    assert_eq!(
        puts(&loader_seen),
        vec![
            json!({"models": [{"model": "qa-embed-a", "file": file("qa-embed-a")}, {"model": "qa-embed-b", "file": file("qa-embed-b")}]}),
            json!({"models": [{"model": "qa-embed-a", "file": file("qa-embed-a")}]}),
        ]
    );
    assert_eq!(w.saved(), None);
    assert!(w.audits("embedding.set-next").is_empty());
    assert!(w.lock_free());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn service_off_stops_the_service_and_refuses_what_core_refuses() {
    let (w, loader_seen) = service_world(true, true).await;
    w.save("qa-embed-a", Some("qa-embed-b"), None);
    let busy = post(w.addr, json!({"action": "service-off"})).await;
    assert_eq!(
        (busy.status, &busy.body["code"]),
        (409, &json!("reindexUnderway"))
    );
    assert!(puts(&loader_seen).is_empty());
    std::fs::remove_file(w.path("embedding-config.json")).unwrap();
    let off = post(w.addr, json!({"action": "service-off"})).await;
    assert_eq!(off.status, 200, "{}", off.body);
    assert_eq!(puts(&loader_seen), vec![json!({"models": []})]);
    assert_eq!(off.body["service"]["enabled"], false);
    assert_eq!(
        w.audits("embedding.service-off"),
        vec![(A.to_string(), None, json!({}))]
    );

    // Without a worker view of the service, noevia does not manage it.
    let (w, _) = service_world(true, false).await;
    std::fs::remove_file(w.path("embedding-worker-status.json")).unwrap();
    let a = post(w.addr, json!({"action": "service-off"})).await;
    assert_eq!((a.status, &a.body["code"]), (409, &json!("notManaged")));
    assert!(w.lock_free());
}
