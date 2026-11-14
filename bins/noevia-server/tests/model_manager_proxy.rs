//! Track R11 (S9b): `/api/model-manager/*` through the whole front with `NOEVIA_RUST_MODEL_MANAGER`
//! unset (relayed to a synthetic Node as before) and `rust` (answered by the front over
//! crates/model-manager-proxy): a representative case of each guard family the crate's recording
//! holds, with a scripted model manager, a scripted router and inference gate on one port, and a
//! scripted Node answering the core bridges (the reads fence, the scan cache, the facts, the load
//! refusal, the model-state forget) and marking anything relayed. Synthetic QA data only; no engine
//! is touched.
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

const HOLD: &str = "0123456789abcdef0123456789abcdef";
const STORE_TOKEN: &str = "synthetic-store-token-0123456789abcdef";
const LOADER_TOKEN: &str = "qa-loader-token";
const SCAN_PATH: &str = "/api/model-manager/models";

/// The scan Node's cache holds (`hit.body`), or none.
type Scan = Arc<Mutex<Option<Value>>>;

/// What the scripted Node answers besides the scan.
#[derive(Clone, Copy)]
struct Knobs {
    publish_applied: bool,
    clear_ok: bool,
    sidecar_names: bool,
}

impl Default for Knobs {
    fn default() -> Self {
        Knobs {
            publish_applied: true,
            clear_ok: true,
            sidecar_names: true,
        }
    }
}

