//! sbstndalton/noevia#2262 `rust-model-state-reads`: GET inference-budget and GET installed through the whole front,
//! with native budget authority, synthetic config/cache owners and a synthetic router. No engine or model loads.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]

use base64::Engine as _;
use noevia_server::{
    config::Config,
    serve::{serve, Limits},
    App,
};
use serde_json::{json, Value};
use server_store::rusqlite::Connection;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

const A: &str = "aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa";
const B: &str = "bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb";
const ADMIN: &str = "Cookie: cowork_session=admin-token; cowork_csrf=csrf\r\n";
const MEMBER: &str = "Cookie: cowork_session=member-token; cowork_csrf=csrf\r\n";
const DEVICE: &str = "nva_synthetic-device-token-0001";
const CASES: &str = include_str!("fixtures/model-sizing/cases.json");
const SECRET: &str = "synthetic-store-token-0123456789abcdef";
const MTP_REASON: &str = "MTP head presence is unverified: the model manager does not report native MTP for this installation. Check the exact download files; a missing label is not proof that the head is absent.";

fn hex(s: &str) -> String {
    Sha256::digest(s.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

async fn call(addr: SocketAddr, method: &str, path: &str, headers: &str) -> (u16, String, String) {
    let mut s = TcpStream::connect(addr).await.unwrap();
    s.write_all(format!("{method} {path} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n{headers}Content-Length: 0\r\n\r\n").as_bytes()).await.unwrap();
    let mut bytes = Vec::new();
    tokio::time::timeout(Duration::from_secs(30), s.read_to_end(&mut bytes))
        .await
        .unwrap()
        .unwrap();
    let text = String::from_utf8(bytes).unwrap();
    let (h, b) = text.split_once("\r\n\r\n").unwrap();
    (
        h.split_whitespace().nth(1).unwrap().parse().unwrap(),
        h.to_ascii_lowercase(),
        b.to_string(),
    )
}

fn seed(dir: &Path, row: Option<&str>) -> Connection {
    let db = Connection::open(dir.join("cowork.db")).unwrap();
    db.execute_batch(include_str!(
        "../../../crates/server-store/tests/fixtures/node-schema.sql"
    ))
    .unwrap();
    let now = noevia_server::identity::now_ms();
    for (uid, role, token) in [(A, "admin", "admin-token"), (B, "member", "member-token")] {
        db.execute("INSERT INTO users(id,username,username_norm,display_name,role,password_hash,webauthn_user_id,created_at,updated_at) VALUES(?1,?1,?1,'Synthetic',?2,'x',?1,1,1)", (uid, role)).unwrap();
        db.execute(
            "INSERT INTO sessions VALUES(?1,?2,?3,?4,?4,?5,'ua','127.0.0.1')",
            (hex(token), uid, hex("csrf"), now, now + 86_400_000),
        )
        .unwrap();
    }
    db.execute(
        "INSERT INTO device_grants VALUES('g-1',?1,'Mac',?2,?2,?3,'ip','ua')",
        (A, now, now + 86_400_000),
    )
    .unwrap();
    db.execute(
        "INSERT INTO device_tokens VALUES(?1,'g-1','access',?2,?3,NULL,NULL)",
        (hex(DEVICE), now, now + 3_600_000),
    )
    .unwrap();
    if let Some(v) = row {
        db.execute(
            "INSERT INTO settings(key,value) VALUES('inference:memory-budget',?1)",
            [v],
        )
        .unwrap();
    }
    db
}

#[derive(Clone)]
struct Stub {
    answer: Arc<Mutex<(u16, String)>>,
    seen: Arc<Mutex<Vec<String>>>,
}

impl Stub {
    fn set(&self, status: u16, text: String) {
        *self.answer.lock().unwrap() = (status, text);
    }
    fn seen(&self) -> Vec<String> {
        self.seen.lock().unwrap().clone()
    }
}

/// One-shot HTTP/1.1 stub. `route(head+body, default)` picks the reply.
async fn stub(
    first: (u16, String),
    route: impl Fn(&str, &(u16, String)) -> (u16, String) + Send + Sync + 'static,
) -> (u16, Stub) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let s = Stub {
        answer: Arc::new(Mutex::new(first)),
        seen: Arc::new(Mutex::new(Vec::new())),
    };
    let shared = s.clone();
    let route = Arc::new(route);
    tokio::spawn(async move {
        while let Ok((mut c, _)) = listener.accept().await {
            let (shared, route) = (shared.clone(), Arc::clone(&route));
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
                shared.seen.lock().unwrap().push(request.clone());
                let current = shared.answer.lock().unwrap().clone();
                let (code, text) = route(&request, &current);
                let out = format!("HTTP/1.1 {code} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{text}", text.len());
                let _ = c.write_all(out.as_bytes()).await;
            });
        }
    });
    (port, s)
}

