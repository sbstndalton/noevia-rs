//! The native memory watchdog (`model_maint::budget_watch`, #697) against Core's real one: every scenario of
//! core `tests/fixtures/budget-watch-node.v1.json` (recorded by core `inference-budget-watch-oracle.test.cjs`
//! from `inference-budget-watch.cjs` wired as index.cjs wired it) is replayed here tick for tick through the
//! production readings ([`NativeIo`]: the router through the gate, the model manager, sysfs files, Core's
//! loopback bridge) against synthetic servers. Each tick's answer, the requests it made, the unloads (body,
//! admin header, key), the quarantine hand-offs and the log line must be Core's, byte for byte.
//!
//! Then what only the native side has: lease contention (another job's maintenance hold neither blocks the
//! unload nor lets a baseline be taken, and the watch never asks for a lease), Core not answering (the last
//! budget stands), the production timer stopping at shutdown, and which builds start it at all.
//!
//! Synthetic servers and temp files only; no engine, no model.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
use noevia_server::model_maint::budget_watch::{self, NativeIo, Settings, Status, Watch};
use noevia_server::model_maint::Maint;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const SECRET: &str = "synthetic-front-secret";
const FIXTURE: &str = include_str!("fixtures/budget-watch-node.v1.json");

// ── A small HTTP/1.1 server whose answers the test sets ─────────────────────────

#[derive(Debug, Clone)]
struct Req {
    method: String,
    path: String,
    headers: HashMap<String, String>,
    body: String,
}

#[derive(Debug, Clone)]
enum Answer {
    Text(u16, String),
    /// Close the connection without an answer (a request that does not settle).
    Drop,
    Delay(u64),
}

type Handler = Arc<dyn Fn(&Req) -> Answer + Send + Sync>;

async fn serve(handler: Handler) -> u16 {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        loop {
            let Ok((mut sock, _)) = listener.accept().await else {
                return;
            };
            let handler = Arc::clone(&handler);
            tokio::spawn(async move {
                let mut buf = Vec::new();
                let mut chunk = [0u8; 4096];
                let head_end = loop {
                    if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                        break i;
                    }
                    match sock.read(&mut chunk).await {
                        Ok(0) | Err(_) => return,
                        Ok(n) => buf.extend_from_slice(&chunk[..n]),
                    }
                };
                let head = String::from_utf8_lossy(&buf[..head_end]).into_owned();
                let mut lines = head.split("\r\n");
                let mut first = lines.next().unwrap_or("").split(' ');
                let method = first.next().unwrap_or("").to_owned();
                let target = first.next().unwrap_or("").to_owned();
                let headers: HashMap<String, String> = lines
                    .filter_map(|l| l.split_once(':'))
                    .map(|(k, v)| (k.trim().to_ascii_lowercase(), v.trim().to_owned()))
                    .collect();
                let len: usize = headers
                    .get("content-length")
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(0);
                let mut body = buf[head_end + 4..].to_vec();
                while body.len() < len {
                    match sock.read(&mut chunk).await {
                        Ok(0) | Err(_) => return,
                        Ok(n) => body.extend_from_slice(&chunk[..n]),
                    }
                }
                let req = Req {
                    method,
                    path: target.split('?').next().unwrap_or("").to_owned(),
                    headers,
                    body: String::from_utf8_lossy(&body).into_owned(),
                };
                match handler(&req) {
                    Answer::Delay(ms) => {
                        tokio::time::sleep(Duration::from_millis(ms)).await;
                    }
                    Answer::Drop => drop(sock),
                    Answer::Text(status, text) => {
                        let out = format!(
                            "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{text}",
                            text.len()
                        );
                        let _ = sock.write_all(out.as_bytes()).await;
                        let _ = sock.shutdown().await;
                    }
                }
            });
        }
    });
    port
}

