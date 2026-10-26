//! Synthetic, Node-shaped cowork.db. The credential/hash pair comes from the
//! committed server-auth Node differential fixture, never from a live account.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::sync::Arc;

use server_dav::Scope;
use server_dav_adapter::auth::{AuthConfig, AuthGate, ConfigError};
use server_store::rusqlite::{params, Connection};
use server_store::{db_path, Store};

const SCHEMA: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../server-store/tests/fixtures/node-schema.sql"
));
const CREDENTIAL_ID: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const PASSWORD: &str =
    "nv_dav_aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa.T__o4qEYEzGvCBRCkWGgkJUiECUVkENvm8M2ypgmjQ4";
const HASH: &str = "$argon2id$v=19$m=19456,t=2,p=1$htTnVq45aTFtSZ4j6uYg2Q$0dpnt5pAek0kwLdm/v1/iGBKzQ1bxZlOeOT9W9kbBlQ";
const KEY: &str = "synthetic-tenant-key-for-fixtures";

struct Fixture {
    _dir: tempfile::TempDir,
    node: Connection,
    store: Arc<Store>,
}

impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let node = Connection::open(db_path(dir.path())).unwrap();
        node.execute_batch(SCHEMA).unwrap();
        node.execute_batch(
            "CREATE TABLE dav_settings(user_id TEXT PRIMARY KEY REFERENCES users(id) ON DELETE CASCADE, \
             scope TEXT NOT NULL CHECK(scope IN ('off','lan','public')), \
             acknowledged_at INTEGER, updated_at INTEGER NOT NULL);",
        )
        .unwrap();
        node.execute(
            "INSERT INTO users(id,username,username_norm,display_name,role,password_hash,webauthn_user_id,created_at,updated_at) \
             VALUES('u-member','Member.One','member.one','Synthetic Member','member','x','w-member',1,1)",
            [],
        )
        .unwrap();
        node.execute(
            "INSERT INTO users(id,username,username_norm,display_name,role,password_hash,webauthn_user_id,created_at,updated_at) \
             VALUES('u-other','Bob','bob','Synthetic Other','member','x','w-other',1,1)",
            [],
        )
        .unwrap();
        node.execute(
            "INSERT INTO app_passwords(id,user_id,name,scope,password_hash,created_at) \
             VALUES(?1,'u-member','Synthetic Device','lan',?2,1)",
            params![CREDENTIAL_ID, HASH],
        )
        .unwrap();
        node.execute(
            "INSERT INTO user_features(user_id,diary_enabled,updated_at) VALUES('u-member',1,1)",
            [],
        )
        .unwrap();
        node.execute(
            "INSERT INTO dav_settings(user_id,scope,updated_at) VALUES('u-member','lan',1)",
            [],
        )
        .unwrap();
        let store = Arc::new(Store::open(dir.path()).unwrap());
        Self {
            _dir: dir,
            node,
            store,
        }
    }

    fn gate(&self, scope: &str) -> AuthGate {
        AuthGate::new(
            Arc::clone(&self.store),
            AuthConfig::new(scope, KEY).unwrap(),
        )
    }
}

#[test]
fn configuration_refuses_missing_or_invalid_key_and_scope() {
    for scope in ["", "LAN", "off", "public ", "unknown"] {
        assert_eq!(
            AuthConfig::new(scope, KEY).err(),
            Some(ConfigError::InvalidScope)
        );
    }
    for key in ["", "  ", "synthetic\nkey", "synthetic\0key"] {
        assert_eq!(
            AuthConfig::new("lan", key).err(),
            Some(ConfigError::InvalidTenantKey)
        );
    }
    assert_eq!(
        AuthConfig::new("public", &"x".repeat(4097)).err(),
        Some(ConfigError::InvalidTenantKey)
    );
    let valid = AuthConfig::new("lan", &format!("  {KEY}  ")).unwrap();
    assert_eq!(valid.scope(), Scope::Lan);
    assert_eq!(valid.tenant_key(), KEY);
}

#[test]
fn authenticated_identity_is_bound_to_the_credential_owner_and_scope() {
    let f = Fixture::new();
    let lan = f.gate("lan");
    let identity = lan.authenticate("MEMBER.ONE", PASSWORD).unwrap().unwrap();
    assert_eq!(identity.user_id(), "u-member");
    assert_eq!(identity.credential_id(), CREDENTIAL_ID);
    assert_eq!(identity.scope(), Scope::Lan);
    assert!(lan.allowed(&identity).unwrap());
    assert!(lan.authenticate("bob", PASSWORD).unwrap().is_none());
    assert!(lan.authenticate("unknown", PASSWORD).unwrap().is_none());
    assert!(lan
        .authenticate("member.one", "not-an-app-password")
        .unwrap()
        .is_none());
    let public = f.gate("public");
    assert!(public
        .authenticate("member.one", PASSWORD)
        .unwrap()
        .is_none());
    assert!(!public.allowed(&identity).unwrap());
    let used: Option<i64> = f
        .node
        .query_row(
            "SELECT last_used_at FROM app_passwords WHERE id=?1",
            [CREDENTIAL_ID],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        used, None,
        "read-only stage1 does not claim Node last_used_at parity"
    );
}

#[test]
fn live_eligibility_rechecks_revocation_account_sharing_diary_and_local_storage() {
    let f = Fixture::new();
    let gate = f.gate("lan");
    let identity = gate.authenticate("member.one", PASSWORD).unwrap().unwrap();
    assert!(
        gate.allowed(&identity).unwrap(),
        "no storage row means local"
    );

    f.node
        .execute(
            "UPDATE dav_settings SET scope='off' WHERE user_id='u-member'",
            [],
        )
        .unwrap();
    assert!(!gate.allowed(&identity).unwrap());
    f.node
        .execute(
            "UPDATE dav_settings SET scope='lan' WHERE user_id='u-member'",
            [],
        )
        .unwrap();
    f.node
        .execute(
            "UPDATE user_features SET diary_enabled=0 WHERE user_id='u-member'",
            [],
        )
        .unwrap();
    assert!(!gate.allowed(&identity).unwrap());
    f.node
        .execute(
            "UPDATE user_features SET diary_enabled=1 WHERE user_id='u-member'",
            [],
        )
        .unwrap();

    f.node.execute(
        "INSERT INTO storage_connections(user_id,kind,updated_at) VALUES('u-member','webdav',1)",
        [],
    ).unwrap();
    assert!(!gate.allowed(&identity).unwrap());
    f.node
        .execute(
            "UPDATE storage_connections SET kind='local' WHERE user_id='u-member'",
            [],
        )
        .unwrap();
    assert!(gate.allowed(&identity).unwrap());

    f.node
        .execute("UPDATE users SET disabled_at=2 WHERE id='u-member'", [])
        .unwrap();
    assert!(!gate.allowed(&identity).unwrap());
    f.node
        .execute("UPDATE users SET disabled_at=NULL WHERE id='u-member'", [])
        .unwrap();
    f.node
        .execute(
            "UPDATE app_passwords SET scope='public' WHERE id=?1",
            [CREDENTIAL_ID],
        )
        .unwrap();
    assert!(!gate.allowed(&identity).unwrap());
    f.node
        .execute(
            "UPDATE app_passwords SET scope='lan' WHERE id=?1",
            [CREDENTIAL_ID],
        )
        .unwrap();
    f.node
        .execute("DELETE FROM app_passwords WHERE id=?1", [CREDENTIAL_ID])
        .unwrap();
    assert!(!gate.allowed(&identity).unwrap());
    assert!(gate.authenticate("member.one", PASSWORD).unwrap().is_none());
}
