//! Row A: POST /api/models/load and /unload replayed against the Node oracle recorded at core 088d880
//! (tests/fixtures/model-load-node.v1.json): the same public answer for every scenario, over a synthetic
//! router and a fake Node (admission lease, sidecar facts, read invalidation). No engine, no model.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]
mod maint_support;
use maint_support::*;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

const SECRET: &str = "synthetic-store-token-0123456789abcdef";

fn oracle() -> Value {
    serde_json::from_str(include_str!("fixtures/model-load-node.v1.json")).unwrap()
}

/// What one router script entry answers.
fn answer_of(entry: &Value) -> Answer {
    if entry.get("throw") == Some(&Value::Bool(true)) {
        return Answer::Drop;
    }
    if let Some(state) = entry.get("rows").and_then(Value::as_str) {
        return Answer::Reply(
            200,
            json!({"data":[{"id":"m","status":{"value":state}}]}).to_string(),
        );
    }
    let status = entry["status"].as_u64().unwrap() as u16;
    Answer::Reply(status, entry["body"].to_string())
}

struct World {
    addr: std::net::SocketAddr,
    _stop: tokio::sync::oneshot::Sender<()>,
    _dir: tempfile::TempDir,
    router: Arc<Mutex<Vec<Req>>>,
    node: Arc<Mutex<Vec<Req>>>,
    maint: Arc<noevia_server::model_maint::Maint>,
    barrier: Arc<Mutex<bool>>,
}

type Script = Arc<Mutex<HashMap<String, Vec<Value>>>>;

