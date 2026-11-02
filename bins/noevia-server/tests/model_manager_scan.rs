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
    assert_eq!(cache.count(&format!("{CACHE}snapshot")), 1);
    let seen = cache.seen();
    let publish = seen.iter().find(|r| r.path.ends_with("/publish")).unwrap();
    let ask = j(&publish.body);
    assert_eq!(ask["token"], "a".repeat(64));
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(ask["bodyJson"].as_str().unwrap()).unwrap(),
        v
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
    for (at, wanted) in [(5000, 0), (4999, 1)] {
        cache.set(move|r|reply("200 OK",&if r.path.ends_with("/snapshot"){json!({"token":"a".repeat(64),"entryToken":"b".repeat(64),"now":10000,"hit":{"at":at,"body":{"models":["cached"]}}})}else{json!({"applied":false})}.to_string()));
        let (s, h, b) = raw_text(f.addr, "GET", PATH, ADMIN, "", b"").await;
        assert_eq!(s, 200);
        assert!(h.contains("x-model-scan: cached"));
        assert!(h.contains("cache-control: no-store"));
        assert_eq!(j(&b)["models"], json!(["cached"]));
        if wanted > 0 {
            cache.wait_for(&format!("{CACHE}refresh"), wanted).await;
        } else {
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
        assert_eq!(cache.count(&format!("{CACHE}refresh")), wanted);
    }
    assert!(loader.seen().is_empty());
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
    assert_eq!(cache.count(&format!("{CACHE}publish")), 1);
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
    assert_eq!(cache.count(&format!("{CACHE}snapshot")), 1);
    assert_eq!(cache.count(&format!("{CACHE}publish")), 1);
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
    let seen = cache.seen();
    let publish = seen.iter().find(|r| r.path.ends_with("/publish")).unwrap();
    assert!(j(&publish.body)["bodyJson"]
        .as_str()
        .unwrap()
        .contains(r#"m\ud800"#));
}
