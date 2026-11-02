//! #2292: actual identity/dispatch and synthetic actor only; no engine/model work.
#![allow(clippy::unwrap_used, clippy::indexing_slicing, clippy::panic)]
#[path = "mcp_support/mod.rs"]
mod support;
use serde_json::{json, Value};
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};
use support::*;
const PATH: &str = "/api/models/calibration/cancel";
const FACTS: &str = "/internal/calibration-cancel/facts";
const ABORT: &str = "/internal/calibration-cancel/abort";
fn job() -> Value {
    let cases: Value =
        serde_json::from_str(include_str!("fixtures/calibration-cancel-node.json")).unwrap();
    cases[1]["body"].clone()
}
fn facts() -> Value {
    json!({"available":true,"token":"a".repeat(64),"generation":"b".repeat(64),"job":job()})
}
async fn rig() -> (tempfile::TempDir, Front, Upstream) {
    let d = tempfile::tempdir().unwrap();
    seed(d.path());
    let up = upstream().await;
    up.set(|r| {
        if r.path == FACTS {
            reply("200 OK", &facts().to_string())
        } else if r.path == ABORT {
            reply(
                "200 OK",
                &json!({"applied":true,"generation":"b".repeat(64),"job":job()}).to_string(),
            )
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
async fn native_public_response_and_the_only_abort_selector_is_the_checked_opaque_token() {
    let (_d, f, up) = rig().await;
    // This path has never accepted a model selector; even malformed JSON is ignored.
    let (s, v) = write(
        f.addr,
        "POST",
        &format!("{PATH}?model=other"),
        ADMIN,
        "{malformed",
    )
    .await;
    assert_eq!(s, 202, "{v}");
    assert_eq!(v, job());
    let seen = up.seen();
    assert_eq!(seen.len(), 2);
    assert_eq!(seen[0].path, FACTS);
    assert_eq!(j(&seen[0].body), json!({}));
    assert_eq!(seen[1].path, ABORT);
    assert_eq!(j(&seen[1].body), json!({"token":"a".repeat(64)}));
    for r in seen {
        assert_eq!(
            r.header("x-noevia-store-token").as_deref(),
            Some(STORE_TOKEN)
        );
        assert_eq!(r.header("cookie"), None);
        assert_eq!(r.header("x-noevia-client-ip"), None);
    }
    assert_eq!(
        write(f.addr, "POST", PATH, ADMIN, r#"{"model":"\ud800"}"#)
            .await
            .0,
        202
    );
    assert_eq!(
        write(f.addr, "POST", PATH, ADMIN, r#"{"model":"nonexistent"}"#)
            .await
            .0,
        202
    );
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn auth_account_role_and_csrf_precede_every_actor_call() {
    let (_d, f, up) = rig().await;
    assert_eq!(get(f.addr, PATH, "").await.0, 401);
    assert_eq!(write(f.addr, "POST", PATH, MEMBER, "").await.0, 403);
    assert_eq!(get(f.addr, PATH, MEMBER).await.0, 403);
    assert_eq!(raw(f.addr, "POST", PATH, ADMIN, "", b"").await.0, 403);
    assert!(up.seen().is_empty());
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn availability_and_method_precedence_then_no_running_job_policy() {
    let (_d, f, up) = rig().await;
    for m in ["GET", "HEAD", "PUT", "DELETE", "OPTIONS"] {
        assert_eq!(write(f.addr, m, PATH, ADMIN, "").await.0, 405);
    }
    assert_eq!(up.count(ABORT), 0);
    up.set(|_| reply("200 OK", r#"{"available":false}"#));
    assert_eq!(get(f.addr, PATH, ADMIN).await.0, 404);
    assert_eq!(write(f.addr, "POST", PATH, ADMIN, "").await.0, 404);
    for status in [
        Value::Null,
        json!("passed"),
        json!("cancelled"),
        json!("interrupted"),
        json!("failed"),
    ] {
        up.set(move |_| {
            let mut v = facts();
            if status.is_null() {
                v["job"] = Value::Null;
            } else {
                v["job"]["status"] = status.clone();
            }
            reply("200 OK", &v.to_string())
        });
        let (s, v) = write(f.addr, "POST", PATH, ADMIN, "").await;
        assert_eq!(s, 409);
        assert_eq!(v, json!({"error":"No calibration is running."}));
    }
    assert_eq!(up.count(ABORT), 0);
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_replaced_or_completed_job_is_rechecked_without_cancelling_it_from_the_old_snapshot() {
    let (_d, f, up) = rig().await;
    let n = Arc::new(AtomicUsize::new(0));
    let count = Arc::clone(&n);
    up.set(move |r| {
        if r.path == ABORT {
            count.fetch_add(1, Ordering::SeqCst);
            return reply("409 Changed", r#"{"applied":false}"#);
        }
        let mut v = facts();
        if count.load(Ordering::SeqCst) > 0 {
            v["job"]["status"] = json!("passed");
        }
        reply("200 OK", &v.to_string())
    });
    assert_eq!(write(f.addr, "POST", PATH, ADMIN, "").await.0, 409);
    assert_eq!(n.load(Ordering::SeqCst), 1);
    assert_eq!(up.count(FACTS), 2);
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_new_running_job_is_not_retargeted_by_a_stale_cancellation_request() {
    let (_d, f, up) = rig().await;
    let count = Arc::new(AtomicUsize::new(0));
    let n = Arc::clone(&count);
    up.set(move |r| {
        if r.path == ABORT {
            n.fetch_add(1, Ordering::SeqCst);
            return reply("409 Changed", r#"{"applied":false}"#);
        }
        let mut v = facts();
        if n.load(Ordering::SeqCst) > 0 {
            v["generation"] = json!("c".repeat(64));
            v["job"]["id"] = json!("replacement");
        }
        reply("200 OK", &v.to_string())
    });
    let (s, v) = write(f.addr, "POST", PATH, ADMIN, "").await;
    assert_eq!(s, 409, "{v}");
    assert_eq!(count.load(Ordering::SeqCst), 1);
    assert!(v["error"].as_str().unwrap().contains("changed"));
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn conflict_retries_are_bounded_and_repeat_cancellation_while_running_is_accepted() {
    let (_d, f, up) = rig().await;
    for _ in 0..2 {
        assert_eq!(write(f.addr, "POST", PATH, ADMIN, "").await.0, 202);
    }
    let start = up.count(ABORT);
    up.set(|r| {
        if r.path == ABORT {
            reply("409 Changed", r#"{"applied":false}"#)
        } else {
            reply("200 OK", &facts().to_string())
        }
    });
    assert_eq!(write(f.addr, "POST", PATH, ADMIN, "").await.0, 503);
    assert_eq!(up.count(ABORT) - start, 3);
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn malformed_or_dead_actor_state_fails_closed_without_a_false_success() {
    let (_d, f, up) = rig().await;
    for bad in [
        json!({}),
        json!({"available":true,"job":7}),
        json!({"available":true,"job":{"status":"running"}}),
        json!({"available":true,"token":"bad","job":job()}),
    ] {
        up.set(move |_| reply("200 OK", &bad.to_string()));
        assert_eq!(write(f.addr, "POST", PATH, ADMIN, "").await.0, 503);
    }
    assert_eq!(up.count(ABORT), 0);
    up.set(|r| {
        if r.path == FACTS {
            reply("200 OK", &facts().to_string())
        } else {
            reply("503 Dead", "{}")
        }
    });
    assert_eq!(write(f.addr, "POST", PATH, ADMIN, "").await.0, 503);
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn selected_response_omits_private_actor_fields_and_round_trips_wtf16_without_model_aliasing()
{
    let (_d, f, up) = rig().await;
    up.set(|r|if r.path==FACTS{reply("200 OK",&facts().to_string())}else{reply("200 OK",r#"{"applied":true,"generation":"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb","job":{"id":"j","model":"m\ud800","status":"running","steps":[],"originalText":"never-public","unknown":"never-public"}}"#)});
    let (s, headers, body) = raw_text(
        f.addr,
        "POST",
        PATH,
        ADMIN,
        &format!("Origin: http://{}\r\nX-CSRF-Token: csrf\r\n", f.addr),
        b"",
    )
    .await;
    assert!(headers.contains("cache-control: no-store"));
    assert_eq!(s, 202, "{body}");
    assert!(body.contains(r#"m\ud800"#));
    assert!(!body.contains("never-public"));
    assert!(noevia_server::FEATURES.contains(&"rust-calibration-cancel"));
}
