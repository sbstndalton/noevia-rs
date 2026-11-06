//! Real HTTP through the native front for `PUT /api/models/preset`, the preset editor (sbstndalton/noevia
//! D2). Every scenario of tests/fixtures/preset-editor/preset-editor-cases.json was first run through Core's
//! own route and manager (record-preset-editor.cjs, Core 3521cdc2, before the editor moved) against a
//! scripted router, inference gate and Model Loader on one port, for synthetic models.ini texts and
//! synthetic GGUF headers; the front must give the same status and body text, send the router, the gate and
//! the loader the same requests, and leave the same models.ini files and reload guard state file. The legacy
//! upstream is a synthetic Node that only leases its gate, answers the budget and takes the cache-drop
//! call, so nothing but the native handler can answer. Then the front's own guarantees: the gates, a
//! client that leaves, a lease that is lost, a budget Node does not answer, a second apply, an engine that
//! cannot be reached. Synthetic data only; no engine is touched.
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
use std::sync::{Arc, Mutex};
use std::time::Duration;

const CASES: &str = include_str!("fixtures/preset-editor/preset-editor-cases.json");
const TOKEN: &str = "0123456789abcdef0123456789abcdef";
const STORE: &str = "synthetic-store-token-0123456789abcdef";
const PATH: &str = "/api/models/preset";

fn gguf(kv: &[(&str, u32, Value)]) -> Vec<u8> {
    let u32b = |n: u32| n.to_le_bytes().to_vec();
    let u64b = |n: u64| n.to_le_bytes().to_vec();
    let s = |t: &str| {
        let mut v = u64b(t.len() as u64);
        v.extend_from_slice(t.as_bytes());
        v
    };
    let mut out = b"GGUF".to_vec();
    out.extend(u32b(3));
    out.extend(u64b(0));
    out.extend(u64b(kv.len() as u64));
    for (k, t, v) in kv {
        out.extend(s(k));
        out.extend(u32b(*t));
        match t {
            4 => out.extend(u32b(v.as_u64().unwrap() as u32)),
            8 => out.extend(s(v.as_str().unwrap())),
            _ => panic!("type"),
        }
    }
    out
}

/// The header record-preset-editor.cjs writes.
fn hybrid() -> Vec<u8> {
    gguf(&[
        ("general.architecture", 8, json!("qwen35")),
        ("qwen35.context_length", 4, json!(262_144)),
        ("qwen35.embedding_length", 4, json!(4096)),
        ("qwen35.block_count", 4, json!(32)),
        ("qwen35.attention.head_count", 4, json!(16)),
        ("qwen35.attention.head_count_kv", 4, json!(4)),
        ("qwen35.attention.key_length", 4, json!(256)),
        ("qwen35.attention.value_length", 4, json!(256)),
        ("qwen35.full_attention_interval", 4, json!(4)),
        ("qwen35.ssm.state_size", 4, json!(128)),
        ("tokenizer.chat_template", 8, json!("{{ messages }}")),
    ])
}

fn sha(t: &str) -> String {
    hex(t)
}

#[derive(Default)]
struct Counts {
    listings: AtomicUsize,
    reloads: AtomicUsize,
    loader: AtomicUsize,
}

