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
const LEASE_ID: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

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
}

type Script = Arc<Mutex<HashMap<String, Vec<Value>>>>;

impl World {
    async fn new(s: &Value) -> World {
        let dir = tempfile::tempdir().unwrap();
        let _db = seed_database(dir.path());
        let ini = dir.path().join("models.ini");
        std::fs::write(&ini, "[m]\nmodel=/models/m.gguf\nctx-size=4096\n").unwrap();
        let missing = s.get("missingFile") == Some(&Value::Bool(true));
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
                script.entry("GET /models".into()).or_default().insert(0, f);
            }
        }
        let script: Script = Arc::new(Mutex::new(script));
        let (port, router) = engine(Arc::new(move |req: &Req| {
            let key = format!("{} {}", req.method, req.target.split('?').next().unwrap());
            if missing && key == "GET /models" {
                return Answer::Reply(
                    200,
                    json!({"data":[{"id":"m","source":"preset","status":{"value":"unloaded"}}]})
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
        let lease = s.get("lease").cloned();
        let (node_port, node) = engine(Arc::new(move |req: &Req| {
            if req.headers.get("x-noevia-store-token").map(String::as_str) != Some(SECRET) {
                return Answer::Reply(404, "{}".into());
            }
            match (req.method.as_str(), req.target.as_str()) {
                ("DELETE", _) => Answer::Reply(204, String::new()),
                (_, t) if t.ends_with("/renew") => Answer::Reply(200, "{}".into()),
                ("POST", "/internal/inference-admission/lease") => match &lease {
                    Some(l) => {
                        Answer::Reply(l["status"].as_u64().unwrap() as u16, l["body"].to_string())
                    }
                    None => {
                        let ask: Value = serde_json::from_str(&req.body).unwrap();
                        let headers = if ask.get("embedding").is_some() {
                            json!(null)
                        } else {
                            json!({"x-noevia-lease":"syntheticLease1"})
                        };
                        let mut b = json!({"lease":LEASE_ID,"ttlMs":30000});
                        if !headers.is_null() {
                            b["headers"] = headers;
                        }
                        Answer::Reply(201, b.to_string())
                    }
                },
                ("POST", "/internal/models/facts") => {
                    let scan = if missing {
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
                    Answer::Reply(200, r#"{"ok":true}"#.into())
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
        let (addr, stop) = start(dir.path(), &env).await;
        World {
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
                .filter(|r| r.target == "/internal/inference-admission/lease" && r.method == "POST")
                .count();
            let released = node.iter().filter(|r| r.method == "DELETE").count();
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
        if calls != want_calls {
            failures.push(format!(
                "{}: engine calls {:?}, want {:?}",
                s["name"], calls, want_calls
            ));
        }
        w.leases_settled().await;
        let node = w.node.lock().unwrap().clone();
        let granted = node
            .iter()
            .filter(|r| r.target == "/internal/inference-admission/lease" && r.method == "POST")
            .count();
        let released = node.iter().filter(|r| r.method == "DELETE").count();
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
        .filter(|r| r.target == "/internal/inference-admission/lease" && r.method == "POST")
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
        .filter(|r| r.target == "/internal/inference-admission/lease" && r.method == "POST")
        .map(|r| serde_json::from_str(&r.body).unwrap())
        .collect();
    assert_eq!(asks, vec![json!({"model":"m","embedding":true})]);
    assert_eq!(node.iter().filter(|r| r.method == "DELETE").count(), 1);
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
async fn a_caller_who_leaves_mid_load_gives_the_lease_back() {
    let o = oracle();
    let mut s = o["scenarios"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["name"] == "load ready")
        .unwrap()
        .clone();
    // The engine never becomes ready while the caller waits.
    s["router"] = json!({"GET /models":[{"rows":"unloaded"},{"rows":"unloaded"},{"rows":"loading"}],"POST /models/load":[{"status":200,"body":{}}]});
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
    let end = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let released = w
            .node
            .lock()
            .unwrap()
            .iter()
            .filter(|r| r.method == "DELETE")
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
