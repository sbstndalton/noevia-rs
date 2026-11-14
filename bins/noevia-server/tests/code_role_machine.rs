//! Council p6: the role engine's machine pin through the front's own adapters (code_routes.rs
//! `FrontRoles` on a `code_machine::MachineRole`) against a synthetic inference machine on
//! loopback. The Planner preparation and the review run on the Code role's machine as core's do
//! (index.cjs `codeRoleEndpoint` on a machine + chat-machines.cjs `ready`, `fetch`,
//! `connectError`): pinned on the machine's model, the call sent there with no credential, the
//! refusals with Node's codes and texts (role-engine.cjs, chat-machines-1840.test.cjs), and a
//! Code role that moved during the task refused at the next call.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]
use axum::{
    extract::State,
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
    routing::{get, post},
    Router,
};
use engine_admission::machines::Target;
use js_json::{wtf16, JValue};
use noevia_server::code_machine::{self, FrontMachines, MachineRole, TargetReader, ROLE_MOVED};
use noevia_server::code_routes::FrontRoles;
use role_engine::{CallRequest, PinRequest, RoleEngine};
use std::sync::{Arc, Mutex};

const ANSWER: &str = r#"{"answer":"QA plan"}"#;

#[derive(Default)]
struct Machine {
    health: Mutex<u16>,
    served: Mutex<Vec<&'static str>>,
    /// Each chat completion: its Authorization header and its body.
    seen: Mutex<Vec<(Option<String>, String)>>,
}

async fn health(State(m): State<Arc<Machine>>) -> impl IntoResponse {
    StatusCode::from_u16(*m.health.lock().unwrap()).unwrap()
}

async fn models(State(m): State<Arc<Machine>>) -> impl IntoResponse {
    let data: Vec<_> = m
        .served
        .lock()
        .unwrap()
        .iter()
        .map(|id| serde_json::json!({ "id": id }))
        .collect();
    (
        [("content-type", "application/json")],
        serde_json::json!({ "data": data }).to_string(),
    )
}

async fn completions(
    State(m): State<Arc<Machine>>,
    headers: HeaderMap,
    body: String,
) -> impl IntoResponse {
    let auth = headers
        .get("authorization")
        .map(|v| v.to_str().unwrap().to_string());
    m.seen.lock().unwrap().push((auth, body));
    let delta = serde_json::json!({ "choices": [{ "delta": { "content": ANSWER } }] });
    let stop = serde_json::json!({ "choices": [{ "delta": {}, "finish_reason": "stop" }] });
    (
        [("content-type", "text/event-stream")],
        format!("data: {delta}\n\ndata: {stop}\n\ndata: [DONE]\n\n"),
    )
}

async fn machine() -> (Arc<Machine>, u16) {
    let m = Arc::new(Machine {
        health: Mutex::new(200),
        served: Mutex::new(vec!["qa-coder"]),
        ..Machine::default()
    });
    let app = Router::new()
        .route("/health", get(health))
        .route("/v1/models", get(models))
        .route("/v1/chat/completions", post(completions))
        .with_state(Arc::clone(&m));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (m, port)
}

fn target(port: u16, id: &str) -> Target {
    Target {
        role: "code".into(),
        machine_id: id.into(),
        machine_name: "QA Box".into(),
        port: u32::from(port),
        model: "qa-coder".into(),
        base_url: format!("http://127.0.0.1:{port}"),
    }
}

/// A role engine on `t`, with where the Code role is "now" read from `now`.
fn engine(t: &Target, now: Arc<Mutex<Option<Target>>>) -> RoleEngine<FrontRoles> {
    let reader: TargetReader = Arc::new(move |owner: &str| {
        assert_eq!(owner, "qa-admin");
        now.lock().unwrap().clone()
    });
    RoleEngine::new(FrontRoles::on_machine(MachineRole {
        machines: Arc::new(FrontMachines::new().unwrap()),
        reader,
        owner: "qa-admin".into(),
        target: t.clone(),
    }))
}

fn pin(task: &str, model: Option<&str>) -> PinRequest {
    PinRequest {
        task_id: task.into(),
        model: model.map(str::to_string),
        thinking: false,
        roles: None,
        cancel: None,
    }
}

