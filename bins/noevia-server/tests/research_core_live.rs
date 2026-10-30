//! sbstndalton/noevia#2218 (review H1): the front's research calls against a REAL Core process.
//!
//! `research.rs` answers the front's lease and tool-bridge calls with a synthetic Node that accepts any token, so it
//! could not see Core's wiring build both routes with an empty secret (Core read `process.env` after its
//! project-store client had deleted the token) and answer every call with the 404 of an unknown path. Here
//! `tests/research_core_live.cjs` boots the paired Core's real `server/index.cjs` (a stand-in project store, a
//! stand-in router with one model, a Diary that answers 503; all on 127.0.0.1; no model, no web) over the data
//! directory the front uses, and:
//!
//! - the front's availability call reaches Core's bridge and brings back Core's own words (no search tools on this
//!   synthetic Core), not the "could not reach" words a 404 gives;
//! - the front's admission client gets a real lease from Core's route and gives it back, and a wrong secret gets
//!   nothing.
//!
//! The search tool itself (an MCP server behind Core) is not stood up here.
//!
//! Needs a noevia-core checkout at `contracts/http/front-core.ref` with its server dependencies and `dav-parse.wasm`:
//! `NOEVIA_RESEARCH_CORE` (CI: `front-core`, built by the paired-core action). Without it the test says so and
//! passes; with `NOEVIA_RESEARCH_CORE_REQUIRED` a missing checkout fails.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]

use engine_admission::{Admission, Ask};
use model_stream::Cancel;
use noevia_server::config::Config;
use noevia_server::serve::{serve, Limits};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::io::{BufRead, BufReader};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

const ORIGIN: &str = "http://127.0.0.1:18021";
const START: &str = "0123456789abcdef0123456789abcdef";
const UNREACHABLE: &str = "could not reach this server's search tools";

fn paired_core() -> Option<PathBuf> {
    let required = std::env::var("NOEVIA_RESEARCH_CORE_REQUIRED").is_ok();
    match std::env::var("NOEVIA_RESEARCH_CORE") {
        Ok(path) if !path.is_empty() => Some(path.into()),
        _ => {
            assert!(
                !required,
                "NOEVIA_RESEARCH_CORE_REQUIRED is set but NOEVIA_RESEARCH_CORE is not"
            );
            eprintln!("SKIPPED: NOEVIA_RESEARCH_CORE (a noevia-core checkout at contracts/http/front-core.ref) is not set");
            None
        }
    }
}

/// The real Core, until dropped.
struct LiveCore {
    child: Child,
    port: u16,
    token: String,
    user: String,
    cookie: String,
    csrf: String,
}

impl LiveCore {
    fn start(core: &std::path::Path, data: &std::path::Path) -> LiveCore {
        let script =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/research_core_live.cjs");
        let mut child = Command::new("node")
            .arg(script)
            .arg(core)
            .arg(data)
            .arg(START)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("node is on PATH");
        let stdout = child.stdout.take().unwrap();
        let (tx, rx) = std::sync::mpsc::channel::<String>();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                if tx.send(line).is_err() {
                    return;
                }
            }
        });
        let line = rx
            .recv_timeout(std::time::Duration::from_secs(120))
            .expect("the paired Core became ready (its own message is on stderr above)");
        let v: Value = serde_json::from_str(&line).unwrap();
        LiveCore {
            child,
            port: u16::try_from(v["port"].as_u64().unwrap()).unwrap(),
            token: v["token"].as_str().unwrap().to_owned(),
            user: v["user"].as_str().unwrap().to_owned(),
            cookie: v["cookie"].as_str().unwrap().to_owned(),
            csrf: v["csrf"].as_str().unwrap().to_owned(),
        }
    }
    fn base(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }
}

