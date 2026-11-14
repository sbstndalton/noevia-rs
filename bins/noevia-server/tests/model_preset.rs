//! Real HTTP through the native front for `PUT /api/models/preset` (track R11), behind
//! `NOEVIA_RUST_MODEL_WRITES=rust`. Every scenario of tests/fixtures/model-maint/preset-cases.json
//! was first run through Core's own route and manager (record-preset.cjs, before the route moved)
//! against a scripted router and inference gate on one port; the front must give the same status
//! and body text, send the router and the gate the same requests, and leave the same models.ini,
//! recovery copies and reload guard state file. The legacy upstream is a scripted Node that only
//! grants maintenance leases and answers the budget facts, so only the native handler can answer.
//! Then: with the switch unset the PUT is relayed, and the named differences of model_maint
//! preset.rs. Synthetic data only; no engine is touched.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]
mod maint_support;
use maint_support::*;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

const CASES: &str = include_str!("fixtures/model-maint/preset-cases.json");
const TOKEN: &str = "0123456789abcdef0123456789abcdef";
const PATH: &str = "/api/models/preset";

/// The scripted engine of record-preset.cjs.
fn scripted(case: &Value, ini: PathBuf, calls: Arc<AtomicUsize>) -> Handler {
    let case = case.clone();
    Arc::new(move |req: &Req| {
        let router = &case["router"];
        let t = req.target.as_str();
        if t.starts_with("/gate/v1/") {
            if req.method == "POST" && t == "/gate/v1/maintenance" {
                if case["gate"] == "drop" {
                    return Answer::Drop;
                }
                if case["gate"] == "absent" {
                    return Answer::Reply(404, r#"{"error":"not found"}"#.into());
                }
                if let Some(r) = case["gate"].get("refuse") {
                    return Answer::Reply(
                        r["status"].as_u64().unwrap() as u16,
                        json!({"error": {"type": "gate_error", "code": r["code"], "message": r["message"], "retryable": false}}).to_string(),
                    );
                }
                return Answer::Reply(
                    201,
                    json!({"hold": TOKEN, "idle_ttl_ms": 600_000}).to_string(),
                );
            }
            if req.method == "DELETE" {
                return Answer::Reply(204, String::new());
            }
            return Answer::Reply(200, "{}".into());
        }
        if req.method == "GET" && t == "/models" {
            let listings = router["listings"].as_array().unwrap();
            let i = calls.fetch_add(1, Ordering::SeqCst).min(listings.len() - 1);
            let l = &listings[i];
            if l["drop"] == true {
                return Answer::Drop;
            }
            let body = match l.get("text").and_then(Value::as_str) {
                Some(text) => text.to_owned(),
                None => json!({"data": l["rows"]}).to_string(),
            };
            return Answer::Reply(l["status"].as_u64().unwrap() as u16, body);
        }
        if req.method == "GET" && t == "/models?reload=1" {
            if let Some(text) = router["iniAfterReload"].as_str() {
                std::fs::write(&ini, text).unwrap();
            }
            let r = router.get("reload").filter(|r| !r.is_null());
            if r.is_some_and(|r| r["drop"] == true) {
                return Answer::Drop;
            }
            return match r {
                Some(r) => Answer::Reply(
                    r["status"].as_u64().unwrap() as u16,
                    r["text"].as_str().unwrap().to_owned(),
                ),
                None => Answer::Reply(200, r#"{"success":true}"#.into()),
            };
        }
        Answer::Reply(404, "{}".into())
    })
}

fn lines(seen: &[Req]) -> (Vec<String>, Vec<String>) {
    let (mut router, mut gate) = (Vec::new(), Vec::new());
    for r in seen {
        // Asked only to tell a gate without an admin token from no gate at all (#2210).
        if r.method == "GET" && r.target == "/gate/v1/status" {
            continue;
        }
        if r.target.starts_with("/gate/v1/") {
            let mut norm = r.target.replace(TOKEN, "T");
            if r.method == "POST" && norm == "/gate/v1/maintenance" {
                norm = format!("{norm} {}", r.body);
            }
            gate.push(format!("{} {norm}", r.method));
        } else {
            let hold = if r.headers.contains_key("x-noevia-maintenance") {
                "hold"
            } else {
                "nohold"
            };
            let mut line = format!("{} {} {hold}", r.method, r.target);
            if r.method == "POST" {
                line = format!("{line} {}", r.body);
            }
            router.push(line);
        }
    }
    (router, gate)
}

fn hex_bytes(s: &str) -> Vec<u8> {
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
        .collect()
}

fn backups(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.starts_with("models.ini.noevia-backup-"))
        .collect();
    names.sort();
    names
}

/// The scripted Node: maintenance leases, and the inference budget when the case has one.
async fn node(budget: Option<f64>) -> (u16, Arc<std::sync::Mutex<Vec<Req>>>) {
    engine(Arc::new(move |req: &Req| {
        if req.method == "POST" && req.target == "/internal/model-maintenance/lease" {
            return Answer::Reply(
                201,
                json!({"lease": TOKEN, "ttlMs": 600_000, "renewMs": 200_000, "hardMs": 3_600_000})
                    .to_string(),
            );
        }
        if req.method == "POST" && req.target == "/internal/models/facts" {
            if let Some(b) = budget {
                return Answer::Reply(200, json!({"budget": {"budgetGib": b}}).to_string());
            }
            return Answer::Reply(503, "{}".into());
        }
        Answer::Reply(204, String::new())
    }))
    .await
}

struct Front {
    addr: std::net::SocketAddr,
    stop: tokio::sync::oneshot::Sender<()>,
    ini: PathBuf,
    state: PathBuf,
    seen: Arc<std::sync::Mutex<Vec<Req>>>,
}

/// The front over `dir` for one recorded case, with the switch as given.
async fn front_for(case: &Value, dir: &Path, switch: bool) -> Front {
    let ini = dir.join("models.ini");
    std::fs::write(&ini, case["ini"].as_str().unwrap()).unwrap();
    if let Some(roles) = case["roles"].as_str() {
        std::fs::write(dir.join("models.roles.json"), roles).unwrap();
    }
    let models = dir.join("models");
    if let Some(mount) = case["mount"].as_object() {
        std::fs::create_dir(&models).unwrap();
        for (name, hex) in mount {
            std::fs::write(models.join(name), hex_bytes(hex.as_str().unwrap())).unwrap();
        }
    }
    let calls = Arc::new(AtomicUsize::new(0));
    let (port, seen) = engine(scripted(case, ini.clone(), calls)).await;
    let base = format!("http://127.0.0.1:{port}/v1");
    let state = dir.join(format!("native-preset-reload-{}.json", suffix(&base)));
    match &case["state"] {
        Value::Null => {}
        Value::String(s) => std::fs::write(&state, s).unwrap(),
        other => std::fs::write(&state, other.to_string()).unwrap(),
    }
    let mut env: Vec<(String, String)> = vec![
        (
            "MODEL_MANAGER_KIND".into(),
            case["kind"].as_str().unwrap().into(),
        ),
        ("MODEL_MANAGER_BASE_URL".into(), base),
    ];
    if case["presets"] == true {
        env.push(("LLAMACPP_PRESET_PATH".into(), ini.display().to_string()));
    }
    if case["mount"].is_object() {
        env.push(("LLAMACPP_MODELS_PATH".into(), models.display().to_string()));
    }
    if switch {
        env.push(("NOEVIA_RUST_MODEL_WRITES".into(), "rust".into()));
        // With no gate's hold the front leases Node's own maintenance gate (Node's answer to that
        // was its own hold in the recording); the budget is Core's figure over the facts bridge.
        let (node_port, _) = node(case["budget"].as_f64()).await;
        env.push((
            "NOEVIA_LEGACY_UPSTREAM".into(),
            format!("http://127.0.0.1:{node_port}"),
        ));
        env.push((
            "NOEVIA_PROJECT_STORE_TOKEN".into(),
            "synthetic-store-token-0123456789abcdef".into(),
        ));
    }
    if case["gateToken"] == true {
        env.push((
            "INFERENCE_GATE_ADMIN_TOKEN".into(),
            "synthetic-admin-token".into(),
        ));
    }
    let (addr, stop) = start(dir, &env).await;
    Front {
        addr,
        stop,
        ini,
        state,
        seen,
    }
}

fn request_body(case: &Value) -> Vec<u8> {
    let request = &case["request"];
    match request["big"].as_u64() {
        Some(n) => format!("\"{}\"", "a".repeat(n as usize - 2)).into_bytes(),
        None => request["body"].as_str().unwrap().as_bytes().to_vec(),
    }
}

fn who(case: &Value) -> &'static str {
    if case["request"]["role"] == "admin" {
        ADMIN
    } else {
        MEMBER
    }
}

