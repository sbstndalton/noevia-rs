//! The Diary routes the front answers itself (sbstndalton/noevia#2185), through the real gate
//! against a synthetic Node database and a synthetic Diary sidecar on loopback. The sidecar client's
//! own contract (headers, assertion, 428, caps, deadlines) is crates/server-diary/tests/sidecar.rs;
//! this file proves the wiring: the gate and its order, the add-on switch, the reply headers, the
//! environment, and what stays Node's.
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
use noevia_server::{config::Config, diary_routes, routes, serve::Conn, App};
use server_store::rusqlite::Connection;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::convert::Infallible;
use std::sync::{Arc, Mutex};

const SCHEMA: &str = include_str!("../../../crates/server-store/tests/fixtures/node-schema.sql");
const USER: &str = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa";
const SESSION: &str = "synthetic-diary-session-0001";
const CSRF: &str = "synthetic-diary-csrf";
const ORIGIN: &str = "https://front.example.test";
const KEY: &str = "synthetic-diary-tenant-key";
const BEARER: &str = "synthetic-diary-bearer";

fn hex(s: &str) -> String {
    Sha256::digest(s.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

#[derive(Clone, Debug)]
struct Seen {
    method: String,
    target: String,
    headers: HashMap<String, String>,
    body: Vec<u8>,
}

/// A sidecar that answers `/api/months`, `/api/workspace-export` and `/api/workspace-import`.
async fn sidecar() -> (String, Arc<Mutex<Vec<Seen>>>) {
    let seen = Arc::new(Mutex::new(Vec::<Seen>::new()));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let log = Arc::clone(&seen);
    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                return;
            };
            let log = Arc::clone(&log);
            tokio::spawn(async move {
                let service = service_fn(move |req: hyper::Request<hyper::body::Incoming>| {
                    let log = Arc::clone(&log);
                    async move {
                        let (parts, body) = req.into_parts();
                        let body = body.collect().await.unwrap().to_bytes().to_vec();
                        let target = parts.uri.path_and_query().unwrap().as_str().to_owned();
                        log.lock().unwrap().push(Seen {
                            method: parts.method.to_string(),
                            target: target.clone(),
                            headers: parts
                                .headers
                                .iter()
                                .map(|(k, v)| {
                                    (k.as_str().to_owned(), v.to_str().unwrap().to_owned())
                                })
                                .collect(),
                            body,
                        });
                        let (ctype, out): (&str, &[u8]) = if target.starts_with("/api/months") {
                            (
                                "application/json",
                                br#"{"months":[{"id":"2026-09"},{"id":"2026-08"}]}"#,
                            )
                        } else if target.starts_with("/api/workspace-export") {
                            ("application/zip", &[80, 75, 0, 255, 9])
                        } else {
                            ("application/json", br#"{"fingerprint":"reviewed"}"#)
                        };
                        Ok::<_, Infallible>(
                            hyper::Response::builder()
                                .header("content-type", ctype)
                                .body(Full::new(bytes::Bytes::from_static(out)))
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
    (base, seen)
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
}

fn app(dir: &std::path::Path, base: &str) -> Arc<App> {
    let mut vars = HashMap::<String, String>::new();
    vars.insert("NOEVIA_LEGACY_UPSTREAM".into(), "http://127.0.0.1:9".into());
    vars.insert("UI_DATA_DIR".into(), dir.display().to_string());
    vars.insert("PUBLIC_ORIGIN".into(), ORIGIN.into());
    vars.insert("DIARY_BASE_URL".into(), base.into());
    vars.insert("DIARY_AUTH_TOKEN".into(), BEARER.into());
    vars.insert("DIARY_TENANT_KEY".into(), KEY.into());
    App::new(Config::from_lookup(|key| vars.get(key).cloned()).unwrap())
}

fn conn() -> Conn {
    Conn {
        peer: "127.0.0.1:1".parse().unwrap(),
        local: Some("127.0.0.1:8021".parse().unwrap()),
    }
}

struct Answer {
    status: StatusCode,
    headers: axum::http::HeaderMap,
    body: Vec<u8>,
}

async fn call(app: &Arc<App>, method: &str, path: &str, signed_in: bool, body: &[u8]) -> Answer {
    let mut req = Request::builder().method(method).uri(path);
    if signed_in {
        req = req
            .header(
                "cookie",
                format!("cowork_session={SESSION}; cowork_csrf={CSRF}"),
            )
            .header("x-csrf-token", CSRF)
            .header("origin", ORIGIN);
    }
    let req = req.body(Body::from(body.to_vec())).unwrap();
    let route_path = path.split('?').next().unwrap().to_owned();
    let res = diary_routes::serve(app, conn(), req, route_path).await;
    let status = res.status();
    let headers = res.headers().clone();
    let body = res.into_body().collect().await.unwrap().to_bytes().to_vec();
    Answer {
        status,
        headers,
        body,
    }
}

#[test]
fn the_front_lists_the_capability_and_owns_exactly_the_five_routes() {
    assert!(noevia_server::FEATURES.contains(&"rust-diary"));
    for path in [
        "/api/diary/source",
        "/api/diary/today",
        "/api/diary/storage-import",
        "/api/diary/workspace-export",
        "/api/diary/workspace-import",
    ] {
        for method in ["GET", "POST", "PUT", "DELETE", "HEAD", "OPTIONS"] {
            assert_eq!(
                routes::dispatch(method, path),
                Some(routes::Native::Diary),
                "{method} {path}"
            );
        }
    }
    for path in [
        "/api/diary",
        "/api/diary-connector",
        "/api/diary/context",
        "/api/diary/exchanges",
        "/api/diary/local-exchange",
        "/api/diary/storage-status",
        "/api/profile/diary-connectors",
        "/api/diary/history",
    ] {
        assert_eq!(routes::dispatch("GET", path), None, "{path}");
        assert_eq!(routes::dispatch("POST", path), None, "{path}");
    }
}

#[test]
fn an_unsupported_diary_source_stops_startup_as_it_stops_node() {
    let mut vars = HashMap::<String, String>::new();
    vars.insert("NOEVIA_LEGACY_UPSTREAM".into(), "http://127.0.0.1:9".into());
    vars.insert("UI_DATA_DIR".into(), "/data".into());
    vars.insert("DIARY_SOURCE".into(), "local".into());
    let err = Config::from_lookup(|key| vars.get(key).cloned()).unwrap_err();
    assert!(err.contains("Unsupported DIARY_SOURCE 'local'"));
    vars.remove("DIARY_SOURCE");
    assert!(Config::from_lookup(|key| vars.get(key).cloned()).is_ok());
}

#[tokio::test]
async fn the_gate_comes_first_and_the_addon_switch_second() {
    let dir = tempfile::tempdir().unwrap();
    seed(dir.path(), false);
    let (base, seen) = sidecar().await;
    let app = app(dir.path(), &base);
    // Signed out: Node's 401 with its challenge, nothing sent anywhere.
    for (method, path) in [
        ("GET", "/api/diary/source"),
        ("GET", "/api/diary/today"),
        ("GET", "/api/diary/workspace-export"),
        ("POST", "/api/diary/workspace-import"),
        ("POST", "/api/diary/storage-import"),
    ] {
        let a = call(&app, method, path, false, b"").await;
        assert_eq!(a.status, 401, "{method} {path}");
        assert_eq!(a.headers["www-authenticate"], "Bearer realm=\"cowork\"");
        assert_eq!(a.body, br#"{"error":"unauthorized"}"#);
    }
    // A write without the CSRF token is refused by the gate before the add-on is looked at.
    let req = Request::builder()
        .method("POST")
        .uri("/api/diary/storage-import")
        .header("cookie", format!("cowork_session={SESSION}"))
        .header("origin", ORIGIN)
        .body(Body::from("{}"))
        .unwrap();
    let res = diary_routes::serve(&app, conn(), req, "/api/diary/storage-import".into()).await;
    assert_eq!(res.status(), 403);
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    assert_eq!(bytes.as_ref(), br#"{"error":"invalid CSRF token"}"#);
    // Signed in without the add-on: 404 with Node's reply headers, and still nothing sent.
    for (method, path) in [
        ("GET", "/api/diary/source"),
        ("GET", "/api/diary/today"),
        ("GET", "/api/diary/workspace-export"),
        ("POST", "/api/diary/workspace-import"),
        ("POST", "/api/diary/storage-import"),
        ("GET", "/api/diary/storage-import"),
    ] {
        let a = call(&app, method, path, true, b"{}").await;
        assert_eq!(a.status, 404, "{method} {path}");
        assert_eq!(a.body, br#"{"error":"Diary add-on is disabled"}"#);
        assert_eq!(a.headers["content-type"], "application/json");
        assert_eq!(a.headers["cache-control"], "no-store");
        assert_eq!(a.headers["x-noevia-api"], "1");
        assert_eq!(a.headers["x-content-type-options"], "nosniff");
        assert_eq!(a.headers["x-frame-options"], "DENY");
    }
    assert!(seen.lock().unwrap().is_empty());
}

#[tokio::test]
async fn with_the_addon_the_front_speaks_to_the_sidecar_for_the_signed_in_person() {
    let dir = tempfile::tempdir().unwrap();
    seed(dir.path(), true);
    let marker = dir.path().join("users").join(USER);
    std::fs::create_dir_all(&marker).unwrap();
    std::fs::write(marker.join("migration.json"), "{}").unwrap();
    let (base, seen) = sidecar().await;
    let app = app(dir.path(), &base);

    let a = call(&app, "GET", "/api/diary/source", true, b"").await;
    assert_eq!(a.status, 200);
    assert_eq!(
        a.body,
        br#"{"source":"sidecar","months":[{"id":"2026-08","label":"2026-08"},{"id":"2026-09","label":"2026-09"}]}"#
    );
    let request = seen.lock().unwrap()[0].clone();
    assert_eq!(request.target, "/api/months");
    assert_eq!(request.headers["authorization"], format!("Bearer {BEARER}"));
    assert_eq!(request.headers["x-cowork-user-id"], USER);
    assert_eq!(request.headers["x-cowork-legacy-owner"], "1");
    let assertion = &request.headers["x-cowork-tenant-assertion"];
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs_f64();
    tenant_assertion::verify(&tenant_assertion::Request {
        key: KEY,
        user_id: USER,
        assertion,
        method: "GET",
        path: "/api/months",
        query: b"",
        body_hash: &hex(""),
        storage: &request.headers["x-cowork-storage"],
        legacy_owner: "1",
        blocked: "",
        now,
    })
    .unwrap();

    // The archive: bytes unchanged and core's download headers.
    let a = call(&app, "GET", "/api/diary/workspace-export", true, b"").await;
    assert_eq!(a.status, 200);
    assert_eq!(a.body, [80, 75, 0, 255, 9]);
    assert_eq!(a.headers["content-type"], "application/zip");
    assert_eq!(
        a.headers["content-disposition"],
        "attachment; filename=\"noevia-workspace.zip\""
    );
    assert_eq!(a.headers["cache-control"], "no-store");
    assert_eq!(a.headers["x-content-type-options"], "nosniff");
    assert_eq!(a.headers["x-noevia-api"], "1");

    // An upload: through CSRF and origin, body and query to the sidecar, answer relayed.
    let zip = [80u8, 75, 3, 4, 0, 255];
    let a = call(
        &app,
        "POST",
        "/api/diary/workspace-import?action=preview&name=Folder",
        true,
        &zip,
    )
    .await;
    assert_eq!(
        (a.status, a.body.as_slice()),
        (StatusCode::OK, br#"{"fingerprint":"reviewed"}"#.as_slice())
    );
    let upload = seen.lock().unwrap().last().unwrap().clone();
    assert_eq!(upload.method, "POST");
    assert_eq!(
        upload.target,
        "/api/workspace-import?action=preview&name=Folder"
    );
    assert_eq!(upload.body, zip);
    assert_eq!(upload.headers["content-type"], "application/zip");

    // Method guards, after the add-on gate.
    for (method, path) in [
        ("GET", "/api/diary/storage-import"),
        ("POST", "/api/diary/workspace-export"),
        ("GET", "/api/diary/workspace-import"),
    ] {
        let a = call(&app, method, path, true, b"").await;
        assert_eq!(a.status, 405, "{method} {path}");
        assert_eq!(a.body, br#"{"error":"Method not allowed"}"#);
    }
    let sent = seen.lock().unwrap().len();
    assert_eq!(sent, 3, "refusals never reached the sidecar");
}

#[tokio::test]
async fn a_trailing_slash_is_not_the_route_and_goes_to_node_as_before() {
    let dir = tempfile::tempdir().unwrap();
    seed(dir.path(), true);
    let (base, seen) = sidecar().await;
    let app = app(dir.path(), &base);
    // Node's exact matcher does not know it; here the proxy gets it (nothing listens upstream, so
    // the proxy's own failure answers) and the sidecar is never asked.
    let a = call(&app, "GET", "/api/diary/source/", true, b"").await;
    assert_ne!(a.status, 200);
    assert!(seen.lock().unwrap().is_empty());
}
