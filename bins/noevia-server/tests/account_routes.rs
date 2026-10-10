//! M3: with NOEVIA_RUST_AUTH=1 the front answers sign-in and the account itself: the switched
//! routes never reach Node, what the route module hands on still does (with its body), a
//! dot-segment path is routed on its WHATWG pathname like Node, and without the switch every one
//! of them is Node's. Synthetic data only.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]

use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::body::Incoming;
use hyper::{Request, Response};
use noevia_server::config::Config;
use noevia_server::serve::{serve, Limits};
use server_store::rusqlite::Connection;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::Path;
use std::sync::{Arc, Mutex};
use tokio::net::{TcpListener, TcpStream};

const SCHEMA: &str = include_str!("../../../crates/server-store/tests/fixtures/node-schema.sql");
const CODE: &str = "synthetic-front-setup-code";

type Hits = Arc<Mutex<Vec<String>>>;

async fn upstream() -> (SocketAddr, Hits) {
    let hits: Hits = Arc::default();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let h = Arc::clone(&hits);
    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                return;
            };
            let h = Arc::clone(&h);
            tokio::spawn(async move {
                let svc = hyper::service::service_fn(move |r: Request<Incoming>| {
                    let h = Arc::clone(&h);
                    async move {
                        let line = format!("{} {}", r.method(), r.uri().path());
                        let body = r.into_body().collect().await.unwrap().to_bytes();
                        h.lock().unwrap().push(format!("{line} {}", body.len()));
                        Ok::<_, std::convert::Infallible>(
                            Response::builder()
                                .header("x-upstream", "1")
                                .body(Full::new(Bytes::from_static(b"{}")))
                                .unwrap(),
                        )
                    }
                });
                let _ = hyper::server::conn::http1::Builder::new()
                    .serve_connection(hyper_util::rt::TokioIo::new(stream), svc)
                    .await;
            });
        }
    });
    (addr, hits)
}

fn seed(dir: &Path) {
    let c = Connection::open(server_store::db_path(dir)).unwrap();
    c.execute_batch(SCHEMA).unwrap();
    c.execute(
        "INSERT INTO settings VALUES('setup_code_hash', ?1)",
        [server_auth::js::digest(CODE)],
    )
    .unwrap();
}

async fn front(up: SocketAddr, data: &Path, rust_auth: bool) -> SocketAddr {
    let mut env: HashMap<&str, String> = HashMap::new();
    env.insert("NOEVIA_LEGACY_UPSTREAM", format!("http://{up}"));
    env.insert("UI_PORT", "1".into());
    env.insert("UI_DATA_DIR", data.display().to_string());
    env.insert("NOEVIA_WEB_DIST", data.join("dist").display().to_string());
    env.insert("PUBLIC_ORIGIN", "http://127.0.0.1:18021".into());
    if rust_auth {
        env.insert("NOEVIA_RUST_AUTH", "1".into());
    }
    let app = noevia_server::App::new(Config::from_lookup(|k| env.get(k).cloned()).unwrap());
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(serve(
        listener,
        app,
        Limits::default(),
        std::future::pending(),
    ));
    addr
}

async fn call(
    addr: SocketAddr,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
    body: &str,
) -> (u16, HashMap<String, Vec<String>>, String) {
    let stream = TcpStream::connect(addr).await.unwrap();
    let (mut send, conn) =
        hyper::client::conn::http1::handshake(hyper_util::rt::TokioIo::new(stream))
            .await
            .unwrap();
    tokio::spawn(conn);
    let mut b = Request::builder()
        .method(method)
        .uri(path)
        .header("host", "front.test");
    for (k, v) in headers {
        b = b.header(*k, *v);
    }
    let res = send
        .send_request(
            b.body(Full::new(Bytes::from(body.to_string())).boxed())
                .unwrap(),
        )
        .await
        .unwrap();
    let status = res.status().as_u16();
    let mut h: HashMap<String, Vec<String>> = HashMap::new();
    for (k, v) in res.headers() {
        h.entry(k.to_string())
            .or_default()
            .push(v.to_str().unwrap().to_string());
    }
    let text =
        String::from_utf8(res.into_body().collect().await.unwrap().to_bytes().to_vec()).unwrap();
    (status, h, text)
}

