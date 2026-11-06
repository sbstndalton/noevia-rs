//! Synthetic public oracle replay; no Node embedding executor or actual engine.
#![allow(clippy::unwrap_used, clippy::indexing_slicing)]
#[path = "mcp_support/mod.rs"]
mod support;
use serde_json::Value;
use support::*;
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn public_node_refusals_are_native_and_identical() {
    let d = tempfile::tempdir().unwrap();
    seed(d.path());
    let up = upstream().await;
    up.set(|r| {
        if r.path == "/v1/models" {
            reply("200 OK", r#"{"data":[{"id":"fixture-next"}]}"#)
        } else {
            reply("404 Not Found", r#"{"error":"unexpected Node execution"}"#)
        }
    });
    let f = front_env(
        d.path(),
        true,
        &[
            ("NOEVIA_LEGACY_UPSTREAM", up.base()),
            ("INFERENCE_BASE_URL", up.base()),
            ("DIARY_BASE_URL", up.base()),
        ],
    )
    .await;
    let oracle: Value = serde_json::from_str(include_str!(
        "../../../crates/embedding-worker/tests/fixtures/embedding-route-oracle.json"
    ))
    .unwrap();
    // Last two historical rows use mocked status in Node and are tested below
    // against actual native state rather than mistaken for differential evidence.
    for row in oracle["requests"].as_array().unwrap().iter().take(14) {
        let who = match row["role"].as_str() {
            Some("admin") => ADMIN,
            Some("member") => MEMBER,
            _ => "",
        };
        let method = row["method"].as_str().unwrap();
        let body = if row["input"].as_str() == Some("malformed") {
            "malformed".into()
        } else {
            row["input"].to_string()
        };
        let (status, body) = write(f.addr, method, "/api/embedding", who, &body).await;
        assert_eq!(
            status,
            u16::try_from(row["status"].as_u64().unwrap()).unwrap(),
            "{row}"
        );
        // The unit oracle invokes the route after Core's authentication gate.
        // Anonymous public requests stop at that gate's historical unauthorized body.
        let expected = if row["role"].is_null() {
            serde_json::json!({"error":"unauthorized"})
        } else {
            row["body"].clone()
        };
        assert_eq!(body, expected, "{row}");
    }
    assert!(
        up.seen().iter().all(|r| r.path == "/v1/models"),
        "public embedding request reached Node"
    );
    let (status, result) = write(
        f.addr,
        "POST",
        "/api/embedding",
        ADMIN,
        r#"{"action":"set-next","scope":"projects","target":{"model":"fixture-next"}}"#,
    )
    .await;
    assert_eq!(status, 200, "{result}");
    assert_eq!(result["projects"]["next"]["model"], "fixture-next");
    let saved: Value =
        serde_json::from_slice(&std::fs::read(d.path().join("embedding-config.json")).unwrap())
            .unwrap();
    assert_eq!(saved["version"], 1);
    assert_eq!(saved["next"]["model"], "fixture-next");
    let (status, result) = write(
        f.addr,
        "POST",
        "/api/embedding",
        ADMIN,
        r#"{"action":"switch","scope":"projects"}"#,
    )
    .await;
    assert_eq!(status, 200, "{result}");
    assert_eq!(result["projects"]["active"]["model"], "fixture-next");
    assert!(result["projects"]["next"].is_null());
    let (status, result) = write(
        f.addr,
        "POST",
        "/api/embedding",
        ADMIN,
        r#"{"action":"rollback","scope":"projects"}"#,
    )
    .await;
    assert_eq!(status, 200, "{result}");
    assert_eq!(result["projects"]["previous"]["model"], "fixture-next");
    assert!(up.seen().iter().all(|r| r.path == "/v1/models"));
    let (status, result) = get(f.addr, "/api/embedding", ADMIN).await;
    assert_eq!(status, 200, "{result}");
    assert_eq!(result["projects"]["active"]["model"], "default");
    assert_eq!(result["diary"]["status"], 404);
    assert!(!result.to_string().contains("unexpected Node execution"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn managed_service_uses_shared_destination_receipt_and_hides_protocol() {
    use serde_json::json;
    use std::sync::{Arc, Mutex};
    let d = tempfile::tempdir().unwrap();
    seed(d.path());
    let mut env = vec![];
    let authority = d.path().join("authority.sqlite");
    let up = upstream().await;
    let models = Arc::new(Mutex::new(
        json!([{"model":"fixture-old","file":"fixture-old.gguf"}]),
    ));
    let seen_fences = Arc::new(Mutex::new(Vec::<model_publication::Fence>::new()));
    let (m, fs, path) = (models.clone(), seen_fences.clone(), authority.clone());
    up.set(move |r|{
        if r.path == "/gate/v1/maintenance" && r.method == "POST" { return reply("201 Created",r#"{"hold":"0123456789abcdef0123456789abcdef","idle_ttl_ms":600000}"#); }
        if r.path.starts_with("/gate/") { return reply("204 No Content",""); }

        if r.path=="/api/v1/embedding-service"{
            if r.method=="PUT"{
                assert_eq!(r.header("x-model-loader-token").as_deref(),Some("fixture-loader-secret"));
                let fence:model_publication::Fence=serde_json::from_str(&r.header("x-noevia-maintenance-fence").unwrap()).unwrap();
                assert_eq!(fence.kind,"embedding-service");
                let guard=model_publication::Guard::at(&path).unwrap();
                guard.running(&fence).unwrap();
                *m.lock().unwrap()=serde_json::from_str::<Value>(&r.body).unwrap()["models"].clone();
                guard.finish(&fence,"settled",None).unwrap();guard.commit().unwrap();
                fs.lock().unwrap().push(fence);
            }
            let models=m.lock().unwrap().clone();
            let status=json!({"managed":true,"maintenanceProtocol":"embedding-service/1","enabled":!models.as_array().unwrap().is_empty(),"models":models,"previous":[],"running":{"loaded":[],"listed":[],"reachable":true}});
            return reply("200 OK",&status.to_string());
        }
        if r.path=="/api/internal/embedding"{return reply("200 OK",r#"{"active":{"model":"fixture-diary"},"next":null,"previous":null}"#);}
        reply("404 Not Found","{}")
    });
    env.extend([
        ("MODEL_MANAGER_KIND", "llamacpp".into()),
        ("MODEL_MANAGER_BASE_URL", up.base()),
        ("INFERENCE_GATE_ADMIN_TOKEN", "fixture-admin-secret".into()),
        (
            "NOEVIA_PRESET_AUTHORITY_FILE",
            authority.display().to_string(),
        ),
        ("MODEL_LOADER_URL", up.base()),
        ("MODEL_LOADER_TOKEN", "fixture-loader-secret".into()),
        ("DIARY_BASE_URL", up.base()),
    ]);
    let f = front_env(d.path(), true, &env).await;
    let (status, result) = write(
        f.addr,
        "POST",
        "/api/embedding",
        ADMIN,
        r#"{"action":"service-off"}"#,
    )
    .await;
    assert_eq!(status, 200, "{result}");
    assert_eq!(result["service"]["enabled"], false);
    assert!(result["service"].get("maintenanceProtocol").is_none());
    assert_eq!(seen_fences.lock().unwrap().len(), 1);
    assert!(!result.to_string().contains("fixture-loader-secret"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn credential_reflection_and_ssrf_cannot_escape_the_native_boundary() {
    let d = tempfile::tempdir().unwrap();
    seed(d.path());
    let up = upstream().await;
    up.set(|r| {
        assert_eq!(
            r.header("authorization").as_deref(),
            Some("bearer fixture-diary-secret")
        );
        reply(
            "401 Unauthorized",
            r#"{"error":"fixture-diary-secret","code":"fixture-diary-secret"}"#,
        )
    });
    let f = front_env(
        d.path(),
        true,
        &[
            ("DIARY_BASE_URL", up.base()),
            ("DIARY_AUTH_TOKEN", "fixture-diary-secret".into()),
        ],
    )
    .await;
    let (status, result) = get(f.addr, "/api/embedding", ADMIN).await;
    assert_eq!(status, 200);
    assert_eq!(result["diary"]["status"], 401);
    assert!(!result.to_string().contains("fixture-diary-secret"));
    let (status,result)=write(f.addr,"POST","/api/embedding",ADMIN,r#"{"action":"set-next","scope":"projects","target":{"baseUrl":"http://169.254.169.254","model":"fixture-next"}}"#).await;
    assert_eq!(status, 400);
    assert_eq!(result["code"], "publicEndpoint");
    assert_eq!(up.count("/api/internal/embedding"), 1);
}