/// Synthetic configuration/cache owners. The retired model-facts bridge never answers.
async fn node(installed: Value) -> (u16, Stub) {
    stub((200,"{}".to_owned()), move |req, current| {
        if req.starts_with("POST /internal/model-config ") {
            if current.0 != 200 { return current.clone(); }
            return (200,json!({"env":{"EMBEDDING_MODEL":"syn-embed"},"embedding":{"status":{"stale":false,"enabled":true,"running":{"loaded":installed["sidecarLoaded"]}},"protected":["syn-embed"]},"config":null,"rerank":null,"revision":"synthetic-revision"}).to_string());
        }
        if req.starts_with("POST /internal/model-manager-scan/snapshot ") {
            return (200,json!({"token":"a".repeat(64),"hit":{"body":{"models":installed["scan"]}}}).to_string());
        }
        (200,json!({"proxied":true}).to_string())
    })
    .await
}

async fn router(text: Value) -> (u16, Stub) {
    stub((200, text.to_string()), |req, current| {
        if req.starts_with("GET /models ") {
            current.clone()
        } else {
            (404, "{}".to_owned())
        }
    })
    .await
}

async fn front(dir: &Path, node_port: u16, engine: Option<u16>) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let meminfo = dir.join("meminfo");
    std::fs::write(&meminfo, "MemTotal:       67108864 kB\n").unwrap();
    let mut env: HashMap<String, String> = [
        (
            "NOEVIA_LEGACY_UPSTREAM",
            format!("http://127.0.0.1:{node_port}"),
        ),
        ("UI_DATA_DIR", dir.display().to_string()),
        (
            "NOEVIA_START_ID",
            "0123456789abcdef0123456789abcdef".to_owned(),
        ),
        ("INFERENCE_MEMORY_BUDGET_GIB", "6".to_owned()),
        ("TRUST_PROXY", "true".to_owned()),
        ("NOEVIA_FEATURE_NATIVE_CLIENT_AUTH", "1".to_owned()),
        ("PUBLIC_ORIGIN", format!("http://{addr}")),
        ("NOEVIA_TEST_MEMINFO_PATH", meminfo.display().to_string()),
        ("NOEVIA_PROJECT_STORE_TOKEN", SECRET.to_owned()),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_owned(), v))
    .collect();
    if let Some(port) = engine {
        for (k, v) in [
            ("MODEL_MANAGER_KIND", "llamacpp".to_owned()),
            ("MODEL_MANAGER_BASE_URL", format!("http://127.0.0.1:{port}")),
            (
                "LLAMACPP_PRESET_PATH",
                dir.join("models.ini").display().to_string(),
            ),
            (
                "LLAMACPP_MODELS_PATH",
                dir.join("models").display().to_string(),
            ),
        ] {
            env.insert(k.to_owned(), v);
        }
    }
    let app = App::new(Config::from_lookup(|k| env.get(k).cloned()).unwrap());
    app.config.model_facts.publish().unwrap();
    tokio::spawn(serve(
        listener,
        app,
        Limits::default(),
        std::future::pending(),
    ));
    addr
}

/// The recorded fixture's `good.gguf` (a sparse 4.5 GiB file) and a models.ini naming it.
fn lay(dir: &Path) {
    let cases: Value = serde_json::from_str(CASES).unwrap();
    let f = cases["files"]
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["path"] == "models/good.gguf")
        .unwrap();
    let file = dir.join("models/good.gguf");
    std::fs::create_dir_all(file.parent().unwrap()).unwrap();
    std::fs::write(
        &file,
        base64::engine::general_purpose::STANDARD
            .decode(f["b64"].as_str().unwrap())
            .unwrap(),
    )
    .unwrap();
    std::fs::File::options()
        .write(true)
        .open(&file)
        .unwrap()
        .set_len(f["size"].as_u64().unwrap())
        .unwrap();
    std::fs::write(
        dir.join("models.ini"),
        "version = 1\n[good]\nmodel = /models/good.gguf\nctx-size = 16384\n[gone]\nmodel = /models/gone.gguf\n",
    )
    .unwrap();
}

