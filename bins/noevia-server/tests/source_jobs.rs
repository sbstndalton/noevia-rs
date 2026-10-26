//! Synthetic internal source-store contracts; no public route or production files.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use http_body_util::BodyExt;
use noevia_server::project_store::{router, Store, TOKEN_HEADER};
use serde_json::{json, Value};
use tower::ServiceExt;

#[test]
fn compiled_server_advertises_live_source_store() {
    assert!(
        noevia_server::FEATURES.contains(&server_projects::source_jobs::SOURCE_JOBS_STORE_MARKER)
    );
}

const USER: &str = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa";
const OTHER: &str = "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb";
const START: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const TOKEN: &str = "synthetic-source-store-token-0000000";
const OWNER1: &str = "cccccccc-cccc-4ccc-8ccc-cccccccccccc";
const OWNER2: &str = "dddddddd-dddd-4ddd-8ddd-dddddddddddd";
const REQ1: &str = "11111111-1111-4111-8111-111111111111";
const REQ2: &str = "22222222-2222-4222-8222-222222222222";
const REQ3: &str = "33333333-3333-4333-8333-333333333333";

fn setup() -> (tempfile::TempDir, axum::Router) {
    let root = tempfile::tempdir().unwrap();
    for user in [USER, OTHER] {
        let dir = root.path().join("users").join(user);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("projects.json"),
            r#"{"projects":[{"id":"a"},{"id":"b"}]}"#,
        )
        .unwrap();
    }
    let app = router(Store {
        data_dir: root.path().into(),
        token: TOKEN.into(),
        switch: server_store::RustProjects::owned(),
        start_id: START.into(),
    });
    (root, app)
}
async fn call(
    app: &axum::Router,
    op: &str,
    token: Option<&str>,
    body: Value,
) -> (StatusCode, Value) {
    let mut req = Request::builder()
        .method("POST")
        .uri(format!("/v1/source-jobs/{op}"))
        .header("content-type", "application/json");
    if let Some(token) = token {
        req = req.header(TOKEN_HEADER, token);
    }
    let response = app
        .clone()
        .oneshot(req.body(Body::from(body.to_string())).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (status, serde_json::from_slice(&bytes).unwrap())
}
fn owner(born: &str, owner: &str) -> Value {
    json!({"user":USER,"start":START,"born":born,"owner":owner})
}
#[tokio::test]
async fn source_route_requires_secret_fences_owner_and_scopes_reads() {
    let (root, app) = setup();
    let first = json!({"user":USER,"start":START,"born":"100","owner":OWNER1,"projectId":"a","request":REQ1});
    assert_eq!(
        call(&app, "start", None, first.clone()).await.0,
        StatusCode::UNAUTHORIZED
    );
    assert!(!root.path().join("users").join(USER).join("jobs").exists());
    assert_eq!(
        call(&app, "recover", Some(TOKEN), owner("100", OWNER1))
            .await
            .0,
        StatusCode::OK
    );
    let (status, job) = call(&app, "start", Some(TOKEN), first.clone()).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        call(&app, "start", Some(TOKEN), first).await.1,
        job,
        "exact retry"
    );
    let id = job["id"].as_str().unwrap();
    let run = job["run"].as_str().unwrap();
    assert_eq!(
        call(
            &app,
            "read",
            Some(TOKEN),
            json!({"user":OTHER,"projectId":"a","id":id})
        )
        .await
        .0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        call(
            &app,
            "read",
            Some(TOKEN),
            json!({"user":USER,"projectId":"b","id":id})
        )
        .await
        .0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        call(
            &app,
            "read",
            Some(TOKEN),
            json!({"user":USER,"projectId":"a","id":id})
        )
        .await
        .1["status"],
        "running"
    );
    let progress = json!({"user":USER,"start":START,"born":"100","owner":OWNER1,"id":id,"run":run,"stage":"indexing","n":1});
    assert_eq!(
        call(&app, "progress", Some(TOKEN), progress.clone())
            .await
            .0,
        StatusCode::OK
    );
    assert_eq!(
        call(&app, "progress", Some(TOKEN), progress).await.0,
        StatusCode::OK
    );
    let finish = json!({"user":USER,"start":START,"born":"100","owner":OWNER1,"id":id,"run":run,"ok":true,"result":{"status":200,"body":{"name":"a.pdf"}}});
    assert_eq!(
        call(&app, "finish", Some(TOKEN), finish.clone()).await.0,
        StatusCode::OK
    );
    assert_eq!(
        call(&app, "finish", Some(TOKEN), finish).await.0,
        StatusCode::OK
    );
    assert_eq!(
        call(
            &app,
            "read",
            Some(TOKEN),
            json!({"user":USER,"projectId":"a","id":id})
        )
        .await
        .1["result"]["body"]["name"],
        "a.pdf"
    );
    std::fs::remove_dir_all(root.path().join("users").join(USER)).unwrap();
    assert_eq!(
        call(&app, "recover", Some(TOKEN), owner("200", OWNER2))
            .await
            .0,
        StatusCode::GONE
    );
    assert!(!root.path().join("users").join(USER).exists());
}
#[tokio::test]
async fn two_active_cap_and_newer_owner_interrupt_only_source_jobs() {
    let (_root, app) = setup();
    assert_eq!(
        call(&app, "recover", Some(TOKEN), owner("100", OWNER1))
            .await
            .0,
        StatusCode::OK
    );
    let make = |request| json!({"user":USER,"start":START,"born":"100","owner":OWNER1,"projectId":"a","request":request});
    let (_, first) = call(&app, "start", Some(TOKEN), make(REQ1)).await;
    assert_eq!(
        call(&app, "start", Some(TOKEN), make(REQ2)).await.0,
        StatusCode::OK
    );
    assert_eq!(
        call(&app, "start", Some(TOKEN), make(REQ3)).await.0,
        StatusCode::TOO_MANY_REQUESTS
    );
    let abandoned = json!({"user":USER,"start":START,"born":"100","owner":OWNER1,"request":REQ1});
    assert_eq!(
        call(&app, "abandon", Some(TOKEN), abandoned.clone())
            .await
            .0,
        StatusCode::OK
    );
    assert_eq!(
        call(&app, "abandon", Some(TOKEN), abandoned).await.0,
        StatusCode::OK
    );
    assert_eq!(
        call(&app, "start", Some(TOKEN), make(REQ1)).await.0,
        StatusCode::CONFLICT
    );
    assert_eq!(
        call(&app, "start", Some(TOKEN), make(REQ3)).await.0,
        StatusCode::OK
    );
    assert_eq!(
        call(&app, "recover", Some(TOKEN), owner("200", OWNER2))
            .await
            .1["count"],
        2
    );
    assert_eq!(
        call(&app, "recover", Some(TOKEN), owner("100", OWNER1))
            .await
            .0,
        StatusCode::CONFLICT
    );
    assert_eq!(
        call(
            &app,
            "read",
            Some(TOKEN),
            json!({"user":USER,"projectId":"a","id":first["id"]})
        )
        .await
        .1["status"],
        "interrupted"
    );
}