/// A fixture reply spec: `{status, body}`, `{status, text}` or `{drop: true}`.
fn answer(spec: Option<&Value>) -> Answer {
    let Some(spec) = spec.filter(|s| !s.is_null()) else {
        return Answer::Text(404, "{}".into());
    };
    if spec.get("drop").and_then(Value::as_bool) == Some(true) {
        return Answer::Drop;
    }
    let status = spec["status"].as_u64().unwrap() as u16;
    match spec.get("text").and_then(Value::as_str) {
        Some(t) => Answer::Text(status, t.to_owned()),
        None => Answer::Text(status, spec["body"].to_string()),
    }
}

// ── The synthetic deployment ────────────────────────────────────────────────────

#[derive(Default)]
struct World {
    step: Mutex<Value>,
    requests: Mutex<Vec<String>>,
    unloads: Mutex<Vec<Value>>,
    quarantined: Mutex<Vec<Value>>,
    logs: Mutex<Vec<String>>,
    clock: Mutex<f64>,
    node_down: Mutex<bool>,
    leases: Mutex<Vec<String>>,
    quarantine_reply: Mutex<Option<Answer>>,
    quarantine_attempts: Mutex<usize>,
    unload_delay: Mutex<bool>,
    accepted: tokio::sync::Notify,
}

impl World {
    fn take(&self) -> (Vec<String>, Vec<Value>, Vec<Value>, Vec<String>) {
        let mut r = std::mem::take(&mut *self.requests.lock().unwrap());
        r.sort();
        (
            r,
            std::mem::take(&mut *self.unloads.lock().unwrap()),
            std::mem::take(&mut *self.quarantined.lock().unwrap()),
            std::mem::take(&mut *self.logs.lock().unwrap()),
        )
    }
}

struct Deployment {
    world: Arc<World>,
    router: u16,
    loader: u16,
    node: u16,
    dir: tempfile::TempDir,
}

