use axum::{
    body::Bytes,
    extract::State,
    http::{HeaderMap, Method, StatusCode},
    response::IntoResponse,
    routing::any,
    Router,
};
use backend_runtime::{
    approvals::Binding, chat::Tool, mcp::Session, tools::McpTool, transport::Endpoint, Control,
};
use serde_json::{json, Value};
use std::{sync::Arc, time::Duration};
use tokio::sync::Mutex;

#[derive(Clone)]
struct Fixture {
    calls: Arc<Mutex<Vec<String>>>,
    malformed: bool,
    cancel: Option<Control>,
}
async fn handle(
    State(f): State<Fixture>,
    method: Method,
    headers: HeaderMap,
    bytes: Bytes,
) -> axum::response::Response {
    assert_eq!(
        headers
            .get("mcp-protocol-version")
            .and_then(|h| h.to_str().ok()),
        Some("2025-06-18")
    );
    if method == Method::DELETE {
        assert_eq!(
            headers.get("mcp-session-id").and_then(|h| h.to_str().ok()),
            Some("synthetic-session")
        );
        f.calls.lock().await.push("DELETE".into());
        return StatusCode::METHOD_NOT_ALLOWED.into_response();
    }
    let Ok(v) = serde_json::from_slice::<Value>(&bytes) else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    let Some(name) = v.get("method").and_then(Value::as_str) else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    f.calls.lock().await.push(name.into());
    if name == "initialize" {
        assert_eq!(
            v.pointer("/params/protocolVersion"),
            Some(&json!("2025-06-18"))
        );
        let body = if f.malformed {
            "not json".into()
        } else {
            json!({"jsonrpc":"2.0","id":v.get("id"),"result":{"protocolVersion":"2025-06-18","capabilities":{"tools":{}}}}).to_string()
        };
        return (
            [
                ("mcp-session-id", "synthetic-session"),
                ("content-type", "application/json"),
            ],
            body,
        )
            .into_response();
    }
    assert_eq!(
        headers.get("mcp-session-id").and_then(|h| h.to_str().ok()),
        Some("synthetic-session")
    );
    if name == "notifications/initialized" {
        return StatusCode::ACCEPTED.into_response();
    }
    assert_eq!(name, "tools/call");
    if let Some(c) = &f.cancel {
        c.cancel();
    }
    ([("content-type", "application/json")], json!({"jsonrpc":"2.0","id":v.get("id"),"result":{"content":[{"type":"text","text":"actual result"}]}}).to_string()).into_response()
}

#[tokio::test]
async fn failed_handshake_closes_issued_session_without_replaying(
) -> Result<(), Box<dyn std::error::Error>> {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let origin = format!("http://{}", listener.local_addr()?);
    let app = Router::new()
        .route("/mcp", any(handle))
        .with_state(Fixture {
            calls: calls.clone(),
            malformed: true,
            cancel: None,
        });
    let server = tokio::spawn(async move { axum::serve(listener, app).await });
    let mut session = Session::new(Endpoint::new(&format!("{origin}/mcp"), &[origin], None)?)?;
    assert!(session
        .initialize(&Control::new(Duration::from_secs(5))?)
        .await
        .is_err());
    assert_eq!(*calls.lock().await, ["initialize", "DELETE"]);
    server.abort();
    Ok(())
}

#[tokio::test]
async fn tool_result_and_cancellation_both_close_without_reusing_session(
) -> Result<(), Box<dyn std::error::Error>> {
    for cancel in [false, true] {
        let control = Control::new(Duration::from_secs(5))?;
        let calls = Arc::new(Mutex::new(Vec::new()));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let origin = format!("http://{}", listener.local_addr()?);
        let app = Router::new()
            .route("/mcp", any(handle))
            .with_state(Fixture {
                calls: calls.clone(),
                malformed: false,
                cancel: cancel.then(|| control.clone()),
            });
        let server = tokio::spawn(async move { axum::serve(listener, app).await });
        let tool = McpTool {
            user: "synthetic-user".into(),
            scope: "project".into(),
            session: Arc::new(Mutex::new(Session::new(Endpoint::new(
                &format!("{origin}/mcp"),
                &[origin],
                None,
            )?)?)),
            name: "read".into(),
            parameters: json!({"type":"object"}),
            writes: false,
        };
        let binding = Binding {
            user: "synthetic-user".into(),
            scope: "project".into(),
            generation: "1".into(),
            canonical_chat_id: "chat".into(),
            holder: backend_runtime::approvals::Holder::Project {
                id: "project".into(),
            },
            start: "a".repeat(32),
            tombstoned: false,
        };
        let result = tool.call(&binding, json!({}), &control).await;
        if cancel {
            assert!(result.is_err());
        } else {
            assert_eq!(
                result?.pointer("/content/0/text"),
                Some(&json!("actual result"))
            );
        }
        assert_eq!(
            *calls.lock().await,
            [
                "initialize",
                "notifications/initialized",
                "tools/call",
                "DELETE"
            ]
        );
        server.abort();
    }
    Ok(())
}
