//! Council p4: the Code engine relay over the front's machines (code_machine.rs) against a
//! synthetic inference machine on loopback: readiness (health, served model), the context from
//! `/props`, a forwarded chat completion with the model forced and no credential, the refusals and
//! the lease's end. Answers are chat-machines.cjs's and code-machine.cjs's
//! (code-machine-1853.test.cjs, chat-machines-1840.test.cjs).
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
use code_engine_relay::{Chunk, Machines, Options, Relay, Reply, Request, Target};
use noevia_server::code_machine::{CodeTarget, FrontMachines, CONTEXT_FALLBACK};
use std::sync::{Arc, Mutex};

/// What the synthetic machine serves, and what it saw.
#[derive(Default)]
struct Machine {
    health: Mutex<u16>,
    served: Mutex<Vec<&'static str>>,
    n_ctx: Mutex<Option<u64>>,
    seen: Mutex<Vec<(String, String)>>,
    health_calls: Mutex<usize>,
}

async fn health(State(m): State<Arc<Machine>>) -> impl IntoResponse {
    *m.health_calls.lock().unwrap() += 1;
    StatusCode::from_u16(*m.health.lock().unwrap()).unwrap()
}

fn json(v: serde_json::Value) -> axum::response::Response {
    ([("content-type", "application/json")], v.to_string()).into_response()
}

async fn models(State(m): State<Arc<Machine>>) -> impl IntoResponse {
    let data: Vec<_> = m
        .served
        .lock()
        .unwrap()
        .iter()
        .map(|id| serde_json::json!({ "id": id }))
        .collect();
    json(serde_json::json!({ "data": data }))
}