fn node(scan: Scan, knobs: Arc<Mutex<Knobs>>) -> Handler {
    Arc::new(move |req: &Req| {
        let t = req.target.as_str();
        let k = *knobs.lock().unwrap();
        match (req.method.as_str(), t) {
            ("POST", "/internal/auto-roles/invalidate") => Answer::Reply(200, "{}".into()),
            ("POST", "/internal/model-manager-scan/snapshot") => {
                let hit = scan
                    .lock()
                    .unwrap()
                    .clone()
                    .map_or(Value::Null, |b| json!({"at": 9000, "body": b}));
                Answer::Reply(
                    200,
                    json!({"token": "a".repeat(64), "entryToken": "b".repeat(64), "now": 10000, "hit": hit})
                        .to_string(),
                )
            }
            ("POST", "/internal/model-manager-scan/publish") => {
                Answer::Reply(200, json!({"applied": k.publish_applied}).to_string())
            }
            ("POST", "/internal/model-manager-scan/clear") if k.clear_ok => {
                Answer::Reply(200, r#"{"cleared":true}"#.into())
            }
            ("POST", "/internal/model-manager-scan/clear") => {
                Answer::Reply(503, r#"{"error":"Model scan cache is unavailable."}"#.into())
            }
            ("POST", "/internal/models/facts") if !req.body.contains("budget") && !k.sidecar_names => {
                Answer::Reply(200, json!({"sidecarLoaded": [], "scan": null}).to_string())
            }
            ("POST", "/internal/models/facts") if req.body.contains("budget") => {
                Answer::Reply(200, json!({"budget": {"budgetGib": 12}}).to_string())
            }
            ("POST", "/internal/models/facts") => Answer::Reply(
                200,
                json!({"sidecarNames": ["qa-embedder"], "sidecarLoaded": [], "scan": null})
                    .to_string(),
            ),
            ("POST", "/internal/model-maintenance/load-refusal") if req.body.contains("qa-big") => {
                Answer::Reply(
                    200,
                    json!({"refusal": {"error": "qa: does not fit", "code": "over_budget", "needGib": 20}})
                        .to_string(),
                )
            }
            ("POST", "/internal/model-maintenance/load-refusal") => {
                Answer::Reply(200, r#"{"refusal":null}"#.into())
            }
            ("POST", "/internal/model-state/forget") => Answer::Reply(200, "{}".into()),
            _ => Answer::Reply(202, r#"{"relayed":true}"#.into()),
        }
    })
}

/// The router lists two unloaded models, reloads and unloads; the gate grants the hold.
fn gate_and_router() -> Handler {
    Arc::new(|req: &Req| {
        match (req.method.as_str(), req.target.as_str()) {
        ("POST", "/gate/v1/maintenance") => {
            Answer::Reply(201, json!({"hold": HOLD, "idle_ttl_ms": 600_000}).to_string())
        }
        ("DELETE", t) if t.starts_with("/gate/v1/") => Answer::Reply(204, String::new()),
        ("GET", "/models") => Answer::Reply(
            200,
            json!({"data": [
                {"id": "qa-model", "status": {"value": "unloaded", "preset": "p", "args": ["--model", "/models/qa.gguf"]}},
                {"id": "qa-chat", "status": {"value": "unloaded", "preset": "c", "args": ["--model", "/models/qa-chat.gguf"]}}
            ]})
            .to_string(),
        ),
        ("GET", "/models?reload=1") => Answer::Reply(200, r#"{"success":true}"#.into()),
        ("POST", "/models/unload") => Answer::Reply(200, r#"{"success":true}"#.into()),
        _ => Answer::Reply(404, "{}".into()),
    }
    })
}

/// The model manager: what each test scripts by method and target.
fn loader(answers: Vec<(&'static str, Answer)>) -> Handler {
    let answers = Arc::new(Mutex::new(answers));
    Arc::new(move |req: &Req| {
        let key = format!("{} {}", req.method, req.target);
        let mut list = answers.lock().unwrap();
        match list.iter().position(|(k, _)| *k == key) {
            Some(i) => list.remove(i).1,
            None => Answer::Reply(200, r#"{"ok":true}"#.into()),
        }
    })
}

struct Rig {
    addr: std::net::SocketAddr,
    _stop: tokio::sync::oneshot::Sender<()>,
    _dir: tempfile::TempDir,
    dir: std::path::PathBuf,
    node: Arc<Mutex<Vec<Req>>>,
    engine: Arc<Mutex<Vec<Req>>>,
    loader: Arc<Mutex<Vec<Req>>>,
    scan: Scan,
    knobs: Arc<Mutex<Knobs>>,
    base: String,
}

async fn rig(switch: bool, loader_answers: Vec<(&'static str, Answer)>, with_loader: bool) -> Rig {
    let d = tempfile::tempdir().unwrap();
    let _db = seed_database(d.path());
    let scan: Scan = Arc::default();
    let knobs: Arc<Mutex<Knobs>> = Arc::default();
    let (node_port, node_seen) = engine(node(Arc::clone(&scan), Arc::clone(&knobs))).await;
    let (engine_port, engine_seen) = engine(gate_and_router()).await;
    let (loader_port, loader_seen) = engine(loader(loader_answers)).await;
    let ini = d.path().join("models.ini");
    std::fs::write(
        &ini,
        "[*]\nctx-size = 4096\n\n[qa-model]\nmodel = /models/qa.gguf\n\n[qa-chat]\nmodel = /models/qa-chat.gguf\n",
    )
    .unwrap();
    let models = d.path().join("models");
    std::fs::create_dir_all(&models).unwrap();
    std::fs::write(models.join("qa.gguf"), b"GGUF synthetic qa weights").unwrap();
    let base = format!("http://127.0.0.1:{engine_port}/v1");
    let mut env: Vec<(String, String)> = vec![
        (
            "NOEVIA_LEGACY_UPSTREAM".into(),
            format!("http://127.0.0.1:{node_port}"),
        ),
        ("NOEVIA_PROJECT_STORE_TOKEN".into(), STORE_TOKEN.into()),
        ("MODEL_MANAGER_KIND".into(), "llamacpp".into()),
        ("MODEL_MANAGER_BASE_URL".into(), base.clone()),
        ("LLAMACPP_PRESET_PATH".into(), ini.display().to_string()),
        ("LLAMACPP_MODELS_PATH".into(), models.display().to_string()),
        (
            "INFERENCE_GATE_ADMIN_TOKEN".into(),
            "synthetic-admin-token".into(),
        ),
    ];
    if with_loader {
        env.push((
            "MODEL_LOADER_URL".into(),
            format!("http://127.0.0.1:{loader_port}/"),
        ));
        env.push(("MODEL_LOADER_TOKEN".into(), LOADER_TOKEN.into()));
    }
    if switch {
        env.push(("NOEVIA_RUST_MODEL_MANAGER".into(), "rust".into()));
    }
    let (addr, stop) = start(d.path(), &env).await;
    Rig {
        addr,
        _stop: stop,
        dir: d.path().to_path_buf(),
        _dir: d,
        node: node_seen,
        engine: engine_seen,
        loader: loader_seen,
        scan,
        knobs,
        base,
    }
}

fn targets(seen: &Mutex<Vec<Req>>) -> Vec<String> {
    seen.lock()
        .unwrap()
        .iter()
        .map(|r| format!("{} {}", r.method, r.target))
        .collect()
}

fn relayed(r: &Rig) -> Vec<String> {
    targets(&r.node)
        .into_iter()
        .filter(|t| !t.contains(" /internal/"))
        .collect()
}

fn j(text: &str) -> Value {
    serde_json::from_str(text).unwrap_or(Value::Null)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn switch_unset_every_method_is_relayed_to_node() {
    let r = rig(false, vec![], true).await;
    for (method, path) in [
        ("GET", "/api/model-manager/benchmark/runs"),
        ("POST", "/api/model-manager/benchmark/start"),
        ("PUT", "/api/model-manager/sections/qa-model"),
        ("DELETE", "/api/model-manager/sections/qa-model"),
        ("POST", "/api/model-manager/models/delete"),
        ("PATCH", "/api/model-manager/a%2Fb"),
    ] {
        let (status, _, body) = call(r.addr, method, path, Some(ADMIN), b"{}").await;
        assert_eq!(status, 202, "{method} {path}: {body}");
        assert_eq!(j(&body)["relayed"], true);
    }
    assert_eq!(relayed(&r).len(), 6, "{:?}", relayed(&r));
    assert!(targets(&r.loader).is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn switch_on_the_sweep_start_is_relayed_and_the_scan_get_stays_the_scan_route() {
    let r = rig(
        true,
        vec![(
            "GET /api/v1/models",
            Answer::Reply(200, r#"{"models":[]}"#.into()),
        )],
        true,
    )
    .await;
    for path in [
        "/api/model-manager/benchmark/sweep",
        "/api/model-manager/benchmark/sw%65ep",
    ] {
        let (status, _, body) = call(
            r.addr,
            "POST",
            path,
            Some(ADMIN),
            br#"{ "aliases" : ["qa-a", "qa-b"], "n": 1.50 }"#,
        )
        .await;
        assert_eq!(status, 202, "{path}: {body}");
    }
    assert_eq!(
        relayed(&r),
        [
            "POST /api/model-manager/benchmark/sweep",
            "POST /api/model-manager/benchmark/sw%65ep"
        ]
    );
    // Core is sent the checked value, written again, with its own length.
    for q in r.node.lock().unwrap().iter() {
        assert_eq!(q.body, r#"{"aliases":["qa-a","qa-b"],"n":1.5}"#);
        assert_eq!(
            q.headers.get("content-length").map(String::as_str),
            Some("35")
        );
    }
    // A body core would read as {} (and forward raw) is refused here; nothing is relayed.
    let too_many =
        json!({"aliases": (0..65).map(|i| format!("qa-{i}")).collect::<Vec<_>>()}).to_string();
    for (body, error) in [
        (
            r#"{"aliases":{"qa-big":1}}"#.to_string(),
            "aliases must be an array.",
        ),
        (
            r#"{"aliases":["qa-big"],"x":NaN}"#.to_string(),
            "The request body must be a JSON object.",
        ),
        ("[]".to_string(), "The request body must be a JSON object."),
        (too_many, "Choose at most 64 models for one sweep."),
    ] {
        let (status, _, text) = call(
            r.addr,
            "POST",
            "/api/model-manager/benchmark/sweep",
            Some(ADMIN),
            body.as_bytes(),
        )
        .await;
        assert_eq!((status, j(&text)), (400, json!({"error": error})), "{body}");
    }
    assert_eq!(relayed(&r).len(), 2);
    // Without a session the gate answers; a member's start is core's to refuse, as it came.
    let (status, _, _) = call(
        r.addr,
        "POST",
        "/api/model-manager/benchmark/sweep",
        None,
        b"{}",
    )
    .await;
    assert_eq!(status, 401);
    let (status, _, _) = call(
        r.addr,
        "POST",
        "/api/model-manager/benchmark/sweep",
        Some(MEMBER),
        br#"{"aliases":{"x":1}}"#,
    )
    .await;
    assert_eq!(status, 202);
    assert_eq!(
        r.node.lock().unwrap().last().unwrap().body,
        r#"{"aliases":{"x":1}}"#
    );
    // Nothing of the flow ran: no fence, no load refusal, no hold, no manager call.
    assert!(targets(&r.node).iter().all(|t| !t.contains("/internal/")));
    assert!(targets(&r.loader).is_empty());
    assert!(targets(&r.engine).is_empty());
    // Exact GET models: the scan route (its snapshot, then the manager), not the proxy.
    let (status, headers, _) = call(r.addr, "GET", SCAN_PATH, Some(ADMIN), b"").await;
    assert_eq!(status, 200);
    // A cold cache: the scan route fetches the manager itself (no proxy timeout, no fence).
    assert!(!headers.contains("x-model-scan"), "{headers}");
    assert_eq!(targets(&r.loader), ["GET /api/v1/models"]);
    assert!(targets(&r.node).contains(&"POST /internal/model-manager-scan/snapshot".to_string()));
    assert!(!targets(&r.node).contains(&"POST /internal/auto-roles/invalidate".to_string()));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn path_role_and_configuration_checks() {
    let r = rig(true, vec![], true).await;
    for path in [
        "/api/model-manager/a%2Fb",
        "/api/model-manager/a%5Cb",
        "/api/model-manager/models/delete%0A",
        "/api/model-manager/models-ini%0A",
        "/api/model-manager/%zz",
        "/api/model-manager/%ED%A0%80",
        "/api/model-manager/a~b",
        "/api/model-manager/a..b",
    ] {
        let (status, _, body) = call(r.addr, "GET", path, Some(ADMIN), b"").await;
        assert_eq!(
            (status, j(&body)),
            (400, json!({"error": "Invalid path"})),
            "{path}"
        );
    }
    let (status, _, body) = call(r.addr, "POST", "/api/model-manager/x", Some(MEMBER), b"{}").await;
    assert_eq!(status, 403);
    assert_eq!(
        j(&body)["error"],
        "Administrator required for model management"
    );
    let (status, _, _) = call(r.addr, "GET", "/api/model-manager/x", None, b"").await;
    assert_eq!(status, 401);
    assert!(targets(&r.loader).is_empty());
    assert!(relayed(&r).is_empty());
    // A write drops core's shared reads before and after, even refused.
    let fences = targets(&r.node)
        .iter()
        .filter(|t| *t == "POST /internal/auto-roles/invalidate")
        .count();
    assert_eq!(fences, 2);

    let r = rig(true, vec![], false).await;
    let (status, _, body) = call(r.addr, "GET", "/api/model-manager/x", Some(ADMIN), b"").await;
    assert_eq!(status, 404);
    assert_eq!(
        j(&body)["error"],
        "Model management service is not configured"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn benchmark_start_refuses_non_chat_and_over_budget_models() {
    let r = rig(true, vec![], true).await;
    let (status, _, body) = call(
        r.addr,
        "POST",
        "/api/model-manager/benchmark/start",
        Some(ADMIN),
        br#"{"aliases":["qa-chat","qa-embed-x","laya-mini"]}"#,
    )
    .await;
    assert_eq!(status, 400, "{body}");
    assert_eq!(
        j(&body)["error"],
        "The prompt suite needs chat models; qa-embed-x, laya-mini are an embedding, reranking or routing model."
    );
    let (status, _, body) = call(
        r.addr,
        "POST",
        "/api/model-manager/benchmark/start",
        Some(ADMIN),
        br#"{"aliases":["qa-chat","qa-big"]}"#,
    )
    .await;
    assert_eq!(status, 409, "{body}");
    assert_eq!(
        j(&body),
        json!({"error": "qa: does not fit", "code": "over_budget", "needGib": 20})
    );
    let refusals: Vec<String> = r
        .node
        .lock()
        .unwrap()
        .iter()
        .filter(|q| q.target == "/internal/model-maintenance/load-refusal")
        .map(|q| q.body.clone())
        .collect();
    assert_eq!(refusals.len(), 2, "{refusals:?}");
    assert!(targets(&r.loader).is_empty());
    // A null body (core's TypeError, 500) is the named 400.
    let (status, _, body) = call(
        r.addr,
        "POST",
        "/api/model-manager/benchmark/start",
        Some(ADMIN),
        b"null",
    )
    .await;
    assert_eq!(
        (status, j(&body)),
        (
            400,
            json!({"error": "The request body must be a JSON object."})
        )
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_folder_scan_delete_is_guarded_then_cleaned_up() {
    let r = rig(
        true,
        vec![(
            "POST /api/v1/models/delete",
            Answer::Reply(200, r#"{"deleted":["k-chat"]}"#.into()),
        )],
        true,
    )
    .await;
    *r.scan.lock().unwrap() = Some(json!({"models": [
        {"key": "k-chat", "modelId": "qa-chat", "sections": ["qa-chat"]},
        {"key": "k-router", "modelId": "laya-qa", "sections": []},
        {"key": "k-embed", "modelId": "qa-embedder", "sections": []}
    ]}));
    let (status, _, body) = call(
        r.addr,
        "POST",
        "/api/model-manager/models/delete",
        Some(ADMIN),
        br#"{"models":["k-router"]}"#,
    )
    .await;
    assert_eq!(status, 400, "{body}");
    assert_eq!(j(&body)["errorId"], "systemModel");
    let (status, _, body) = call(
        r.addr,
        "POST",
        "/api/model-manager/models/delete",
        Some(ADMIN),
        br#"{"models":["k-embed"]}"#,
    )
    .await;
    assert_eq!(status, 409, "{body}");
    assert_eq!(
        j(&body),
        json!({"error": "A running sidecar (embedding or reranking) depends on this model — not deleted", "errorId": "modelInUse", "service": "embedding"})
    );
    assert!(targets(&r.loader).is_empty());

    let (status, _, body) = call(
        r.addr,
        "POST",
        "/api/model-manager/models/delete",
        Some(ADMIN),
        br#"{"models":["k-chat"]}"#,
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(
        j(&body),
        json!({"deleted": ["k-chat"], "unloaded": true, "rolesCleared": []})
    );
    let loader_seen = r.loader.lock().unwrap().clone();
    assert_eq!(loader_seen.len(), 1);
    assert_eq!(loader_seen[0].target, "/api/v1/models/delete");
    assert_eq!(loader_seen[0].body, r#"{"models":["k-chat"]}"#);
    assert_eq!(
        loader_seen[0]
            .headers
            .get("x-model-loader-token")
            .map(String::as_str),
        Some(LOADER_TOKEN)
    );
    let engine = targets(&r.engine);
    assert!(
        engine.contains(&"POST /models/unload".to_string()),
        "{engine:?}"
    );
    let node: Vec<Req> = r.node.lock().unwrap().clone();
    assert!(node
        .iter()
        .any(|q| q.target == "/internal/model-state/forget" && q.body.contains("qa-chat")));
    assert!(node
        .iter()
        .any(|q| q.target == "/internal/model-manager-scan/clear"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_section_put_is_checked_by_the_preset_refusal() {
    let r = rig(true, vec![], true).await;
    let (status, _, body) = call(
        r.addr,
        "PUT",
        "/api/model-manager/sections/qa-model",
        Some(ADMIN),
        br#"{"values":{"model":"/models/qa.gguf","batch-size":"512","ubatch-size":"4096"}}"#,
    )
    .await;
    assert_eq!(status, 400, "{body}");
    assert_eq!(
        j(&body),
        json!({"error": "Not saved: Micro batch cannot exceed batch size", "code": "invalid_size"})
    );
    let (status, _, body) = call(
        r.addr,
        "PUT",
        "/api/model-manager/sections/qa-model",
        Some(ADMIN),
        br#"{"values":{"constructor":5}}"#,
    )
    .await;
    assert_eq!(status, 400, "{body}");
    assert_eq!(
        j(&body)["error"],
        "Not saved: function Object() { [native code] } must be a text path"
    );
    assert!(targets(&r.loader).is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_models_ini_write_and_its_reload_run_under_one_hold() {
    let r = rig(
        true,
        vec![(
            "PUT /api/v1/models-ini",
            Answer::Reply(200, r#"{"preset":"written","revision":"r2"}"#.into()),
        )],
        true,
    )
    .await;
    let (status, _, body) = call(
        r.addr,
        "PUT",
        "/api/model-manager/models-ini",
        Some(ADMIN),
        br#"{"text":"[qa-model]\nmodel = /models/qa.gguf\n"}"#,
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(j(&body), json!({"preset": "written", "revision": "r2"}));
    let engine = targets(&r.engine);
    let at = |t: &str| {
        engine
            .iter()
            .position(|c| c == t)
            .unwrap_or_else(|| panic!("{t}: {engine:?}"))
    };
    assert!(
        at("POST /gate/v1/maintenance") < at("GET /models?reload=1"),
        "{engine:?}"
    );
    assert!(
        at("GET /models?reload=1") < at(&format!("DELETE /gate/v1/maintenance/{HOLD}")),
        "{engine:?}"
    );
    assert_eq!(targets(&r.loader), ["PUT /api/v1/models-ini"]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_forward_carries_the_token_and_the_sizing_budget() {
    let r = rig(
        true,
        vec![(
            "GET /api/v1/sections/qa-model/autoconfig?ctx=8192&budget_gib=12",
            Answer::Reply(200, r#"{"options":{"ctx-size":"8192"}}"#.into()),
        )],
        true,
    )
    .await;
    let (status, headers, body) = call(
        r.addr,
        "GET",
        "/api/model-manager/sections/qa-model/autoconfig?budget_gib=99&ctx=8192",
        Some(ADMIN),
        b"",
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert!(headers.contains("cache-control: no-store"));
    assert_eq!(j(&body), json!({"options": {"ctx-size": "8192"}}));
    let seen = r.loader.lock().unwrap().clone();
    assert_eq!(seen.len(), 1);
    assert_eq!(
        seen[0].target,
        "/api/v1/sections/qa-model/autoconfig?ctx=8192&budget_gib=12"
    );
    assert_eq!(
        seen[0]
            .headers
            .get("x-model-loader-token")
            .map(String::as_str),
        Some(LOADER_TOKEN)
    );
    assert_eq!(
        seen[0].headers.get("content-type").map(String::as_str),
        Some("application/json")
    );
    assert!(!seen[0].headers.contains_key("cookie"));
    // A GET does not fence the reads.
    assert!(!targets(&r.node).contains(&"POST /internal/auto-roles/invalidate".to_string()));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn no_answer_is_502_and_a_refusal_keeps_its_detail() {
    let r = rig(
        true,
        vec![
            ("GET /api/v1/status", Answer::Drop),
            (
                "POST /api/v1/qa",
                Answer::Reply(422, r#"{"detail":"qa: bad request"}"#.into()),
            ),
        ],
        true,
    )
    .await;
    let (status, _, body) =
        call(r.addr, "GET", "/api/model-manager/status", Some(ADMIN), b"").await;
    assert_eq!(
        (status, j(&body)),
        (
            502,
            json!({"error": "The model management service is not responding."})
        )
    );
    let (status, _, body) = call(
        r.addr,
        "POST",
        "/api/model-manager/qa",
        Some(ADMIN),
        br#"{"x":1}"#,
    )
    .await;
    assert_eq!(
        (status, j(&body)),
        (422, json!({"error": "qa: bad request"}))
    );
    let seen = r.loader.lock().unwrap().clone();
    assert_eq!(seen[1].body, r#"{"x":1}"#);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_finished_benchmark_run_becomes_throughput_evidence() {
    let now_s = noevia_server::identity::now_ms() as f64 / 1000.0;
    let run = json!({
        "run": {"id": 12, "status": "done", "finished_at": now_s - 30.0},
        "variants": [{"alias": "qa-model", "argv_json": "{}"}],
        "results": [
            {"alias": "qa-model", "gen_tps": 10.04, "prompt_tps": 100},
            {"alias": "qa-model", "gen_tps": 12.06, "prompt_tps": 120}
        ]
    });
    let r = rig(
        true,
        vec![(
            "GET /api/v1/benchmark/runs/12",
            Answer::Reply(200, run.to_string()),
        )],
        true,
    )
    .await;
    let (status, _, body) = call(
        r.addr,
        "GET",
        "/api/model-manager/benchmark/runs/12",
        Some(ADMIN),
        b"",
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(j(&body), run);
    let log = r
        .dir
        .join(format!("evidence-{}", suffix(&r.base)))
        .join("evidence.jsonl");
    let started = std::time::Instant::now();
    loop {
        if let Ok(text) = std::fs::read_to_string(&log) {
            if text.contains("model-manager-benchmark") {
                assert!(text.contains("\"rate\":11.1"), "{text}");
                break;
            }
        }
        assert!(
            started.elapsed() < Duration::from_secs(20),
            "no evidence record in {}",
            log.display()
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_delete_body_is_judged_as_forwarded() {
    let r = rig(true, vec![], true).await;
    *r.scan.lock().unwrap() = Some(json!({"models": [
        {"key": "k-router", "modelId": "laya-qa", "sections": []},
        {"key": "k-chat", "modelId": "qa-chat", "sections": []}
    ]}));
    for (body, error) in [
        (r#"{"models":{"k-router":0}}"#, "models must be an array."),
        (
            r#"{"models":["k-router"],"x":NaN}"#,
            "The request body must be a JSON object.",
        ),
        ("null", "The request body must be a JSON object."),
    ] {
        let (status, _, text) = call(
            r.addr,
            "POST",
            "/api/model-manager/models/delete",
            Some(ADMIN),
            body.as_bytes(),
        )
        .await;
        assert_eq!((status, j(&text)), (400, json!({"error": error})), "{body}");
    }
    assert!(targets(&r.loader).is_empty());
    // A duplicate key is judged as JSON.parse reads it, and forwarded that way.
    let (status, _, text) = call(
        r.addr,
        "POST",
        "/api/model-manager/models/delete",
        Some(ADMIN),
        br#"{ "models": ["k-router"], "models": ["k-chat"] }"#,
    )
    .await;
    assert_eq!(status, 200, "{text}");
    let seen = r.loader.lock().unwrap().clone();
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].body, r#"{"models":["k-chat"]}"#);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_cold_cache_scans_fresh_and_publishes_with_the_snapshot_token() {
    let scan = json!({"models": [{"key": "k-chat", "modelId": "qa-chat", "sections": []}]});
    let r = rig(
        true,
        vec![("GET /api/v1/models", Answer::Reply(200, scan.to_string()))],
        true,
    )
    .await;
    r.knobs.lock().unwrap().publish_applied = false;
    let (status, _, body) = call(
        r.addr,
        "POST",
        "/api/model-manager/models/delete",
        Some(ADMIN),
        br#"{"models":["k-chat"]}"#,
    )
    .await;
    // A refused publish changes nothing: the delete goes on, judged by the fresh scan.
    assert_eq!(status, 200, "{body}");
    assert_eq!(
        targets(&r.loader),
        ["GET /api/v1/models", "POST /api/v1/models/delete"]
    );
    let node: Vec<Req> = r.node.lock().unwrap().clone();
    let publish = node
        .iter()
        .find(|q| q.target == "/internal/model-manager-scan/publish")
        .unwrap();
    let ask = j(&publish.body);
    assert_eq!(ask["token"], "a".repeat(64));
    assert_eq!(j(ask["bodyJson"].as_str().unwrap()), scan);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_unconfirmed_scan_clear_is_not_forwarded() {
    let r = rig(true, vec![], true).await;
    r.knobs.lock().unwrap().clear_ok = false;
    let (status, _, body) = call(
        r.addr,
        "POST",
        "/api/model-manager/qa",
        Some(ADMIN),
        br#"{}"#,
    )
    .await;
    assert_eq!(
        (status, j(&body)),
        (
            502,
            json!({"error": "The model management service is not responding."})
        )
    );
    assert!(targets(&r.loader).is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_dropped_models_ini_forward_still_releases_the_hold() {
    let r = rig(true, vec![("PUT /api/v1/models-ini", Answer::Drop)], true).await;
    let (status, _, body) = call(
        r.addr,
        "PUT",
        "/api/model-manager/models-ini",
        Some(ADMIN),
        br#"{"text":""}"#,
    )
    .await;
    assert_eq!(status, 502, "{body}");
    let engine = targets(&r.engine);
    assert!(
        engine.contains(&"POST /gate/v1/maintenance".to_string()),
        "{engine:?}"
    );
    assert!(
        !engine.contains(&"GET /models?reload=1".to_string()),
        "{engine:?}"
    );
    assert!(
        engine.contains(&format!("DELETE /gate/v1/maintenance/{HOLD}")),
        "{engine:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn path_spellings_through_the_whole_front() {
    let r = rig(true, vec![], true).await;
    *r.scan.lock().unwrap() =
        Some(json!({"models": [{"key": "k-router", "modelId": "laya-qa", "sections": []}]}));
    let (status, _, _) = call(
        r.addr,
        "HEAD",
        "/api/model-manager/status",
        Some(ADMIN),
        b"",
    )
    .await;
    assert_eq!(status, 200);
    let (status, _, _) = call(
        r.addr,
        "GET",
        "/api/model-manager/status/",
        Some(ADMIN),
        b"",
    )
    .await;
    assert_eq!(status, 200);
    // WHATWG resolves the dot segments: this is the guarded delete.
    let (status, _, body) = call(
        r.addr,
        "POST",
        "/api/model-manager/x/%2e%2e/models/delete",
        Some(ADMIN),
        br#"{"models":["k-router"]}"#,
    )
    .await;
    assert_eq!(status, 400, "{body}");
    assert_eq!(j(&body)["errorId"], "systemModel");
    // An encoded spelling of the scan path is not the exact scan GET: the proxy forwards it.
    let (status, _, _) = call(
        r.addr,
        "GET",
        "/api/model-manager/m%6Fdels",
        Some(ADMIN),
        b"",
    )
    .await;
    assert_eq!(status, 200);
    assert_eq!(
        targets(&r.loader),
        [
            "HEAD /api/v1/status",
            "GET /api/v1/status/",
            "GET /api/v1/m%6Fdels"
        ]
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sidecar_and_protection_refusals() {
    let r = rig(true, vec![], true).await;
    *r.scan.lock().unwrap() =
        Some(json!({"models": [{"key": "k-chat", "modelId": "qa-chat", "sections": []}]}));
    let (status, _, body) = call(
        r.addr,
        "DELETE",
        "/api/model-manager/sections/qa-embedder",
        Some(ADMIN),
        b"",
    )
    .await;
    assert_eq!(status, 409, "{body}");
    assert_eq!(
        j(&body),
        json!({"error": "A running sidecar (embedding or reranking) depends on this model — not deleted", "errorId": "modelInUse", "service": "embedding"})
    );
    // Facts without sidecar names: protection is not current, nothing is deleted.
    r.knobs.lock().unwrap().sidecar_names = false;
    let (status, _, body) = call(
        r.addr,
        "POST",
        "/api/model-manager/models/delete",
        Some(ADMIN),
        br#"{"models":["k-chat"]}"#,
    )
    .await;
    assert_eq!(status, 409, "{body}");
    assert_eq!(
        j(&body),
        json!({"error": "Embedding model protection is not current.", "errorId": "modelInUse"})
    );
    assert!(targets(&r.loader).is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_body_over_one_mebibyte_is_413() {
    let r = rig(true, vec![], true).await;
    let big = vec![b'x'; 1024 * 1024 + 1];
    let (status, _, _) = call(r.addr, "POST", "/api/model-manager/qa", Some(ADMIN), &big).await;
    assert_eq!(status, 413);
    assert!(targets(&r.loader).is_empty());
}
