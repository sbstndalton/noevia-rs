#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]
use server_auth::{
    secrets::SecretKeys, AuthConfig, Authenticator, Credential, Creds, Identity, PublicUser,
    Refusal, Role,
};
use server_storage_reads::{
    connection_row::ProjectionError,
    owner_secret::{project_connection, DECRYPTED_BYTES},
};
use server_store::{rusqlite::Connection, Store};
const SCHEMA: &str = include_str!("../../server-store/tests/fixtures/node-schema.sql");
fn setup() -> (tempfile::TempDir, Connection, Store, SecretKeys) {
    let d = tempfile::tempdir().unwrap();
    let node = Connection::open(d.path().join("cowork.db")).unwrap();
    node.execute_batch(SCHEMA).unwrap();
    std::fs::write(d.path().join("secrets.key"), [2u8; 32]).unwrap();
    std::fs::write(d.path().join("secrets.key.previous"), [1u8; 32]).unwrap();
    let keys = SecretKeys::load(d.path(), None).unwrap();
    let store = Store::open(d.path()).unwrap();
    (d, node, store, keys)
}
fn id(owner: &str) -> Identity {
    Identity {
        user: PublicUser {
            id: owner.into(),
            username: "synthetic".into(),
            display_name: "Synthetic".into(),
            role: Role::Member,
            disabled: false,
            diary_enabled: false,
            onboarded: true,
        },
        account_role: Role::Admin,
        credential: Credential::Legacy,
    }
}
fn set(node: &Connection, owner: &str, secret: &str) {
    node.execute("INSERT OR IGNORE INTO users(id,username,username_norm,display_name,role,password_hash,webauthn_user_id,created_at,updated_at) VALUES(?1,?1,?1,?1,'member','x',?1,1,1)",[owner]).unwrap();
    node.execute("INSERT OR REPLACE INTO storage_connections(user_id,kind,secret,updated_at) VALUES(?1,'webdav',?2,42)",(owner,secret)).unwrap();
}
fn seal(key: u8, owner: Option<&[u8]>, plain: &[u8]) -> String {
    secret_envelope::seal(&[key; 32], &[9u8; 12], plain, owner).unwrap()
}
#[test]
fn credential_truth_current_previous_legacy_and_wrong_owner() {
    let (_d, node, store, keys) = setup();
    let current = seal(2, Some(b"a"), b"current");
    let previous = seal(1, Some(b"a"), b"previous");
    let v1 = seal(1, None, b"legacy");
    for (cipher, want) in [
        ("".into(), ""),
        ("plain".into(), "plain"),
        ("enc:v3:opaque".into(), "enc:v3:opaque"),
        (current.clone(), "current"),
        (previous, "previous"),
        (v1.clone(), "legacy"),
        (seal(2, Some(b"a"), b""), ""),
    ] {
        set(&node, "a", &cipher);
        let p = project_connection(&store, &id("a"), &keys).unwrap();
        assert_eq!(
            p.connection().secret.as_slice(),
            want.as_bytes(),
            "M4_SECRET_OPEN"
        );
        assert_eq!(p.secret_configured(), !want.is_empty());
        assert!(!p.secret_needs_reauth());
        assert_eq!(
            node.query_row(
                "SELECT secret,updated_at FROM storage_connections WHERE user_id='a'",
                [],
                |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))
            )
            .unwrap(),
            (cipher, 42),
            "M4_SECRET_NO_UPGRADE"
        );
    }
    set(&node, "b", &current);
    let p = project_connection(&store, &id("b"), &keys).unwrap();
    assert!(p.connection().secret.is_empty(), "M4_SECRET_AAD");
    assert!(p.secret_needs_reauth());
    assert!(!p.secret_configured());
    // Legacy credentials are deliberately unbound, even when copied to another row.
    set(&node, "b", &v1);
    assert_eq!(
        project_connection(&store, &id("b"), &keys)
            .unwrap()
            .connection()
            .secret
            .as_slice(),
        b"legacy"
    );
    for bad in [
        "enc:v2:".to_owned(),
        "enc:v1:AAA".into(),
        seal(3, Some(b"a"), b"wrong-key"),
        current.replace("enc:v2:", "enc:v1:"),
    ] {
        set(&node, "a", &bad);
        let p = project_connection(&store, &id("a"), &keys).unwrap();
        assert!(p.connection().secret.is_empty(), "M4_SECRET_FAILURE_EMPTY");
        assert!(p.secret_needs_reauth());
    }
    set(&node, "a", &seal(2, Some(b"a"), &[0xff, 0xe2, 0x82]));
    assert_eq!(
        project_connection(&store, &id("a"), &keys)
            .unwrap()
            .connection()
            .secret
            .as_slice(),
        "��".as_bytes()
    );
}
#[test]
fn plaintext_and_decrypted_caps_are_not_unreadable_fallback() {
    let (_d, node, store, keys) = setup();
    for n in [DECRYPTED_BYTES - 1, DECRYPTED_BYTES] {
        set(&node, "a", &"x".repeat(n));
        assert_eq!(
            project_connection(&store, &id("a"), &keys)
                .unwrap()
                .connection()
                .secret
                .len(),
            n
        );
    }
    set(&node, "a", &"x".repeat(DECRYPTED_BYTES + 1));
    assert_eq!(
        project_connection(&store, &id("a"), &keys).err(),
        Some(ProjectionError::Cap),
        "M4_SECRET_DECRYPTED_CAP"
    );
    set(
        &node,
        "a",
        &seal(2, Some(b"a"), &vec![b'x'; DECRYPTED_BYTES + 1]),
    );
    assert_eq!(
        project_connection(&store, &id("a"), &keys).err(),
        Some(ProjectionError::Cap)
    );
}
#[test]
fn actual_key_loader_synthetic_infrastructure_semantics() {
    let d = tempfile::tempdir().unwrap();
    assert!(SecretKeys::load(d.path(), None).is_err());
    assert!(!d.path().join("secrets.key").exists());
    for n in [31, 33] {
        std::fs::write(d.path().join("secrets.key"), vec![1u8; n]).unwrap();
        assert!(SecretKeys::load(d.path(), None).is_err());
    }
    std::fs::write(d.path().join("secrets.key"), [2u8; 32]).unwrap();
    let env = "01".repeat(32);
    let old = seal(1, Some(b"a"), b"old");
    for text in [
        format!(" {env}\n"),
        "AQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQE=".into(),
    ] {
        assert_eq!(
            SecretKeys::load(d.path(), Some(&text))
                .unwrap()
                .open(&old, Some("a"))
                .unwrap()
                .0
                .as_bytes(),
            b"old"
        );
    }
    assert!(SecretKeys::load(d.path(), Some(" \n ")).is_err());
    std::fs::write(d.path().join("secrets.key.previous"), [3u8; 32]).unwrap();
    assert!(SecretKeys::load(d.path(), Some(&env))
        .unwrap()
        .open(&old, Some("a"))
        .is_err());
    std::fs::write(d.path().join("secrets.key.previous"), [3u8; 31]).unwrap();
    assert!(SecretKeys::load(d.path(), Some(&env)).is_err());
    std::fs::write(d.path().join("secrets.key.previous"), [2u8; 32]).unwrap();
    assert!(!SecretKeys::load(d.path(), None).unwrap().has_previous());
}
#[test]
fn actual_auth_gate_before_row_open_and_effective_device_role() {
    let (_d, node, store, keys) = setup();
    node.execute("INSERT INTO users(id,username,username_norm,display_name,role,password_hash,webauthn_user_id,created_at,updated_at) VALUES('a','A','a','A','admin','x','w',1,1)",[]).unwrap();
    let config = AuthConfig::from_lookup(|k| match k {
        "PUBLIC_ORIGIN" => Some("https://app.synthetic.invalid".into()),
        "TRUST_PROXY" => Some("true".into()),
        "NOEVIA_FEATURE_NATIVE_CLIENT_AUTH" => Some("1".into()),
        _ => None,
    })
    .unwrap();
    let auth = Authenticator::new(config);
    let now = 1000;
    let token = "nva_synthetic";
    let hash = server_auth::js::digest(token);
    node.execute(
        "INSERT INTO device_grants VALUES('grant','a','Synthetic',0,0,99999,'','')",
        [],
    )
    .unwrap();
    node.execute(
        "INSERT INTO device_tokens VALUES(?1,'grant','access',0,99999,NULL,NULL)",
        [hash],
    )
    .unwrap();
    set(&node, "a", &seal(2, Some(b"a"), b"own"));
    let creds = Creds {
        method: "POST".into(),
        authorization: Some(format!("Bearer {token}")),
        ..Creds::default()
    };
    let gated = auth
        .gate(&store, &creds, "/api/integrations/storage/file", now)
        .unwrap()
        .unwrap();
    let p = project_connection(&store, &gated, &keys).unwrap();
    assert_eq!(p.effective_role(), Role::Member, "M4_GATE_EFFECTIVE_ROLE");
    assert_eq!(p.connection().secret.as_slice(), b"own");
    for c in [
        Creds {
            cookie: Some("cowork_csrf=x".into()),
            ..creds.clone()
        },
        Creds {
            origin: Some("https://foreign.invalid".into()),
            ..creds.clone()
        },
        Creds {
            authorization: None,
            ..creds.clone()
        },
    ] {
        assert!(auth
            .gate(&store, &c, "/api/integrations/storage/file", now)
            .unwrap()
            .is_err());
    }
    node.execute("UPDATE users SET disabled_at=1", []).unwrap();
    assert_eq!(
        auth.gate(&store, &creds, "/api/integrations/storage/file", now)
            .unwrap()
            .err(),
        Some(Refusal::Unauthorized)
    );
    node.execute("UPDATE users SET disabled_at=NULL", [])
        .unwrap();
    assert!(auth
        .gate(&store, &creds, "/api/integrations/storage/file", 99999)
        .unwrap()
        .is_err());
    node.execute(
        "INSERT INTO sessions VALUES(?1,'a',?2,0,1000,99999,'','')",
        (
            server_auth::js::digest("session"),
            server_auth::js::digest("csrf"),
        ),
    )
    .unwrap();
    let browser = Creds {
        method: "POST".into(),
        cookie: Some("cowork_session=session; cowork_csrf=csrf".into()),
        csrf: Some("csrf".into()),
        ..Creds::default()
    };
    assert!(auth
        .gate(&store, &browser, "/api/integrations/storage/file", now)
        .unwrap()
        .is_ok());
    assert_eq!(
        auth.gate(
            &store,
            &Creds {
                csrf: Some("bad".into()),
                ..browser.clone()
            },
            "/api/integrations/storage/file",
            now
        )
        .unwrap()
        .err(),
        Some(Refusal::Csrf)
    );
    assert!(auth
        .gate(
            &store,
            &browser,
            "/api/integrations/storage/file",
            server_auth::identity::IDLE_MS as i64 + 1000
        )
        .unwrap()
        .is_err());
    // Only call adapter after gate success. This test does not integrate the front or certify zero calls in an unwired route.
}

