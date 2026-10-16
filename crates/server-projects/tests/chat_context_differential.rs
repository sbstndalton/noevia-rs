//! sbstndalton/noevia#1264 slice 1: crates/server-projects `context` against noevia-core's own
//! GET/POST /api/chats/{chatId}/context. tests/fixtures/node-chat-context.v1.json is written by
//! `node tools/gen-chat-context-fixtures.cjs <noevia-core> <out>` (synthetic workspaces only): for
//! every case Node's status, body text and the projects.json bytes it left. The Rust port must give
//! the same three, byte for byte, except where it is documented to answer otherwise (FIXED).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use serde_json::Value;
use server_projects::{chats, context, store, Body, Ctx};
use server_store::RustProjects;

/// Node bugs the port fixes: (case, status, body). #1902: a malformed escape is 400, not 500.
const FIXED: &[(&str, u16, &str)] = &[
    (
        "get-malformed-escape",
        400,
        "{\"error\":\"Invalid chat identifier\"}",
    ),
    (
        "post-malformed-escape",
        400,
        "{\"error\":\"Invalid chat identifier\"}",
    ),
];

fn fixture() -> Value {
    serde_json::from_str(include_str!("fixtures/node-chat-context.v1.json")).unwrap()
}

fn run_case(c: &Value, user: &str) -> (u16, String, Option<String>) {
    let root = tempfile::tempdir().unwrap();
    let dir = root.path().join("users").join(user);
    std::fs::create_dir_all(&dir).unwrap();
    let ws = &c["ws"];
    for (key, file) in [
        ("projects", store::PROJECTS_FILE),
        ("tombstones", chats::TOMBSTONES_FILE),
    ] {
        if let Some(text) = ws[key].as_str() {
            std::fs::write(dir.join(file), text).unwrap();
        }
    }
    let file = dir.join(store::PROJECTS_FILE);
    let before = std::fs::read_to_string(&file).ok();
    let ctx = Ctx {
        data_dir: root.path(),
        user_id: user,
        switch: RustProjects::from_env_value(Some("1")).unwrap(),
        now_ms: c["now"].as_i64().unwrap(),
    };
    let chat = c["chat"].as_str().unwrap();
    let reply = match c["method"].as_str().unwrap() {
        "GET" => context::get(ctx, chat),
        "POST" => context::post(ctx, chat),
        m => panic!("{m}"),
    };
    let Body::Json(text) = reply.body else {
        panic!("not JSON")
    };
    let after = std::fs::read_to_string(&file).ok();
    (
        reply.status,
        text,
        if after == before { None } else { after },
    )
}

#[test]
fn every_case_answers_and_writes_as_node() {
    let f = fixture();
    let user = f["user"].as_str().unwrap();
    let cases = f["cases"].as_array().unwrap();
    assert!(cases.len() >= 40, "{}", cases.len());
    let mut fixed_seen = 0;
    let mut wrote = 0;
    for c in cases {
        let name = c["name"].as_str().unwrap();
        let (status, text, after) = run_case(c, user);
        if let Some((_, want, body)) = FIXED.iter().find(|(n, _, _)| *n == name) {
            fixed_seen += 1;
            assert_eq!((status, text.as_str()), (*want, *body), "{name}");
            assert_eq!(after, None, "{name}: nothing written");
            continue;
        }
        let expected = &c["expected"];
        assert_eq!(
            u64::from(status),
            expected["status"].as_u64().unwrap(),
            "{name}: {text}"
        );
        assert_eq!(text, expected["body"].as_str().unwrap(), "{name}");
        assert_eq!(
            after.as_deref(),
            expected["projectsAfter"].as_str(),
            "{name}: projects.json"
        );
        wrote += usize::from(after.is_some());
    }
    assert_eq!(fixed_seen, FIXED.len());
    // The fixture exercises the writes, not only the reads.
    assert!(wrote >= 10, "{wrote}");
}
