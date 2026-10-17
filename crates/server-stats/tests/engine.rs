#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]
//! The engine reads' limits, against synthetic engines on loopback: a redirect is never followed
//! (the address is configuration only), every read has the JS's timeout, and the timeouts are
//! core's 8 s / 6 s.
use server_stats::{Engine, Kind, Timeouts};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// An engine answering every request with `reply(target)` after `delay(target)`; counts requests.
async fn serve(
    reply: impl Fn(&str) -> String + Send + Sync + 'static,
    delay: impl Fn(&str) -> Duration + Send + Sync + 'static,
) -> (u16, Arc<AtomicUsize>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let hits = Arc::new(AtomicUsize::new(0));
    let count = Arc::clone(&hits);
    let reply = Arc::new(reply);
    let delay = Arc::new(delay);
    tokio::spawn(async move {
        while let Ok((mut s, _)) = listener.accept().await {
            let (reply, delay, count) =
                (Arc::clone(&reply), Arc::clone(&delay), Arc::clone(&count));
            tokio::spawn(async move {
                let mut head = Vec::new();
                let mut buf = [0u8; 1024];
                while !head.windows(4).any(|w| w == b"\r\n\r\n") {
                    match s.read(&mut buf).await {
                        Ok(0) | Err(_) => return,
                        Ok(n) => head.extend_from_slice(&buf[..n]),
                    }
                }
                count.fetch_add(1, Ordering::SeqCst);
                let text = String::from_utf8_lossy(&head).to_string();
                let target = text.split(' ').nth(1).unwrap().to_string();
                tokio::time::sleep(delay(&target)).await;
                let _ = s.write_all(reply(&target).as_bytes()).await;
                let _ = s.shutdown().await;
            });
        }
    });
    (port, hits)
}

fn ok(body: &str) -> String {
    format!(
        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
}

const LISTING: &str = r#"{"data":[{"id":"a","status":{"value":"loaded"}}]}"#;
const METRICS: &str = "llamacpp:prompt_tokens_total 5\n";

#[test]
fn timeouts_are_cores() {
    let t = Timeouts::default();
    assert_eq!(t.listing, Duration::from_millis(8000));
    assert_eq!(t.metrics, Duration::from_millis(6000));
}

#[tokio::test]
async fn a_redirect_is_refused_and_never_followed() {
    let (elsewhere, hits) = serve(|_| ok(LISTING), |_| Duration::ZERO).await;
    for status in [301, 302, 303, 307, 308] {
        let (port, _) = serve(
            move |_| {
                format!("HTTP/1.1 {status} Moved\r\nLocation: http://127.0.0.1:{elsewhere}/models\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
            },
            |_| Duration::ZERO,
        )
        .await;
        let e = Engine::new(
            Kind::LlamaCpp,
            &format!("http://127.0.0.1:{port}"),
            None,
            Timeouts::default(),
        )
        .unwrap();
        let reply = e.read().await;
        assert!(reply.body.starts_with(r#"{"up":false,"#), "{status}");
    }
    assert_eq!(
        hits.load(Ordering::SeqCst),
        0,
        "the redirect target was asked"
    );
}

#[tokio::test]
async fn a_slow_listing_or_metrics_read_times_out() {
    let short = Timeouts {
        listing: Duration::from_millis(300),
        metrics: Duration::from_millis(200),
    };
    // The listing itself is too slow: up is false.
    let (port, _) = serve(|_| ok(LISTING), |_| Duration::from_secs(5)).await;
    let e = Engine::new(
        Kind::LlamaCpp,
        &format!("http://127.0.0.1:{port}"),
        None,
        short,
    )
    .unwrap();
    let started = Instant::now();
    assert!(e.read().await.body.starts_with(r#"{"up":false,"#));
    assert!(started.elapsed() < Duration::from_secs(2));
    // Only the metrics are slow: the engine is up, the model's values are {}.
    let (port, _) = serve(
        |t| ok(if t == "/models" { LISTING } else { METRICS }),
        |t| {
            if t == "/models" {
                Duration::ZERO
            } else {
                Duration::from_secs(5)
            }
        },
    )
    .await;
    let e = Engine::new(
        Kind::LlamaCpp,
        &format!("http://127.0.0.1:{port}"),
        None,
        short,
    )
    .unwrap();
    let started = Instant::now();
    let body = e.read().await.body;
    assert!(
        body.contains(r#""up":true"#) && body.contains(r#""inputTokensTotal":null"#),
        "{body}"
    );
    assert!(started.elapsed() < Duration::from_secs(2));
    // Fast enough: the counters arrive.
    let (port, _) = serve(
        |t| ok(if t == "/models" { LISTING } else { METRICS }),
        |_| Duration::ZERO,
    )
    .await;
    let e = Engine::new(
        Kind::LlamaCpp,
        &format!("http://127.0.0.1:{port}"),
        None,
        short,
    )
    .unwrap();
    assert!(e.read().await.body.contains(r#""inputTokensTotal":5"#));
}

#[tokio::test]
async fn the_none_kind_asks_nothing() {
    let e = Engine::new(Kind::None, "http://127.0.0.1:9", None, Timeouts::default()).unwrap();
    assert!(e
        .read()
        .await
        .body
        .starts_with(r#"{"up":false,"telemetryScope":null,"mtp":[]"#));
}

#[tokio::test]
async fn a_dead_engine_is_down_not_an_error() {
    // Nothing listens on port 9 (discard) on a test host; a refused connection is up:false.
    // Bound but never listening, and held to the end of the test: a connect is refused, and no
    // parallel test's `bind("127.0.0.1:0")` can be handed the port as it could a dropped
    // listener's (#2016).
    let dead = tokio::net::TcpSocket::new_v4().unwrap();
    dead.bind("127.0.0.1:0".parse().unwrap()).unwrap();
    let port = dead.local_addr().unwrap().port();
    let e = Engine::new(
        Kind::LlamaCpp,
        &format!("http://127.0.0.1:{port}/v1"),
        Some("synthetic"),
        Timeouts::default(),
    )
    .unwrap();
    let reply = e.read().await;
    assert_eq!(reply.status, 200);
    assert!(reply.body.starts_with(r#"{"up":false,"#));
    drop(dead);
}
