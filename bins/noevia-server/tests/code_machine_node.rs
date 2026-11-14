//! Council p11 (p9-1 CONTINUE 2; noevia#2676/#2720 item 4): the front's machine client for the
//! Code relay and the role engine (code_machine.rs `FrontMachines`) against core's own recording.
//! crates/code-engine-relay/tools/record-relay.cjs runs core's chat-machines.cjs `ready` and
//! `contextTokens` over core's machine-health.cjs probe and a scripted inference machine on
//! loopback (`/health`, `/v1/models`, `/props` answering as each scenario says), its
//! `connectError` over the error shapes a fetch rejects with, and the pure `modelIds` /
//! `propsFactsOf` over machine bodies, into crates/code-engine-relay/tests/fixtures/relay-node.v1.json.
//! This test serves the same machine with axum and asks `FrontMachines` (the relay's
//! [`Machines::ready`] / [`Machines::context_tokens`], the role engine's `role_connect_error`) and
//! the pure readers the same questions.
//!
//! Without the fixture the replay says so and passes, unless `CODE_RELAY_ORACLE_REQUIRED=1`.
//! Synthetic names only.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use axum::{
    extract::State,
    http::{StatusCode, Uri},
    response::IntoResponse,
    Router,
};
use code_engine_relay::Machines;
use engine_admission::machines::Target;
use js_json::{wtf16, JValue};
use noevia_server::code_machine::{
    self, CodeTarget, FrontMachines, TAG_ADDRESS, TAG_REDIRECT, TAG_TIMEOUT,
};
use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

fn fixture() -> Option<JValue> {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../crates/code-engine-relay/tests/fixtures/relay-node.v1.json");
    match std::fs::read_to_string(&path) {
        Ok(text) => Some(wtf16::parse(&text).expect("the fixture is JSON")),
        Err(_) => {
            assert!(
                std::env::var("CODE_RELAY_ORACLE_REQUIRED").as_deref() != Ok("1"),
                "CODE_RELAY_ORACLE_REQUIRED=1 but {} is missing",
                path.display()
            );
            eprintln!(
                "code_machine_node: {} not recorded yet (record-relay.cjs); nothing replayed",
                path.display()
            );
            None
        }
    }
}

fn s(v: &JValue) -> String {
    v.as_str().unwrap_or_default().to_string()
}

fn show(v: &JValue) -> String {
    wtf16::stringify(v).unwrap_or_default()
}

/// One scenario's machine: each path's status and body, and the paths asked.
struct Script {
    health: u16,
    models: (u16, String),
    props: (u16, String),
    seen: Mutex<BTreeSet<String>>,
}

async fn serve(State(m): State<Arc<Script>>, uri: Uri) -> impl IntoResponse {
    let path = uri.path().to_string();
    m.seen.lock().unwrap().insert(path.clone());
    let (status, body) = match path.as_str() {
        "/health" => (m.health, String::new()),
        "/v1/models" => m.models.clone(),
        "/props" => m.props.clone(),
        _ => (404, String::new()),
    };
    (
        StatusCode::from_u16(status).unwrap(),
        [("content-type", "application/json")],
        body,
    )
}

fn answer_of(v: &JValue) -> (u16, String) {
    (
        v.get("status").as_num().map_or(200, |n| n as u16),
        s(v.get("body")),
    )
}