async fn props(State(m): State<Arc<Machine>>) -> impl IntoResponse {
    match *m.n_ctx.lock().unwrap() {
        Some(n) => json(serde_json::json!({ "default_generation_settings": { "n_ctx": n } })),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

async fn completions(
    State(m): State<Arc<Machine>>,
    headers: HeaderMap,
    body: String,
) -> impl IntoResponse {
    let auth = headers
        .get("authorization")
        .map(|v| v.to_str().unwrap().to_string())
        .unwrap_or_default();
    m.seen.lock().unwrap().push((auth, body));
    (
        [("content-type", "text/event-stream")],
        "data: {\"choices\":[]}\n\ndata: [DONE]\n\n",
    )
}

async fn machine() -> (Arc<Machine>, u16) {
    let m = Arc::new(Machine {
        health: Mutex::new(200),
        served: Mutex::new(vec!["qa-coder"]),
        n_ctx: Mutex::new(Some(32768)),
        ..Machine::default()
    });
    let app = Router::new()
        .route("/health", get(health))
        .route("/v1/models", get(models))
        .route("/props", get(props))
        .route("/v1/chat/completions", post(completions))
        .with_state(Arc::clone(&m));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (m, port)
}

fn code_target(port: u16) -> CodeTarget {
    CodeTarget {
        machine: engine_admission::machines::Target {
            role: "code".into(),
            machine_id: "qa-m1".into(),
            machine_name: "QA Box".into(),
            port: u32::from(port),
            model: "qa-coder".into(),
            base_url: format!("http://127.0.0.1:{port}"),
        },
    }
}

fn relay(port: u16) -> (Arc<FrontMachines>, Relay<FrontMachines>, Target) {
    let machines = Arc::new(FrontMachines::new().unwrap());
    let t = code_target(port);
    machines.remember(&t);
    let target = t.relay();
    let fixed = target.clone();
    let relay = Relay::new(Options::new(
        Arc::clone(&machines),
        Arc::new(move |_: &str| Some(fixed.clone())),
        Some("http://egress:8095/v1".into()),
    ));
    (machines, relay, target)
}

fn completion(token: &str, body: &str) -> Request<Vec<u8>> {
    Request {
        method: "POST".into(),
        url: "/v1/chat/completions".into(),
        authorization: Some(format!("Bearer {token}")),
        accept: Some("text/event-stream".into()),
        body: body.as_bytes().to_vec(),
    }
}

#[tokio::test]
async fn a_ready_machine_opens_a_lease_and_relays_with_the_model_forced() {
    let (m, port) = machine().await;
    let (_machines, relay, _) = relay(port);
    let opened = relay.open("qa-task-1", None, None).await.unwrap();
    assert_eq!(opened.base_url, "http://egress:8095/v1");
    assert_eq!(opened.model, "qa-coder");
    assert_eq!(opened.context_tokens, Some(32768.0));
    assert!(opened.token.starts_with("ncr_"));
    let reply = relay
        .handle(completion(
            &opened.token,
            r#"{"model":"qa-other","messages":[],"stream":true}"#,
        ))
        .await;
    let Reply::Stream(mut s) = reply else {
        panic!("{reply:?}")
    };
    assert_eq!(s.status(), 200);
    assert_eq!(s.content_type(), "text/event-stream");
    let mut got = Vec::new();
    loop {
        match s.next().await {
            Chunk::Data(d) => got.extend(d),
            Chunk::End => break,
            Chunk::Cut => panic!("cut"),
        }
    }
    assert!(String::from_utf8(got)
        .unwrap()
        .ends_with("data: [DONE]\n\n"));
    let seen = m.seen.lock().unwrap().clone();
    assert_eq!(seen.len(), 1);
    // No credential reaches the machine (neither the relay token nor any key).
    assert_eq!(seen[0].0, "");
    let body: serde_json::Value = serde_json::from_str(&seen[0].1).unwrap();
    assert_eq!(body["model"], "qa-coder");
    // The lease ends with the run: the token stops working.
    let token = opened.token.clone();
    opened.close();
    let after = relay.handle(completion(&token, "{}")).await;
    assert!(
        matches!(after, Reply::Json { status: 401, .. }),
        "{after:?}"
    );
}

#[tokio::test]
async fn a_dropped_lease_revokes_its_token() {
    let (_m, port) = machine().await;
    let (_machines, relay, _) = relay(port);
    let token = {
        let opened = relay.open("qa-task-2", None, None).await.unwrap();
        assert_eq!(relay.active(), 1);
        opened.token.clone()
    };
    assert_eq!(relay.active(), 0);
    let r = relay.handle(completion(&token, "{}")).await;
    assert!(matches!(r, Reply::Json { status: 401, .. }), "{r:?}");
}

#[tokio::test]
async fn a_machine_serving_another_model_refuses_the_open() {
    let (m, port) = machine().await;
    *m.served.lock().unwrap() = vec!["qa-other-model"];
    let (_machines, relay, _) = relay(port);
    let r = relay.open("qa-task-3", None, None).await.unwrap_err();
    assert_eq!(r.code, "machineWrongModel");
    assert_eq!(
        r.message,
        format!("The Code machine (QA Box) serves qa-other-model, not qa-coder. Start qa-coder on port {port}, or choose the model it serves for the Code role in Settings \u{2192} Inference machines.")
    );
}

#[tokio::test]
async fn a_machine_that_answers_its_health_check_with_an_error_is_unreachable() {
    let (m, port) = machine().await;
    *m.health.lock().unwrap() = 503;
    let (_machines, relay, _) = relay(port);
    let r = relay.open("qa-task-4", None, None).await.unwrap_err();
    assert_eq!(r.code, "machineUnreachable");
    assert_eq!(
        r.message,
        format!("Code model\u{2019}s machine (QA Box) is unreachable: it answered HTTP 503. Check that it is on and serving qa-coder on port {port}, or move the Code role back to this machine in Settings \u{2192} Inference machines.")
    );
}

#[tokio::test]
async fn a_login_wall_is_up_and_a_missing_props_gives_the_floor() {
    let (m, port) = machine().await;
    *m.health.lock().unwrap() = 401;
    *m.n_ctx.lock().unwrap() = None;
    let (_machines, relay, _) = relay(port);
    let opened = relay.open("qa-task-5", None, None).await.unwrap();
    assert_eq!(opened.context_tokens, Some(CONTEXT_FALLBACK));
}

#[tokio::test]
async fn a_machine_that_is_not_listening_is_unreachable_with_no_detail() {
    // A port nothing listens on.
    let port = {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap().port()
    };
    let (_machines, relay, _) = relay(port);
    let r = relay.open("qa-task-6", None, None).await.unwrap_err();
    assert_eq!(r.code, "machineUnreachable");
    assert_eq!(
        r.message,
        format!("Code model\u{2019}s machine (QA Box) is unreachable. Check that it is on and serving qa-coder on port {port}, or move the Code role back to this machine in Settings \u{2192} Inference machines.")
    );
}

#[tokio::test]
async fn the_health_result_is_cached_between_opens() {
    let (m, port) = machine().await;
    let (_machines, relay, _) = relay(port);
    for i in 0..3 {
        relay
            .open(&format!("qa-task-c{i}"), None, None)
            .await
            .unwrap();
    }
    assert_eq!(*m.health_calls.lock().unwrap(), 1);
}

#[tokio::test]
async fn an_address_off_this_network_is_refused_before_any_request() {
    let machines = FrontMachines::new().unwrap();
    let t = Target {
        machine_id: "qa-m1".into(),
        machine_name: "QA Box".into(),
        base_url: "http://203.0.113.9:8080".into(),
        model: "qa-coder".into(),
    };
    let r = machines.ready(&t).await;
    assert_eq!(r.code.as_deref(), Some("machineUnreachable"));
    assert!(
        r.error
            .as_deref()
            .unwrap()
            .contains("is unreachable: its address is not on this network."),
        "{r:?}"
    );
    let sent = machines
        .fetch(code_engine_relay::Forward {
            url: "http://203.0.113.9:8080/v1/chat/completions".into(),
            accept: "*/*".into(),
            body: "{}".into(),
        })
        .await;
    let Err(e) = sent else { panic!("sent") };
    assert!(machines
        .connect_error(&t, Some(&e))
        .contains("is unreachable: its address is not on this network."));
    // A request that ran out of time (no error): Node's `TimeoutError` text.
    assert!(machines
        .connect_error(&t, None)
        .contains("is unreachable: no answer in time."));
}

#[tokio::test]
async fn the_switch_off_starts_no_relay() {
    // NOEVIA_RUST_CODE_RELAY unset: the front keeps the 409 it always gave (code_routes ON_A_MACHINE).
    assert!(!noevia_server::code_machine::enabled(&|_| None));
    assert!(!noevia_server::code_machine::enabled(&|k| {
        (k == noevia_server::code_machine::SWITCH).then(|| "yes".to_string())
    }));
    // Council p9: the relay switch alone is not enough while Code tasks are Node's (core then keeps
    // its own listener on CODE_ENGINE_RELAY_PORT): the front binds nothing.
    assert!(!noevia_server::code_machine::enabled(&|k| {
        (k == noevia_server::code_machine::SWITCH).then(|| "1".to_string())
    }));
    assert!(noevia_server::code_machine::enabled(&|k| match k {
        noevia_server::code_machine::SWITCH => Some("1".to_string()),
        noevia_server::code_routes::SWITCH => Some("rust".to_string()),
        _ => None,
    }));
}
