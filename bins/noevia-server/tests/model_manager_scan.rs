//! #2304: actual native identity/dispatch; cache and loader are independent synthetic upstreams.
#![allow(clippy::unwrap_used, clippy::indexing_slicing)]
#[path = "mcp_support/mod.rs"]
mod support;
use serde_json::json;
use support::*;
const PATH: &str = "/api/model-manager/models";
const CACHE: &str = "/internal/model-manager-scan/";
fn oracle(name: &str) -> serde_json::Value {
    let fixture: serde_json::Value =
        serde_json::from_str(include_str!("fixtures/model-manager-scan-node.json")).unwrap();
    fixture["rows"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["spec"]["name"] == name)
        .unwrap()
        .clone()
}
async fn rig(configured: bool) -> (tempfile::TempDir, Front, Upstream, Upstream) {
    let d = tempfile::tempdir().unwrap();
    seed(d.path());
    let cache = upstream().await;
    let loader = upstream().await;
    cache.set(|r| {
        reply(
            "200 OK",
            &if r.path.ends_with("/snapshot") {
                json!({"token":"a".repeat(64),"entryToken":"b".repeat(64),"now":10000,"hit":null})
            } else {
                json!({"applied":true})
            }
            .to_string(),
        )
    });
    loader.set(|_| reply("200 OK", r#"{"models":["synthetic"],"unregistered":[]}"#));
    let mut env = vec![
        ("NOEVIA_LEGACY_UPSTREAM", cache.base()),
        ("NOEVIA_PROJECT_STORE_TOKEN", STORE_TOKEN.into()),
    ];
    if configured {
        env.push(("MODEL_LOADER_URL", loader.base()));
        env.push(("MODEL_LOADER_TOKEN", "synthetic-loader-key".into()));
    }
    let f = front_env(d.path(), true, &env).await;
    (d, f, cache, loader)
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cold_native_read_fetches_fixed_loader_and_conditionally_publishes_raw_data() {
    let (_d, f, cache, loader) = rig(true).await;
    let (s, v) = get(f.addr, PATH, ADMIN).await;
    assert_eq!(s, 200, "{v}");
    assert_eq!(v, oracle("cold")["result"]["body"]);
    assert!(cache.seen().is_empty(), "Node never owns cache operations");
    assert_eq!(
        f.app
            .config
            .model_facts
            .scan_snapshot()
            .unwrap()
            .get("hit")
            .get("body")
            .get("models")
            .clone(),
        js_json::JValue::Arr(vec![js_json::JValue::Str("synthetic".into())])
    );
    let seen = loader.seen();
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].path, "/api/v1/models");
    assert_eq!(
        seen[0].header("x-model-loader-token").as_deref(),
        Some("synthetic-loader-key")
    );
    assert_eq!(seen[0].header("cookie"), None);
    assert_eq!(seen[0].header("x-noevia-client-ip"), None);
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn queried_reads_preserve_query_and_bypass_every_cache_operation() {
    let (_d, f, cache, loader) = rig(true).await;
    assert_eq!(
        get(f.addr, &format!("{PATH}?filter=a%2Fb&x=1"), ADMIN)
            .await
            .0,
        200
    );
    assert!(cache.seen().is_empty());
    assert_eq!(loader.seen()[0].path, "/api/v1/models?filter=a%2Fb&x=1");
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cached_read_uses_existing_entry_and_refreshes_only_when_older_than_five_seconds() {
    let (_d, f, cache, loader) = rig(true).await;
    assert_eq!(get(f.addr, PATH, ADMIN).await.0, 200);
    let (_, h, b) = raw_text(f.addr, "GET", PATH, ADMIN, "", b"").await;
    assert!(h.contains("x-model-scan: cached"));
    assert_eq!(j(&b), oracle("cold")["result"]["body"]);
    assert_eq!(loader.seen().len(), 1);
    tokio::time::sleep(std::time::Duration::from_millis(5100)).await;
    assert_eq!(get(f.addr, PATH, ADMIN).await.0, 200);
    loader.wait_for("/api/v1/models", 2).await;
    assert!(cache.seen().is_empty());
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn session_role_and_missing_configuration_precede_cache_and_loader() {
    let (_d, f, cache, loader) = rig(true).await;
    assert_eq!(get(f.addr, PATH, "").await.0, 401);
    assert_eq!(get(f.addr, PATH, MEMBER).await.0, 403);
    assert!(cache.seen().is_empty());
    assert!(loader.seen().is_empty());
    let (_d, f, cache, loader) = rig(false).await;
    assert_eq!(get(f.addr, PATH, ADMIN).await.0, 404);
    assert!(cache.seen().is_empty());
    assert!(loader.seen().is_empty());
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn only_exact_get_is_owned_head_other_methods_and_other_paths_stay_nodes() {
    let (_d, f, cache, loader) = rig(true).await;
    for (method, path) in [
        ("HEAD", PATH),
        ("POST", PATH),
        ("GET", "/api/model-manager/models/"),
        ("GET", "/api/model-manager/models/delete"),
        ("GET", "/api/model-manager"),
    ] {
        let _ = write(f.addr, method, path, ADMIN, "{bad").await;
    }
    assert_eq!(loader.seen().len(), 0);
    assert_eq!(cache.count(&format!("{CACHE}snapshot")), 0);
    assert!(noevia_server::FEATURES.contains(&"rust-model-manager-scan"));
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn upstream_errors_and_nonobject_json_have_the_original_public_shape() {
    let (_d, f, cache, loader) = rig(true).await;
    loader.set(|_| {
        reply(
            "409 Conflict",
            r#"{"detail":"loader-refusal","private":"not-a-public-error-field"}"#,
        )
    });
    let (s, v) = get(f.addr, PATH, ADMIN).await;
    assert_eq!(s, 409);
    assert_eq!(v, oracle("loader refusal")["result"]["body"]);
    loader.set(|_| reply("200 OK", "not-json"));
    assert_eq!(
        get(f.addr, PATH, ADMIN).await.1,
        json!({"error":"not-json"})
    );
    assert!(cache.seen().is_empty());
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn restarted_owner_publication_refusal_is_not_retried_or_allowed_to_retarget() {
    let (_d, f, cache, _loader) = rig(true).await;
    cache.set(|r| {
        reply(
            "200 OK",
            &if r.path.ends_with("/snapshot") {
                json!({"token":"a".repeat(64),"entryToken":"b".repeat(64),"now":10000,"hit":null})
            } else {
                json!({"applied":false})
            }
            .to_string(),
        )
    });
    assert_eq!(get(f.addr, PATH, ADMIN).await.0, 200);
    assert!(cache.seen().is_empty());
    let snap = f.app.config.model_facts.scan_snapshot().unwrap();
    assert!(!matches!(snap.get("hit"), js_json::JValue::Null));
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn oversized_success_fails_closed_without_caching_a_false_success() {
    let (_d, f, cache, loader) = rig(true).await;
    loader.set(|_| reply("200 OK", &format!("\"{}\"", "x".repeat(8 * 1024 * 1024))));
    assert_eq!(get(f.addr, PATH, ADMIN).await.0, 502);
    assert_eq!(cache.count(&format!("{CACHE}publish")), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn lone_surrogate_data_survives_public_response_and_opaque_cache_publication() {
    let (_d, f, cache, loader) = rig(true).await;
    loader.set(|_| reply("200 OK", r#"{"models":["m\ud800"]}"#));
    let (s, _, body) = raw_text(f.addr, "GET", PATH, ADMIN, "", b"").await;
    assert_eq!(s, 200);
    assert!(body.contains(r#"m\ud800"#));
    assert!(cache.seen().is_empty());
    let snap = f.app.config.model_facts.scan_snapshot().unwrap();
    assert!(js_json::wtf16::stringify(snap.get("hit"))
        .unwrap()
        .contains(r"m\ud800"));
}

#[test]
fn conditional_cache_operations_port_the_core_oracle() {
    use js_json::{wtf16, JValue};
    use noevia_server::model_facts::Authority;
    let a = Authority::from_lookup(&|_| None);
    let b = Authority::from_lookup(&|_| None);
    let first = a.scan_snapshot().unwrap();
    let token = first.get("token").as_str().unwrap();
    assert!(!b.scan_publish(token, JValue::obj(Vec::<(String, JValue)>::new())));
    let body = wtf16::parse(r#"{"models":["m\ud800"]}"#).unwrap();
    assert!(a.scan_publish(token, body.clone()));
    let old = a.scan_snapshot().unwrap();
    assert!(a.scan_publish(token, JValue::obj([("models", JValue::Arr(vec![]))])));
    assert!(a
        .scan_refresh_token(old.get("entryToken").as_str().unwrap())
        .is_none());
    a.scan_invalidate().unwrap();
    assert!(!a.scan_publish(token, body));
    assert!(matches!(
        a.scan_snapshot().unwrap().get("hit"),
        JValue::Null
    ));
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn late_scan_after_invalidation_cannot_republish() {
    let (_d, f, cache, loader) = rig(true).await;
    // Hold the synthetic response until invalidation is complete; no timing assumption.
    let released = std::sync::Arc::new((std::sync::Mutex::new(false), std::sync::Condvar::new()));
    let waiting = std::sync::Arc::clone(&released);
    loader.set(move |_| {
        let (lock, signal) = &*waiting;
        let guard = lock.lock().unwrap();
        let (_guard, timeout) = signal
            .wait_timeout_while(guard, std::time::Duration::from_secs(10), |done| !*done)
            .unwrap();
        assert!(!timeout.timed_out(), "test must release the scan");
        reply("200 OK", r#"{"models":["obsolete"]}"#)
    });
    let address = f.addr;
    let task = tokio::spawn(async move { get(address, PATH, ADMIN).await });
    loader.wait_for("/api/v1/models", 1).await;
    f.app.config.model_facts.scan_invalidate().unwrap();
    let (lock, signal) = &*released;
    *lock.lock().unwrap() = true;
    signal.notify_all();
    assert_eq!(task.await.unwrap().0, 200);
    assert!(matches!(
        f.app.config.model_facts.scan_snapshot().unwrap().get("hit"),
        js_json::JValue::Null
    ));
    assert!(cache.seen().is_empty());
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn raw_traversal_and_separators_are_rejected_at_front_boundary() {
    let (_d, f, node, loader) = rig(true).await;
    for tail in [
        "sections/a%2fb",
        "sections/a%5Cb",
        "sections/%2e%2e",
        "sections/../models",
        "sections/%",
        "sections/%ff",
    ] {
        let path = format!("/api/model-manager/{tail}");
        let (status, v) = get(f.addr, &path, ADMIN).await;
        assert_eq!(
            (status, v),
            (400, json!({"error":"Invalid path"})),
            "{path}"
        );
    }
    assert!(node.seen().is_empty());
    assert!(loader.seen().is_empty());
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn native_installed_facts_and_public_scan_share_the_same_cache() {
    let (_d, f, node, loader) = rig(true).await;
    loader.set(|_| {
        reply(
            "200 OK",
            r#"{"models":[{"modelId":"synthetic","sections":["synthetic"],"bytes":42}]}"#,
        )
    });
    let before = f.app.config.model_facts.scan_snapshot().unwrap();
    let token = before.get("token").as_str().unwrap();
    let body = js_json::wtf16::parse(
        r#"{"models":[{"modelId":"synthetic","sections":["synthetic"],"bytes":42}]}"#,
    )
    .unwrap();
    assert!(f.app.config.model_facts.scan_publish(token, body));
    node.set(|_| reply("200 OK", r#"{"env":{},"revision":"synthetic-config"}"#));
    let facts = noevia_server::model_facts::installed(&f.app, &[], true, None)
        .await
        .unwrap();
    assert!(matches!(facts.get("scan"), js_json::JValue::Arr(_)));
    assert_eq!(get(f.addr, PATH, ADMIN).await.0, 200);
    assert!(loader.seen().is_empty());
}

#[test]
fn budget_mutation_uses_the_same_generation_fence() {
    let d = tempfile::tempdir().unwrap();
    let _db = seed(d.path());
    let authority = noevia_server::model_facts::Authority::from_lookup(&|key| match key {
        "UI_DATA_DIR" => Some(d.path().display().to_string()),
        "NOEVIA_START_ID" => Some("synthetic-start".into()),
        _ => None,
    });
    authority.publish().unwrap();
    let before = authority.scan_snapshot().unwrap();
    let token = before.get("token").as_str().unwrap();
    let body = js_json::wtf16::parse(r#"{"models":["obsolete"]}"#).unwrap();
    assert!(authority.scan_publish(token, body.clone()));
    assert!(authority.save(&json!({"budgetGib": 2})).is_ok());
    assert!(matches!(
        authority.scan_snapshot().unwrap().get("hit"),
        js_json::JValue::Null
    ));
    assert!(!authority.scan_publish(token, body));
}

#[test]
fn boot_and_scan_operations_never_create_the_application_database() {
    let d = tempfile::tempdir().unwrap();
    let authority = noevia_server::model_facts::Authority::from_lookup(&|key| match key {
        "UI_DATA_DIR" => Some(d.path().display().to_string()),
        "NOEVIA_START_ID" => Some("synthetic-start".into()),
        _ => None,
    });
    let db = d.path().join("cowork.db");
    assert!(!db.exists(), "reading a boot budget must not create SQLite");
    authority.publish().unwrap();
    let snapshot = authority.scan_snapshot().unwrap();
    let body = js_json::wtf16::parse(r#"{"models":[]}"#).unwrap();
    assert!(authority.scan_publish(snapshot.get("token").as_str().unwrap(), body));
    authority.scan_invalidate().unwrap();
    assert!(authority.scan_snapshot().is_some());
    assert!(!db.exists(), "scan paths are purely in memory");
    assert_eq!(authority.save(&json!({"budgetGib": 2})).unwrap_err().0, 503);
    assert!(
        !db.exists(),
        "an early budget write requires an existing store"
    );
}
