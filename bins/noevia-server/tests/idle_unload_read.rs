//! Synthetic loader and Node baseline; this route never writes or starts a worker.
#![allow(clippy::indexing_slicing, clippy::unwrap_used)]
#[path = "mcp_support/mod.rs"]
mod support;
use serde_json::json;
use support::*;

const PATH: &str = "/api/models/idle-unload";
const FACTS: &str = "/internal/idle-unload-read/facts";
const SAVED: &str =
    r#"{"seconds":1800,"source":"default","fallbackSeconds":1800,"fallbackSource":"default"}"#;

async fn rig(configured: bool) -> (tempfile::TempDir, Front, Upstream, Upstream) {
    let dir = tempfile::tempdir().unwrap();
    seed(dir.path());
    let owner = upstream().await;
    let loader = upstream().await;
    owner.set(|_| reply("200 OK", r#"{"seconds":1800}"#));
    loader.set(|_| reply("200 OK", SAVED));
    let mut env = vec![
        ("NOEVIA_LEGACY_UPSTREAM", owner.base()),
        ("NOEVIA_PROJECT_STORE_TOKEN", STORE_TOKEN.into()),
    ];
    if configured {
        env.push(("MODEL_LOADER_URL", loader.base()));
        env.push(("MODEL_LOADER_TOKEN", "synthetic-loader-key".into()));
    }
    let f = front_env(dir.path(), true, &env).await;
    (dir, f, owner, loader)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn saved_and_live_timers_match_only_when_known() {
    let (_dir, f, owner, loader) = rig(true).await;
    for (facts, applied) in [
        (r#"{"seconds":1800}"#, true),
        (r#"{"seconds":900}"#, false),
        (r#"{"seconds":null}"#, false),
    ] {
        owner.set(move |_| reply("200 OK", facts));
        let (status, headers, body) = raw_text(f.addr, "GET", PATH, ADMIN, "", b"").await;
        assert_eq!(status, 200);
        assert!(headers.contains("cache-control: no-store"));
        assert_eq!(
            j(&body),
            json!({
                "seconds": 1800, "source": "default", "fallbackSeconds": 1800,
                "fallbackSource": "default", "defaultSeconds": 1800, "minSeconds": 300,
                "maxSeconds": 86400, "applied": applied,
            })
        );
    }
    assert_eq!(owner.count(FACTS), 3);
    for call in loader.seen() {
        assert_eq!(call.method, "GET");
        assert_eq!(call.path, "/api/v1/idle-unload");
        assert_eq!(
            call.header("x-model-loader-token").as_deref(),
            Some("synthetic-loader-key")
        );
        assert_eq!(call.header("cookie"), None);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn authorization_and_only_exact_get_precede_any_loader_read() {
    let (_dir, f, owner, loader) = rig(true).await;
    assert_eq!(get(f.addr, PATH, "").await.0, 401);
    assert_eq!(get(f.addr, PATH, MEMBER).await.0, 403);
    assert!(owner.seen().is_empty());
    owner.set(|_| reply("418 I'm a teapot", "{}"));
    for (method, path) in [
        ("HEAD", PATH),
        ("PUT", PATH),
        ("GET", "/api/models/idle-unload/"),
    ] {
        assert_eq!(write(f.addr, method, path, ADMIN, "").await.0, 418);
    }
    assert_eq!(owner.count(FACTS), 0);
    assert!(loader.seen().is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn loader_failures_keep_the_old_public_status_and_error_id() {
    let (_dir, f, owner, loader) = rig(true).await;
    for (loader_status, public_status, id) in [
        ("404 Not Found", 404, "idleUnloadUnavailable"),
        ("405 Method Not Allowed", 404, "idleUnloadUnavailable"),
        ("400 Bad Request", 400, "idleUnloadInvalid"),
        ("503 Service Unavailable", 503, "idleUnloadUnavailable"),
        ("500 Internal Server Error", 502, "idleUnloadEngine"),
        ("200 OK", 502, "idleUnloadEngine"),
    ] {
        loader.set(move |_| reply(loader_status, "{}"));
        let (status, body) = get(f.addr, PATH, ADMIN).await;
        assert_eq!(status, public_status);
        assert_eq!(body["errorId"], id);
    }
    assert!(owner.seen().is_empty());
    let (_dir, f, owner, loader) = rig(false).await;
    assert_eq!(get(f.addr, PATH, ADMIN).await.0, 404);
    assert!(owner.seen().is_empty() && loader.seen().is_empty());
}