async fn deployment() -> Deployment {
    let world = Arc::new(World::default());
    let w = Arc::clone(&world);
    let router = serve(Arc::new(move |req: &Req| {
        if req.path.starts_with("/gate/") {
            return Answer::Text(404, "{}".into());
        }
        w.requests
            .lock()
            .unwrap()
            .push(format!("{} {}", req.method, req.path));
        let step = w.step.lock().unwrap().clone();
        match (req.method.as_str(), req.path.as_str()) {
            ("GET", "/models") => answer(step.get("listing")),
            ("POST", "/models/unload") => {
                let body: Value = serde_json::from_str(&req.body).unwrap_or(Value::Null);
                w.unloads.lock().unwrap().push(json!({
                    "body": body,
                    "admin": req.headers.get("x-noevia-gate-admin"),
                    "authorization": req.headers.get("authorization"),
                }));
                w.accepted.notify_one();
                if *w.unload_delay.lock().unwrap() {
                    return Answer::Delay(65_000);
                }
                let model = body.get("model").and_then(Value::as_str).unwrap_or("");
                match step.get("unload").and_then(|u| u.get(model)) {
                    Some(spec) => answer(Some(spec)),
                    None => Answer::Text(200, r#"{"success":true}"#.into()),
                }
            }
            _ => Answer::Text(404, "{}".into()),
        }
    }))
    .await;
    let w = Arc::clone(&world);
    let loader = serve(Arc::new(move |req: &Req| {
        let token = req
            .headers
            .get("x-model-loader-token")
            .cloned()
            .unwrap_or_default();
        w.requests
            .lock()
            .unwrap()
            .push(format!("{} {} token={token}", req.method, req.path));
        if req.method == "GET" && req.path == "/api/v1/backends" {
            return answer(w.step.lock().unwrap().get("backends"));
        }
        Answer::Text(404, "{}".into())
    }))
    .await;
    let w = Arc::clone(&world);
    // Core: the facts call, the budget watch bridge (rust-model-facts.cjs, rust-budget-watch.cjs) and the
    // maintenance lease, each only for the front's secret.
    let node = serve(Arc::new(move |req: &Req| {
        if req.headers.get("x-noevia-store-token").map(String::as_str) != Some(SECRET) || req.method != "POST" {
            return Answer::Text(404, r#"{"error":"Not found"}"#.into());
        }
        if req.path.starts_with("/internal/model-maintenance/lease") {
            if req.path == "/internal/model-maintenance/lease" {
                w.leases.lock().unwrap().push("acquire".into());
                return Answer::Text(
                    201,
                    json!({ "lease": "0123456789abcdef0123456789abcdef", "ttlMs": 60000, "renewMs": 20000, "hardMs": 3_600_000 }).to_string(),
                );
            }
            w.leases.lock().unwrap().push(req.path.clone());
            return Answer::Text(200, r#"{"ok":true}"#.into());
        }
        if *w.node_down.lock().unwrap() {
            return Answer::Text(503, r#"{"error":"unavailable"}"#.into());
        }
        let body: Value = serde_json::from_str(&req.body).unwrap_or(Value::Null);
        let step = w.step.lock().unwrap().clone();
        match (req.path.as_str(), body["want"].as_str()) {
            ("/internal/models/facts", Some("budget")) => {
                Answer::Text(200, json!({ "budget": { "budgetGib": step["budget"], "source": "admin" } }).to_string())
            }
            ("/internal/budget-watch", Some("held")) => Answer::Text(200, json!({ "held": step["held"] }).to_string()),
            ("/internal/budget-watch", Some("quarantine")) => {
                *w.quarantine_attempts.lock().unwrap() += 1;
                if let Some(answer) = w.quarantine_reply.lock().unwrap().clone() { return answer; }
                let invalidated = step["budget"].as_f64() != body["budgetGib"].as_f64();
                if invalidated { return Answer::Text(200, r#"{"ok":true,"invalidated":true}"#.into()); }
                w.quarantined.lock().unwrap().push(json!([body["model"], body["budgetGib"]]));
                Answer::Text(200, r#"{"ok":true}"#.into())
            }
            _ => Answer::Text(400, r#"{"error":"A known fact is required."}"#.into()),
        }
    }))
    .await;
    Deployment {
        world,
        router,
        loader,
        node,
        dir: tempfile::tempdir().unwrap(),
    }
}

impl Deployment {
    fn gtt(&self) -> std::path::PathBuf {
        self.dir.path().join("mem_info_gtt_used")
    }
    fn vram(&self) -> std::path::PathBuf {
        self.dir.path().join("mem_info_vram_used")
    }

    fn env(
        &self,
        admin: bool,
        loader: bool,
        extra: &serde_json::Map<String, Value>,
    ) -> HashMap<String, String> {
        let mut env: HashMap<String, String> = [
            ("MODEL_MANAGER_KIND", "llamacpp".to_owned()),
            (
                "MODEL_MANAGER_BASE_URL",
                format!("http://127.0.0.1:{}/v1", self.router),
            ),
            ("MODEL_MANAGER_API_KEY", "synthetic-key".to_owned()),
            ("UI_DATA_DIR", self.dir.path().display().to_string()),
            (
                "NOEVIA_START_ID",
                "0123456789abcdef0123456789abcdef".to_owned(),
            ),
            (
                "NOEVIA_LEGACY_UPSTREAM",
                format!("http://127.0.0.1:{}", self.node),
            ),
            ("NOEVIA_PROJECT_STORE_TOKEN", SECRET.to_owned()),
            ("INFERENCE_GTT_USED_PATH", self.gtt().display().to_string()),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_owned(), v))
        .collect();
        if admin {
            env.insert(
                "INFERENCE_GATE_ADMIN_TOKEN".into(),
                "synthetic-admin".into(),
            );
        }
        if loader {
            env.insert(
                "MODEL_LOADER_URL".into(),
                format!("http://127.0.0.1:{}/", self.loader),
            );
            env.insert("MODEL_LOADER_TOKEN".into(), "synthetic-loader-token".into());
        }
        for (k, v) in extra {
            env.insert(k.clone(), v.as_str().unwrap().to_owned());
        }
        self.snapshot_budget(12.0);
        env
    }

    fn maint(&self, env: &HashMap<String, String>) -> Arc<Maint> {
        let get = |k: &str| env.get(k).cloned();
        let engine = Arc::new(server_stats::Engine::from_lookup(&get).unwrap());
        let sizing = Arc::new(noevia_server::model_sizing::Sizing::from_lookup(
            engine.kind(),
            Arc::clone(&engine),
            &get,
        ));
        Arc::new(Maint::from_lookup(engine.kind(), &engine, sizing, &get).unwrap())
    }

    /// The production watch over this deployment, with the test's log sink and clock.
    fn watch(&self, env: &HashMap<String, String>, maint: Arc<Maint>) -> Watch<NativeIo> {
        let get = |k: &str| env.get(k).cloned();
        let w = Arc::clone(&self.world);
        let c = Arc::clone(&self.world);
        let io = NativeIo::from_lookup(maint, &get, Path::new("/nonexistent-drm"))
            .unwrap()
            .expect("llama.cpp with a router")
            .with_log(Arc::new(move |line: &str| {
                w.logs.lock().unwrap().push(line.to_owned())
            }))
            .with_clock(Arc::new(move || *c.clock.lock().unwrap()));
        Watch::new(Settings::from_lookup(&get), io)
    }

    fn snapshot_budget(&self, budget: f64) {
        std::fs::write(self.dir.path().join("model-facts.json"),json!({"protocol":"model-facts/1","start":"0123456789abcdef0123456789abcdef","generation":1,"at":noevia_server::identity::now_ms(),"budget":{"budgetGib":budget}}).to_string()).unwrap();
    }
    fn files(&self, step: &Value) {
        if let Some(budget) = step["budget"].as_f64() {
            self.snapshot_budget(budget);
        }
        for (path, key) in [(self.gtt(), "gtt"), (self.vram(), "vram")] {
            match step[key].as_str() {
                Some(text) => std::fs::write(&path, text).unwrap(),
                None => {
                    let _ = std::fs::remove_file(&path);
                }
            }
        }
    }
}

/// `JSON.parse(JSON.stringify(status))`: numbers as JS writes them.
fn as_node(status: &Option<Status>) -> Value {
    match status {
        Some(s) => serde_json::from_str(&js_json::stringify(&s.to_json()).unwrap()).unwrap(),
        None => Value::Null,
    }
}

#[tokio::test]
async fn every_core_scenario_replays_tick_for_tick() {
    let fixture: Value = serde_json::from_str(FIXTURE).unwrap();
    assert_eq!(fixture["version"], 1);
    let scenarios = fixture["scenarios"].as_array().unwrap();
    assert!(scenarios.len() >= 15, "the whole fixture");
    let mut ticks = 0;
    for scenario in scenarios {
        let name = scenario["name"].as_str().unwrap();
        let d = deployment().await;
        // Both counters exist when the watch starts (Core reads gpuMemoryFiles once).
        std::fs::write(d.gtt(), "0").unwrap();
        std::fs::write(d.vram(), "0").unwrap();
        let env = d.env(
            scenario["admin"].as_bool().unwrap(),
            !scenario["noLoader"].as_bool().unwrap(),
            scenario["env"].as_object().unwrap(),
        );
        let maint = d.maint(&env);
        let watch = d.watch(&env, maint);
        *d.world.clock.lock().unwrap() = 1000.0;
        let steps = scenario["steps"].as_array().unwrap();
        let expect = scenario["expect"].as_array().unwrap();
        assert_eq!(steps.len(), expect.len(), "{name}");
        for (i, (step, want)) in steps.iter().zip(expect).enumerate() {
            *d.world.clock.lock().unwrap() += step["advanceMs"].as_f64().unwrap();
            *d.world.step.lock().unwrap() = step.clone();
            d.files(step);
            d.world.take();
            let result = watch.tick().await;
            let (requests, unloads, quarantined, logs) = d.world.take();
            let at = format!("{name} / tick {i}");
            assert_eq!(as_node(&result), want["result"], "{at}: result");
            let want_requests: Vec<String> = want["requests"]
                .as_array()
                .unwrap()
                .iter()
                .map(|r| r.as_str().unwrap().to_owned())
                .collect();
            assert_eq!(requests, want_requests, "{at}: requests");
            assert_eq!(Value::Array(unloads), want["unloads"], "{at}: unloads");
            assert_eq!(
                Value::Array(quarantined),
                want["quarantined"],
                "{at}: quarantine"
            );
            let want_logs: Vec<String> = want["log"]
                .as_array()
                .unwrap()
                .iter()
                .map(|r| r.as_str().unwrap().to_owned())
                .collect();
            assert_eq!(logs, want_logs, "{at}: log");
            ticks += 1;
        }
    }
    assert!(ticks >= 50, "{ticks} ticks replayed");
}

/// 20 GiB of GTT with `chat` loaded: over a 12 GiB budget.
fn over() -> Value {
    json!({ "budget": 12, "held": false, "listing": { "status": 200, "body": { "data": [{ "id": "chat", "status": { "value": "loaded" } }] } },
            "backends": { "status": 200, "body": { "backends": [] } }, "unload": {} })
}

#[tokio::test]
async fn deliberate_node_emergency_preemption_parity_does_not_wait_for_another_maintenance_owner() {
    let d = deployment().await;
    std::fs::write(d.gtt(), "0").unwrap();
    std::fs::write(d.vram(), "0").unwrap();
    let env = d.env(false, true, &serde_json::Map::new());
    let maint = d.maint(&env);
    let watch = d.watch(&env, Arc::clone(&maint));
    // Deliberate inherited Node emergencyUnload parity, not a claim of coordinated preemption.
    // Coordinated preemption: https://github.com/sbstndalton/noevia/issues/2356.
    // A job of this front takes the maintenance lease from Core.
    let hold = maint
        .acquire("synthetic calibration", Duration::from_secs(600), false)
        .await
        .expect("lease granted");
    assert_eq!(*d.world.leases.lock().unwrap(), vec!["acquire".to_owned()]);
    // Nothing loaded, 11 GiB on the device: the job's memory, not an idle baseline (Core says not held here).
    let mut idle = over();
    idle["listing"]["body"]["data"][0]["status"]["value"] = json!("unloaded");
    *d.world.step.lock().unwrap() = idle.clone();
    std::fs::write(d.gtt(), (11u64 << 30).to_string()).unwrap();
    let r = watch.tick().await.unwrap();
    assert_eq!(
        (r.state(), as_node(&Some(r.clone()))["baselineGib"].as_f64()),
        ("ok", Some(0.0))
    );
    // Over the budget while the job holds the engine: the watch unloads anyway, through the gate's own
    // scheduling, as Core's emergencyUnload did; it never asks for a lease of its own.
    *d.world.step.lock().unwrap() = over();
    std::fs::write(d.gtt(), (20u64 << 30).to_string()).unwrap();
    watch.tick().await;
    let unloaded = tokio::time::timeout(Duration::from_secs(5), watch.tick())
        .await
        .expect("never waits behind the hold");
    assert_eq!(unloaded.unwrap().state(), "unloaded");
    let (_, unloads, quarantined, _) = d.world.take();
    assert_eq!(unloads.len(), 1);
    assert_eq!(quarantined, vec![json!(["chat", 12])]);
    assert_eq!(
        *d.world.leases.lock().unwrap(),
        vec!["acquire".to_owned()],
        "the watch took no lease"
    );
    hold.release().await;
    // The hold given back: an idle reading is the baseline again.
    *d.world.step.lock().unwrap() = idle;
    std::fs::write(d.gtt(), (1u64 << 30).to_string()).unwrap();
    let r = watch.tick().await;
    assert_eq!(as_node(&r)["baselineGib"].as_f64(), Some(1.0));
}

async fn assert_native_load_refused(maint: &Maint) {
    let result = maint
        .router
        .as_ref()
        .unwrap()
        .request(
            reqwest::Method::POST,
            "/models/load",
            Some(&json!({"model":"chat"})),
            None,
            Duration::from_secs(1),
        )
        .await
        .unwrap();
    assert_eq!(result.status, 409);
    assert_eq!(result.body.unwrap()["code"], "inference_budget_unloaded");
}

#[tokio::test]
async fn quarantine_503_timeout_and_false_remain_fenced_until_acknowledged_then_reload_is_refused()
{
    for fault in [
        Answer::Text(503, "{}".into()),
        Answer::Delay(6000),
        Answer::Text(200, r#"{"ok":false}"#.into()),
    ] {
        let d = deployment().await;
        std::fs::write(d.gtt(), (20u64 << 30).to_string()).unwrap();
        std::fs::write(d.vram(), "0").unwrap();
        let env = d.env(false, false, &serde_json::Map::new());
        let maint = d.maint(&env);
        let watch = d.watch(&env, Arc::clone(&maint));
        *d.world.step.lock().unwrap() = over();
        assert_eq!(watch.tick().await.unwrap().state(), "over");
        *d.world.quarantine_reply.lock().unwrap() = Some(fault);
        let failed = watch.tick().await.unwrap();
        assert_eq!(failed.state(), "over");
        assert_eq!(as_node(&Some(failed))["strikes"], 2);
        assert!(
            d.world.logs.lock().unwrap().is_empty(),
            "no completed-unload log before acknowledgement"
        );
        assert_native_load_refused(&maint).await;
        assert!(d.world.quarantined.lock().unwrap().is_empty());
        let attempts = *d.world.quarantine_attempts.lock().unwrap();
        watch.tick().await;
        assert_eq!(
            *d.world.quarantine_attempts.lock().unwrap(),
            attempts,
            "backoff also applies after a timeout"
        );
        *d.world.quarantine_reply.lock().unwrap() = None;
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(watch.tick().await.unwrap().state(), "unloaded");
        assert_eq!(d.world.quarantined.lock().unwrap().len(), 1);
        assert_eq!(
            d.world.unloads.lock().unwrap().len(),
            1,
            "retry only the quarantine, not the unload"
        );
        assert_native_load_refused(&maint).await;
        // Recorded fences survive process restart and clear only on confirmed invalidation.
        let restarted = d.watch(&env, Arc::clone(&maint));
        assert_native_load_refused(&maint).await;
        let mut changed = over();
        changed["budget"] = json!(24);
        d.snapshot_budget(24.0);
        *d.world.step.lock().unwrap() = changed;
        assert_eq!(restarted.tick().await.unwrap().state(), "ok");
        let reply = maint
            .router
            .as_ref()
            .unwrap()
            .request(
                reqwest::Method::POST,
                "/models/load",
                Some(&json!({"model":"chat"})),
                None,
                Duration::from_secs(1),
            )
            .await
            .unwrap();
        assert_ne!(
            reply.status, 409,
            "new budget invalidates the old obligation"
        );
    }
}

#[tokio::test]
async fn cancellation_after_the_gate_accepted_unload_preserves_durable_quarantine_across_restart() {
    let d = deployment().await;
    std::fs::write(d.gtt(), (20u64 << 30).to_string()).unwrap();
    std::fs::write(d.vram(), "0").unwrap();
    let env = d.env(false, false, &serde_json::Map::new());
    let maint = d.maint(&env);
    let mut native = d.watch(&env, Arc::clone(&maint));
    native.settings.every = Duration::from_millis(10);
    let watch = Arc::new(native);
    *d.world.step.lock().unwrap() = over();
    assert_eq!(watch.tick().await.unwrap().state(), "over");
    *d.world.unload_delay.lock().unwrap() = true;
    let (stop, rx) = tokio::sync::watch::channel(false);
    let task = tokio::spawn(Arc::clone(&watch).run(rx));
    tokio::time::timeout(Duration::from_secs(2), d.world.accepted.notified())
        .await
        .unwrap();
    assert_native_load_refused(&maint).await;
    stop.send(true).unwrap();
    tokio::time::timeout(Duration::from_millis(200), task)
        .await
        .unwrap()
        .unwrap();
    drop(watch);
    assert_native_load_refused(&maint).await;
    // Recreated worker reads the fsynced obligation; it does not need the lost HTTP answer.
    let restarted = d.watch(&env, Arc::clone(&maint));
    let mut idle = over();
    idle["listing"]["body"]["data"] = json!([]);
    *d.world.step.lock().unwrap() = idle;
    restarted.tick().await;
    assert_eq!(d.world.quarantined.lock().unwrap().len(), 1);
    assert_native_load_refused(&maint).await;
}

fn fence_dir(maint: &Maint) -> std::path::PathBuf {
    maint
        .data_dir
        .as_ref()
        .unwrap()
        .join(format!("budget-watch-{}", maint.suffix))
}
fn seed_fence(maint: &Maint, model: &str, budget: f64, recorded: bool) -> std::path::PathBuf {
    let dir = fence_dir(maint);
    std::fs::create_dir_all(&dir).unwrap();
    let revision = maint.ini.as_ref().map(|i| i.snapshot().unwrap().revision);
    let hash = noevia_server::model_maint::ini::sha(model);
    let file = dir.join(format!("{hash}.json"));
    std::fs::write(
        &file,
        json!({"model":model,"revision":revision,"budgetGib":budget,"at":0,"recorded":recorded})
            .to_string(),
    )
    .unwrap();
    std::fs::write(
        dir.join(format!("{hash}.context")),
        json!({"revision":revision,"revisionKnown":true,"budgetGib":budget}).to_string(),
    )
    .unwrap();
    file
}
async fn native_load(maint: &Maint, model: &str) -> u16 {
    maint
        .router
        .as_ref()
        .unwrap()
        .request(
            reqwest::Method::POST,
            "/models/load",
            Some(&json!({"model":model})),
            None,
            Duration::from_secs(1),
        )
        .await
        .unwrap()
        .status
}
fn model_artifacts(maint: &Maint, model: &str) -> Vec<String> {
    let hash = noevia_server::model_maint::ini::sha(model);
    let mut files: Vec<String> = std::fs::read_dir(fence_dir(maint))
        .unwrap()
        .map(|f| f.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|n| n.starts_with(&hash))
        .collect();
    files.sort();
    files
}

#[tokio::test]
async fn damaged_fence_is_isolated_valid_fences_reconcile_and_revision_change_clears_every_artifact(
) {
    let d = deployment().await;
    let ini = d.dir.path().join("models.ini");
    std::fs::write(&ini, "[chat]\nctx-size=1024\n[other]\n[third]\n").unwrap();
    let mut env = d.env(false, false, &serde_json::Map::new());
    env.insert("LLAMACPP_PRESET_PATH".into(), ini.display().to_string());
    let maint = d.maint(&env);
    let watch = d.watch(&env, Arc::clone(&maint));
    let mut idle = over();
    idle["listing"]["body"]["data"] = json!([]);
    *d.world.step.lock().unwrap() = idle;
    let broken = seed_fence(&maint, "chat", 12.0, false);
    std::fs::write(&broken, "broken payload").unwrap();
    seed_fence(&maint, "other", 8.0, false); // stale: must clear even without an IPC ack
    seed_fence(&maint, "third", 12.0, false); // live: must still acknowledge
    watch.tick().await;
    assert_eq!(
        *d.world.quarantined.lock().unwrap(),
        vec![json!(["third", 12])]
    );
    assert!(model_artifacts(&maint, "other").is_empty());
    assert!(!broken.exists());
    let archived = model_artifacts(&maint, "chat");
    assert_eq!(
        archived.iter().filter(|n| n.contains(".damaged-")).count(),
        1
    );
    assert_eq!(native_load(&maint, "chat").await, 409);
    assert_ne!(native_load(&maint, "other").await, 409);
    assert_eq!(native_load(&maint, "third").await, 409);
    let restarted = d.watch(&env, Arc::clone(&maint));
    restarted.tick().await;
    assert_eq!(
        model_artifacts(&maint, "chat"),
        archived,
        "restart does not rename/log the same damage again"
    );
    assert_eq!(native_load(&maint, "chat").await, 409);
    // Admission itself invalidates stale damaged state immediately, without a watch tick.
    std::fs::write(&ini, "[chat]\nctx-size=2048\n[other]\n[third]\n").unwrap();
    assert_ne!(native_load(&maint, "chat").await, 409);
    assert!(
        model_artifacts(&maint, "chat").is_empty(),
        "context and damaged archive both removed"
    );
    assert_ne!(native_load(&maint, "third").await, 409);
    assert!(model_artifacts(&maint, "third").is_empty());
}

#[tokio::test]
async fn legacy_damage_without_context_survives_restart_then_budget_change_invalidates_it_without_bridge_ack(
) {
    let d = deployment().await;
    let env = d.env(false, false, &serde_json::Map::new());
    let maint = d.maint(&env);
    let mut idle = over();
    idle["listing"]["body"]["data"] = json!([]);
    *d.world.step.lock().unwrap() = idle.clone();
    let file = seed_fence(&maint, "chat", 12.0, false);
    std::fs::remove_file(file.with_extension("context")).unwrap();
    std::fs::write(file, "legacy damage").unwrap();
    d.watch(&env, Arc::clone(&maint)).tick().await;
    assert_eq!(native_load(&maint, "chat").await, 409);
    let saved = model_artifacts(&maint, "chat");
    let restarted = d.watch(&env, Arc::clone(&maint));
    restarted.tick().await;
    assert_eq!(model_artifacts(&maint, "chat"), saved);
    *d.world.quarantine_reply.lock().unwrap() = Some(Answer::Text(503, "{}".into()));
    idle["budget"] = json!(24);
    d.snapshot_budget(24.0);
    *d.world.step.lock().unwrap() = idle;
    assert_ne!(native_load(&maint, "chat").await, 409);
    assert!(model_artifacts(&maint, "chat").is_empty());
}

#[tokio::test]
async fn the_production_timer_reads_on_its_interval_and_stops_at_shutdown() {
    let d = deployment().await;
    std::fs::write(d.gtt(), (1u64 << 30).to_string()).unwrap();
    std::fs::write(d.vram(), "0").unwrap();
    let mut extra = serde_json::Map::new();
    extra.insert("INFERENCE_BUDGET_WATCH_INTERVAL_MS".into(), json!("2000"));
    let env = d.env(false, false, &extra);
    *d.world.step.lock().unwrap() = over();
    let maint = d.maint(&env);
    let get = |k: &str| env.get(k).cloned();
    let (stop, rx) = tokio::sync::watch::channel(false);
    let handle = budget_watch::spawn(&maint, &get, rx).expect("started");
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert!(
        d.world.take().0.is_empty(),
        "nothing before the first interval"
    );
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert_eq!(
        d.world.take().0,
        vec!["GET /models".to_owned()],
        "one reading at 2 s"
    );
    stop.send(true).unwrap();
    tokio::time::timeout(Duration::from_secs(1), handle)
        .await
        .expect("stops at once")
        .unwrap();
    tokio::time::sleep(Duration::from_millis(2500)).await;
    assert!(d.world.take().0.is_empty(), "nothing after shutdown");
}

#[tokio::test]
async fn only_a_llamacpp_front_with_the_watch_on_starts_it() {
    assert!(noevia_server::FEATURES.contains(&"rust-budget-watch-v3"));
    let d = deployment().await;
    let (_stop, rx) = tokio::sync::watch::channel(false);
    let on = d.env(false, false, &serde_json::Map::new());
    let maint = d.maint(&on);
    let started = budget_watch::spawn(&maint, &|k: &str| on.get(k).cloned(), rx.clone())
        .expect("on by default");
    started.abort();
    let mut off = on.clone();
    off.insert("INFERENCE_BUDGET_WATCH".into(), "off".into());
    assert!(budget_watch::spawn(&maint, &|k: &str| off.get(k).cloned(), rx.clone()).is_none());
    let mut other = on.clone();
    other.insert("MODEL_MANAGER_KIND".into(), "lemonade".into());
    let other_maint = d.maint(&other);
    assert!(budget_watch::spawn(&other_maint, &|k: &str| other.get(k).cloned(), rx).is_none());
}