#[tokio::test]
async fn every_recorded_core_answer_is_the_fronts_byte_for_byte() {
    let cases: Value = serde_json::from_str(CASES).unwrap();
    let mut checked = 0;
    for case in cases["cases"].as_array().unwrap() {
        let name = case["name"].as_str().unwrap();
        let d = tempfile::tempdir().unwrap();
        let _db = seed_database(d.path());
        let f = front_for(case, d.path(), true).await;
        let (status, headers, body) =
            call(f.addr, "PUT", PATH, Some(who(case)), &request_body(case)).await;
        let _ = f.stop.send(());
        let expect = &case["expect"];
        assert_eq!(
            (status, body.as_str()),
            (
                expect["status"].as_u64().unwrap() as u16,
                expect["body"].as_str().unwrap()
            ),
            "{name}"
        );
        assert!(headers.contains("content-type: application/json"), "{name}");
        // Settled: the hold's release is sent after the answer.
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        let (router, gate) = lines(&f.seen.lock().unwrap());
        let want = |k: &str| -> Vec<String> {
            expect[k]
                .as_array()
                .unwrap()
                .iter()
                .map(|v| v.as_str().unwrap().to_owned())
                .collect()
        };
        assert_eq!(router, want("router"), "{name}: the router was asked");
        assert_eq!(gate, want("gate"), "{name}: the gate was asked");
        assert_eq!(
            std::fs::read_to_string(&f.ini).unwrap(),
            expect["ini"].as_str().unwrap(),
            "{name}: models.ini"
        );
        assert_eq!(
            backups(d.path()),
            want("backups"),
            "{name}: the recovery copies"
        );
        assert_eq!(
            std::fs::read_to_string(&f.state).ok().as_deref(),
            expect["state"].as_str(),
            "{name}: the reload guard state file"
        );
        checked += 1;
    }
    assert_eq!(checked, cases["cases"].as_array().unwrap().len());
    assert!(
        checked >= 80,
        "every recorded scenario was replayed ({checked})"
    );
}

