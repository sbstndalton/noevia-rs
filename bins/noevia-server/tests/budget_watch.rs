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

    fn files(&self, step: &Value) {
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
async fn another_jobs_maintenance_hold_neither_blocks_the_unload_nor_lets_a_baseline_be_taken() {
    let d = deployment().await;
    std::fs::write(d.gtt(), "0").unwrap();
    std::fs::write(d.vram(), "0").unwrap();
    let env = d.env(false, true, &serde_json::Map::new());
    let maint = d.maint(&env);
    let watch = d.watch(&env, Arc::clone(&maint));
    // A job of this front (a calibration, say) takes the maintenance lease from Core.
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

#[tokio::test]
async fn when_core_does_not_answer_the_last_budget_stands_and_no_baseline_is_taken() {
    let d = deployment().await;
    std::fs::write(d.gtt(), (20u64 << 30).to_string()).unwrap();
    std::fs::write(d.vram(), "0").unwrap();
    let env = d.env(false, false, &serde_json::Map::new());
    let maint = d.maint(&env);
    let watch = d.watch(&env, maint);
    *d.world.step.lock().unwrap() = over();
    *d.world.node_down.lock().unwrap() = true;
    assert_eq!(
        watch.tick().await,
        Some(Status::NoBudget),
        "no budget has been read yet"
    );
    *d.world.node_down.lock().unwrap() = false;
    assert_eq!(watch.tick().await.unwrap().state(), "over");
    // Core restarts: the 12 GiB it last answered still holds and the overload is still ended.
    *d.world.node_down.lock().unwrap() = true;
    assert_eq!(watch.tick().await.unwrap().state(), "unloaded");
    let (_, unloads, quarantined, logs) = d.world.take();
    assert_eq!(unloads.len(), 1);
    assert!(quarantined.is_empty(), "Core was not there to record it");
    assert_eq!(logs.len(), 1);
    // And while Core cannot say whether the gate is held, an idle reading is no baseline.
    let mut idle = over();
    idle["listing"]["body"]["data"] = json!([]);
    *d.world.step.lock().unwrap() = idle;
    std::fs::write(d.gtt(), (1u64 << 30).to_string()).unwrap();
    assert_eq!(
        as_node(&watch.tick().await)["baselineGib"].as_f64(),
        Some(0.0)
    );
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
    assert!(noevia_server::FEATURES.contains(&"rust-budget-watch"));
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
