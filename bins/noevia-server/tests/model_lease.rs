//! The reload's exclusion with no inference gate (sbstndalton/noevia#2210): Node's own maintenance gate
//! is the arbiter and the front takes a lease on it. Against a scripted Node here; `real_node_*` runs the
//! same against Core's real lease route and gate when `NOEVIA_MCP_CORE` names a checkout. Also the
//! reload's gate hold edge cases and the evidence inbox's safety. Synthetic data only.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]
mod maint_support;
use maint_support::*;
use noevia_server::model_maint::evidence_live::supervise;
use noevia_server::model_maint::{HoldError, Maint};
use serde_json::{json, Value};
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

const SECRET: &str = "synthetic-store-token-0123456789abcdef";
const LEASE: &str = "/internal/model-maintenance/lease";
const ID: &str = "0123456789abcdef0123456789abcdef";
const IN_PROGRESS: &str = r#"{"error":"Requests are in progress. Try again when chats finish."}"#;
const GRANT: &str = r#"{"lease":"0123456789abcdef0123456789abcdef","ttlMs":180000,"renewMs":100,"hardMs":3600000}"#;

type Order = Arc<Mutex<Vec<String>>>;

fn router(order: Order, loaded: &[&str], unload_ms: u64) -> Handler {
    let rows: Vec<Value> = loaded
        .iter()
        .map(|id| json!({"id": id, "status": {"value": "loaded", "preset": "p"}}))
        .collect();
    let listing = json!({ "data": rows }).to_string();
    Arc::new(move |req: &Req| {
        if req.target.starts_with("/gate/v1/") {
            // No gate: the router's own 404.
            return Answer::Reply(404, r#"{"error":"not found"}"#.into());
        }
        order
            .lock()
            .unwrap()
            .push(format!("router {} {}", req.method, req.target));
        match (req.method.as_str(), req.target.as_str()) {
            ("GET", "/models") => Answer::Reply(200, listing.clone()),
            ("POST", "/models/unload") => {
                Answer::After(Duration::from_millis(unload_ms), 200, "{}".into())
            }
            ("GET", "/models?reload=1") => Answer::Reply(200, r#"{"success":true}"#.into()),
            _ => Answer::Reply(404, "{}".into()),
        }
    })
}

fn node(order: Order, acquire: fn() -> Answer, renew: fn() -> Answer) -> Handler {
    Arc::new(move |req: &Req| {
        assert_eq!(
            req.headers.get("x-noevia-store-token").map(String::as_str),
            Some(SECRET)
        );
        order
            .lock()
            .unwrap()
            .push(format!("node {} {}", req.method, req.target));
        if req.method == "POST" && req.target == LEASE {
            acquire()
        } else if req.method == "POST" {
            renew()
        } else {
            Answer::Reply(204, String::new())
        }
    })
}

fn grant() -> Answer {
    Answer::Reply(201, GRANT.into())
}
fn short_grant() -> Answer {
    Answer::Reply(
        201,
        // Covers a reload (120 s) by 300 ms only: the confirmation goes stale quickly.
        json!({"lease": ID, "ttlMs": 120_300, "renewMs": 100, "hardMs": 3_600_000}).to_string(),
    )
}
fn renewed() -> Answer {
    Answer::Reply(200, "{}".into())
}

struct Setup {
    _d: tempfile::TempDir,
    _db: Box<dyn std::any::Any>,
    addr: std::net::SocketAddr,
    _stop: tokio::sync::oneshot::Sender<()>,
}

async fn front(router_port: u16, node_port: u16, extra: &[(&str, &str)]) -> Setup {
    let d = tempfile::tempdir().unwrap();
    let db: Box<dyn std::any::Any> = Box::new(seed_database(d.path()));
    let ini = d.path().join("models.ini");
    std::fs::write(&ini, "[A]\nmodel = /models/a.gguf\n").unwrap();
    let mut env: Vec<(String, String)> = vec![
        ("MODEL_MANAGER_KIND".into(), "llamacpp".into()),
        (
            "MODEL_MANAGER_BASE_URL".into(),
            format!("http://127.0.0.1:{router_port}/v1"),
        ),
        ("LLAMACPP_PRESET_PATH".into(), ini.display().to_string()),
        (
            "NOEVIA_LEGACY_UPSTREAM".into(),
            format!("http://127.0.0.1:{node_port}"),
        ),
        ("NOEVIA_PROJECT_STORE_TOKEN".into(), SECRET.into()),
        ("NOEVIA_PROJECT_STORE_PORT".into(), String::new()),
    ];
    for (k, v) in extra {
        env.push(((*k).into(), (*v).into()));
    }
    let (addr, stop) = start(d.path(), &env).await;
    Setup {
        _d: d,
        _db: db,
        addr,
        _stop: stop,
    }
}

async fn reload(addr: std::net::SocketAddr, body: &[u8]) -> (u16, String) {
    let (status, _, text) = call(
        addr,
        "POST",
        "/api/models/presets/reload",
        Some(ADMIN),
        body,
    )
    .await;
    (status, text)
}

async fn until(what: &str, f: impl Fn() -> bool) {
    let started = std::time::Instant::now();
    while !f() {
        assert!(started.elapsed() < Duration::from_secs(10), "{what}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[tokio::test]
async fn the_lease_is_taken_before_the_first_router_call_and_released_after_the_last() {
    let order: Order = Arc::default();
    let (r, _) = engine(router(Arc::clone(&order), &[], 0)).await;
    let (n, _) = engine(node(Arc::clone(&order), grant, renewed)).await;
    let s = front(r, n, &[]).await;
    assert_eq!(
        reload(s.addr, b"{}").await,
        (200, r#"{"reloaded":true,"unloaded":[]}"#.to_owned())
    );
    let seen = order.lock().unwrap().clone();
    assert_eq!(seen[0], format!("node POST {LEASE}"));
    assert_eq!(seen.last().unwrap(), &format!("node DELETE {LEASE}/{ID}"));
    assert!(seen.contains(&"router GET /models?reload=1".to_owned()));
}

#[tokio::test]
async fn node_busy_refuses_with_the_old_409_and_the_router_is_not_touched() {
    let order: Order = Arc::default();
    let (r, _) = engine(router(Arc::clone(&order), &["A"], 0)).await;
    let (n, _) = engine(node(
        Arc::clone(&order),
        || Answer::Reply(409, r#"{"error":"busy","chatActive":true}"#.into()),
        renewed,
    ))
    .await;
    let s = front(r, n, &[]).await;
    assert_eq!(
        reload(s.addr, br#"{"unload":true}"#).await,
        (409, IN_PROGRESS.to_owned())
    );
    assert_eq!(
        order.lock().unwrap().clone(),
        vec![format!("node POST {LEASE}")]
    );
}

#[tokio::test]
async fn node_unreachable_or_refusing_fails_closed_with_nothing_changed() {
    let order: Order = Arc::default();
    let (r, _) = engine(router(Arc::clone(&order), &["A"], 0)).await;
    // A port nothing listens on.
    let s = front(r, 9, &[]).await;
    let (status, body) = reload(s.addr, br#"{"unload":true}"#).await;
    assert_eq!(status, 503, "{body}");
    assert!(body.contains("nothing was changed"), "{body}");
    assert!(order.lock().unwrap().is_empty());
    // Node answers but gives no lease (a wrong secret looks like this).
    let (n, _) = engine(node(
        Arc::clone(&order),
        || Answer::Reply(404, "{}".into()),
        renewed,
    ))
    .await;
    let s = front(r, n, &[]).await;
    assert_eq!(reload(s.addr, b"{}").await.0, 503);
    assert_eq!(
        order.lock().unwrap().clone(),
        vec![format!("node POST {LEASE}")]
    );
    // And with no secret at all, Node is not asked.
    order.lock().unwrap().clear();
    let s = front(r, n, &[("NOEVIA_PROJECT_STORE_TOKEN", "")]).await;
    assert_eq!(reload(s.addr, b"{}").await.0, 503);
    assert!(order.lock().unwrap().is_empty());
}

#[tokio::test]
async fn a_step_the_lease_cannot_outlast_is_not_started() {
    // Node's hard limit falls inside the reload's 120 s limit.
    let order: Order = Arc::default();
    let (r, _) = engine(router(Arc::clone(&order), &[], 0)).await;
    let (n, _) = engine(node(
        Arc::clone(&order),
        || {
            Answer::Reply(
                201,
                json!({"lease": ID, "ttlMs": 180_000, "renewMs": 100, "hardMs": 100_000})
                    .to_string(),
            )
        },
        renewed,
    ))
    .await;
    let s = front(r, n, &[]).await;
    assert_eq!(reload(s.addr, b"{}").await.0, 503);
    assert!(!order
        .lock()
        .unwrap()
        .contains(&"router GET /models?reload=1".to_owned()));
}

#[tokio::test]
async fn a_lease_node_ended_stops_the_work_before_its_next_step() {
    let order: Order = Arc::default();
    let (r, _) = engine(router(Arc::clone(&order), &["A", "B"], 500)).await;
    let (n, _) = engine(node(Arc::clone(&order), short_grant, || {
        Answer::Reply(404, "{}".into())
    }))
    .await;
    let s = front(r, n, &[]).await;
    let (status, body) = reload(s.addr, br#"{"unload":true}"#).await;
    assert_eq!(status, 503, "{body}");
    let seen = order.lock().unwrap().clone();
    assert!(
        !seen.contains(&"router GET /models?reload=1".to_owned()),
        "{seen:?}"
    );
    assert_eq!(
        seen.iter().filter(|l| l.contains("/models/unload")).count(),
        1,
        "{seen:?}"
    );
}

#[tokio::test]
async fn a_node_that_stops_answering_renewals_stops_the_work_when_the_confirmation_runs_out() {
    let order: Order = Arc::default();
    let (r, _) = engine(router(Arc::clone(&order), &["A"], 500)).await;
    let (n, _) = engine(node(Arc::clone(&order), short_grant, || Answer::Drop)).await;
    let s = front(r, n, &[]).await;
    let (status, _) = reload(s.addr, br#"{"unload":true}"#).await;
    assert_eq!(status, 503);
    assert!(!order
        .lock()
        .unwrap()
        .contains(&"router GET /models?reload=1".to_owned()));
}

#[tokio::test]
async fn renewals_keep_a_long_reload_going() {
    let order: Order = Arc::default();
    let (r, _) = engine(router(Arc::clone(&order), &["A"], 500)).await;
    let (n, _) = engine(node(Arc::clone(&order), short_grant, renewed)).await;
    let s = front(r, n, &[]).await;
    assert_eq!(reload(s.addr, br#"{"unload":true}"#).await.0, 200);
    let seen = order.lock().unwrap().clone();
    assert!(
        seen.iter().filter(|l| l.ends_with("/renew")).count() >= 2,
        "{seen:?}"
    );
}

fn gate_without_hold_access(order: Order) -> Handler {
    Arc::new(move |req: &Req| {
        if req.target == "/gate/v1/status" {
            return Answer::Reply(200, r#"{"maintenance":{"held":false}}"#.into());
        }
        order
            .lock()
            .unwrap()
            .push(format!("router {} {}", req.method, req.target));
        match req.target.as_str() {
            "/models" => Answer::Reply(200, r#"{"data":[]}"#.into()),
            "/models?reload=1" => Answer::Reply(200, "{}".into()),
            _ => Answer::Reply(404, "{}".into()),
        }
    })
}

#[tokio::test]
async fn a_gate_with_no_admin_token_refuses_an_unload_and_a_plain_reload_takes_nodes_lease() {
    let order: Order = Arc::default();
    let (r, _) = engine(gate_without_hold_access(Arc::clone(&order))).await;
    let (n, _) = engine(node(Arc::clone(&order), grant, renewed)).await;
    let s = front(r, n, &[]).await;
    let (status, body) = reload(s.addr, br#"{"unload":true}"#).await;
    assert_eq!(status, 503);
    assert!(body.contains("INFERENCE_GATE_ADMIN_TOKEN"), "{body}");
    assert!(order.lock().unwrap().is_empty());
    assert_eq!(reload(s.addr, b"{}").await.0, 200);
    assert_eq!(order.lock().unwrap()[0], format!("node POST {LEASE}"));
}

#[tokio::test]
async fn a_gate_that_grants_is_the_arbiter_and_node_is_not_asked() {
    let order: Order = Arc::default();
    let log = Arc::clone(&order);
    let (r, seen) = engine(Arc::new(move |req: &Req| {
        log.lock().unwrap().push(req.target.clone());
        match (req.method.as_str(), req.target.as_str()) {
            ("POST", "/gate/v1/maintenance") => {
                Answer::Reply(201, json!({"hold": ID, "idle_ttl_ms": 600_000}).to_string())
            }
            ("GET", "/models") => Answer::Reply(200, r#"{"data":[]}"#.into()),
            ("GET", "/models?reload=1") => Answer::Reply(200, "{}".into()),
            _ => Answer::Reply(204, String::new()),
        }
    }))
    .await;
    let node_asked: Order = Arc::default();
    let (n, _) = engine(node(Arc::clone(&node_asked), grant, renewed)).await;
    let s = front(r, n, &[("INFERENCE_GATE_ADMIN_TOKEN", "synthetic-admin")]).await;
    assert_eq!(reload(s.addr, br#"{"unload":true}"#).await.0, 200);
    assert!(node_asked.lock().unwrap().is_empty());
    let held = seen.lock().unwrap();
    assert!(held
        .iter()
        .filter(|q| q.target.starts_with("/models"))
        .all(|q| q.headers.get("x-noevia-maintenance").map(String::as_str) == Some(ID)));
}

fn maint(port: u16, dir: &Path, extra: &[(&str, String)]) -> Maint {
    let mut env: std::collections::HashMap<String, String> = [
        ("MODEL_MANAGER_KIND", "llamacpp".to_string()),
        (
            "MODEL_MANAGER_BASE_URL",
            format!("http://127.0.0.1:{port}/v1"),
        ),
        ("UI_DATA_DIR", dir.display().to_string()),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_owned(), v))
    .collect();
    for (k, v) in extra {
        env.insert((*k).to_owned(), v.clone());
    }
    let get = |k: &str| env.get(k).cloned();
    let engine = Arc::new(server_stats::Engine::from_lookup(&get).unwrap());
    let sizing = Arc::new(noevia_server::model_sizing::Sizing::from_lookup(
        engine.kind(),
        Arc::clone(&engine),
        &get,
    ));
    Maint::from_lookup(engine.kind(), &engine, sizing, &get).unwrap()
}

#[tokio::test]
async fn a_dropped_lease_is_given_back_and_the_next_job_may_run() {
    let d = tempfile::tempdir().unwrap();
    let order: Order = Arc::default();
    let (r, _) = engine(router(Arc::clone(&order), &[], 0)).await;
    let (n, _) = engine(node(Arc::clone(&order), grant, renewed)).await;
    let m = maint(
        r,
        d.path(),
        &[
            ("NOEVIA_LEGACY_UPSTREAM", format!("http://127.0.0.1:{n}")),
            ("NOEVIA_PROJECT_STORE_TOKEN", SECRET.into()),
        ],
    );
    let hold = m.acquire("", Duration::from_secs(60), false).await.unwrap();
    assert!(hold.token().is_none(), "no gate token for the engine");
    assert!(matches!(
        m.acquire("", Duration::from_secs(60), false).await,
        Err(HoldError::Busy)
    ));
    drop(hold);
    let log = Arc::clone(&order);
    until("the drop gave the lease back", move || {
        log.lock()
            .unwrap()
            .contains(&format!("node DELETE {LEASE}/{ID}"))
    })
    .await;
    assert!(!m.local_held());
}

#[tokio::test]
async fn a_finish_during_a_retake_leaves_no_unowned_grant() {
    // L4: the renewal finds the hold gone, and the retake is slow; the job ends meanwhile.
    let d = tempfile::tempdir().unwrap();
    let asked = Arc::new(AtomicUsize::new(0));
    let count = Arc::clone(&asked);
    const SECOND: &str = "ffffffffffffffffffffffffffffffff";
    let (r, seen) = engine(Arc::new(move |req: &Req| {
        match (req.method.as_str(), req.target.as_str()) {
            ("POST", "/gate/v1/maintenance") => {
                if count.fetch_add(1, Ordering::SeqCst) == 0 {
                    Answer::Reply(201, json!({"hold": ID, "idle_ttl_ms": 300}).to_string())
                } else {
                    Answer::After(
                        Duration::from_millis(500),
                        201,
                        json!({"hold": SECOND, "idle_ttl_ms": 60_000}).to_string(),
                    )
                }
            }
            ("POST", _) => Answer::Reply(404, "{}".into()),
            _ => Answer::Reply(204, String::new()),
        }
    }))
    .await;
    let m = maint(
        r,
        d.path(),
        &[("INFERENCE_GATE_ADMIN_TOKEN", "synthetic-admin".into())],
    );
    let hold = m.acquire("", Duration::from_secs(60), false).await.unwrap();
    let count = Arc::clone(&asked);
    until("the retake is under way", move || {
        count.load(Ordering::SeqCst) == 2
    })
    .await;
    hold.release().await;
    let log = Arc::clone(&seen);
    until("the late grant was given back", move || {
        log.lock()
            .unwrap()
            .iter()
            .any(|q| q.method == "DELETE" && q.target.ends_with(SECOND))
    })
    .await;
}

#[tokio::test]
async fn cover_moves_the_deadline_to_fit_the_work() {
    // L4: the deadline at acquire is short of the worst case; the work says what it found.
    assert!(
        noevia_server::model_maint::reload::worst_case(6) > Duration::from_secs(6 * 60 + 120),
        "six unloads and the reload"
    );
    let d = tempfile::tempdir().unwrap();
    let (r, seen) = engine(Arc::new(|req: &Req| {
        if req.method == "POST" && req.target == "/gate/v1/maintenance" {
            return Answer::Reply(201, json!({"hold": ID, "idle_ttl_ms": 300}).to_string());
        }
        if req.method == "POST" {
            return Answer::Reply(200, "{}".into());
        }
        Answer::Reply(204, String::new())
    }))
    .await;
    let m = maint(
        r,
        d.path(),
        &[("INFERENCE_GATE_ADMIN_TOKEN", "synthetic-admin".into())],
    );
    let hold = m
        .acquire("", Duration::from_millis(300), false)
        .await
        .unwrap();
    hold.cover(Duration::from_secs(30));
    tokio::time::sleep(Duration::from_millis(800)).await;
    assert!(
        !seen.lock().unwrap().iter().any(|q| q.method == "DELETE"),
        "the deadline moved, so nothing was given back"
    );
    hold.release().await;
}

#[tokio::test]
async fn the_missing_token_warning_is_for_a_configured_gate_only() {
    let d = tempfile::tempdir().unwrap();
    let (gate, _) = engine(gate_without_hold_access(Arc::default())).await;
    let warning = maint(gate, d.path(), &[]).missing_token_warning().await;
    assert!(warning
        .unwrap()
        .contains("INFERENCE_GATE_ADMIN_TOKEN is not set"));
    let with_token = maint(
        gate,
        d.path(),
        &[("INFERENCE_GATE_ADMIN_TOKEN", "synthetic-admin".into())],
    );
    assert!(with_token.missing_token_warning().await.is_none());
    let (no_gate, _) = engine(router(Arc::default(), &[], 0)).await;
    assert!(maint(no_gate, d.path(), &[])
        .missing_token_warning()
        .await
        .is_none());
}

// ── The evidence inbox ───────────────────────────────────────────────────────────────────────

struct Inbox {
    d: tempfile::TempDir,
    m: Maint,
    props: Arc<AtomicUsize>,
    dir: std::path::PathBuf,
    log: std::path::PathBuf,
    ini: std::path::PathBuf,
}

/// `props_fail`: how many `/props` answers are lost first.
async fn inbox(props_fail: usize) -> Inbox {
    let d = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(d.path().join("models/fx")).unwrap();
    std::fs::write(d.path().join("models/fx/fx.gguf"), vec![7u8; 2048]).unwrap();
    let ini = d.path().join("models.ini");
    std::fs::write(&ini, "[fx]\nmodel = /models/fx/fx.gguf\nctx-size = 8192\n").unwrap();
    let props = Arc::new(AtomicUsize::new(0));
    let count = Arc::clone(&props);
    let (r, _) = engine(Arc::new(move |req: &Req| match req.target.as_str() {
        "/models" => Answer::Reply(
            200,
            r#"{"data":[{"id":"fx","status":{"value":"unloaded","args":["--model","/models/fx/fx.gguf"]}}]}"#.into(),
        ),
        "/props" => {
            if count.fetch_add(1, Ordering::SeqCst) < props_fail {
                Answer::Drop
            } else {
                Answer::Reply(200, r#"{"build_info":"b"}"#.into())
            }
        }
        _ => Answer::Reply(404, "{}".into()),
    }))
    .await;
    let base = format!("http://127.0.0.1:{r}/v1");
    let sfx = suffix(&base);
    let m = maint(
        r,
        d.path(),
        &[
            ("LLAMACPP_PRESET_PATH", ini.display().to_string()),
            (
                "LLAMACPP_MODELS_PATH",
                d.path().join("models").display().to_string(),
            ),
        ],
    );
    m.set_retry_base(Duration::from_millis(1));
    let dir = d.path().join(format!("evidence-inbox-{sfx}"));
    std::fs::create_dir_all(&dir).unwrap();
    let log = d.path().join(format!("evidence-{sfx}/evidence.jsonl"));
    Inbox {
        d,
        m,
        props,
        dir,
        log,
        ini,
    }
}

impl Inbox {
    fn put(&self, name: &str, entry: &Value) {
        std::fs::write(self.dir.join(name), entry.to_string()).unwrap();
    }
    fn left(&self) -> Vec<String> {
        let mut v: Vec<String> = std::fs::read_dir(&self.dir)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        v.sort();
        v
    }
    fn lines(&self) -> Vec<Value> {
        std::fs::read_to_string(&self.log)
            .unwrap_or_default()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect()
    }
    async fn live(&self) -> String {
        let body = self.m.evidence_body("fx").await.unwrap();
        serde_json::from_str::<Value>(&body).unwrap()["identityHash"]
            .as_str()
            .unwrap()
            .to_owned()
    }
}

fn result(result: &str, hash: Option<&str>, at: Option<i64>) -> Value {
    let mut e = json!({"kind": "record", "model": "fx", "record": {
        "category": "vision", "result": result, "value": null,
        "suite": {"name": "s", "version": 1}, "source": "probe", "limitations": []}});
    if let Some(h) = hash {
        e["identityHash"] = json!(h);
    }
    if let Some(a) = at {
        e["at"] = json!(a);
    }
    e
}

#[tokio::test]
async fn a_result_is_filed_only_under_the_identity_it_was_measured_on_with_its_own_time() {
    let i = inbox(0).await;
    let hash = i.live().await;
    i.put(
        "001.json",
        &result("passed", Some(&hash), Some(1_700_000_000_123)),
    );
    assert_eq!(i.m.drain_inbox().await, 1);
    let lines = i.lines();
    assert_eq!(lines.len(), 1);
    assert_eq!(lines[0]["identityHash"], json!(hash));
    assert_eq!(lines[0]["at"], json!(1_700_000_000_123i64));

    // The configuration changes between the record and the drain: discarded, not refiled.
    std::fs::write(
        &i.ini,
        "[fx]\nmodel = /models/fx/fx.gguf\nctx-size = 4096\n",
    )
    .unwrap();
    i.put(
        "002.json",
        &result("failed", Some(&hash), Some(1_700_000_000_456)),
    );
    assert_eq!(i.m.drain_inbox().await, 1);
    assert_eq!(i.lines().len(), 1, "measured on another configuration");
    assert!(i.left().is_empty());
}

#[tokio::test]
async fn an_entry_left_by_a_rollback_has_no_identity_and_is_discarded_while_a_rate_still_files() {
    let i = inbox(0).await;
    i.put("001.json", &result("passed", None, None));
    i.put(
        "002.json",
        &json!({"kind": "record", "model": "fx", "record": {
            "category": "throughput", "result": "reported", "value": {"rate": 5.0},
            "suite": {"name": "s", "version": 1}, "source": "chat", "limitations": []}}),
    );
    assert_eq!(i.m.drain_inbox().await, 2);
    assert!(i.left().is_empty());
    let lines = i.lines();
    assert_eq!(lines.len(), 1);
    assert_eq!(lines[0]["result"], json!("reported"));
    assert_eq!(lines[0]["identityHash"].as_str().unwrap().len(), 64);
}

#[tokio::test]
async fn a_build_node_could_not_read_is_no_change_of_configuration() {
    let i = inbox(0).await;
    let body = i.m.evidence_body("fx").await.unwrap();
    let identity_with_null_build = {
        // Filed once under the real identity, then Node's hash is rebuilt from it with no build.
        let hash = serde_json::from_str::<Value>(&body).unwrap()["identityHash"]
            .as_str()
            .unwrap()
            .to_owned();
        i.put("001.json", &result("passed", Some(&hash), None));
        i.m.drain_inbox().await;
        let mut identity = i.lines()[0]["identity"].clone();
        identity["build"] = Value::Null;
        let parsed = js_json::parse(&identity.to_string()).unwrap();
        noevia_server::model_maint::evidence::identity_hash(&parsed)
    };
    let mut e = result("failed", Some(&identity_with_null_build), None);
    e["buildUnknown"] = json!(true);
    i.put("002.json", &e);
    i.m.drain_inbox().await;
    assert_eq!(i.lines().len(), 2, "filed under the live identity");
    // Without the mark the same hash is a different configuration.
    let unmarked = result("passed", Some(&identity_with_null_build), None);
    i.put("003.json", &unmarked);
    i.m.drain_inbox().await;
    assert_eq!(i.lines().len(), 2);
}

#[tokio::test]
async fn a_transient_failure_does_not_lose_the_record() {
    // The next identity reads have no build, so the hash differs: tried again, then filed.
    let j = inbox(2).await;
    let mut want = None;
    while want.is_none() {
        let body = j.m.evidence_body("fx").await.unwrap();
        let v: Value = serde_json::from_str(&body).unwrap();
        if j.props.load(Ordering::SeqCst) > 2 {
            want = v["identityHash"].as_str().map(str::to_owned);
        }
    }
    let want = want.unwrap();
    j.props.store(0, Ordering::SeqCst);
    j.put("001.json", &result("passed", Some(&want), None));
    let mut passes = 0;
    while !j.left().is_empty() {
        j.m.drain_inbox().await;
        tokio::time::sleep(Duration::from_millis(30)).await;
        passes += 1;
        assert!(passes < 20);
    }
    assert!(passes >= 2, "it took more than one try");
    assert_eq!(j.lines().len(), 1, "filed once the engine answered");
}

#[tokio::test]
async fn a_record_that_keeps_failing_is_discarded_after_a_bounded_number_of_tries_without_blocking()
{
    let i = inbox(0).await;
    // The model file is gone: no identity, every try.
    std::fs::remove_file(i.d.path().join("models/fx/fx.gguf")).unwrap();
    let hash = "a".repeat(64);
    i.put("001.json", &result("passed", Some(&hash), None));
    i.put("002.json", &json!({"kind": "frobnicate", "model": "fx"}));
    i.m.drain_inbox().await;
    assert_eq!(i.left(), ["001.json"], "the one behind it was not held up");
    let mut tries = 1;
    while !i.left().is_empty() {
        tokio::time::sleep(Duration::from_millis(40)).await;
        i.m.drain_inbox().await;
        tries += 1;
        assert!(tries < 30, "bounded");
    }
    assert_eq!(i.lines().len(), 0);
}

#[tokio::test]
async fn a_failing_remove_does_not_apply_the_entry_again() {
    use std::os::unix::fs::PermissionsExt;
    let i = inbox(0).await;
    let hash = i.live().await;
    i.put("001.json", &result("passed", Some(&hash), None));
    std::fs::set_permissions(&i.dir, std::fs::Permissions::from_mode(0o555)).unwrap();
    i.m.drain_inbox().await;
    let reads = i.props.load(Ordering::SeqCst);
    for _ in 0..3 {
        i.m.drain_inbox().await;
    }
    assert_eq!(
        i.props.load(Ordering::SeqCst),
        reads,
        "no identity read means no second apply"
    );
    assert_eq!(i.left(), ["001.json"]);
    std::fs::set_permissions(&i.dir, std::fs::Permissions::from_mode(0o755)).unwrap();
    i.m.drain_inbox().await;
    assert!(i.left().is_empty(), "removed once it can be");
    assert_eq!(i.lines().len(), 1);
}

#[tokio::test]
async fn an_orphan_tmp_older_than_an_hour_is_swept_and_a_young_one_is_not() {
    let i = inbox(0).await;
    std::fs::write(i.dir.join("old.tmp"), "{").unwrap();
    std::fs::write(i.dir.join("new.tmp"), "{").unwrap();
    std::fs::File::options()
        .write(true)
        .open(i.dir.join("old.tmp"))
        .unwrap()
        .set_modified(std::time::SystemTime::now() - Duration::from_secs(3700))
        .unwrap();
    i.m.drain_inbox().await;
    assert_eq!(i.left(), ["new.tmp"]);
}

#[tokio::test]
async fn a_panicking_drain_pass_does_not_end_the_loop() {
    let passes = Arc::new(AtomicUsize::new(0));
    let count = Arc::clone(&passes);
    let task = tokio::spawn(supervise(Duration::from_millis(10), move || {
        let n = count.fetch_add(1, Ordering::SeqCst);
        async move {
            assert!(n != 0, "the first pass panics");
        }
    }));
    let seen = Arc::clone(&passes);
    until("passes after the panic", move || {
        seen.load(Ordering::SeqCst) >= 3
    })
    .await;
    assert!(!task.is_finished());
    task.abort();
}

// ── Against Core's real route and gate ───────────────────────────────────────────────────────

const REAL_NODE: &str = r#"
const http = require('node:http');
const core = process.argv[1];
const { createMaintenanceGate } = require(core + '/server/inference-maintenance.cjs');
const { createLeaseRoutes } = require(core + '/server/model-maintenance-lease.cjs');
const gate = createMaintenanceGate();
const json = (res, status, body) => { res.writeHead(status, { 'content-type': 'application/json' }); res.end(JSON.stringify(body)); };
const routes = createLeaseRoutes({ secret: process.argv[2], hold: () => gate.hold(), json, ttlMs: Number(process.argv[3]) });
let leave = null;
const server = http.createServer(async (req, res) => {
  if (await routes.handle(req, res)) return;
  if (req.url === '/chat/start') {
    try { leave = gate.enter(); return json(res, 200, { chat: 'running' }); } catch (e) { return json(res, e.status, { error: 'held' }); }
  }
  if (req.url === '/chat/end') { leave?.(); leave = null; return json(res, 200, {}); }
  if (req.url === '/tune') {
    try { gate.hold()(); return json(res, 200, { tune: 'started' }); } catch (e) { return json(res, e.status, { error: 'held' }); }
  }
  if (req.url === '/held') return json(res, 200, { held: gate.held() });
  json(res, 404, {});
});
server.listen(0, '127.0.0.1', () => console.log(server.address().port));
"#;

struct RealNode {
    child: std::process::Child,
    port: u16,
    client: reqwest::Client,
}

impl RealNode {
    async fn start(ttl_ms: u64) -> Option<RealNode> {
        let Ok(core) = std::env::var("NOEVIA_MCP_CORE") else {
            assert!(
                std::env::var("NOEVIA_MCP_CORE_REQUIRED").is_err(),
                "NOEVIA_MCP_CORE is required here"
            );
            eprintln!("skipped: NOEVIA_MCP_CORE is not set");
            return None;
        };
        let mut child = std::process::Command::new("node")
            .args(["-e", REAL_NODE, "--", &core, SECRET, &ttl_ms.to_string()])
            .stdout(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        let out = child.stdout.take().unwrap();
        let line = tokio::task::spawn_blocking(move || {
            let mut line = String::new();
            std::io::BufRead::read_line(&mut std::io::BufReader::new(out), &mut line).unwrap();
            line
        });
        let line = tokio::time::timeout(Duration::from_secs(20), line)
            .await
            .unwrap()
            .unwrap();
        Some(RealNode {
            child,
            port: line.trim().parse().unwrap(),
            client: reqwest::Client::builder().no_proxy().build().unwrap(),
        })
    }
    async fn ask(&self, path: &str) -> (u16, Value) {
        let r = self
            .client
            .get(format!("http://127.0.0.1:{}{path}", self.port))
            .send()
            .await
            .unwrap();
        let status = r.status().as_u16();
        (
            status,
            serde_json::from_slice(&r.bytes().await.unwrap()).unwrap(),
        )
    }
}

impl Drop for RealNode {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[tokio::test]
async fn real_node_chat_refuses_the_reload_and_the_reload_holds_chat_and_tune_back() {
    let Some(node) = RealNode::start(180_000).await else {
        return;
    };
    let order: Order = Arc::default();
    let (r, _) = engine(router(Arc::clone(&order), &["A"], 700)).await;
    let s = front(r, node.port, &[]).await;
    // Chat active: refused, with the old 409, and the router is not touched.
    assert_eq!(node.ask("/chat/start").await.0, 200);
    assert_eq!(
        reload(s.addr, br#"{"unload":true}"#).await,
        (409, IN_PROGRESS.to_owned())
    );
    assert!(order.lock().unwrap().is_empty());
    assert_eq!(node.ask("/chat/end").await.0, 200);
    // The reload runs: chat and tune are held back for as long as it does.
    let addr = s.addr;
    let job = tokio::spawn(async move { reload(addr, br#"{"unload":true}"#).await });
    let log = Arc::clone(&order);
    until("the reload is unloading", move || {
        log.lock().unwrap().iter().any(|l| l.contains("unload"))
    })
    .await;
    assert_eq!(node.ask("/held").await.1["held"], json!(true));
    assert_eq!(node.ask("/chat/start").await.0, 503);
    assert_eq!(node.ask("/tune").await.0, 409);
    assert_eq!(job.await.unwrap().0, 200);
    // Over: everything runs again.
    assert_eq!(node.ask("/held").await.1["held"], json!(false));
    assert_eq!(node.ask("/tune").await.0, 200);
    assert_eq!(node.ask("/chat/start").await.0, 200);
}

#[tokio::test]
async fn real_node_a_lease_whose_front_died_expires_by_itself() {
    let Some(node) = RealNode::start(1000).await else {
        return;
    };
    let d = tempfile::tempdir().unwrap();
    let (r, _) = engine(router(Arc::default(), &[], 0)).await;
    let m = maint(
        r,
        d.path(),
        &[
            (
                "NOEVIA_LEGACY_UPSTREAM",
                format!("http://127.0.0.1:{}", node.port),
            ),
            ("NOEVIA_PROJECT_STORE_TOKEN", SECRET.into()),
        ],
    );
    let hold = m.acquire("", Duration::from_secs(60), false).await.unwrap();
    assert_eq!(node.ask("/held").await.1["held"], json!(true));
    // Renewals keep it past its limit...
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert_eq!(node.ask("/held").await.1["held"], json!(true));
    // ...and a front that dies neither renews nor releases: forgotten here, and its runtime stopped.
    let rt_gone = tokio::task::spawn_blocking(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let hold2 = rt.block_on(async {
            drop(hold);
            tokio::time::sleep(Duration::from_millis(100)).await;
            m.acquire("", Duration::from_secs(60), false).await.unwrap()
        });
        std::mem::forget(hold2);
        drop(rt);
    });
    rt_gone.await.unwrap();
    assert_eq!(node.ask("/held").await.1["held"], json!(true));
    tokio::time::sleep(Duration::from_millis(2000)).await;
    assert_eq!(node.ask("/held").await.1["held"], json!(false), "expired");
    assert_eq!(node.ask("/chat/start").await.0, 200);
}
