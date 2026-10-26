//! Synthetic Node database contract for web-address GETs and administrator POST writers.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use noevia_server::{config::Config, routes, web_address_routes, App};
use server_store::rusqlite::Connection;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::sync::Arc;

const SCHEMA: &str = include_str!("../../../crates/server-store/tests/fixtures/node-schema.sql");
const SESSION: &str = "synthetic-address-session-0001";
const DEVICE: &str = "nva_synthetic-address-device-0001";
const CSRF: &str = "synthetic-address-csrf";

fn hex(s: &str) -> String {
    Sha256::digest(s.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn seed(dir: &std::path::Path) -> Connection {
    let db = Connection::open(server_store::db_path(dir)).unwrap();
    db.execute_batch(SCHEMA).unwrap();
    let now = i64::try_from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis(),
    )
    .unwrap();
    db.execute("INSERT INTO users(id,username,username_norm,display_name,role,password_hash,webauthn_user_id,created_at,updated_at) VALUES('u-1','Admin','admin','Synthetic','admin','x','w',1,1)", []).unwrap();
    db.execute(
        "INSERT INTO sessions VALUES(?1,'u-1',?2,?3,?3,?4,'ua','127.0.0.1')",
        (hex(SESSION), hex(CSRF), now, now + 86_400_000),
    )
    .unwrap();
    db.execute(
        "INSERT INTO device_grants VALUES('g-1','u-1','Mac',?1,?1,?2,'ip','ua')",
        (now, now + 86_400_000),
    )
    .unwrap();
    db.execute(
        "INSERT INTO device_tokens VALUES(?1,'g-1','access',?2,?3,NULL,NULL)",
        (hex(DEVICE), now, now + 3_600_000),
    )
    .unwrap();
    db
}

fn app(dir: &std::path::Path) -> Arc<App> {
    let mut vars = HashMap::<String, String>::new();
    vars.insert("NOEVIA_LEGACY_UPSTREAM".into(), "http://127.0.0.1:9".into());
    vars.insert("UI_DATA_DIR".into(), dir.display().to_string());
    vars.insert("PUBLIC_ORIGIN".into(), "https://env.example.test".into());
    vars.insert("WEBAUTHN_RP_ID".into(), "example.test".into());
    vars.insert("TRUST_PROXY".into(), "true".into());
    vars.insert("NOEVIA_FEATURE_NATIVE_CLIENT_AUTH".into(), "true".into());
    App::new(Config::from_lookup(|key| vars.get(key).cloned()).unwrap())
}

async fn get(app: &Arc<App>, path: &str, auth: Option<(&str, &str)>) -> (StatusCode, String) {
    let mut req = Request::builder().method("GET").uri(path);
    if let Some((name, value)) = auth {
        req = req.header(name, value);
    }
    let res = web_address_routes::serve(app, req.body(Body::empty()).unwrap(), path.into()).await;
    let status = res.status();
    let body =
        String::from_utf8(res.into_body().collect().await.unwrap().to_bytes().to_vec()).unwrap();
    (status, body)
}

fn put(db: &Connection, key: &str, value: &str) {
    db.execute(
        "INSERT OR REPLACE INTO settings(key,value) VALUES(?1,?2)",
        [key, value],
    )
    .unwrap();
}

#[tokio::test]
async fn public_gets_and_admin_view_follow_live_node_writes() {
    let dir = tempfile::tempdir().unwrap();
    let db = seed(dir.path());
    let app = app(dir.path());
    put(&db, "instance_id", "synthetic-instance");
    put(&db, "public_origin", "https://setup.example.test");
    put(
        &db,
        "previous_origins",
        r#"["https://old.example.test","https://env.example.test","https://old.example.test",4]"#,
    );
    assert_eq!(
        get(&app, "/api/instance", None).await,
        (StatusCode::OK, r#"{"id":"synthetic-instance"}"#.into())
    );
    assert_eq!(
        get(&app, "/.well-known/webauthn", None).await,
        (
            StatusCode::OK,
            r#"{"origins":["https://env.example.test","https://old.example.test"]}"#.into()
        )
    );
    let cookie = format!("cowork_session={SESSION}");
    assert_eq!(get(&app,"/api/admin/web-address",Some(("cookie",&cookie))).await,
        (StatusCode::OK,r#"{"origin":"https://env.example.test","source":"environment","previous":["https://old.example.test","https://env.example.test","https://old.example.test"],"rpId":"example.test"}"#.into()));

    // A synthetic external writer changes settings under the running Rust process. The next GET sees it.
    put(&db, "public_origin_admin", "https://renamed.other.test");
    put(
        &db,
        "previous_origins",
        r#"["https://env.example.test","https://old.example.test"]"#,
    );
    assert_eq!(get(&app,"/api/admin/web-address",Some(("cookie",&cookie))).await,
        (StatusCode::OK,r#"{"origin":"https://renamed.other.test","source":"settings","previous":["https://env.example.test","https://old.example.test"],"rpId":"renamed.other.test"}"#.into()));
    assert_eq!(get(&app,"/.well-known/webauthn",None).await,
        (StatusCode::OK,r#"{"origins":["https://renamed.other.test","https://env.example.test","https://old.example.test"]}"#.into()));
    put(&db, "previous_origins", r#"["https://old.example.test"]"#);
    assert_eq!(
        get(&app, "/.well-known/webauthn", None).await,
        (
            StatusCode::OK,
            r#"{"origins":["https://renamed.other.test","https://old.example.test"]}"#.into()
        )
    );

    put(&db, "previous_origins", "malformed");
    assert_eq!(
        get(&app, "/.well-known/webauthn", None).await,
        (
            StatusCode::OK,
            r#"{"origins":["https://renamed.other.test"]}"#.into()
        )
    );
}

#[tokio::test]
async fn admin_gate_and_missing_instance_fail_closed_without_a_write() {
    let dir = tempfile::tempdir().unwrap();
    let db = seed(dir.path());
    let app = app(dir.path());
    let (status, _) = get(&app, "/api/instance", None).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    let count: i64 = db
        .query_row(
            "SELECT count(*) FROM settings WHERE key='instance_id'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(count, 0);
    assert_eq!(
        get(&app, "/api/admin/web-address", None).await,
        (
            StatusCode::UNAUTHORIZED,
            r#"{"error":"unauthorized"}"#.into()
        )
    );
    let bearer = format!("Bearer {DEVICE}");
    assert_eq!(get(&app,"/api/admin/web-address",Some(("authorization",&bearer))).await,
        (StatusCode::FORBIDDEN,r#"{"error":"This needs a signed-in browser session.","code":"browser_session_required"}"#.into()));
    db.execute("UPDATE users SET role='member' WHERE id='u-1'", [])
        .unwrap();
    let cookie = format!("cowork_session={SESSION}");
    assert_eq!(
        get(&app, "/api/admin/web-address", Some(("cookie", &cookie))).await,
        (
            StatusCode::FORBIDDEN,
            r#"{"error":"Administrator required"}"#.into()
        )
    );
    for path in [
        "/api/instance",
        "/.well-known/webauthn",
        "/api/admin/web-address",
    ] {
        assert_eq!(
            routes::dispatch("GET", path),
            Some(routes::Native::WebAddress)
        );
        for method in ["HEAD", "PUT", "OPTIONS"] {
            assert_eq!(routes::dispatch(method, path), None);
        }
    }
    assert_eq!(routes::dispatch("POST", "/api/instance"), None);
    assert_eq!(routes::dispatch("POST", "/.well-known/webauthn"), None);
    assert_eq!(
        routes::dispatch("POST", "/api/admin/web-address"),
        Some(routes::Native::WebAddress)
    );
    assert_eq!(
        routes::dispatch("POST", "/api/admin/web-address/remove"),
        Some(routes::Native::WebAddress)
    );
}

#[tokio::test]
async fn setup_and_no_origin_views_match_node_precedence() {
    let dir = tempfile::tempdir().unwrap();
    let db = seed(dir.path());
    let mut vars = HashMap::<String, String>::new();
    vars.insert("NOEVIA_LEGACY_UPSTREAM".into(), "http://127.0.0.1:9".into());
    vars.insert("UI_DATA_DIR".into(), dir.path().display().to_string());
    vars.insert("WEBAUTHN_RP_ID".into(), "example.test".into());
    let app = App::new(Config::from_lookup(|key| vars.get(key).cloned()).unwrap());
    let cookie = format!("cowork_session={SESSION}");
    put(&db, "public_origin", "https://setup.example.test");
    assert_eq!(get(&app, "/api/admin/web-address", Some(("cookie", &cookie))).await,
        (StatusCode::OK, r#"{"origin":"https://setup.example.test","source":"setup","previous":[],"rpId":"example.test"}"#.into()));
    db.execute("DELETE FROM settings WHERE key='public_origin'", [])
        .unwrap();
    assert_eq!(
        get(&app, "/api/admin/web-address", Some(("cookie", &cookie))).await,
        (
            StatusCode::OK,
            r#"{"origin":"","source":"none","previous":[],"rpId":"localhost"}"#.into()
        )
    );
    assert_eq!(
        get(&app, "/.well-known/webauthn", None).await,
        (StatusCode::OK, r#"{"origins":[]}"#.into())
    );
    put(&db, "public_origin_admin", "https://admin.other.test");
    assert_eq!(get(&app, "/api/admin/web-address", Some(("cookie", &cookie))).await,
        (StatusCode::OK, r#"{"origin":"https://admin.other.test","source":"settings","previous":[],"rpId":"admin.other.test"}"#.into()));
}

async fn post(
    app: &Arc<App>,
    path: &str,
    json: &str,
    origin: &str,
) -> (StatusCode, serde_json::Value) {
    let cookie = format!("cowork_session={SESSION}; cowork_csrf={CSRF}");
    let request = Request::builder()
        .method("POST")
        .uri(path)
        .header("cookie", cookie)
        .header("x-csrf-token", CSRF)
        .header("origin", origin)
        .body(Body::from(json.to_owned()))
        .unwrap();
    let res = web_address_routes::serve(app, request, path.into()).await;
    let status = res.status();
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    (status, serde_json::from_slice(&bytes).unwrap())
}

#[tokio::test]
async fn forced_rename_and_remove_write_settings_passkey_and_audit_atomically() {
    let dir = tempfile::tempdir().unwrap();
    let db = seed(dir.path());
    put(&db, "instance_id", "synthetic-instance");
    db.execute("INSERT INTO passkeys(id,user_id,name,public_key,webauthn_user_id,counter,device_type,backed_up,transports,created_at,rp_id) VALUES('pk','u-1','k',?1,'w',0,'singleDevice',0,'[]',1,NULL)", [vec![1_u8]]).unwrap();
    let app = app(dir.path());
    let (status, body) = post(
        &app,
        "/api/admin/web-address",
        r#"{"origin":"https://new.example.test","force":true}"#,
        "https://env.example.test",
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        body.get("origin"),
        Some(&serde_json::json!("https://new.example.test"))
    );
    assert_eq!(
        body.get("previous"),
        Some(&serde_json::json!(["https://env.example.test"]))
    );
    assert_eq!(
        db.query_row("SELECT rp_id FROM passkeys WHERE id='pk'", [], |r| r
            .get::<_, String>(0))
            .unwrap(),
        "example.test"
    );
    assert_eq!(
        db.query_row(
            "SELECT count(*) FROM audit_events WHERE action='settings.public_origin'",
            [],
            |r| r.get::<_, i64>(0)
        )
        .unwrap(),
        1
    );
    assert_eq!(
        get(
            &app,
            "/api/admin/web-address",
            Some(("cookie", &format!("cowork_session={SESSION}")))
        )
        .await
        .0,
        StatusCode::OK
    );
    let (status, body) = post(
        &app,
        "/api/admin/web-address/remove",
        r#"{"origin":"https://env.example.test"}"#,
        "https://new.example.test",
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        body.get("removed"),
        Some(&serde_json::json!("https://env.example.test"))
    );
    assert_eq!(body.get("previous"), Some(&serde_json::json!([])));
    assert_eq!(db.query_row("SELECT count(*) FROM audit_events WHERE action='settings.public_origin.earlier_removed'", [], |r| r.get::<_,i64>(0)).unwrap(), 1);
}

#[tokio::test]
async fn audit_failure_rolls_back_both_settings_and_legacy_passkey_freeze() {
    let dir = tempfile::tempdir().unwrap();
    let db = seed(dir.path());
    put(&db, "instance_id", "synthetic-instance");
    db.execute("INSERT INTO passkeys(id,user_id,name,public_key,webauthn_user_id,counter,device_type,backed_up,transports,created_at,rp_id) VALUES('pk','u-1','k',?1,'w',0,'singleDevice',0,'[]',1,NULL)", [vec![1_u8]]).unwrap();
    db.execute_batch("CREATE TRIGGER fail_address_audit BEFORE INSERT ON audit_events WHEN NEW.action='settings.public_origin' BEGIN SELECT RAISE(ABORT,'synthetic audit failure'); END;").unwrap();
    let app = app(dir.path());
    let (status, _) = post(
        &app,
        "/api/admin/web-address",
        r#"{"origin":"https://new.example.test","force":true}"#,
        "https://env.example.test",
    )
    .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(
        db.query_row(
            "SELECT count(*) FROM settings WHERE key IN ('public_origin_admin','previous_origins')",
            [],
            |r| r.get::<_, i64>(0)
        )
        .unwrap(),
        0
    );
    assert_eq!(
        db.query_row(
            "SELECT count(*) FROM passkeys WHERE id='pk' AND rp_id IS NULL",
            [],
            |r| r.get::<_, i64>(0)
        )
        .unwrap(),
        1
    );
    assert_eq!(
        db.query_row(
            "SELECT count(*) FROM audit_events WHERE action='settings.public_origin'",
            [],
            |r| r.get::<_, i64>(0)
        )
        .unwrap(),
        0
    );
}

#[tokio::test]
async fn force_uses_node_truthiness_and_false_values_require_probe() {
    for (force, expected) in [
        ("false", StatusCode::CONFLICT),
        ("null", StatusCode::CONFLICT),
        ("0", StatusCode::CONFLICT),
        ("\"\"", StatusCode::CONFLICT),
        ("\"0\"", StatusCode::OK),
        ("[]", StatusCode::OK),
        ("{}", StatusCode::OK),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let db = seed(dir.path());
        put(&db, "instance_id", "synthetic-instance");
        let app = app(dir.path());
        let json = format!(r#"{{"origin":"http://127.0.0.1:9","force":{force}}}"#);
        let (status, _) = post(
            &app,
            "/api/admin/web-address",
            &json,
            "https://env.example.test",
        )
        .await;
        assert_eq!(status, expected, "force={force}");
    }
}

#[tokio::test]
async fn invalid_public_origin_is_rejected_before_any_probe() {
    let dir = tempfile::tempdir().unwrap();
    let db = seed(dir.path());
    put(&db, "instance_id", "synthetic-instance");
    let app = app(dir.path());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let body = format!(r#"{{"origin":"http://{addr}/path"}}"#);
    let (status, response) = post(
        &app,
        "/api/admin/web-address",
        &body,
        "https://env.example.test",
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(
        response.get("error"),
        Some(&serde_json::json!(
            "Enter just the address, without a path, such as https://noevia.example.com."
        ))
    );
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(100), listener.accept())
            .await
            .is_err(),
        "a non-canonical address must never be probed"
    );
}

#[tokio::test]
async fn real_loopback_probe_is_credential_free_and_redirects_are_not_followed() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    for (response, expected) in [
        ("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 27\r\nConnection: close\r\n\r\n{\"id\":\"synthetic-instance\"}", StatusCode::OK),
        ("HTTP/1.1 302 Found\r\nLocation: http://127.0.0.1:9/api/instance\r\nContent-Length: 0\r\nConnection: close\r\n\r\n", StatusCode::CONFLICT),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let db = seed(dir.path());
        put(&db, "instance_id", "synthetic-instance");
        let app = app(dir.path());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let reply = response.to_owned();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut raw = vec![0_u8; 8192];
            let n = stream.read(&mut raw).await.unwrap();
            let text = String::from_utf8_lossy(raw.get(..n).unwrap_or_default());
            assert!(text.starts_with("GET /api/instance HTTP/1.1"), "{text}");
            let lower = text.to_ascii_lowercase();
            assert!(!lower.contains("\ncookie:"), "{text}");
            assert!(!lower.contains("\nauthorization:"), "{text}");
            stream.write_all(reply.as_bytes()).await.unwrap();
        });
        let body = format!(r#"{{"origin":"http://{addr}"}}"#);
        let (status, _) = post(&app, "/api/admin/web-address", &body, "https://env.example.test").await;
        server.await.unwrap();
        assert_eq!(status, expected);
    }
}

#[tokio::test]
async fn changed_origin_during_probe_refuses_stale_approval_without_mutation() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let dir = tempfile::tempdir().unwrap();
    let db = seed(dir.path());
    put(&db, "instance_id", "synthetic-instance");
    let app = app(dir.path());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (seen_tx, seen_rx) = tokio::sync::oneshot::channel();
    let (release_tx, release_rx) = tokio::sync::oneshot::channel::<()>();
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut raw = [0_u8; 8192];
        let _ = stream.read(&mut raw).await.unwrap();
        seen_tx.send(()).unwrap();
        release_rx.await.unwrap();
        stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 27\r\nConnection: close\r\n\r\n{\"id\":\"synthetic-instance\"}").await.unwrap();
    });
    let body = format!(r#"{{"origin":"http://{addr}"}}"#);
    let task = tokio::spawn({
        let app = Arc::clone(&app);
        async move {
            post(
                &app,
                "/api/admin/web-address",
                &body,
                "https://env.example.test",
            )
            .await
        }
    });
    seen_rx.await.unwrap();
    put(
        &db,
        "previous_origins",
        r#"["https://concurrent.example.test"]"#,
    );
    release_tx.send(()).unwrap();
    server.await.unwrap();
    let (status, body) = task.await.unwrap();
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(
        db.query_row(
            "SELECT value FROM settings WHERE key='previous_origins'",
            [],
            |r| r.get::<_, String>(0)
        )
        .unwrap(),
        r#"["https://concurrent.example.test"]"#
    );
    assert_eq!(
        db.query_row(
            "SELECT count(*) FROM settings WHERE key='public_origin_admin'",
            [],
            |r| r.get::<_, i64>(0)
        )
        .unwrap(),
        0
    );
}

#[tokio::test]
async fn changed_instance_id_during_probe_refuses_stale_approval_without_mutation() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let dir = tempfile::tempdir().unwrap();
    let db = seed(dir.path());
    put(&db, "instance_id", "synthetic-instance");
    let app = app(dir.path());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (seen_tx, seen_rx) = tokio::sync::oneshot::channel();
    let (release_tx, release_rx) = tokio::sync::oneshot::channel::<()>();
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut raw = [0_u8; 8192];
        let _ = stream.read(&mut raw).await.unwrap();
        seen_tx.send(()).unwrap();
        release_rx.await.unwrap();
        stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 27\r\nConnection: close\r\n\r\n{\"id\":\"synthetic-instance\"}").await.unwrap();
    });
    let body = format!(r#"{{"origin":"http://{addr}"}}"#);
    let task = tokio::spawn({
        let app = Arc::clone(&app);
        async move {
            post(
                &app,
                "/api/admin/web-address",
                &body,
                "https://env.example.test",
            )
            .await
        }
    });
    seen_rx.await.unwrap();
    put(&db, "instance_id", "changed-instance");
    release_tx.send(()).unwrap();
    server.await.unwrap();
    let (status, body) = task.await.unwrap();
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(
        db.query_row(
            "SELECT value FROM settings WHERE key='instance_id'",
            [],
            |r| r.get::<_, String>(0)
        )
        .unwrap(),
        "changed-instance"
    );
    assert_eq!(
        db.query_row(
            "SELECT count(*) FROM settings WHERE key='public_origin_admin'",
            [],
            |r| r.get::<_, i64>(0)
        )
        .unwrap(),
        0
    );
}

#[tokio::test]
async fn revoked_session_during_probe_is_regated_before_the_write() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let dir = tempfile::tempdir().unwrap();
    let db = seed(dir.path());
    put(&db, "instance_id", "synthetic-instance");
    let app = app(dir.path());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (seen_tx, seen_rx) = tokio::sync::oneshot::channel();
    let (release_tx, release_rx) = tokio::sync::oneshot::channel::<()>();
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut raw = [0_u8; 8192];
        let _ = stream.read(&mut raw).await.unwrap();
        seen_tx.send(()).unwrap();
        release_rx.await.unwrap();
        stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 27\r\nConnection: close\r\n\r\n{\"id\":\"synthetic-instance\"}").await.unwrap();
    });
    let body = format!(r#"{{"origin":"http://{addr}"}}"#);
    let task = tokio::spawn({
        let app = Arc::clone(&app);
        async move {
            post(
                &app,
                "/api/admin/web-address",
                &body,
                "https://env.example.test",
            )
            .await
        }
    });
    seen_rx.await.unwrap();
    db.execute("DELETE FROM sessions WHERE id_hash=?1", [hex(SESSION)])
        .unwrap();
    release_tx.send(()).unwrap();
    server.await.unwrap();
    let (status, body) = task.await.unwrap();
    assert_eq!(status, StatusCode::UNAUTHORIZED, "{body}");
    assert_eq!(
        db.query_row(
            "SELECT count(*) FROM settings WHERE key='public_origin_admin'",
            [],
            |r| r.get::<_, i64>(0)
        )
        .unwrap(),
        0
    );
}
