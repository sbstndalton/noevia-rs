//! sbstndalton/noevia#2262 `rust-model-state-reads`: GET inference-budget and GET installed through the whole front,
//! with a synthetic Node answering the loopback facts call and a synthetic router. No engine, no model loads.
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

/// A synthetic Node: `/internal/models/facts` by `want`, needing the secret; anything else is a proxied request.
/// `answer` is the budget figure; `installed` the installed facts.
async fn node(installed: Value) -> (u16, Stub) {
    let budget = (
        200,
        r#"{"budgetGib":6,"source":"deployment","defaultGib":6,"minGib":2,"maxGib":60,"hostRamGib":64,"limited":false}"#.to_owned(),
    );
    stub(budget, move |req, current| {
        if !req.starts_with("POST /internal/models/facts ") {
            return (200, r#"{"proxied":true}"#.to_owned());
        }
        if !req
            .to_ascii_lowercase()
            .contains(&format!("x-noevia-store-token: {SECRET}"))
        {
            return (404, r#"{"error":"Not found"}"#.to_owned());
        }
        if req.contains(r#""want":"installed""#) {
            return (200, installed.to_string());
        }
        (current.0, format!(r#"{{"budget":{}}}"#, current.1))
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
async fn the_budget_is_the_figure_node_enforces_not_the_settings_row() {
    let d = tempfile::tempdir().unwrap();
    lay(d.path());
    // The row says 50: a boot read that failed left Node on the deployment's 6, and that is what limits loads.
    let _db = seed(d.path(), Some(r#"{"budgetGib":50}"#));
    let (np, n) = node(installed_facts()).await;
    let (ep, _e) = router(listing()).await;
    let addr = front(d.path(), np, Some(ep)).await;
    let (status, head, body) = call(addr, "GET", "/api/models/inference-budget", ADMIN).await;
    assert_eq!(status, 200, "{body}");
    assert!(head.contains("cache-control: no-store"));
    let v = parse(&body);
    assert_eq!(
        (v["budgetGib"].as_f64(), v["source"].as_str()),
        (Some(6.0), Some("deployment"))
    );
    assert!(v.get("estimateError").is_none());
    // Node's key order: the budget's own keys, then the rows.
    assert!(body.starts_with(r#"{"budgetGib":6,"source":"deployment","defaultGib":6,"minGib":2,"maxGib":60,"hostRamGib":64,"limited":false,"models":["#), "{body}");
    let rows = v["models"].as_array().unwrap();
    let names: Vec<_> = rows.iter().map(|r| r["model"].as_str().unwrap()).collect();
    assert!(
        !names.contains(&"gone"),
        "#1708: a missing file is not in the table: {names:?}"
    );
    assert!(
        names.contains(&"syn-sleeper") && names.contains(&"good-long"),
        "{names:?}"
    );
    let good = rows.iter().find(|r| r["model"] == "good").unwrap();
    assert_eq!(good["labels"], json!(["vision"]));
    assert_eq!(good["loaded"], true);
    assert_eq!(good["sleeping"], false);
    let total = good["estimate"]["totalGib"].as_f64().unwrap();
    assert!(total > 4.5, "{total}");
    assert_eq!(good["fits"], json!(total <= 6.0));
    let sleeper = rows.iter().find(|r| r["model"] == "syn-sleeper").unwrap();
    assert_eq!(
        (&sleeper["loaded"], &sleeper["sleeping"]),
        (&json!(false), &json!(true))
    );
    assert_eq!(
        (&sleeper["estimate"], &sleeper["fits"]),
        (&Value::Null, &Value::Null)
    );
    assert_eq!(
        rows.iter()
            .find(|r| r["model"] == "laya_multilingual_f16")
            .unwrap()["system"],
        true
    );

    // A PUT goes through Node and lands in its figure: the very next GET has it, against the same row and the fit moves.
    n.set(200, json!({"budgetGib":60,"source":"admin","defaultGib":6,"minGib":2,"maxGib":60,"hostRamGib":64,"limited":false}).to_string());
    let (_, _, body) = call(addr, "GET", "/api/models/inference-budget", ADMIN).await;
    let v = parse(&body);
    assert_eq!(
        (v["budgetGib"].as_f64(), v["source"].as_str()),
        (Some(60.0), Some("admin"))
    );
    assert_eq!(
        v["models"]
            .as_array()
            .unwrap()
            .iter()
            .find(|r| r["model"] == "good")
            .unwrap()["fits"],
        true
    );
    let (_, _, body) = call(addr, "PUT", "/api/models/inference-budget", ADMIN).await;
    assert_eq!(
        parse(&body),
        json!({"proxied":true}),
        "the PUT stays Node's"
    );
    assert!(n
        .seen()
        .iter()
        .any(|r| r.starts_with("PUT /api/models/inference-budget ")));
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
    assert_eq!(asked.len(), 1);
    let sent = parse(asked[0].split_once("\r\n\r\n").unwrap().1);
    assert_eq!(sent["want"], "installed");
    assert_eq!(sent["scan"], true);
    assert_eq!(
        sent["firstLoaded"], "good",
        "Node's remembered model keeps its source"
    );
    assert_eq!(sent["ids"][0], "good");
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
    e.set(500, "{}".to_owned());
    let (status, _, _) = call(addr, "GET", "/api/models/installed", ADMIN).await;
    assert_eq!(status, 502);
    let (_, _, body) = call(addr, "GET", "/api/models/inference-budget", ADMIN).await;
    assert_eq!(parse(&body)["estimateError"], "engine");
    drop(n);
}

#[tokio::test]
async fn no_answer_from_node_is_a_503_never_a_guess() {
    let d = tempfile::tempdir().unwrap();
    lay(d.path());
    let _db = seed(d.path(), Some(r#"{"budgetGib":50}"#));
    let (ep, _e) = router(listing()).await;
    // Nothing listens on port 9: no Node.
    let addr = front(d.path(), 9, Some(ep)).await;
    for path in ["/api/models/inference-budget", "/api/models/installed"] {
        let (status, _, body) = call(addr, "GET", path, ADMIN).await;
        assert_eq!(status, 503, "{path} {body}");
        assert!(!body.contains("50"), "{body}");
    }
    // Node faulting behind the bridge, or answering nonsense, is the same.
    let (np, n) = node(installed_facts()).await;
    let addr = front(d.path(), np, Some(ep)).await;
    n.set(503, "null".to_owned());
    let (status, _, _) = call(addr, "GET", "/api/models/inference-budget", ADMIN).await;
    assert_eq!(status, 503);
    n.set(200, r#"{"budgetGib":"six"}"#.to_owned());
    let (status, _, _) = call(addr, "GET", "/api/models/inference-budget", ADMIN).await;
    assert_eq!(status, 503);
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
