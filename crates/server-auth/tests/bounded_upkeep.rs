#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
use server_auth::{upkeep::Upkept, AuthConfig, Authenticator, Creds, Refusal};
use server_store::{
    bounded::{BoundedStore, BoundedUpkeepWriter, Budget, QualifiedRoot},
    rusqlite::Connection,
    RustAuth,
};
use std::time::{Duration, Instant};
const SCHEMA: &str = include_str!("../../server-store/tests/fixtures/node-schema.sql");
fn config() -> AuthConfig {
    AuthConfig {
        public_origin: String::new(),
        additional_origins: vec![],
        legacy_token: String::new(),
        legacy_compat: false,
        trust_proxy: true,
        native_client_auth_env: Some(true),
        origin_from_settings: false,
    }
}
#[test]
fn upkeep_policy_retained_before_auth_browser_refusal() {
    let d = tempfile::tempdir().unwrap();
    let c = Connection::open(server_store::db_path(d.path())).unwrap();
    c.execute_batch(SCHEMA).unwrap();
    let now = 1_800_000_000_000i64;
    let live = server_auth::js::digest("live");
    let idle = server_auth::js::digest("idle");
    let token = server_auth::js::digest("nva_device");
    c.execute_batch(&format!("INSERT INTO users(id,username,username_norm,display_name,role,password_hash,webauthn_user_id,created_at,updated_at) VALUES('u','A','a','A','admin','x','w',1,1); INSERT INTO sessions VALUES('{live}','u','c',1,{now},{far},'ua','ip'); INSERT INTO sessions VALUES('{idle}','u','c',1,{old},{far},'ua','ip'); INSERT INTO device_grants VALUES('g','u','Mac',1,{due},{far},'ip','ua'); INSERT INTO device_tokens VALUES('{token}','g','access',1,{far},NULL,NULL);",far=now+99999,old=now-604800000,due=now-60000)).unwrap();
    let a = Authenticator::new(config());
    for (creds, want) in [
        (
            Creds {
                cookie: Some("cowork_session=live".into()),
                method: "GET".into(),
                ..Creds::default()
            },
            Upkept::Touched,
        ),
        (
            Creds {
                cookie: Some("cowork_session=idle".into()),
                method: "GET".into(),
                ..Creds::default()
            },
            Upkept::Deleted,
        ),
        (
            Creds {
                authorization: Some("Bearer nva_device".into()),
                method: "GET".into(),
                ..Creds::default()
            },
            Upkept::GrantTouched,
        ),
    ] {
        let b = Budget::new(Instant::now() + Duration::from_secs(2), 2_000_000).unwrap();
        let root = QualifiedRoot::open(d.path(), &b).unwrap();
        let mut w =
            BoundedUpkeepWriter::open(&root, RustAuth::from_env_value(Some("1")).unwrap(), &b)
                .unwrap();
        assert_eq!(
            w.write(|tx| a.upkeep_in(tx, &creds, now)).unwrap(),
            want,
            "ROUTING_ASSERT/auth/upkeep"
        );
        w.close().unwrap();
        let mut r = BoundedStore::open(&root, &b).unwrap();
        let gate = r
            .read(|r| a.gate_in(r, &creds, "/api/providers/chatgpt", now))
            .unwrap();
        match want {
            Upkept::Touched => assert!(gate.is_ok()),
            Upkept::Deleted => assert_eq!(gate.unwrap_err(), Refusal::Unauthorized),
            Upkept::GrantTouched => assert_eq!(
                gate.unwrap_err(),
                Refusal::BrowserOnly,
                "ROUTING_ASSERT/auth/device403"
            ),
            _ => {}
        }
        r.close().unwrap();
    }
    assert_eq!(
        c.query_row(
            "SELECT last_used_at FROM device_grants WHERE id='g'",
            [],
            |r| r.get::<_, i64>(0)
        )
        .unwrap(),
        now
    );
    assert_eq!(
        c.query_row(
            "SELECT count(*) FROM sessions WHERE id_hash=?1",
            [idle],
            |r| r.get::<_, i64>(0)
        )
        .unwrap(),
        0
    );
}
