//! #2293: real front identity/dispatch against a synthetic actor; never touches an engine.
#![allow(clippy::unwrap_used, clippy::indexing_slicing, clippy::panic)]
#[path = "mcp_support/mod.rs"]
mod support;
use serde_json::{json, Value};
use support::*;
const PATH: &str = "/api/models/autotune/clear";
const FACTS: &str = "/internal/autotune-clear/facts";
const CLEAR: &str = "/internal/autotune-clear/clear";
const INVALIDATE: &str = "/internal/auto-roles/invalidate";
const SCAN_CLEAR: &str = "/internal/model-manager-scan/clear";
/// What the route asked the actor (Core's model-read invalidation and scan clear left out).
fn actor(up: &Upstream) -> Vec<Seen> {
    up.seen()
        .into_iter()
        .filter(|r| r.path != INVALIDATE && r.path != SCAN_CLEAR)
        .collect()
}
fn facts() -> Value {
    json!({"available":true,"starting":false,"jobPresent":true,"status":"failed","reloadPending":false,"token":"a".repeat(64)})
}
async fn rig() -> (tempfile::TempDir, Front, Upstream) {
    let d = tempfile::tempdir().unwrap();
    seed(d.path());
    let up = upstream().await;
    up.set(|r| {
        if r.path == FACTS {
            reply("200 OK", &facts().to_string())
        } else if r.path == CLEAR {
            reply("200 OK", r#"{"applied":true}"#)
        } else {
            reply("500 WrongPath", "{}")
        }
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
async fn native_dismissal_ignores_body_and_query_and_uses_only_the_checked_token() {
    let (_d, f, up) = rig().await;
    let (s, v) = write(
        f.addr,
        "POST",
        &format!("{PATH}?model=other"),
        ADMIN,
        "{malformed",
    )
    .await;
    assert_eq!((s, v), (200, json!({"cleared":true})));
    let seen = actor(&up);
    assert_eq!(seen.len(), 2);
    assert_eq!(seen[0].path, FACTS);
    assert_eq!(j(&seen[0].body), json!({}));
    assert_eq!(seen[1].path, CLEAR);
    assert_eq!(j(&seen[1].body), json!({"token":"a".repeat(64)}));
    for r in seen {
        assert_eq!(
            r.header("x-noevia-store-token").as_deref(),
            Some(STORE_TOKEN)
        );
        assert_eq!(r.header("cookie"), None);
        assert_eq!(r.header("x-noevia-client-ip"), None);
    }
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn auth_role_csrf_precede_private_facts_and_availability_precedes_method() {
    let (_d, f, up) = rig().await;
    assert_eq!(get(f.addr, PATH, "").await.0, 401);
    assert_eq!(write(f.addr, "POST", PATH, MEMBER, "").await.0, 403);
    assert_eq!(get(f.addr, PATH, MEMBER).await.0, 403);
    assert_eq!(raw(f.addr, "POST", PATH, ADMIN, "", b"").await.0, 403);
    assert!(actor(&up).is_empty());
    assert_eq!(get(f.addr, PATH, ADMIN).await.0, 405);
    assert_eq!(up.count(CLEAR), 0);
    up.set(|_| reply("200 OK", r#"{"available":false}"#));
    assert_eq!(get(f.addr, PATH, ADMIN).await.0, 404);
    assert_eq!(write(f.addr, "POST", PATH, ADMIN, "").await.0, 404);
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn active_absent_unfinished_and_reload_pending_refusals_preserve_guard_order() {
    let (_d, f, up) = rig().await;
    for (key, value, message) in [
        (
            "starting",
            json!(true),
            "Auto-tune is running. Cancel it before clearing.",
        ),
        (
            "status",
            json!("running"),
            "Auto-tune is running. Cancel it before clearing.",
        ),
        (
            "jobPresent",
            json!(false),
            "There is no finished auto-tune to clear.",
        ),
        (
            "status",
            json!("unknown"),
            "There is no finished auto-tune to clear.",
        ),
        (
            "reloadPending",
            json!(true),
            "The model server has not re-read the restored settings yet. Try again shortly.",
        ),
    ] {
        up.set(move |_| {
            let mut v = facts();
            v[key] = value.clone();
            reply("200 OK", &v.to_string())
        });
        assert_eq!(
            write(f.addr, "POST", PATH, ADMIN, "").await,
            (409, json!({"error":message}))
        );
    }
    assert_eq!(up.count(CLEAR), 0);
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn lost_generation_never_retargets_and_dead_malformed_actor_cannot_claim_success() {
    let (_d, f, up) = rig().await;
    up.set(|r| {
        if r.path == FACTS {
            reply("200 OK", &facts().to_string())
        } else {
            reply("409 Changed", r#"{"applied":false}"#)
        }
    });
    assert_eq!(write(f.addr, "POST", PATH, ADMIN, "").await.0, 409);
    assert_eq!(up.count(FACTS), 1);
    assert_eq!(up.count(CLEAR), 1);
    for bad in [
        json!({}),
        json!({"available":true}),
        json!({"available":true,"starting":"no"}),
        json!({"available":true,"starting":false,"jobPresent":true,"status":"failed","reloadPending":false,"token":"bad"}),
    ] {
        up.set(move |_| reply("200 OK", &bad.to_string()));
        assert_eq!(write(f.addr, "POST", PATH, ADMIN, "").await.0, 503);
    }
    up.set(|r| {
        if r.path == FACTS {
            reply("200 OK", &facts().to_string())
        } else {
            reply("200 OK", r#"{"applied":false}"#)
        }
    });
    assert_eq!(write(f.addr, "POST", PATH, ADMIN, "").await.0, 503);
}

/// #2618, core routes/models.cjs at cc74c6f3 (lines 196-202 and 244): every request Core served
/// under /api/models/ but a GET or HEAD invalidated its shared model reads first (a member's
/// too), then, past the administrator check, cleared its cached folder scan (not for OPTIONS);
/// a signed-out or CSRF-refused request never reached it. Both come before the actor is asked.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn writes_drop_cores_model_reads_then_past_the_admin_check_its_scan() {
    let (_d, f, up) = rig().await;
    let calls = || (up.count(INVALIDATE), up.count(SCAN_CLEAR));
    assert_eq!(write(f.addr, "POST", PATH, "", "{}").await.0, 401);
    assert_eq!(raw(f.addr, "POST", PATH, ADMIN, "", b"{}").await.0, 403);
    assert_eq!(calls(), (0, 0));
    assert_eq!(get(f.addr, PATH, MEMBER).await.0, 403);
    assert_eq!(calls(), (0, 0));
    assert_eq!(write(f.addr, "POST", PATH, MEMBER, "{}").await.0, 403);
    assert_eq!(calls(), (1, 0));
    assert_eq!(get(f.addr, PATH, ADMIN).await.0, 405);
    assert_eq!(calls(), (1, 0));
    assert_eq!(write(f.addr, "OPTIONS", PATH, ADMIN, "").await.0, 405);
    assert_eq!(calls(), (2, 0));
    assert_eq!(write(f.addr, "PUT", PATH, ADMIN, "").await.0, 405);
    assert_eq!(calls(), (3, 1));
    let before = up.seen().len();
    assert_eq!(write(f.addr, "POST", PATH, ADMIN, "").await.0, 200);
    assert_eq!(calls(), (4, 2));
    let paths: Vec<String> = up.seen()[before..].iter().map(|r| r.path.clone()).collect();
    assert_eq!(&paths[..3], [INVALIDATE, SCAN_CLEAR, FACTS], "{paths:?}");
}
