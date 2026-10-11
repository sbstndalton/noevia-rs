#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
use noevia_server::chatgpt_worker::{JobInput, OwnedPrefilter, OwnedStatusWorker};
use server_auth::{AuthConfig, Authenticator};
use server_store::{rusqlite::Connection, RustAuth};
use std::{
    sync::Arc,
    time::{Duration, Instant},
};
#[path = "../../../tools/replay/src/bin/support/chatgpt_worker_auth_oracle.rs"]
mod worker_oracle;
static TEST_SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());
fn test_serial() -> std::sync::MutexGuard<'static, ()> {
    TEST_SERIAL
        .lock()
        .unwrap_or_else(|error| error.into_inner())
}
const SCHEMA: &str = include_str!("../../../crates/server-store/tests/fixtures/node-schema.sql");
const NOW: i64 = 1_800_000_000_000;
fn fixture() -> (tempfile::TempDir, Connection) {
    let d = tempfile::tempdir().unwrap();
    std::fs::write(d.path().join("secrets.key"), [7; 32]).unwrap();
    let c = Connection::open(server_store::db_path(d.path())).unwrap();
    c.execute_batch(SCHEMA).unwrap();
    c.execute_batch(&worker_oracle::setup_sql()).unwrap();
    (d, c)
}
fn auth() -> Authenticator {
    Authenticator::new(AuthConfig {
        public_origin: String::new(),
        additional_origins: vec![],
        legacy_token: String::new(),
        legacy_compat: false,
        trust_proxy: true,
        native_client_auth_env: Some(true),
        origin_from_settings: false,
    })
}
fn worker(d: &tempfile::TempDir, p: Arc<OwnedPrefilter>) -> OwnedStatusWorker {
    OwnedStatusWorker::new(
        d.path().to_owned(),
        auth(),
        RustAuth::from_env_value(Some("1")).unwrap(),
        p,
        None,
        None,
    )
    .unwrap()
}
fn input(cookie: &str, bearer: &str) -> JobInput {
    JobInput::from_headers(
        Instant::now(),
        NOW,
        "GET",
        "/api/providers/chatgpt",
        Some("127.0.0.1:8021".parse().unwrap()),
        [
            ("cookie", cookie.as_bytes()),
            ("authorization", bearer.as_bytes()),
        ],
    )
    .unwrap()
    .unwrap()
}
// Independent test runtimes retain this guard through awaited cleanup; tasks never acquire it.
#[allow(clippy::await_holding_lock)]
#[tokio::test]
async fn worker_real_auth_upkeep_and_status_cases() {
    let _serial = test_serial();
    for (cookie, bearer, code) in [
        ("cowork_session=live", "", 200),
        ("cowork_session=idle", "", 401),
        ("", "Bearer nva_device", 403),
        ("", "", 401),
        ("cowork_session=live", "Bearer nva_device", 401),
    ] {
        let (d, c) = fixture();
        let w = worker(&d, Arc::new(OwnedPrefilter::ready(false, vec![]).unwrap()));
        let reply = w.submit(input(cookie, bearer)).unwrap().wait().await;
        assert_eq!(reply.status, code, "ROUTING_ASSERT/worker/auth");
        if code == 200 {
            assert!(reply.body.contains("\"state\":\"disconnected\""));
            assert_eq!(
                c.query_row(
                    "SELECT last_seen_at FROM sessions WHERE id_hash=?1",
                    [server_auth::js::digest("live")],
                    |r| r.get::<_, i64>(0)
                )
                .unwrap(),
                NOW
            );
        }
        if cookie == "cowork_session=idle" {
            assert_eq!(
                c.query_row(
                    "SELECT count(*) FROM sessions WHERE id_hash=?1",
                    [server_auth::js::digest("idle")],
                    |r| r.get::<_, i64>(0)
                )
                .unwrap(),
                0
            );
            assert!(reply.www_authenticate);
        }
        if code == 403 {
            assert!(reply.body.contains("browser_session_required"));
            assert_eq!(
                c.query_row(
                    "SELECT last_used_at FROM device_grants WHERE id='g'",
                    [],
                    |r| r.get::<_, i64>(0)
                )
                .unwrap(),
                NOW
            );
        }
        w.shutdown().unwrap();
    }
}
// Independent test runtimes retain this guard through awaited cleanup; tasks never acquire it.
#[allow(clippy::await_holding_lock)]
#[tokio::test]
async fn worker_product_off_prefilter_and_busy_upkeep() {
    let _serial = test_serial();
    let (d, c) = fixture();
    c.execute_batch("UPDATE settings SET value='false' WHERE key='feature:chatgptOAuth'; DROP TABLE chatgpt_oauth_tokens;").unwrap();
    let w = worker(&d, Arc::new(OwnedPrefilter::ready(false, vec![]).unwrap()));
    assert_eq!(
        w.submit(input("cowork_session=live", ""))
            .unwrap()
            .wait()
            .await
            .status,
        404
    );
    w.shutdown().unwrap();
    let (d, c) = fixture();
    let w = worker(
        &d,
        Arc::new(OwnedPrefilter::ready(true, vec!["127.0.0.1".into()]).unwrap()),
    );
    let reply = w
        .submit(input("cowork_session=live", ""))
        .unwrap()
        .wait()
        .await;
    assert_eq!(reply.status, 403);
    assert!(reply.body.is_empty());
    assert_ne!(
        c.query_row(
            "SELECT last_seen_at FROM sessions WHERE id_hash=?1",
            [server_auth::js::digest("live")],
            |r| r.get::<_, i64>(0)
        )
        .unwrap(),
        NOW
    );
    w.shutdown().unwrap();
    let (d, c) = fixture();
    c.execute_batch("BEGIN IMMEDIATE").unwrap();
    let w = worker(&d, Arc::new(OwnedPrefilter::ready(false, vec![]).unwrap()));
    assert_eq!(
        w.submit(input("cowork_session=live", ""))
            .unwrap()
            .wait()
            .await
            .status,
        200
    );
    assert!(c.execute_batch("COMMIT").is_ok());
    w.shutdown().unwrap();
}
#[test]
fn worker_input_exact_method_header_and_arrival() {
    let _serial = test_serial();
    assert!(JobInput::from_headers(
        Instant::now(),
        NOW,
        "DELETE",
        "/api/providers/chatgpt",
        None,
        []
    )
    .unwrap()
    .is_none());
    assert!(JobInput::from_headers(
        Instant::now(),
        NOW,
        "GET",
        "/api/providers/chatgpt/models",
        None,
        []
    )
    .unwrap()
    .is_none());
    assert!(JobInput::from_headers(
        Instant::now(),
        NOW,
        "GET",
        "/api/providers/chatgpt",
        None,
        [("cookie", vec![b'x'; 8193].as_slice())]
    )
    .is_err());
    let (d, _c) = fixture();
    let w = worker(&d, Arc::new(OwnedPrefilter::ready(false, vec![]).unwrap()));
    let old = JobInput::from_headers(
        Instant::now() - Duration::from_secs(2),
        NOW,
        "GET",
        "/api/providers/chatgpt",
        None,
        [],
    )
    .unwrap()
    .unwrap();
    assert!(w.submit(old).is_err());
    w.shutdown().unwrap();
}

