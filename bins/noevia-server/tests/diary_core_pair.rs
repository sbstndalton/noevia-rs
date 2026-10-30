//! The Diary routes Core refuses and the Diary routes routes.toml gives the front are one list
//! (sbstndalton/noevia#2206 review L3). Core's `rust-diary-routes.cjs` and `rust-diary.cjs` refuse a
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

/// The Diary half of routes.toml: the proxy-only five of #2185, the rest of #2206, local-exchange.
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
    if Command::new("node").arg("--version").output().is_err() {
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
  const asked = JSON.parse(process.argv[2]);
  const refused = asked.map(([m, p]) => routes.owned(m, p) || proxy.owned(p));
  const named = [...Object.keys(routes.FIXED), ...proxy.OWNED_PATHS];
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
    let out = Command::new("node")
        .args(["-e", ASK])
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
    // The route that stays Node's is neither refused nor named.
    assert!(
        !asked
            .iter()
            .zip(refused)
            .any(|((_, p, _), r)| p == "/api/diary/local-exchange"
                && matches!(r, js_json::JValue::Bool(true))),
        "local-exchange stays Node's"
    );
}
