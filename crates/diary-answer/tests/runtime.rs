//! Behaviour of the runtime the recorded replays cannot show: embeddings over HTTP (credentials,
//! redirects, too-long inputs, retries), request headers, real connection failures, giving up a
//! call before its answer starts or during a retry wait, and the budget arithmetic on paused time.
//! Expected values follow `diary/agent/llm.py`, `util.py` and `deadline.py`.
#![allow(clippy::indexing_slicing, clippy::panic)] // serde_json `Value[...]` on test data

use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::body::Body;
use axum::http::{Response, StatusCode};
use diary_answer::deadline::{Budget, Timeout};
use diary_answer::model::{Client, Config, EmbedError, Sleep};
use diary_core::llm::{Event, LlmError};
use serde_json::{json, Value};

type TestResult = Result<(), Box<dyn std::error::Error>>;

/// (path, headers, JSON body) of one request.
type Request = (String, Vec<(String, String)>, Value);
/// (status, headers, body) of one scripted answer.
type Answer = (u16, Vec<(&'static str, String)>, String);
type Stub = Result<(String, Arc<Mutex<Seen>>), Box<dyn std::error::Error>>;

#[derive(Default)]
struct Seen {
    requests: Vec<Request>,
    dropped: bool,
}

/// A stub that answers each request with the next of `script` (status, headers, body) after
/// `delay`, and records the request; `dropped` is set when a handler is cancelled mid-wait.
async fn stub(script: Vec<Answer>, delay: Duration) -> Stub {
    let seen = Arc::new(Mutex::new(Seen::default()));
    let script = Arc::new(Mutex::new(script));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    let s = seen.clone();
    let app = axum::Router::new().fallback(move |req: axum::http::Request<Body>| {
        let seen = s.clone();
        let script = script.clone();
        async move {
            let path = req.uri().path().to_owned();
            let headers: Vec<(String, String)> = req
                .headers()
                .iter()
                .map(|(k, v)| (k.as_str().to_owned(), v.to_str().unwrap_or("").to_owned()))
                .collect();
            let body = axum::body::to_bytes(req.into_body(), 1 << 20)
                .await
                .unwrap_or_default();
            seen.lock()
                .unwrap_or_else(|p| p.into_inner())
                .requests
                .push((
                    path,
                    headers,
                    serde_json::from_slice(&body).unwrap_or(Value::Null),
                ));
            struct Guard(Arc<Mutex<Seen>>, bool);
            impl Drop for Guard {
                fn drop(&mut self) {
                    if !self.1 {
                        self.0.lock().unwrap_or_else(|p| p.into_inner()).dropped = true;
                    }
                }
            }
            let mut guard = Guard(seen.clone(), false);
            tokio::time::sleep(delay).await;
            guard.1 = true;
            let (status, headers, body) = {
                let mut q = script.lock().unwrap_or_else(|p| p.into_inner());
                if q.is_empty() {
                    (500, Vec::new(), "no more".to_owned())
                } else {
                    q.remove(0)
                }
            };
            let mut resp = Response::builder()
                .status(StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR));
            for (k, v) in headers {
                resp = resp.header(k, v);
            }
            resp.body(Body::from(body))
                .unwrap_or_else(|_| Response::new(Body::empty()))
        }
    });
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    Ok((format!("http://{addr}/v1"), seen))
}

fn json_ok(body: Value) -> Answer {
    (
        200,
        vec![("content-type", "application/json".to_owned())],
        body.to_string(),
    )
}

fn recorder(waits: Arc<Mutex<Vec<f64>>>) -> Sleep {
    Arc::new(move |s: f64| {
        waits.lock().unwrap_or_else(|p| p.into_inner()).push(s);
        Box::pin(async {})
    })
}

fn header<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
    headers
        .iter()
        .find(|(k, _)| k == name)
        .map(|(_, v)| v.as_str())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn embeddings_on_the_engine_carry_its_credential_and_come_back_in_index_order() -> TestResult
{
    let reply = json!({"data": [{"index": 1, "embedding": [0.5, 0.25]}, {"index": 0, "embedding": [1.0, 0.0]}]});
    let (base, seen) = stub(vec![json_ok(reply)], Duration::ZERO).await?;
    let client = Client::new(Config {
        base_url: base,
        api_key: "qa-key".into(),
        embed_model: "qa-embed".into(),
        ..Config::default()
    })?;
    let got = client
        .embed(&["first".into(), "second".into()], Some(""))
        .await;
    assert_eq!(got, Ok(vec![vec![1.0, 0.0], vec![0.5, 0.25]]));
    let s = seen.lock().unwrap_or_else(|p| p.into_inner());
    let (path, headers, body) = &s.requests[0];
    assert_eq!(path, "/v1/embeddings");
    assert_eq!(header(headers, "authorization"), Some("Bearer qa-key"));
    assert_eq!(header(headers, "content-type"), Some("application/json"));
    assert_eq!(header(headers, "accept-encoding"), Some("identity"));
    // `model or self.embed_model`: an empty id means the configured one.
    assert_eq!(
        *body,
        json!({"model": "qa-embed", "input": ["first", "second"]})
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_dedicated_embeddings_server_never_receives_the_engine_credential() -> TestResult {
    let (base, seen) = stub(
        vec![json_ok(json!({"data": [{"embedding": [1.0]}]}))],
        Duration::ZERO,
    )
    .await?;
    let client = Client::new(Config {
        base_url: "http://qa-engine.invalid/v1".into(),
        api_key: "qa-key".into(),
        embed_base_url: format!("{base}/"),
        embed_model: "qa-embed".into(),
        ..Config::default()
    })?;
    assert_eq!(client.embed(&["x".into()], None).await, Ok(vec![vec![1.0]]));
    let s = seen.lock().unwrap_or_else(|p| p.into_inner());
    assert_eq!(header(&s.requests[0].1, "authorization"), None);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn embeddings_refuse_redirects_and_too_long_inputs_without_retrying() -> TestResult {
    let (base, seen) = stub(
        vec![
            (307, vec![("location", "http://qa-elsewhere.invalid/v1/embeddings".to_owned())], String::new()),
            (400, Vec::new(), "input (812 tokens) is too large to process. increase the physical batch size (current batch size: 512)".into()),
        ],
        Duration::ZERO,
    )
    .await?;
    let waits = Arc::new(Mutex::new(Vec::new()));
    let client = Client::new(Config {
        base_url: base,
        embed_model: "qa-embed".into(),
        ..Config::default()
    })?
    .with_sleep(recorder(waits.clone()));
    assert_eq!(
        client.embed(&["x".into()], None).await,
        Err(EmbedError::Redirected(307))
    );
    match client.embed(&["x".into()], None).await {
        Err(EmbedError::TooLarge(t)) => assert_eq!((t.tokens, t.limit), (Some(812), Some(512))),
        other => panic!("{other:?}"),
    }
    assert_eq!(
        seen.lock()
            .unwrap_or_else(|p| p.into_inner())
            .requests
            .len(),
        2
    );
    assert!(waits.lock().unwrap_or_else(|p| p.into_inner()).is_empty());
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn embedding_failures_are_retried_with_doubling_waits() -> TestResult {
    let (base, seen) = stub(
        vec![
            (500, Vec::new(), "down".into()),
            json_ok(json!({"data": []})),
            json_ok(json!({"data": [{"embedding": [2.0]}]})),
        ],
        Duration::ZERO,
    )
    .await?;
    let waits = Arc::new(Mutex::new(Vec::new()));
    let client = Client::new(Config {
        base_url: base,
        embed_model: "qa-embed".into(),
        retry_backoff_s: 0.5,
        ..Config::default()
    })?
    .with_sleep(recorder(waits.clone()));
    assert_eq!(client.embed(&["x".into()], None).await, Ok(vec![vec![2.0]]));
    assert_eq!(
        *waits.lock().unwrap_or_else(|p| p.into_inner()),
        vec![0.5, 1.0]
    );
    assert_eq!(
        seen.lock()
            .unwrap_or_else(|p| p.into_inner())
            .requests
            .len(),
        3
    );
    assert_eq!(
        client.embed(&[], None).await,
        Ok(Vec::new()),
        "no texts, no call"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn chat_requests_carry_the_python_headers() -> TestResult {
    let answer = json!({"choices": [{"message": {"content": "ok"}, "finish_reason": "stop"}]});
    let (base, seen) = stub(vec![json_ok(answer)], Duration::ZERO).await?;
    let client = Client::new(Config {
        base_url: base,
        api_key: "qa-key".into(),
        chat_model: "qa-chat".into(),
        ..Config::default()
    })?;
    let reply = client
        .chat(
            &json!([{"role": "user", "content": "QA"}]),
            0.2,
            None,
            None,
            None,
        )
        .await;
    assert_eq!(reply.map(|r| r.content), Ok("ok".to_owned()));
    let s = seen.lock().unwrap_or_else(|p| p.into_inner());
    let (path, headers, _) = &s.requests[0];
    assert_eq!(path, "/v1/chat/completions");
    assert_eq!(header(headers, "authorization"), Some("Bearer qa-key"));
    assert_eq!(header(headers, "accept-encoding"), Some("identity"));
    assert_eq!(header(headers, "content-type"), Some("application/json"));
    Ok(())
}

fn closed_port() -> Result<String, Box<dyn std::error::Error>> {
    let l = std::net::TcpListener::bind("127.0.0.1:0")?;
    let addr = l.local_addr()?;
    drop(l);
    Ok(format!("http://{addr}/v1"))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_refused_connection_is_retried_only_where_python_retries_it() -> TestResult {
    let base = closed_port()?;
    let waits = Arc::new(Mutex::new(Vec::new()));
    let client = Client::new(Config {
        base_url: base,
        chat_model: "qa-chat".into(),
        max_retries: 2,
        retry_backoff_s: 0.5,
        ..Config::default()
    })?
    .with_sleep(recorder(waits.clone()));
    let messages = json!([{"role": "user", "content": "QA"}]);
    let budget = Budget::new(Some(30.0), None);
    let out = budget
        .run(false, None, |p| async move {
            client.chat(&messages, 0.2, None, None, Some(&p)).await
        })
        .await;
    assert_eq!(
        out,
        Ok(Err(LlmError::Llm(
            "chat failed after 2 attempts: ConnectError".into()
        )))
    );
    assert_eq!(*waits.lock().unwrap_or_else(|p| p.into_inner()), vec![0.5]);
    let client = Client::new(Config {
        base_url: closed_port()?,
        chat_model: "qa-chat".into(),
        ..Config::default()
    })?;
    let mut emit = |_e: Event| {};
    let streamed = client
        .chat_stream(&json!([]), &mut emit, 0.7, None, None)
        .await;
    assert_eq!(streamed.err(), Some(LlmError::Transport("ConnectError")));
    Ok(())
}

async fn wait_dropped(seen: &Arc<Mutex<Seen>>) -> bool {
    for _ in 0..100 {
        if seen.lock().unwrap_or_else(|p| p.into_inner()).dropped {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    false
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn giving_up_before_the_answer_starts_closes_the_connection() -> TestResult {
    // #127: "also while waiting for the first byte" (prompt processing).
    let (base, seen) = stub(
        vec![(
            200,
            vec![("content-type", "text/event-stream".to_owned())],
            "data: [DONE]\n\n".into(),
        )],
        Duration::from_secs(3),
    )
    .await?;
    let client = Client::new(Config {
        base_url: base,
        chat_model: "qa-chat".into(),
        ..Config::default()
    })?;
    let messages = json!([{"role": "user", "content": "QA"}]);
    let budget = Budget::new(Some(5.0), Some(0.2));
    let mut emit = |_e: Event| {};
    let out = budget
        .run(true, None, |p| async move {
            client
                .chat_stream(&messages, &mut emit, 0.7, None, Some(&p))
                .await
        })
        .await;
    assert_eq!(out.err(), Some(Timeout::NoFirstToken));
    assert!(
        wait_dropped(&seen).await,
        "the engine's handler was cancelled: the connection closed"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn giving_up_during_a_retry_wait_sends_nothing_again() -> TestResult {
    let (base, seen) = stub(
        vec![
            (503, Vec::new(), "busy".into()),
            (503, Vec::new(), "busy".into()),
        ],
        Duration::ZERO,
    )
    .await?;
    let client = Client::new(Config {
        base_url: base,
        chat_model: "qa-chat".into(),
        max_retries: 3,
        retry_backoff_s: 5.0,
        ..Config::default()
    })?;
    let messages = json!([{"role": "user", "content": "QA"}]);
    let budget = Budget::new(Some(0.4), None);
    let out = budget
        .run(false, None, |p| async move {
            client.chat(&messages, 0.2, None, None, Some(&p)).await
        })
        .await;
    assert_eq!(out.err(), Some(Timeout::NotFinished));
    tokio::time::sleep(Duration::from_millis(600)).await;
    assert_eq!(
        seen.lock()
            .unwrap_or_else(|p| p.into_inner())
            .requests
            .len(),
        1,
        "no second try after the give-up"
    );
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn a_cap_limits_one_call() {
    let budget = Budget::new(Some(300.0), None);
    let out = budget
        .run(false, Some(1.0), |_p| {
            tokio::time::sleep(Duration::from_secs(2))
        })
        .await;
    assert_eq!(out, Err(Timeout::NotFinished));
    // Python `min(remaining, nan)` is `remaining`: a NaN cap does not refuse the call.
    let out = budget.run(false, Some(f64::NAN), |_p| async { 5 }).await;
    assert_eq!(out, Ok(5));
}

#[tokio::test(start_paused = true)]
async fn a_call_that_finishes_counts_as_its_first_token() {
    let budget = Budget::new(Some(10.0), Some(1.0));
    let out = budget.run(true, None, |_p| async { "done" }).await;
    assert_eq!(out, Ok("done"));
}

#[tokio::test(start_paused = true)]
async fn the_first_token_wait_is_capped_by_the_total() {
    let budget = Budget::new(Some(1.0), Some(5.0));
    let started = tokio::time::Instant::now();
    let out = budget
        .run(true, None, |_p| tokio::time::sleep(Duration::from_secs(60)))
        .await;
    assert_eq!(out, Err(Timeout::NoFirstToken));
    assert!(started.elapsed() <= Duration::from_millis(1100));
}

#[tokio::test(start_paused = true)]
async fn progress_starts_the_answer_and_the_total_still_bounds_it() {
    let budget = Budget::new(Some(3.0), Some(1.0));
    let out = budget
        .run(true, None, |p| async move {
            tokio::time::sleep(Duration::from_millis(500)).await;
            p.alive();
            tokio::time::sleep(Duration::from_secs(10)).await;
        })
        .await;
    assert_eq!(out, Err(Timeout::NotFinished));
}

#[tokio::test(start_paused = true)]
async fn an_unbounded_budget_is_not_spent() {
    let budget = Budget::new(Some(f64::INFINITY), None);
    assert_eq!(budget.run(false, None, |_p| async { 1 }).await, Ok(1));
    let budget = Budget::new(Some(-3.0), None);
    assert_eq!(
        budget.run(false, None, |_p| async { 1 }).await,
        Err(Timeout::BeforeCall)
    );
}
