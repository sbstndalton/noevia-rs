//! `/api/diary/local-exchange` through the front: the real gate, the real inference-admission lease
//! client against a synthetic Node (core `inference-admission-lease.cjs`, ask `diaryExchange`), and a
//! synthetic Diary sidecar. The relay's own contract is crates/server-diary/tests/local_exchange.rs;
//! this file proves the wiring: nothing is held before the gate, the add-on and the method; the lease
//! names the signed-in account; Node's refusals keep their words; the lease ends with the response.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::{BodyExt, Full};
use hyper::service::service_fn;
use hyper_util::rt::TokioIo;
use noevia_server::{config::Config, diary_routes, serve::Conn, App};
use server_store::rusqlite::Connection;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::convert::Infallible;
use std::sync::{Arc, Mutex};
use std::time::Duration;

const SCHEMA: &str = include_str!("../../../crates/server-store/tests/fixtures/node-schema.sql");
const USER: &str = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa";
const SESSION: &str = "synthetic-lx-session-0001";
const CSRF: &str = "synthetic-lx-csrf";
const ORIGIN: &str = "https://front.example.test";
const STORE_TOKEN: &str = "synthetic-store-token-0123456789abcdef";

fn hex(s: &str) -> String {
    Sha256::digest(s.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

type Log = Arc<Mutex<Vec<String>>>;
/// `(path, body) -> (status, body, content type)`.
type Answer = Arc<dyn Fn(&str, &str) -> (u16, String, &'static str) + Send + Sync>;

/// A loopback server: each request is logged as `METHOD path body`, then answered by `answer`.
async fn server(answer: Answer) -> (String, Log) {
    let log: Log = Arc::default();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let seen = Arc::clone(&log);
    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                return;
            };
            let (seen, answer) = (Arc::clone(&seen), Arc::clone(&answer));
            tokio::spawn(async move {
                let service = service_fn(move |req: hyper::Request<hyper::body::Incoming>| {
                    let (seen, answer) = (Arc::clone(&seen), Arc::clone(&answer));
                    async move {
                        let (parts, body) = req.into_parts();
                        let token = parts
                            .headers
                            .get("x-noevia-store-token")
                            .is_some_and(|v| v == STORE_TOKEN);
                        let body = body.collect().await.unwrap().to_bytes();
                        let body = String::from_utf8_lossy(&body).into_owned();
                        let path = parts.uri.path().to_owned();
                        seen.lock().unwrap().push(format!(
                            "{} {path} {body}{}",
                            parts.method,
                            if token { "" } else { " (no token)" }
                        ));
                        let (status, text, ctype) = answer(&path, &body);
                        Ok::<_, Infallible>(
                            hyper::Response::builder()
                                .status(status)
                                .header("content-type", ctype)
                                .body(Full::new(bytes::Bytes::from(text)))
                                .unwrap(),
                        )
                    }
                });
                let _ = hyper::server::conn::http1::Builder::new()
                    .serve_connection(TokioIo::new(stream), service)
                    .await;
            });
        }
    });
    (base, log)
}

const LEASE_PATH: &str = "/internal/inference-admission/lease";

/// Row H: this process issues the lease id and Node's latch takes it at `PUT <path>/<id>`; a Node start
/// is registered with the authority first, as the real session route does.
fn session() {
    engine_admission::authority::global()
        .node_session("diary-local-exchange-test", 1)
        .unwrap();
}

/// The log's line for the lease's take (`PUT .../<id> <body>`), with the id the authority issued.
fn taken(held: &Log) -> Vec<String> {
    held.lock()
        .unwrap()
        .iter()
        .filter(|l| l.starts_with(&format!("PUT {LEASE_PATH}/")))
        .cloned()
        .collect()
}

fn released(held: &Log) -> bool {
    held.lock()
        .unwrap()
        .iter()
        .any(|l| l.starts_with(&format!("DELETE {LEASE_PATH}/")))
}