fn listing() -> Value {
    json!({"data":[
        {"id":"good","source":"preset","can_remove":true,"status":{"value":"loaded","args":["--model","/models/good.gguf"]},"meta":{"n_ctx_train":32768},"architecture":{"input_modalities":["text","image"]}},
        {"id":"good-long","source":"preset","status":{"value":"unloaded","args":["--model","/models/good.gguf"]}},
        {"id":"gone","source":"preset","status":{"value":"unloaded","failed":true,"args":["--model","/models/gone.gguf"]}},
        {"id":"laya_multilingual_f16","source":"preset","status":{"value":"unloaded"}},
        {"id":"syn-path-only-laya","source":"preset","status":{"value":"unloaded","args":["--model","/models/laya_multilingual_f16.gguf"]}},
        {"id":"syn-embed","source":"preset","status":{"value":"unloaded","args":["--embedding"]}},
        {"id":"syn-sleeper","source":"cache","can_remove":true,"status":{"value":"sleeping"},"meta":{"size":3400000000.0}},
        {"id":"0123456789abcdef0123456789abcdef","status":{"value":"loaded"}},
        {"id":17,"status":{"value":"unloaded"}},
    ]})
}

fn installed_facts() -> Value {
    json!({
        "sidecarNames":["syn-embed"],
        "sidecarLoaded":["syn-embed"],
        "scan":[{"modelId":"good","sections":["good","good-long"],"bytes":4831838208u64,"shape":{"arch":"llama"}}],
    })
}

fn parse(text: &str) -> Value {
    serde_json::from_str(text).unwrap()
}