fn case_named(name: &str) -> Value {
    let cases: Value = serde_json::from_str(CASES).unwrap();
    cases["cases"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["name"] == name)
        .unwrap_or_else(|| panic!("no recorded case {name}"))
        .clone()
}

#[tokio::test]
async fn with_the_switch_unset_the_put_is_relayed() {
    let case = case_named("saved: the full answer");
    let d = tempfile::tempdir().unwrap();
    let _db = seed_database(d.path());
    let f = front_for(&case, d.path(), false).await;
    let (status, _, _) = call(f.addr, "PUT", PATH, Some(ADMIN), &request_body(&case)).await;
    let _ = f.stop.send(());
    // The legacy upstream is dead: 502 from the proxy, not an answer of the front.
    assert_eq!(status, 502);
    assert!(f.seen.lock().unwrap().is_empty(), "nothing was asked");
    assert_eq!(
        std::fs::read_to_string(&f.ini).unwrap(),
        case["ini"].as_str().unwrap()
    );
}

#[tokio::test]
async fn signed_out_and_other_paths_are_not_the_puts() {
    let case = case_named("saved: the full answer");
    let d = tempfile::tempdir().unwrap();
    let _db = seed_database(d.path());
    let f = front_for(&case, d.path(), true).await;
    // Signed out: the front's own 401, before anything of the route.
    let (status, _, _) = call(f.addr, "PUT", PATH, None, &request_body(&case)).await;
    assert_eq!(status, 401);
    // Other methods stay Node's (the scripted Node answers 204 to anything it does not know).
    for m in ["POST", "DELETE"] {
        let (status, _, _) = call(f.addr, m, PATH, Some(ADMIN), &request_body(&case)).await;
        assert_eq!(status, 204, "{m}");
    }
    // A trailing slash is not the save (the front's own answer to that path, as before).
    let (status, _, _) = call(
        f.addr,
        "PUT",
        "/api/models/preset/",
        Some(ADMIN),
        &request_body(&case),
    )
    .await;
    assert_eq!(status, 404);
    let _ = f.stop.send(());
    assert!(f.seen.lock().unwrap().is_empty(), "nothing was asked");
    assert_eq!(
        std::fs::read_to_string(&f.ini).unwrap(),
        case["ini"].as_str().unwrap()
    );
}