// Independent test runtimes retain this guard through awaited cleanup; tasks never acquire it.
#[allow(clippy::await_holding_lock)]
#[tokio::test]
async fn worker_new_cookie_origin_recent_device_and_account_boundaries() {
    let _serial = test_serial();
    for (headers, change, code) in [
        (
            vec![
                ("cookie", "cowork_session=bad"),
                ("cookie", "cowork_session=live"),
                ("origin", "https://invalid.test"),
                ("x-csrf-token", "wrong"),
            ],
            "",
            200,
        ),
        (
            vec![
                ("authorization", "Bearer nva_device"),
                ("authorization", "Bearer ignored"),
            ],
            "UPDATE device_grants SET last_used_at=1800000000000",
            403,
        ),
        (
            vec![("cookie", "cowork_session=live")],
            "UPDATE users SET disabled_at=1",
            401,
        ),
        (
            vec![("cookie", "cowork_session=live")],
            "UPDATE sessions SET expires_at=1800000000000",
            401,
        ),
        (vec![("cookie", "cowork_session=missing")], "", 401),
        (
            vec![
                ("authorization", "Bearer nva_device"),
                ("cookie", "cowork_csrf=x"),
            ],
            "",
            401,
        ),
    ] {
        let (d, c) = fixture();
        c.execute_batch(change).unwrap();
        let w = worker(&d, Arc::new(OwnedPrefilter::ready(false, vec![]).unwrap()));
        let request = JobInput::from_headers(
            Instant::now(),
            NOW,
            "GET",
            "/api/providers/chatgpt?status=1",
            Some("127.0.0.1:8021".parse().unwrap()),
            headers.iter().map(|(k, v)| (*k, v.as_bytes())),
        )
        .unwrap()
        .unwrap();
        let reply = w.submit(request).unwrap().wait().await;
        w.shutdown().unwrap();
        assert_eq!(reply.status, code);
        if change.starts_with("UPDATE device_grants") {
            assert_eq!(
                c.query_row("SELECT last_used_at FROM device_grants", [], |r| r
                    .get::<_, i64>(0))
                    .unwrap(),
                NOW
            );
        }
    }
}

