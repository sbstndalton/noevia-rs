//! Real HTTP through the native front for the read-only sizing reads (sbstndalton/noevia#2189):
//! `GET /api/models/estimate?model=` and `GET /api/models/preset/suggest?model=`. Every answer is
//! compared byte for byte with the one noevia-core itself gave for the same synthetic fixtures
//! (tests/fixtures/model-sizing/cases.json, recorded by record-node.cjs at the last Core with both
//! handlers): sparse GGUF headers, a models.ini, a fake llama.cpp router listing and the saved
//! inference budget. The legacy upstream is dead, so a proxied request answers 502: a 200/4xx/5xx
//! with Node's body can only come from the native handler. The fake router proves the reads ask it
//! for nothing but its listing. Synthetic data only.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]
use base64::Engine as _;
use noevia_server::{
    config::Config,
    serve::{serve, Limits},
    App,
};
use serde_json::Value;
use server_store::rusqlite::Connection;
use sha2::{Digest, Sha256};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use std::{collections::HashMap, path::Path};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::oneshot,
};

const A: &str = "aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa";
const B: &str = "bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb";
const ADMIN: &str = "cowork_session=admin-token; cowork_csrf=csrf";
const MEMBER: &str = "cowork_session=member-token; cowork_csrf=csrf";
const DEVICE: &str = "nva_synthetic-device-token-0001";
const CASES: &str = include_str!("fixtures/model-sizing/cases.json");

