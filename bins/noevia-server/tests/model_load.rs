//! Real HTTP through the native front for `POST /api/models/load` and `/api/models/unload` (sbstndalton/noevia#2291,
//! #545, #548, #697, #2115). Every scenario of tests/fixtures/load-model/load-model-node.json was first run through
//! Core's own route with Core's real llama.cpp manager and inference-gate client over scripted transports
//! (record-load.cjs, Core 76cc868); the front must give the same status and body text (key order included), send the
//! engine the same load and unload requests (the lease, the admin header), ask the gate for the same lease and give
//! it back the same number of times.
//! Two recorded scenarios are Core failures the port fixes (`load-null-body` and `unload-null-body`: a TypeError);
//! their expected answers are pinned below, not recorded.
//! Core's budget decision is a bridge call (scripted here); the recorded fence scenario is Core's own fence file.
//! The router and the gate are scripted; no engine is touched and no model is loaded. Synthetic QA-named data only.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]
mod maint_support;
use maint_support::*;
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};
use std::time::Duration;

const CASES: &str = include_str!("fixtures/load-model/load-model-node.json");
const SECRET: &str = "synthetic-store-token-0123456789abcdef";
const BUDGET_REFUSAL: &str = r#"{"error":"qa-model-a stays unloaded until its settings or inference budget change.","code":"inference_budget_unloaded"}"#;

