//! Port contract row J: the front is the one maintenance lease authority. Node's maintenance gate takes
//! its holds here (`POST /v1/maintenance-lease/*` on the store listener, behind its secret); the front's
//! own reload and calibration take the same table first and only then hold Node's local chat admission
//! (the latch, `/internal/model-maintenance/lease`). Ported from core `model-maintenance-lease.test.cjs`
//! and `preset-reload.test.cjs`: exclusive hold, busy 409, renewal, hard expiry, lost-coverage fencing,
//! release, restart, concurrent acquire race and cancellation while waiting. Synthetic stubs only.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]
mod maint_support;
use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use http_body_util::BodyExt;
use maint_support::*;
use noevia_server::model_maint::lease::{self, Authority};
use noevia_server::model_maint::{HoldError, Maint};
use noevia_server::project_store::{maintenance_lease_routes, Store, TOKEN_HEADER};
use serde_json::{json, Value};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tower::ServiceExt;

const SECRET: &str = "synthetic-store-token-0123456789abcdef";
const LATCH: &str = "/internal/model-maintenance/lease";
const GRANT: &str =
    r#"{"lease":"0123456789abcdef0123456789abcdef","ttlMs":180000,"renewMs":100,"hardMs":3600000}"#;

fn store(dir: &std::path::Path) -> Store {
    Store {
        data_dir: dir.into(),
        token: SECRET.into(),
        switch: server_store::RustProjects::owned(),
        start_id: "0123456789abcdef0123456789abcdef".into(),
    }
}

