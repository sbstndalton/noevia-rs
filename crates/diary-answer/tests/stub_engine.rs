//! The async model client against a stub engine on 127.0.0.1: the recorded completions of
//! `diary-core/tests/llm-fixtures.json` replayed over real HTTP (requests, events, replies,
//! failures, retry waits), and the exchange deadline closing a given-up call's connection.
#![allow(clippy::indexing_slicing, clippy::panic)] // serde_json `Value[...]` on fixture data

use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::body::Body;
use axum::http::{Response, StatusCode};
use diary_answer::deadline::{Budget, Timeout};
use diary_answer::model::{Client, Config, Sleep};
use diary_core::llm::{self, Event, LlmError};
use serde_json::{json, Value};

type TestResult = Result<(), Box<dyn std::error::Error>>;

#[derive(Default)]
struct Stub {
    responses: Vec<Value>,
    sent: Vec<Value>,
}

async fn serve(stub: Arc<Mutex<Stub>>) -> Result<String, Box<dyn std::error::Error>> {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    let app = axum::Router::new().fallback(move |req: axum::http::Request<Body>| {
        let stub = stub.clone();
        async move {
            let bytes = axum::body::to_bytes(req.into_body(), 1 << 24)
                .await
                .unwrap_or_default();
            let mut s = stub.lock().unwrap_or_else(|e| e.into_inner());
            s.sent
                .push(serde_json::from_slice(&bytes).unwrap_or(Value::Null));
            let r = if s.responses.is_empty() {
                json!([500, "text/plain", "no more synthetic answers"])
            } else {
                s.responses.remove(0)
            };
            let status = r[0].as_u64().unwrap_or(500) as u16;
            let mut ctype = r[1].as_str().unwrap_or("").to_owned();
            let mut body = r[2].as_str().unwrap_or("").as_bytes().to_vec();
            if let Some(c) = ctype.strip_suffix("+bom").map(str::to_owned) {
                body.splice(0..0, [0xEF, 0xBB, 0xBF]);
                ctype = c;
            } else if let Some(c) = ctype.strip_suffix("+badutf8").map(str::to_owned) {
                body.push(0xFF);
                ctype = c;
            }
            let mut resp = Response::builder()
                .status(StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR));
            if status != 204 && status != 304 {
                resp = resp.header("content-type", ctype);
            }
            resp.body(Body::from(if status == 204 || status == 304 {
                Vec::new()
            } else {
                body
            }))
            .unwrap_or_else(|_| Response::new(Body::empty()))
        }
    });
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    Ok(format!("http://{addr}/v1"))
}

fn recorder(waits: Arc<Mutex<Vec<f64>>>) -> Sleep {
    Arc::new(move |s: f64| {
        waits.lock().unwrap_or_else(|e| e.into_inner()).push(s);
        Box::pin(async {})
    })
}

