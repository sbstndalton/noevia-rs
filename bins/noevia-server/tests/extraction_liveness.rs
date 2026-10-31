//! sbstndalton/noevia#2240: a request to the document service has no total deadline of minutes (a
//! large scan is slow with the service healthy), but a service that stops answering `/health` while
//! it holds the request is given up on within a few asks. Synthetic sidecars only.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]

use noevia_server::extraction::HttpEngine;
use server_projects::extract::Engine;
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// The tests set the process environment (`DOCLING_BASE_URL`): one at a time.
static ENV: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

const PAGES: &str = r#"{"total":1,"pages":[{"number":1,"text":"Synthetic total 42","status":"native","method":"docling"}]}"#;

/// A service that accepts every connection. `/health` is answered at once when `alive`, else never;
/// `/extract` is answered with `PAGES` after `extract_after`, or never.
async fn service(alive: bool, extract_after: Option<Duration>) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move {
        loop {
            let Ok((mut s, _)) = listener.accept().await else {
                return;
            };
            tokio::spawn(async move {
                let mut buf = Vec::new();
                let mut chunk = [0u8; 65536];
                let (head, mut body) = loop {
                    let Ok(n) = s.read(&mut chunk).await else {
                        return;
                    };
                    if n == 0 {
                        return;
                    }
                    buf.extend_from_slice(&chunk[..n]);
                    if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                        break (
                            String::from_utf8_lossy(&buf[..i]).to_string(),
                            buf[i + 4..].to_vec(),
                        );
                    }
                };
                let path = head
                    .lines()
                    .next()
                    .and_then(|l| l.split(' ').nth(1))
                    .unwrap_or("")
                    .to_string();
                let len: usize = head
                    .lines()
                    .filter_map(|l| l.split_once(": "))
                    .find(|(k, _)| k.eq_ignore_ascii_case("content-length"))
                    .and_then(|(_, v)| v.parse().ok())
                    .unwrap_or(0);
                while body.len() < len {
                    match s.read(&mut chunk).await {
                        Ok(n) if n > 0 => body.extend_from_slice(&chunk[..n]),
                        _ => break,
                    }
                }
                let text = if path == "/health" {
                    if !alive {
                        std::future::pending::<()>().await;
                    }
                    r#"{"ranges":true}"#.to_string()
                } else {
                    match extract_after {
                        Some(wait) => tokio::time::sleep(wait).await,
                        None => std::future::pending::<()>().await,
                    }
                    PAGES.to_string()
                };
                let resp = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{text}",
                    text.len()
                );
                let _ = s.write_all(resp.as_bytes()).await;
            });
        }
    });
    url
}

async fn read(every: Duration, misses: u32) -> (Instant, Result<(), String>, bool) {
    let engine = HttpEngine::new(tokio::runtime::Handle::current(), None)
        .unwrap()
        .with_liveness(every, misses);
    let started = Instant::now();
    let (outcome, timed_out) = tokio::task::spawn_blocking(move || {
        match engine.docling("a.pdf", b"%PDF-1.4 synthetic", false, None) {
            Ok(_) => (Ok(()), false),
            Err(e) => (Err(e.message), e.timed_out),
        }
    })
    .await
    .unwrap();
    (started, outcome, timed_out)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_service_that_holds_the_request_and_stops_answering_is_given_up_on_within_a_few_asks() {
    let _env = ENV.lock().await;
    let url = service(false, None).await;
    std::env::set_var("DOCLING_BASE_URL", &url);
    let (started, outcome, timed_out) = read(Duration::from_millis(200), 2).await;
    std::env::remove_var("DOCLING_BASE_URL");
    let message = outcome.expect_err("a wedged service is not an answer");
    // Two silent asks (200 ms apart, each waiting at most the interval), not the 65 minutes of the
    // request's own deadline.
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "{:?}",
        started.elapsed()
    );
    assert!(timed_out);
    // Retryable (the queue takes it), and in words with no network error text.
    assert!(message.contains("unavailable"), "{message}");
    assert!(!message.contains("fetch failed"), "{message}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_service_that_keeps_answering_health_is_waited_for_beyond_the_asks() {
    let _env = ENV.lock().await;
    // The read takes 2 s, ten times the ask interval times two: a long scan on a healthy service.
    let url = service(true, Some(Duration::from_secs(2))).await;
    std::env::set_var("DOCLING_BASE_URL", &url);
    let (started, outcome, _) = read(Duration::from_millis(200), 2).await;
    std::env::remove_var("DOCLING_BASE_URL");
    assert_eq!(outcome, Ok(()));
    assert!(started.elapsed() >= Duration::from_secs(2));
}