#[tokio::test]
async fn native_live_budget_does_not_reread_settings_or_ask_node() {
    let d = tempfile::tempdir().unwrap();
    lay(d.path());
    let db = seed(d.path(), Some(r#"{"budgetGib":7}"#));
    let (np, n) = node(installed_facts()).await;
    let (ep, _e) = router(listing()).await;
    let addr = front(d.path(), np, Some(ep)).await;
    db.execute(
        r#"UPDATE settings SET value='{"budgetGib":50}' WHERE key='inference:memory-budget'"#,
        [],
    )
    .unwrap();
    let (status, _, body) = call(addr, "GET", "/api/models/inference-budget", ADMIN).await;
    assert_eq!(status, 200);
    assert_eq!(parse(&body)["budgetGib"], 7.0);
    assert!(!n
        .seen()
        .iter()
        .any(|r| r.contains("/internal/models/facts")));
}

#[tokio::test]
async fn denied_callers_never_reach_node_or_the_engine() {
    let d = tempfile::tempdir().unwrap();
    lay(d.path());
    let _db = seed(d.path(), None);
    let (np, n) = node(installed_facts()).await;
    let (ep, e) = router(listing()).await;
    let addr = front(d.path(), np, Some(ep)).await;
    let device = format!("Authorization: Bearer {DEVICE}\r\n");
    for (headers, want) in [("", 401), (MEMBER, 403), (device.as_str(), 403)] {
        let (status, _, body) = call(addr, "GET", "/api/models/inference-budget", headers).await;
        assert_eq!(status, want, "{headers:?} {body}");
        if want == 403 {
            assert_eq!(
                parse(&body),
                json!({"error":"Administrator required for shared model profiles"})
            );
        }
    }
    let (status, _, _) = call(addr, "GET", "/api/models/installed", "").await;
    assert_eq!(status, 401);
    assert!(n.seen().is_empty(), "{:?}", n.seen());
    assert!(e.seen().is_empty());
    // A member reads the installed list, as the chat needs it.
    let (status, _, body) = call(addr, "GET", "/api/models/installed", MEMBER).await;
    assert_eq!(status, 200, "{body}");
}

#[tokio::test]
async fn the_installed_list_is_nodes_row_for_row() {
    let d = tempfile::tempdir().unwrap();
    lay(d.path());
    let _db = seed(d.path(), None);
    let (np, n) = node(installed_facts()).await;
    let (ep, e) = router(listing()).await;
    let addr = front(d.path(), np, Some(ep)).await;
    let (status, head, body) = call(addr, "GET", "/api/models/installed", ADMIN).await;
    assert_eq!(status, 200, "{body}");
    assert!(head.contains("cache-control: no-store"));
    let mtp = json!({"supported":false,"enabled":false,"reason":MTP_REASON});
    let row = |extra: Value| {
        let mut base = json!({"sizeGB":null,"shape":null,"loaded":false,"labels":[],"system":false,"mtp":mtp,"maxContext":null,"suggested":false,"status":"unloaded","failed":false,"missingFile":false,"servedElsewhere":false,"canDelete":false,"sidecarProtected":false,"source":"preset"});
        for (k, v) in extra.as_object().unwrap() {
            base[k] = v.clone();
        }
        base
    };
    assert_eq!(
        parse(&body),
        json!([
            row(
                json!({"longVariant":"good-long","longLoaded":false,"name":"good","sizeGB":4.8,"shape":{"arch":"llama"},"loaded":true,"labels":["vision"],"maxContext":32768,"status":"loaded","canDelete":true})
            ),
            row(json!({"name":"gone","status":"missing","failed":true,"missingFile":true})),
            row(
                json!({"name":"laya_multilingual_f16","system":true,"status":"served-elsewhere","servedElsewhere":true})
            ),
            // #2267: a Laya row named only by its model path is the system model Node's list shows.
            row(
                json!({"name":"syn-path-only-laya","system":true,"status":"served-elsewhere","servedElsewhere":true})
            ),
            row(
                json!({"name":"syn-embed","loaded":true,"labels":["embeddings"],"status":"loaded","sidecarProtected":true})
            ),
            row(
                json!({"name":"syn-sleeper","sizeGB":3.4,"loaded":true,"status":"sleeping","canDelete":true,"source":"cache"})
            ),
        ])
    );
    // Node's wire order, which the corpus and the web client's rows follow.
    assert!(
        body.starts_with(
            r#"[{"longVariant":"good-long","longLoaded":false,"name":"good","sizeGB":4.8,"shape":"#
        ),
        "{body}"
    );
    assert_eq!(
        e.seen().len(),
        1,
        "one listing and no /props: {:?}",
        e.seen()
    );
    let asked = n.seen();
    assert_eq!(asked.len(), 4);
    let sent = parse(asked[0].split_once("\r\n\r\n").unwrap().1);
    assert!(asked.iter().all(|r| !r.contains("/internal/models/facts")));
    assert_eq!(
        sent["firstLoaded"], "good",
        "Node's remembered model keeps its source"
    );
    // Another method is Node's.
    let (_, _, body) = call(addr, "POST", "/api/models/installed", ADMIN).await;
    assert_eq!(parse(&body), json!({"proxied":true}));
}

#[tokio::test]
async fn without_a_scan_nothing_reads_as_missing_and_laya_is_still_elsewhere() {
    let d = tempfile::tempdir().unwrap();
    lay(d.path());
    let _db = seed(d.path(), None);
    let (np, _n) = node(json!({"sidecarNames":["syn-embed"],"sidecarLoaded":[],"scan":null})).await;
    let (ep, _e) = router(listing()).await;
    let addr = front(d.path(), np, Some(ep)).await;
    let (_, _, body) = call(addr, "GET", "/api/models/installed", ADMIN).await;
    let v = parse(&body);
    let by = |n: &str| {
        v.as_array()
            .unwrap()
            .iter()
            .find(|r| r["name"] == n)
            .unwrap()
            .clone()
    };
    assert_eq!(
        (
            by("gone")["missingFile"].clone(),
            by("gone")["status"].clone(),
            by("gone")["failed"].clone()
        ),
        (json!(false), json!("unloaded"), json!(true))
    );
    assert_eq!(by("laya_multilingual_f16")["status"], "served-elsewhere");
    assert_eq!(
        (
            by("syn-embed")["loaded"].clone(),
            by("syn-embed")["status"].clone()
        ),
        (json!(false), json!("unloaded"))
    );
}

#[tokio::test]
async fn an_engine_that_is_down_is_nodes_answer_on_both_reads() {
    let d = tempfile::tempdir().unwrap();
    lay(d.path());
    let _db = seed(d.path(), None);
    let (np, n) = node(installed_facts()).await;
    // Nothing listens on port 9.
    let addr = front(d.path(), np, Some(9)).await;
    let (status, _, body) = call(addr, "GET", "/api/models/installed", ADMIN).await;
    assert_eq!(
        (status, parse(&body)),
        (502, json!({"error":"Could not list installed models."}))
    );
    let (status, _, body) = call(addr, "GET", "/api/models/inference-budget", ADMIN).await;
    assert_eq!(status, 200);
    let v = parse(&body);
    assert_eq!(
        (v["budgetGib"].as_f64(), &v["models"], &v["estimateError"]),
        (Some(6.0), &Value::Null, &json!("engine"))
    );

    // A router that answers something else than a listing, or an error.
    let d = tempfile::tempdir().unwrap();
    lay(d.path());
    let _db = seed(d.path(), None);
    let (ep, e) = router(json!({"data":"nope"})).await;
    let addr = front(d.path(), np, Some(ep)).await;
    let (status, _, _) = call(addr, "GET", "/api/models/installed", ADMIN).await;
    assert_eq!(status, 502);
    e.set(200, json!({"data":[null]}).to_string());
    let (status, _, _) = call(addr, "GET", "/api/models/installed", ADMIN).await;
    assert_eq!(
        status, 502,
        "a malformed unknown listing never becomes an empty catalogue"
    );
    e.set(500, "{}".to_owned());
    let (status, _, _) = call(addr, "GET", "/api/models/installed", ADMIN).await;
    assert_eq!(status, 502);
    let (_, _, body) = call(addr, "GET", "/api/models/inference-budget", ADMIN).await;
    assert_eq!(parse(&body)["estimateError"], "engine");
    drop(n);
}

#[tokio::test]
async fn unavailable_configuration_is_not_an_empty_catalogue() {
    let d = tempfile::tempdir().unwrap();
    lay(d.path());
    let _db = seed(d.path(), None);
    let (ep, _e) = router(listing()).await;
    let addr = front(d.path(), 9, Some(ep)).await;
    assert_eq!(
        call(addr, "GET", "/api/models/installed", ADMIN).await.0,
        503
    );
    assert_eq!(
        call(addr, "GET", "/api/models/inference-budget", ADMIN)
            .await
            .0,
        200
    );
}

#[tokio::test]
async fn with_no_model_manager_the_answers_are_nodes_recorded_ones() {
    let d = tempfile::tempdir().unwrap();
    let _db = seed(d.path(), None);
    let (np, _n) = node(installed_facts()).await;
    let addr = front(d.path(), np, None).await;
    // Corpus 000140.
    let (status, _, body) = call(addr, "GET", "/api/models/installed", ADMIN).await;
    assert_eq!(
        (status, body.as_str()),
        (502, r#"{"error":"Could not list installed models."}"#)
    );
    // Corpus 000139's shape: the figure and no rows, with no estimate error.
    let (status, _, body) = call(addr, "GET", "/api/models/inference-budget", ADMIN).await;
    assert_eq!(status, 200);
    let v = parse(&body);
    assert_eq!(v["models"], Value::Null);
    assert!(v.get("estimateError").is_none());
}

#[test]
fn budget_boot_failure_live_save_and_snapshot_are_one_state() {
    let d = tempfile::tempdir().unwrap();
    std::fs::write(d.path().join("meminfo"), "MemTotal:       67108864 kB\n").unwrap();
    let env = HashMap::from([
        (
            "NOEVIA_TEST_MEMINFO_PATH",
            d.path().join("meminfo").display().to_string(),
        ),
        ("UI_DATA_DIR", d.path().display().to_string()),
        ("NOEVIA_START_ID", "synthetic-start".to_owned()),
        ("INFERENCE_MEMORY_BUDGET_GIB", "6".to_owned()),
    ]);
    // No readable settings table at boot: the deployment limit is the enforced value.
    let authority = noevia_server::model_facts::Authority::from_lookup(&|k| env.get(k).cloned());
    authority.publish().unwrap();
    assert_eq!(authority.budget().unwrap()["budgetGib"], 6.0);
    let c = Connection::open(d.path().join("cowork.db")).unwrap();
    c.execute_batch("CREATE TABLE settings(key TEXT PRIMARY KEY,value TEXT); INSERT INTO settings VALUES('inference:memory-budget','{\"budgetGib\":50}');").unwrap();
    assert_eq!(authority.budget().unwrap()["budgetGib"], 6.0);
    assert_eq!(
        authority.save(&json!({"budgetGib":9})).unwrap()["budgetGib"],
        9.0
    );
    let snapshot = noevia_server::model_facts::snapshot(d.path(), "synthetic-start").unwrap();
    assert_eq!(snapshot["generation"], 1);
    assert_eq!(snapshot["budget"]["source"], "admin");
    assert!(noevia_server::model_facts::snapshot(d.path(), "old-start").is_none());
    std::fs::write(d.path().join("model-facts.json"), "{\"budget\":").unwrap();
    assert!(noevia_server::model_facts::snapshot(d.path(), "synthetic-start").is_none());
}

#[tokio::test]
async fn stale_generation_scan_is_not_published_and_loading_stays_loading() {
    let d = tempfile::tempdir().unwrap();
    lay(d.path());
    let _db = seed(d.path(), None);
    let config_reads = std::sync::atomic::AtomicUsize::new(0);
    let invalidated = std::sync::atomic::AtomicBool::new(false);
    let (np,_n)=stub((200,"{}".into()),move |req,_|{
        if req.contains("/internal/model-config ") {
            if config_reads.fetch_add(1,std::sync::atomic::Ordering::SeqCst)>0 {invalidated.store(true,std::sync::atomic::Ordering::SeqCst);}
            return (200,json!({"env":{},"revision":"same-config"}).to_string());
        }
        if req.contains("/internal/model-manager-scan/snapshot ") {
            return (200,json!({"token":if invalidated.load(std::sync::atomic::Ordering::SeqCst){"new"}else{"old"},"hit":{"body":{"models":[{"modelId":"obsolete","bytes":5e9}]}}}).to_string());
        }
        (500,"{}".into())
    }).await;
    let (ep,_e)=router(json!({"data":[{"id":"synthetic-loading","source":"preset","status":{"value":"loading","failed":false}}]})).await;
    let addr = front(d.path(), np, Some(ep)).await;
    let (status, _, body) = call(addr, "GET", "/api/models/installed", ADMIN).await;
    assert_eq!(status, 200);
    let rows = parse(&body);
    assert_eq!(rows[0]["status"], "loading");
    assert_eq!(rows[0]["missingFile"], false);
    assert_eq!(rows[0]["shape"], Value::Null);
}

#[test]
fn rejected_budget_publication_preserves_the_row_and_budget_after_restart() {
    for row in [None, Some(r#"{"budgetGib":7}"#)] {
        let d = tempfile::tempdir().unwrap();
        let db = seed(d.path(), row);
        std::fs::write(d.path().join("meminfo"), "MemTotal:       67108864 kB\n").unwrap();
        let env = HashMap::from([
            ("UI_DATA_DIR", d.path().display().to_string()),
            (
                "NOEVIA_TEST_MEMINFO_PATH",
                d.path().join("meminfo").display().to_string(),
            ),
            ("NOEVIA_START_ID", "synthetic-first-start".to_owned()),
            ("INFERENCE_MEMORY_BUDGET_GIB", "6".to_owned()),
        ]);
        let boot = |start: &str| {
            noevia_server::model_facts::Authority::from_lookup(&|k| {
                if k == "NOEVIA_START_ID" {
                    Some(start.to_owned())
                } else {
                    env.get(k).cloned()
                }
            })
        };
        let authority = boot("synthetic-first-start");
        authority.publish().unwrap();
        let old = authority.budget().unwrap();
        // A directory at the temporary file path makes publication fail even as root,
        // while keeping the last complete snapshot readable.
        let obstruction = d.path().join("model-facts.json.tmp");
        std::fs::create_dir(&obstruction).unwrap();
        assert_eq!(authority.save(&json!({"budgetGib":9})).unwrap_err().0, 503);
        assert_eq!(authority.budget().unwrap(), old);
        drop(authority);
        std::fs::remove_dir(obstruction).unwrap();
        let restarted = boot("synthetic-second-start");
        restarted.publish().unwrap();
        assert_eq!(restarted.budget().unwrap(), old);
        let stored: Vec<String> = db
            .prepare("SELECT value FROM settings WHERE key='inference:memory-budget'")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .map(Result::unwrap)
            .collect();
        assert_eq!(
            stored,
            row.into_iter().map(str::to_owned).collect::<Vec<_>>()
        );
    }
}

#[test]
fn rejected_budget_commit_restores_the_snapshot_without_regressing_generation() {
    let d = tempfile::tempdir().unwrap();
    let db = seed(d.path(), Some(r#"{"budgetGib":7}"#));
    db.pragma_update(None, "journal_mode", "DELETE").unwrap();
    std::fs::write(d.path().join("meminfo"), "MemTotal:       67108864 kB\n").unwrap();
    let env = HashMap::from([
        ("UI_DATA_DIR", d.path().display().to_string()),
        (
            "NOEVIA_TEST_MEMINFO_PATH",
            d.path().join("meminfo").display().to_string(),
        ),
        ("NOEVIA_START_ID", "synthetic-start".to_owned()),
    ]);
    let authority = noevia_server::model_facts::Authority::from_lookup(&|k| env.get(k).cloned());
    authority.publish().unwrap();
    let old = authority.budget().unwrap();
    // A concurrent reader allows the row update, but prevents its commit.
    db.execute_batch("BEGIN; SELECT value FROM settings;")
        .unwrap();
    assert_eq!(authority.save(&json!({"budgetGib":9})).unwrap_err().0, 503);
    db.execute_batch("ROLLBACK;").unwrap();
    assert_eq!(authority.budget().unwrap(), old);
    let snapshot = noevia_server::model_facts::snapshot(d.path(), "synthetic-start").unwrap();
    assert_eq!(snapshot["generation"], 2);
    assert_eq!(
        db.query_row(
            "SELECT value FROM settings WHERE key='inference:memory-budget'",
            [],
            |r| r.get::<_, String>(0)
        )
        .unwrap(),
        r#"{"budgetGib":7}"#
    );
}

#[tokio::test]
async fn null_address_targets_keep_protection_when_the_managed_service_has_no_address() {
    for address in [Value::Null, json!("")] {
        let d = tempfile::tempdir().unwrap();
        let _db = seed(d.path(), None);
        let raw = json!({
            "embedding":{"baseUrl":address,"status":{"stale":false,"enabled":true,"running":{"loaded":[]}},"protected":["synthetic-running"]},
            "config":{"names":["synthetic-active","synthetic-next","synthetic-previous"],"targets":[
                {"model":"synthetic-active","baseUrl":null},
                {"model":"synthetic-next","baseUrl":null},
                {"model":"synthetic-previous","baseUrl":null}
            ]},"revision":"synthetic-revision"
        });
        let (np, _) = stub((200, raw.to_string()), |_, current| current.clone()).await;
        let env = HashMap::from([
            ("UI_DATA_DIR", d.path().display().to_string()),
            (
                "NOEVIA_START_ID",
                "0123456789abcdef0123456789abcdef".to_owned(),
            ),
            ("NOEVIA_LEGACY_UPSTREAM", format!("http://127.0.0.1:{np}")),
            ("NOEVIA_PROJECT_STORE_TOKEN", SECRET.to_owned()),
        ]);
        let app = App::new(Config::from_lookup(|k| env.get(k).cloned()).unwrap());
        let (names, _, _) = noevia_server::model_facts::sidecars(&app, None)
            .await
            .unwrap();
        assert_eq!(
            names,
            [
                "synthetic-active",
                "synthetic-next",
                "synthetic-previous",
                "synthetic-running"
            ]
        );
    }
}

#[tokio::test]
async fn concurrent_cold_facts_share_one_bounded_scan() {
    let d = tempfile::tempdir().unwrap();
    let _db = seed(d.path(), None);
    let (np, _node) = stub((200, "{}".into()), |req, _| {
        if req.contains("/internal/model-config ") {
            return (200, json!({"env":{},"revision":"synthetic"}).to_string());
        }
        (
            200,
            json!({"token":"synthetic-generation","hit":null}).to_string(),
        )
    })
    .await;
    let loader = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = loader.local_addr().unwrap().port();
    let count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let seen = Arc::clone(&count);
    let task = tokio::spawn(async move {
        while let Ok((mut stream, _)) = loader.accept().await {
            seen.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            tokio::spawn(async move {
                let mut bytes = [0u8; 8192];
                let _ = stream.read(&mut bytes).await;
                tokio::time::sleep(Duration::from_millis(50)).await;
                let body = json!({"models":[{"modelId":"synthetic","bytes":12}]}).to_string();
                stream.write_all(format!("HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",body.len()).as_bytes()).await.unwrap();
            });
        }
    });
    let env = HashMap::from([
        ("UI_DATA_DIR", d.path().display().to_string()),
        ("NOEVIA_LEGACY_UPSTREAM", format!("http://127.0.0.1:{np}")),
        ("NOEVIA_PROJECT_STORE_TOKEN", SECRET.to_owned()),
        (
            "NOEVIA_START_ID",
            "0123456789abcdef0123456789abcdef".to_owned(),
        ),
        ("MODEL_LOADER_URL", format!("http://127.0.0.1:{port}")),
    ]);
    let app = App::new(Config::from_lookup(|k| env.get(k).cloned()).unwrap());
    let (a, b) = tokio::join!(
        noevia_server::model_facts::installed(&app, &[], true, None),
        noevia_server::model_facts::installed(&app, &[], true, None)
    );
    assert_eq!(a, b);
    assert!(a.is_some());
    assert_eq!(count.load(std::sync::atomic::Ordering::SeqCst), 1);
    task.abort();
    let _ = task.await;
}

#[tokio::test]
async fn changed_config_during_scan_is_unavailable_not_a_stale_protection_catalogue() {
    let d = tempfile::tempdir().unwrap();
    lay(d.path());
    let _db = seed(d.path(), None);
    let calls = std::sync::atomic::AtomicUsize::new(0);
    let (np,_n)=stub((200,"{}".into()),move |req,_|{
        if req.contains("/internal/model-config ") {
            return (200,json!({"env":{},"revision":calls.fetch_add(1,std::sync::atomic::Ordering::SeqCst).to_string()}).to_string());
        }
        (200,json!({"token":"same-generation","hit":null}).to_string())
    }).await;
    let (ep, _e) = router(listing()).await;
    let addr = front(d.path(), np, Some(ep)).await;
    assert_eq!(
        call(addr, "GET", "/api/models/installed", ADMIN).await.0,
        503
    );
}

#[test]
fn saved_boot_budget_keeps_the_oracles_number_coercion_and_host_bound() {
    let d = tempfile::tempdir().unwrap();
    let db = seed(d.path(), None);
    std::fs::write(d.path().join("meminfo"), "MemTotal:       67108864 kB\n").unwrap();
    let env = HashMap::from([
        ("UI_DATA_DIR", d.path().display().to_string()),
        (
            "NOEVIA_START_ID",
            "0123456789abcdef0123456789abcdef".to_owned(),
        ),
        (
            "NOEVIA_TEST_MEMINFO_PATH",
            d.path().join("meminfo").display().to_string(),
        ),
    ]);
    for (row, budget, source) in [
        (json!({"budgetGib":[["3"]]}), 3.0, "admin"),
        (json!({"budgetGib":true}), 2.0, "admin"),
        (json!({"budgetGib":"Infinity"}), 60.0, "admin"),
        (json!({"budgetGib":{}}), 16.0, "deployment"),
    ] {
        db.execute(
            "INSERT OR REPLACE INTO settings(key,value) VALUES('inference:memory-budget',?1)",
            [row.to_string()],
        )
        .unwrap();
        let authority =
            noevia_server::model_facts::Authority::from_lookup(&|k| env.get(k).cloned());
        authority.publish().unwrap();
        let got = authority.budget().unwrap();
        assert_eq!(got["budgetGib"], budget);
        assert_eq!(got["source"], source);
    }
}
