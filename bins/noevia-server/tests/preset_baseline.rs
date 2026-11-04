//! The preset reload baseline's authority over HTTP (port contract 2026-11-04 RS-1): the six
//! `POST /v1/preset-baseline/*` routes answer on the project-store listener only, behind its secret,
//! and never on the public front. Synthetic data only; no engine is touched.
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
use noevia_server::model_maint::reload::{Guard, Timing};
use noevia_server::project_store::{self, preset_baseline_routes, Store, TOKEN_HEADER};
use noevia_server::{
    config::Config,
    serve::{serve, Limits},
    App,
};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tower::ServiceExt;

const TOKEN: &str = "synthetic-preset-baseline-secret-000000";
const START: &str = "0123456789abcdef0123456789abcdef";
const BASE: &str = "[*]\ncache-ram = 512\n\n[A]\nmodel = /models/a.gguf\n";

fn store(dir: &std::path::Path) -> Store {
    Store {
        data_dir: dir.into(),
        token: TOKEN.into(),
        switch: server_store::RustProjects::owned(),
        start_id: START.into(),
    }
}

async fn call(
    app: &axum::Router,
    path: &str,
    token: Option<&str>,
    body: &str,
) -> (StatusCode, Value) {
    let mut req = Request::builder()
        .method("POST")
        .uri(path)
        .header("content-type", "application/json");
    if let Some(t) = token {
        req = req.header(TOKEN_HEADER, t);
    }
    let res = app
        .clone()
        .oneshot(req.body(Body::from(body.to_owned())).unwrap())
        .await
        .unwrap();
    let status = res.status();
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

#[tokio::test]
async fn every_route_needs_the_store_secret_and_touches_nothing_without_it() {
    let d = tempfile::tempdir().unwrap();
    let file = d.path().join("native-preset-reload-x.json");
    let guard = Guard::with(Some(file.clone()), START, Timing::default());
    let app = preset_baseline_routes(store(d.path()), Some(guard.clone()));
    for op in [
        "session",
        "begin",
        "settle",
        "invalidate",
        "verdict",
        "router-text",
    ] {
        let path = format!("/v1/preset-baseline/{op}");
        let body =
            r#"{"origin":"node","nodeBoot":"b","protocol":1,"ticket":"t","outcome":"forget"}"#;
        for token in [None, Some("wrong-secret-0000000000000000000000000")] {
            let (status, answer) = call(&app, &path, token, body).await;
            assert_eq!(status, StatusCode::UNAUTHORIZED, "{op}");
            assert_eq!(answer, json!({"error": "unauthorised"}));
        }
    }
    assert!(!file.exists(), "nothing was written");
    assert!(!guard.bridged(), "no session was opened");
    // With the secret: lossless JSON objects only.
    let (status, _) = call(&app, "/v1/preset-baseline/begin", Some(TOKEN), "[1]").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (status, _) = call(&app, "/v1/preset-baseline/begin", Some(TOKEN), "{").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (status, answer) = call(
        &app,
        "/v1/preset-baseline/session",
        Some(TOKEN),
        r#"{"nodeBoot":"boot-1","protocol":1}"#,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(answer, json!({"epoch": START, "protocol": 1}));
    assert!(guard.bridged());
    // Without the front's model maintenance there is nothing to answer.
    let none = preset_baseline_routes(store(d.path()), None);
    let (status, _) = call(&none, "/v1/preset-baseline/verdict", Some(TOKEN), "{}").await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
}

/// A raw POST to `addr` (status, body).
async fn post(addr: std::net::SocketAddr, path: &str, token: &str, body: &str) -> (u16, String) {
    let mut s = TcpStream::connect(addr).await.unwrap();
    s.write_all(
        format!(
            "POST {path} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\nContent-Type: application/json\r\n{TOKEN_HEADER}: {token}\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        )
        .as_bytes(),
    )
    .await
    .unwrap();
    let mut bytes = Vec::new();
    tokio::time::timeout(Duration::from_secs(60), s.read_to_end(&mut bytes))
        .await
        .unwrap()
        .unwrap();
    let text = String::from_utf8(bytes).unwrap();
    let (h, b) = text.split_once("\r\n\r\n").unwrap();
    (
        h.split_whitespace().nth(1).unwrap().parse().unwrap(),
        b.to_owned(),
    )
}

#[tokio::test]
async fn the_store_listener_serves_the_fronts_authority_and_the_public_front_does_not() {
    let d = tempfile::tempdir().unwrap();
    seed_database(d.path());
    let base = "http://127.0.0.1:9";
    let file = d
        .path()
        .join(format!("native-preset-reload-{}.json", suffix(base)));
    let public = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = public.local_addr().unwrap();
    let env: HashMap<String, String> = HashMap::from([
        ("NOEVIA_LEGACY_UPSTREAM".into(), "http://127.0.0.1:9".into()),
        ("UI_DATA_DIR".into(), d.path().display().to_string()),
        ("TRUST_PROXY".into(), "true".into()),
        ("NOEVIA_FEATURE_NATIVE_CLIENT_AUTH".into(), "1".into()),
        ("PUBLIC_ORIGIN".into(), format!("http://{addr}")),
        ("NOEVIA_START_ID".into(), START.into()),
        ("MODEL_MANAGER_BASE_URL".into(), base.into()),
    ]);
    let app = App::new(Config::from_lookup(|k| env.get(k).cloned()).unwrap());
    assert_eq!(app.config.maint.guard.file(), Some(file.as_path()));
    tokio::spawn(serve(
        public,
        Arc::clone(&app),
        Limits::default(),
        std::future::pending(),
    ));
    let sl = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let store_addr = sl.local_addr().unwrap();
    tokio::spawn(project_store::serve_with_context(
        sl,
        store(d.path()),
        None,
        Some(Arc::clone(&app)),
        std::future::pending(),
    ));

    // Through the public front, even with the secret, nothing native answers (it is proxied to
    // Node, which has no such route; here the dead upstream) and the authority is untouched.
    for op in ["session", "begin", "invalidate"] {
        let (status, _) = post(
            addr,
            &format!("/v1/preset-baseline/{op}"),
            TOKEN,
            r#"{"origin":"node","nodeBoot":"b","protocol":1}"#,
        )
        .await;
        assert_ne!(status, 200, "{op} on the public front");
    }
    assert!(!file.exists());
    assert!(!app.config.maint.guard.bridged());

    // On the store listener: a whole Node reload, then the reads.
    let (status, body) = post(
        store_addr,
        "/v1/preset-baseline/session",
        TOKEN,
        r#"{"nodeBoot":"boot-1","protocol":1}"#,
    )
    .await;
    assert_eq!(
        (status, body.as_str()),
        (
            200,
            format!(r#"{{"epoch":"{START}","protocol":1}}"#).as_str()
        )
    );
    let (status, body) = post(
        store_addr,
        "/v1/preset-baseline/begin",
        TOKEN,
        r#"{"origin":"node"}"#,
    )
    .await;
    assert_eq!(status, 200);
    let begun: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(begun["epoch"], START);
    let ticket = begun["ticket"].as_str().unwrap();
    let rows = json!([{"id": "A", "status": {"value": "loaded", "preset": "p"}}]);
    // A read while the reload is in flight is unknown.
    let (_, body) = post(
        store_addr,
        "/v1/preset-baseline/router-text",
        TOKEN,
        &json!({"rows": rows}).to_string(),
    )
    .await;
    assert_eq!(body, r#"{"text":null}"#);
    let settle =
        json!({"ticket": ticket, "outcome": "record", "before": BASE, "rows": rows, "after": BASE});
    let (status, body) = post(
        store_addr,
        "/v1/preset-baseline/settle",
        TOKEN,
        &settle.to_string(),
    )
    .await;
    assert_eq!(status, 200);
    assert!(body.contains(r#""saved":true"#), "{body}");
    assert_eq!(
        std::fs::read_to_string(&file).unwrap(),
        json!({"text": BASE, "view": r#"[["A","p"]]"#}).to_string()
    );
    assert!(
        app.config.maint.guard.known(),
        "the front's own readers see it"
    );
    let (_, body) = post(
        store_addr,
        "/v1/preset-baseline/verdict",
        TOKEN,
        &json!({"current": BASE, "rows": rows, "loaded": ["A"]}).to_string(),
    )
    .await;
    assert_eq!(body, r#"{"safe":true,"reason":"unchanged"}"#);
    let (status, body) = post(
        store_addr,
        "/v1/preset-baseline/settle",
        TOKEN,
        &settle.to_string(),
    )
    .await;
    assert_eq!(status, 409);
    assert!(body.contains(r#""code":"stale_ticket""#), "{body}");
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "{}");
    let (status, _) = post(store_addr, "/v1/preset-baseline/session", "wrong", "{}").await;
    assert_eq!(status, 401);
}