fn hex(s: &str) -> String {
    Sha256::digest(s.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

async fn call(
    addr: std::net::SocketAddr,
    method: &str,
    path: &str,
    headers: &str,
) -> (u16, String, String) {
    let mut s = TcpStream::connect(addr).await.unwrap();
    s.write_all(
        format!(
            "{method} {path} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n{headers}Content-Length: 0\r\n\r\n"
        )
        .as_bytes(),
    )
    .await
    .unwrap();
    let mut bytes = Vec::new();
    tokio::time::timeout(Duration::from_secs(30), s.read_to_end(&mut bytes))
        .await
        .unwrap()
        .unwrap();
    let text = String::from_utf8(bytes).unwrap();
    let (h, b) = text.split_once("\r\n\r\n").unwrap();
    (
        h.split_whitespace().nth(1).unwrap().parse().unwrap(),
        h.to_ascii_lowercase(),
        b.to_string(),
    )
}

fn cookie(c: &str) -> String {
    format!("Cookie: {c}\r\n")
}

/// An admin and a member with sessions, a device token on the admin's account, and the saved
/// inference budget setting (as the text Node stored, or none).
fn seed_database(dir: &Path, budget: Option<&str>) -> Connection {
    let db = Connection::open(dir.join("cowork.db")).unwrap();
    db.execute_batch(include_str!(
        "../../../crates/server-store/tests/fixtures/node-schema.sql"
    ))
    .unwrap();
    let now = noevia_server::identity::now_ms();
    for (uid, role, token) in [(A, "admin", "admin-token"), (B, "member", "member-token")] {
        db.execute("INSERT INTO users(id,username,username_norm,display_name,role,password_hash,webauthn_user_id,created_at,updated_at) VALUES(?1,?1,?1,'Synthetic',?2,'x',?1,1,1)", (uid, role)).unwrap();
        db.execute(
            "INSERT INTO sessions VALUES(?1,?2,?3,?4,?4,?5,'ua','127.0.0.1')",
            (hex(token), uid, hex("csrf"), now, now + 86_400_000),
        )
        .unwrap();
    }
    db.execute(
        "INSERT INTO device_grants VALUES('g-1',?1,'Mac',?2,?2,?3,'ip','ua')",
        (A, now, now + 86_400_000),
    )
    .unwrap();
    db.execute(
        "INSERT INTO device_tokens VALUES(?1,'g-1','access',?2,?3,NULL,NULL)",
        (hex(DEVICE), now, now + 3_600_000),
    )
    .unwrap();
    if let Some(v) = budget {
        db.execute(
            "INSERT INTO settings(key,value) VALUES('inference:memory-budget',?1)",
            [v],
        )
        .unwrap();
    }
    db
}

async fn start(
    dir: &Path,
    extra: &[(String, String)],
) -> (std::net::SocketAddr, oneshot::Sender<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    // A 64 GiB host, as on the host the answers were recorded on: no budget here reaches its
    // ceiling (60 GiB), whatever runner this is.
    let meminfo = dir.join("meminfo");
    std::fs::write(&meminfo, "MemTotal:       67108864 kB\n").unwrap();
    let mut env: HashMap<String, String> = HashMap::from([
        ("NOEVIA_LEGACY_UPSTREAM".into(), "http://127.0.0.1:9".into()),
        ("UI_DATA_DIR".into(), dir.display().to_string()),
        ("TRUST_PROXY".into(), "true".into()),
        ("NOEVIA_FEATURE_NATIVE_CLIENT_AUTH".into(), "1".into()),
        ("PUBLIC_ORIGIN".into(), format!("http://{addr}")),
        (
            "NOEVIA_TEST_MEMINFO_PATH".into(),
            meminfo.display().to_string(),
        ),
    ]);
    for (k, v) in extra {
        env.insert(k.clone(), v.clone());
    }
    let app = App::new(Config::from_lookup(|k| env.get(k).cloned()).unwrap());
    let (stop, rx) = oneshot::channel();
    tokio::spawn(serve(
        listener,
        Arc::clone(&app),
        Limits::default(),
        async {
            let _ = rx.await;
        },
    ));
    (addr, stop)
}

/// A fake llama.cpp router: `GET /models` answers `status` with `text`; it records every request
/// line.
async fn router(status: u16, text: String) -> (u16, Arc<Mutex<Vec<String>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let log = Arc::clone(&seen);
    tokio::spawn(async move {
        loop {
            let Ok((mut s, _)) = listener.accept().await else {
                return;
            };
            let log = Arc::clone(&log);
            let text = text.clone();
            tokio::spawn(async move {
                let mut buf = Vec::new();
                let mut chunk = [0u8; 4096];
                while !buf.windows(4).any(|w| w == b"\r\n\r\n") {
                    match s.read(&mut chunk).await {
                        Ok(0) | Err(_) => return,
                        Ok(n) => buf.extend_from_slice(&chunk[..n]),
                    }
                }
                let head = String::from_utf8_lossy(&buf).to_string();
                let line = head.lines().next().unwrap_or("").to_string();
                log.lock().unwrap().push(line.clone());
                let (code, body) = if line.starts_with("GET /models ") {
                    (status, text)
                } else {
                    (404, "{}".to_string())
                };
                let reply = format!(
                    "HTTP/1.1 {code} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = s.write_all(reply.as_bytes()).await;
            });
        }
    });
    (port, seen)
}

/// The fixture's model mount, cache mount, outside file and models.ini under `root`.
fn lay(root: &Path, cases: &Value, scenario: &Value) {
    for f in cases["files"].as_array().unwrap() {
        let file = root.join(f["path"].as_str().unwrap());
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        if let Some(target) = f["symlink"].as_str() {
            std::os::unix::fs::symlink(target, &file).unwrap();
            continue;
        }
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(f["b64"].as_str().unwrap())
            .unwrap();
        std::fs::write(&file, bytes).unwrap();
        std::fs::File::options()
            .write(true)
            .open(&file)
            .unwrap()
            .set_len(f["size"].as_u64().unwrap())
            .unwrap();
    }
    let ini = root.join("models.ini");
    if scenario["iniSymlink"].as_bool() == Some(true) {
        std::fs::write(root.join("real.ini"), cases["ini"].as_str().unwrap()).unwrap();
        std::os::unix::fs::symlink("real.ini", &ini).unwrap();
    } else if let Some(b) = scenario["iniB64"].as_str() {
        std::fs::write(
            &ini,
            base64::engine::general_purpose::STANDARD.decode(b).unwrap(),
        )
        .unwrap();
    } else if let Some(pad) = scenario["iniPad"].as_u64() {
        let text = format!(
            "[good]\nmodel = /models/good.gguf\n; {}\n",
            "x".repeat(pad as usize)
        );
        std::fs::write(&ini, text).unwrap();
    } else if scenario.get("ini").is_some_and(Value::is_null) {
        // No models.ini at all.
    } else {
        let text = scenario["ini"]
            .as_str()
            .unwrap_or_else(|| cases["ini"].as_str().unwrap());
        std::fs::write(&ini, text).unwrap();
    }
}

#[tokio::test]
async fn every_recorded_node_answer_is_the_fronts_byte_for_byte() {
    let cases: Value = serde_json::from_str(CASES).unwrap();
    let mut checked = 0;
    for scenario in cases["scenarios"].as_array().unwrap() {
        let name = scenario["name"].as_str().unwrap();
        let d = tempfile::tempdir().unwrap();
        let root = d.path().join("root");
        std::fs::create_dir_all(&root).unwrap();
        lay(&root, &cases, scenario);
        let _db = seed_database(d.path(), scenario["budgetSetting"].as_str());
        let (engine_port, seen) = match &scenario["listing"] {
            Value::String(s) if s == "down" => (9, Arc::new(Mutex::new(Vec::new()))),
            Value::Object(o) => {
                router(
                    o["status"].as_u64().unwrap() as u16,
                    o["text"].as_str().unwrap().to_owned(),
                )
                .await
            }
            _ => router(200, cases["listing"].as_str().unwrap().to_owned()).await,
        };
        let mut env: Vec<(String, String)> = scenario["env"]
            .as_object()
            .unwrap()
            .iter()
            .map(|(k, v)| {
                (
                    k.clone(),
                    v.as_str()
                        .unwrap()
                        .replace("$ROOT", &root.display().to_string()),
                )
            })
            .collect();
        env.push(("MODEL_MANAGER_KIND".into(), "llamacpp".into()));
        env.push((
            "MODEL_MANAGER_BASE_URL".into(),
            format!("http://127.0.0.1:{engine_port}/v1"),
        ));
        let (addr, stop) = start(d.path(), &env).await;
        for r in scenario["results"].as_array().unwrap() {
            let route = r["route"].as_str().unwrap();
            let query = r["query"].as_str().unwrap();
            let target = format!("/api/models/{route}{query}");
            let who = if r["role"] == "admin" { ADMIN } else { MEMBER };
            let (status, headers, body) = call(addr, "GET", &target, &cookie(who)).await;
            assert_eq!(
                (status, body.as_str()),
                (
                    r["status"].as_u64().unwrap() as u16,
                    r["body"].as_str().unwrap()
                ),
                "{name}: GET {target} ({})",
                r["role"]
            );
            assert!(
                headers.contains("cache-control: no-store"),
                "{name} {target}"
            );
            assert!(headers.contains("content-type: application/json"));
            checked += 1;
        }
        let _ = stop.send(());
        let seen = seen.lock().unwrap().clone();
        assert!(
            seen.iter().all(|l| l.starts_with("GET /models ")),
            "{name}: the router was asked {seen:?}"
        );
    }
    assert_eq!(checked, 178, "every recorded exchange was replayed");
}

#[tokio::test]
async fn nothing_is_written_and_signed_out_device_and_disabled_kinds_answer_like_node() {
    let cases: Value = serde_json::from_str(CASES).unwrap();
    let d = tempfile::tempdir().unwrap();
    let root = d.path().join("root");
    std::fs::create_dir_all(&root).unwrap();
    lay(&root, &cases, &cases["scenarios"][0]);
    let db = seed_database(d.path(), Some(r#"{"budgetGib":6}"#));
    let snapshot = |dir: &Path| {
        let mut out = Vec::new();
        let mut stack = vec![dir.to_path_buf()];
        while let Some(p) = stack.pop() {
            for e in std::fs::read_dir(&p).unwrap() {
                let e = e.unwrap();
                let m = std::fs::symlink_metadata(e.path()).unwrap();
                if m.is_dir() {
                    stack.push(e.path());
                } else {
                    let content = if m.is_file() && m.len() < 1 << 20 {
                        std::fs::read(e.path()).unwrap()
                    } else {
                        Vec::new()
                    };
                    out.push((e.path(), m.len(), content));
                }
            }
        }
        out.sort();
        out
    };
    let before = snapshot(&root);
    let (port, seen) = router(200, cases["listing"].as_str().unwrap().to_owned()).await;
    let env = [
        ("MODEL_MANAGER_KIND", "llamacpp".to_string()),
        (
            "MODEL_MANAGER_BASE_URL",
            format!("http://127.0.0.1:{port}/v1"),
        ),
        (
            "LLAMACPP_PRESET_PATH",
            root.join("models.ini").display().to_string(),
        ),
        (
            "LLAMACPP_MODELS_PATH",
            root.join("models").display().to_string(),
        ),
        (
            "LLAMACPP_CACHE_PATH",
            root.join("cache").display().to_string(),
        ),
    ]
    .map(|(k, v)| (k.to_string(), v));
    let (addr, stop) = start(d.path(), &env).await;
    for p in [
        "/api/models/estimate?model=good",
        "/api/models/preset/suggest?model=good",
    ] {
        let (s, h, _) = call(addr, "GET", p, "").await;
        assert_eq!(s, 401, "{p}");
        assert!(h.contains("www-authenticate: bearer realm=\"cowork\""));
        // A device token acts as a member, even on an administrator's account.
        let (s, _, b) = call(
            addr,
            "GET",
            p,
            &format!("Authorization: Bearer {DEVICE}\r\n"),
        )
        .await;
        assert_eq!(
            (s, b.as_str()),
            (
                403,
                r#"{"error":"Administrator required for shared model profiles"}"#
            ),
            "{p}"
        );
        let (s, _, _) = call(addr, "GET", p, &cookie(ADMIN)).await;
        assert_eq!(s, 200, "{p}");
    }
    let _ = stop.send(());
    assert_eq!(
        before,
        snapshot(&root),
        "models.ini and the mounts unchanged"
    );
    let budget: String = db
        .query_row(
            "SELECT value FROM settings WHERE key='inference:memory-budget'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(budget, r#"{"budgetGib":6}"#, "the budget is not written");
    assert!(seen
        .lock()
        .unwrap()
        .iter()
        .all(|l| l == "GET /models HTTP/1.1"));

    // No model manager (the `none` kind): routes/models.cjs's 404s; the admin check comes first.
    let d = tempfile::tempdir().unwrap();
    let _db = seed_database(d.path(), None);
    let (addr, stop) = start(d.path(), &[]).await;
    for (p, body) in [
        (
            "/api/models/estimate?model=good",
            r#"{"error":"Memory estimates need the native engine"}"#,
        ),
        (
            "/api/models/estimate",
            r#"{"error":"Memory estimates need the native engine"}"#,
        ),
        (
            "/api/models/preset/suggest?model=good",
            r#"{"error":"Native preset suggestions are unavailable"}"#,
        ),
    ] {
        let (s, _, b) = call(addr, "GET", p, &cookie(ADMIN)).await;
        assert_eq!((s, b.as_str()), (404, body), "{p}");
        let (s, _, _) = call(addr, "GET", p, &cookie(MEMBER)).await;
        assert_eq!(s, 403, "{p}");
    }
    // Every other method and a trailing slash stay Node's (the dead upstream: 502).
    let csrf = format!(
        "{}X-CSRF-Token: csrf\r\nOrigin: http://{addr}\r\n",
        cookie(ADMIN)
    );
    for (m, p) in [
        ("POST", "/api/models/estimate?model=m"),
        ("PUT", "/api/models/preset/suggest?model=m"),
        ("HEAD", "/api/models/estimate?model=m"),
        ("GET", "/api/models/estimate/?model=m"),
        ("GET", "/api/models/preset/suggest/?model=m"),
        ("GET", "/api/models/preset?model=m"),
        // #2210: GET evidence is the front's; the other methods of its path are Node's.
        ("POST", "/api/models/evidence?model=m"),
        ("PUT", "/api/models/inference-budget"),
    ] {
        let (s, _, b) = call(addr, m, p, &csrf).await;
        assert_eq!(s, 502, "{m} {p}: {b}");
    }
    let _ = stop.send(());
}
