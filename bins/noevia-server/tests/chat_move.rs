//! Synthetic internal move-store contracts. No public move route switch, Node or inference.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]
use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use http_body_util::BodyExt;
use noevia_server::project_store::{router, Store, TOKEN_HEADER};
use serde_json::{json, Value};
use tower::ServiceExt;
const USER: &str = "11111111-2222-4333-8444-555555555555";
const OTHER: &str = "99999999-2222-4333-8444-555555555555";
const START: &str = "0123456789abcdef0123456789abcdef";
const TOKEN: &str = "synthetic-move-foundation-secret-0000";
fn setup() -> (tempfile::TempDir, axum::Router) {
    let root = tempfile::tempdir().unwrap();
    for user in [USER, OTHER] {
        let dir = root.path().join("users").join(user);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("projects.json"),
            r#"{"projects":[{"id":"qa-p","chats":[]}]}"#,
        )
        .unwrap();
        std::fs::write(
            dir.join("free-chats.json"),
            r#"[{"id":"qa-chat","title":"QA"}]"#,
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
) -> (StatusCode, Value, axum::http::HeaderMap) {
    let mut req = Request::builder()
        .method("POST")
        .uri(op)
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
    let headers = response.headers().clone();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (status, serde_json::from_slice(&bytes).unwrap(), headers)
}
#[tokio::test]
async fn internal_endpoints_require_secret_bind_user_and_preserve_tagged_context() {
    let (root, app) = setup();
    let ask = json!({"user":USER,"chat":"qa-chat"});
    for token in [None, Some("old-start-secret")] {
        assert_eq!(
            call(&app, "/v1/chat/grant-context", token, ask.clone())
                .await
                .0,
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(
            call(
                &app,
                "/v1/chat/move",
                token,
                json!({"user":USER,"chat":"qa-chat","projectId":"qa-p"})
            )
            .await
            .0,
            StatusCode::UNAUTHORIZED
        );
    }
    let (status, context, _) = call(&app, "/v1/chat/grant-context", Some(TOKEN), ask.clone()).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(context["holder"], json!({"kind":"free"}));
    assert_eq!(context["generation"], "0");
    assert_eq!(context["start"], START);
    let (status, moved, _) = call(
        &app,
        "/v1/chat/move",
        Some(TOKEN),
        json!({"user":USER,"chat":"qa-chat","projectId":"qa-p"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(moved["generation"], "1");
    assert_eq!(moved["to"], json!({"kind":"project","id":"qa-p"}));
    let (_, other, _) = call(
        &app,
        "/v1/chat/grant-context",
        Some(TOKEN),
        json!({"user":OTHER,"chat":"qa-chat"}),
    )
    .await;
    assert_eq!(other["holder"], json!({"kind":"free"}));
    assert_eq!(other["generation"], "0");
    assert!(!root
        .path()
        .join("users")
        .join(OTHER)
        .join("chat-move-state.json")
        .exists());
    for body in [
        json!({}),
        json!({"user":"../bad","chat":"qa-chat"}),
        json!({"user":USER,"chat":"!!!"}),
    ] {
        assert_eq!(
            call(&app, "/v1/chat/grant-context", Some(TOKEN), body)
                .await
                .0,
            StatusCode::BAD_REQUEST
        );
    }
    for body in [
        json!({"user":USER,"chat":"qa-chat"}),
        json!({"user":USER,"chat":"qa-chat","projectId":3}),
        json!({"user":USER,"chat":"qa-chat","projectId":null,"frame":{"kind":"invalid"}}),
    ] {
        assert_eq!(
            call(&app, "/v1/chat/move", Some(TOKEN), body).await.0,
            StatusCode::BAD_REQUEST
        );
    }
    // A removed account is never recreated by context, recovery or a move.
    let theirs = root.path().join("users").join(OTHER);
    std::fs::remove_dir_all(&theirs).unwrap();
    assert_eq!(
        call(
            &app,
            "/v1/chat/grant-context",
            Some(TOKEN),
            json!({"user":OTHER,"chat":"qa-chat"})
        )
        .await
        .0,
        StatusCode::GONE
    );
    assert!(!theirs.exists());
}
#[tokio::test]
async fn internal_context_recovers_pending_moves_but_file_writers_stay_fenced() {
    use server_projects::chat_move::{self, Holder, Step};
    let (root, app) = setup();
    assert!(chat_move::move_with(
        root.path(),
        USER,
        "qa-chat",
        Holder::Project("qa-p".into()),
        None,
        START,
        &|s| if s == Step::Prepared {
            Err(server_projects::store::StoreError::Busy)
        } else {
            Ok(())
        }
    )
    .is_err());
    let (status, context, _) = call(
        &app,
        "/v1/chat/grant-context",
        Some(TOKEN),
        json!({"user":USER,"chat":"qa-chat"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(context["holder"], json!({"kind":"project","id":"qa-p"}));
    assert_eq!(context["generation"], "1");
    let dir = root.path().join("users").join(USER);
    let _held =
        server_projects::lock::acquire(&dir.join(server_projects::transcript::LOCK_NAME)).unwrap();
    let (status, _, headers) = call(
        &app,
        "/v1/chat/grant-context",
        Some(TOKEN),
        json!({"user":USER,"chat":"qa-chat"}),
    )
    .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(headers["retry-after"], "1");
}