/// Node's lease route: `grant` for the ask (a 201 grants the id the front issued), then renew and release.
async fn node(grant: (u16, &'static str)) -> (String, Log) {
    session();
    server(Arc::new(move |path, body| {
        if path.ends_with("/renew") {
            (200, r#"{"ttlMs":30000}"#.into(), "application/json")
        } else if path.starts_with(&format!("{LEASE_PATH}/")) && !body.is_empty() {
            let id = path.rsplit('/').next().unwrap_or("");
            let reply = if grant.0 == 201 {
                format!(r#"{{"lease":"{id}","ttlMs":30000}}"#)
            } else {
                grant.1.to_owned()
            };
            (grant.0, reply, "application/json")
        } else {
            (204, String::new(), "application/json")
        }
    }))
    .await
}

fn seed(dir: &std::path::Path, diary: bool) {
    let db = Connection::open(server_store::db_path(dir)).unwrap();
    db.execute_batch(SCHEMA).unwrap();
    let now = noevia_server::identity::now_ms();
    db.execute("INSERT INTO users(id,username,username_norm,display_name,role,password_hash,webauthn_user_id,created_at,updated_at) VALUES(?1,'ada','ada','Synthetic','member','x','w',1,1)", [USER]).unwrap();
    if diary {
        db.execute(
            "INSERT INTO user_features(user_id,diary_enabled,updated_at) VALUES(?1,1,1)",
            [USER],
        )
        .unwrap();
    }
    db.execute(
        "INSERT INTO sessions VALUES(?1,?2,?3,?4,?4,?5,'ua','127.0.0.1')",
        (hex(SESSION), USER, hex(CSRF), now, now + 86_400_000),
    )
    .unwrap();
    let marker = dir.join("users").join(USER);
    std::fs::create_dir_all(&marker).unwrap();
    std::fs::write(marker.join("migration.json"), "{}").unwrap();
}

fn app(dir: &std::path::Path, node: &str, sidecar: &str) -> Arc<App> {
    app_with(dir, node, sidecar, &[])
}

fn app_with(dir: &std::path::Path, node: &str, sidecar: &str, extra: &[(&str, &str)]) -> Arc<App> {
    let mut vars = HashMap::<String, String>::new();
    for (k, v) in extra {
        vars.insert((*k).into(), (*v).into());
    }
    vars.insert("NOEVIA_LEGACY_UPSTREAM".into(), node.into());
    vars.insert("NOEVIA_PROJECT_STORE_TOKEN".into(), STORE_TOKEN.into());
    vars.insert("UI_DATA_DIR".into(), dir.display().to_string());
    vars.insert("PUBLIC_ORIGIN".into(), ORIGIN.into());
    vars.insert("DIARY_BASE_URL".into(), sidecar.into());
    vars.insert("DIARY_AUTH_TOKEN".into(), "synthetic-bearer".into());
    vars.insert("DIARY_TENANT_KEY".into(), "synthetic-tenant-key".into());
    App::new(Config::from_lookup(|key| vars.get(key).cloned()).unwrap())
}

fn conn() -> Conn {
    Conn {
        peer: "127.0.0.1:1".parse().unwrap(),
        local: Some("127.0.0.1:8021".parse().unwrap()),
    }
}

async fn call(
    app: &Arc<App>,
    method: &str,
    signed_in: bool,
    body: &[u8],
) -> axum::http::Response<Body> {
    let mut req = Request::builder()
        .method(method)
        .uri("/api/diary/local-exchange");
    if signed_in {
        req = req
            .header(
                "cookie",
                format!("cowork_session={SESSION}; cowork_csrf={CSRF}"),
            )
            .header("x-csrf-token", CSRF)
            .header("origin", ORIGIN);
    }
    diary_routes::serve(
        app,
        conn(),
        req.body(Body::from(body.to_vec())).unwrap(),
        "/api/diary/local-exchange".into(),
    )
    .await
}

async fn text(res: axum::http::Response<Body>) -> String {
    String::from_utf8(res.into_body().collect().await.unwrap().to_bytes().to_vec()).unwrap()
}

async fn settle(check: impl Fn() -> bool) -> bool {
    for _ in 0..100 {
        if check() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    check()
}

const GRANT: &str = "";

#[tokio::test]
async fn nothing_is_held_before_the_gate_the_addon_and_the_method() {
    let dir = tempfile::tempdir().unwrap();
    seed(dir.path(), false);
    let (node, held) = node((201, GRANT)).await;
    let (sidecar, heard) = server(Arc::new(|_, _| (200, "{}".into(), "application/json"))).await;
    let app = app(dir.path(), &node, &sidecar);
    assert_eq!(
        call(&app, "POST", false, b"{}").await.status(),
        StatusCode::UNAUTHORIZED
    );
    let off = call(&app, "POST", true, b"{}").await;
    assert_eq!(off.status(), StatusCode::NOT_FOUND);
    assert_eq!(text(off).await, r#"{"error":"Diary add-on is disabled"}"#);
    let dir2 = tempfile::tempdir().unwrap();
    seed(dir2.path(), true);
    let app2 = self::app(dir2.path(), &node, &sidecar);
    let get = call(&app2, "GET", true, b"").await;
    assert_eq!(get.status(), StatusCode::METHOD_NOT_ALLOWED);
    assert_eq!(text(get).await, r#"{"error":"Method not allowed"}"#);
    assert!(held.lock().unwrap().is_empty(), "no lease asked");
    assert!(heard.lock().unwrap().is_empty(), "no sidecar call");
}

#[tokio::test]
async fn node_refusals_keep_their_status_and_words_and_the_sidecar_hears_nothing() {
    let dir = tempfile::tempdir().unwrap();
    seed(dir.path(), true);
    let (sidecar, heard) = server(Arc::new(|_, _| (200, "{}".into(), "application/json"))).await;
    for (grant, status, body) in [
        (
            (
                429,
                r#"{"error":"Please wait before sending another message"}"#,
            ),
            429,
            r#"{"error":"Please wait before sending another message"}"#,
        ),
        (
            (
                503,
                r#"{"error":"Model configuration is being applied. Try again shortly.","code":"inferenceMaintenance"}"#,
            ),
            503,
            r#"{"error":"Model configuration is being applied. Try again shortly."}"#,
        ),
    ] {
        let (node, held) = node(grant).await;
        let app = app(dir.path(), &node, &sidecar);
        let res = call(&app, "POST", true, br#"{"message":"hi","stream":true}"#).await;
        assert_eq!(res.status().as_u16(), status);
        assert_eq!(res.headers().get("cache-control").unwrap(), "no-store");
        assert_eq!(text(res).await, body);
        let taken = taken(&held);
        assert_eq!(taken.len(), 1, "{taken:?}");
        assert!(
            taken[0].ends_with(&format!(r#" {{"diaryExchange":{{"user":"{USER}"}}}}"#)),
            "{taken:?}"
        );
    }
    assert!(heard.lock().unwrap().is_empty());
}

#[tokio::test]
async fn the_lease_names_the_account_and_ends_with_the_stream() {
    let dir = tempfile::tempdir().unwrap();
    seed(dir.path(), true);
    let (node, held) = node((201, GRANT)).await;
    let (sidecar, heard) = server(Arc::new(|_, _| {
        (
            200,
            "data: {\"type\":\"status\",\"text\":\"Opening diary context…\"}\n\ndata: {\"type\":\"done\"}\n\n".into(),
            "text/event-stream",
        )
    }))
    .await;
    let app = app(dir.path(), &node, &sidecar);
    let res = call(&app, "POST", true, br#"{"message":"hi","stream":true}"#).await;
    assert_eq!(res.status(), StatusCode::OK);
    let h = res.headers();
    assert_eq!(h.get("content-type").unwrap(), "text/event-stream");
    assert_eq!(h.get("cache-control").unwrap(), "no-cache, no-transform");
    assert_eq!(h.get("x-accel-buffering").unwrap(), "no");
    assert_eq!(h.get("x-content-type-options").unwrap(), "nosniff");
    assert_eq!(
        text(res).await,
        "data: {\"type\":\"status\",\"text\":\"Connecting to diary companion…\"}\n\ndata: {\"type\":\"status\",\"text\":\"Opening diary context…\"}\n\ndata: {\"type\":\"done\"}\n\n"
    );
    assert!(
        settle(|| released(&held)).await,
        "{:?}",
        held.lock().unwrap()
    );
    let taken = taken(&held);
    assert_eq!(taken.len(), 1, "{taken:?}");
    assert!(
        taken[0].ends_with(&format!(r#" {{"diaryExchange":{{"user":"{USER}"}}}}"#)),
        "{taken:?}"
    );
    let heard = heard.lock().unwrap().clone();
    assert_eq!(
        heard,
        vec![r#"POST /api/local-exchange {"message":"hi","stream":true} (no token)"#.to_owned()]
    );
}

#[tokio::test]
async fn an_unstreamed_exchange_relays_the_reply_and_gives_the_lease_back() {
    let dir = tempfile::tempdir().unwrap();
    seed(dir.path(), true);
    let (node, held) = node((201, GRANT)).await;
    let (sidecar, _heard) = server(Arc::new(|_, _| {
        (200, r#"{"reply":"ok"}"#.into(), "application/json")
    }))
    .await;
    let app = app(dir.path(), &node, &sidecar);
    let res = call(&app, "POST", true, br#"{"message":"hi"}"#).await;
    assert_eq!(res.status(), StatusCode::OK);
    assert_eq!(
        res.headers().get("content-type").unwrap(),
        "application/json"
    );
    assert_eq!(text(res).await, r#"{"reply":"ok"}"#);
    assert!(settle(|| released(&held)).await);
}

/// Chat port S6: the `llm:` budget the exchange shares with chat is the front's. Node's lease counts
/// nothing; the front counts a call once the gate let it in, and a call over the budget gives the
/// lease back before its 429 (core routes/diary.cjs's words) and never reaches the sidecar.
#[tokio::test]
async fn the_front_counts_the_shared_llm_budget_after_the_gate_and_gives_the_lease_back() {
    let dir = tempfile::tempdir().unwrap();
    seed(dir.path(), true);
    let (node, held) = node((201, GRANT)).await;
    let (sidecar, heard) = server(Arc::new(|_, _| {
        (200, r#"{"reply":"ok"}"#.into(), "application/json")
    }))
    .await;
    let app = app_with(dir.path(), &node, &sidecar, &[("LLM_RATE_LIMIT", "1")]);
    let first = call(&app, "POST", true, br#"{"message":"hi"}"#).await;
    assert_eq!(first.status(), StatusCode::OK);
    let _ = text(first).await;
    let second = call(&app, "POST", true, br#"{"message":"hi"}"#).await;
    assert_eq!(second.status(), StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(
        text(second).await,
        r#"{"error":"Please wait before sending another message"}"#
    );
    let release = format!("DELETE /internal/inference-admission/lease/{LEASE} ");
    assert!(
        settle(|| held
            .lock()
            .unwrap()
            .iter()
            .filter(|l| **l == release)
            .count()
            == 2)
        .await,
        "{:?}",
        held.lock().unwrap()
    );
    assert_eq!(
        heard.lock().unwrap().len(),
        1,
        "only the first call reached the sidecar"
    );
}
