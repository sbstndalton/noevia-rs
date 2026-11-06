//! Row H review P1s: a lease's lifetime is measured on absolute deadlines from the moment each call
//! is sent, so neither unrelated authority changes nor slow HTTP can make a lease outlive Node's latch.
//! Public API only, so the same tests run against the head before the fix (fail-before).
#![allow(clippy::unwrap_used, clippy::indexing_slicing, clippy::panic)]

use engine_admission::{Admission, Ask, Authority, Refused};
use model_stream::Cancel;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::time::Instant;

#[derive(Default)]
struct Node {
    puts: AtomicUsize,
    renews: AtomicUsize,
    deletes: AtomicUsize,
    put_delay_ms: AtomicU64,
    renew_delay_ms: AtomicU64,
    /// 0: 200.
    renew_status: AtomicU64,
}

const TTL_MS: u64 = 300;

async fn node() -> (String, Arc<Node>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let st = Arc::new(Node::default());
    let state = Arc::clone(&st);
    tokio::spawn(async move {
        loop {
            let (mut s, _) = listener.accept().await.unwrap();
            let st = Arc::clone(&state);
            tokio::spawn(async move {
                let mut buf = vec![0u8; 4096];
                let n = s.read(&mut buf).await.unwrap_or(0);
                let text = String::from_utf8_lossy(&buf[..n]).into_owned();
                let first = text.lines().next().unwrap_or("").to_owned();
                let id = first
                    .split(' ')
                    .nth(1)
                    .unwrap_or("")
                    .rsplit('/')
                    .find(|p| p.len() == 32)
                    .unwrap_or("")
                    .to_owned();
                let (status, body) = if first.starts_with("PUT") {
                    st.puts.fetch_add(1, Ordering::SeqCst);
                    tokio::time::sleep(Duration::from_millis(
                        st.put_delay_ms.load(Ordering::SeqCst),
                    ))
                    .await;
                    (201, format!(r#"{{"lease":"{id}","ttlMs":{TTL_MS}}}"#))
                } else if first.contains("/renew") {
                    st.renews.fetch_add(1, Ordering::SeqCst);
                    tokio::time::sleep(Duration::from_millis(
                        st.renew_delay_ms.load(Ordering::SeqCst),
                    ))
                    .await;
                    let code = st.renew_status.load(Ordering::SeqCst);
                    (if code == 0 { 200 } else { code }, "{}".to_owned())
                } else {
                    st.deletes.fetch_add(1, Ordering::SeqCst);
                    (204, String::new())
                };
                let reply = format!(
                    "HTTP/1.1 {status} X\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = s.write_all(reply.as_bytes()).await;
            });
        }
    });
    (base, st)
}

fn setup(base: &str, authority: &Authority) -> Arc<Admission> {
    authority.node_session("boot-1", 1).unwrap();
    Arc::new(Admission::with_authority(base, Some("secret"), authority.clone()).unwrap())
}

/// Other tickets issued and given back every 20 ms: the authority's change notification fires constantly.
fn churn(authority: &Authority, for_ms: u64) -> tokio::task::JoinHandle<()> {
    let a = authority.clone();
    tokio::spawn(async move {
        let until = Instant::now() + Duration::from_millis(for_ms);
        while Instant::now() < until {
            if let Some(t) = a.issue() {
                a.end(&t.id);
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
}

#[tokio::test]
async fn a_grant_that_arrives_after_its_ttl_is_refused_and_given_back() {
    let (base, node) = node().await;
    let authority = Authority::new();
    let adm = setup(&base, &authority);
    node.put_delay_ms.store(TTL_MS + 150, Ordering::SeqCst);
    let got = adm.enter(Ask::Engine { model: "m" }, &Cancel::new()).await;
    assert_eq!(
        got.err(),
        Some(Refused {
            status: 503,
            message: "The model is busy. Try again shortly.".into(),
            code: None
        })
    );
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(authority.held(), 0);
    assert_eq!(node.deletes.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn unrelated_releases_do_not_starve_renewal() {
    let (base, node) = node().await;
    let authority = Authority::new();
    let lease = setup(&base, &authority)
        .enter(Ask::Engine { model: "m" }, &Cancel::new())
        .await
        .unwrap();
    churn(&authority, 1200).await.unwrap();
    assert!(
        node.renews.load(Ordering::SeqCst) >= 5,
        "renewals: {}",
        node.renews.load(Ordering::SeqCst)
    );
    assert!(!lease.lost().is_cancelled() && lease.covered());
    lease.release().await;
}

#[tokio::test]
async fn under_churn_a_node_that_stops_confirming_still_loses_the_lease_within_its_ttl() {
    let (base, node) = node().await;
    let authority = Authority::new();
    node.renew_status.store(500, Ordering::SeqCst);
    let started = Instant::now();
    let lease = setup(&base, &authority)
        .enter(Ask::Engine { model: "m" }, &Cancel::new())
        .await
        .unwrap();
    let c = churn(&authority, 1500);
    tokio::time::timeout(
        Duration::from_millis(TTL_MS + 150),
        lease.lost().cancelled(),
    )
    .await
    .unwrap();
    assert!(started.elapsed() <= Duration::from_millis(TTL_MS + 150));
    c.abort();
}

#[tokio::test]
async fn a_renewal_that_never_answers_loses_the_lease_at_its_proven_deadline() {
    let (base, node) = node().await;
    let authority = Authority::new();
    node.renew_delay_ms.store(10_000, Ordering::SeqCst);
    let lease = setup(&base, &authority)
        .enter(Ask::Engine { model: "m" }, &Cancel::new())
        .await
        .unwrap();
    tokio::time::timeout(
        Duration::from_millis(TTL_MS + 150),
        lease.lost().cancelled(),
    )
    .await
    .unwrap();
    assert!(!lease.covered());
}

#[tokio::test]
async fn a_node_restart_during_a_slow_renewal_ends_the_lease_at_once() {
    let (base, node) = node().await;
    let authority = Authority::new();
    node.renew_delay_ms.store(10_000, Ordering::SeqCst);
    let lease = setup(&base, &authority)
        .enter(Ask::Engine { model: "m" }, &Cancel::new())
        .await
        .unwrap();
    while node.renews.load(Ordering::SeqCst) == 0 {
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    authority.node_session("boot-2", 2).unwrap();
    tokio::time::timeout(Duration::from_millis(60), lease.lost().cancelled())
        .await
        .unwrap();
    assert_eq!(authority.held(), 0);
}

#[tokio::test]
async fn the_hard_cap_during_a_slow_renewal_ends_the_lease_on_time() {
    let (base, node) = node().await;
    let authority = Authority::with_hard(Duration::from_millis(150));
    node.renew_delay_ms.store(10_000, Ordering::SeqCst);
    let started = Instant::now();
    let lease = setup(&base, &authority)
        .enter(Ask::Engine { model: "m" }, &Cancel::new())
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_millis(TTL_MS), lease.lost().cancelled())
        .await
        .unwrap();
    assert!(started.elapsed() < Duration::from_millis(TTL_MS));
}
