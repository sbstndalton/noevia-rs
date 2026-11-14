//! Real front authentication/dispatch with a synthetic actor only; no engine or model calls.
#![allow(clippy::unwrap_used, clippy::indexing_slicing, clippy::panic)]
#[path = "mcp_support/mod.rs"]
mod support;
use serde_json::{json, Value};
use support::*;
const PATH: &str = "/api/models/autotune/resume";
const FACTS: &str = "/internal/autotune-resume/facts";
const PREPARE: &str = "/internal/autotune-resume/prepare";
const COMMIT: &str = "/internal/autotune-resume/commit";
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
    json!({"available":true,"token":"a".repeat(64),"generation":"b".repeat(64),"starting":false,"revision":"r1","job":{"status":"cancelled","model":"synthetic","unsafe":false,"revision":"r1","pendingRevision":null,"models":[{"model":"synthetic","base":null,"mode":"fast","status":"interrupted","sectionCreated":false,"identitySet":false,"identity":null,"planned":true,"configured":true,"baseConfigured":false,"phases":[{"id":"context","status":"interrupted","restored":true,"steps":[{"status":"passed"},{"status":"pending"}]}]}]}})
}
fn public_job() -> Value {
    json!({"id":"j","model":"synthetic","status":"running","phase":"Waiting for chat to become idle","waiting":true,"models":[{"model":"synthetic","status":"pending","phases":[{"id":"context","status":"pending","steps":[{"status":"passed"}]}]}]})
}
fn stage(request: &Seen) -> (std::time::Duration, String) {
    let ask = j(&request.body);
    let mut f = facts();
    f["applied"] = json!(true);
    if request.path == FACTS {
        return reply("200 OK", &f.to_string());
    }
    if request.path == PREPARE && ask["stage"] == "listing" {
        f["token"] = json!("c".repeat(64));
        f["models"] = json!([{"index":0,"system":false,"modelListed":true,"baseListed":false}]);
        return reply("200 OK", &f.to_string());
    }
    if request.path == PREPARE && ask["stage"] == "identities" {
        f["token"] = json!("d".repeat(64));
        f["identities"] = json!([{"index":0,"field":"model","identity":"e".repeat(64)}]);
        return reply("200 OK", &f.to_string());
    }
    if request.path == COMMIT {
        return reply(
            "200 OK",
            &json!({"applied":true,"generation":"f".repeat(64),"publicJob":public_job()})
                .to_string(),
        );
    }
    reply("500 WrongPath", "{}")
}
async fn rig() -> (tempfile::TempDir, Front, Upstream) {
    let d = tempfile::tempdir().unwrap();
    seed(d.path());
    let up = upstream().await;
    up.set(stage);
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
async fn native_resume_owns_confirmation_queue_identity_reset_and_response() {
    let (_d, f, up) = rig().await;
    let (s, h, b) = raw_text(
        f.addr,
        "POST",
        &format!("{PATH}?model=other"),
        ADMIN,
        &format!("Origin: http://{}\r\nX-CSRF-Token: csrf\r\n", f.addr),
        br#"{"confirmPause":true,"model":"ignored"}"#,
    )
    .await;
    assert_eq!(s, 202);
    assert_eq!(j(&b), public_job());
    assert!(h.contains("cache-control: no-store"));
    let seen = actor(&up);
    assert_eq!(seen.len(), 4);
    assert_eq!(seen[0].path, FACTS);
    assert_eq!(j(&seen[0].body), json!({}));
    assert_eq!(
        j(&seen[1].body),
        json!({"token":"a".repeat(64),"stage":"listing"})
    );
    assert_eq!(
        j(&seen[2].body),
        json!({"token":"c".repeat(64),"stage":"identities","selectors":[{"index":0,"field":"model"}]})
    );
    assert_eq!(
        j(&seen[3].body),
        json!({"token":"d".repeat(64),"plan":{"kind":"launch","reset":[{"index":0,"phases":[{"index":0,"keepSteps":[0]}]}]}})
    );
    for r in seen {
        assert_eq!(
            r.header("x-noevia-store-token").as_deref(),
            Some(STORE_TOKEN)
        );
        assert_eq!(r.header("cookie"), None);
        assert_eq!(r.header("x-noevia-client-ip"), None);
        assert_eq!(r.header("x-forwarded-for"), None);
    }
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn auth_csrf_method_availability_and_confirmation_precede_preparation() {
    let (_d, f, up) = rig().await;
    assert_eq!(get(f.addr, PATH, "").await.0, 401);
    assert_eq!(write(f.addr, "POST", PATH, MEMBER, "{}").await.0, 403);
    assert_eq!(get(f.addr, PATH, MEMBER).await.0, 403);
    assert_eq!(raw(f.addr, "POST", PATH, ADMIN, "", b"{}").await.0, 403);
    assert!(actor(&up).is_empty());
    assert_eq!(get(f.addr, PATH, ADMIN).await.0, 405);
    assert_eq!(write(f.addr, "POST", PATH, ADMIN, "{bad").await.0, 400);
    assert_eq!(
        write(f.addr, "POST", PATH, ADMIN, "{}").await,
        (
            400,
            json!({"error":"Confirm that chat can pause before resuming auto-tune."})
        )
    );
    assert_eq!(up.count(PREPARE), 0);
    up.set(|_| reply("200 OK", r#"{"available":false}"#));
    assert_eq!(get(f.addr, PATH, ADMIN).await.0, 404);
    assert_eq!(write(f.addr, "POST", PATH, ADMIN, "{}").await.0, 404);
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stale_each_stage_or_replaced_run_never_retargets_or_retries() {
    let (_d, f, up) = rig().await;
    for target in ["listing", "identities", "commit"] {
        up.set(move |r| {
            let a = j(&r.body);
            if r.path == PREPARE && a["stage"] == target || r.path == COMMIT && target == "commit" {
                reply("409 Changed", r#"{"applied":false}"#)
            } else {
                stage(r)
            }
        });
        assert_eq!(
            write(f.addr, "POST", PATH, ADMIN, r#"{"confirmPause":true}"#).await,
            (
                409,
                json!({"error":"The auto-tune changed before resuming. Try again."})
            )
        );
    }
    let before = up.count(COMMIT);
    up.set(|r| {
        if r.path == PREPARE {
            let mut f = facts();
            f["applied"] = json!(true);
            f["generation"] = json!("9".repeat(64));
            reply("200 OK", &f.to_string())
        } else {
            stage(r)
        }
    });
    assert_eq!(
        write(f.addr, "POST", PATH, ADMIN, r#"{"confirmPause":true}"#)
            .await
            .0,
        409
    );
    assert_eq!(up.count(COMMIT), before);
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn protected_queue_finishes_without_identity_preparation_or_worker_launch() {
    let (_d, f, up) = rig().await;
    up.set(|r|{let a=j(&r.body);if r.path==PREPARE{assert_eq!(a["stage"],"listing");let mut f=facts();f["applied"]=json!(true);f["token"]=json!("c".repeat(64));f["models"]=json!([{"index":0,"system":true,"modelListed":true,"baseListed":false}]);reply("200 OK",&f.to_string())}else if r.path==COMMIT{assert_eq!(a["plan"],json!({"kind":"filter","complete":true,"keep":[],"skipped":[{"index":0,"reason":"System routing model — not tuned"}],"model":null}));let mut f=facts();f["applied"]=json!(true);let mut job=public_job();job["status"]=json!("passed");job["models"]=json!([]);f["publicJob"]=job;reply("200 OK",&f.to_string())}else{stage(r)}});
    let (s, v) = write(f.addr, "POST", PATH, ADMIN, r#"{"confirmPause":true}"#).await;
    assert_eq!(s, 200);
    assert_eq!(v["status"], "passed");
    assert_eq!(up.count(PREPARE), 1);
    assert_eq!(up.count(COMMIT), 1);
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reload_or_identity_failure_and_malformed_bridge_never_claim_launch_success() {
    let (_d, f, up) = rig().await;
    for failure in ["reload", "identity", "bad-token", "dead"] {
        up.set(move |r| {
            if failure == "dead" {
                return reply("503 Down", "{}");
            }
            if failure == "bad-token" && r.path == FACTS {
                let mut f = facts();
                f["token"] = json!("bad");
                return reply("200 OK", &f.to_string());
            }
            let a = j(&r.body);
            if r.path == PREPARE && a["stage"] == "identities" {
                if failure == "reload" {
                    return reply("409 Pending", r#"{"applied":false,"fault":"reload"}"#);
                }
                if failure == "identity" {
                    let mut f = facts();
                    f["applied"] = json!(true);
                    f["identities"] = json!([{"index":0,"field":"model","identity":null}]);
                    return reply("200 OK", &f.to_string());
                }
            }
            stage(r)
        });
        assert_eq!(
            write(f.addr, "POST", PATH, ADMIN, r#"{"confirmPause":true}"#)
                .await
                .0,
            if matches!(failure, "reload" | "identity") {
                409
            } else {
                503
            }
        );
    }
    assert_eq!(up.count(COMMIT), 0);
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn public_projection_is_recursive_and_preserves_lone_utf16_without_model_selectors() {
    let (_d, f, up) = rig().await;
    up.set(|r| {
        if r.path == COMMIT {
            let mut job = public_job();
            job["unknown"] = json!("never-public");
            job["models"][0]["originalText"] = json!("never-public");
            job["models"][0]["_profile"] = json!("never-public");
            reply(
                "200 OK",
                &json!({"applied":true,"publicJob":job})
                    .to_string()
                    .replace("\"synthetic\"", "\"synthetic\\ud800\""),
            )
        } else {
            stage(r)
        }
    });
    let (s, _, body) = raw_text(
        f.addr,
        "POST",
        PATH,
        ADMIN,
        &format!("Origin: http://{}\r\nX-CSRF-Token: csrf\r\n", f.addr),
        br#"{"confirmPause":true}"#,
    )
    .await;
    assert_eq!(s, 202);
    assert!(!body.contains("never-public"));
    let body = js_json::wtf16::parse(&body).unwrap();
    assert_eq!(js_json::wtf16::len(body.get("model").as_str().unwrap()), 10);
    for r in up.seen() {
        assert!(!r.body.contains("synthetic"));
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unexpected_http_credentials_in_public_job_fail_closed() {
    let (_d, f, up) = rig().await;
    up.set(|r| {
        if r.path == COMMIT {
            let mut job = public_job();
            job["models"][0]["headers"] = json!({"Authorization":"never-public"});
            reply(
                "200 OK",
                &json!({"applied":true,"publicJob":job}).to_string(),
            )
        } else {
            stage(r)
        }
    });
    let (s, body) = write(f.addr, "POST", PATH, ADMIN, r#"{"confirmPause":true}"#).await;
    assert_eq!(s, 503);
    assert!(!body.to_string().contains("never-public"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unknown_public_job_status_is_not_serialized_as_a_valid_resume_result() {
    let (_d, f, up) = rig().await;
    up.set(|r| {
        if r.path == COMMIT {
            let mut job = public_job();
            job["status"] = json!("never-public");
            reply(
                "200 OK",
                &json!({"applied":true,"publicJob":job}).to_string(),
            )
        } else {
            stage(r)
        }
    });
    let (s, body) = write(f.addr, "POST", PATH, ADMIN, r#"{"confirmPause":true}"#).await;
    assert_eq!(s, 503);
    assert!(!body.to_string().contains("never-public"));
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
    assert_eq!(
        write(f.addr, "POST", PATH, ADMIN, r#"{"confirmPause":true}"#)
            .await
            .0,
        202
    );
    assert_eq!(calls(), (4, 2));
    let paths: Vec<String> = up.seen()[before..].iter().map(|r| r.path.clone()).collect();
    assert_eq!(&paths[..3], [INVALIDATE, SCAN_CLEAR, FACTS], "{paths:?}");
}
