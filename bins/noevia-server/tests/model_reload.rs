//! Real HTTP through the native front for `POST /api/models/presets/reload` (sbstndalton/noevia#2210).
//! Every scenario of tests/fixtures/model-maint/reload-cases.json was first run through Core's own
//! route and manager (record-reload.cjs, Core b6113b57, before the route moved) against a scripted
//! router and inference gate on one port; the front must give the same status and body text, send the
//! router and the gate the same requests, and leave the same models.ini and reload guard state file.
//! The legacy upstream is dead, so only the native handler can answer. Then the front's own
//! guarantees: the work outlives a client that leaves, a second reload while one runs is refused, a
//! hold has a deadline, is taken again when the gate loses it, and is given back when dropped.
//! Synthetic data only; no engine is touched.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]
mod maint_support;
use maint_support::*;
use noevia_server::model_maint::{HoldError, Maint};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

const CASES: &str = include_str!("fixtures/model-maint/reload-cases.json");
const TOKEN: &str = "0123456789abcdef0123456789abcdef";

/// The scripted engine of record-reload.cjs.
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
            if let Some(text) = router["iniOnReloadCheck"].as_str() {
                if calls.load(Ordering::SeqCst) == 2 {
                    std::fs::write(&ini, text).unwrap();
                }
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
        if req.method == "POST" && t == "/models/unload" {
            return match router.get("unload").filter(|u| !u.is_null()) {
                Some(u) => Answer::Reply(
                    u["status"].as_u64().unwrap() as u16,
                    u["text"].as_str().unwrap().to_owned(),
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

#[tokio::test]
async fn every_recorded_core_answer_is_the_fronts_byte_for_byte() {
    let cases: Value = serde_json::from_str(CASES).unwrap();
    let mut checked = 0;
    for case in cases["cases"].as_array().unwrap() {
        let name = case["name"].as_str().unwrap();
        let d = tempfile::tempdir().unwrap();
        let _db = seed_database(d.path());
        let ini = d.path().join("models.ini");
        std::fs::write(&ini, case["ini"].as_str().unwrap()).unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let (port, seen) = engine(scripted(case, ini.clone(), calls)).await;
        let base = format!("http://127.0.0.1:{port}/v1");
        let state = d
            .path()
            .join(format!("native-preset-reload-{}.json", suffix(&base)));
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
            ("LLAMACPP_PRESET_PATH".into(), ini.display().to_string()),
        ];
        // With no gate's hold, the front leases Node's own maintenance gate: Node's answer to that
        // was its own hold in the recording.
        let (node_port, _) = engine(Arc::new(|req: &Req| {
            if req.method == "POST" && req.target == "/internal/model-maintenance/lease" {
                Answer::Reply(201, json!({"lease": TOKEN, "ttlMs": 600_000, "renewMs": 200_000, "hardMs": 3_600_000}).to_string())
            } else {
                Answer::Reply(204, String::new())
            }
        }))
        .await;
        env.push((
            "NOEVIA_LEGACY_UPSTREAM".into(),
            format!("http://127.0.0.1:{node_port}"),
        ));
        env.push((
            "NOEVIA_PROJECT_STORE_TOKEN".into(),
            "synthetic-store-token-0123456789abcdef".into(),
        ));
        if case["gateToken"] == true {
            env.push((
                "INFERENCE_GATE_ADMIN_TOKEN".into(),
                "synthetic-admin-token".into(),
            ));
        }
        let (addr, stop) = start(d.path(), &env).await;
        let who = if case["request"]["role"] == "admin" {
            ADMIN
        } else {
            MEMBER
        };
        let (status, headers, body) = call(
            addr,
            "POST",
            "/api/models/presets/reload",
            Some(who),
            case["request"]["body"].as_str().unwrap().as_bytes(),
        )
        .await;
        let _ = stop.send(());
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
        let (router, gate) = lines(&seen.lock().unwrap());
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
            std::fs::read_to_string(&ini).unwrap(),
            expect["ini"].as_str().unwrap(),
            "{name}: models.ini"
        );
        assert_eq!(
            std::fs::read_to_string(&state).ok().as_deref(),
            expect["state"].as_str(),
            "{name}: the reload guard state file"
        );
        checked += 1;
    }
    assert_eq!(checked, cases["cases"].as_array().unwrap().len());
    assert!(
        checked >= 38,
        "every recorded scenario was replayed ({checked})"
    );
}

#[tokio::test]
async fn signed_out_get_and_other_methods_are_not_the_fronts() {
    let d = tempfile::tempdir().unwrap();
    let _db = seed_database(d.path());
    let (addr, stop) = start(
        d.path(),
        &[
            ("MODEL_MANAGER_KIND".into(), "llamacpp".into()),
            (
                "MODEL_MANAGER_BASE_URL".into(),
                "http://127.0.0.1:9/v1".into(),
            ),
        ],
    )
    .await;
    // Signed out: the front's own 401, before anything of the route.
    let (status, _, body) = call(addr, "POST", "/api/models/presets/reload", None, b"{}").await;
    assert_eq!(status, 401, "{body}");
    // Other methods go to Node, which is dead here: 502 from the proxy, not an answer of the front.
    for m in ["GET", "PUT", "DELETE"] {
        let (status, _, _) = call(addr, m, "/api/models/presets/reload", Some(ADMIN), b"").await;
        assert_eq!(status, 502, "{m}");
    }
    let _ = stop.send(());
}

fn running_ini(dir: &Path) -> PathBuf {
    let ini = dir.join("models.ini");
    std::fs::write(&ini, "[A]\nmodel = /models/a.gguf\n").unwrap();
    ini
}

/// A gate+router that answers a reload slowly: the reload request takes `reload_ms`.
fn slow_reload(reload_ms: u64) -> Handler {
    Arc::new(
        move |req: &Req| match (req.method.as_str(), req.target.as_str()) {
            ("POST", "/gate/v1/maintenance") => Answer::Reply(
                201,
                json!({"hold": TOKEN, "idle_ttl_ms": 600_000}).to_string(),
            ),
            ("DELETE", t) if t.starts_with("/gate/v1/") => Answer::Reply(204, String::new()),
            ("GET", "/models") => Answer::Reply(
                200,
                json!({"data": [{"id": "A", "status": {"value": "unloaded", "preset": "p"}}]})
                    .to_string(),
            ),
            ("GET", "/models?reload=1") => Answer::After(
                Duration::from_millis(reload_ms),
                200,
                r#"{"success":true}"#.into(),
            ),
            _ => Answer::Reply(404, "{}".into()),
        },
    )
}

#[tokio::test]
async fn a_client_that_leaves_strands_neither_the_reload_nor_its_hold() {
    let d = tempfile::tempdir().unwrap();
    let _db = seed_database(d.path());
    let ini = running_ini(d.path());
    let (port, seen) = engine(slow_reload(700)).await;
    let base = format!("http://127.0.0.1:{port}/v1");
    let state = d
        .path()
        .join(format!("native-preset-reload-{}.json", suffix(&base)));
    let (addr, stop) = start(
        d.path(),
        &[
            ("MODEL_MANAGER_KIND".into(), "llamacpp".into()),
            ("MODEL_MANAGER_BASE_URL".into(), base),
            ("LLAMACPP_PRESET_PATH".into(), ini.display().to_string()),
            (
                "INFERENCE_GATE_ADMIN_TOKEN".into(),
                "synthetic-admin-token".into(),
            ),
        ],
    )
    .await;
    // The client gives up after 200 ms, in the middle of the router's reload.
    let gone = tokio::time::timeout(
        Duration::from_millis(200),
        call(
            addr,
            "POST",
            "/api/models/presets/reload",
            Some(ADMIN),
            b"{}",
        ),
    )
    .await;
    assert!(gone.is_err(), "the client left before the answer");
    tokio::time::sleep(Duration::from_millis(1500)).await;
    let (router, gate) = lines(&seen.lock().unwrap());
    assert_eq!(
        gate,
        [
            "POST /gate/v1/maintenance {\"reason\":\"\",\"wait_ms\":0}",
            "DELETE /gate/v1/maintenance/T"
        ],
        "the hold was taken and given back"
    );
    assert_eq!(
        router.last().map(String::as_str),
        Some("GET /models hold"),
        "the work went on to its end"
    );
    let saved: Value = serde_json::from_str(&std::fs::read_to_string(&state).unwrap()).unwrap();
    assert_eq!(
        saved["text"], "[A]\nmodel = /models/a.gguf\n",
        "the baseline was recorded"
    );
    let _ = stop.send(());
}

#[tokio::test]
async fn a_second_reload_while_one_runs_is_refused_and_the_first_is_not_harmed() {
    let d = tempfile::tempdir().unwrap();
    let _db = seed_database(d.path());
    let ini = running_ini(d.path());
    let (port, _seen) = engine(slow_reload(800)).await;
    let (addr, stop) = start(
        d.path(),
        &[
            ("MODEL_MANAGER_KIND".into(), "llamacpp".into()),
            (
                "MODEL_MANAGER_BASE_URL".into(),
                format!("http://127.0.0.1:{port}/v1"),
            ),
            ("LLAMACPP_PRESET_PATH".into(), ini.display().to_string()),
            (
                "INFERENCE_GATE_ADMIN_TOKEN".into(),
                "synthetic-admin-token".into(),
            ),
        ],
    )
    .await;
    let first = tokio::spawn(call(
        addr,
        "POST",
        "/api/models/presets/reload",
        Some(ADMIN),
        b"{}",
    ));
    tokio::time::sleep(Duration::from_millis(250)).await;
    let (status, _, body) = call(
        addr,
        "POST",
        "/api/models/presets/reload",
        Some(ADMIN),
        b"{}",
    )
    .await;
    assert_eq!(
        (status, body.as_str()),
        (
            409,
            r#"{"error":"Requests are in progress. Try again when chats finish."}"#
        )
    );
    let (status, _, body) = first.await.unwrap();
    assert_eq!(
        (status, body.as_str()),
        (200, r#"{"reloaded":true,"unloaded":[]}"#)
    );
    // And once it ends, the next one runs.
    let (status, ..) = call(
        addr,
        "POST",
        "/api/models/presets/reload",
        Some(ADMIN),
        b"{}",
    )
    .await;
    assert_eq!(status, 200);
    let _ = stop.send(());
}

fn maint_for(port: u16, dir: &Path) -> Maint {
    let env: std::collections::HashMap<String, String> = [
        ("MODEL_MANAGER_KIND", "llamacpp".to_string()),
        (
            "MODEL_MANAGER_BASE_URL",
            format!("http://127.0.0.1:{port}/v1"),
        ),
        (
            "INFERENCE_GATE_ADMIN_TOKEN",
            "synthetic-admin-token".to_string(),
        ),
        ("UI_DATA_DIR", dir.display().to_string()),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_owned(), v))
    .collect();
    let get = |k: &str| env.get(k).cloned();
    let engine = Arc::new(server_stats::Engine::from_lookup(&get).unwrap());
    let sizing = Arc::new(noevia_server::model_sizing::Sizing::from_lookup(
        engine.kind(),
        Arc::clone(&engine),
        &get,
    ));
    Maint::from_lookup(engine.kind(), &engine, sizing, &get).unwrap()
}

fn gate_log(seen: &Arc<std::sync::Mutex<Vec<Req>>>) -> Vec<String> {
    lines(&seen.lock().unwrap()).1
}

#[tokio::test]
async fn a_hold_is_renewed_has_a_deadline_and_is_given_back_when_it_passes() {
    let d = tempfile::tempdir().unwrap();
    // idle_ttl 3 s: renewed every second.
    let handler: Handler = Arc::new(
        |req: &Req| match (req.method.as_str(), req.target.as_str()) {
            ("POST", "/gate/v1/maintenance") => {
                Answer::Reply(201, json!({"hold": TOKEN, "idle_ttl_ms": 3000}).to_string())
            }
            ("POST", t) if t.ends_with("/renew") => {
                Answer::Reply(200, r#"{"idle_ttl_ms":3000}"#.into())
            }
            ("DELETE", _) => Answer::Reply(204, String::new()),
            _ => Answer::Reply(404, "{}".into()),
        },
    );
    let (port, seen) = engine(handler).await;
    let m = maint_for(port, d.path());
    let hold = m
        .acquire("calibration", Duration::from_millis(2500), true)
        .await
        .unwrap();
    assert_eq!(hold.token().as_deref(), Some(TOKEN));
    assert!(m.local_held());
    assert_eq!(
        m.acquire("again", Duration::from_secs(1), true)
            .await
            .unwrap_err(),
        HoldError::Busy
    );
    tokio::time::sleep(Duration::from_millis(3300)).await;
    let log = gate_log(&seen);
    assert!(
        log.iter().any(|l| l == "POST /gate/v1/maintenance/T/renew"),
        "{log:?}"
    );
    assert_eq!(
        log.iter().filter(|l| l.starts_with("DELETE")).count(),
        1,
        "the deadline gave it back: {log:?}"
    );
    assert_eq!(
        hold.token(),
        None,
        "the overdue hold no longer claims the gate's token"
    );
    hold.release().await;
    assert!(!m.local_held(), "release clears the process's own flag");
    assert_eq!(
        gate_log(&seen)
            .iter()
            .filter(|l| l.starts_with("DELETE"))
            .count(),
        1,
        "nothing is given back twice"
    );
}

#[tokio::test]
async fn a_hold_the_gate_lost_is_taken_again_and_a_dropped_hold_is_given_back() {
    let d = tempfile::tempdir().unwrap();
    let grants = Arc::new(AtomicUsize::new(0));
    let g2 = Arc::clone(&grants);
    let handler: Handler = Arc::new(move |req: &Req| {
        match (req.method.as_str(), req.target.as_str()) {
        ("POST", "/gate/v1/maintenance") => {
            g2.fetch_add(1, Ordering::SeqCst);
            Answer::Reply(201, json!({"hold": TOKEN, "idle_ttl_ms": 3000}).to_string())
        }
        // The gate restarted: it does not know the hold any more.
        ("POST", t) if t.ends_with("/renew") => Answer::Reply(
            404,
            r#"{"error":{"type":"gate_error","code":"maintenance_hold_unknown","message":"gone","retryable":false}}"#.into(),
        ),
        ("DELETE", _) => Answer::Reply(204, String::new()),
        _ => Answer::Reply(404, "{}".into()),
    }
    });
    let (port, seen) = engine(handler).await;
    let m = maint_for(port, d.path());
    let hold = m
        .acquire("tune", Duration::from_secs(30), true)
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert_eq!(
        grants.load(Ordering::SeqCst),
        2,
        "the hold was taken again after the renewal found it gone"
    );
    assert!(hold.token().is_some());
    // A hold dropped without release (the task died) still gives the gate's token back.
    drop(hold);
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(
        gate_log(&seen).iter().any(|l| l.starts_with("DELETE")),
        "{:?}",
        gate_log(&seen)
    );
    assert!(!m.local_held());
}
