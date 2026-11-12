//! Real HTTP through the native front for `POST /api/models/delete` (sbstndalton/noevia#2291, #336, #1660, #1661,
//! #2115, #2219). Every scenario of tests/fixtures/delete-model/delete-model-node.json was first run through Core's
//! own route (record-delete.cjs, Core 76cc868) with scripted fakes; the front must give the same status and body
//! text (key order included), send the engine the same unload and delete requests, the model manager the same
//! section requests, reload the router the same number of times, and leave the same sections.
//! Two recorded scenarios are Core failures the port fixes (`null-body`: a TypeError; `role-cleanup-throws`:
//! an error after the file was gone); their expected answers are pinned below, not recorded.
//! The legacy upstream is a scripted Core (its loopback facts and bridges); no engine is touched.
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

const CASES: &str = include_str!("fixtures/delete-model/delete-model-node.json");
const TOKEN: &str = "0123456789abcdef0123456789abcdef";
const SECRET: &str = "synthetic-store-token-0123456789abcdef";

/// Core's loopback side: the sidecar facts, the installed facts and the state bridges.
fn scripted_core(sc: &Value) -> Handler {
    let sidecars = sc.get("sidecars").cloned().unwrap_or(json!([]));
    let scan: Vec<Value> = sc["models"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|m| m["file"] == true)
        .map(|m| json!({"modelId": m["name"], "sections": [m["name"]]}))
        .collect();
    let installed_fails = sc["installedFails"] == true;
    Arc::new(move |req: &Req| {
        let t = req.target.as_str();
        if t == "/internal/models/facts" {
            let ask: Value = serde_json::from_str(&req.body).unwrap_or_default();
            // The delete's own sidecar ask names no ids; the installed list names the router's rows.
            if installed_fails && ask["ids"].as_array().is_some_and(|a| !a.is_empty()) {
                return Answer::Reply(503, "{}".into());
            }
            return Answer::Reply(
                200,
                json!({"sidecarNames": sidecars, "sidecarLoaded": [], "scan": scan}).to_string(),
            );
        }
        if t.starts_with("/internal/") {
            return Answer::Reply(200, r#"{"ok":true,"nodeCleared":true}"#.into());
        }
        Answer::Reply(200, "{}".into())
    })
}

fn scripted_reply(list: &Value, n: usize) -> Answer {
    let list = list.as_array().unwrap();
    let r = &list[n.min(list.len() - 1)];
    Answer::Reply(r["status"].as_u64().unwrap() as u16, r["body"].to_string())
}

/// The router (and its gate): one preset row per model, the scripted unload and delete answers.
fn scripted_router(sc: &Value, reloads: Arc<Mutex<usize>>) -> Handler {
    let sc = sc.clone();
    let counts = Arc::new(Mutex::new((0usize, 0usize)));
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
        if req.method == "GET" && (t == "/v1/models" || t == "/models") {
            if sc["listing"] == "fail" {
                return Answer::Reply(500, "{}".into());
            }
            let rows: Vec<Value> = sc["models"]
                .as_array()
                .unwrap()
                .iter()
                .map(|m| {
                    let name = m["name"].as_str().unwrap();
                    let path = match sc.get("routerPath") {
                        Some(p) if Some(name) == sc["name"].as_str() => p.as_str().unwrap().to_owned(),
                        _ => format!("/models/{name}.gguf"),
                    };
                    json!({"id": name, "source": "preset", "status": {"value": "unloaded", "args": ["--model", path]}})
                })
                .collect();
            return Answer::Reply(200, json!({"data": rows}).to_string());
        }
        if req.method == "POST" && t == "/models/unload" {
            let mut c = counts.lock().unwrap();
            c.0 += 1;
            return scripted_reply(&sc["unload"], c.0 - 1);
        }
        if req.method == "DELETE" && t.starts_with("/models?model=") {
            let mut c = counts.lock().unwrap();
            c.1 += 1;
            return scripted_reply(&sc["delete"], c.1 - 1);
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

/// The model manager: the sections and their revision-checked delete.
fn scripted_loader(sc: &Value, sections: Arc<Mutex<Vec<(String, bool)>>>) -> Handler {
    let sc = sc.clone();
    Arc::new(move |req: &Req| {
        let t = req.target.as_str();
        let mut now = sections.lock().unwrap();
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
        if req.method == "DELETE" && sc["refuseSection"] != true {
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

struct Ran {
    status: u16,
    body: String,
    loader: Vec<String>,
    engine: Vec<String>,
    reloads: usize,
    after: Vec<String>,
    forgotten: Vec<String>,
}

async fn run(case: &Value, cookie: Option<&str>) -> Ran {
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
    let (router, router_seen) = engine(scripted_router(sc, Arc::clone(&reloads))).await;
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
            .unwrap_or_else(|| {
                let mut b = json!({"name": sc["name"]});
                if sc["confirmSystem"] == true {
                    b["confirmSystem"] = json!(true);
                }
                b
            })
            .to_string(),
    };
    let (status, _h, body) =
        call(addr, "POST", "/api/models/delete", cookie, sent.as_bytes()).await;
    let forgotten: Vec<String> = core_seen
        .lock()
        .unwrap()
        .iter()
        .filter(|r| r.target.contains("/internal/model-state/forget"))
        .map(|r| {
            serde_json::from_str::<Value>(&r.body).unwrap()["name"]
                .as_str()
                .unwrap()
                .to_owned()
        })
        .collect();
    let after = sections
        .lock()
        .unwrap()
        .iter()
        .map(|(n, _)| n.clone())
        .collect();
    let reloaded = *reloads.lock().unwrap();
    let loader_lines = loader_seen
        .lock()
        .unwrap()
        .iter()
        .map(|r| format!("{} {}", r.method, r.target.trim_start_matches("/api/v1/")))
        .collect();
    let engine_lines = router_seen
        .lock()
        .unwrap()
        .iter()
        .filter(|r| {
            r.target == "/models/unload"
                || r.method == "DELETE" && r.target.starts_with("/models?model=")
        })
        .map(|r| format!("{} {}", r.method, r.target))
        .collect();
    Ran {
        status,
        body,
        loader: loader_lines,
        engine: engine_lines,
        reloads: reloaded,
        after,
        forgotten,
    }
}

fn strings(v: &Value) -> Vec<String> {
    v.as_array()
        .unwrap()
        .iter()
        .map(|l| l.as_str().unwrap().to_owned())
        .collect()
}

/// The recorded engine log without the router listing reads (the front reads the list in its own places).
fn recorded_engine(case: &Value) -> Vec<String> {
    strings(&case["engine"])
        .into_iter()
        .filter(|l| !l.starts_with("GET "))
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn every_recorded_scenario_gets_cores_answer_and_side_effects() {
    let recorded: Value = serde_json::from_str(CASES).unwrap();
    for case in recorded["cases"].as_array().unwrap() {
        let id = case["id"].as_str().unwrap();
        let ran = run(case, Some(ADMIN)).await;
        let (status, body) = match id {
            // Core threw a TypeError on a null body; the port answers like any body without a name.
            "null-body" => (400, json!({"error": "name required"})),
            // Core threw after the file was gone; the port answers the delete it did.
            "role-cleanup-throws" => (200, case["answer"].clone()),
            _ => (
                case["answer"]["status"].as_u64().unwrap(),
                case["answer"]["body"].clone(),
            ),
        };
        let want = if id == "role-cleanup-throws" {
            json!({"ok": true, "unloaded": true, "rolesCleared": [], "presetRemoved": true, "presetsReloaded": true})
        } else {
            body
        };
        assert_eq!(u64::from(ran.status), status, "{id}: {}", ran.body);
        assert_eq!(
            serde_json::from_str::<Value>(&ran.body).unwrap(),
            want,
            "{id}"
        );
        // Node's key order (the recorded JSON map is sorted, so it is pinned by name).
        let order: Vec<&str> = match (ran.status, id) {
            (200, _) => vec![
                r#"{"ok":true,"unloaded":"#,
                r#","rolesCleared":["#,
                r#","presetRemoved":"#,
            ],
            (
                _,
                "system-model-refused"
                | "system-model-by-router-path"
                | "system-model-confirm-not-true",
            ) => {
                vec![
                    r#"{"error":"System routing model — not deleted","errorId":"systemModel","service":"decision"}"#,
                ]
            }
            (409, "sidecar-embedding" | "sidecar-reranker") => {
                vec![r#"","errorId":"modelInUse","service":""#]
            }
            (409, "file-missing-hold-refused") => {
                vec![r#"","code":"maintenance_held","retryable":true,"unloaded":false}"#]
            }
            (409, _) if case["answer"]["body"]["code"].is_string() => {
                vec![r#"","code":""#, r#"","retryable":"#, r#","unloaded":"#]
            }
            (502, _) => vec![r#"{"error":"delete failed: 500","unloaded":"#],
            _ => vec![],
        };
        let mut from = 0;
        for piece in order {
            let at = ran.body[from..]
                .find(piece)
                .unwrap_or_else(|| panic!("{id}: key order {piece} in {}", ran.body));
            from += at + piece.len();
        }
        if id == "deletes-everything" {
            assert!(
                ran.body.ends_with(r#""presetsReloaded":true}"#),
                "{}",
                ran.body
            );
        }
        if id == "deleted-hold-refused" {
            assert!(
                ran.body
                    .ends_with(r#""presetRemoved":false,"presetBusy":true}"#),
                "{}",
                ran.body
            );
        }
        assert_eq!(ran.engine, recorded_engine(case), "{id}: engine requests");
        assert_eq!(
            ran.loader,
            strings(&case["loader"]),
            "{id}: loader requests"
        );
        assert_eq!(
            ran.reloads as u64,
            case["reloads"].as_u64().unwrap(),
            "{id}: reloads"
        );
        assert_eq!(
            ran.after,
            strings(&case["sectionsAfter"]),
            "{id}: sections left"
        );
        // The model's state is forgotten exactly when the delete went through.
        let went = ran.status == 200;
        let name = case["scenario"]["name"].as_str().unwrap_or_default();
        let forgot: Vec<String> = if went {
            vec![name.to_owned()]
        } else {
            Vec::new()
        };
        assert_eq!(ran.forgotten, forgot, "{id}: last-loaded state forgotten");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_member_is_refused_and_nothing_is_touched() {
    let recorded: Value = serde_json::from_str(CASES).unwrap();
    let case = &recorded["cases"][0];
    let ran = run(case, Some(MEMBER)).await;
    assert_eq!(
        (ran.status, ran.body.as_str()),
        (403, r#"{"error":"administrator required"}"#)
    );
    assert!(ran.engine.is_empty() && ran.loader.is_empty() && ran.reloads == 0);
    assert_eq!(
        ran.after.len(),
        case["scenario"]["models"].as_array().unwrap().len()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_signed_out_request_is_refused_and_nothing_is_touched() {
    let recorded: Value = serde_json::from_str(CASES).unwrap();
    let case = &recorded["cases"][0];
    let ran = run(case, None).await;
    assert_eq!(ran.status, 401, "{}", ran.body);
    assert!(ran.engine.is_empty() && ran.loader.is_empty() && ran.reloads == 0);
}