fn planner_call() -> CallRequest {
    CallRequest {
        role: "planner".into(),
        state: wtf16::parse(
            r#"{"taskId":"qa-task-1","tenantId":"qa-tenant","revision":1,"request":"Add a median helper to the QA repo.","projectInstructions":"Keep functions small.","capabilities":[{"name":"read"},{"name":"edit"}],"plan":{"goal":"median helper","steps":[{"do":"write median","done_when":"tests pass"}]}}"#,
        )
        .unwrap(),
        instructions: "Act as the QA planner.".into(),
        schema: wtf16::parse(
            r#"{"type":"object","additionalProperties":false,"required":["answer"],"properties":{"answer":{"type":"string"}}}"#,
        )
        .unwrap(),
        constrain: false,
        schema_name: None,
        max_tokens: None,
        max_bytes: None,
        cancel: None,
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn the_planner_pins_and_calls_on_the_code_role_s_machine_with_no_credential() {
    let (m, port) = machine().await;
    let t = target(port, "qa-m1");
    let e = engine(&t, Arc::new(Mutex::new(Some(t.clone()))));
    let session = e.pin_model(pin("qa-task-1", None)).await.unwrap();
    assert_eq!(session.model, "qa-coder");
    // The pinned machine key is the relay's target key: the pipeline Executor's `expect`.
    assert_eq!(session.machine.as_deref(), Some(code_machine::key_of(&t).as_str()));
    assert_eq!(
        session.machine.as_deref(),
        code_engine_relay::target_key(Some(&code_machine::CodeTarget { machine: t.clone() }.relay()))
            .as_deref()
    );
    let answer = session.call(planner_call()).await.unwrap();
    assert_eq!(answer.text, ANSWER);
    assert_eq!(answer.attempts, 1);
    let seen = m.seen.lock().unwrap().clone();
    assert_eq!(seen.len(), 1);
    // Never this server's key, nor any other credential, on a machine.
    assert_eq!(seen[0].0, None);
    let body = wtf16::parse(&seen[0].1).unwrap();
    assert_eq!(body.get("model").as_str(), Some("qa-coder"));
    assert_eq!(body.get("stream"), &JValue::Bool(true));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_code_role_moved_during_the_task_refuses_the_next_call_with_node_s_text() {
    let (m, port) = machine().await;
    let t = target(port, "qa-m1");
    let now = Arc::new(Mutex::new(Some(t.clone())));
    let e = engine(&t, Arc::clone(&now));
    let session = e.pin_model(pin("qa-task-2", None)).await.unwrap();
    // The administrator moves the Code role to another machine.
    *now.lock().unwrap() = Some(target(port, "qa-m2"));
    let f = session.call(planner_call()).await.unwrap_err();
    assert_eq!(f.code, "model_mismatch");
    assert_eq!(f.reason, ROLE_MOVED);
    // Back on this machine (no target): the same refusal.
    *now.lock().unwrap() = None;
    let f = session.call(planner_call()).await.unwrap_err();
    assert_eq!(f.code, "model_mismatch");
    assert!(m.seen.lock().unwrap().is_empty(), "nothing was sent");
}

#[tokio::test(flavor = "multi_thread")]
async fn pin_refusals_are_node_s() {
    let (m, port) = machine().await;
    let t = target(port, "qa-m1");
    let e = engine(&t, Arc::new(Mutex::new(Some(t.clone()))));
    // A task naming another model (role-engine.cjs pinOnMachine).
    let f = e
        .pin_model(pin("qa-task-3", Some("qa-other")))
        .await
        .unwrap_err();
    assert_eq!(f.code, "model_mismatch");
    assert_eq!(
        f.reason,
        "This task names qa-other, but the Code role runs qa-coder on QA Box. One task runs on one model."
    );
    // The machine serves another model (#1851).
    *m.served.lock().unwrap() = vec!["qa-elsewhere"];
    let f = e.pin_model(pin("qa-task-4", None)).await.unwrap_err();
    assert_eq!(f.code, "machineWrongModel");
    assert!(f.reason.contains("qa-coder"), "{}", f.reason);
    // The machine's health answers 503 (#1840).
    let (down, dport) = machine().await;
    *down.health.lock().unwrap() = 503;
    let td = target(dport, "qa-m3");
    let e = engine(&td, Arc::new(Mutex::new(Some(td.clone()))));
    let f = e.pin_model(pin("qa-task-5", None)).await.unwrap_err();
    assert_eq!(f.code, "machineUnreachable");
    assert!(f.reason.contains("it answered HTTP 503"), "{}", f.reason);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_machine_that_stops_answering_mid_task_is_unreachable_with_the_1840_text() {
    // A port nothing listens on: bind, read the port, close.
    let port = {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap().port()
    };
    let t = target(port, "qa-m4");
    let e = engine(&t, Arc::new(Mutex::new(Some(t.clone()))));
    let f = e.pin_model(pin("qa-task-6", None)).await.unwrap_err();
    assert_eq!(f.code, "machineUnreachable");
    assert!(f.reason.contains("QA Box"), "{}", f.reason);
}
