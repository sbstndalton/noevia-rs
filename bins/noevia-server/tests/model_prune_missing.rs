//! Real HTTP through the native front for `POST /api/models/prune-missing` (sbstndalton/noevia#1695, #1698,
//! #2219). Every scenario of tests/fixtures/prune-missing/prune-missing-node.json was first run through Core's
//! own route (record-prune.cjs, Core 6c23568d) with scripted fakes; the front must give the same status and body
//! text, send the model manager the same requests, reload the router the same number of times, leave the same
//! sections, and clear the role references and the last-loaded state of exactly the names Core cleared.
//! The legacy upstream is a scripted Core (its loopback facts and clear calls); no engine is touched.
//! Synthetic QA-named data only.
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

const CASES: &str = include_str!("fixtures/prune-missing/prune-missing-node.json");
const TOKEN: &str = "0123456789abcdef0123456789abcdef";
const SECRET: &str = "synthetic-store-token-0123456789abcdef";

/// Core's loopback side: the installed facts and the two state bridges.
fn scripted_core(sc: &Value) -> Handler {
    let sidecars: Vec<Value> = sc["models"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|m| m["servedElsewhere"] == true)
        .map(|m| m["name"].clone())
        .collect();
    Arc::new(move |req: &Req| {
        let t = req.target.as_str();
        if t == "/internal/models/facts" {
            return Answer::Reply(
                200,
                json!({"sidecarNames": sidecars, "sidecarLoaded": [], "scan": []}).to_string(),
            );
        }
        if t.starts_with("/internal/") {
            return Answer::Reply(200, r#"{"ok":true,"nodeCleared":true}"#.into());
        }
        Answer::Reply(200, "{}".into())
    })
}

/// The router (and its gate) of the scenario: one preset row per model.
fn scripted_router(sc: &Value, reloads: Arc<Mutex<usize>>) -> Handler {
    let sc = sc.clone();
    Arc::new(move |req: &Req| {
        let t = req.target.as_str();
        if t.starts_with("/gate/v1/") {
            return match (req.method.as_str(), t) {
                ("POST", "/gate/v1/maintenance") if sc["holdRefused"] == true => Answer::Reply(
                    409,
                    json!({"error": {"type": "gate_error", "code": "busy", "message": "busy", "retryable": true}}).to_string(),
                ),
                ("POST", "/gate/v1/maintenance") => Answer::Reply(
                    201,
                    json!({"hold": TOKEN, "idle_ttl_ms": 600_000}).to_string(),
                ),
                ("DELETE", _) => Answer::Reply(204, String::new()),
                _ => Answer::Reply(200, "{}".into()),
            };
        }
        if req.method == "GET" && t == "/v1/models" || t == "/models" {
            let rows: Vec<Value> = sc["models"]
                .as_array()
                .unwrap()
                .iter()
                .map(|m| {
                    let name = m["name"].as_str().unwrap();
                    json!({"id": name, "source": m.get("source").cloned().unwrap_or(json!("preset")),
                        "status": {"value": "unloaded", "args": ["--model", format!("/models/{name}.gguf")]}})
                })
                .collect();
            return Answer::Reply(200, json!({"data": rows}).to_string());
        }
        if t == "/models?reload=1" || t == "/v1/models?reload=1" {
            *reloads.lock().unwrap() += 1;
            return if sc["reloadOk"] == false {
                Answer::Reply(500, "{}".into())
            } else {
                Answer::Reply(200, r#"{"success":true}"#.into())
            };
        }
        Answer::Reply(404, "{}".into())
    })
}

/// The model manager: the folder scan, the sections and their revision-checked delete.
fn scripted_loader(sc: &Value, sections: Arc<Mutex<Vec<(String, bool)>>>) -> Handler {
    let sc = sc.clone();
    Arc::new(move |req: &Req| {
        let t = req.target.as_str();
        let scan: Vec<Value> = match sc.get("scan") {
            Some(s) => s.as_array().unwrap().clone(),
            None => sc["models"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|m| m["file"] == true)
                .map(|m| json!({"modelId": m["name"], "sections": [m["name"]]}))
                .collect(),
        };
        let mut now = sections.lock().unwrap();
        if req.method == "GET" && t == "/api/v1/models" {
            return Answer::Reply(200, json!({"models": scan}).to_string());
        }
        if req.method == "GET" && t == "/api/v1/sections" {
            let rows: Vec<Value> = now
                .iter()
                .map(|(n, h)| json!({"name": n, "hasFile": h}))
                .collect();
            return Answer::Reply(
                200,
                json!({"revision": format!("r{}", now.len()), "sections": rows}).to_string(),
            );
        }
        if req.method == "DELETE" && sc["refuseDelete"] != true {
            if let Some(rest) = t.strip_prefix("/api/v1/sections/") {
                let (name, q) = rest.split_once('?').unwrap();
                if q == format!("baseRevision=r{}", now.len()) {
                    now.retain(|(n, _)| n != name);
                    return Answer::Reply(200, "{}".into());
                }
            }
        }
        Answer::Reply(409, "{}".into())
    })
}

/// Core's request lines, as record-prune.cjs logs them.
fn loader_lines(seen: &[Req]) -> Vec<String> {
    seen.iter()
        .map(|r| format!("{} {}", r.method, r.target.trim_start_matches("/api/v1/")))
        .collect()
}

async fn run(
    case: &Value,
    as_member: bool,
) -> (u16, String, Vec<String>, usize, Vec<String>, Vec<String>) {
    let sc = &case["scenario"];
    let d = tempfile::tempdir().unwrap();
    let _db = seed_database(d.path());
    let ini = d.path().join("models.ini");
    std::fs::write(&ini, "version = 1\n").unwrap();
    let reloads = Arc::new(Mutex::new(0usize));
    let sections = Arc::new(Mutex::new(
        sc["models"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| (m["name"].as_str().unwrap().to_owned(), m["hasFile"] == true))
            .collect::<Vec<_>>(),
    ));
    let (core, core_seen) = engine(scripted_core(sc)).await;
    let (router, _r) = engine(scripted_router(sc, Arc::clone(&reloads))).await;
    let (loader, loader_seen) = engine(scripted_loader(sc, Arc::clone(&sections))).await;
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
            "INFERENCE_GATE_ADMIN_TOKEN".to_owned(),
            "synthetic-admin-token".to_owned(),
        ),
        (
            "NOEVIA_FEATURE_NATIVE_CLIENT_AUTH".to_owned(),
            "1".to_owned(),
        ),
    ];
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
    let cookie = if as_member { MEMBER } else { ADMIN };
    let (status, _h, body) =
        call(addr, "POST", "/api/models/prune-missing", Some(cookie), b"").await;
    let calls: Vec<String> = core_seen
        .lock()
        .unwrap()
        .iter()
        .map(|r| format!("{} {} {}", r.method, r.target, r.body))
        .collect();
    let forgotten: Vec<String> = calls
        .iter()
        .filter(|c| c.contains("/internal/model-state/forget"))
        .map(|c| {
            serde_json::from_str::<Value>(c.rsplit_once(' ').unwrap().1).unwrap()["name"]
                .as_str()
                .unwrap()
                .to_owned()
        })
        .collect();
    let after: Vec<String> = sections
        .lock()
        .unwrap()
        .iter()
        .map(|(n, _)| n.clone())
        .collect();
    let reloaded = *reloads.lock().unwrap();
    let lines = loader_lines(&loader_seen.lock().unwrap());
    (status, body, lines, reloaded, after, forgotten)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn every_recorded_scenario_gets_cores_answer_and_side_effects() {
    let recorded: Value = serde_json::from_str(CASES).unwrap();
    for case in recorded["cases"].as_array().unwrap() {
        let id = case["id"].as_str().unwrap();
        let (status, body, lines, reloads, after, forgotten) = run(case, false).await;
        assert_eq!(
            u64::from(status),
            case["answer"]["status"].as_u64().unwrap(),
            "{id}: {body}"
        );
        assert_eq!(
            serde_json::from_str::<Value>(&body).unwrap(),
            case["answer"]["body"],
            "{id}"
        );
        // Node's key order (the recorded JSON map is sorted, so it is pinned here).
        if status == 200 {
            assert!(
                body.starts_with(r#"{"removed":["#)
                    && body.contains(r#","kept":["#)
                    && body.ends_with(r#""reloaded":true}"#)
                        | body.ends_with(r#""reloaded":false}"#),
                "{id}: {body}"
            );
        }
        let want: Vec<String> = case["loader"]
            .as_array()
            .unwrap()
            .iter()
            .map(|l| l.as_str().unwrap().to_owned())
            .collect();
        // Core's own folder scan GET is the same request; the sections GET repeats per removal in both.
        assert_eq!(lines, want, "{id}: loader requests");
        assert_eq!(
            reloads as u64,
            case["reloads"].as_u64().unwrap(),
            "{id}: reloads"
        );
        let sections: Vec<String> = case["sectionsAfter"]
            .as_array()
            .unwrap()
            .iter()
            .map(|l| l.as_str().unwrap().to_owned())
            .collect();
        assert_eq!(after, sections, "{id}: sections left");
        let cleared: Vec<String> = case["rolesCleared"]
            .as_array()
            .unwrap()
            .iter()
            .map(|l| l.as_str().unwrap().to_owned())
            .collect();
        assert_eq!(
            forgotten, cleared,
            "{id}: last-loaded state forgotten for the removed names"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_member_is_refused_and_nothing_is_touched() {
    let recorded: Value = serde_json::from_str(CASES).unwrap();
    let case = &recorded["cases"][0];
    let (status, body, lines, reloads, after, _) = run(case, true).await;
    assert_eq!(
        (status, body.as_str()),
        (403, r#"{"error":"administrator required"}"#)
    );
    assert!(lines.is_empty() && reloads == 0, "{lines:?}");
    assert_eq!(
        after.len(),
        case["scenario"]["models"].as_array().unwrap().len()
    );
}
