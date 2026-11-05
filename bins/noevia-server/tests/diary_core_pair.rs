//! The Diary routes Core refuses and the Diary routes routes.toml gives the front are one list
//! (sbstndalton/noevia#2206 review L3). Core's `rust-diary-routes.cjs`, `rust-diary.cjs` and
//! `rust-diary-local-exchange.cjs` refuse a
//! direct request for the front's routes before authentication; a method listed on one side only is a
//! route that two processes answer, or that nobody does. The guard's `assemble-release.sh` checks
//! only that Core's files exist; this test asks Core's own `owned()` functions about every method on
//! every Diary route routes.toml names.
//!
//! Runs with a paired Core checkout: `NOEVIA_DIARY_CORE` (CI: `front-core`, contracts/http/front-core.ref).
//! Without one it says so and passes; with `NOEVIA_DIARY_CORE_REQUIRED=1` a missing checkout or a
//! missing `node` fails.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]

use noevia_server::routes::{Owner, Switches, ROUTES};
use std::collections::BTreeSet;
use std::path::PathBuf;
use std::process::Command;

const METHODS: [&str; 7] = ["GET", "POST", "PUT", "PATCH", "DELETE", "HEAD", "OPTIONS"];

/// The Diary half of routes.toml: the proxy-only five of #2185, the rest of #2206, the local exchange.
fn diary_path(path: &str) -> bool {
    path == "/api/diary"
        || path == "/api/diary-connector"
        || path.starts_with("/api/diary/")
        || path.starts_with("/api/profile/diary-connectors")
}

fn sample(route: &str) -> String {
    route
        .split('/')
        .map(|seg| if seg.starts_with('{') { "x1" } else { seg })
        .collect::<Vec<_>>()
        .join("/")
}

fn core() -> Option<PathBuf> {
    let required = std::env::var("NOEVIA_DIARY_CORE_REQUIRED").is_ok();
    let Ok(dir) = std::env::var("NOEVIA_DIARY_CORE") else {
        assert!(!required, "NOEVIA_DIARY_CORE is required here");
        eprintln!("SKIPPED: NOEVIA_DIARY_CORE (a noevia-core checkout at contracts/http/front-core.ref) is not set");
        return None;
    };
    if Command::new("timeout")
        .args(["5", "node", "--version"])
        .output()
        .is_err()
    {
        assert!(!required, "node is required here");
        eprintln!("SKIPPED: node is not on PATH");
        return None;
    }
    Some(PathBuf::from(dir))
}

/// Core's answers: for each `METHOD path` line on stdin, whether either module refuses it; and every
/// path the two modules name (FIXED keys, the one-segment pattern's sample, `OWNED_PATHS`).
const ASK: &str = r#"
  const dir = process.argv[1];
  const routes = require(dir + '/server/rust-diary-routes.cjs');
  const proxy = require(dir + '/server/rust-diary.cjs');
  const exchange = require(dir + '/server/rust-diary-local-exchange.cjs');
  const asked = JSON.parse(process.argv[2]);
  const refused = asked.map(([m, p]) => routes.owned(m, p) || proxy.owned(p) || exchange.owned(p));
  const named = [...Object.keys(routes.FIXED), ...proxy.OWNED_PATHS, exchange.PATH];
  process.stdout.write(JSON.stringify({ refused, named }));
"#;