impl Drop for LiveCore {
    fn drop(&mut self) {
        // The driver stops Core and removes its files when its stdin ends.
        drop(self.child.stdin.take());
        for _ in 0..100 {
            if matches!(self.child.try_wait(), Ok(Some(_))) {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

struct Front {
    _dir: tempfile::TempDir,
    addr: SocketAddr,
    core: LiveCore,
}

async fn front(core_dir: &std::path::Path) -> Front {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().to_path_buf();
    let live = tokio::task::spawn_blocking({
        let (core_dir, data) = (core_dir.to_path_buf(), data.clone());
        move || LiveCore::start(&core_dir, &data)
    })
    .await
    .unwrap();
    let flag_name = server_features::spec(server_features::Feature::DeepResearch).env;
    let mut env: HashMap<&str, String> = HashMap::new();
    env.insert("NOEVIA_LEGACY_UPSTREAM", live.base());
    env.insert("UI_PORT", "1".into());
    env.insert("UI_DATA_DIR", data.display().to_string());
    env.insert("NOEVIA_WEB_DIST", data.join("dist").display().to_string());
    env.insert("PUBLIC_ORIGIN", ORIGIN.into());
    env.insert("NOEVIA_START_ID", START.into());
    env.insert("NOEVIA_FEATURE_NATIVE_CLIENT_AUTH", "true".into());
    env.insert("TRUST_PROXY", "true".into());
    // The secret the supervisor hands both processes: the same one the paired Core was started with.
    env.insert("NOEVIA_PROJECT_STORE_TOKEN", live.token.clone());
    env.insert("INFERENCE_BASE_URL", "http://127.0.0.1:9".into());
    env.insert(flag_name, "true".into());
    let mut app = noevia_server::App::new(Config::from_lookup(|k| env.get(k).cloned()).unwrap());
    Arc::get_mut(&mut app).unwrap().storage_child =
        Some(env!("CARGO_BIN_EXE_noevia-server").into());
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(serve(
        listener,
        app,
        Limits::default(),
        std::future::pending(),
    ));
    let user_dir = data.join("users").join(&live.user);
    std::fs::create_dir_all(&user_dir).unwrap();
    std::fs::write(
        user_dir.join("projects.json"),
        r#"{"projects":[{"id":"p1","name":"Base","goal":"g","instructions":"i","pinned":false,"archived":false,"sourceFolders":[],"memories":[],"files":[],"routing":"auto","modes":["chat"],"toolboxes":["core"],"toolsMode":"auto","chats":[],"createdAt":1,"updatedAt":2}]}"#,
    )
    .unwrap();
    Front {
        _dir: dir,
        addr,
        core: live,
    }
}

async fn get(f: &Front, path: &str) -> (u16, Value) {
    let mut s = TcpStream::connect(f.addr).await.unwrap();
    let req = format!(
        "GET {path} HTTP/1.1\r\nhost: front.test\r\nconnection: close\r\ncookie: {}\r\nx-csrf-token: {}\r\norigin: {ORIGIN}\r\n\r\n",
        f.core.cookie, f.core.csrf
    );
    s.write_all(req.as_bytes()).await.unwrap();
    let mut out = Vec::new();
    s.read_to_end(&mut out).await.unwrap();
    let text = String::from_utf8_lossy(&out).into_owned();
    let status = text[9..12].parse().unwrap();
    let (_, b) = text.split_once("\r\n\r\n").unwrap();
    (status, serde_json::from_str(b).unwrap_or(Value::Null))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_real_core_answers_the_fronts_bridge_and_lease_calls() {
    let Some(core_dir) = paired_core() else {
        return;
    };
    let f = front(&core_dir).await;

    // The bridge: Core's own availability answer comes back (this synthetic Core has no search tools).
    let (s, v) = get(&f, "/api/projects/p1/research").await;
    assert_eq!(s, 200, "{v}");
    let reason = v["reason"].as_str().unwrap_or_default();
    assert!(
        !reason.contains(UNREACHABLE),
        "the front could not reach Core's bridge (its secret is not the one Core wired): {v}"
    );
    assert!(reason.contains("web-search toolbox"), "{v}");
    assert_eq!(v["available"], json!(false));

    // The lease: a real grant from Core's admission route, and given back.
    let admission = Arc::new(Admission::new(&f.core.base(), Some(&f.core.token)).unwrap());
    let lease = admission
        .enter(Ask::Engine { model: "small" }, &Cancel::new())
        .await
        .unwrap_or_else(|r| panic!("Core refused the front's lease: {}", r.message));
    lease.release().await;

    // Another secret is nobody: the same 404 as an unknown path, so no lease.
    let stranger = Arc::new(
        Admission::new(
            &f.core.base(),
            Some("not-the-fronts-secret-0123456789abcdef"),
        )
        .unwrap(),
    );
    assert!(
        stranger
            .enter(Ask::Engine { model: "small" }, &Cancel::new())
            .await
            .is_err(),
        "a lease for a caller without the secret"
    );
}
