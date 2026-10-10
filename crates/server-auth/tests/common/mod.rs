//! Builds each fixture scenario's cowork.db from the schema and rows Node itself produced
//! (tools/gen-auth-fixtures.cjs), then opens it with server-store's read-only Store.
#![allow(
    dead_code,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use serde_json::Value;
use server_auth::AuthConfig;
use server_store::rusqlite::{self, types::Value as Sql, Connection};

pub fn fixtures() -> Value {
    let path = std::env::var("NOEVIA_AUTH_FIXTURE").unwrap_or_else(|_| {
        concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/node-auth.v1.json"
        )
        .to_string()
    });
    let f: Value = serde_json::from_str(&std::fs::read_to_string(path).expect("fixture file"))
        .expect("fixture JSON");
    assert_eq!(f["version"], 1);
    f
}

fn sql_value(v: &Value) -> Sql {
    match v {
        Value::Null => Sql::Null,
        Value::Bool(b) => Sql::Integer(i64::from(*b)),
        Value::Number(n) => n
            .as_i64()
            .map(Sql::Integer)
            .unwrap_or_else(|| Sql::Real(n.as_f64().unwrap())),
        Value::String(s) => Sql::Text(s.clone()),
        other => panic!("unexpected cell {other}"),
    }
}

pub struct Built {
    pub dir: tempfile::TempDir,
    pub store: server_store::Store,
}

/// The scenario's database, as Node left it before evaluating any case.
pub fn build(scenario: &Value) -> Built {
    let dir = tempfile::tempdir().unwrap();
    let conn = Connection::open(server_store::db_path(dir.path())).unwrap();
    conn.execute_batch("PRAGMA journal_mode = WAL; PRAGMA foreign_keys = OFF")
        .unwrap();
    let db = &scenario["db"];
    for item in db["schema"].as_array().unwrap() {
        conn.execute_batch(item["sql"].as_str().unwrap()).unwrap();
    }
    for v in db["migrations"].as_array().unwrap() {
        conn.execute(
            "INSERT INTO schema_migrations(version, applied_at) VALUES(?1, 0)",
            [v.as_i64().unwrap()],
        )
        .unwrap();
    }
    for (table, rows) in db["rows"].as_object().unwrap() {
        for row in rows.as_array().unwrap() {
            let obj = row.as_object().unwrap();
            let cols: Vec<&str> = obj.keys().map(String::as_str).collect();
            let marks: Vec<String> = (1..=cols.len()).map(|i| format!("?{i}")).collect();
            let sql = format!(
                "INSERT OR REPLACE INTO {table}({}) VALUES({})",
                cols.join(","),
                marks.join(",")
            );
            let vals: Vec<Sql> = obj.values().map(sql_value).collect();
            conn.execute(&sql, rusqlite::params_from_iter(vals))
                .unwrap();
        }
    }
    drop(conn);
    let store = server_store::Store::open(dir.path()).unwrap();
    Built { dir, store }
}

/// The config createAuth/createRequestAuth ran with (trustProxy on; nativeClientAuth fixed).
pub fn config(scenario: &Value) -> AuthConfig {
    let env = &scenario["env"];
    AuthConfig {
        public_origin: env["publicOrigin"].as_str().unwrap().to_string(),
        additional_origins: env["additionalOrigins"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap().to_string())
            .collect(),
        legacy_token: env["legacyToken"].as_str().unwrap().to_string(),
        legacy_compat: env["legacyCompat"].as_bool().unwrap(),
        trust_proxy: true,
        native_client_auth_env: Some(scenario["nativeClientAuth"].as_bool().unwrap()),
        origin_from_settings: false,
    }
}

pub fn creds(input: &Value, method: &str) -> server_auth::Creds {
    let s = |k: &str| input.get(k).and_then(Value::as_str).map(str::to_string);
    server_auth::Creds {
        cookie: s("cookie"),
        authorization: s("authorization"),
        origin: s("origin"),
        csrf: s("csrf"),
        method: method.to_string(),
    }
}

pub fn now(f: &Value, input: &Value) -> i64 {
    input
        .get("now")
        .and_then(Value::as_i64)
        .unwrap_or_else(|| f["now"].as_i64().unwrap())
}

/// Node's `verdict(authn)` shape.
pub fn verdict(id: Option<&server_auth::Identity>) -> Value {
    use server_auth::Credential;
    let Some(id) = id else { return Value::Null };
    let u = &id.user;
    let (kind, account_role, device) = match &id.credential {
        Credential::Session { .. } => ("session", Value::Null, Value::Null),
        Credential::Legacy => ("legacy", Value::Null, Value::Null),
        Credential::Device {
            grant_id,
            client_name,
            expires_at,
        } => (
            "device",
            Value::from(id.account_role.as_str()),
            serde_json::json!({"id": grant_id, "clientName": client_name, "expiresAt": expires_at}),
        ),
    };
    serde_json::json!({
        "user": {"id": u.id, "username": u.username, "displayName": u.display_name, "role": u.role.as_str(),
                 "disabled": u.disabled, "diaryEnabled": u.diary_enabled, "onboarded": u.onboarded},
        "kind": kind, "accountRole": account_role, "device": device,
    })
}

/// tools/gen-auth-fixtures.cjs GATE_TARGETS.
pub const GATE_TARGETS: &[(&str, &str)] = &[
    ("GET", "/api/projects"),
    ("POST", "/api/projects"),
    ("DELETE", "/api/projects/p1"),
    ("OPTIONS", "/api/projects"),
    ("GET", "/api/admin/users"),
    ("GET", "/api/profile"),
    ("PUT", "/api/integrations/storage"),
    ("GET", "/api/integrations/storage"),
    ("POST", "/api/connectors/gdrive/link"),
    ("GET", "/api/connectors/gdrive"),
    ("GET", "/api/auth/passkeysx"),
    ("GET", "/api/auth/passkeys/1"),
];