async fn op(
    app: &axum::Router,
    name: &str,
    token: Option<&str>,
    body: Value,
) -> (StatusCode, Value) {
    let mut req = Request::builder()
        .method("POST")
        .uri(format!("/v1/maintenance-lease/{name}"))
        .header("content-type", "application/json");
    if let Some(t) = token {
        req = req.header(TOKEN_HEADER, t);
    }
    let res = app
        .clone()
        .oneshot(req.body(Body::from(body.to_string())).unwrap())
        .await
        .unwrap();
    let status = res.status();
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

#[test]
fn the_build_advertises_the_authority_marker_the_release_pairs_with_core() {
    assert_eq!(
        lease::AUTHORITY_MARKER,
        "noevia-maintenance-lease-authority/1"
    );
    assert!(noevia_server::FEATURES.contains(&lease::AUTHORITY_MARKER));
}

/// Registers Node start `boot` (started at `started`) and returns its incarnation.
async fn session(app: &axum::Router, boot: &str, started: u64) -> u64 {
    let (s, b) = op(
        app,
        "session",
        Some(SECRET),
        json!({"boot": boot, "startedAt": started}),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    b["incarnation"].as_u64().unwrap()
}

#[tokio::test]
async fn node_routes_need_the_secret_and_fail_closed_without_an_authority() {
    let d = tempfile::tempdir().unwrap();
    let authority = Authority::new();
    let app = maintenance_lease_routes(store(d.path()), Some(authority.clone()));
    for name in lease::OPS {
        let body = json!({"boot": "b", "startedAt": 1, "incarnation": 1});
        let (s, _) = op(&app, name, None, body.clone()).await;
        assert_eq!(s, StatusCode::UNAUTHORIZED, "{name}");
        let (s, _) = op(&app, name, Some("wrong"), body).await;
        assert_eq!(s, StatusCode::UNAUTHORIZED, "{name}");
    }
    assert!(!authority.held(), "nothing was taken without the secret");
    let none = maintenance_lease_routes(store(d.path()), None);
    let (s, _) = op(
        &none,
        "session",
        Some(SECRET),
        json!({"boot": "b", "startedAt": 1}),
    )
    .await;
    assert_eq!(s, StatusCode::SERVICE_UNAVAILABLE);
}

#[tokio::test]
async fn node_acquire_renew_release_and_busy_409() {
    let d = tempfile::tempdir().unwrap();
    let authority = Authority::new();
    let app = maintenance_lease_routes(store(d.path()), Some(authority.clone()));
    let inc = session(&app, "b1", 100).await;
    let (s, g) = op(
        &app,
        "acquire",
        Some(SECRET),
        json!({"incarnation": inc, "reason": "tune"}),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED);
    assert_eq!(g["ttlMs"], 180_000);
    assert_eq!(g["renewMs"], 10_000);
    assert_eq!(g["hardMs"], 3_600_000);
    assert_eq!(g["epoch"], authority.epoch());
    let id = g["lease"].as_str().unwrap().to_owned();
    let generation = g["generation"].as_u64().unwrap();
    let (s, b) = op(&app, "acquire", Some(SECRET), json!({"incarnation": inc})).await;
    assert_eq!(s, StatusCode::CONFLICT);
    assert_eq!(b["chatActive"], false);
    assert_eq!(b["code"], "maintenance_held");
    assert_eq!(
        authority.try_acquire(lease::Origin::Front, None, "reload"),
        Err(lease::Refusal::Busy)
    );
    let (s, h) = op(&app, "held", Some(SECRET), json!({"incarnation": inc})).await;
    assert_eq!(
        (s, h["held"].clone(), h["front"].clone()),
        (StatusCode::OK, json!(true), json!(false))
    );
    let (s, _) = op(
        &app,
        "renew",
        Some(SECRET),
        json!({"incarnation": inc, "lease": id, "generation": generation}),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    let (s, _) = op(
        &app,
        "renew",
        Some(SECRET),
        json!({"incarnation": inc, "lease": id, "generation": generation + 1}),
    )
    .await;
    assert_eq!(
        s,
        StatusCode::NOT_FOUND,
        "a wrong generation is not this lease"
    );
    let (s, _) = op(
        &app,
        "release",
        Some(SECRET),
        json!({"incarnation": inc, "lease": id}),
    )
    .await;
    assert_eq!(s, StatusCode::NO_CONTENT);
    let (s, _) = op(
        &app,
        "release",
        Some(SECRET),
        json!({"incarnation": inc, "lease": id}),
    )
    .await;
    assert_eq!(s, StatusCode::NO_CONTENT, "release is idempotent");
    let (s, _) = op(
        &app,
        "renew",
        Some(SECRET),
        json!({"incarnation": inc, "lease": id}),
    )
    .await;
    assert_eq!(s, StatusCode::NOT_FOUND, "an ended lease is not renewed");
    assert!(!authority.held());
}

#[tokio::test]
async fn node_bad_bodies_are_400_and_hold_nothing() {
    let d = tempfile::tempdir().unwrap();
    let authority = Authority::new();
    let app = maintenance_lease_routes(store(d.path()), Some(authority.clone()));
    for body in [
        json!({}),
        json!({"boot": ""}),
        json!({"boot": "b"}),
        json!({"boot": "b", "startedAt": "1"}),
    ] {
        let (s, _) = op(&app, "session", Some(SECRET), body.clone()).await;
        assert_eq!(s, StatusCode::BAD_REQUEST, "{body}");
    }
    let inc = session(&app, "b", 1).await;
    for body in [
        json!({}),
        json!({"incarnation": inc, "maxMs": "7200000"}),
        json!({"incarnation": inc, "maxMs": 1.5}),
        json!({"incarnation": inc, "waitMs": true}),
    ] {
        let (s, _) = op(&app, "acquire", Some(SECRET), body.clone()).await;
        assert_eq!(s, StatusCode::BAD_REQUEST, "{body}");
    }
    assert!(!authority.held());
    let (_, g) = op(
        &app,
        "acquire",
        Some(SECRET),
        json!({"incarnation": inc, "maxMs": 1}),
    )
    .await;
    assert_eq!(g["hardMs"], 3_600_000);
    authority.release(g["lease"].as_str().unwrap(), None);
    let (_, g) = op(
        &app,
        "acquire",
        Some(SECRET),
        json!({"incarnation": inc, "maxMs": 100_000_000_i64}),
    )
    .await;
    assert_eq!(g["hardMs"], 86_400_000);
}

#[tokio::test]
async fn node_restart_frees_the_previous_starts_lease_and_a_front_restart_grants_nothing_stale() {
    let d = tempfile::tempdir().unwrap();
    let authority = Authority::new();
    let app = maintenance_lease_routes(store(d.path()), Some(authority.clone()));
    let old = session(&app, "old", 10).await;
    let (_, g) = op(&app, "acquire", Some(SECRET), json!({"incarnation": old})).await;
    let new = session(&app, "new", 20).await;
    assert!(new > old);
    assert!(
        !authority.held(),
        "the crashed Node's lease ended with its start"
    );
    for name in ["renew", "release", "acquire", "held"] {
        let (s, b) = op(
            &app,
            name,
            Some(SECRET),
            json!({"incarnation": old, "lease": g["lease"]}),
        )
        .await;
        assert_eq!(s, StatusCode::GONE, "{name}");
        assert_eq!(b["code"], "incarnation_retired");
    }
    let (_, g) = op(&app, "acquire", Some(SECRET), json!({"incarnation": new})).await;
    let restarted = Authority::new();
    let app2 = maintenance_lease_routes(store(d.path()), Some(restarted.clone()));
    let again = session(&app2, "new", 20).await;
    let (s, _) = op(
        &app2,
        "renew",
        Some(SECRET),
        json!({"incarnation": again, "lease": g["lease"]}),
    )
    .await;
    assert_eq!(s, StatusCode::NOT_FOUND);
}

// Review P2: ordered, server-issued incarnations.
#[tokio::test]
async fn an_old_waiting_acquire_or_old_session_after_a_new_starts_grant_changes_nothing() {
    let d = tempfile::tempdir().unwrap();
    let authority = Authority::new();
    let app = maintenance_lease_routes(store(d.path()), Some(authority.clone()));
    let a = session(&app, "boot-a", 10).await;
    let front = authority
        .try_acquire(lease::Origin::Front, None, "reload")
        .unwrap();
    let waiting = {
        let app = app.clone();
        tokio::spawn(async move {
            op(
                &app,
                "acquire",
                Some(SECRET),
                json!({"incarnation": a, "waitMs": 5000}),
            )
            .await
        })
    };
    tokio::time::sleep(Duration::from_millis(50)).await;
    let b = session(&app, "boot-b", 20).await;
    authority.release(&front.id, Some(front.generation));
    let (s, g) = op(&app, "acquire", Some(SECRET), json!({"incarnation": b})).await;
    assert_eq!(s, StatusCode::CREATED);
    let (s, w) = tokio::time::timeout(Duration::from_secs(2), waiting)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        (s, w["code"].clone()),
        (StatusCode::GONE, json!("incarnation_retired"))
    );
    let (s, _) = op(
        &app,
        "session",
        Some(SECRET),
        json!({"boot": "boot-a", "startedAt": 10}),
    )
    .await;
    assert_eq!(
        s,
        StatusCode::GONE,
        "an old start's delayed session is retired"
    );
    let (s, _) = op(
        &app,
        "session",
        Some(SECRET),
        json!({"boot": "boot-x", "startedAt": 20}),
    )
    .await;
    assert_eq!(s, StatusCode::GONE, "an ambiguous start is retired too");
    assert!(authority.covers(
        g["lease"].as_str().unwrap(),
        g["generation"].as_u64().unwrap(),
        Duration::ZERO
    ));
    assert_eq!(
        session(&app, "boot-b", 20).await,
        b,
        "the active start keeps its incarnation"
    );
}

#[tokio::test]
async fn node_waiting_acquire_gets_it_on_release_and_a_cancelled_wait_takes_nothing() {
    let d = tempfile::tempdir().unwrap();
    let authority = Authority::new();
    let app = maintenance_lease_routes(store(d.path()), Some(authority.clone()));
    let inc = session(&app, "b", 1).await;
    let front = authority
        .try_acquire(lease::Origin::Front, None, "reload")
        .unwrap();
    let wait = op(
        &app,
        "acquire",
        Some(SECRET),
        json!({"incarnation": inc, "waitMs": 5000}),
    );
    assert!(tokio::time::timeout(Duration::from_millis(100), wait)
        .await
        .is_err());
    assert!(authority.covers(&front.id, front.generation, Duration::ZERO));
    let waiter = {
        let app = app.clone();
        tokio::spawn(async move {
            op(
                &app,
                "acquire",
                Some(SECRET),
                json!({"incarnation": inc, "waitMs": 5000}),
            )
            .await
        })
    };
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(!waiter.is_finished());
    authority.release(&front.id, Some(front.generation));
    let (s, g) = tokio::time::timeout(Duration::from_secs(2), waiter)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(s, StatusCode::CREATED);
    assert!(g["generation"].as_u64().unwrap() > front.generation);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_node_and_front_acquires_grant_exactly_one() {
    let d = tempfile::tempdir().unwrap();
    let authority = Authority::new();
    let app = maintenance_lease_routes(store(d.path()), Some(authority.clone()));
    let inc = session(&app, "b", 1).await;
    let barrier = Arc::new(tokio::sync::Barrier::new(32));
    let mut tasks = Vec::new();
    for i in 0..32 {
        let (app, a, barrier) = (app.clone(), authority.clone(), Arc::clone(&barrier));
        tasks.push(tokio::spawn(async move {
            barrier.wait().await;
            if i % 2 == 0 {
                op(&app, "acquire", Some(SECRET), json!({"incarnation": inc}))
                    .await
                    .0
                    == StatusCode::CREATED
            } else {
                a.try_acquire(lease::Origin::Front, None, "r").is_ok()
            }
        }));
    }
    let mut won = 0;
    for t in tasks {
        won += usize::from(t.await.unwrap());
    }
    assert_eq!(won, 1);
}

// ---- The front's own holds: authority first, then Node's chat admission (the latch). ----

fn router_without_gate() -> Handler {
    Arc::new(|req: &Req| {
        if req.target.starts_with("/gate/v1/") {
            return Answer::Reply(404, r#"{"error":"not found"}"#.into());
        }
        Answer::Reply(200, r#"{"data":[]}"#.into())
    })
}

fn latch(calls: Arc<AtomicUsize>, acquire: &'static str, status: u16) -> Handler {
    Arc::new(move |req: &Req| {
        assert_eq!(
            req.headers.get("x-noevia-store-token").map(String::as_str),
            Some(SECRET)
        );
        if req.method == "POST" && req.target == LATCH {
            calls.fetch_add(1, Ordering::SeqCst);
            return Answer::Reply(status, acquire.into());
        }
        if req.method == "POST" {
            return Answer::Reply(200, "{}".into());
        }
        Answer::Reply(204, String::new())
    })
}

async fn maint(node_port: u16) -> (Arc<Maint>, tempfile::TempDir) {
    let (router_port, _) = engine(router_without_gate()).await;
    let d = tempfile::tempdir().unwrap();
    let ini = d.path().join("models.ini");
    std::fs::write(&ini, "[A]\nmodel = /models/a.gguf\n").unwrap();
    let env: Vec<(&str, String)> = vec![
        ("MODEL_MANAGER_KIND", "llamacpp".into()),
        (
            "MODEL_MANAGER_BASE_URL",
            format!("http://127.0.0.1:{router_port}/v1"),
        ),
        ("LLAMACPP_PRESET_PATH", ini.display().to_string()),
        (
            "NOEVIA_LEGACY_UPSTREAM",
            format!("http://127.0.0.1:{node_port}"),
        ),
        ("NOEVIA_PROJECT_STORE_TOKEN", SECRET.into()),
        ("UI_DATA_DIR", d.path().display().to_string()),
    ];
    let get = |k: &str| env.iter().find(|(n, _)| *n == k).map(|(_, v)| v.clone());
    let engine = Arc::new(server_stats::Engine::from_lookup(&get).unwrap());
    let sizing = Arc::new(noevia_server::model_sizing::Sizing::from_lookup(
        engine.kind(),
        Arc::clone(&engine),
        &get,
    ));
    let m = Maint::from_lookup(engine.kind(), &engine, sizing, &get).unwrap();
    // Node has registered its start, as at boot.
    m.authority.node_session("b", 1).unwrap();
    (Arc::new(m), d)
}

/// The active Node start's origin.
fn node_origin(a: &Authority) -> lease::Origin {
    lease::Origin::Node {
        incarnation: a.node_session("b", 1).unwrap().incarnation,
    }
}

// Review P1: a Node restart while a front job holds the lease fences the job before its next step,
// and the lease keeps excluding (the new Node keeps chat closed) until the job releases it.
#[tokio::test]
async fn a_node_restart_under_a_front_hold_fences_its_next_step_and_keeps_exclusion() {
    let calls = Arc::new(AtomicUsize::new(0));
    let (node_port, _) = engine(latch(Arc::clone(&calls), GRANT, 201)).await;
    let (m, _d) = maint(node_port).await;
    let hold = m
        .acquire("synthetic reload", Duration::from_secs(300), false)
        .await
        .unwrap();
    assert!(hold.may_start(Duration::from_secs(60)));
    let restarted = m.authority.node_session("b-restarted", 2).unwrap();
    assert!(restarted.front_held, "the new Node must keep chat closed");
    assert!(
        !hold.may_start(Duration::ZERO),
        "no step starts on the dead Node's confirmation"
    );
    assert!(!hold.excluded());
    assert!(
        m.authority.held_by_front(),
        "exclusion stays while the in-flight step settles"
    );
    let new = lease::Origin::Node {
        incarnation: restarted.incarnation,
    };
    assert_eq!(
        m.authority.try_acquire(new.clone(), None, "t"),
        Err(lease::Refusal::Busy)
    );
    hold.release().await;
    assert!(
        !m.authority
            .node_session("b-restarted", 2)
            .unwrap()
            .front_held
    );
    assert!(m.authority.try_acquire(new, None, "t").is_ok());
}

// Review P2: aborting the acquisition while Node's latch answer is outstanding leaks nothing: the
// local job flag and the authority are free at once, and the latch is given back by its ticket.
#[tokio::test]
async fn an_acquisition_aborted_during_the_latch_call_frees_everything_at_once() {
    let deletes = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let seen = Arc::clone(&deletes);
    let (node_port, _) = engine(Arc::new(move |req: &Req| {
        if req.method == "POST" && req.target == LATCH {
            assert!(
                req.body.contains("\"ticket\""),
                "the latch is asked for by ticket"
            );
            return Answer::After(Duration::from_secs(30), 201, GRANT.into());
        }
        if req.method == "DELETE" {
            seen.lock().unwrap().push(req.target.clone());
        }
        Answer::Reply(204, String::new())
    }))
    .await;
    let (m, _d) = maint(node_port).await;
    let task = {
        let m = Arc::clone(&m);
        tokio::spawn(async move {
            m.acquire("synthetic reload", Duration::from_secs(300), false)
                .await
                .map(|_| ())
        })
    };
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(
        m.local_held() && m.authority.held_by_front(),
        "blocked inside the latch call"
    );
    task.abort();
    let _ = task.await;
    assert!(!m.local_held(), "the local reservation is free at once");
    assert!(!m.authority.held(), "the authority grant is free at once");
    for _ in 0..50 {
        if !deletes.lock().unwrap().is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let deletes = deletes.lock().unwrap().clone();
    assert_eq!(deletes.len(), 1, "{deletes:?}");
    assert!(
        deletes[0].starts_with(&format!("{LATCH}/ticket/")),
        "{deletes:?}"
    );
    // The next job may run at once.
    assert!(m
        .authority
        .try_acquire(node_origin(&m.authority), None, "t")
        .is_ok());
}

#[tokio::test]
async fn a_front_hold_takes_the_authority_and_the_latch_and_excludes_node() {
    let calls = Arc::new(AtomicUsize::new(0));
    let (node_port, _) = engine(latch(Arc::clone(&calls), GRANT, 201)).await;
    let (m, _d) = maint(node_port).await;
    let hold = m
        .acquire("synthetic reload", Duration::from_secs(300), false)
        .await
        .unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert!(m.authority.held_by_front());
    assert!(hold.excluded());
    assert!(hold.may_start(Duration::from_secs(120)));
    // Node's gate asks the same table: busy.
    assert_eq!(
        m.authority
            .try_acquire(node_origin(&m.authority), None, "tune"),
        Err(lease::Refusal::Busy)
    );
    hold.release().await;
    assert!(!m.authority.held(), "released with the hold");
    assert!(m
        .authority
        .try_acquire(node_origin(&m.authority), None, "tune")
        .is_ok());
}

#[tokio::test]
async fn a_node_held_lease_refuses_the_front_before_node_is_asked() {
    let calls = Arc::new(AtomicUsize::new(0));
    let (node_port, _) = engine(latch(Arc::clone(&calls), GRANT, 201)).await;
    let (m, _d) = maint(node_port).await;
    let node = m
        .authority
        .try_acquire(node_origin(&m.authority), None, "delete")
        .unwrap();
    assert_eq!(
        m.acquire("r", Duration::from_secs(300), false)
            .await
            .unwrap_err(),
        HoldError::Busy
    );
    assert_eq!(
        calls.load(Ordering::SeqCst),
        0,
        "the latch is never asked while Node holds the lease"
    );
    assert!(!m.local_held(), "the local job flag is not left set");
    m.authority.release(&node.id, Some(node.generation));
    let hold = m
        .acquire("r", Duration::from_secs(300), false)
        .await
        .unwrap();
    drop(hold);
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(!m.authority.held(), "a dropped hold gives the lease back");
}

#[tokio::test]
async fn node_chat_busy_or_unreachable_gives_the_authority_back_and_fails_closed() {
    let calls = Arc::new(AtomicUsize::new(0));
    let (busy_port, _) = engine(latch(
        Arc::clone(&calls),
        r#"{"error":"Requests are in progress.","chatActive":true}"#,
        409,
    ))
    .await;
    let (m, _d) = maint(busy_port).await;
    assert_eq!(
        m.acquire("r", Duration::from_secs(300), false)
            .await
            .unwrap_err(),
        HoldError::Busy
    );
    assert!(!m.authority.held());
    // Nothing listening: fails closed, nothing held.
    let gone = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = gone.local_addr().unwrap().port();
    drop(gone);
    let (m, _d) = maint(port).await;
    assert_eq!(
        m.acquire("r", Duration::from_secs(300), false)
            .await
            .unwrap_err(),
        HoldError::NodeUnavailable
    );
    assert!(!m.authority.held());
    assert!(!m.local_held());
}

#[tokio::test]
async fn a_hold_whose_authority_lease_ended_is_fenced_out() {
    // Node renews rarely here (10 s) and the authority's TTL is 300 ms, so the authority's lease
    // ends under the hold: no step may start, exclusion is not claimed, and another holder gets it.
    const SLOW: &str = r#"{"lease":"0123456789abcdef0123456789abcdef","ttlMs":180000,"renewMs":10000,"hardMs":3600000}"#;
    let calls = Arc::new(AtomicUsize::new(0));
    let (node_port, _) = engine(latch(Arc::clone(&calls), SLOW, 201)).await;
    let (mut m, _d) = maint(node_port).await;
    Arc::get_mut(&mut m).unwrap().authority =
        Authority::with_timing(Duration::from_millis(300), lease::HARD, lease::MAX);
    let hold = m
        .acquire("r", Duration::from_secs(300), false)
        .await
        .unwrap();
    assert!(hold.excluded());
    assert!(
        !hold.may_start(Duration::from_secs(1)),
        "300 ms cannot cover a 1 s step"
    );
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert!(
        !hold.excluded(),
        "lost coverage: the hold no longer claims exclusion"
    );
    assert!(!hold.may_start(Duration::ZERO));
    let other = m
        .authority
        .try_acquire(node_origin(&m.authority), None, "tune")
        .unwrap();
    // The old hold's release does not end the newer lease (fencing by id and generation).
    hold.release().await;
    assert!(m
        .authority
        .covers(&other.id, other.generation, Duration::ZERO));
}