fn message_matches(rust: &str, python: &str) -> bool {
    match python.split_once(" attempts: ") {
        Some((head, _)) => rust.starts_with(&format!("{head} attempts: ")),
        None => rust == python,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn recorded_completions_over_http() -> TestResult {
    let fx: Value = serde_json::from_str(include_str!("../../diary-core/tests/llm-fixtures.json"))?;
    let stub = Arc::new(Mutex::new(Stub::default()));
    let base = serve(stub.clone()).await?;
    let messages = json!([{"role": "user", "content": "QA question"}]);
    let mut replayed = 0;
    for case in fx["completions"].as_array().ok_or("completions")? {
        let name = case["name"].as_str().unwrap_or("?");
        let responses = case["responses"].as_array().cloned().unwrap_or_default();
        // Transport exceptions and given-up calls are covered by the protocol replay; a deadline
        // drops the future here (tested below).
        // A 204 or 304 cannot carry the body the recording's mock transport delivered with it.
        if responses
            .iter()
            .any(|r| r[1] == json!("raise") || r[0] == json!(204) || r[0] == json!(304))
            || !case["abandon_after"].is_null()
        {
            continue;
        }
        {
            let mut s = stub.lock().unwrap_or_else(|e| e.into_inner());
            s.responses = responses;
            s.sent.clear();
        }
        let waits = Arc::new(Mutex::new(Vec::new()));
        let client = Client::new(Config {
            base_url: base.clone(),
            chat_model: "qa-chat".into(),
            max_retries: case["retries"].as_i64().ok_or("retries")?,
            retry_backoff_s: 0.5,
            reasoning_tokens: case["reasoning_tokens"]
                .as_i64()
                .unwrap_or(llm::DEFAULT_MAX_REASONING_TOKENS),
            ..Config::default()
        })?
        .with_sleep(recorder(waits.clone()));
        let stop: Option<Vec<String>> = case["stop"].as_array().map(|a| {
            a.iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect()
        });
        let max_tokens = case["max_tokens"].as_i64();
        let mut events: Vec<Event> = Vec::new();
        let budget = Budget::new(None, None);
        let messages = &messages;
        let client = &client;
        let result = match case["mode"].as_str() {
            Some("stream") => {
                let mut emit = |e: Event| events.push(e);
                budget
                    .run(false, None, |p| async move {
                        client
                            .chat_stream(messages, &mut emit, 0.7, max_tokens, Some(&p))
                            .await
                    })
                    .await
            }
            Some("cancellable") => {
                budget
                    .run(false, None, |p| async move {
                        client
                            .chat(messages, 0.2, max_tokens, stop.as_deref(), Some(&p))
                            .await
                    })
                    .await
            }
            _ => Ok(client
                .chat(messages, 0.2, max_tokens, stop.as_deref(), None)
                .await),
        }
        .map_err(|t| format!("{name}: {t:?}"))?;
        let expected = &case["expected"];
        let sent = stub.lock().unwrap_or_else(|e| e.into_inner()).sent.clone();
        assert_eq!(json!(sent), expected["sent"], "{name}: requests");
        let got: Vec<Value> = events.iter().map(Event::to_json).collect();
        assert_eq!(json!(got), expected["events"], "{name}: events");
        assert_eq!(
            json!(*waits.lock().unwrap_or_else(|e| e.into_inner())),
            expected["sleeps"],
            "{name}: retry waits"
        );
        let want = &expected["result"];
        match result {
            Ok(reply) => {
                assert!(
                    want["error"].is_null(),
                    "{name}: Python raised {}",
                    want["error"]
                );
                assert_eq!(
                    json!({"content": reply.content, "reasoning": reply.reasoning, "truncated": reply.truncated}),
                    *want,
                    "{name}"
                );
            }
            Err(e) => {
                assert_eq!(
                    json!(e.python_class()),
                    want["error"],
                    "{name}: {e:?} vs {}",
                    want["message"]
                );
                if let LlmError::Llm(m) = &e {
                    let py = want["message"].as_str().unwrap_or("");
                    assert!(message_matches(m, py), "{name}: {m:?} vs {py:?}");
                }
            }
        }
        replayed += 1;
    }
    assert!(replayed >= 170, "replayed {replayed}");
    Ok(())
}

/// A stub that sends the stream headers, then one line every `gap`, and reports when the client
/// went away (the send into the body fails).
async fn slow_engine(
    first_after: Duration,
    gap: Duration,
) -> Result<(String, Arc<Mutex<Option<bool>>>), Box<dyn std::error::Error>> {
    let gone = Arc::new(Mutex::new(None));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    let flag = gone.clone();
    let app = axum::Router::new().fallback(move |_req: axum::http::Request<Body>| {
        let flag = flag.clone();
        async move {
            let (tx, rx) = tokio::sync::mpsc::channel::<Result<Vec<u8>, std::io::Error>>(1);
            tokio::spawn(async move {
                tokio::time::sleep(first_after).await;
                for i in 0..200 {
                    let line = format!(
                        "data: {}\n\n",
                        json!({"choices": [{"delta": {"content": format!("w{i} ")}}]})
                    );
                    if tx.send(Ok(line.into_bytes())).await.is_err() {
                        *flag.lock().unwrap_or_else(|e| e.into_inner()) = Some(true);
                        return;
                    }
                    tokio::time::sleep(gap).await;
                }
                let _ = tx.send(Ok(b"data: [DONE]\n\n".to_vec())).await;
                *flag.lock().unwrap_or_else(|e| e.into_inner()) = Some(false);
            });
            Response::builder()
                .header("content-type", "text/event-stream")
                .body(Body::new(futures_stream(rx)))
                .unwrap_or_else(|_| Response::new(Body::empty()))
        }
    });
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    Ok((format!("http://{addr}/v1"), gone))
}

fn futures_stream(
    mut rx: tokio::sync::mpsc::Receiver<Result<Vec<u8>, std::io::Error>>,
) -> impl axum::body::HttpBody<Data = axum::body::Bytes, Error = std::io::Error> + Send + 'static {
    // A minimal stream body over the channel.
    struct ChannelBody(tokio::sync::mpsc::Receiver<Result<Vec<u8>, std::io::Error>>);
    impl axum::body::HttpBody for ChannelBody {
        type Data = axum::body::Bytes;
        type Error = std::io::Error;
        fn poll_frame(
            mut self: std::pin::Pin<&mut Self>,
            cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<Option<Result<http_body::Frame<Self::Data>, Self::Error>>> {
            self.0
                .poll_recv(cx)
                .map(|o| o.map(|r| r.map(|b| http_body::Frame::data(axum::body::Bytes::from(b)))))
        }
    }
    let _ = &mut rx;
    ChannelBody(rx)
}

async fn wait_gone(gone: &Arc<Mutex<Option<bool>>>) -> Option<bool> {
    for _ in 0..100 {
        if let Some(v) = *gone.lock().unwrap_or_else(|e| e.into_inner()) {
            return Some(v);
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    None
}

fn client_for(base: &str) -> Result<Client, LlmError> {
    Client::new(Config {
        base_url: base.to_owned(),
        chat_model: "qa-chat".into(),
        ..Config::default()
    })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn no_first_token_gives_up_and_closes_the_connection() -> TestResult {
    let (base, gone) = slow_engine(Duration::from_millis(600), Duration::from_millis(10)).await?;
    let client = client_for(&base)?;
    let messages = json!([{"role": "user", "content": "QA"}]);
    let budget = Budget::new(Some(5.0), Some(0.2));
    let mut events = Vec::new();
    let mut emit = |e: Event| events.push(e);
    let out = budget
        .run(true, None, |p| async move {
            client
                .chat_stream(&messages, &mut emit, 0.7, None, Some(&p))
                .await
        })
        .await;
    assert_eq!(out.err(), Some(Timeout::NoFirstToken));
    assert_eq!(
        wait_gone(&gone).await,
        Some(true),
        "the engine saw the client leave"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_total_deadline_gives_up_a_long_answer_and_closes_it() -> TestResult {
    let (base, gone) = slow_engine(Duration::from_millis(10), Duration::from_millis(20)).await?;
    let client = client_for(&base)?;
    let messages = json!([{"role": "user", "content": "QA"}]);
    let budget = Budget::new(Some(0.5), Some(5.0));
    let mut emit = |_e: Event| {};
    let out = budget
        .run(true, None, |p| async move {
            client
                .chat_stream(&messages, &mut emit, 0.7, None, Some(&p))
                .await
        })
        .await;
    assert_eq!(out.err(), Some(Timeout::NotFinished));
    assert_eq!(
        wait_gone(&gone).await,
        Some(true),
        "the engine saw the client leave"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_streamed_answer_within_the_budget_completes() -> TestResult {
    let (base, gone) = slow_engine(Duration::from_millis(5), Duration::from_millis(0)).await?;
    let client = client_for(&base)?;
    let messages = json!([{"role": "user", "content": "QA"}]);
    let budget = Budget::new(Some(10.0), Some(5.0));
    let mut events = Vec::new();
    let mut emit = |e: Event| events.push(e);
    let out = budget
        .run(true, None, |p| async move {
            client
                .chat_stream(&messages, &mut emit, 0.7, None, Some(&p))
                .await
        })
        .await;
    let reply = out
        .map_err(|t| format!("{t:?}"))?
        .map_err(|e| format!("{e:?}"))?;
    assert!(reply.content.starts_with("w0 w1 "));
    assert_eq!(wait_gone(&gone).await, Some(false));
    Ok(())
}

#[tokio::test]
async fn a_spent_budget_never_calls() {
    let budget = Budget::new(Some(0.0), None);
    let out = budget
        .run(false, None, |_p| async { unreachable_call() })
        .await;
    assert_eq!(out.err(), Some(Timeout::BeforeCall));
}

fn unreachable_call() -> u8 {
    7
}
