//! #2286: the native image recheck over synthetic router/admission HTTP only. No real engine.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]
mod maint_support;
use maint_support::*;
use serde_json::{json, Value};
use std::{
    path::PathBuf,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::io::AsyncWriteExt;
const PATH: &str = "/api/models/evidence/recheck";
const SECRET: &str = "synthetic-store-token-0123456789abcdef";
const BODY: &[u8] = br#"{"model":"fx","category":"vision"}"#;
#[derive(Clone)]
struct State {
    status: u16,
    body: String,
    delay: Duration,
    change: bool,
    ttl: u64,
    renew: u16,
    admission: u16,
}
impl Default for State {
    fn default() -> Self {
        Self {
            status: 200,
            body: "{}".into(),
            delay: Duration::ZERO,
            change: false,
            ttl: 30000,
            renew: 200,
            admission: 201,
        }
    }
}
struct World {
    dir: tempfile::TempDir,
    addr: std::net::SocketAddr,
    _stop: tokio::sync::oneshot::Sender<()>,
    state: Arc<Mutex<State>>,
    router: Arc<Mutex<Vec<Req>>>,
    node: Arc<Mutex<Vec<Req>>>,
    log: PathBuf,
}
impl World {
    async fn new(kind: &str) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let _db = seed_database(dir.path());
        let models = dir.path().join("models");
        std::fs::create_dir_all(models.join("fx")).unwrap();
        std::fs::write(models.join("fx/fx.gguf"), vec![7u8; 2048]).unwrap();
        let ini = dir.path().join("models.ini");
        std::fs::write(&ini, "[fx]\nmodel=/models/fx/fx.gguf\nctx-size=4096\n").unwrap();
        let state = Arc::new(Mutex::new(State::default()));
        let (s, p) = (Arc::clone(&state), ini.clone());
        let (port,router)=engine(Arc::new(move |req|match req.target.as_str(){
            "/models"=>Answer::Reply(200,json!({"data":[{"id":"fx","status":{"value":"loaded","args":["--model","/models/fx/fx.gguf"]}}]}).to_string()),
            "/props"=>Answer::Reply(200,json!({"build_info":"synthetic"}).to_string()),
            "/v1/chat/completions"=>{let st=s.lock().unwrap().clone();if st.change{std::fs::write(&p,"[fx]\nmodel=/models/fx/fx.gguf\nctx-size=8192\n").unwrap();}Answer::After(st.delay,st.status,st.body)},
            _=>Answer::Reply(404,"{}".into()),
        })).await;
        let s = Arc::clone(&state);
        let (node_port,node)=engine(Arc::new(move |req|{
            let st=s.lock().unwrap().clone();
            if req.method=="DELETE" {return Answer::Reply(204,String::new());}
            if req.target.ends_with("/renew"){return Answer::Reply(st.renew,"{}".into());}
            if req.target=="/internal/inference-admission/lease" {
                return Answer::Reply(st.admission,if st.admission==201 {json!({"lease":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","ttlMs":st.ttl,"headers":{"x-noevia-lease":"syntheticLease1"}}).to_string()} else {json!({"error":"Synthetic budget or maintenance refusal"}).to_string()});
            }
            Answer::Reply(404,"{}".into())
        })).await;
        let base = format!("http://127.0.0.1:{port}/v1");
        let log = dir
            .path()
            .join(format!("evidence-{}", suffix(&base)))
            .join("evidence.jsonl");
        let (addr, stop) = start(
            dir.path(),
            &[
                ("MODEL_MANAGER_KIND".into(), kind.into()),
                ("MODEL_MANAGER_BASE_URL".into(), base.clone()),
                ("INFERENCE_BASE_URL".into(), base),
                ("INFERENCE_API_KEY".into(), "synthetic-key".into()),
                ("LLAMACPP_PRESET_PATH".into(), ini.display().to_string()),
                ("LLAMACPP_MODELS_PATH".into(), models.display().to_string()),
                (
                    "NOEVIA_LEGACY_UPSTREAM".into(),
                    format!("http://127.0.0.1:{node_port}"),
                ),
                ("NOEVIA_PROJECT_STORE_TOKEN".into(), SECRET.into()),
            ],
        )
        .await;
        Self {
            dir,
            addr,
            _stop: stop,
            state,
            router,
            node,
            log,
        }
    }
    fn records(&self) -> Vec<Value> {
        std::fs::read_to_string(&self.log)
            .unwrap_or_default()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect()
    }
    async fn post(&self, body: &[u8], cookie: Option<&str>) -> (u16, Value) {
        let (s, _, b) = call(self.addr, "POST", PATH, cookie, body).await;
        (s, serde_json::from_str(&b).unwrap())
    }
    async fn probing(&self) {
        let end = tokio::time::Instant::now() + Duration::from_secs(2);
        while !self
            .router
            .lock()
            .unwrap()
            .iter()
            .any(|r| r.target == "/v1/chat/completions")
        {
            assert!(tokio::time::Instant::now() < end);
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }
}
#[tokio::test]
async fn identity_role_method_engine_and_body_refusals_precede_any_probe() {
    let w = World::new("llamacpp").await;
    assert_eq!(w.post(BODY, None).await.0, 401);
    assert_eq!(
        w.post(BODY, Some(MEMBER)).await,
        (403, json!({"error":"administrator required"}))
    );
    assert_eq!(
        w.post(BODY, Some("cowork_session=admin-token; cowork_csrf=wrong"))
            .await
            .0,
        403
    );
    for raw in [
        "{",
        "null",
        "[]",
        "{}",
        r#"{"model":1}"#,
        r#"{"model":""}"#,
        r#"{"model":"\ud800","category":"vision"}"#,
    ] {
        assert_eq!(
            w.post(raw.as_bytes(), Some(ADMIN)).await,
            (400, json!({"error":"Choose a model"}))
        );
    }
    let long = json!({"model":"😀".repeat(101),"category":"vision"}).to_string();
    assert_eq!(w.post(long.as_bytes(), Some(ADMIN)).await.0, 400);
    assert_eq!(
        w.post(
            br#"{"model":"fx","category":"context_capacity"}"#,
            Some(ADMIN)
        )
        .await,
        (
            400,
            json!({"error":"Only image input can be rechecked here; use Measure context for context capacity."})
        )
    );
    assert!(w.node.lock().unwrap().is_empty());
    assert!(w.router.lock().unwrap().is_empty());
    assert!(w.records().is_empty());
    let other = World::new("none").await;
    assert_eq!(
        other.post(b"bad JSON", Some(ADMIN)).await,
        (
            404,
            json!({"error":"Qualification evidence needs the native engine"})
        )
    );
    assert!(other.node.lock().unwrap().is_empty());
}
#[tokio::test]
async fn accepted_pixel_is_recorded_under_its_measured_identity_with_normal_admission_headers() {
    let w = World::new("llamacpp").await;
    let (s, body) = w.post(BODY, Some(ADMIN)).await;
    assert_eq!(s, 200, "{body}");
    let row = body["categories"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["category"] == "vision")
        .unwrap();
    assert_eq!(row["state"], "verified");
    assert_eq!(
        row["limitations"],
        json!(["1×1 image accepted; not an accuracy test"])
    );
    let records = w.records();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0]["result"], "passed");
    assert_eq!(records[0]["source"], "recheck");
    assert_eq!(
        records[0]["suite"],
        json!({"name":"vision-probe","version":1})
    );
    assert_eq!(records[0]["identityHash"], body["identityHash"]);
    let router = w.router.lock().unwrap();
    let probe = router
        .iter()
        .find(|r| r.target == "/v1/chat/completions")
        .unwrap();
    assert_eq!(probe.method, "POST");
    assert_eq!(probe.headers["authorization"], "Bearer synthetic-key");
    assert_eq!(probe.headers["x-noevia-lease"], "syntheticLease1");
    let payload: Value = serde_json::from_str(&probe.body).unwrap();
    assert_eq!(payload["model"], "fx");
    assert_eq!(payload["max_tokens"], 1);
    assert!(payload.get("stream").is_none());
    assert_eq!(
        payload["messages"][0]["content"][0],
        json!({"type":"text","text":"ok"})
    );
    assert_eq!(payload["messages"][0]["content"][1]["image_url"]["url"],"data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg==");
    let node = w.node.lock().unwrap();
    assert!(node[0].target == "/internal/inference-admission/lease");
    assert_eq!(node[0].headers["x-noevia-store-token"], SECRET);
    assert!(!node[0].headers.contains_key("x-noevia-client-ip"));
    assert!(!node[0].headers.contains_key("x-forwarded-for"));
    assert_eq!(
        serde_json::from_str::<Value>(&node[0].body).unwrap(),
        json!({"model":"fx"})
    );
    assert!(node.iter().any(|r| r.method == "DELETE"));
}
#[tokio::test]
async fn missing_projector_records_failed_while_other_probe_or_admission_errors_never_qualify() {
    let w = World::new("llamacpp").await;
    {
        let mut s = w.state.lock().unwrap();
        s.status = 400;
        s.body = "missing MMPROJ file".into();
    }
    let (status, body) = w.post(BODY, Some(ADMIN)).await;
    assert_eq!(status, 200);
    assert_eq!(w.records()[0]["result"], "failed");
    let row = body["categories"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["category"] == "vision")
        .unwrap();
    assert_eq!(row["state"], "failed");
    let bad = World::new("llamacpp").await;
    bad.state.lock().unwrap().status = 500;
    let (s, b) = bad.post(BODY, Some(ADMIN)).await;
    assert_eq!(s, 503);
    assert!(b["error"].as_str().unwrap().contains("HTTP 500"));
    assert!(bad.records().is_empty());
    let refused = World::new("llamacpp").await;
    refused.state.lock().unwrap().admission = 409;
    assert_eq!(
        refused.post(BODY, Some(ADMIN)).await,
        (
            503,
            json!({"error":"Synthetic budget or maintenance refusal"})
        )
    );
    assert!(refused.router.lock().unwrap().is_empty());
    assert!(refused.records().is_empty());
}
#[tokio::test]
async fn lease_loss_and_changed_configuration_never_file_the_probes_result() {
    let w = World::new("llamacpp").await;
    {
        let mut s = w.state.lock().unwrap();
        s.ttl = 150;
        s.renew = 404;
        s.delay = Duration::from_millis(800);
    }
    let started = tokio::time::Instant::now();
    let (s, b) = w.post(BODY, Some(ADMIN)).await;
    assert_eq!(s, 503);
    assert!(b["error"].as_str().unwrap().contains("lease ended"));
    assert!(started.elapsed() < Duration::from_millis(600));
    assert!(w.records().is_empty());
    assert!(w.node.lock().unwrap().iter().any(|r| r.method == "DELETE"));
    let changed = World::new("llamacpp").await;
    changed.state.lock().unwrap().change = true;
    let (s, b) = changed.post(BODY, Some(ADMIN)).await;
    assert_eq!(s, 503);
    assert!(b["error"]
        .as_str()
        .unwrap()
        .contains("configuration changed"));
    assert!(changed.records().is_empty());
}
#[tokio::test]
async fn bounded_detached_work_survives_browser_disconnect_and_releases_its_lease() {
    let w = World::new("llamacpp").await;
    w.state.lock().unwrap().delay = Duration::from_millis(250);
    let mut stream = tokio::net::TcpStream::connect(w.addr).await.unwrap();
    let header=format!("POST {PATH} HTTP/1.1\r\nHost: {}\r\nCookie: {ADMIN}\r\nOrigin: http://{}\r\nX-CSRF-Token: csrf\r\nContent-Length: {}\r\n\r\n",w.addr,w.addr,BODY.len());
    stream.write_all(header.as_bytes()).await.unwrap();
    stream.write_all(BODY).await.unwrap();
    w.probing().await;
    assert_eq!(
        w.post(BODY, Some(ADMIN)).await.0,
        503,
        "only one detached image recheck may run"
    );
    drop(stream);
    let end = tokio::time::Instant::now() + Duration::from_secs(2);
    while w.records().is_empty() {
        assert!(tokio::time::Instant::now() < end);
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(w.records()[0]["result"], "passed");
    while !w.node.lock().unwrap().iter().any(|r| r.method == "DELETE") {
        assert!(tokio::time::Instant::now() < end);
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(w.dir.path().exists());
}