impl World {
    async fn new(s: &Value) -> World {
        static START: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(10);
        engine_admission::authority::global()
            .node_session(
                "load-tests",
                START.fetch_add(10, std::sync::atomic::Ordering::SeqCst),
            )
            .unwrap();
        let dir = tempfile::tempdir().unwrap();
        let _db = seed_database(dir.path());
        let ini = dir.path().join("models.ini");
        std::fs::write(&ini, "[m]\nmodel=/models/m.gguf\nctx-size=4096\n").unwrap();
        let restored = s.get("restored") == Some(&Value::Bool(true));
        let missing = s.get("missingFile") == Some(&Value::Bool(true)) || restored;
        let mut script: HashMap<String, Vec<Value>> = HashMap::new();
        if let Some(r) = s.get("router").and_then(Value::as_object) {
            for (k, v) in r {
                script.insert(k.clone(), v.as_array().unwrap().clone());
            }
        }
        // The front reads the installed list once before a load reaches the engine (Node read its own,
        // which the oracle's fake answered without the router): that read takes a copy of the first answer,
        // or no answer when Node's read failed.
        let first = if s.get("installedThrows") == Some(&Value::Bool(true)) {
            Some(json!({"throw": true}))
        } else {
            script.get("GET /models").and_then(|q| q.first().cloned())
        };
        let reaches_engine = s["path"] == "/api/models/load";
        if reaches_engine {
            if let Some(f) = first {
                let q = script.entry("GET /models".into()).or_default();
                if q.is_empty() {
                    // The last entry repeats: keep the default listing after a one-off failure.
                    q.push(json!({"rows": "loaded"}));
                }
                q.insert(0, f);
            }
        }
        let script: Script = Arc::new(Mutex::new(script));
        let lose_renewal = s.get("loseRenewal") == Some(&Value::Bool(true));
        let barrier = Arc::new(Mutex::new(false));
        let node_barrier = Arc::clone(&barrier);
        let delay_load_ack = s.get("delayLoadAck") == Some(&Value::Bool(true));
        let verified_gate = s.get("verifiedGate") == Some(&Value::Bool(true));
        let long_unload = s.get("longUnload") == Some(&Value::Bool(true));
        let long_load = s.get("longLoad") == Some(&Value::Bool(true));
        let launched = Mutex::new(None::<std::time::Instant>);
        let (port, router) = engine(Arc::new(move |req: &Req| {
            let key = format!("{} {}", req.method, req.target.split('?').next().unwrap());
            if key == "GET /gate/v1/capabilities" && verified_gate {
                return Answer::Reply(200, json!({"protocol":"noevia-gate-capabilities/1", "capabilities":["noevia-model-load-settlement/1"]}).to_string());
            }
            if long_unload {
                if key == "POST /models/unload" {
                    *launched.lock().unwrap() = Some(std::time::Instant::now());
                    return Answer::After(Duration::from_millis(500), 200, "{}".into());
                }
                if key == "GET /models" {
                    let state = match *launched.lock().unwrap() {
                        None => "loaded",
                        Some(t) if t.elapsed() < Duration::from_millis(1500) => "unloading",
                        Some(_) => "unloaded",
                    };
                    return Answer::Reply(200, json!({"data":[{"id":"m", "status":{"value":state}}]}).to_string());
                }
            }
            if long_load {
                if key == "POST /models/load" {
                    *launched.lock().unwrap() = Some(std::time::Instant::now());
                    return if delay_load_ack { Answer::After(Duration::from_millis(500), 200, "{}".into()) } else { Answer::Reply(200, "{}".into()) };
                }
                if key == "GET /models" {
                    let state = match *launched.lock().unwrap() {
                        None => "unloaded",
                        Some(t) if t.elapsed() < Duration::from_millis(1500) => "loading",
                        Some(_) => "loaded",
                    };
                    return Answer::Reply(200, json!({"data":[{"id":"m", "status":{"value":state}}]}).to_string());
                }
            }
            if missing && key == "GET /models" {
                return Answer::Reply(
                    200,
                    json!({"data":[{"id":"m","source":"preset","status":{"value": if restored {"loaded"} else {"unloaded"}}}]})
                        .to_string(),
                );
            }
            let mut s = script.lock().unwrap();
            match s.get_mut(&key) {
                Some(q) if !q.is_empty() => {
                    let next = if q.len() > 1 {
                        q.remove(0)
                    } else {
                        q[0].clone()
                    };
                    answer_of(&next)
                }
                _ => Answer::Reply(
                    200,
                    json!({"data":[{"id":"m","status":{"value":"loaded"}}]}).to_string(),
                ),
            }
        }))
        .await;
        // Core's lease route at this branch also carries the gate's `retryable: true` (rust-model-load.test.cjs);
        // the oracle recorded the route before that one-line change.
        let mut lease = s.get("lease").cloned();
        if s["acquire"]["gate"]["retryable"] == Value::Bool(true) {
            if let Some(l) = lease.as_mut() {
                l["body"]["retryable"] = Value::Bool(true);
            }
        }
        let invalidated = Arc::new(Mutex::new(None::<std::time::Instant>));
        let invalidate_fails = s.get("invalidateFails") == Some(&Value::Bool(true));
        let expired = s.get("expiredGrant") == Some(&Value::Bool(true));
        let (node_port, node) = engine(Arc::new(move |req: &Req| {
            if req.headers.get("x-noevia-store-token").map(String::as_str) != Some(SECRET) {
                return Answer::Reply(404, "{}".into());
            }
            let recovery_prefix = "/internal/model-maintenance/lease/recovery/";
            if req.target.starts_with(recovery_prefix) {
                if req.method == "POST" { *node_barrier.lock().unwrap() = true; }
                if req.method == "DELETE" {
                    let proof: Value = serde_json::from_str(&req.body).unwrap();
                    assert!(proof["lease"].is_string());
                    assert_eq!(proof["generation"], 1);
                    *node_barrier.lock().unwrap() = false;
                }
                return Answer::Reply(204, String::new());
            }
            if req.method == "POST" && req.target == "/internal/model-maintenance/lease" {
                return Answer::Reply(201, json!({"lease":"0123456789abcdef0123456789abcdef", "generation":1,"ttlMs":30000,"renewMs":10000,"hardMs":3600000}).to_string());
            }
            match (req.method.as_str(), req.target.as_str()) {
                ("DELETE", _) => Answer::Reply(204, String::new()),
                (_, t) if t.ends_with("/renew") => Answer::Reply(if lose_renewal && t.contains("inference-admission") { 404 } else { 200 }, "{}".into()),
                ("PUT", t) if t.starts_with("/internal/inference-admission/lease/") => match &lease
                {
                    Some(l) => {
                        Answer::Reply(l["status"].as_u64().unwrap() as u16, l["body"].to_string())
                    }
                    None => {
                        if *node_barrier.lock().unwrap() { return Answer::Reply(503, json!({"code":"maintenance_held","message":"busy","retryable":true}).to_string()); }
                        let ask: Value = serde_json::from_str(&req.body).unwrap();
                        let headers = if ask.get("embedding").is_some() {
                            json!(null)
                        } else {
                            json!({"x-noevia-lease":"syntheticLease1"})
                        };
                        let mut b = json!({"lease":t.rsplit('/').next().unwrap(),"ttlMs": if lose_renewal { 150 } else { 30000 }});
                        if !headers.is_null() {
                            b["headers"] = headers;
                        }
                        if expired {
                            b["ttlMs"] = json!(1);
                            return Answer::After(Duration::from_millis(50), 201, b.to_string());
                        }
                        Answer::Reply(201, b.to_string())
                    }
                },
                ("POST", "/internal/models/facts") => {
                    let scan = if restored
                        && invalidated
                            .lock()
                            .unwrap()
                            .is_some_and(|t| t.elapsed() >= Duration::from_millis(70))
                    {
                        json!([{ "key":"m.gguf", "modelId":"m" }])
                    } else if missing {
                        json!([{"key":"other.gguf","modelId":"other"}])
                    } else {
                        json!([])
                    };
                    Answer::Reply(
                        200,
                        json!({"sidecarNames":["embed-sidecar"],"sidecarLoaded":[],"scan":scan})
                            .to_string(),
                    )
                }
                ("POST", "/internal/models/invalidate") => {
                    *invalidated.lock().unwrap() = Some(std::time::Instant::now());
                    if invalidate_fails {
                        Answer::Reply(503, "{}".into())
                    } else {
                        Answer::After(Duration::from_millis(80), 200, r#"{"ok":true}"#.into())
                    }
                }
                _ => Answer::Reply(404, "{}".into()),
            }
        }))
        .await;
        let kind = if s.get("kind").and_then(Value::as_str) == Some("none") {
            "none"
        } else {
            "llamacpp"
        };
        let base = format!("http://127.0.0.1:{port}/v1");
        let mut env = vec![
            ("MODEL_MANAGER_KIND".to_owned(), kind.to_owned()),
            ("MODEL_MANAGER_BASE_URL".into(), base.clone()),
            ("INFERENCE_BASE_URL".into(), base),
            ("MODEL_MANAGER_API_KEY".into(), "synthetic-key".into()),
            ("LLAMACPP_PRESET_PATH".into(), ini.display().to_string()),
            (
                "NOEVIA_LEGACY_UPSTREAM".into(),
                format!("http://127.0.0.1:{node_port}"),
            ),
            ("NOEVIA_PROJECT_STORE_TOKEN".into(), SECRET.into()),
        ];
        if s.get("admin") == Some(&Value::Bool(true)) {
            env.push((
                "INFERENCE_GATE_ADMIN_TOKEN".into(),
                "synthetic-admin".into(),
            ));
        }
        let (addr, stop, app) = start_with_app(dir.path(), &env).await;
        app.config
            .maint
            .authority
            .node_session("synthetic-start", 1)
            .unwrap();
        World {
            maint: Arc::clone(&app.config.maint),
            barrier,
            addr,
            _stop: stop,
            _dir: dir,
            router,
            node,
        }
    }

    async fn post(&self, path: &str, body: &str, cookie: &str) -> (u16, Value) {
        let (s, _, b) = call(self.addr, "POST", path, Some(cookie), body.as_bytes()).await;
        (s, serde_json::from_str(&b).unwrap_or(Value::String(b)))
    }

    /// The router calls a load or unload made (the installed-list read before a load left out).
    fn engine_calls(&self, skip_first_listing: bool) -> Vec<String> {
        let mut out = Vec::new();
        let mut skipped = !skip_first_listing;
        for r in self.router.lock().unwrap().iter() {
            let key = format!("{} {}", r.method, r.target.split('?').next().unwrap());
            if !skipped && key == "GET /models" {
                skipped = true;
                continue;
            }
            // The authenticated capability handshake is separate from Node's engine oracle.
            if key == "GET /gate/v1/capabilities" {
                continue;
            }
            let lease = r.headers.contains_key("x-noevia-lease");
            let admin = r.headers.contains_key("x-noevia-gate-admin");
            out.push(format!(
                "{key}{}{}",
                if lease { "+L" } else { "" },
                if admin { "+A" } else { "" }
            ));
        }
        out
    }

    /// Every lease the front took was given back.
    async fn leases_settled(&self) {
        let end = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            let node = self.node.lock().unwrap().clone();
            let granted = node
                .iter()
                .filter(|r| {
                    r.target.starts_with("/internal/inference-admission/lease/")
                        && r.method == "PUT"
                })
                .count();
            let released = node
                .iter()
                .filter(|r| {
                    r.method == "DELETE"
                        && r.target.starts_with("/internal/inference-admission/lease/")
                })
                .count();
            if released >= granted || tokio::time::Instant::now() >= end {
                return;
            }
            drop(node);
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }
}

#[tokio::test]
async fn every_recorded_node_answer_is_the_fronts() {
    let o = oracle();
    assert_eq!(o["core"], "088d880");
    let scenarios = o["scenarios"].as_array().unwrap();
    assert_eq!(scenarios.len(), 41);
    let mut failures = Vec::new();
    for s in scenarios {
        let w = World::new(s).await;
        let cookie = if s.get("role").and_then(Value::as_str) == Some("user") {
            MEMBER
        } else {
            ADMIN
        };
        let got = w
            .post(
                s["path"].as_str().unwrap(),
                s["raw"].as_str().unwrap(),
                cookie,
            )
            .await;
        let want = (
            s["expect"]["status"].as_u64().unwrap() as u16,
            s["expect"]["body"].clone(),
        );
        if got != want {
            failures.push(format!("{}: got {:?}, want {:?}", s["name"], got, want));
            continue;
        }
        // The engine saw what Node's adapter sent: the same calls, the lease header on the load, the admin
        // header only on a forced unload.
        let want_calls: Vec<String> = s["routerCalls"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| {
                format!(
                    "{}{}{}",
                    c["call"].as_str().unwrap(),
                    if c["lease"].is_string() { "+L" } else { "" },
                    if c["admin"] == Value::Bool(true) {
                        "+A"
                    } else {
                        ""
                    }
                )
            })
            .collect();
        let calls = w.engine_calls(s["path"] == "/api/models/load");
        if !calls.starts_with(&want_calls)
            || calls
                .iter()
                .skip(want_calls.len())
                .any(|c| c != "GET /models")
        {
            failures.push(format!(
                "{}: engine calls {:?}, want {:?}",
                s["name"], calls, want_calls
            ));
        }
        w.leases_settled().await;
        let node = w.node.lock().unwrap().clone();
        let granted = node
            .iter()
            .filter(|r| {
                r.target.starts_with("/internal/inference-admission/lease/") && r.method == "PUT"
            })
            .count();
        let released = node
            .iter()
            .filter(|r| {
                r.method == "DELETE" && r.target.starts_with("/internal/inference-admission/lease/")
            })
            .count();
        if s.get("lease").is_none() && released != granted {
            failures.push(format!(
                "{}: {granted} leases, {released} given back",
                s["name"]
            ));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

#[tokio::test]
async fn refusals_before_the_engine_take_no_lease() {
    let o = oracle();
    for s in o["scenarios"].as_array().unwrap() {
        if !s["routerCalls"].as_array().unwrap().is_empty() || s.get("lease").is_some() {
            continue;
        }
        let w = World::new(s).await;
        let cookie = if s.get("role").and_then(Value::as_str) == Some("user") {
            MEMBER
        } else {
            ADMIN
        };
        w.post(
            s["path"].as_str().unwrap(),
            s["raw"].as_str().unwrap(),
            cookie,
        )
        .await;
        let leases = w
            .node
            .lock()
            .unwrap()
            .iter()
            .filter(|r| r.target.starts_with("/internal/inference-admission"))
            .count();
        assert_eq!(leases, 0, "{}", s["name"]);
        assert!(
            w.router.lock().unwrap().iter().all(|r| r.method == "GET"),
            "{}",
            s["name"]
        );
    }
}

#[tokio::test]
async fn a_load_takes_the_engine_lease_and_an_unload_only_the_maintenance_gate() {
    let o = oracle();
    let pick = |n: &str| {
        o["scenarios"]
            .as_array()
            .unwrap()
            .iter()
            .find(|s| s["name"] == n)
            .unwrap()
            .clone()
    };
    let w = World::new(&pick("load ready")).await;
    assert_eq!(
        w.post("/api/models/load", r#"{"name":"m"}"#, ADMIN).await.0,
        200
    );
    w.leases_settled().await;
    let asks: Vec<Value> = w
        .node
        .lock()
        .unwrap()
        .iter()
        .filter(|r| {
            r.target.starts_with("/internal/inference-admission/lease/") && r.method == "PUT"
        })
        .map(|r| serde_json::from_str(&r.body).unwrap())
        .collect();
    assert_eq!(asks, vec![json!({"model":"m"})]);
    let w = World::new(&pick("unload ok")).await;
    assert_eq!(
        w.post("/api/models/unload", r#"{"name":"m"}"#, ADMIN)
            .await
            .0,
        200
    );
    w.leases_settled().await;
    let node = w.node.lock().unwrap().clone();
    let asks: Vec<Value> = node
        .iter()
        .filter(|r| {
            r.target.starts_with("/internal/inference-admission/lease/") && r.method == "PUT"
        })
        .map(|r| serde_json::from_str(&r.body).unwrap())
        .collect();
    assert_eq!(asks, vec![json!({"model":"m","embedding":true})]);
    assert_eq!(
        node.iter()
            .filter(|r| r.method == "DELETE"
                && r.target.starts_with("/internal/inference-admission/lease/"))
            .count(),
        1
    );
    // Node's shared reads were dropped for the write.
    assert!(node
        .iter()
        .any(|r| r.target == "/internal/models/invalidate"));
}

#[tokio::test]
async fn a_maintenance_hold_or_an_unreachable_node_refuses_without_touching_the_engine() {
    let o = oracle();
    let base = o["scenarios"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["name"] == "unload ok")
        .unwrap()
        .clone();
    for (status, body) in [
        (
            503,
            json!({"error":"Model configuration is being applied. Try again shortly."}),
        ),
        (404, json!({"error":"Not found"})),
    ] {
        let mut s = base.clone();
        s["lease"] = json!({"status":status,"body":body});
        for (path, raw) in [
            ("/api/models/unload", r#"{"name":"m"}"#),
            ("/api/models/load", r#"{"name":"m"}"#),
        ] {
            let w = World::new(&s).await;
            assert_eq!(
                w.post(path, raw, ADMIN).await,
                (503, json!({"error":"Internal error"})),
                "{path} {status}"
            );
            assert!(
                w.router.lock().unwrap().iter().all(|r| r.method == "GET"),
                "{path}"
            );
        }
    }
}

#[tokio::test]
async fn a_caller_who_leaves_mid_load_retains_exclusion_until_settled() {
    let o = oracle();
    let mut s = o["scenarios"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["name"] == "load ready")
        .unwrap()
        .clone();
    s["longLoad"] = json!(true);
    // This fake engine continues independently after its launch acknowledgement.
    s["router"] = json!({"GET /models":[{"rows":"unloaded"},{"rows":"unloaded"},{"rows":"loading"},{"rows":"loading"},{"rows":"loading"},{"rows":"loading"},{"rows":"loading"},{"rows":"loaded"}],"POST /models/load":[{"status":200,"body":{}}]});
    let w = World::new(&s).await;
    let addr = w.addr;
    let task = tokio::spawn(async move {
        call(
            addr,
            "POST",
            "/api/models/load",
            Some(ADMIN),
            br#"{"name":"m"}"#,
        )
        .await
    });
    let end = tokio::time::Instant::now() + Duration::from_secs(10);
    while !w
        .router
        .lock()
        .unwrap()
        .iter()
        .any(|r| r.target == "/models/load")
    {
        assert!(
            tokio::time::Instant::now() < end,
            "the load never reached the engine"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    task.abort();
    let _ = task.await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(
        !w.node.lock().unwrap().iter().any(|r| r.method == "DELETE"),
        "launched work released before settlement"
    );
    // The synthetic engine finishes independently of the disconnected HTTP caller.
    let end = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let released = w
            .node
            .lock()
            .unwrap()
            .iter()
            .filter(|r| {
                r.method == "DELETE" && r.target.starts_with("/internal/inference-admission/lease/")
            })
            .count();
        if released == 1 {
            break;
        }
        assert!(
            tokio::time::Instant::now() < end,
            "the lease was not given back"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    // And it stops asking the engine.
    let seen = w.router.lock().unwrap().len();
    tokio::time::sleep(Duration::from_millis(600)).await;
    assert!(w.router.lock().unwrap().len() <= seen + 1);
}

#[tokio::test]
async fn freshness_and_expired_grants_refuse_before_engine_mutation() {
    for flag in ["invalidateFails", "expiredGrant"] {
        let o = oracle();
        let mut s = o["scenarios"]
            .as_array()
            .unwrap()
            .iter()
            .find(|s| s["name"] == "load ready")
            .unwrap()
            .clone();
        s[flag] = json!(true);
        let w = World::new(&s).await;
        assert_eq!(
            w.post("/api/models/load", r#"{"name":"m"}"#, MEMBER)
                .await
                .0,
            403
        );
        assert_eq!(w.post("/api/models/load", "{", ADMIN).await.0, 400);
        assert_eq!(
            w.post("/api/models/load", r#"{"name":"m"}"#, ADMIN).await.0,
            503,
            "{flag}"
        );
        assert!(w.router.lock().unwrap().iter().all(|r| r.method == "GET"));
    }
}

#[tokio::test]
async fn restored_file_waits_for_scan_invalidation_acknowledgement() {
    let o = oracle();
    let mut s = o["scenarios"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["name"] == "load ready")
        .unwrap()
        .clone();
    s["restored"] = json!(true);
    let w = World::new(&s).await;
    assert_eq!(
        w.post("/api/models/load", r#"{"name":"m"}"#, ADMIN).await.0,
        200
    );
}

#[tokio::test]
async fn a_fake_gate_health_or_token_is_not_a_settlement_capability() {
    use noevia_server::{model_load_capability::verified, model_maint::router::Router};
    let (port, seen) = engine(Arc::new(|_| {
        Answer::Reply(200, json!({"ok":true}).to_string())
    }))
    .await;
    let router = Router::new(&format!("http://127.0.0.1:{port}"), None).unwrap();
    let admin = || {
        Some((
            reqwest::header::HeaderName::from_static("x-noevia-gate-admin"),
            reqwest::header::HeaderValue::from_static("synthetic-token"),
        ))
    };
    assert!(!verified(&router, None).await);
    assert!(!verified(&router, admin()).await);
    assert_eq!(
        seen.lock().unwrap().last().unwrap().target,
        "/gate/v1/capabilities"
    );
    let (port, _) = engine(Arc::new(|_| Answer::Reply(200, json!({"protocol":"noevia-gate-capabilities/1", "capabilities":["noevia-model-load-settlement/1"]}).to_string()))).await;
    let router = Router::new(&format!("http://127.0.0.1:{port}"), None).unwrap();
    assert!(verified(&router, admin()).await);
}

#[tokio::test]
async fn capability_startup_and_reconnect_never_reuse_an_old_gate_proof() {
    use noevia_server::{model_load_capability::Capability, model_maint::router::Router};
    use std::sync::atomic::{AtomicBool, Ordering};
    let capable = Arc::new(AtomicBool::new(true));
    let answers = Arc::clone(&capable);
    let (port, seen) = engine(Arc::new(move |_| Answer::Reply(200,
        if answers.load(Ordering::SeqCst) {
            json!({"protocol":"noevia-gate-capabilities/1", "capabilities":["noevia-model-load-settlement/1"]})
        } else { json!({"ok":true}) }.to_string()))).await;
    let router = Router::new(&format!("http://127.0.0.1:{port}"), None).unwrap();
    let capability = Capability::start(
        Some(router),
        Some((
            reqwest::header::HeaderName::from_static("x-noevia-gate-admin"),
            reqwest::header::HeaderValue::from_static("synthetic-token"),
        )),
    );
    tokio::time::timeout(Duration::from_secs(3), async {
        while !capability.is_verified() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(seen.lock().unwrap()[0].target, "/gate/v1/capabilities");
    capable.store(false, Ordering::SeqCst);
    assert!(!capability.refresh().await);
    assert!(!capability.is_verified());
    capable.store(true, Ordering::SeqCst);
    assert!(capability.refresh().await);
    assert!(capability.is_verified());
    assert_eq!(seen.lock().unwrap().len(), 3);
}

#[tokio::test]
async fn application_probes_at_startup_and_before_the_next_mutation() {
    let w = World::new(&json!({"path":"/api/models/load", "admin":true})).await;
    let probes = || {
        w.router
            .lock()
            .unwrap()
            .iter()
            .filter(|r| r.target == "/gate/v1/capabilities")
            .count()
    };
    tokio::time::timeout(Duration::from_secs(3), async {
        while probes() == 0 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("application startup must probe the configured engine");
    let initial = probes();
    assert_eq!(
        w.post("/api/models/load", r#"{"name":"m"}"#, ADMIN).await.0,
        200
    );
    assert!(
        probes() > initial,
        "next mutation must refresh after possible reconnect"
    );
}

#[tokio::test]
async fn an_old_startup_reply_cannot_restore_proof_after_a_newer_failed_probe() {
    use noevia_server::{model_load_capability::Capability, model_maint::router::Router};
    use std::sync::atomic::{AtomicUsize, Ordering};
    let calls = Arc::new(AtomicUsize::new(0));
    let count = Arc::clone(&calls);
    let (port, _) = engine(Arc::new(move |_| {
        if count.fetch_add(1, Ordering::SeqCst) == 0 {
            Answer::After(Duration::from_millis(200), 200, json!({"protocol":"noevia-gate-capabilities/1", "capabilities":["noevia-model-load-settlement/1"]}).to_string())
        } else { Answer::Reply(200, json!({"ok":true}).to_string()) }
    })).await;
    let router = Router::new(&format!("http://127.0.0.1:{port}"), None).unwrap();
    let capability = Capability::start(
        Some(router),
        Some((
            reqwest::header::HeaderName::from_static("x-noevia-gate-admin"),
            reqwest::header::HeaderValue::from_static("synthetic-token"),
        )),
    );
    tokio::time::timeout(Duration::from_secs(3), async {
        while calls.load(Ordering::SeqCst) == 0 {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    assert!(!capability.refresh().await);
    tokio::time::sleep(Duration::from_millis(250)).await;
    assert!(!capability.is_verified());
}

#[tokio::test]
async fn no_gate_renewal_loss_cannot_admit_a_second_load_before_settlement() {
    let w = Arc::new(
        World::new(&json!({"path":"/api/models/load", "longLoad":true,"loseRenewal":true})).await,
    );
    let first = Arc::clone(&w);
    let pending = tokio::spawn(async move {
        first
            .post("/api/models/load", r#"{"name":"m"}"#, ADMIN)
            .await
    });
    tokio::time::sleep(Duration::from_millis(300)).await;
    let second = w.post("/api/models/load", r#"{"name":"m"}"#, ADMIN).await;
    assert_eq!(
        second.0, 503,
        "lost renewal must not admit another operation while actual engine work runs"
    );
    assert_eq!(
        w.router
            .lock()
            .unwrap()
            .iter()
            .filter(|r| r.target == "/models/load" && r.method == "POST")
            .count(),
        1
    );
    assert_eq!(pending.await.unwrap().0, 503);
    tokio::time::sleep(Duration::from_millis(1600)).await;
    let after = w.post("/api/models/load", r#"{"name":"m"}"#, ADMIN).await;
    // It reached the engine after covered reconciliation, rather than remaining incident-blocked.
    assert!(
        w.router
            .lock()
            .unwrap()
            .iter()
            .filter(|r| r.target == "/models/load" && r.method == "POST")
            .count()
            >= 2,
        "{after:?}"
    );
    assert!(w
        .node
        .lock()
        .unwrap()
        .iter()
        .any(|r| r.method == "DELETE" && r.target.contains("/recovery/")));
}

#[tokio::test]
async fn no_gate_node_restart_rebuilds_the_incident_before_new_admission() {
    let w = Arc::new(World::new(&json!({"path":"/api/models/load", "longLoad":true})).await);
    let first = Arc::clone(&w);
    let pending = tokio::spawn(async move {
        first
            .post("/api/models/load", r#"{"name":"m"}"#, ADMIN)
            .await
    });
    tokio::time::sleep(Duration::from_millis(200)).await;
    // Model Core's startup handshake: volatile latches disappear, then the same J authority
    // reconstructs front_held before the replacement process accepts ordinary requests.
    let session = w
        .maint
        .authority
        .node_session("synthetic-replacement", 2)
        .unwrap();
    *w.barrier.lock().unwrap() = session.front_held;
    assert_eq!(
        w.post("/api/models/load", r#"{"name":"m"}"#, ADMIN).await.0,
        503
    );
    assert_eq!(
        w.router
            .lock()
            .unwrap()
            .iter()
            .filter(|r| r.target == "/models/load" && r.method == "POST")
            .count(),
        1
    );
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert!(*w.barrier.lock().unwrap());
    assert_eq!(pending.await.unwrap().0, 503);
}

#[tokio::test]
async fn no_gate_unload_caller_drop_keeps_exclusion_after_http_ack_until_engine_settles() {
    let w = Arc::new(World::new(&json!({"path":"/api/models/unload", "longUnload":true})).await);
    let first = Arc::clone(&w);
    let pending = tokio::spawn(async move {
        first
            .post("/api/models/unload", r#"{"name":"m"}"#, ADMIN)
            .await
    });
    tokio::time::sleep(Duration::from_millis(200)).await;
    pending.abort();
    tokio::time::sleep(Duration::from_millis(500)).await;
    // The router's HTTP acknowledgement has returned, but independently timed work still runs.
    assert_eq!(
        w.post("/api/models/load", r#"{"name":"m"}"#, ADMIN).await.0,
        503
    );
    assert!(w
        .router
        .lock()
        .unwrap()
        .iter()
        .all(|r| r.target != "/models/load"));
    tokio::time::sleep(Duration::from_millis(1100)).await;
    let _ = w.post("/api/models/load", r#"{"name":"m"}"#, ADMIN).await;
    assert!(w
        .router
        .lock()
        .unwrap()
        .iter()
        .any(|r| r.target == "/models/load"));
}

#[tokio::test]
async fn renewal_loss_during_http_post_keeps_its_future_and_exclusion_until_settled() {
    let w = Arc::new(World::new(&json!({"path":"/api/models/load", "longLoad":true,"loseRenewal":true,"delayLoadAck":true})).await);
    let first = Arc::clone(&w);
    let pending = tokio::spawn(async move {
        first
            .post("/api/models/load", r#"{"name":"m"}"#, ADMIN)
            .await
    });
    tokio::time::sleep(Duration::from_millis(300)).await;
    let second = w.post("/api/models/load", r#"{"name":"m"}"#, ADMIN).await;
    assert_eq!(
        second.0, 503,
        "lost renewal must not admit another operation while actual engine work runs"
    );
    assert_eq!(
        w.router
            .lock()
            .unwrap()
            .iter()
            .filter(|r| r.target == "/models/load" && r.method == "POST")
            .count(),
        1
    );
    assert_eq!(pending.await.unwrap().0, 503);
    tokio::time::sleep(Duration::from_millis(1600)).await;
    let after = w.post("/api/models/load", r#"{"name":"m"}"#, ADMIN).await;
    // It reached the engine after covered reconciliation, rather than remaining incident-blocked.
    assert!(
        w.router
            .lock()
            .unwrap()
            .iter()
            .filter(|r| r.target == "/models/load" && r.method == "POST")
            .count()
            >= 2,
        "{after:?}"
    );
    assert!(w
        .node
        .lock()
        .unwrap()
        .iter()
        .any(|r| r.method == "DELETE" && r.target.contains("/recovery/")));
}

#[tokio::test]
async fn verified_gate_renewal_loss_still_requires_covered_incident_completion() {
    let w = Arc::new(World::new(&json!({"path":"/api/models/load", "longLoad":true,"loseRenewal":true,"admin":true,"verifiedGate":true})).await);
    let first = Arc::clone(&w);
    let pending = tokio::spawn(async move {
        first
            .post("/api/models/load", r#"{"name":"m"}"#, ADMIN)
            .await
    });
    tokio::time::sleep(Duration::from_millis(300)).await;
    let second = w.post("/api/models/load", r#"{"name":"m"}"#, ADMIN).await;
    assert_eq!(
        second.0, 503,
        "lost renewal must not admit another operation while actual engine work runs"
    );
    assert_eq!(
        w.router
            .lock()
            .unwrap()
            .iter()
            .filter(|r| r.target == "/models/load" && r.method == "POST")
            .count(),
        1
    );
    assert_eq!(pending.await.unwrap().0, 503);
    tokio::time::sleep(Duration::from_millis(1600)).await;
    let after = w.post("/api/models/load", r#"{"name":"m"}"#, ADMIN).await;
    // It reached the engine after covered reconciliation, rather than remaining incident-blocked.
    assert!(
        w.router
            .lock()
            .unwrap()
            .iter()
            .filter(|r| r.target == "/models/load" && r.method == "POST")
            .count()
            >= 2,
        "{after:?}"
    );
    assert!(w
        .node
        .lock()
        .unwrap()
        .iter()
        .any(|r| r.method == "DELETE" && r.target.contains("/recovery/")));
}
