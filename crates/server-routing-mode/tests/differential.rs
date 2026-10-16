#![allow(clippy::unwrap_used, clippy::indexing_slicing, clippy::panic)]
mod support;
use serde_json::{json, Value};
use server_routing_mode::{self as rm, input::Json, storage::Root};
use server_store::{
    rusqlite::{Connection, OptionalExtension},
    RustAuth, RustRoutingMode, Store, Writer,
};
use std::sync::atomic::AtomicBool;
fn evaluate(
    c: &Value,
    dir: &std::path::Path,
    store: &Store,
    writer: &Writer,
    cap: RustRoutingMode,
) -> (u16, Value) {
    let method = c["method"].as_str().unwrap();
    let main = c["path"] == "/api/routing-mode";
    if method != "PUT" && !(method == "GET" && main) {
        return (405, json!({"error":"Method not allowed"}));
    }
    let env = rm::feature_env(
        c["env"]
            .get("NOEVIA_FEATURE_ROUTING_MODES")
            .and_then(Value::as_str),
    )
    .unwrap();
    let enabled = store.read(|r| rm::enabled(r, env)).unwrap();
    if !enabled {
        return if method == "GET" {
            (200, json!({"enabled":false}))
        } else {
            (
                409,
                json!({"error":"Routing modes are turned off on this server."}),
            )
        };
    }
    let parsed = if method == "PUT" {
        if c["readError"] == true {
            return (400, json!({"error":"invalid JSON"}));
        }
        match Json::parse(&support::unhex(c["body"].as_str().unwrap()).unwrap()) {
            Ok(j) => Some(j),
            Err(_) => return (400, json!({"error":"invalid JSON"})),
        }
    } else {
        None
    };
    let admin = c["role"] == "admin";
    if !main && !admin {
        return (403, json!({"error":"Administrator required"}));
    }
    let result = if !main {
        rm::validate_allowed(parsed.as_ref().unwrap())
            .map(|v| {
                rm::put_allowed(writer, cap, support::ACTOR, v, support::NOW).map_err(|_| {
                    rm::Error {
                        status: 500,
                        message: "Internal error".into(),
                    }
                })
            })
            .and_then(|v| v)
    } else {
        let root = Root::open(dir).unwrap();
        let cancel = AtomicBool::new(false);
        let modes = store.read(rm::allowed).unwrap();
        let setting = if let Some(j) = parsed {
            rm::validate(&j, &modes).inspect(|s| {
                root.write(support::ACTOR, s, cap, &cancel).unwrap();
            })
        } else {
            Ok(root.read(support::ACTOR, &cancel).unwrap())
        };
        setting.map(|s| s.view(store.read(rm::allowed).unwrap(), admin))
    };
    match result {
        Ok(v) => (200, v),
        Err(e) => (e.status, json!({"error":e.message})),
    }
}
#[test]
fn actual_node_configuration_file_sql_and_audit_matrix() {
    let path = std::env::var("NOEVIA_ROUTING_FIXTURE").unwrap_or_else(|_| {
        concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/node-routing.v1.json"
        )
        .into()
    });
    let fixture: Value = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    let expected_sha = std::env::var("NOEVIA_ROUTING_CORE_SHA").unwrap_or_else(|_| {
        include_str!("../../../contracts/http/core.ref")
            .trim()
            .into()
    });
    assert_eq!(fixture["source"]["core"], expected_sha);
    let cases = fixture["cases"].as_array().unwrap();
    let expected = fixture["expected"].as_array().unwrap();
    assert_eq!(cases.len(), expected.len());
    assert!(cases.len() > 250);
    for (c, node) in cases.iter().zip(expected) {
        let d = tempfile::tempdir().unwrap();
        let db = Connection::open(d.path().join("cowork.db")).unwrap();
        db.execute_batch(include_str!(
            "../../server-store/tests/fixtures/node-schema.sql"
        ))
        .unwrap();
        db.execute("INSERT INTO users(id,username,username_norm,display_name,role,password_hash,webauthn_user_id,created_at,updated_at) VALUES(?1,?1,?1,'Synthetic','admin','x',?1,1,1)",[support::ACTOR]).unwrap();
        for (field, key) in [("allowed", rm::KEY), ("feature", rm::FEATURE_KEY)] {
            if let Some(value) = c[field].as_str() {
                db.execute("INSERT INTO settings VALUES(?1,?2)", [key, value])
                    .unwrap();
            }
        }
        let tenant = d.path().join("users").join(support::ACTOR);
        if let Some(bytes) = c["file"].as_str() {
            std::fs::create_dir_all(&tenant).unwrap();
            std::fs::write(tenant.join(rm::FILE), support::unhex(bytes).unwrap()).unwrap();
        }
        if c["auditAbort"] == true {
            db.execute_batch("CREATE TRIGGER routing_audit_abort BEFORE INSERT ON audit_events BEGIN SELECT RAISE(ABORT,'synthetic audit refusal'); END;").unwrap();
        }
        let auth = RustAuth::owned();
        let cap = RustRoutingMode::from_env_value(Some("1"), auth).unwrap();
        let store = Store::open(d.path()).unwrap();
        let writer = Writer::open(d.path(), auth).unwrap();
        let (status, body) = evaluate(c, d.path(), &store, &writer, cap);
        let qualifier = c["auditAbort"] == true
            && node["response"]["status"] == 400
            && node["response"]["body"]["error"] == "synthetic audit refusal";
        if qualifier {
            assert_eq!(status, 500, "{}", c["name"]);
            assert_eq!(body, json!({"error":"Internal error"}));
        } else {
            assert_eq!(
                status,
                node["response"]["status"].as_u64().unwrap() as u16,
                "ROUTING_ASSERT/status/{}",
                c["name"].as_str().unwrap()
            );
            assert_eq!(
                body,
                node["response"]["body"],
                "ROUTING_ASSERT/body/{}",
                c["name"].as_str().unwrap()
            );
        }
        let file = std::fs::read(tenant.join(rm::FILE))
            .ok()
            .map(|b| support::hex(&b));
        assert_eq!(
            json!(file),
            node["file"],
            "ROUTING_ASSERT/file/{}",
            c["name"].as_str().unwrap()
        );
        let setting: Option<String> = db
            .query_row("SELECT value FROM settings WHERE key=?1", [rm::KEY], |r| {
                r.get(0)
            })
            .optional()
            .unwrap();
        assert_eq!(
            json!(setting),
            node["allowed"],
            "ROUTING_ASSERT/allowed/{}",
            c["name"].as_str().unwrap()
        );
        let audit:Vec<Value>=db.prepare("SELECT actor_user_id,target_user_id,action,detail,created_at FROM audit_events ORDER BY id").unwrap().query_map([],|r|Ok(json!({"actor_user_id":r.get::<_,String>(0)?,"target_user_id":r.get::<_,Option<String>>(1)?,"action":r.get::<_,String>(2)?,"detail":serde_json::from_str::<Value>(&r.get::<_,String>(3)?).unwrap(),"created_at":r.get::<_,i64>(4)?}))).unwrap().map(Result::unwrap).collect();
        assert_eq!(
            json!(audit),
            node["audit"],
            "ROUTING_ASSERT/audit/{}",
            c["name"].as_str().unwrap()
        );
    }
}
#[test]
fn synthetic_random_cases_compare_to_actual_node_when_required() {
    // Offline property coverage always runs separately. The required gate also supplies actual
    // source and runs deterministic generated cases through the real pinned factory.
    let Ok(core) = std::env::var("NOEVIA_ROUTING_ORACLE_CORE") else {
        return;
    };
    let sha = std::env::var("NOEVIA_ROUTING_CORE_SHA").unwrap_or_else(|_| {
        include_str!("../../../contracts/http/core.ref")
            .trim()
            .into()
    });
    support::source(std::path::Path::new(&core), &sha).unwrap();
    let mut cases = Vec::new();
    let mut seed = 0x1337_u64;
    for i in 0..256 {
        seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
        let mode = rm::MODES[(seed % 3) as usize];
        let body=json!({"mode":mode,"whenSensitive":if seed&8==0{"ask"}else{"local"},"cloud":{"providerId":format!("synthetic{}",seed%80),"fast":format!("model/{:x}",seed)}}).to_string();
        cases.push(json!({"name":format!("property/{i}"),"file":null,"allowed":null,"feature":"true","env":{},"routeEnv":{},"featureUpdate":null,"directFeature":null,"auditAbort":false,"readError":false,"method":"PUT","path":"/api/routing-mode","role":"member","body":support::hex(body.as_bytes())}));
    }
    let output = support::oracle(std::path::Path::new(&core), &cases).unwrap();
    for (c, node) in cases.iter().zip(output.as_array().unwrap()) {
        let body = support::unhex(c["body"].as_str().unwrap()).unwrap();
        let s = rm::validate(&Json::parse(&body).unwrap(), &rm::all_modes()).unwrap();
        assert_eq!(node["response"]["body"], s.view(rm::all_modes(), false));
        assert_eq!(node["file"], support::hex(s.bytes().as_bytes()));
    }
}
