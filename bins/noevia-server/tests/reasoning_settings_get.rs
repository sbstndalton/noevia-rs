#![cfg(feature = "rust-reasoning-settings")]
//! Track R12: `GET /api/reasoning-settings` through the real HTTP front with NOEVIA_RUST_SETTINGS_ROUTES=1,
//! replayed on the cases recorded from core's own route
//! (crates/server-reasoning-settings/tests/fixtures/node-reasoning-get.v1.json): the same environment, settings
//! row, query, projects and provider files give Node's reply. A dead legacy upstream makes any proxied request
//! answer 502, so with the switch unset the GET must reach it. An environment the front refuses to start with
//! (the Lemonade manager kind, an engine address it cannot use) is skipped here; the crate's replay covers it.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]
use noevia_server::{
    config::Config,
    serve::{serve, Limits},
    App,
};
use serde_json::Value;
use server_store::rusqlite::Connection;
use sha2::{Digest, Sha256};
use std::{collections::HashMap, path::Path, sync::Arc};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::oneshot,
};

const FIXTURE: &str = include_str!(
    "../../../crates/server-reasoning-settings/tests/fixtures/node-reasoning-get.v1.json"
);
const COOKIE: &str = "cowork_session=qa-token; cowork_csrf=csrf";
const SWITCH: (&str, &str) = ("NOEVIA_RUST_SETTINGS_ROUTES", "1");