#[tokio::test]
async fn switched_routes_are_answered_by_the_front() {
    let data = tempfile::tempdir().unwrap();
    seed(data.path());
    let (up, hits) = upstream().await;
    let addr = front(up, data.path(), true).await;
    let origin = ("origin", "http://127.0.0.1:18021");

    let (s, h, body) = call(addr, "GET", "/api/setup/status", &[], "").await;
    assert_eq!(s, 200, "{body}");
    assert!(!h.contains_key("x-upstream"));
    assert_eq!(
        body,
        r#"{"configured":false,"publicOrigin":"http://127.0.0.1:18021"}"#
    );
    assert_eq!(h["x-noevia-api"], vec!["1"]);
    assert_eq!(h["cache-control"], vec!["no-store"]);

    let setup = format!(
        r#"{{"setupCode":"{CODE}","username":"owner","password":"synthetic front password","diaryEnabled":false}}"#
    );
    let (s, h, body) = call(
        addr,
        "POST",
        "/api/setup/complete",
        &[origin, ("content-type", "application/json")],
        &setup,
    )
    .await;
    assert_eq!(s, 201, "{body}");
    let cookies = &h["set-cookie"];
    assert_eq!(cookies.len(), 2);
    let session = cookies[0].split(';').next().unwrap().to_string();
    let csrf_pair = cookies[1].split(';').next().unwrap().to_string();
    let csrf = csrf_pair.split_once('=').unwrap().1.to_string();
    let jar = format!("{session}; {csrf_pair}");

    let (s, _, body) = call(addr, "GET", "/api/auth/session", &[("cookie", &jar)], "").await;
    assert_eq!(s, 200, "{body}");
    assert!(body.contains(&format!(r#""csrfToken":"{csrf}""#)));
    // Routed on the WHATWG pathname, as Node routes.
    let (s, h, _) = call(
        addr,
        "GET",
        "/api/profile/../auth/session",
        &[("cookie", &jar)],
        "",
    )
    .await;
    assert_eq!(s, 200);
    assert!(!h.contains_key("x-upstream"));
    // A foreign origin is refused before any sign-in work.
    let (s, _, body) = call(
        addr,
        "POST",
        "/api/auth/login/password",
        &[("origin", "https://evil.example.test")],
        "{}",
    )
    .await;
    assert_eq!(
        (s, body.as_str()),
        (403, r#"{"error":"origin not allowed"}"#)
    );
    // A path shape the account mount does not answer goes on to Node, body included.
    let w = [
        ("cookie", jar.as_str()),
        ("x-csrf-token", csrf.as_str()),
        origin,
    ];
    let (s, h, _) = call(
        addr,
        "DELETE",
        "/api/profile/app-passwords/not-an-id",
        &w,
        "{\"x\":1}",
    )
    .await;
    assert_eq!(s, 200);
    assert_eq!(h["x-upstream"], vec!["1"]);
    // Methods a switched route does not list are Node's too.
    let (_, h, _) = call(addr, "DELETE", "/api/profile/onboarding", &w, "").await;
    assert_eq!(h["x-upstream"], vec!["1"]);
    let (s, _, _) = call(addr, "POST", "/api/auth/logout", &w, "").await;
    assert_eq!(s, 200);

    // http.cjs readJson: a body over 1 MiB is a 413 with Node's message.
    let big = format!("{{\"username\":\"{}\"}}", "x".repeat(1024 * 1024));
    let (s, _, body) = call(addr, "POST", "/api/auth/login/password", &[origin], &big).await;
    assert_eq!(
        (s, body.as_str()),
        (413, r#"{"error":"Request exceeds size limit"}"#)
    );

    let seen = hits.lock().unwrap().clone();
    assert_eq!(
        seen,
        vec![
            "DELETE /api/profile/app-passwords/not-an-id 7".to_string(),
            "DELETE /api/profile/onboarding 0".to_string(),
        ]
    );
    // What Node no longer writes, the front did.
    let c = Connection::open(server_store::db_path(data.path())).unwrap();
    let audits: i64 = c
        .query_row(
            "SELECT count(*) FROM audit_events WHERE action='setup.complete'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(audits, 1);
}

#[tokio::test]
async fn without_the_switch_node_answers_all_of_them() {
    let data = tempfile::tempdir().unwrap();
    seed(data.path());
    let (up, hits) = upstream().await;
    let addr = front(up, data.path(), false).await;
    for (m, p) in [
        ("GET", "/api/setup/status"),
        ("POST", "/api/setup/complete"),
        ("POST", "/api/auth/login/password"),
        ("GET", "/api/auth/session"),
        ("POST", "/api/auth/device/token"),
        ("GET", "/api/account/memory"),
    ] {
        let (_, h, _) = call(addr, m, p, &[], "").await;
        assert_eq!(h["x-upstream"], vec!["1"], "{m} {p}");
    }
    assert_eq!(hits.lock().unwrap().len(), 6);
}
