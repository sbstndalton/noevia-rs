#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]
use serde_json::{json, Value};
use server_sampling_settings::{get, input, put, KEY};
use server_store::{
    rusqlite::{params, types::Value as SqlValue, Connection},
    RustAuth, RustSamplingSettings, Store, Writer,
};
#[test]
fn actual_node_catalogue_defaults_and_put_matrix() {
    let text = std::env::var("NOEVIA_SAMPLING_FIXTURE")
        .map(|p| std::fs::read_to_string(p).unwrap())
        .unwrap_or_else(|_| include_str!("fixtures/node-sampling.v1.json").into());
    let fixture: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(
        fixture["core"],
        std::env::var("NOEVIA_SAMPLING_CORE_SHA")
            .unwrap_or_else(|_| "965598462d64dc04e1df55c3bb7a25d4343380e1".into())
    );
    for case in fixture["cases"].as_array().unwrap() {
        let dir = tempfile::tempdir().unwrap();
        let db = Connection::open(dir.path().join("cowork.db")).unwrap();
        db.execute_batch(
            &include_str!("../../server-store/tests/fixtures/node-schema.sql")
                .replace("value TEXT NOT NULL", "value"),
        )
        .unwrap();
        db.execute("INSERT INTO users(id,username,username_norm,display_name,role,password_hash,webauthn_user_id,created_at,updated_at) VALUES('a','a','a','Synthetic','admin','x','a',1,1)", []).unwrap();
        let initial = &case["initial"];
        let value = match initial["kind"].as_str().unwrap() {
            "missing" => None,
            "null" => Some(SqlValue::Null),
            "text" => Some(SqlValue::Text(initial["value"].as_str().unwrap().into())),
            "number" => Some(SqlValue::Real(initial["value"].as_f64().unwrap())),
            "blob" => Some(SqlValue::Blob(
                initial["value"]
                    .as_str()
                    .unwrap()
                    .as_bytes()
                    .as_chunks::<2>()
                    .0
                    .iter()
                    .map(|v| u8::from_str_radix(std::str::from_utf8(v).unwrap(), 16).unwrap())
                    .collect(),
            )),
            _ => panic!("unknown fixture kind"),
        };
        if let Some(value) = value {
            db.execute(
                "INSERT INTO settings(key,value) VALUES(?1,?2)",
                params![KEY, value],
            )
            .unwrap();
        }
        if case["input"]["auditAbort"] == true {
            db.execute_batch("CREATE TRIGGER sampling_audit_abort BEFORE INSERT ON audit_events BEGIN SELECT RAISE(ABORT,'synthetic audit refusal'); END;").unwrap();
        }
        let auth = RustAuth::from_env_value(Some("1")).unwrap();
        let token = RustSamplingSettings::from_env_value(Some("1"), auth).unwrap();
        let writer = Writer::open(dir.path(), auth).unwrap();
        let store = Store::open(dir.path()).unwrap();
        let admin = case["role"] == "admin";
        let (status, body) = match case["method"].as_str().unwrap() {
            "GET" => (200, store.read(|r| get(r, admin)).unwrap()),
            "PUT" if !admin => (403, json!({"error":"Administrator required"})),
            "PUT" => {
                let input = &case["input"];
                let bytes = if let Some(n) = input["repeatExtra"].as_u64() {
                    format!(
                        "{{\"enabled\":true,\"extra\":\"{}\"}}",
                        "x".repeat(n as usize)
                    )
                    .into_bytes()
                } else if input["invalidUtf8"] == true {
                    let mut bytes = b"{\"enabled\":true,\"extra\":\"".to_vec();
                    bytes.push(255);
                    bytes.extend_from_slice(b"\"}");
                    bytes
                } else {
                    input["raw"].as_str().unwrap_or("").as_bytes().to_vec()
                };
                let parsed = if input["readError"] == true {
                    Err(input::Error::Json)
                } else {
                    input::parse(&bytes)
                };
                match parsed {
                    Ok(enabled) => match put(
                        &writer,
                        token,
                        "a",
                        enabled,
                        fixture["now"].as_i64().unwrap(),
                    ) {
                        Ok(body) => (200, body),
                        Err(server_store::StoreError::Sqlite(_)) => {
                            (500, json!({"error":"Internal error"}))
                        }
                        Err(err) => panic!("unexpected store error: {err}"),
                    },
                    Err(input::Error::Json) => (400, json!({"error":"invalid JSON"})),
                    Err(input::Error::Enabled) => {
                        (400, json!({"error":"enabled must be true or false"}))
                    }
                }
            }
            _ => (405, json!({"error":"Method not allowed"})),
        };
        let setting: Option<Value> = db
            .query_row(
                "SELECT typeof(value),value FROM settings WHERE key=?1",
                [KEY],
                |r| {
                    let kind: String = r.get(0)?;
                    let value: SqlValue = r.get(1)?;
                    let value = match value {
                        SqlValue::Null => Value::Null,
                        SqlValue::Text(v) => json!(v),
                        SqlValue::Integer(v) => json!(v),
                        SqlValue::Real(v) => {
                            if v.fract() == 0.0 {
                                json!(v as i64)
                            } else {
                                json!(v)
                            }
                        }
                        SqlValue::Blob(v) => {
                            json!(v.iter().map(|b| format!("{b:02x}")).collect::<String>())
                        }
                    };
                    Ok(json!({"type":kind,"value":value}))
                },
            )
            .optional()
            .unwrap();
        let audit:Vec<Value>=db.prepare("SELECT actor_user_id,target_user_id,action,detail,created_at FROM audit_events ORDER BY id").unwrap().query_map([],|r| {
            let detail:String=r.get(3)?;
            Ok(json!({"actor_user_id":r.get::<_,String>(0)?,"target_user_id":r.get::<_,Option<String>>(1)?,"action":r.get::<_,String>(2)?,"detail":serde_json::from_str::<Value>(&detail).unwrap(),"created_at":r.get::<_,i64>(4)?}))
        }).unwrap().collect::<Result<_,_>>().unwrap();
        assert_eq!(
            json!({"status":status,"headers":{"content-type":"application/json","cache-control":"no-store"},"body":body,"setting":setting,"audit":audit}),
            case["expected"],
            "{}",
            case["name"]
        );
    }
}
use server_store::rusqlite::OptionalExtension;