// Independent test runtimes retain this guard through awaited cleanup; tasks never acquire it.
#[allow(clippy::await_holding_lock)]
#[tokio::test]
async fn worker_actual_node_oracle_new_cases() {
    let _serial = test_serial();
    let reference =
        worker_oracle::reference_at(&std::env::var("NOEVIA_WORKER_AUTH_REFERENCE").unwrap())
            .unwrap();
    for ((cookie, bearer), expected) in [
        ("cowork_session=live", ""),
        ("cowork_session=idle", ""),
        ("", "Bearer nva_device"),
        ("cowork_session=live", "Bearer nva_device"),
        ("", ""),
    ]
    .into_iter()
    .zip(reference)
    {
        let (d, c) = fixture();
        let w = worker(&d, Arc::new(OwnedPrefilter::ready(false, vec![]).unwrap()));
        let reply = w.submit(input(cookie, bearer)).unwrap().wait().await;
        w.shutdown().unwrap();
        let code = if !expected.get("authenticated").unwrap().as_bool().unwrap() {
            401
        } else if expected.get("browserOnly").unwrap().as_bool().unwrap() {
            403
        } else {
            200
        };
        assert_eq!(reply.status, code);
        let sessions:Vec<serde_json::Value>=c.prepare("SELECT id_hash,last_seen_at FROM sessions ORDER BY id_hash").unwrap().query_map([],|r|Ok(serde_json::json!({"id_hash":r.get::<_,String>(0)?,"last_seen_at":r.get::<_,i64>(1)?}))).unwrap().map(Result::unwrap).collect();
        let grants:Vec<serde_json::Value>=c.prepare("SELECT id,last_used_at FROM device_grants ORDER BY id").unwrap().query_map([],|r|Ok(serde_json::json!({"id":r.get::<_,String>(0)?,"last_used_at":r.get::<_,i64>(1)?}))).unwrap().map(Result::unwrap).collect();
        assert_eq!(
            serde_json::json!({"sessions":sessions,"grants":grants}),
            serde_json::json!({"sessions":expected.get("sessions").unwrap(),"grants":expected.get("grants").unwrap()}),
            "ROUTING_ASSERT/worker/node_delta"
        );
    }
}

// Independent test runtimes retain this guard through awaited cleanup; tasks never acquire it.
#[allow(clippy::await_holding_lock)]
#[tokio::test]
async fn worker_auth_static_vm_exhaustion() {
    let _serial = test_serial();
    let (d, c) = fixture();
    c.execute_batch("WITH RECURSIVE n(x) AS (VALUES(1) UNION ALL SELECT x+1 FROM n WHERE x<100000) INSERT INTO users(id,username,username_norm,display_name,role,password_hash,webauthn_user_id,created_at,updated_at) SELECT 'bulk'||x,'B'||x,'b'||x,'B','admin','x','w'||x,100001-x,1 FROM n;").unwrap();
    let auth = Authenticator::new(AuthConfig {
        public_origin: String::new(),
        additional_origins: vec![],
        legacy_token: "synthetic-legacy".into(),
        legacy_compat: true,
        trust_proxy: true,
        native_client_auth_env: Some(false),
        origin_from_settings: false,
    });
    let w = OwnedStatusWorker::new(
        d.path().to_owned(),
        auth,
        RustAuth::from_env_value(Some("1")).unwrap(),
        Arc::new(OwnedPrefilter::ready(false, vec![]).unwrap()),
        None,
        None,
    )
    .unwrap();
    let reply = w
        .submit(input("", "Bearer synthetic-legacy"))
        .unwrap()
        .wait()
        .await;
    w.shutdown().unwrap();
    assert_eq!(reply.status, 503, "ROUTING_ASSERT/worker/auth_vm");
}

#[test]
fn worker_oracle_rejects_grant_timestamp_delta() {
    let reference =
        worker_oracle::reference_at(&std::env::var("NOEVIA_WORKER_AUTH_REFERENCE").unwrap())
            .unwrap();
    let mut changed = reference.clone();
    let row = changed.get_mut(2).unwrap();
    let value = row
        .get_mut("grants")
        .unwrap()
        .get_mut(0)
        .unwrap()
        .get_mut("last_used_at")
        .unwrap();
    let original = value.as_i64().unwrap();
    *value = serde_json::json!(original + 1);
    assert_eq!(
        worker_oracle::differences(&reference, &changed).unwrap(),
        vec!["device/grants/0/last_used_at"]
    );
    println!("M6_WORKER_NEGATIVE grants.last_used_at +1 rejected");
}
