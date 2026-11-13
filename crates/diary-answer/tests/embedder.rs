//! The HTTP embedder against a stub embeddings server: the framing each profile sends, which
//! endpoint gets the engine credential, and a document the server refuses as too long cut and
//! averaged (`embed_fitting`). Expected values follow `diary/agent/embedding.py`.
#![allow(clippy::indexing_slicing, clippy::panic)] // serde_json `Value[...]` on test data

use std::sync::{Arc, Mutex};

use axum::body::Body;
use axum::http::{Response, StatusCode};
use diary_answer::embedder::HttpEmbedder;
use diary_answer::model::Config;
use diary_retrieval::embedding::config::EmbeddingConfig;
use diary_retrieval::index::{Embedder, Kind};
use diary_retrieval::Target;
use serde_json::{json, Value};

type TestResult = Result<(), Box<dyn std::error::Error>>;
type Seen = Arc<Mutex<Vec<(Option<String>, Value)>>>;

/// Answers [len(text), 1.0] per input; refuses any input over 100 characters as too long.
async fn stub() -> Result<(String, Seen), Box<dyn std::error::Error>> {
    let seen: Seen = Arc::new(Mutex::new(Vec::new()));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    let s = seen.clone();
    let app = axum::Router::new().fallback(move |req: axum::http::Request<Body>| {
        let seen = s.clone();
        async move {
            let auth = req.headers().get("authorization").and_then(|v| v.to_str().ok()).map(str::to_owned);
            let bytes = axum::body::to_bytes(req.into_body(), 1 << 20).await.unwrap_or_default();
            let body: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
            seen.lock().unwrap_or_else(|p| p.into_inner()).push((auth, body.clone()));
            let inputs: Vec<String> = body["input"].as_array().map(|a| a.iter().filter_map(Value::as_str).map(str::to_owned).collect()).unwrap_or_default();
            if let Some(long) = inputs.iter().find(|t| t.chars().count() > 100) {
                let msg = format!("input ({} tokens) is too large to process. increase the physical batch size (current batch size: 100)", long.chars().count());
                return Response::builder().status(StatusCode::BAD_REQUEST).body(Body::from(msg)).unwrap_or_else(|_| Response::new(Body::empty()));
            }
            let data: Vec<Value> = inputs.iter().enumerate().map(|(i, t)| json!({"index": i, "embedding": [t.chars().count() as f64, 1.0]})).collect();
            Response::builder()
                .header("content-type", "application/json")
                .body(Body::from(json!({"data": data}).to_string()))
                .unwrap_or_else(|_| Response::new(Body::empty()))
        }
    });
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    Ok((format!("http://{addr}/v1"), seen))
}

fn embedder(engine: &str) -> Result<HttpEmbedder, Box<dyn std::error::Error>> {
    let cfg = json!({"llm": {"embed_model": "qa-embed-a"}});
    let config = Arc::new(Mutex::new(EmbeddingConfig::new(&cfg, None)?));
    Ok(HttpEmbedder::new(
        Config {
            base_url: engine.to_owned(),
            api_key: "qa-key".into(),
            max_retries: 1,
            ..Config::default()
        },
        config,
        tokio::runtime::Handle::current(),
    ))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn each_profile_frames_its_text_and_only_the_engine_gets_the_credential() -> TestResult {
    let (base, seen) = stub().await?;
    let e = Arc::new(embedder(&base)?);
    let dedicated = Target::new(Some(&base), "qa-embed-b", "embeddinggemma-2");
    let engine = Target::new(None, "qa-embed-a", "nomic-v1");
    let e2 = e.clone();
    let (q, d) = tokio::task::spawn_blocking(move || {
        (
            e2.embed(&dedicated, &["kites"], Kind::Query),
            e2.embed(&engine, &["kites"], Kind::Document),
        )
    })
    .await?;
    assert_eq!(q.map_err(|e| e.to_string())?, vec![vec![34.0, 1.0]]);
    assert_eq!(d.map_err(|e| e.to_string())?, vec![vec![5.0, 1.0]]);
    let s = seen.lock().unwrap_or_else(|p| p.into_inner());
    assert_eq!(
        s[0],
        (
            None,
            json!({"model": "qa-embed-b", "input": ["task: search result | query: kites"]})
        )
    );
    assert_eq!(
        s[1],
        (
            Some("Bearer qa-key".into()),
            json!({"model": "qa-embed-a", "input": ["kites"]})
        )
    );
    assert_eq!(e.active(), Target::new(None, "qa-embed-a", "nomic-v1"));
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_document_refused_as_too_long_is_cut_and_averaged() -> TestResult {
    let (base, seen) = stub().await?;
    let e = embedder(&base)?;
    let target = Target::new(None, "qa-embed-a", "nomic-v1");
    let text = "Synthetic paragraph about kites and tides.\n\n".repeat(6);
    let got =
        tokio::task::spawn_blocking(move || e.embed(&target, &[text.as_str()], Kind::Document))
            .await?;
    let v = got.map_err(|e| e.to_string())?;
    assert_eq!(v.len(), 1);
    let norm = (v[0][0] * v[0][0] + v[0][1] * v[0][1]).sqrt();
    assert!(
        (norm - 1.0).abs() < 1e-12,
        "the mean of the piece vectors is normalised: {v:?}"
    );
    let s = seen.lock().unwrap_or_else(|p| p.into_inner());
    assert!(
        s.len() >= 3,
        "refused once, then sent in pieces: {}",
        s.len()
    );
    assert!(s
        .iter()
        .skip(1)
        .all(|(_, b)| b["input"].as_array().is_some_and(|a| a.len() == 1)));
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unknown_profile_fails_closed_before_any_call() -> TestResult {
    let (base, seen) = stub().await?;
    let e = embedder(&base)?;
    let target = Target::new(None, "qa-embed-a", "qa-unknown-profile");
    let got = tokio::task::spawn_blocking(move || e.embed(&target, &["x"], Kind::Document)).await?;
    assert!(got.is_err());
    assert!(seen.lock().unwrap_or_else(|p| p.into_inner()).is_empty());
    Ok(())
}