fn scripted_core(sc: &Value) -> Handler {
    let sidecars = sc.get("env").map_or(json!([]), |e| {
        e.get("EMBEDDING_MODEL").map_or(json!([]), |m| json!([m]))
    });
    let scan: Vec<Value> = sc["models"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|m| m["file"] == true)
        .map(|m| json!({"modelId": m["name"], "sections": [m["name"]]}))
        .collect();
    let installed_fails = sc["installedFails"] == true;
    let fenced = sc["fence"] == true;
    Arc::new(move |req: &Req| {
        let t = req.target.as_str();
        if t == "/internal/models/facts" {
            let ask: Value = serde_json::from_str(&req.body).unwrap_or_default();
            if installed_fails && ask["ids"].as_array().is_some_and(|a| !a.is_empty()) {
                return Answer::Reply(503, "{}".into());
            }
            return Answer::Reply(
                200,
                json!({"sidecarNames": sidecars, "sidecarLoaded": [], "scan": scan}).to_string(),
            );
        }
        if t == "/internal/model-maintenance/load-refusal" {
            return Answer::Reply(
                200,
                if fenced {
                    format!(r#"{{"refusal":{BUDGET_REFUSAL}}}"#)
                } else {
                    r#"{"refusal":null}"#.into()
                },
            );
        }
        if t.starts_with("/internal/") {
            return Answer::Reply(200, r#"{"ok":true}"#.into());
        }
        Answer::Reply(200, "{}".into())
    })
}

fn scripted_reply(list: &Value, n: usize) -> Answer {
    let list = list.as_array().unwrap();
    let r = &list[n.min(list.len() - 1)];
    Answer::Reply(r["status"].as_u64().unwrap() as u16, r["body"].to_string())
}

fn sse(events: &Value) -> String {
    events
        .as_array()
        .unwrap()
        .iter()
        .map(|e| format!("event: {}\ndata: {}\n\n", e[0].as_str().unwrap(), e[1]))
        .collect()
}

/// The router and its gate: one row per model, the scripted lease, load and unload answers.
fn scripted_router(sc: &Value, loads: Arc<Mutex<usize>>, polls: Arc<Mutex<usize>>) -> Handler {
    let sc = sc.clone();
    let unloads = Arc::new(Mutex::new(0usize));
    Arc::new(move |req: &Req| {
        let t = req.target.as_str();
        if t == "/gate/v1/leases" && req.method == "POST" {
            let l = &sc["lease"];
            if l["throws"] == true {
                return Answer::Drop;
            }
            if l.get("sse").is_some() {
                return Answer::Events(Duration::ZERO, sse(&l["sse"]));
            }
            return Answer::Reply(l["status"].as_u64().unwrap() as u16, l["body"].to_string());
        }
        if t.starts_with("/gate/v1/leases/") && req.method == "DELETE" {
            return Answer::Reply(204, String::new());
        }
        if t.starts_with("/gate/v1/") {
            return Answer::Reply(404, "{}".into());
        }
        if req.method == "GET" && (t == "/v1/models" || t == "/models") {
            if sc["listing"] == "fail" {
                return Answer::Reply(500, "{}".into());
            }
            let loaded = *loads.lock().unwrap() > 0;
            if loaded && t == "/models" {
                *polls.lock().unwrap() += 1;
            }
            let n = (*polls.lock().unwrap()).saturating_sub(1);
            let after = sc["after"].as_array().filter(|_| loaded);
            let rows: Vec<Value> = sc["models"]
                .as_array()
                .unwrap()
                .iter()
                .map(|m| {
                    let name = m["name"].as_str().unwrap();
                    let status = match after {
                        Some(a) => a[n.min(a.len() - 1)].as_str().unwrap().to_owned(),
                        None => m["status"].as_str().unwrap().to_owned(),
                    };
                    json!({"id": name, "source": "preset", "status": {"value": status, "args": ["--model", format!("/models/{name}.gguf")]}})
                })
                .collect();
            return Answer::Reply(200, json!({"data": rows}).to_string());
        }
        if req.method == "POST" && t == "/models/load" {
            let mut c = loads.lock().unwrap();
            *c += 1;
            return scripted_reply(&sc["load"], *c - 1);
        }
        if req.method == "POST" && t == "/models/unload" {
            let mut c = unloads.lock().unwrap();
            *c += 1;
            return scripted_reply(&sc["unload"], *c - 1);
        }
        Answer::Reply(404, "{}".into())
    })
}

/// The model manager's sections: which presets have their file (what the installed list's `missingFile` reads).
fn scripted_loader(sc: &Value) -> Handler {
    let rows: Vec<Value> = sc["models"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| json!({"name": m["name"], "hasFile": m["file"]}))
        .collect();
    Arc::new(move |req: &Req| {
        if req.method == "GET" && req.target == "/api/v1/sections" {
            return Answer::Reply(
                200,
                json!({"revision": format!("r{}", rows.len()), "sections": rows}).to_string(),
            );
        }
        Answer::Reply(404, "{}".into())
    })
}

struct Ran {
    status: u16,
    body: String,
    engine: Vec<String>,
    gate: Vec<String>,
}

fn canonical(line: &str) -> String {
    // `POST /models/load {json} lease=yes`: the JSON part compared as JSON (key order is serde's on both sides).
    let (head, rest) = line.split_once(" {").unwrap();
    let (json, tail) = rest.rsplit_once("} ").unwrap();
    let value: Value = serde_json::from_str(&format!("{{{json}}}")).unwrap();
    format!("{head} {value} {tail}")
}

async fn run(case: &Value, cookie: Option<&str>) -> Ran {
    let sc = &case["scenario"];
    let verb = sc["verb"].as_str().unwrap();
    let d = tempfile::tempdir().unwrap();
    let _db = seed_database(d.path());
    let ini = d.path().join("models.ini");
    std::fs::write(&ini, "version = 1\n").unwrap();
    let (loads, polls) = (Arc::new(Mutex::new(0usize)), Arc::new(Mutex::new(0usize)));
    let (core, _core_seen) = engine(scripted_core(sc)).await;
    let (loader, _loader_seen) = engine(scripted_loader(sc)).await;
    let (router, router_seen) =
        engine(scripted_router(sc, Arc::clone(&loads), Arc::clone(&polls))).await;
    let mut env = vec![
        (
            "NOEVIA_LEGACY_UPSTREAM".to_owned(),
            format!("http://127.0.0.1:{core}"),
        ),
        ("NOEVIA_PROJECT_STORE_TOKEN".to_owned(), SECRET.to_owned()),
        (
            "MODEL_LOADER_URL".to_owned(),
            format!("http://127.0.0.1:{loader}"),
        ),
        (
            "NOEVIA_FEATURE_NATIVE_CLIENT_AUTH".to_owned(),
            "1".to_owned(),
        ),
    ];
    if sc["admin"] == true {
        env.push((
            "INFERENCE_GATE_ADMIN_TOKEN".to_owned(),
            "synthetic-admin-token".to_owned(),
        ));
    }
    if let Some(extra) = sc.get("env").and_then(Value::as_object) {
        for (k, v) in extra {
            env.push((k.clone(), v.as_str().unwrap().to_owned()));
        }
    }
    if sc["disabled"] != true {
        env.extend([
            ("MODEL_MANAGER_KIND".to_owned(), "llamacpp".to_owned()),
            (
                "MODEL_MANAGER_BASE_URL".to_owned(),
                format!("http://127.0.0.1:{router}"),
            ),
            ("LLAMACPP_PRESET_PATH".to_owned(), ini.display().to_string()),
        ]);
    }
    let (addr, _stop) = start(d.path(), &env).await;
    let sent = match sc.get("raw") {
        Some(raw) => raw.as_str().unwrap().to_owned(),
        None => sc
            .get("body")
            .cloned()
            .unwrap_or_else(|| json!({"name": sc["name"]}))
            .to_string(),
    };
    let (status, _h, body) = call(
        addr,
        "POST",
        &format!("/api/models/{verb}"),
        cookie,
        sent.as_bytes(),
    )
    .await;
    let seen = router_seen.lock().unwrap().clone();
    let engine_lines = seen
        .iter()
        .filter(|r| r.target == "/models/load" || r.target == "/models/unload")
        .map(|r| {
            let lease = r.headers.contains_key("x-noevia-lease");
            let admin = r.headers.contains_key("x-noevia-gate-admin");
            if r.target == "/models/load" {
                format!(
                    "{} {} {} lease={}",
                    r.method,
                    r.target,
                    r.body,
                    if lease { "yes" } else { "no" }
                )
            } else {
                format!(
                    "{} {} {} admin={}",
                    r.method,
                    r.target,
                    r.body,
                    if admin { "yes" } else { "no" }
                )
            }
        })
        .collect();
    let gate_lines = seen
        .iter()
        .filter(|r| r.target.starts_with("/gate/v1/leases"))
        .map(|r| {
            if r.method == "POST" {
                let m: Value = serde_json::from_str(&r.body).unwrap();
                format!("POST {} model={}", r.target, m["model"].as_str().unwrap())
            } else {
                format!("{} {}", r.method, r.target)
            }
        })
        .collect();
    Ran {
        status,
        body,
        engine: engine_lines,
        gate: gate_lines,
    }
}

fn strings(v: &Value) -> Vec<String> {
    v.as_array()
        .unwrap()
        .iter()
        .map(|l| l.as_str().unwrap().to_owned())
        .collect()
}

/// The recorded engine log without the listing reads (the front reads the list in its own places).
fn recorded_engine(case: &Value) -> Vec<String> {
    strings(&case["engine"])
        .into_iter()
        .filter(|l| !l.starts_with("GET "))
        .map(|l| canonical(&l))
        .collect()
}

/// `POST /gate/v1/leases {"model":..,"holder":..}` as the model alone (the holder names the client).
fn recorded_gate(case: &Value) -> Vec<String> {
    strings(&case["gate"])
        .into_iter()
        .map(|l| match l.split_once(" {") {
            Some((head, rest)) => {
                let v: Value = serde_json::from_str(&format!("{{{rest}")).unwrap();
                format!("{head} model={}", v["model"].as_str().unwrap())
            }
            None => l,
        })
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn every_recorded_scenario_gets_cores_answer_and_requests() {
    let recorded: Value = serde_json::from_str(CASES).unwrap();
    for case in recorded["cases"].as_array().unwrap() {
        let id = case["id"].as_str().unwrap();
        let ran = run(case, Some(ADMIN)).await;
        let (status, want) = match id {
            // Core threw a TypeError on a null body; the port answers like any body without a name.
            "load-null-body" | "unload-null-body" => (400, json!({"error": "name required"})),
            // The budget decision is Core's (a bridge call), whose refusal the route does not pass on.
            _ => (
                case["answer"]["status"].as_u64().unwrap(),
                case["answer"]["body"].clone(),
            ),
        };
        assert_eq!(u64::from(ran.status), status, "{id}: {}", ran.body);
        assert_eq!(
            serde_json::from_str::<Value>(&ran.body).unwrap(),
            want,
            "{id}"
        );
        // Node's key order (the recorded JSON map is sorted, so it is pinned by name).
        let order: &[&str] = match (ran.status, case["answer"]["body"].get("code")) {
            (200, _) if id.contains("force-success") => &[r#"{"ok":true,"forced":true}"#],
            (200, _) => &[r#"{"ok":true}"#],
            (_, Some(_)) if id.contains("in-use-can") || id.contains("in-use-cannot") => {
                &[r#"","code":"model_in_use","retryable":true,"canForce":"#]
            }
            (_, Some(_)) => &[r#"","code":""#, r#"","retryable":"#],
            _ => &[],
        };
        let mut from = 0;
        for piece in order {
            let at = ran.body[from..]
                .find(piece)
                .unwrap_or_else(|| panic!("{id}: key order {piece} in {}", ran.body));
            from += at + piece.len();
        }
        assert_eq!(ran.engine, recorded_engine(case), "{id}: engine requests");
        assert_eq!(ran.gate, recorded_gate(case), "{id}: gate requests");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_budget_fence_stops_a_load_before_the_gate_or_the_router() {
    let recorded: Value = serde_json::from_str(CASES).unwrap();
    let case = recorded["cases"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["id"] == "budget-fence-refused")
        .unwrap();
    let ran = run(case, Some(ADMIN)).await;
    assert_eq!(
        (ran.status, ran.body.as_str()),
        (502, r#"{"error":"load failed: 409"}"#)
    );
    assert!(
        ran.engine.is_empty() && ran.gate.is_empty(),
        "{ran:?}",
        ran = (ran.engine, ran.gate)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_member_is_refused_and_nothing_is_touched() {
    let recorded: Value = serde_json::from_str(CASES).unwrap();
    for id in ["load-success", "unload-success"] {
        let case = recorded["cases"]
            .as_array()
            .unwrap()
            .iter()
            .find(|c| c["id"] == id)
            .unwrap();
        let ran = run(case, Some(MEMBER)).await;
        assert_eq!(
            (ran.status, ran.body.as_str()),
            (403, r#"{"error":"administrator required"}"#),
            "{id}"
        );
        assert!(ran.engine.is_empty() && ran.gate.is_empty(), "{id}");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_signed_out_request_is_refused_and_nothing_is_touched() {
    let recorded: Value = serde_json::from_str(CASES).unwrap();
    let case = &recorded["cases"][0];
    let ran = run(case, None).await;
    assert_eq!(ran.status, 401, "{}", ran.body);
    assert!(ran.engine.is_empty() && ran.gate.is_empty());
}

/// A client that leaves while its load waits in the gate's queue starts no switch.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_client_that_leaves_the_queue_starts_no_switch() {
    let d = tempfile::tempdir().unwrap();
    let _db = seed_database(d.path());
    let ini = d.path().join("models.ini");
    std::fs::write(&ini, "version = 1\n").unwrap();
    let sc = json!({"models": [{"name": "qa-model-a", "file": true, "status": "unloaded"}]});
    let (core, _) = engine(scripted_core(&sc)).await;
    let granted = sse(&json!([["granted", {"lease": "a".repeat(32)}]]));
    let (router, seen) = engine(Arc::new(move |req: &Req| {
        match (req.method.as_str(), req.target.as_str()) {
            ("POST", "/gate/v1/leases") => Answer::Events(Duration::from_secs(2), granted.clone()),
            ("GET", "/v1/models" | "/models") => Answer::Reply(
                200,
                json!({"data": [{"id": "qa-model-a", "source": "preset", "status": {"value": "unloaded", "args": ["--model", "/models/qa-model-a.gguf"]}}]}).to_string(),
            ),
            _ => Answer::Reply(404, "{}".into()),
        }
    }))
    .await;
    let env = vec![
        (
            "NOEVIA_LEGACY_UPSTREAM".to_owned(),
            format!("http://127.0.0.1:{core}"),
        ),
        ("NOEVIA_PROJECT_STORE_TOKEN".to_owned(), SECRET.to_owned()),
        ("MODEL_MANAGER_KIND".to_owned(), "llamacpp".to_owned()),
        (
            "MODEL_MANAGER_BASE_URL".to_owned(),
            format!("http://127.0.0.1:{router}"),
        ),
        ("LLAMACPP_PRESET_PATH".to_owned(), ini.display().to_string()),
    ];
    let (addr, _stop) = start(d.path(), &env).await;
    let gone = tokio::time::timeout(
        Duration::from_millis(600),
        call(
            addr,
            "POST",
            "/api/models/load",
            Some(ADMIN),
            br#"{"name":"qa-model-a"}"#,
        ),
    )
    .await;
    assert!(gone.is_err(), "the call was still waiting in the queue");
    tokio::time::sleep(Duration::from_secs(3)).await;
    let log = seen.lock().unwrap().clone();
    assert!(log.iter().any(|r| r.target == "/gate/v1/leases"));
    assert!(
        !log.iter().any(|r| r.target == "/models/load"),
        "a client that left started a switch: {log:?}"
    );
}