/// The scripted engine of record-preset-editor.cjs: router, gate and Model Loader.
fn scripted(case: &Value, ini: PathBuf, counts: Arc<Counts>) -> Handler {
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
        if t == "/api/v1/models-ini" && req.method == "PUT" {
            let modes = case["loader"].as_array().cloned().unwrap_or_default();
            let n = counts.loader.fetch_add(1, Ordering::SeqCst);
            let mode = modes
                .get(n.min(modes.len().saturating_sub(1)))
                .and_then(Value::as_str)
                .unwrap_or("ok")
                .to_owned();
            let asked: Value = serde_json::from_str(&req.body).unwrap();
            let text = asked["text"].as_str().unwrap().to_owned();
            let current = std::fs::read_to_string(&ini).unwrap();
            return match mode.as_str() {
                "ok" => {
                    if sha(&current) != asked["baseRevision"].as_str().unwrap() {
                        return Answer::Reply(409, r#"{"ok":false}"#.into());
                    }
                    std::fs::write(&ini, &text).unwrap();
                    Answer::Reply(200, json!({"ok": true, "revision": sha(&text)}).to_string())
                }
                "lost" => {
                    std::fs::write(&ini, &text).unwrap();
                    Answer::Drop
                }
                "lost500" => {
                    std::fs::write(&ini, &text).unwrap();
                    Answer::Reply(500, r#"{"ok":false}"#.into())
                }
                "drop" => Answer::Drop,
                "thirdparty" => {
                    std::fs::write(&ini, format!("{text}; third party\n")).unwrap();
                    Answer::Drop
                }
                "weird" => Answer::Reply(200, r#"{"ok":"maybe"}"#.into()),
                "400" => Answer::Reply(400, json!({"detail": "bad text ".repeat(40)}).to_string()),
                other => Answer::Reply(other.parse().unwrap(), r#"{"ok":false}"#.into()),
            };
        }
        if req.method == "GET" && t == "/models" {
            let listings = router["listings"].as_array().unwrap();
            let l = &listings[counts
                .listings
                .fetch_add(1, Ordering::SeqCst)
                .min(listings.len() - 1)];
            let wait = router["delay"]["listing"].as_u64().unwrap_or(0);
            if l["drop"] == true {
                return Answer::Drop;
            }
            let body = match l.get("text").and_then(Value::as_str) {
                Some(text) => text.to_owned(),
                None => json!({"data": l["rows"]}).to_string(),
            };
            let status = l["status"].as_u64().unwrap() as u16;
            return if wait > 0 {
                Answer::After(Duration::from_millis(wait), status, body)
            } else {
                Answer::Reply(status, body)
            };
        }
        if req.method == "GET" && t == "/models?reload=1" {
            let n = counts.reloads.fetch_add(1, Ordering::SeqCst);
            if n == 0 {
                if let Some(text) = router["iniOnReload"].as_str() {
                    std::fs::write(&ini, text).unwrap();
                }
            }
            let list = router["reload"].as_array().cloned().unwrap_or_default();
            let wait = router["delay"]["reload"].as_u64().unwrap_or(0);
            let r = list.get(n.min(list.len().saturating_sub(1)));
            if r.is_some_and(|r| r["drop"] == true) {
                return Answer::Drop;
            }
            let (status, text) = match r {
                Some(r) => (
                    r["status"].as_u64().unwrap() as u16,
                    r["text"].as_str().unwrap().to_owned(),
                ),
                None => (200, r#"{"success":true}"#.into()),
            };
            return if wait > 0 {
                Answer::After(Duration::from_millis(wait), status, text)
            } else {
                Answer::Reply(status, text)
            };
        }
        if req.method == "POST" && t == "/models/unload" {
            return Answer::Reply(200, r#"{"success":true}"#.into());
        }
        Answer::Reply(404, "{}".into())
    })
}

/// The synthetic Node: it leases its gate, answers the budget and takes the cache-drop call.
fn node(budget: Value, mode: Arc<Mutex<&'static str>>) -> Handler {
    Arc::new(move |req: &Req| {
        let token_ok = req.headers.get("x-noevia-store-token").map(String::as_str) == Some(STORE);
        match (req.method.as_str(), req.target.as_str()) {
            ("POST", "/internal/model-maintenance/lease") => match *mode.lock().unwrap() {
                "busy" => Answer::Reply(409, "{}".into()),
                "down" => Answer::Drop,
                _ => Answer::Reply(
                    201,
                    json!({"lease": TOKEN, "ttlMs": 600_000, "renewMs": 200_000, "hardMs": 3_600_000}).to_string(),
                ),
            },
            (_, t) if t.starts_with("/internal/model-maintenance/lease") => {
                Answer::Reply(200, "{}".into())
            }
            ("POST", "/internal/models/facts") if token_ok => {
                if *mode.lock().unwrap() == "budget-down" {
                    return Answer::Drop;
                }
                Answer::Reply(200, json!({"budget": {"budgetGib": budget}}).to_string())
            }
            ("POST", "/internal/preset-editor/effects") if token_ok => {
                Answer::Reply(200, r#"{"ok":true}"#.into())
            }
            _ => Answer::Reply(599, r#"{"error":"unexpected Node executor"}"#.into()),
        }
    })
}

fn lines(seen: &[Req]) -> (Vec<String>, Vec<String>, Vec<String>) {
    let (mut router, mut gate, mut loader) = (Vec::new(), Vec::new(), Vec::new());
    for r in seen {
        // Asked only to tell a gate without an admin token from no gate at all (#2210).
        if r.method == "GET" && r.target == "/gate/v1/status" {
            continue;
        }
        if r.target.starts_with("/gate/v1/") {
            if r.method == "POST"
                && r.target == "/gate/v1/maintenance"
                && serde_json::from_str::<Value>(&r.body)
                    .ok()
                    .is_some_and(|v| v["reason"] == "Reconciling interrupted native preset editing")
            {
                continue;
            }
            let mut norm = r.target.replace(TOKEN, "T");
            if r.method == "POST" && norm == "/gate/v1/maintenance" {
                // Private additive protocol fields carry the exact hold and renewal budget.
                // The Node oracle still compares the existing public reason/wait request.
                let mut body: Value = serde_json::from_str(&r.body).unwrap();
                body.as_object_mut().unwrap().remove("idle_ttl_ms");
                body.as_object_mut().unwrap().remove("maintenance");
                norm = format!("{norm} {body}");
            }
            gate.push(format!("{} {norm}", r.method));
        } else if r.target == "/api/v1/models-ini" {
            let mut body: Value = serde_json::from_str(&r.body).unwrap();
            body.as_object_mut().unwrap().remove("maintenance");
            loader.push(format!("PUT {} {body}", r.target));
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
    (router, gate, loader)
}

fn strings(v: &Value) -> Vec<String> {
    v.as_array()
        .unwrap()
        .iter()
        .map(|s| s.as_str().unwrap().to_owned())
        .collect()
}

fn files_of(dir: &Path) -> serde_json::Map<String, Value> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.starts_with("models.ini"))
        .collect();
    names.sort();
    names
        .into_iter()
        .map(|n| {
            let text = std::fs::read_to_string(dir.join(&n)).unwrap();
            (n, Value::String(text))
        })
        .collect()
}

struct Rig {
    addr: std::net::SocketAddr,
    stop: tokio::sync::oneshot::Sender<()>,
    dir: tempfile::TempDir,
    ini: PathBuf,
    state: PathBuf,
    seen: Arc<Mutex<Vec<Req>>>,
    node_seen: Arc<Mutex<Vec<Req>>>,
}

async fn rig(case: &Value, node_mode: Arc<Mutex<&'static str>>) -> Rig {
    rig_with(case, node_mode, None).await
}

/// `wrap` puts a handler in front of the scripted engine (a gate that forgets its hold, say).
async fn rig_with(
    case: &Value,
    node_mode: Arc<Mutex<&'static str>>,
    wrap: Option<Box<dyn Fn(Handler) -> Handler>>,
) -> Rig {
    let d = tempfile::tempdir().unwrap();
    let _db = seed_database(d.path());
    let ini = d.path().join("models.ini");
    std::fs::write(&ini, case["ini"].as_str().unwrap()).unwrap();
    if case["gguf"] == true {
        std::fs::write(d.path().join("q.gguf"), hybrid()).unwrap();
    }
    if let Some(roles) = case["roles"].as_str() {
        std::fs::write(d.path().join("models.roles.json"), roles).unwrap();
    }
    let counts = Arc::new(Counts::default());
    let scripted_handler = scripted(case, ini.clone(), counts);
    let handler = match wrap {
        Some(w) => w(scripted_handler),
        None => scripted_handler,
    };
    let (port, seen) = engine(handler).await;
    let base = format!("http://127.0.0.1:{port}/v1");
    let state = d
        .path()
        .join(format!("native-preset-reload-{}.json", suffix(&base)));
    match &case["state"] {
        Value::Null => {}
        Value::String(s) => std::fs::write(&state, s).unwrap(),
        other => std::fs::write(&state, other.to_string()).unwrap(),
    }
    let (node_port, node_seen) = engine(node(case["budget"].clone(), node_mode)).await;
    let mut env: Vec<(String, String)> = vec![
        (
            "MODEL_MANAGER_KIND".into(),
            case["kind"].as_str().unwrap().into(),
        ),
        ("MODEL_MANAGER_BASE_URL".into(), base),
        (
            "NOEVIA_LEGACY_UPSTREAM".into(),
            format!("http://127.0.0.1:{node_port}"),
        ),
        ("NOEVIA_PROJECT_STORE_TOKEN".into(), STORE.into()),
    ];
    if case["presetPath"] == true {
        env.push(("LLAMACPP_PRESET_PATH".into(), ini.display().to_string()));
    }
    if case["mount"] == true {
        env.push((
            "LLAMACPP_MODELS_PATH".into(),
            d.path().display().to_string(),
        ));
    }
    if case["writer"] == "loader" {
        env.push(("MODELS_INI_WRITER".into(), "model-loader".into()));
        env.push((
            "MODEL_LOADER_URL".into(),
            format!("http://127.0.0.1:{port}"),
        ));
        env.push(("MODEL_LOADER_TOKEN".into(), "synthetic-loader-token".into()));
    }
    if case["gateToken"] == true {
        env.push((
            "INFERENCE_GATE_ADMIN_TOKEN".into(),
            "synthetic-admin-token".into(),
        ));
    }
    let (addr, stop) = start(d.path(), &env).await;
    Rig {
        addr,
        stop,
        dir: d,
        ini,
        state,
        seen,
        node_seen,
    }
}

async fn put(addr: std::net::SocketAddr, who: &str, body: &str) -> (u16, String, String) {
    call(addr, "PUT", PATH, Some(who), body.as_bytes()).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn every_recorded_core_answer_is_the_fronts_byte_for_byte() {
    let cases: Value = serde_json::from_str(CASES).unwrap();
    let mut checked = 0;
    for case in cases["cases"].as_array().unwrap() {
        let name = case["name"].as_str().unwrap();
        let mode = Arc::new(Mutex::new("ok"));
        // This oracle compares the completed public transaction. Fresh recovery is a separate
        // protocol obligation now, verified by the settlement tests; refuse its new hold here
        // so its asynchronous reread cannot change the transaction's recorded file/state trace.
        let r = rig_with(
            case,
            mode,
            Some(Box::new(|inner: Handler| {
                Arc::new(move |req: &Req| {
                    if req.method == "POST"
                        && req.target == "/gate/v1/maintenance"
                        && serde_json::from_str::<Value>(&req.body)
                            .ok()
                            .is_some_and(|v| {
                                v["reason"] == "Reconciling interrupted native preset editing"
                            })
                    {
                        return Answer::Reply(409, "{}".into());
                    }
                    inner(req)
                })
            })),
        )
        .await;
        let who = if case["request"]["role"] == "admin" {
            ADMIN
        } else {
            MEMBER
        };
        let rev = sha(case["ini"].as_str().unwrap());
        let body = case["request"]["body"]
            .as_str()
            .unwrap()
            .replace("$REV", &rev);
        let expect = &case["expect"];
        let answers = expect["answers"].as_array().unwrap();
        let got: Vec<(u16, String)> = if case["concurrent"] == true {
            let addr = r.addr;
            let b = body.clone();
            let first = tokio::spawn(async move { put(addr, ADMIN, &b).await });
            tokio::time::sleep(Duration::from_millis(200)).await;
            let second = put(r.addr, who, &body).await;
            let first = first.await.unwrap();
            vec![(first.0, first.2), (second.0, second.2)]
        } else {
            let (status, headers, text) = put(r.addr, who, &body).await;
            assert!(headers.contains("content-type: application/json"), "{name}");
            vec![(status, text)]
        };
        tokio::time::sleep(Duration::from_millis(200)).await;
        for (i, (status, text)) in got.iter().enumerate() {
            assert_eq!(
                (*status, text.as_str()),
                (
                    answers[i]["status"].as_u64().unwrap() as u16,
                    answers[i]["body"].as_str().unwrap()
                ),
                "{name}: answer {i}"
            );
        }
        let (router, gate, loader) = lines(&r.seen.lock().unwrap());
        assert_eq!(
            router,
            strings(&expect["router"]),
            "{name}: the router was asked"
        );
        // Core's gate observer asks the gate's status on its own timer (inference-maintenance.cjs); that is not
        // part of an apply, and the front's own status probe is dropped by `lines` the same way.
        let recorded_gate: Vec<String> = strings(&expect["gate"])
            .into_iter()
            .filter(|l| l != "GET /gate/v1/status")
            .collect();
        assert_eq!(gate, recorded_gate, "{name}: the gate was asked");
        assert_eq!(
            loader,
            strings(&expect["loader"]),
            "{name}: the loader was asked"
        );
        assert_eq!(
            Value::Object(files_of(r.dir.path())),
            expect["files"],
            "{name}: the models.ini files"
        );
        assert_eq!(
            std::fs::read_to_string(&r.state).ok().as_deref(),
            expect["state"].as_str(),
            "{name}: the reload guard state file"
        );
        // Nothing but the lease, the budget and the cache drop was asked of Node.
        for s in r.node_seen.lock().unwrap().iter() {
            assert!(
                s.target.starts_with("/internal/model-maintenance/lease")
                    || s.target == "/internal/models/facts"
                    || s.target == "/internal/preset-editor/effects",
                "{name}: Node was asked {}",
                s.target
            );
        }
        let _ = r.stop.send(());
        checked += 1;
    }
    assert_eq!(checked, cases["cases"].as_array().unwrap().len());
    assert!(
        checked >= 100,
        "every recorded scenario was replayed ({checked})"
    );
}

fn base_case() -> Value {
    let cases: Value = serde_json::from_str(CASES).unwrap();
    cases["cases"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["name"] == "one option changes and the router reloads")
        .unwrap()
        .clone()
}

fn good_body(case: &Value) -> String {
    case["request"]["body"]
        .as_str()
        .unwrap()
        .replace("$REV", &sha(case["ini"].as_str().unwrap()))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_gates_come_before_any_work() {
    let case = base_case();
    let r = rig(&case, Arc::new(Mutex::new("ok"))).await;
    let body = good_body(&case);
    // Signed out: the front's own 401.
    let (status, _, text) = call(r.addr, "PUT", PATH, None, body.as_bytes()).await;
    assert_eq!(status, 401, "{text}");
    // A member: 403 before the body is read.
    let (status, _, text) = put(r.addr, MEMBER, &body).await;
    assert_eq!(
        (status, text.as_str()),
        (403, r#"{"error":"administrator required"}"#)
    );
    // No CSRF token: refused by the session gate.
    let mut s = tokio::net::TcpStream::connect(r.addr).await.unwrap();
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    s.write_all(
        format!(
            "PUT {PATH} HTTP/1.1\r\nHost: {}\r\nConnection: close\r\nCookie: {ADMIN}\r\nContent-Length: {}\r\n\r\n{body}",
            r.addr,
            body.len()
        )
        .as_bytes(),
    )
    .await
    .unwrap();
    let mut out = String::new();
    s.read_to_string(&mut out).await.unwrap();
    assert!(out.starts_with("HTTP/1.1 403"), "{out}");
    // Other methods are not the front's: they go to Node (the synthetic one answers 599 to anything it
    // does not know), and no editor work starts.
    for m in ["POST", "DELETE", "PATCH"] {
        let (status, _, _) = call(r.addr, m, PATH, Some(ADMIN), b"{}").await;
        assert_eq!(status, 599, "{m}");
    }
    assert!(
        r.seen.lock().unwrap().is_empty(),
        "the engine was not asked"
    );
    assert!(
        !r.node_seen
            .lock()
            .unwrap()
            .iter()
            .any(|s| s.target.starts_with("/internal/model-maintenance/lease")),
        "no hold was taken"
    );
    assert_eq!(
        std::fs::read_to_string(&r.ini).unwrap(),
        case["ini"].as_str().unwrap()
    );
    let _ = r.stop.send(());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_oversized_body_is_413_and_nothing_is_held() {
    let case = base_case();
    let r = rig(&case, Arc::new(Mutex::new("ok"))).await;
    let big = format!("{{\"pad\":\"{}\"}}", "x".repeat(1024 * 1024 + 10));
    let (status, _, text) = put(r.addr, ADMIN, &big).await;
    assert_eq!(
        (status, text.as_str()),
        (413, r#"{"error":"Request exceeds size limit"}"#)
    );
    assert!(r.seen.lock().unwrap().is_empty());
    let _ = r.stop.send(());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_node_that_cannot_lease_its_gate_changes_nothing() {
    let case = {
        let mut c = base_case();
        c["gate"] = json!("absent");
        c
    };
    for (mode, status) in [("busy", 409), ("down", 503)] {
        let r = rig(&case, Arc::new(Mutex::new(mode))).await;
        let (s, _, text) = put(r.addr, ADMIN, &good_body(&case)).await;
        assert_eq!(s, status, "{mode}: {text}");
        let (router, _, _) = lines(&r.seen.lock().unwrap());
        assert!(
            router.is_empty(),
            "{mode}: the engine was not asked: {router:?}"
        );
        assert_eq!(
            std::fs::read_to_string(&r.ini).unwrap(),
            case["ini"].as_str().unwrap()
        );
        let _ = r.stop.send(());
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_budget_node_does_not_answer_refuses_closed_and_writes_nothing() {
    let mut case = base_case();
    for (k, v) in [
        ("mount", json!(true)),
        ("gguf", json!(true)),
        ("budget", json!(3)),
    ] {
        case[k] = v;
    }
    let r = rig(&case, Arc::new(Mutex::new("budget-down"))).await;
    let (status, _, text) = put(r.addr, ADMIN, &good_body(&case)).await;
    assert_eq!(
        (status, text.as_str()),
        (
            503,
            r#"{"error":"The inference memory budget is unavailable right now. Try again shortly."}"#
        )
    );
    assert_eq!(
        std::fs::read_to_string(&r.ini).unwrap(),
        case["ini"].as_str().unwrap()
    );
    let (router, gate, _) = lines(&r.seen.lock().unwrap());
    assert!(!router.iter().any(|l| l.contains("reload=1")), "{router:?}");
    assert!(
        gate.iter().any(|l| l.starts_with("DELETE")),
        "the hold was given back"
    );
    let _ = r.stop.send(());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_client_that_leaves_strands_neither_the_apply_nor_its_hold() {
    let mut case = base_case();
    case["router"]["delay"] = json!({"reload": 700});
    let r = rig(&case, Arc::new(Mutex::new("ok"))).await;
    let addr = r.addr;
    let body = good_body(&case);
    let task = tokio::spawn(async move { put(addr, ADMIN, &body).await });
    // The client gives up once the router has been asked to reload.
    for _ in 0..200 {
        if r.seen
            .lock()
            .unwrap()
            .iter()
            .any(|s| s.target == "/models?reload=1")
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    task.abort();
    // The work finishes: the file is the new one, the hold is given back.
    for _ in 0..200 {
        if r.seen
            .lock()
            .unwrap()
            .iter()
            .any(|s| s.method == "DELETE" && s.target.starts_with("/gate/v1/maintenance"))
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let (router, gate, _) = lines(&r.seen.lock().unwrap());
    assert_eq!(
        router.iter().filter(|l| l.contains("reload=1")).count(),
        1,
        "{router:?}"
    );
    assert!(
        gate.iter()
            .any(|l| l.starts_with("DELETE /gate/v1/maintenance/")),
        "{gate:?}"
    );
    assert!(std::fs::read_to_string(&r.ini)
        .unwrap()
        .contains("ctx-size = 8192"));
    // A new apply is not refused: nothing is stranded.
    let fresh = sha(&std::fs::read_to_string(&r.ini).unwrap());
    let body = json!({"model": "m", "baseRevision": fresh, "confirmReload": true, "options": {"ctx-size": "4096"}}).to_string();
    let (status, _, text) = put(r.addr, ADMIN, &body).await;
    assert_eq!(status, 200, "{text}");
    let _ = r.stop.send(());
}

/// Abort the request at each await the apply waits on: the work runs to its end, the file is either
/// the old one or the new one with the reload done, and the hold is given back.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn leaving_at_each_wait_still_ends_in_a_consistent_state() {
    for (stage, delay) in [
        ("first listing", json!({"listing": 400})),
        ("reload", json!({"reload": 400})),
    ] {
        let mut case = base_case();
        case["router"]["delay"] = delay;
        let r = rig(&case, Arc::new(Mutex::new("ok"))).await;
        let addr = r.addr;
        let body = good_body(&case);
        let task = tokio::spawn(async move { put(addr, ADMIN, &body).await });
        for _ in 0..200 {
            if !r.seen.lock().unwrap().is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        task.abort();
        for _ in 0..300 {
            if r.seen
                .lock()
                .unwrap()
                .iter()
                .any(|s| s.method == "DELETE" && s.target.starts_with("/gate/v1/maintenance"))
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        let (router, gate, _) = lines(&r.seen.lock().unwrap());
        assert!(
            gate.iter()
                .any(|l| l.starts_with("DELETE /gate/v1/maintenance/")),
            "{stage}: {gate:?}"
        );
        let text = std::fs::read_to_string(&r.ini).unwrap();
        let reloaded = router.iter().any(|l| l.contains("reload=1"));
        assert!(
            text.contains("ctx-size = 8192") && reloaded,
            "{stage}: the apply ran to its end ({router:?})"
        );
        let _ = r.stop.send(());
    }
}

/// A lease on Node's gate that is no longer sure to outlast a step stops that step. Node grants a short
/// lease it never renews: the commit does not start (nothing changed), or the reload does not start
/// (the previous file is put back, the engine is not asked).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_lost_lease_stops_the_destructive_steps() {
    let mut case = base_case();
    case["gate"] = json!("absent");
    // A lease whose hard limit is too short to cover a reload (120 s) and the margin.
    let d = tempfile::tempdir().unwrap();
    let _db = seed_database(d.path());
    let ini = d.path().join("models.ini");
    std::fs::write(&ini, case["ini"].as_str().unwrap()).unwrap();
    let counts = Arc::new(Counts::default());
    let (port, seen) = engine(scripted(&case, ini.clone(), counts)).await;
    let (node_port, _) = engine(Arc::new(|req: &Req| {
        if req.method == "POST" && req.target == "/internal/model-maintenance/lease" {
            Answer::Reply(
                201,
                json!({"lease": TOKEN, "ttlMs": 20_000, "renewMs": 5_000, "hardMs": 20_000})
                    .to_string(),
            )
        } else {
            Answer::Reply(200, "{}".into())
        }
    }))
    .await;
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
                "NOEVIA_LEGACY_UPSTREAM".into(),
                format!("http://127.0.0.1:{node_port}"),
            ),
            ("NOEVIA_PROJECT_STORE_TOKEN".into(), STORE.into()),
        ],
    )
    .await;
    let (status, _, text) = put(addr, ADMIN, &good_body(&case)).await;
    assert_eq!(status, 503, "{text}");
    assert!(text.contains("nothing was changed"), "{text}");
    assert_eq!(
        std::fs::read_to_string(&ini).unwrap(),
        case["ini"].as_str().unwrap()
    );
    let (router, _, _) = lines(&seen.lock().unwrap());
    assert!(!router.iter().any(|l| l.contains("reload=1")), "{router:?}");
    let _ = stop.send(());
}

#[test]
fn exact_method_dispatch() {
    use noevia_server::routes::{dispatch, Native};
    assert_eq!(dispatch("PUT", PATH), Some(Native::Models));
    assert_eq!(dispatch("GET", PATH), Some(Native::Models));
    for m in ["POST", "DELETE", "PATCH", "HEAD"] {
        assert_eq!(dispatch(m, PATH), None, "{m}");
    }
    assert_eq!(dispatch("PUT", "/api/models/preset/"), None);
    assert_eq!(dispatch("PUT", "/api/models/preset/suggest"), None);
    assert!(noevia_server::FEATURES.contains(&"rust-preset-editor-d2-v1"));
}

// ── D2 review: exclusion that is lost stops every later destructive step ────────────────────────────

/// A gate that grants `idle_ttl_ms` 3000 holds (renewed every second) and answers the renewals as `renew`:
/// "forgotten" (404), "dropped" (no answer) or "ok". After a forgotten renewal the next hold is a new token.
fn lossy_gate(renew: &'static str) -> Box<dyn Fn(Handler) -> Handler> {
    Box::new(move |inner: Handler| {
        let grants = Arc::new(AtomicUsize::new(0));
        Arc::new(move |req: &Req| {
            let t = req.target.as_str();
            if req.method == "POST" && t == "/gate/v1/maintenance" {
                let n = grants.fetch_add(1, Ordering::SeqCst);
                let token = format!("{n:032x}");
                return Answer::Reply(201, json!({"hold": token, "idle_ttl_ms": 3000}).to_string());
            }
            if req.method == "POST" && t.ends_with("/renew") {
                return match renew {
                    "forgotten" if t.contains(&format!("{:032x}", 0)) => {
                        Answer::Reply(404, "{}".into())
                    }
                    "dropped" => Answer::Drop,
                    _ => Answer::Reply(200, r#"{"idle_ttl_ms":3000}"#.into()),
                };
            }
            if t.starts_with("/gate/v1/") {
                return Answer::Reply(
                    if req.method == "DELETE" { 204 } else { 200 },
                    String::new(),
                );
            }
            inner(req)
        })
    })
}

fn gate_case(listing_ms: u64, reload_ms: u64, reload_status: u64) -> Value {
    let mut case = base_case();
    case["gate"] = json!("grant");
    case["router"]["delay"] = json!({"listing": listing_ms, "reload": reload_ms});
    if reload_status != 200 {
        case["router"]["reload"] = json!([{"status": reload_status, "text": "no"}]);
    }
    case
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_gate_that_forgot_the_hold_during_preparation_stops_the_commit_and_the_reload() {
    // The first listing takes 2.5 s; the renewal at 1 s learns that the gate forgot the hold.
    let case = gate_case(2500, 0, 200);
    let r = rig_with(
        &case,
        Arc::new(Mutex::new("ok")),
        Some(lossy_gate("forgotten")),
    )
    .await;
    let (status, _, text) = put(r.addr, ADMIN, &good_body(&case)).await;
    assert_eq!(status, 503, "{text}");
    assert!(text.contains("nothing was changed"), "{text}");
    assert_eq!(
        std::fs::read_to_string(&r.ini).unwrap(),
        case["ini"].as_str().unwrap()
    );
    let (router, gate, _) = lines(&r.seen.lock().unwrap());
    assert!(!router.iter().any(|l| l.contains("reload=1")), "{router:?}");
    // Release the original grant; never reacquire to continue work after coverage was lost.
    assert!(
        gate.iter().filter(|l| l.starts_with("DELETE")).count() >= 1,
        "{gate:?}"
    );
    assert!(
        gate.iter()
            .filter(|l| l.starts_with("POST /gate/v1/maintenance {"))
            .count()
            == 1,
        "lost work must not reacquire a replacement grant: {gate:?}"
    );
    assert_eq!(
        files_of(r.dir.path()).len(),
        1,
        "no recovery copy: no write started"
    );
    let _ = r.stop.send(());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn renewals_that_stop_being_confirmed_stop_the_commit_and_the_reload() {
    let case = gate_case(2600, 0, 200);
    let r = rig_with(
        &case,
        Arc::new(Mutex::new("ok")),
        Some(lossy_gate("dropped")),
    )
    .await;
    let (status, _, text) = put(r.addr, ADMIN, &good_body(&case)).await;
    assert_eq!(status, 503, "{text}");
    assert_eq!(
        std::fs::read_to_string(&r.ini).unwrap(),
        case["ini"].as_str().unwrap()
    );
    let (router, _, _) = lines(&r.seen.lock().unwrap());
    assert!(!router.iter().any(|l| l.contains("reload=1")), "{router:?}");
    let _ = r.stop.send(());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_hold_the_gate_keeps_renewing_does_not_get_in_the_way() {
    let case = gate_case(1500, 0, 200);
    let r = rig_with(&case, Arc::new(Mutex::new("ok")), Some(lossy_gate("ok"))).await;
    let (status, _, text) = put(r.addr, ADMIN, &good_body(&case)).await;
    assert_eq!(status, 200, "{text}");
    let _ = r.stop.send(());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_failed_reload_after_the_gate_forgot_the_hold_does_not_restore_unfenced() {
    failed_reload_without_coverage(false).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_failed_reload_with_competing_file_changes_reports_uncertainty() {
    failed_reload_without_coverage(true).await;
}

async fn failed_reload_without_coverage(competing: bool) {
    // The reload takes 2.5 s and fails; the renewal at 1 s learns that the exclusion is gone. The restore is a
    // write, so it does not start: the new settings stay, the answer says so, no second recovery copy.
    let mut case = gate_case(0, 2500, 500);
    if competing {
        case["router"]["iniOnReload"] = json!("[other]\nmodel = /models/other.gguf\n");
    }
    let r = rig_with(
        &case,
        Arc::new(Mutex::new("ok")),
        Some(lossy_gate("forgotten")),
    )
    .await;
    let (status, _, text) = put(r.addr, ADMIN, &good_body(&case)).await;
    assert_eq!(status, 503, "{text}");
    assert_eq!(text, json!({"error": "Reload failed or timed out, and restoring the previous preset file could not be confirmed. Stop inference and inspect native presets before retrying."}).to_string());
    let now = std::fs::read_to_string(&r.ini).unwrap();
    if competing {
        assert_eq!(now, case["router"]["iniOnReload"].as_str().unwrap());
    } else {
        assert!(now.contains("ctx-size = 8192"));
    }
    let (router, _, _) = lines(&r.seen.lock().unwrap());
    assert_eq!(
        router.iter().filter(|l| l.contains("reload=1")).count(),
        1,
        "{router:?}"
    );
    assert_eq!(
        files_of(r.dir.path()).len(),
        2,
        "one recovery copy, taken by the first write"
    );
    let _ = r.stop.send(());
}

/// Node's lease is sure to cover the commit but not the reload: the file stays as saved, nothing restores it
/// and the engine is not asked, for the web writer and for the model-loader alike.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_lease_lost_after_the_commit_neither_reloads_nor_restores() {
    for writer in ["web", "loader"] {
        let mut case = base_case();
        case["gate"] = json!("absent");
        case["writer"] = json!(writer);
        let d = tempfile::tempdir().unwrap();
        let _db = seed_database(d.path());
        let ini = d.path().join("models.ini");
        std::fs::write(&ini, case["ini"].as_str().unwrap()).unwrap();
        let counts = Arc::new(Counts::default());
        let (port, seen) = engine(scripted(&case, ini.clone(), counts)).await;
        let (node_port, _) = engine(Arc::new(|req: &Req| {
            if req.method == "POST" && req.target == "/internal/model-maintenance/lease" {
                Answer::Reply(
                    201,
                    json!({"lease": TOKEN, "ttlMs": 100_000, "renewMs": 50_000, "hardMs": 100_000})
                        .to_string(),
                )
            } else {
                Answer::Reply(200, "{}".into())
            }
        }))
        .await;
        let mut env = vec![
            ("MODEL_MANAGER_KIND".to_owned(), "llamacpp".to_owned()),
            (
                "MODEL_MANAGER_BASE_URL".to_owned(),
                format!("http://127.0.0.1:{port}/v1"),
            ),
            ("LLAMACPP_PRESET_PATH".to_owned(), ini.display().to_string()),
            (
                "NOEVIA_LEGACY_UPSTREAM".to_owned(),
                format!("http://127.0.0.1:{node_port}"),
            ),
            ("NOEVIA_PROJECT_STORE_TOKEN".to_owned(), STORE.to_owned()),
        ];
        if writer == "loader" {
            env.push(("MODELS_INI_WRITER".into(), "model-loader".into()));
            env.push((
                "MODEL_LOADER_URL".into(),
                format!("http://127.0.0.1:{port}"),
            ));
        }
        let (addr, stop) = start(d.path(), &env).await;
        let (status, _, text) = put(addr, ADMIN, &good_body(&case)).await;
        assert_eq!(status, 503, "{writer}: {text}");
        assert_eq!(text, json!({"error": "The model service stopped confirming that chat is held back after the preset file was saved, so the engine was not asked to reload and the previous preset file was not put back. The current models.ini settings could not be confirmed; inspect native presets, then reload them once chat is idle."}).to_string(), "{writer}");
        let now = std::fs::read_to_string(&ini).unwrap();
        assert!(
            now.contains("ctx-size = 8192"),
            "{writer}: the new settings stay\n{now}"
        );
        let (router, _, loader) = lines(&seen.lock().unwrap());
        assert!(
            !router.iter().any(|l| l.contains("reload=1")),
            "{writer}: {router:?}"
        );
        if writer == "loader" {
            assert_eq!(loader.len(), 1, "no second (restoring) write: {loader:?}");
        } else {
            assert_eq!(files_of(d.path()).len(), 2, "web: one recovery copy only");
        }
        let _ = stop.send(());
    }
}

// ── D2 review: the acquisition is cancellation-safe ──────────────────────────────────────────────────

fn maint_with(port: u16, node_port: Option<u16>, dir: &Path) -> noevia_server::model_maint::Maint {
    let mut env: std::collections::HashMap<String, String> = [
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
        ("NOEVIA_PROJECT_STORE_TOKEN", STORE.to_string()),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_owned(), v))
    .collect();
    if let Some(n) = node_port {
        env.insert(
            "NOEVIA_LEGACY_UPSTREAM".into(),
            format!("http://127.0.0.1:{n}"),
        );
    }
    let get = |k: &str| env.get(k).cloned();
    let engine = Arc::new(server_stats::Engine::from_lookup(&get).unwrap());
    let sizing = Arc::new(noevia_server::model_sizing::Sizing::from_lookup(
        engine.kind(),
        Arc::clone(&engine),
        &get,
    ));
    noevia_server::model_maint::Maint::from_lookup(engine.kind(), &engine, sizing, &get).unwrap()
}

async fn until(what: &str, mut ok: impl FnMut() -> bool) {
    for _ in 0..200 {
        if ok() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("{what}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_cancelled_gate_acquisition_frees_the_reservation_and_gives_the_grant_back() {
    let d = tempfile::tempdir().unwrap();
    let (port, seen) = engine(Arc::new(|req: &Req| {
        match (req.method.as_str(), req.target.as_str()) {
            ("POST", "/gate/v1/maintenance") => Answer::After(
                Duration::from_millis(600),
                201,
                json!({"hold": TOKEN, "idle_ttl_ms": 60_000}).to_string(),
            ),
            ("DELETE", _) => Answer::Reply(204, String::new()),
            _ => Answer::Reply(404, "{}".into()),
        }
    }))
    .await;
    let m = Arc::new(maint_with(port, None, d.path()));
    let task = {
        let m = Arc::clone(&m);
        tokio::spawn(async move {
            m.acquire("", Duration::from_secs(60), false)
                .await
                .map(|_| ())
        })
    };
    tokio::time::sleep(Duration::from_millis(150)).await;
    task.abort();
    let _ = task.await;
    // The grant arrives for nobody: it is given back, and the local flag is free again.
    until("the late grant is given back", || {
        seen.lock().unwrap().iter().any(|r| r.method == "DELETE")
    })
    .await;
    until("the local reservation is free", || !m.local_held()).await;
    let again = m.acquire("", Duration::from_secs(60), false).await;
    assert!(
        again.is_ok(),
        "a later job is not Busy for ever: {:?}",
        again.err()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_cancelled_node_lease_acquisition_frees_the_reservation_and_gives_the_lease_back() {
    let d = tempfile::tempdir().unwrap();
    let (port, _) = engine(Arc::new(|req: &Req| {
        if req.target == "/gate/v1/maintenance" && req.method == "POST" {
            Answer::Reply(404, r#"{"error":"not found"}"#.into())
        } else {
            Answer::Reply(404, "{}".into())
        }
    }))
    .await;
    let (node_port, node_seen) = engine(Arc::new(|req: &Req| {
        if req.method == "POST" && req.target == "/internal/model-maintenance/lease" {
            Answer::After(
                Duration::from_millis(600),
                201,
                json!({"lease": TOKEN, "ttlMs": 60_000, "renewMs": 20_000, "hardMs": 600_000})
                    .to_string(),
            )
        } else {
            Answer::Reply(200, "{}".into())
        }
    }))
    .await;
    let m = Arc::new(maint_with(port, Some(node_port), d.path()));
    let task = {
        let m = Arc::clone(&m);
        tokio::spawn(async move {
            m.acquire("", Duration::from_secs(60), false)
                .await
                .map(|_| ())
        })
    };
    tokio::time::sleep(Duration::from_millis(200)).await;
    task.abort();
    let _ = task.await;
    until("the late lease is given back", || {
        node_seen.lock().unwrap().iter().any(|r| {
            r.method == "DELETE" && r.target.starts_with("/internal/model-maintenance/lease/")
        })
    })
    .await;
    until("the local reservation is free", || !m.local_held()).await;
}