/// Named difference: with a models mount and no budget from Core's facts bridge, the save is
/// refused before anything is written (Core always has a budget).
#[tokio::test]
async fn no_budget_from_core_refuses_the_save_with_a_mount() {
    let mut case = case_named("budget: the estimate fits");
    case["budget"] = Value::Null;
    let d = tempfile::tempdir().unwrap();
    let _db = seed_database(d.path());
    let f = front_for(&case, d.path(), true).await;
    let (status, _, body) = call(f.addr, "PUT", PATH, Some(ADMIN), &request_body(&case)).await;
    let _ = f.stop.send(());
    assert_eq!(
        (status, body.as_str()),
        (
            503,
            r#"{"error":"The inference memory budget could not be read."}"#
        )
    );
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    let (router, gate) = lines(&f.seen.lock().unwrap());
    assert_eq!(router, ["GET /models hold"], "nothing past the dry run");
    assert_eq!(gate.len(), 2, "the hold was taken and given back: {gate:?}");
    assert_eq!(
        std::fs::read_to_string(&f.ini).unwrap(),
        case["ini"].as_str().unwrap()
    );
    assert!(backups(d.path()).is_empty());
}

/// Named difference: without a gate's hold and without Node's maintenance gate, nothing is done.
#[tokio::test]
async fn no_arbiter_changes_nothing() {
    let case = case_named("no admin token: this process holds alone");
    let d = tempfile::tempdir().unwrap();
    let _db = seed_database(d.path());
    let ini = d.path().join("models.ini");
    std::fs::write(&ini, case["ini"].as_str().unwrap()).unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    let (port, seen) = engine(scripted(&case, ini.clone(), calls)).await;
    let (addr, stop) = start(
        d.path(),
        &[
            ("NOEVIA_RUST_MODEL_WRITES".into(), "rust".into()),
            ("MODEL_MANAGER_KIND".into(), "llamacpp".into()),
            (
                "MODEL_MANAGER_BASE_URL".into(),
                format!("http://127.0.0.1:{port}/v1"),
            ),
            ("LLAMACPP_PRESET_PATH".into(), ini.display().to_string()),
            (
                "NOEVIA_PROJECT_STORE_TOKEN".into(),
                "synthetic-store-token-0123456789abcdef".into(),
            ),
        ],
    )
    .await;
    let (status, _, body) = call(addr, "PUT", PATH, Some(ADMIN), &request_body(&case)).await;
    let _ = stop.send(());
    assert_eq!(status, 503, "{body}");
    assert!(
        body.contains("could not confirm that chat is idle"),
        "{body}"
    );
    let (router, _) = lines(&seen.lock().unwrap());
    assert!(router.is_empty(), "{router:?}");
    assert_eq!(
        std::fs::read_to_string(&ini).unwrap(),
        case["ini"].as_str().unwrap()
    );
}
