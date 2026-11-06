//! Preset GET uses authenticated raw snapshot facts. Synthetic owner, no engine calls.
#![allow(clippy::unwrap_used, clippy::indexing_slicing)]
#[path = "mcp_support/mod.rs"]
mod support;
use serde_json::json;
use support::*;

const PATH: &str = "/api/models/preset";
const FACTS: &str = "/internal/preset-read/facts";

async fn rig() -> (tempfile::TempDir, Front, Upstream) {
    let d = tempfile::tempdir().unwrap();
    seed(d.path());
    let up = upstream().await;
    up.set(|r| {
        if r.path != FACTS {
            return reply_with("202 NodeMethod", "X-Node-Method: yes\r\n", "{}");
        }
        reply(
            "200 OK",
            &json!({
                "kind":"preset", "revision":"a".repeat(64), "exists":true,
                "options":{"ctx-size":"4096"}, "defaults":{"parallel":"2"},
                "fields":["ctx-size","parallel"], "private":"not public"
            })
            .to_string(),
        )
    });
    let f = front_env(
        d.path(),
        true,
        &[
            ("NOEVIA_LEGACY_UPSTREAM", up.base()),
            ("NOEVIA_PROJECT_STORE_TOKEN", STORE_TOKEN.into()),
        ],
    )
    .await;
    (d, f, up)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn exact_get_requires_admin_and_builds_view_from_single_snapshot() {
    let (_d, f, up) = rig().await;
    assert_eq!(get(f.addr, PATH, "").await.0, 401);
    assert_eq!(get(f.addr, PATH, MEMBER).await.0, 403);
    assert!(up.seen().is_empty());
    let (status, headers, body) = raw_text(
        f.addr,
        "GET",
        &format!("{PATH}?model=a%2Bb+c&model=ignored"),
        ADMIN,
        "",
        b"",
    )
    .await;
    assert_eq!(status, 200);
    assert!(headers.contains("cache-control: no-store"));
    assert_eq!(
        j(&body),
        json!({
            "model":"a+b c", "revision":"a".repeat(64), "exists":true,
            "options":{"ctx-size":"4096"}, "defaults":{"parallel":"2"},
            "fields":["ctx-size","parallel"]
        })
    );
    let seen = up.seen();
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].path, FACTS);
    assert_eq!(j(&seen[0].body), json!({"model":"a+b c"}));
    assert_eq!(
        seen[0].header("x-noevia-store-token").as_deref(),
        Some(STORE_TOKEN)
    );
    for key in ["cookie", "x-noevia-client-ip", "x-forwarded-for"] {
        assert_eq!(seen[0].header(key), None);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn other_methods_and_trailing_path_still_proxy() {
    // The PUT is the front's preset editor (D2, tests/preset_editor.rs), not a proxied method.
    let (_d, f, up) = rig().await;
    for (method, path) in [
        ("POST", PATH),
        ("HEAD", PATH),
        ("GET", "/api/models/preset/"),
    ] {
        let (status, headers, _) = raw_text(f.addr, method, path, ADMIN, "", b"").await;
        assert_eq!(status, 202, "{method} {path}");
        assert!(headers.contains("x-node-method: yes"));
    }
    assert!(up.seen().iter().all(|r| r.path != FACTS));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn owner_faults_keep_public_status_and_hide_private_data() {
    let (_d, f, up) = rig().await;
    for (kind, status, message) in [
        ("unavailable", 404, "Native presets are unavailable"),
        (
            "unconfigured",
            501,
            "Native preset editing is not exposed by this llama.cpp adapter",
        ),
        ("nonregular", 409, "Preset path must be a regular file"),
        (
            "duplicate",
            409,
            "Duplicate preset sections require operator repair",
        ),
        ("oversized", 413, "Preset file exceeds the editor limit"),
        ("read-failed", 500, "Internal error"),
        (
            "preset",
            503,
            "Preset state is unavailable. Try again shortly.",
        ),
    ] {
        up.set(move |_| {
            reply(
                "200 OK",
                &json!({"kind":kind,"private":"hidden"}).to_string(),
            )
        });
        let (got, body) = get(f.addr, PATH, ADMIN).await;
        assert_eq!((got, body), (status, json!({"error":message})));
    }
}