async fn machine(sc: &JValue) -> (Arc<Script>, u16) {
    let m = Arc::new(Script {
        health: sc.get("health").as_num().map_or(200, |n| n as u16),
        models: answer_of(sc.get("models")),
        props: answer_of(sc.get("props")),
        seen: Mutex::new(BTreeSet::new()),
    });
    let app = Router::new().fallback(serve).with_state(Arc::clone(&m));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (m, port)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn front_machines_answer_ready_and_context_as_core_s_recording() {
    let Some(fx) = fixture() else {
        return;
    };
    let base = fx.get("machines").get("target");
    let JValue::Arr(scenarios) = fx.get("machines").get("scenarios") else {
        panic!("the fixture has no machine scenarios");
    };
    assert!(scenarios.len() >= 15, "{} machine scenarios", scenarios.len());
    let mut diffs = Vec::new();
    for sc in scenarios {
        let name = s(sc.get("name"));
        let (m, port) = machine(sc).await;
        let t = Target {
            role: s(base.get("role")),
            machine_id: s(base.get("machine").get("id")),
            machine_name: s(base.get("machine").get("name")),
            port: u32::from(port),
            model: s(base.get("model")),
            base_url: format!("http://127.0.0.1:{port}"),
        };
        // A fresh client per scenario: Node's run makes a fresh probe cache for each.
        let fm = FrontMachines::new().unwrap();
        let code = CodeTarget { machine: t.clone() };
        fm.remember(&code);
        let relay_target = code.relay();
        let ready = fm.ready(&relay_target).await;
        let context = fm.context_tokens(&relay_target).await;

        let mut got = vec![("ok".to_string(), JValue::Bool(ready.ok))];
        if let Some(c) = &ready.code {
            got.push(("code".into(), JValue::Str(c.clone())));
        }
        if let Some(e) = &ready.error {
            got.push(("error".into(), JValue::Str(e.replace(&port.to_string(), "{port}"))));
        }
        let got = JValue::Obj(got);
        let want = sc.get("ready");
        let same = got.get("ok") == want.get("ok")
            && got.get("code") == want.get("code")
            && got.get("error") == want.get("error");
        if !same {
            diffs.push(format!(
                "{name}: ready\n  ours: {}\n  Node: {}",
                show(&got),
                show(want)
            ));
        }
        if context.map(JValue::Num).unwrap_or(JValue::Null) != *sc.get("contextTokens") {
            diffs.push(format!(
                "{name}: contextTokens ours {context:?}, Node {}",
                show(sc.get("contextTokens"))
            ));
        }
        let seen = JValue::Arr(
            m.seen
                .lock()
                .unwrap()
                .iter()
                .map(|p| JValue::Str(p.clone()))
                .collect(),
        );
        if seen != *sc.get("paths") {
            diffs.push(format!(
                "{name}: paths asked\n  ours: {}\n  Node: {}",
                show(&seen),
                show(sc.get("paths"))
            ));
        }
    }
    assert!(
        diffs.is_empty(),
        "{} differences from core's chat-machines:\n{}",
        diffs.len(),
        diffs.join("\n")
    );
}

/// The error shape a fetch rejected with, as the front's role client tags it (code_machine.rs
/// `role_fetch`: refused address, redirect, timeout, anything else).
fn tag_of(err: &JValue) -> String {
    if err.get("code").as_str() == Some("EPUBLICADDR") {
        TAG_ADDRESS.into()
    } else if s(err.get("message")).to_lowercase().contains("redirect") {
        TAG_REDIRECT.into()
    } else if err.get("name").as_str() == Some("TimeoutError") {
        TAG_TIMEOUT.into()
    } else {
        format!("machine: {}", s(err.get("message")))
    }
}

#[test]
fn role_connect_errors_are_core_s_texts() {
    let Some(fx) = fixture() else {
        return;
    };
    let t = fx.get("connectErrors").get("target");
    let JValue::Arr(cases) = fx.get("connectErrors").get("cases") else {
        panic!("the fixture has no connectError cases");
    };
    let fm = FrontMachines::new().unwrap();
    let mut diffs = Vec::new();
    for c in cases {
        let detail = if c.get("err").is_nullish() {
            "machine: fetch failed".to_string()
        } else {
            tag_of(c.get("err"))
        };
        let got = fm.role_connect_error(t, &role_engine::TransportError { detail });
        if Some(got.as_str()) != c.get("text").as_str() {
            diffs.push(format!(
                "{}: ours {got:?}, Node {}",
                s(c.get("name")),
                show(c.get("text"))
            ));
        }
    }
    assert!(diffs.is_empty(), "{}", diffs.join("\n"));
}

#[test]
fn model_ids_and_props_context_are_core_s_readers() {
    let Some(fx) = fixture() else {
        return;
    };
    let mut diffs = Vec::new();
    if let JValue::Arr(cases) = fx.get("modelIds") {
        assert!(cases.len() >= 10);
        for c in cases {
            let body = wtf16::parse(&s(c.get("body"))).unwrap();
            let got = JValue::Arr(
                code_machine::model_ids(&body)
                    .into_iter()
                    .map(JValue::Str)
                    .collect(),
            );
            if got != *c.get("ids") {
                diffs.push(format!(
                    "modelIds {}\n  ours: {}\n  Node: {}",
                    s(c.get("body")),
                    show(&got),
                    show(c.get("ids"))
                ));
            }
        }
    } else {
        panic!("the fixture has no modelIds cases");
    }
    if let JValue::Arr(cases) = fx.get("propsContext") {
        assert!(cases.len() >= 20);
        for c in cases {
            let body = wtf16::parse(&s(c.get("body"))).unwrap();
            let got = code_machine::props_context(&body).map_or(JValue::Null, JValue::Num);
            if got != *c.get("contextTokens") {
                diffs.push(format!(
                    "propsContext {}: ours {}, Node {}",
                    s(c.get("body")),
                    show(&got),
                    show(c.get("contextTokens"))
                ));
            }
        }
    } else {
        panic!("the fixture has no propsContext cases");
    }
    assert!(diffs.is_empty(), "{}", diffs.join("\n"));
}