fn hex(s: &str) -> String {
    Sha256::digest(s.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

async fn call(addr: std::net::SocketAddr, method: &str, target: &str, cookie: &str) -> (u16, String) {
    let mut s = TcpStream::connect(addr).await.unwrap();
    s.write_all(
        format!(
            "{method} {target} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\nCookie: {cookie}\r\nX-CSRF-Token: csrf\r\nOrigin: http://{addr}\r\nContent-Length: 0\r\n\r\n"
        )
        .as_bytes(),
    )
    .await
    .unwrap();
    let mut bytes = Vec::new();
    tokio::time::timeout(
        std::time::Duration::from_secs(20),
        s.read_to_end(&mut bytes),
    )
    .await
    .unwrap()
    .unwrap();
    let text = String::from_utf8(bytes).unwrap();
    let (h, b) = text.split_once("\r\n\r\n").unwrap();
    (
        h.split_whitespace().nth(1).unwrap().parse().unwrap(),
        b.to_string(),
    )
}

fn seed_database(dir: &Path, user: &str, role: &str, setting: Option<&str>) {
    let db = Connection::open(dir.join("cowork.db")).unwrap();
    db.execute_batch(include_str!(
        "../../../crates/server-store/tests/fixtures/node-schema.sql"
    ))
    .unwrap();
    let now = noevia_server::identity::now_ms();
    db.execute("INSERT INTO users(id,username,username_norm,display_name,role,password_hash,webauthn_user_id,created_at,updated_at) VALUES(?1,?1,?1,'Synthetic QA',?2,'x',?1,1,1)", (user, role)).unwrap();
    db.execute(
        "INSERT INTO sessions VALUES(?1,?2,?3,?4,?4,?5,'ua','127.0.0.1')",
        (hex("qa-token"), user, hex("csrf"), now, now + 86400000),
    )
    .unwrap();
    if let Some(v) = setting {
        db.execute(
            "INSERT INTO settings(key,value) VALUES('reasoning_effort_default',?1)",
            [v],
        )
        .unwrap();
    }
}

async fn start(
    dir: &Path,
    extra: &[(&str, &str)],
) -> Option<(std::net::SocketAddr, oneshot::Sender<()>)> {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let origin = format!("http://{addr}");
    let mut env: HashMap<&str, String> = HashMap::from([
        ("NOEVIA_LEGACY_UPSTREAM", "http://127.0.0.1:9".into()),
        ("UI_DATA_DIR", dir.display().to_string()),
        ("TRUST_PROXY", "true".into()),
        ("NOEVIA_FEATURE_NATIVE_CLIENT_AUTH", "1".into()),
        ("PUBLIC_ORIGIN", origin),
    ]);
    for (k, v) in extra {
        env.insert(k, (*v).to_string());
    }
    let config = Config::from_lookup(|k| env.get(k).cloned()).ok()?;
    let app = App::new(config);
    let (stop, rx) = oneshot::channel();
    tokio::spawn(serve(
        listener,
        Arc::clone(&app),
        Limits::default(),
        async {
            let _ = rx.await;
        },
    ));
    Some((addr, stop))
}

#[tokio::test]
async fn recorded_node_cases_through_the_front() {
    let fixture: Value = serde_json::from_str(FIXTURE).unwrap();
    let user = fixture["user"].as_str().unwrap();
    let (mut ran, mut skipped) = (0, Vec::new());
    for case in fixture["cases"].as_array().unwrap() {
        let name = case["name"].as_str().unwrap();
        let input = &case["input"];
        let d = tempfile::tempdir().unwrap();
        seed_database(
            d.path(),
            user,
            input["role"].as_str().unwrap_or("member"),
            input["setting"].as_str(),
        );
        let tenant = d.path().join("users").join(user);
        std::fs::create_dir_all(&tenant).unwrap();
        for (key, path) in [
            ("projects", tenant.join("projects.json")),
            ("private", tenant.join("providers.json")),
            ("shared", d.path().join("shared-providers.json")),
        ] {
            if let Some(text) = input[key].as_str() {
                std::fs::write(path, text).unwrap();
            }
        }
        let mut env: Vec<(&str, &str)> = vec![SWITCH];
        if let Some(vars) = input["env"].as_object() {
            env.extend(vars.iter().map(|(k, v)| (k.as_str(), v.as_str().unwrap())));
        }
        let Some((addr, stop)) = start(d.path(), &env).await else {
            skipped.push(name.to_string());
            continue;
        };
        let target = format!("/api/reasoning-settings{}", input["query"].as_str().unwrap());
        let (status, reply) = call(addr, "GET", &target, COOKIE).await;
        let _ = stop.send(());
        ran += 1;
        if case["difference"] == "unholdable-providers" {
            assert_eq!(status, 503, "{name}: fails closed");
            continue;
        }
        let expected = &case["expected"];
        assert_eq!(u64::from(status), expected["status"].as_u64().unwrap(), "{name}: {reply}");
        assert_eq!(reply, expected["reply"].as_str().unwrap(), "{name}");
    }
    // Only environments the front refuses at startup are skipped (in production it never runs with them).
    assert!(ran >= 100, "ran {ran}, skipped {skipped:?}");
    for name in &skipped {
        assert!(
            ["local-lemonade-kind", "local-kind-unset-lemonade-url", "local-inference-unset", "local-inference-invalid"]
                .contains(&name.as_str()),
            "{name} was skipped"
        );
    }
}

#[tokio::test]
async fn switch_unset_leaves_the_get_to_node_and_the_gate_comes_first() {
    let user = "dddddddd-4444-4ddd-8ddd-dddddddddddd";
    let d = tempfile::tempdir().unwrap();
    seed_database(d.path(), user, "member", Some("high"));
    for extra in [&[][..], &[("NOEVIA_RUST_SETTINGS_ROUTES", "yes")][..]] {
        let (addr, stop) = start(d.path(), extra).await.unwrap();
        assert_eq!(call(addr, "GET", "/api/reasoning-settings", COOKIE).await.0, 502);
        let _ = stop.send(());
    }
    let (addr, stop) = start(d.path(), &[SWITCH]).await.unwrap();
    assert_eq!(call(addr, "GET", "/api/reasoning-settings", "").await.0, 401);
    assert_eq!(
        call(addr, "GET", "/api/reasoning-settings", COOKIE).await,
        (200, r#"{"default":"high","effort":"high","mode":"hint","admin":false}"#.to_string())
    );
    // Other methods stay where they were: POST to Node (502), PUT the native admin route (403 for a member).
    assert_eq!(call(addr, "POST", "/api/reasoning-settings", COOKIE).await.0, 502);
    assert_eq!(call(addr, "PUT", "/api/reasoning-settings", COOKIE).await.0, 403);
    let _ = stop.send(());
}
