#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]
use server_auth::{secrets::SecretKeys, Credential, Identity, PublicUser, Role};
use server_storage_reads::{
    connection_row::{ProjectionError, FIELD_BYTES, KIND_BYTES, ROW_BYTES, SECRET_BYTES},
    owner_secret::project_connection,
    protocol::Kind,
};
use server_store::{
    rusqlite::{types::Value, Connection},
    Store,
};
const SCHEMA: &str = include_str!("../../server-store/tests/fixtures/node-schema.sql");
fn setup() -> (tempfile::TempDir, Connection, Store, SecretKeys) {
    let d = tempfile::tempdir().unwrap();
    let node = Connection::open(d.path().join("cowork.db")).unwrap();
    node.execute_batch(SCHEMA).unwrap();
    std::fs::write(d.path().join("secrets.key"), [1u8; 32]).unwrap();
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
fn row(node: &Connection, owner: &str, kind: &str, secret: &str) {
    node.execute("INSERT OR IGNORE INTO users(id,username,username_norm,display_name,role,password_hash,webauthn_user_id,created_at,updated_at) VALUES(?1,?1,?1,?1,'member','x',?1,1,1)",[owner]).unwrap();
    node.execute("INSERT INTO storage_connections(user_id,kind,base_url,bucket,region,username,secret,corpus_root,updated_at) VALUES(?1,?2,'https://synthetic.invalid','bucket',' EU-WEST-1 ','synthetic',?3,'notes',42)",(owner,kind,secret)).unwrap();
}
#[test]
fn owner_where_and_detached_readonly_snapshot() {
    let (_d, node, store, keys) = setup();
    row(&node, "foreign", "s3", "foreign-secret");
    row(&node, "own", "webdav", "own-secret");
    let out = project_connection(&store, &id("own"), &keys).unwrap();
    assert_eq!(
        out.connection().secret.as_slice(),
        b"own-secret",
        "M4_ROW_OWNER"
    );
    assert_eq!(out.effective_role(), Role::Member, "M4_ROW_EFFECTIVE_ROLE");
    assert_eq!(out.connection().region, "eu-west-1");
    assert!(out.secret_configured());
    assert!(!out.secret_needs_reauth());
    let state: (String, i64) = node
        .query_row(
            "SELECT secret,updated_at FROM storage_connections WHERE user_id='own'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(state, ("own-secret".into(), 42), "M4_ROW_NO_WRITE");
    assert_eq!(
        node.query_row("SELECT count(*) FROM audit_events", [], |r| r
            .get::<_, i64>(0))
            .unwrap(),
        0
    );
    // Exclusive operation is completed after projection, while the snapshot stays alive.
    node.execute_batch("BEGIN EXCLUSIVE; UPDATE storage_connections SET secret='changed' WHERE user_id='own'; COMMIT").unwrap();
    assert_eq!(
        out.connection().secret.as_slice(),
        b"own-secret",
        "M4_ROW_DETACHED"
    );
    assert_eq!(
        project_connection(&store, &id("own"), &keys)
            .unwrap()
            .connection()
            .secret
            .as_slice(),
        b"changed"
    );
}
#[test]
fn missing_only_defaults_and_schema_refusals() {
    let (_d, node, store, keys) = setup();
    let missing = project_connection(&store, &id("absent"), &keys).unwrap();
    assert_eq!(missing.connection().kind, Kind::Local);
    assert_eq!(missing.connection().region, "us-east-1");
    assert!(!missing.secret_configured());
    row(&node, "own", "future-kind", "");
    assert_eq!(
        project_connection(&store, &id("own"), &keys)
            .unwrap()
            .connection()
            .kind,
        Kind::Local
    );
    node.execute_batch("ALTER TABLE storage_connections DROP COLUMN region")
        .unwrap();
    assert_eq!(
        project_connection(&store, &id("absent"), &keys).err(),
        Some(ProjectionError::Schema),
        "M4_ROW_SCHEMA"
    );
    node.execute_batch("DROP TABLE storage_connections")
        .unwrap();
    assert_eq!(
        project_connection(&store, &id("absent"), &keys).err(),
        Some(ProjectionError::Schema)
    );
    node.execute_batch("CREATE VIEW storage_connections AS SELECT 'x' AS user_id")
        .unwrap();
    assert_eq!(
        project_connection(&store, &id("absent"), &keys).err(),
        Some(ProjectionError::Schema)
    );
}
#[test]
fn malformed_schema_pk_and_types_refuse() {
    let (_d, node, store, keys) = setup();
    node.execute_batch("DROP TABLE storage_connections; CREATE TABLE storage_connections(user_id,kind,base_url,bucket,region,username,secret,corpus_root,updated_at)").unwrap();
    row(&node, "own", "webdav", "secret");
    row(&node, "own", "webdav", "other");
    assert_eq!(
        project_connection(&store, &id("own"), &keys).err(),
        Some(ProjectionError::Schema)
    );
    node.execute_batch("DROP TABLE storage_connections; CREATE TABLE storage_connections(user_id TEXT PRIMARY KEY,kind,base_url,bucket,region,username,secret,corpus_root,updated_at)").unwrap();
    row(&node, "own", "webdav", "secret");
    for column in [
        "kind",
        "base_url",
        "bucket",
        "region",
        "username",
        "secret",
        "corpus_root",
    ] {
        for v in [
            Value::Null,
            Value::Integer(1),
            Value::Real(1.5),
            Value::Blob(vec![0xff]),
        ] {
            node.execute(&format!("UPDATE storage_connections SET {column}=?1"), [&v])
                .unwrap();
            assert_eq!(
                project_connection(&store, &id("own"), &keys).err(),
                Some(ProjectionError::Row),
                "{column}"
            );
            node.execute(&format!("UPDATE storage_connections SET {column}=''"), [])
                .unwrap();
        }
    }
    node.execute("UPDATE storage_connections SET updated_at='bad'", [])
        .unwrap();
    assert_eq!(
        project_connection(&store, &id("own"), &keys).err(),
        Some(ProjectionError::Row)
    );
}
#[test]
fn logical_caps_at_and_over_boundaries() {
    let (_d, node, store, keys) = setup();
    row(&node, "own", "webdav", "");
    for (field, cap) in [
        ("kind", KIND_BYTES),
        ("base_url", FIELD_BYTES),
        ("bucket", FIELD_BYTES),
        ("region", FIELD_BYTES),
        ("username", FIELD_BYTES),
        ("corpus_root", FIELD_BYTES),
    ] {
        for n in [cap - 1, cap] {
            node.execute(
                &format!("UPDATE storage_connections SET {field}=?1"),
                ["x".repeat(n)],
            )
            .unwrap();
            assert!(
                project_connection(&store, &id("own"), &keys).is_ok(),
                "{field}:{n}"
            );
        }
        node.execute(
            &format!("UPDATE storage_connections SET {field}=?1"),
            ["x".repeat(cap + 1)],
        )
        .unwrap();
        assert_eq!(
            project_connection(&store, &id("own"), &keys).err(),
            Some(ProjectionError::Cap),
            "M4_ROW_CAP"
        );
        node.execute(&format!("UPDATE storage_connections SET {field}=''"), [])
            .unwrap();
    }
    // Valid encrypted-empty data padded with skipped base64 punctuation reaches envelope cap
    // without simultaneously violating the decrypted cap.
    let sealed = secret_envelope::seal(&[1u8; 32], &[9u8; 12], b"", Some(b"own")).unwrap();
    for n in [SECRET_BYTES - 1, SECRET_BYTES] {
        let v = format!("{sealed}{}", "!".repeat(n - sealed.len()));
        node.execute("UPDATE storage_connections SET secret=?1", [v])
            .unwrap();
        assert!(project_connection(&store, &id("own"), &keys).is_ok());
    }
    node.execute(
        "UPDATE storage_connections SET secret=?1",
        ["x".repeat(SECRET_BYTES + 1)],
    )
    .unwrap();
    assert_eq!(
        project_connection(&store, &id("own"), &keys).err(),
        Some(ProjectionError::Cap)
    );
    node.execute("UPDATE storage_connections SET secret='',kind='',base_url=?1,bucket=?1,username=?1,region=''",["x".repeat(FIELD_BYTES)]).unwrap();
    // user_id=3; three fields=196608; remaining corpus exact aggregate.
    let remain = ROW_BYTES - 3 - 3 * FIELD_BYTES;
    node.execute(
        "UPDATE storage_connections SET corpus_root=?1",
        ["x".repeat(remain)],
    )
    .unwrap();
    assert!(project_connection(&store, &id("own"), &keys).is_ok());
    node.execute(
        "UPDATE storage_connections SET corpus_root=?1",
        ["x".repeat(remain + 1)],
    )
    .unwrap();
    assert_eq!(
        project_connection(&store, &id("own"), &keys).err(),
        Some(ProjectionError::Cap)
    );
    assert_eq!(
        project_connection(&store, &id(""), &keys).err(),
        Some(ProjectionError::Owner)
    );
    assert!(project_connection(&store, &id(&"x".repeat(65_536)), &keys).is_ok());
    assert_eq!(
        project_connection(&store, &id(&"x".repeat(65_537)), &keys).err(),
        Some(ProjectionError::Owner)
    );
}
#[test]
fn deterministic_two_owner_property_and_store_upgrade() {
    let (_d, node, store, keys) = setup();
    row(&node, "a", "s3", "A");
    row(&node, "b", "nextcloud", "B");
    let mut seed = 0x045e_ea74_u64;
    for _ in 0..64 {
        seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
        let own = if seed & 1 == 0 { "a" } else { "b" };
        let foreign = if own == "a" { "b" } else { "a" };
        let before = project_connection(&store, &id(own), &keys).unwrap();
        node.execute(
            "UPDATE storage_connections SET secret=?1 WHERE user_id=?2",
            (format!("synthetic-{seed}"), foreign),
        )
        .unwrap();
        let after = project_connection(&store, &id(own), &keys).unwrap();
        assert_eq!(before.connection().secret, after.connection().secret);
    }
    node.execute("INSERT INTO schema_migrations VALUES(6,0)", [])
        .unwrap();
    assert_eq!(
        project_connection(&store, &id("a"), &keys).err(),
        Some(ProjectionError::Unavailable)
    );
    for e in [
        ProjectionError::Owner,
        ProjectionError::Unavailable,
        ProjectionError::Schema,
        ProjectionError::Row,
        ProjectionError::Cap,
        ProjectionError::Allocation,
    ] {
        assert!(!format!("{e:?}:{e}").contains("synthetic"));
    }
}
