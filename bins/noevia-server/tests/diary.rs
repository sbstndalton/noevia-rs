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
    sidecar_slow_put(std::time::Duration::ZERO).await
}

/// [`sidecar`] whose `PUT` answers only after `put_delay` (a write that takes a while).
async fn sidecar_slow_put(put_delay: std::time::Duration) -> (String, Arc<Mutex<Vec<Seen>>>) {
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
                        let slow = parts.method == hyper::Method::PUT && !put_delay.is_zero();
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
                        if slow {
                            tokio::time::sleep(put_delay).await;
                        }
                        let (ctype, out): (&str, &[u8]) = if target.starts_with("/api/months") {
                            (
                                "application/json",
                                br#"{"months":[{"id":"2026-09"},{"id":"2026-08"}]}"#,
                            )
                        } else if target.starts_with("/api/workspace-export") {
                            ("application/zip", &[80, 75, 0, 255, 9])
                        } else if target.starts_with("/api/storage-status") {
                            ("application/json", br#"{"mode":"legacy"}"#)
                        } else if target.starts_with("/api/files") {
                            ("application/json", br#"{"files":[{"path":"Notes/a.md"}]}"#)
                        } else if target.starts_with("/api/file") {
                            ("application/json", br#"{"content":"hi","version":"v"}"#)
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

/// A sidecar that answers every request with one fixed status and JSON body.
async fn fixed_sidecar(status: u16, body: &'static str) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                return;
            };
            tokio::spawn(async move {
                let service = service_fn(
                    move |req: hyper::Request<hyper::body::Incoming>| async move {
                        let _ = req.into_body().collect().await;
                        Ok::<_, Infallible>(
                            hyper::Response::builder()
                                .status(status)
                                .header("content-type", "application/json")
                                .body(Full::new(bytes::Bytes::from_static(body.as_bytes())))
                                .unwrap(),
                        )
                    },
                );
                let _ = hyper::server::conn::http1::Builder::new()
                    .serve_connection(TokioIo::new(stream), service)
                    .await;
            });
        }
    });
    base
}

/// sbstndalton/noevia#2245 (rs#360 review): the wait of a storage throttle reaches the person as the
/// `Retry-After` header and the body's `retryAfter`, through the front's own reply (`into_response`).
/// The sidecar-client tests assert the reply struct; this is the header on the wire.
#[tokio::test]
async fn a_throttled_diary_read_carries_its_wait_as_retry_after_on_the_wire() {
    let dir = tempfile::tempdir().unwrap();
    seed(dir.path(), true);
    let marker = dir.path().join("users").join(USER);
    std::fs::create_dir_all(&marker).unwrap();
    std::fs::write(marker.join("migration.json"), "{}").unwrap();
    let base = fixed_sidecar(
        503,
        r#"{"detail":"Storage is limiting sign-ins","code":"storageThrottled","retryAfter":7}"#,
    )
    .await;
    let app = app(dir.path(), &base);
    let a = call(&app, "GET", "/api/diary/today?month=2026-10", true, b"").await;
    assert_eq!(a.status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(a.headers.get("retry-after").unwrap(), "7");
    assert_eq!(
        a.body,
        br#"{"error":"Storage is limiting sign-ins","code":"storageThrottled","retryAfter":7}"#
    );
    // A plain outage has no wait to give.
    let down = fixed_sidecar(503, r#"{"detail":"down"}"#).await;
    let app = self::app(dir.path(), &down);
    let a = call(&app, "GET", "/api/diary/today?month=2026-10", true, b"").await;
    assert_eq!(a.status, StatusCode::SERVICE_UNAVAILABLE);
    assert!(a.headers.get("retry-after").is_none());
}

#[test]
fn the_front_lists_the_capabilities_and_owns_exactly_these_routes() {
    assert!(noevia_server::FEATURES.contains(&"rust-diary"));
    assert!(noevia_server::FEATURES.contains(&"rust-diary-routes"));
    for path in [
        "/api/diary/source",
        "/api/diary/today",
        "/api/diary/storage-import",
        "/api/diary/workspace-export",
        "/api/diary/workspace-import",
        "/api/diary",
        "/api/diary-connector",
        "/api/diary/storage-status",
    ] {
        for method in ["GET", "POST", "PUT", "DELETE", "HEAD", "OPTIONS"] {
            assert_eq!(
                routes::dispatch(method, path),
                Some(routes::Native::Diary),
                "{method} {path}"
            );
        }
    }
    for (path, methods) in [
        ("/api/diary/context", ["GET", "POST"].as_slice()),
        ("/api/diary/exchanges", ["GET", "POST"].as_slice()),
        ("/api/profile/diary-connectors", ["GET", "POST"].as_slice()),
        (
            "/api/profile/diary-connectors/0123456789abcdef0123456789abcdef",
            ["DELETE"].as_slice(),
        ),
    ] {
        for method in ["GET", "POST", "PUT", "DELETE", "HEAD", "OPTIONS"] {
            assert_eq!(
                routes::dispatch(method, path),
                methods.contains(&method).then_some(routes::Native::Diary),
                "{method} {path}"
            );
        }
    }
    // Node keeps the local exchange, and the paths the web client never names.
    for path in ["/api/diary/local-exchange", "/api/diary/history"] {
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

/// A legacy data dir not yet claimed for this user: the workspace is not theirs yet, so the front
/// answers the project routes' 503 and never asks the sidecar (Node would claim first).
#[tokio::test]
async fn a_pending_legacy_claim_answers_setting_up_and_never_reaches_the_sidecar() {
    let dir = tempfile::tempdir().unwrap();
    seed(dir.path(), true);
    std::fs::write(dir.path().join("projects.json"), "[]").unwrap();
    let (base, seen) = sidecar().await;
    let app = app(dir.path(), &base);
    let a = call(&app, "GET", "/api/diary/source", true, b"").await;
    assert_eq!(a.status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(
        a.body,
        br#"{"error":"Your account is still being set up; try again in a moment."}"#
    );
    assert!(seen.lock().unwrap().is_empty());
    // Once the claim has happened (the user's directory exists) the same request is served.
    std::fs::create_dir_all(dir.path().join("users").join(USER)).unwrap();
    let a = call(&app, "GET", "/api/diary/source", true, b"").await;
    assert_eq!(a.status, 200);
    assert_eq!(seen.lock().unwrap().len(), 1);
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
    assert_eq!(
        a.status,
        StatusCode::BAD_GATEWAY,
        "the proxy's own answer, not a native one"
    );
    assert!(seen.lock().unwrap().is_empty());
}

// ── sbstndalton/noevia#2206 ────────────────────────────────────────────────────

fn db(dir: &std::path::Path) -> Connection {
    let c = Connection::open(server_store::db_path(dir)).unwrap();
    c.execute_batch("CREATE TABLE IF NOT EXISTS diary_connectors(id TEXT PRIMARY KEY,user_id TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,name TEXT NOT NULL,token_hash TEXT NOT NULL,created_at INTEGER NOT NULL)").unwrap();
    c
}

fn json_of(a: &Answer) -> js_json::JValue {
    js_json::wtf16::parse(std::str::from_utf8(&a.body).unwrap()).unwrap()
}

async fn with_account() -> (tempfile::TempDir, Arc<App>, Arc<Mutex<Vec<Seen>>>) {
    with_account_slow_put(std::time::Duration::ZERO).await
}

async fn with_account_slow_put(
    put_delay: std::time::Duration,
) -> (tempfile::TempDir, Arc<App>, Arc<Mutex<Vec<Seen>>>) {
    let dir = tempfile::tempdir().unwrap();
    seed(dir.path(), true);
    db(dir.path());
    std::fs::create_dir_all(dir.path().join("users").join(USER)).unwrap();
    let (base, seen) = sidecar_slow_put(put_delay).await;
    let app = app(dir.path(), &base);
    (dir, app, seen)
}

#[tokio::test]
async fn the_gate_comes_first_for_the_new_routes_and_a_bare_diary_path_is_not_found() {
    let (_dir, app, seen) = with_account().await;
    for (method, path) in [
        ("GET", "/api/diary"),
        ("GET", "/api/diary/storage-status"),
        ("GET", "/api/diary/exchanges"),
        ("POST", "/api/diary/exchanges"),
        ("GET", "/api/diary/context"),
        ("POST", "/api/diary/context"),
        ("GET", "/api/profile/diary-connectors"),
        ("POST", "/api/profile/diary-connectors"),
        (
            "DELETE",
            "/api/profile/diary-connectors/0123456789abcdef0123456789abcdef",
        ),
    ] {
        let a = call(&app, method, path, false, b"{}").await;
        assert_eq!(a.status, 401, "{method} {path}");
        assert_eq!(a.headers["www-authenticate"], "Bearer realm=\"cowork\"");
    }
    // Signed in, `/api/diary` has no handler in Node: its unknown-path answer, for every method.
    for method in ["GET", "POST", "PUT", "DELETE"] {
        let a = call(&app, method, "/api/diary", true, b"{}").await;
        assert_eq!(
            (a.status.as_u16(), a.body.as_slice()),
            (404, br#"{"error":"not found"}"#.as_slice()),
            "{method}"
        );
        assert_eq!(a.headers["x-noevia-api"], "1");
    }
    assert!(seen.lock().unwrap().is_empty());
}

#[tokio::test]
async fn the_storage_status_shows_the_hold_both_processes_share() {
    let (dir, app, seen) = with_account().await;
    let a = call(&app, "GET", "/api/diary/storage-status", true, b"").await;
    assert_eq!(
        (a.status.as_u16(), a.body.as_slice()),
        (200, br#"{"mode":"legacy"}"#.as_slice())
    );
    // A refused login recorded the way Node's backup worker records it (the same file, the same
    // fingerprint of the account's saved connection: none saved, so local).
    let fingerprint = server_projects::storage_latch::fingerprint("local", "", "", "", "");
    server_projects::storage_latch::note(
        dir.path(),
        USER,
        &fingerprint,
        server_projects::source_storage::Failure {
            kind: "login",
            retry_after: None,
        },
        server_projects::wall_ms(),
    )
    .unwrap();
    let a = call(&app, "GET", "/api/diary/storage-status", true, b"").await;
    let v = json_of(&a);
    assert_eq!(a.status, 200);
    assert_eq!(v.get("mode").as_str(), Some("legacy"));
    let held = v.get("automaticStorage");
    assert_eq!(held.get("held").as_bool(), Some(true));
    assert_eq!(held.get("kind").as_str(), Some("login"));
    assert!(held.get("nextAttemptAt").as_num().unwrap() > 1.0e12);
    // Another method is the route's own 405, after the add-on.
    let a = call(&app, "POST", "/api/diary/storage-status", true, b"{}").await;
    assert_eq!(
        (a.status.as_u16(), a.body.as_slice()),
        (405, br#"{"error":"Method not allowed"}"#.as_slice())
    );
    assert_eq!(seen.lock().unwrap().len(), 2);
    assert!(seen
        .lock()
        .unwrap()
        .iter()
        .all(|r| r.target == "/api/storage-status"));
}

#[tokio::test]
async fn the_exchanges_use_the_start_the_supervisor_gave_both_processes() {
    let (dir, app, _seen) = with_account().await;
    let day = dir
        .path()
        .join("users")
        .join(USER)
        .join("diary-conversations")
        .join("2026-10-30");
    std::fs::create_dir_all(&day).unwrap();
    std::fs::write(
        day.join("exchange-aaaaaaaaaaaaaaaa.json"),
        r#"{"id":"exchange-aaaaaaaaaaaaaaaa","state":"running","startedAt":1}"#,
    )
    .unwrap();
    // The app was built without NOEVIA_START_ID: nothing can be shown to belong to a live Node.
    let a = call(
        &app,
        "GET",
        "/api/diary/exchanges?day=2026-10-30",
        true,
        b"",
    )
    .await;
    assert_eq!(a.status, 200);
    assert_eq!(
        json_of(&a).get("exchanges"),
        &js_json::wtf16::parse(
            r#"[{"id":"exchange-aaaaaaaaaaaaaaaa","state":"uncertain","startedAt":1}]"#
        )
        .unwrap()
    );
    let a = call(&app, "GET", "/api/diary/exchanges", true, b"").await;
    assert_eq!(
        (a.status.as_u16(), a.body.as_slice()),
        (400, br#"{"error":"Valid diary day required."}"#.as_slice())
    );
    let a = call(
        &app,
        "POST",
        "/api/diary/exchanges",
        true,
        br#"{"action":"dismiss","day":"2026-10-30","id":"exchange-aaaaaaaaaaaaaaaa"}"#,
    )
    .await;
    assert_eq!(a.status, 200);
    assert_eq!(
        json_of(&a).get("exchange").get("state").as_str(),
        Some("uncertain")
    );
    // The add-on switch comes after the gate and before the day.
    let off = tempfile::tempdir().unwrap();
    seed(off.path(), false);
    let (base, _) = sidecar().await;
    let app = self::app(off.path(), &base);
    let a = call(&app, "GET", "/api/diary/exchanges", true, b"").await;
    assert_eq!(
        (a.status.as_u16(), a.body.as_slice()),
        (404, br#"{"error":"Diary add-on is disabled"}"#.as_slice())
    );
}

#[tokio::test]
async fn the_context_is_made_once_in_the_persons_projects_and_needs_the_addon() {
    let (dir, app, _seen) = with_account().await;
    let a = call(&app, "GET", "/api/diary/context", true, b"").await;
    assert_eq!(
        (a.status.as_u16(), a.body.as_slice()),
        (200, br#"{"project":null}"#.as_slice())
    );
    let a = call(&app, "POST", "/api/diary/context", true, b"").await;
    assert_eq!(a.status, 200);
    let v = json_of(&a);
    assert_eq!(
        v.get("project").get("id").as_str(),
        Some("cowork-diary-extras")
    );
    assert_eq!(
        v.get("project").get("name").as_str(),
        Some("Diary attachments")
    );
    let listed =
        std::fs::read_to_string(dir.path().join("users").join(USER).join("projects.json")).unwrap();
    assert_eq!(listed.matches("cowork-diary-extras").count(), 1);
    let again = call(&app, "POST", "/api/diary/context", true, b"").await;
    assert_eq!(json_of(&again).get("project"), v.get("project"));
    assert_eq!(
        std::fs::read_to_string(dir.path().join("users").join(USER).join("projects.json")).unwrap(),
        listed
    );
    let off = tempfile::tempdir().unwrap();
    seed(off.path(), false);
    let (base, _) = sidecar().await;
    let app = self::app(off.path(), &base);
    let a = call(&app, "GET", "/api/diary/context", true, b"").await;
    assert_eq!(
        (a.status.as_u16(), a.body.as_slice()),
        (404, br#"{"error":"Diary add-on is disabled"}"#.as_slice())
    );
    assert!(!off
        .path()
        .join("users")
        .join(USER)
        .join("projects.json")
        .exists());
}

#[tokio::test]
async fn connectors_are_made_listed_and_revoked_by_the_person_and_each_use_is_audited() {
    let (dir, app, seen) = with_account().await;
    let a = call(&app, "GET", "/api/profile/diary-connectors", true, b"").await;
    assert_eq!(
        (a.status.as_u16(), a.body.as_slice()),
        (200, br#"{"connectors":[]}"#.as_slice())
    );
    let a = call(
        &app,
        "POST",
        "/api/profile/diary-connectors",
        true,
        br#"{"name":" Phone "}"#,
    )
    .await;
    assert_eq!(a.status, 201);
    let made = json_of(&a);
    let (id, token) = (
        made.get("id").as_str().unwrap().to_owned(),
        made.get("token").as_str().unwrap().to_owned(),
    );
    assert_eq!(a.headers["cache-control"], "no-store");
    let a = call(&app, "GET", "/api/profile/diary-connectors", true, b"").await;
    assert!(std::str::from_utf8(&a.body).unwrap().starts_with(&format!(
        r#"{{"connectors":[{{"id":"{id}","name":"Phone","createdAt":"#
    )));
    assert!(!std::str::from_utf8(&a.body).unwrap().contains(&token));
    // Bad requests, in Node's words, with a status that says what to change.
    for (body, status) in [
        (&br#"{"name":""}"#[..], 400),
        (b"{}", 400),
        (b"[]", 400),
        (b"{broken", 400),
    ] {
        let a = call(&app, "POST", "/api/profile/diary-connectors", true, body).await;
        assert_eq!(a.status, status, "{}", String::from_utf8_lossy(body));
    }

    // The credential opens the file bridge with no session and no origin.
    let bridge = |body: &str| {
        let req = Request::builder()
            .method("POST")
            .uri("/api/diary-connector")
            .header("authorization", format!("Bearer {token}"))
            .body(Body::from(body.to_owned()))
            .unwrap();
        (req, body.to_owned())
    };
    let (req, _) = bridge(r#"{"action":"list","path":"Notes"}"#);
    let res = diary_routes::serve(&app, conn(), req, "/api/diary-connector".into()).await;
    assert_eq!(res.status(), 200);
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    assert_eq!(bytes.as_ref(), br#"{"files":[{"path":"Notes/a.md"}]}"#);
    let (req, _) =
        bridge(r#"{"action":"write","path":"Notes/a.md","content":"hello","version":null}"#);
    let res = diary_routes::serve(&app, conn(), req, "/api/diary-connector".into()).await;
    assert_eq!(res.status(), 200);
    let audit: Vec<(String, String)> = {
        let c = db(dir.path());
        let mut st = c
            .prepare("SELECT action,detail FROM audit_events ORDER BY id")
            .unwrap();
        st.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .map(Result::unwrap)
            .collect()
    };
    assert_eq!(audit[0].0, "diary-connector.create");
    assert_eq!(
        audit[1],
        (
            "diary-connector.write".to_owned(),
            format!(r#"{{"credentialId":"{id}","path":"Notes/a.md","bytes":5}}"#)
        )
    );
    assert!(!audit
        .iter()
        .any(|(_, d)| d.contains("hello") || d.contains(&token)));
    let wrote = seen.lock().unwrap().last().unwrap().clone();
    assert_eq!(
        (wrote.method.as_str(), wrote.target.as_str()),
        ("PUT", "/api/file")
    );
    assert_eq!(wrote.headers["x-cowork-user-id"], USER);

    // Revoked: the same credential no longer works, and the revoke is audited.
    let a = call(
        &app,
        "DELETE",
        &format!("/api/profile/diary-connectors/{id}"),
        true,
        b"",
    )
    .await;
    assert_eq!(
        (a.status.as_u16(), a.body.as_slice()),
        (200, br#"{"revoked":true}"#.as_slice())
    );
    let a = call(
        &app,
        "DELETE",
        &format!("/api/profile/diary-connectors/{id}"),
        true,
        b"",
    )
    .await;
    assert_eq!(a.body, br#"{"revoked":false}"#);
    let (req, _) = bridge(r#"{"action":"list"}"#);
    let res = diary_routes::serve(&app, conn(), req, "/api/diary-connector".into()).await;
    assert_eq!(res.status(), 401);
    // A malformed id is no connector route: the unknown-path answer, after the gate.
    let a = call(
        &app,
        "DELETE",
        "/api/profile/diary-connectors/not-an-id",
        true,
        b"",
    )
    .await;
    assert_eq!(
        (a.status.as_u16(), a.body.as_slice()),
        (404, br#"{"error":"not found"}"#.as_slice())
    );
}

/// Review M1 (sbstndalton/noevia#2206): the connector's write is audited once the sidecar has it,
/// whether or not the client is still there to read the answer. The client here goes away while the
/// sidecar is still writing; the handler's future is dropped, and the row must still appear.
#[tokio::test]
async fn a_connector_write_is_audited_even_when_the_client_disconnects_after_the_sidecar_has_it() {
    let (dir, app, seen) = with_account_slow_put(std::time::Duration::from_millis(600)).await;
    let a = call(
        &app,
        "POST",
        "/api/profile/diary-connectors",
        true,
        br#"{"name":"Phone"}"#,
    )
    .await;
    let made = json_of(&a);
    let (id, token) = (
        made.get("id").as_str().unwrap().to_owned(),
        made.get("token").as_str().unwrap().to_owned(),
    );
    let req = Request::builder()
        .method("POST")
        .uri("/api/diary-connector")
        .header("authorization", format!("Bearer {token}"))
        .body(Body::from(
            r#"{"action":"write","path":"Notes/a.md","content":"hello","version":null}"#,
        ))
        .unwrap();
    let served = {
        let app = Arc::clone(&app);
        tokio::spawn(async move {
            diary_routes::serve(&app, conn(), req, "/api/diary-connector".into()).await
        })
    };
    // The sidecar has the write (its PUT is in the log and is still being answered) ...
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    while !seen.lock().unwrap().iter().any(|s| s.method == "PUT") {
        assert!(
            std::time::Instant::now() < deadline,
            "the write never reached the sidecar"
        );
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    // ... and the client goes away: the handler's future is dropped.
    served.abort();
    assert!(served.await.unwrap_err().is_cancelled());
    let rows = |dir: &std::path::Path| -> Vec<String> {
        let c = db(dir);
        let mut st = c
            .prepare("SELECT detail FROM audit_events WHERE action='diary-connector.write'")
            .unwrap();
        st.query_map([], |r| r.get(0))
            .unwrap()
            .map(Result::unwrap)
            .collect()
    };
    assert!(
        rows(dir.path()).is_empty(),
        "not audited before the write is done"
    );
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    let audited = loop {
        let got = rows(dir.path());
        if !got.is_empty() || std::time::Instant::now() > deadline {
            break got;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    };
    assert_eq!(
        audited,
        vec![format!(
            r#"{{"credentialId":"{id}","path":"Notes/a.md","bytes":5}}"#
        )]
    );
}

#[tokio::test]
async fn the_connector_endpoint_is_checked_before_anything_else_and_throttled_by_bucket() {
    let (_dir, app, seen) = with_account().await;
    let post = |auth: Option<&str>, origin: bool, method: &str| {
        let mut req = Request::builder()
            .method(method)
            .uri("/api/diary-connector");
        if let Some(a) = auth {
            req = req.header("authorization", a);
        }
        if origin {
            req = req.header("origin", ORIGIN);
        }
        req.body(Body::from("{}")).unwrap()
    };
    let go = |req| async {
        let res = diary_routes::serve(&app, conn(), req, "/api/diary-connector".into()).await;
        let status = res.status().as_u16();
        let headers = res.headers().clone();
        let body = res.into_body().collect().await.unwrap().to_bytes().to_vec();
        (status, headers, String::from_utf8(body).unwrap())
    };
    // Method, then origin, both before the credential is looked at.
    let (s, h, b) = go(post(None, false, "GET")).await;
    assert_eq!((s, b.as_str()), (405, r#"{"error":"POST required"}"#));
    assert_eq!(h["cache-control"], "no-store");
    let (s, _, b) = go(post(Some("Bearer x"), true, "POST")).await;
    assert_eq!(
        (s, b.as_str()),
        (403, r#"{"error":"Use the authenticated connector client"}"#)
    );
    // An empty Origin header is no origin (core tested `req.headers.origin` for truthiness).
    let mut empty = post(Some("Bearer nope"), false, "POST");
    empty
        .headers_mut()
        .insert("origin", axum::http::HeaderValue::from_static(""));
    let (s, _, _) = go(empty).await;
    assert_eq!(s, 401);
    // No credential, a session-looking one, a malformed one: all the same 401, no challenge header.
    for auth in [
        None,
        Some("Bearer nope"),
        Some("nv_diary_x"),
        Some("Basic abc"),
    ] {
        let (s, h, b) = go(post(auth, false, "POST")).await;
        assert_eq!(
            (s, b.as_str()),
            (401, r#"{"error":"Diary connector credential required"}"#),
            "{auth:?}"
        );
        assert!(h.get("www-authenticate").is_none());
    }
    // Unknown credentials, whatever they are, share one bucket (TRUST_PROXY is off): the 301st in a minute is a 429.
    let mut limited = None;
    for n in 0..320 {
        let (s, _, _) = go(post(Some(&format!("Bearer nope-{n}")), false, "POST")).await;
        if s == 429 {
            limited = Some(n);
            break;
        }
    }
    assert!(limited.is_some_and(|n| n >= 290), "{limited:?}");
    assert!(seen.lock().unwrap().is_empty());
}

#[tokio::test]
async fn a_working_connector_is_never_locked_out_by_unknown_credentials_and_has_its_own_limit() {
    let (_dir, app, _seen) = with_account().await;
    let a = call(
        &app,
        "POST",
        "/api/profile/diary-connectors",
        true,
        br#"{"name":"Phone"}"#,
    )
    .await;
    let token = json_of(&a).get("token").as_str().unwrap().to_owned();
    let go = |auth: String| {
        let app = Arc::clone(&app);
        async move {
            let req = Request::builder()
                .method("POST")
                .uri("/api/diary-connector")
                .header("authorization", auth)
                .body(Body::from(r#"{"action":"list"}"#))
                .unwrap();
            diary_routes::serve(&app, conn(), req, "/api/diary-connector".into())
                .await
                .status()
                .as_u16()
        }
    };
    // Different unknown credentials: one shared bucket (a bucket each would never fill).
    for n in 0..310 {
        go(format!("Bearer nope-{n}")).await;
    }
    assert_eq!(go("Bearer nope-last".into()).await, 429);
    assert_eq!(
        go(format!("Bearer {token}")).await,
        200,
        "the credential has a bucket of its own"
    );
    let mut over = None;
    for n in 0..130 {
        if go(format!("Bearer {token}")).await == 429 {
            over = Some(n);
            break;
        }
    }
    assert!(over.is_some_and(|n| n >= 115), "{over:?}: 120 a minute");
}
