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
    (
        "get-absent-dir-malformed-escape",
        400,
        "{\"error\":\"Invalid chat identifier\"}",
    ),
    (
        "post-absent-dir-malformed-escape",
        400,
        "{\"error\":\"Invalid chat identifier\"}",
    ),
];

fn fixture() -> Value {
    serde_json::from_str(include_str!("fixtures/node-chat-context.v1.json")).unwrap()
}

/// What a case leaves of the user's directory: (made, private to its owner, the names under users/).
type DirState = (bool, bool, Vec<String>);

fn run_case(c: &Value, user: &str) -> (u16, String, Option<String>, DirState) {
    let root = tempfile::tempdir().unwrap();
    let dir = root.path().join("users").join(user);
    let ws = &c["ws"];
    // A new account has no directory yet (#2026); every other case has one with its files.
    if ws["absent"].as_bool() != Some(true) {
        std::fs::create_dir_all(&dir).unwrap();
    }
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
        switch: RustProjects::owned(),
        now_ms: c["now"].as_i64().unwrap(),
    };
    let chat = c["chat"].as_str().unwrap();
    // The front's step before any Rust-owned route, Node's workspace load for the signed-in user.
    store::ensure_user_dir(root.path(), user).unwrap();
    let reply = match c["method"].as_str().unwrap() {
        "GET" => context::get(ctx, chat),
        "POST" => context::post(ctx, chat),
        m => panic!("{m}"),
    };
    let Body::Json(text) = reply.body else {
        panic!("not JSON")
    };
    let after = std::fs::read_to_string(&file).ok();
    let made = dir.is_dir();
    #[cfg(unix)]
    let private = made && {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(&dir).unwrap().permissions().mode() & 0o077 == 0
    };
    // Node's mode 0700 has no non-unix reading: the fixture's flag is taken as met.
    #[cfg(not(unix))]
    let private = made;
    let users = std::fs::read_dir(root.path().join("users"))
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    (
        reply.status,
        text,
        if after == before { None } else { after },
        (made, private, users),
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
    let mut absent_seen = 0;
    let mut existing_tombstone_seen = false;
    let mut missing_tombstone_get_seen = false;
    let mut missing_tombstone_post_seen = false;
    for c in cases {
        let name = c["name"].as_str().unwrap();
        let (status, text, after, (made, private, users)) = run_case(c, user);
        if let Some((_, want, body)) = FIXED.iter().find(|(n, _, _)| *n == name) {
            fixed_seen += 1;
            assert_eq!((status, text.as_str()), (*want, *body), "{name}");
            assert_eq!(after, None, "{name}: nothing written");
            if c["ws"]["absent"].as_bool() == Some(true) {
                // Node's workspace load precedes the id check: the directory exists after the 400.
                let dir = &c["expected"]["userDir"];
                assert_eq!(Some(made), dir["made"].as_bool(), "{name}: directory made");
                assert_eq!(Some(private), dir["private"].as_bool(), "{name}: mode");
                absent_seen += 1;
            }
            continue;
        }
        let expected = &c["expected"];
        match name {
            "post-existing-tombstoned" => {
                existing_tombstone_seen = true;
                assert_eq!(status, 200, "{name}");
                assert!(text.contains("cowork-chat-context-dead-1"), "{text}");
                assert!(text.contains("Existing tombstoned context"), "{text}");
                assert_eq!(after, None, "{name}: existing context is not recreated");
            }
            "get-tombstoned" => {
                missing_tombstone_get_seen = true;
                assert_eq!((status, text.as_str()), (200, r#"{"project":null}"#));
                assert_eq!(after, None, "{name}: GET never writes");
            }
            "post-tombstoned" => {
                missing_tombstone_post_seen = true;
                assert_eq!(
                    (status, text.as_str()),
                    (410, r#"{"error":"This chat was deleted."}"#)
                );
                assert_eq!(after, None, "{name}: tombstone blocks creation");
            }
            _ => {}
        }
        if c["ws"]["absent"].as_bool() == Some(true) {
            // The new account's first request: Node made the directory (and only it), private.
            let dir = &expected["userDir"];
            assert_eq!(Some(made), dir["made"].as_bool(), "{name}: directory made");
            assert_eq!(Some(private), dir["private"].as_bool(), "{name}: mode");
            let node: Vec<_> = dir["users"]
                .as_array()
                .unwrap()
                .iter()
                .map(|v| v.as_str().unwrap().to_string())
                .collect();
            assert_eq!(users, node, "{name}: users/");
            absent_seen += 1;
        }
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
    assert!(
        existing_tombstone_seen,
        "matching tombstoned context case ran"
    );
    assert!(
        missing_tombstone_get_seen,
        "missing tombstoned GET case ran"
    );
    assert!(
        missing_tombstone_post_seen,
        "missing tombstoned POST case ran"
    );
    assert_eq!(absent_seen, 6, "the new-account cases ran");
    // The fixture exercises the writes, not only the reads.
    assert!(wrote >= 10, "{wrote}");
}
