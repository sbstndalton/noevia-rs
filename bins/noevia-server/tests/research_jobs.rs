//! Synthetic internal deep-research contracts; no public route, model, or storage calls.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use http_body_util::BodyExt;
use noevia_server::project_store::{router, Store, TOKEN_HEADER};
use serde_json::{json, Value};
use tower::ServiceExt;
const USER: &str = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa";
const OTHER: &str = "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb";
const START: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const TOKEN: &str = "synthetic-research-store-token-00000";
const OWNER: &str = "cccccccc-cccc-4ccc-8ccc-cccccccccccc";
const REQ1: &str = "11111111-1111-4111-8111-111111111111";
const REQ2: &str = "22222222-2222-4222-8222-222222222222";
const HASH: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
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
        .uri(format!("/v1/research-jobs/{op}"))
        .header("content-type", "application/json");
    if let Some(t) = token {
        req = req.header(TOKEN_HEADER, t);
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
fn owner() -> Value {
    json!({"user":USER,"start":START,"born":"100","owner":OWNER})
}
fn start(request: &str, project: &str) -> Value {
    json!({"user":USER,"start":START,"born":"100","owner":OWNER,"projectId":project,"request":request,"question":"Q?","mode":"skipped","questions":["Q?"]})
}
#[test]
fn compiled_server_advertises_live_research_store() {
    assert!(noevia_server::FEATURES
        .contains(&server_projects::research_jobs::RESEARCH_JOBS_STORE_MARKER));
}
#[tokio::test]
async fn authenticated_admission_scope_events_cancel_and_partial_late_artifact() {
    let (root, app) = setup();
    assert_eq!(
        call(&app, "start", None, start(REQ1, "a")).await.0,
        StatusCode::UNAUTHORIZED
    );
    assert!(!root.path().join("users").join(USER).join("jobs").exists());
    assert_eq!(
        call(&app, "recover", Some(TOKEN), owner()).await.0,
        StatusCode::OK
    );
    let (_, job) = call(&app, "start", Some(TOKEN), start(REQ1, "a")).await;
    let id = job["id"].as_str().unwrap();
    let run = job["run"].as_str().unwrap();
    assert_eq!(
        call(&app, "start", Some(TOKEN), start(REQ1, "a")).await.1,
        job
    );
    assert_eq!(
        call(&app, "start", Some(TOKEN), start(REQ2, "a")).await.0,
        StatusCode::CONFLICT
    );
    assert_eq!(
        call(&app, "start", Some(TOKEN), start(REQ2, "b")).await.0,
        StatusCode::OK
    );
    for (user, project) in [(OTHER, "a"), (USER, "b")] {
        assert_eq!(
            call(
                &app,
                "read",
                Some(TOKEN),
                json!({"user":user,"projectId":project,"id":id})
            )
            .await
            .0,
            StatusCode::NOT_FOUND
        );
    }
    let ev = json!({"user":USER,"start":START,"born":"100","owner":OWNER,"id":id,"run":run,"n":1,"kind":"checkpoint.created","data":{"step":1,"question":"Q?"}});
    assert_eq!(
        call(&app, "event", Some(TOKEN), ev.clone()).await.0,
        StatusCode::OK
    );
    assert_eq!(call(&app, "event", Some(TOKEN), ev).await.0, StatusCode::OK);
    let cancel = json!({"user":USER,"start":START,"born":"100","owner":OWNER,"projectId":"a","id":id,"run":run});
    assert_eq!(
        call(&app, "cancel", Some(TOKEN), cancel).await.1["cancelRequested"],
        true
    );
    let result = json!({"question":"Q?","markdown":"# Q","sources":[],"sections":1});
    assert_eq!(
        call(&app, "report", Some(TOKEN), json!({"user":USER,"start":START,"born":"100","owner":OWNER,"id":id,"run":run,"result":result})).await.0,
        StatusCode::OK
    );
    let finish = json!({"user":USER,"start":START,"born":"100","owner":OWNER,"id":id,"run":run,"ok":true,"result":result});
    assert_eq!(
        call(&app, "finish", Some(TOKEN), finish.clone()).await.0,
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
        .1["status"],
        "cancelled"
    );
    let prep = json!({"user":USER,"start":START,"born":"100","owner":OWNER,"projectId":"a","id":id,"run":run,"name":"Research a.md","hash":HASH,"bytes":5,"target":HASH});
    assert_eq!(
        call(&app, "prepare", Some(TOKEN), prep.clone()).await.0,
        StatusCode::OK
    );
    assert_eq!(
        call(&app, "artifact", Some(TOKEN), prep.clone()).await.0,
        StatusCode::OK
    );
    assert_eq!(
        call(&app, "artifact", Some(TOKEN), prep).await.0,
        StatusCode::OK
    );
    assert_eq!(
        call(&app, "finish", Some(TOKEN), finish).await.0,
        StatusCode::OK
    );
    assert_eq!(
        call(
            &app,
            "list",
            Some(TOKEN),
            json!({"user":USER,"projectId":"a"})
        )
        .await
        .1["jobs"][0]["artifacts"][0]["name"],
        "Research a.md"
    );
}
