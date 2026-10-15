//! M4: crates/server-projects `free_chats` against noevia-core's own GET/POST /api/freechats.
//! tests/fixtures/node-free-chats.v1.json is written by `node tools/gen-free-chats-fixtures.cjs
//! <noevia-core> <out>` (synthetic workspaces only): for every case Node's status, body text and
//! the free-chats.json bytes it left. The Rust port must give the same three, byte for byte, except
//! where it is documented to refuse instead (STRICTER).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use serde_json::Value;
use server_projects::{chats, free_chats, Body, Ctx};
use server_store::RustProjects;

/// Cases where Node answers what the port refuses (no write): a number past the f64 range in the
/// body; a stored meta whose `updatedAt` is a non-numeric string (Node's sort comparator is then
/// inconsistent). (A stored object with a `length` key was one until #1420: Node now loads any
/// stored value that is not a list as `[]`, and so does the port.)
const STRICTER: &[(&str, u16)] = &[
    ("post-body-infinity", 400),
    ("post-stored-updated-string", 500),
];

fn fixture() -> Value {
    serde_json::from_str(include_str!("fixtures/node-free-chats.v1.json")).unwrap()
}

fn body_of(c: &Value) -> Option<Vec<u8>> {
    if let Some(r) = c.get("bodyRepeat") {
        let mut s = r["head"].as_str().unwrap_or("").to_string();
        s.push_str(
            &r["text"]
                .as_str()
                .unwrap()
                .repeat(r["count"].as_u64().unwrap() as usize),
        );
        s.push_str(r["tail"].as_str().unwrap_or(""));
        return Some(s.into_bytes());
    }
    c["body"].as_str().map(|s| s.as_bytes().to_vec())
}

/// One case through the port on a fresh synthetic workspace: (status, body, free-chats.json if
/// written).
pub fn run_case(c: &Value, user: &str) -> (u16, String, Option<String>) {
    let root = tempfile::tempdir().unwrap();
    let dir = root.path().join("users").join(user);
    std::fs::create_dir_all(&dir).unwrap();
    let ws = &c["ws"];
    for (key, file) in [
        ("projects", "projects.json"),
        ("free", chats::FREE_CHATS_FILE),
        ("tombstones", chats::TOMBSTONES_FILE),
    ] {
        if let Some(text) = ws[key].as_str() {
            std::fs::write(dir.join(file), text).unwrap();
        }
    }
    let file = dir.join(chats::FREE_CHATS_FILE);
    let before = std::fs::read_to_string(&file).ok();
    let ctx = Ctx {
        data_dir: root.path(),
        user_id: user,
        switch: RustProjects::from_env_value(Some("1")).unwrap(),
        now_ms: c["now"].as_i64().unwrap(),
    };
    let reply = match c["method"].as_str().unwrap() {
        "GET" => free_chats::get(ctx),
        "POST" => {
            let mut body = body_of(c).unwrap_or_default();
            let over = body.len() > chats::BODY_LIMIT;
            body.truncate(chats::BODY_LIMIT + 1);
            free_chats::post(ctx, &body, over, c["admin"].as_bool().unwrap())
        }
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
    assert!(cases.len() >= 70, "{}", cases.len());
    let mut stricter_seen = 0;
    for c in cases {
        let name = c["name"].as_str().unwrap();
        let (status, text, wrote) = run_case(c, user);
        if let Some((_, want)) = STRICTER.iter().find(|(n, _)| *n == name) {
            stricter_seen += 1;
            assert_eq!(status, *want, "{name}");
            assert_eq!(wrote, None, "{name}: a refusal writes nothing");
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
            wrote.as_deref(),
            expected["freeAfter"].as_str(),
            "{name}: free-chats.json"
        );
    }
    assert_eq!(stricter_seen, STRICTER.len());
}

/// The fixture is Node's: the generator ran at the pinned noevia-core commit
/// (contracts/http/core.ref), with Node 22. CI regenerates it there and compares the bytes.
#[test]
fn fixture_provenance() {
    let f = fixture();
    let pinned = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../contracts/http/core.ref"),
    )
    .unwrap();
    assert_eq!(
        f["core"].as_str().unwrap(),
        pinned.trim(),
        "regenerate with tools/gen-free-chats-fixtures.cjs at core.ref"
    );
    assert_eq!(f["node"].as_str().unwrap(), "v22");
}
