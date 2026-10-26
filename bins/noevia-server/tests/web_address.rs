//! Synthetic Node database contract for the three read-only web-address GETs.
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
        "INSERT INTO sessions VALUES(?1,'u-1','csrf',?2,?2,?3,'ua','127.0.0.1')",
        (hex(SESSION), now, now + 86_400_000),
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

    // Node's POST writer changes settings under the running Rust process. The next GET sees it.
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
        for method in ["HEAD", "POST", "PUT", "OPTIONS"] {
            assert_eq!(routes::dispatch(method, path), None);
        }
    }
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
