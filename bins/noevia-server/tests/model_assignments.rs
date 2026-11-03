//! Exact HTTP and original Node projection on synthetic owner facts. No engines or settings writes.
#![allow(clippy::indexing_slicing, clippy::unwrap_used)]
#[path = "mcp_support/mod.rs"]
mod support;
use support::*;

const PATH: &str = "/api/models/assignments";
const FACTS: &str = "/internal/model-assignments/facts";

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn native_projection_matches_the_old_node_fixture_for_members_and_admins() {
    let dir = tempfile::tempdir().unwrap();
    seed(dir.path());
    let owner = upstream().await;
    let front = front_env(
        dir.path(),
        true,
        &[
            ("NOEVIA_LEGACY_UPSTREAM", owner.base()),
            ("NOEVIA_PROJECT_STORE_TOKEN", STORE_TOKEN.into()),
        ],
    )
    .await;
    let fixtures = j(include_str!("fixtures/model-assignments-node.json"));
    for fixture in fixtures.as_array().unwrap() {
        let admin = fixture["admin"].as_bool().unwrap();
        let facts = fixture["facts"].to_string();
        owner.set(move |r| {
            assert_eq!(r.path, FACTS);
            assert_eq!(r.method, "POST");
            assert_eq!(j(&r.body)["admin"], admin);
            reply("200 OK", &facts)
        });
        let (status, headers, body) = raw_text(
            front.addr,
            "GET",
            PATH,
            if admin { ADMIN } else { MEMBER },
            "",
            b"",
        )
        .await;
        assert_eq!(status, 200, "{}", fixture["name"]);
        assert!(headers.contains("cache-control: no-store"));
        assert_eq!(j(&body), fixture["output"], "{}", fixture["name"]);
    }
    assert_eq!(owner.count(FACTS), 13);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn no_probe_before_auth_and_other_methods_keep_the_node_fallback() {
    let dir = tempfile::tempdir().unwrap();
    seed(dir.path());
    let owner = upstream().await;
    owner.set(|_| reply("418 I'm a teapot", "{}"));
    let front = front_env(
        dir.path(),
        true,
        &[
            ("NOEVIA_LEGACY_UPSTREAM", owner.base()),
            ("NOEVIA_PROJECT_STORE_TOKEN", STORE_TOKEN.into()),
        ],
    )
    .await;
    assert_eq!(get(front.addr, PATH, "").await.0, 401);
    assert!(owner.seen().is_empty());
    for (method, path) in [
        ("PUT", PATH),
        ("HEAD", PATH),
        ("GET", "/api/models/assignments/"),
    ] {
        assert_eq!(write(front.addr, method, path, ADMIN, "").await.0, 418);
    }
    assert_eq!(owner.count(FACTS), 0);
}