#[test]
fn core_refuses_exactly_the_diary_methods_routes_toml_gives_the_front() {
    let Some(core) = core() else { return };
    let mut asked: Vec<(String, String, bool)> = Vec::new();
    let mut named_by_toml = BTreeSet::new();
    for r in ROUTES.iter().filter(|r| diary_path(r.path)) {
        let path = sample(r.path);
        let rust = r.owner_under(Switches::default()) == Owner::Rust;
        if rust {
            named_by_toml.insert(r.path.to_owned());
        }
        for m in METHODS {
            let ours = rust && (r.methods.is_empty() || r.methods.contains(&m));
            asked.push((m.to_owned(), path.clone(), ours));
        }
    }
    assert!(
        asked.iter().filter(|(_, _, ours)| *ours).count() >= 20,
        "routes.toml names the Diary routes"
    );
    let list = format!(
        "[{}]",
        asked
            .iter()
            .map(|(m, p, _)| format!(r#"["{m}","{p}"]"#))
            .collect::<Vec<_>>()
            .join(",")
    );
    let out = Command::new("timeout")
        .args(["10", "node", "-e", ASK])
        .arg(&core)
        .arg(&list)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "node: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let text = String::from_utf8(out.stdout).unwrap();
    let v = js_json::wtf16::parse(&text).unwrap();
    let js_json::JValue::Arr(refused) = v.get("refused") else {
        panic!("{text}");
    };
    assert_eq!(refused.len(), asked.len());
    let mut wrong = Vec::new();
    for ((m, p, ours), core_says) in asked.iter().zip(refused) {
        let core_says = matches!(core_says, js_json::JValue::Bool(true));
        if core_says != *ours {
            wrong.push(format!(
                "{m} {p}: routes.toml gives the front: {ours}; Core refuses: {core_says}"
            ));
        }
    }
    assert!(wrong.is_empty(), "{}", wrong.join("\n"));
    // Every path Core names is a rust route in routes.toml (an extra refusal would be caught above only
    // for paths routes.toml lists at all; this catches a path it does not list).
    let js_json::JValue::Arr(named) = v.get("named") else {
        panic!("{text}");
    };
    for p in named {
        let p = p.as_str().unwrap();
        assert!(
            named_by_toml.contains(p),
            "Core refuses {p}, which routes.toml does not give the front"
        );
    }
    // The local exchange is the front's for every method, and Core refuses every one.
    let exchange: Vec<_> = asked
        .iter()
        .zip(refused)
        .filter(|((_, p, _), _)| p == "/api/diary/local-exchange")
        .collect();
    assert_eq!(exchange.len(), METHODS.len());
    assert!(
        exchange
            .iter()
            .all(|((_, _, ours), r)| *ours && matches!(r, js_json::JValue::Bool(true))),
        "local-exchange is the front's and Core refuses it"
    );
}

#[path = "support/approval_front.rs"]
mod approval_fixture;

/// The existing required paired-Core job supplies the exact front-core.ref checkout and pinned
/// WASM. These cases exercise its real registry, executor, credentials and HTTP MCP transport.
async fn paired_approval_executor(op: &str) {
    use serde_json::json;
    use std::sync::Arc;
    let Some(core) = core() else { return };
    let script =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/support/approval-executor.cjs");
    let child_core = core.clone();
    let (mut child, port, _stdout) = tokio::task::spawn_blocking(move || {
        use std::io::BufRead;
        let mut child = Command::new("timeout")
            .args(["45", "node"])
            .arg(script)
            .arg(child_core)
            .env("DAV_PARSE_WASM_REQUIRED", "1")
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::inherit())
            .spawn()
            .unwrap();
        let stdout = child.stdout.take().unwrap();
        let mut ready = String::new();
        let mut reader = std::io::BufReader::new(stdout);
        reader.read_line(&mut ready).unwrap();
        let port: u16 = ready.trim().parse().unwrap();
        (child, port, reader)
    })
    .await
    .unwrap();
    let peer: std::net::SocketAddr = format!("127.0.0.1:{port}").parse().unwrap();
    let f = approval_fixture::front_with_upstream(
        format!("http://{peer}"),
        Arc::new(approval_fixture::Node::default()),
    )
    .await;
    let (b, _) = f.ticket("ap-real-executor", "qa-one").await;
    let mut payload = json!({"user":f.user,"chat":"qa-one","scope":"space:free","binding":b,"protocol":noevia_server::approval_registry::PROTOCOL});
    if op == "redeem" {
        let (status, ticket) = f.op("approvals/granted", payload.clone()).await;
        assert_eq!(status, 200);
        payload["ticket"] = ticket["ticket"].clone();
    }
    let control = |path: &str| format!("POST {path} HTTP/1.1\r\nhost: fixture\r\n");
    let (status, configured) = approval_fixture::http(
        peer,
        &control("/configure"),
        &json!({"port":f.store.port(),"user":f.user,"binding":b}).to_string(),
    )
    .await;
    assert_eq!(status, 200, "{configured}");
    let (status, paused) = approval_fixture::http(peer, &control("/start"), "{}").await;
    assert_eq!(status, 200, "{paused}");
    assert_eq!(paused["paused"], json!(true));
    if op != "control" {
        server_projects::chat_move::move_chat(
            &f.data,
            &f.user,
            "qa-one",
            server_projects::chat_move::Holder::Project("qa-p".into()),
            None,
            approval_fixture::START,
        )
        .unwrap();
        let (status, answer) = f.op(&format!("approvals/{op}"), payload).await;
        assert_eq!(status, 200, "{answer}");
        assert_eq!(
            answer[if op == "granted" {
                "granted"
            } else {
                "redeemed"
            }],
            json!(false)
        );
    }
    let (status, completed) = approval_fixture::http(peer, &control("/resume"), "{}").await;
    let _ = approval_fixture::http(peer, &control("/stop"), "{}").await;
    assert!(
        tokio::task::spawn_blocking(move || child.wait().unwrap().success())
            .await
            .unwrap()
    );
    assert_eq!(status, 200, "{completed}");
    if op == "control" {
        assert_eq!(
            completed["outbound"],
            json!(1),
            "positive control must reach the actual outbound endpoint: {completed}"
        );
        assert_ne!(completed["outcome"]["notRun"], json!(true));
    } else {
        assert_eq!(completed["outbound"], json!(0), "no write request may reach the endpoint after {op} context invalidation returns: {completed}");
        assert_eq!(
            completed["outcome"]["notRun"],
            json!(true),
            "the real executor must classify the stopped call: {completed}"
        );
    }
}
#[tokio::test]
async fn paired_real_executor_stops_after_context_invalidating_granted() {
    paired_approval_executor("granted").await;
}
#[tokio::test]
async fn paired_real_executor_stops_after_context_invalidating_redeem() {
    paired_approval_executor("redeem").await;
}
#[tokio::test]
async fn paired_real_executor_control_reaches_the_outbound_endpoint() {
    paired_approval_executor("control").await;
}