// Each named policy has one designated semantic assertion for source-bound mutation kills.
#[test]
#[rustfmt::skip]
fn owner_binding_policy() {
 let (_d,node,store,keys)=setup();set(&node,"own","own-secret");set(&node,"foreign","foreign-secret");
 let result=project_connection(&store,&id("own"),&keys);
 assert!(result.is_ok_and(|p|p.connection().secret.as_slice()==b"own-secret"),"M4_OWNER_BINDING");
}
#[test]
#[rustfmt::skip]
fn effective_role_policy() {
 let (_d,_node,store,keys)=setup();let result=project_connection(&store,&id("own"),&keys);
 assert!(result.is_ok_and(|p|p.effective_role()==Role::Member),"M4_EFFECTIVE_ROLE");
}
#[test]
#[rustfmt::skip]
fn aad_owner_policy() {
 let (_d,node,store,keys)=setup();set(&node,"a",&seal(2,Some(b"a"),b"bound"));let result=project_connection(&store,&id("a"),&keys);
 assert!(result.is_ok_and(|p|p.connection().secret.as_slice()==b"bound"),"M4_AAD_OWNER");
}
#[test]
#[rustfmt::skip]
fn decrypted_cap_policy() {
 let (_d,node,store,keys)=setup();set(&node,"a",&"x".repeat(DECRYPTED_BYTES+1));let result=project_connection(&store,&id("a"),&keys);
 assert!(result.err()==Some(ProjectionError::Cap),"M4_DECRYPTED_CAP");
}
