//! Signed grants end to end (noevia `docs/egress-grant-contract.md`): the proxy installs a
//! grant from a valid `ngr1.` token, enforces exactly its hosts, and refuses forged, tampered,
//! expired, superseded, revoked, replayed and wrong-version tokens. Synthetic keys and hosts.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use std::io;
use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use egress::{mint, Act, GrantKey, SignedGrant};
use egress_proxy::{
    BoxFuture, Clock, Connector, KeySource, LogSink, Proxy, ProxyBuilder, Resolver,
};
use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

const NOW: u64 = 1_800_000_000_000;
const PUBLIC: &str = "93.184.216.34";

fn key() -> GrantKey {
    GrantKey::from_bytes([0x42; 32])
}

struct FixedResolver;
impl Resolver for FixedResolver {
    fn resolve<'a>(&'a self, _host: &'a str) -> BoxFuture<'a, io::Result<Vec<IpAddr>>> {
        Box::pin(async { Ok(vec![PUBLIC.parse().unwrap()]) })
    }
}

struct ToUpstream(SocketAddr);
impl Connector for ToUpstream {
    fn connect(&self, _addr: SocketAddr) -> BoxFuture<'_, io::Result<TcpStream>> {
        let a = self.0;
        Box::pin(async move { TcpStream::connect(a).await })
    }
}

struct ManualClock(AtomicU64);
impl Clock for ManualClock {
    fn now_ms(&self) -> u64 {
        self.0.load(Ordering::SeqCst)
    }
}

struct H {
    addr: SocketAddr,
    proxy: Arc<Proxy>,
    clock: Arc<ManualClock>,
    logs: Arc<Mutex<Vec<Value>>>,
}

async fn upstream() -> SocketAddr {
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let (mut s, _) = l.accept().await.unwrap();
            tokio::spawn(async move {
                let mut buf = [0u8; 4096];
                let _ = s.read(&mut buf).await;
                s.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok")
                    .await
                    .ok();
                s.shutdown().await.ok();
            });
        }
    });
    addr
}

async fn start_with(source: Option<KeySource>) -> H {
    let up = upstream().await;
    let clock = Arc::new(ManualClock(AtomicU64::new(NOW)));
    let logs = Arc::new(Mutex::new(Vec::new()));
    let sink = logs.clone();
    let log: LogSink = Arc::new(move |v| sink.lock().unwrap().push(v));
    let mut b = ProxyBuilder::new(Vec::new())
        .resolver(Arc::new(FixedResolver))
        .connector(Arc::new(ToUpstream(up)))
        .clock(clock.clone())
        .log(log);
    if let Some(s) = source {
        b = b.grant_key(s);
    }
    let proxy = b.build();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(proxy.clone().serve(listener));
    H {
        addr,
        proxy,
        clock,
        logs,
    }
}

async fn start() -> H {
    start_with(Some(KeySource::Static(key()))).await
}

fn g(task: &str, iat: u64, nonce: &str, hosts: &[&str]) -> SignedGrant {
    SignedGrant {
        act: Act::Grant,
        task: task.into(),
        hosts: hosts.iter().map(|h| (*h).to_owned()).collect(),
        iat,
        exp: iat + 3_600_000,
        idle: 600_000,
        nonce: nonce.into(),
    }
}

fn revoke(task: &str, iat: u64, nonce: &str) -> SignedGrant {
    SignedGrant {
        act: Act::Revoke,
        hosts: vec![],
        ..g(task, iat, nonce, &[])
    }
}

const N1: &str = "AAAAAAAAAAAAAAAAAAAAAA";
const N2: &str = "BBBBBBBBBBBBBBBBBBBBBA";
const N3: &str = "CCCCCCCCCCCCCCCCCCCCCA";

fn b64(input: &[u8]) -> String {
    const T: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for c in input.chunks(3) {
        let n = (u32::from(c[0]) << 16)
            | (u32::from(*c.get(1).unwrap_or(&0)) << 8)
            | u32::from(*c.get(2).unwrap_or(&0));
        for i in 0..4 {
            if i <= c.len() {
                out.push(T[((n >> (18 - 6 * i)) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

async fn roundtrip(addr: SocketAddr, req: &str) -> String {
    let mut s = TcpStream::connect(addr).await.unwrap();
    s.write_all(req.as_bytes()).await.unwrap();
    let mut out = Vec::new();
    tokio::time::timeout(Duration::from_secs(5), s.read_to_end(&mut out))
        .await
        .expect("proxy did not close")
        .ok();
    String::from_utf8_lossy(&out).into_owned()
}

async fn get(h: &H, host: &str, token: &str) -> String {
    roundtrip(
        h.addr,
        &format!(
            "GET http://{host}/ HTTP/1.1\r\nHost: {host}\r\nProxy-Authorization: Basic {}\r\nConnection: close\r\n\r\n",
            b64(format!("task:{token}").as_bytes())
        ),
    )
    .await
}

async fn post_revoke(h: &H, token: &str) -> String {
    roundtrip(
        h.addr,
        &format!(
            "POST /v1/revoke HTTP/1.1\r\nHost: egress\r\nAuthorization: Bearer {token}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
        ),
    )
    .await
}

fn status(r: &str) -> u16 {
    r.get(9..12).and_then(|s| s.parse().ok()).unwrap_or(0)
}

fn refusal_reasons(h: &H) -> Vec<String> {
    h.logs
        .lock()
        .unwrap()
        .iter()
        .filter(|v| v["event"] == "egress.refused")
        .map(|v| v["reason"].as_str().unwrap_or("").to_owned())
        .collect()
}

fn logs_text(h: &H) -> String {
    serde_json::to_string(&*h.logs.lock().unwrap()).unwrap()
}

#[tokio::test]
async fn valid_grant_allows_exactly_its_hosts() {
    let h = start().await;
    let t = mint(&key(), &g("task-a", NOW, N1, &["registry.example.test"])).unwrap();
    assert_eq!(status(&get(&h, "registry.example.test", &t).await), 200);
    assert_eq!(status(&get(&h, "cdn.registry.example.test", &t).await), 200);
    let r = get(&h, "other.example.test", &t).await;
    assert_eq!(status(&r), 403, "{r}");
    let r = get(&h, "notregistry.example.test", &t).await;
    assert_eq!(status(&r), 403, "{r}");
    // Grants and keys never reach the log.
    assert!(!logs_text(&h).contains(&t));
    assert!(!logs_text(&h).contains(&t[5..40]));
}

#[tokio::test]
async fn forged_tampered_and_wrong_version_are_refused() {
    let h = start().await;
    let real = mint(&key(), &g("task-a", NOW, N1, &["a.example.test"])).unwrap();
    let forged = mint(
        &GrantKey::from_bytes([0x43; 32]),
        &g("task-a", NOW, N1, &["a.example.test"]),
    )
    .unwrap();
    // Swap in another grant's payload under the real tag.
    let other = mint(&key(), &g("task-a", NOW, N1, &["b.example.test"])).unwrap();
    let tampered = format!(
        "{}.{}",
        other.rsplit_once('.').unwrap().0,
        real.rsplit_once('.').unwrap().1
    );
    let v2 = real.replacen("ngr1.", "ngr2.", 1);
    for (tok, reason) in [
        (forged, "grant signature is invalid"),
        (tampered, "grant signature is invalid"),
        (v2, "grant version is not supported"),
        ("ngr1.garbage".to_owned(), "grant is malformed"),
    ] {
        let r = get(&h, "a.example.test", &tok).await;
        assert_eq!(status(&r), 407, "{r}");
        assert!(r.contains(reason), "{r}");
    }
    assert!(h.proxy.sweep() == 0);
    // The good one still works afterwards.
    assert_eq!(status(&get(&h, "a.example.test", &real).await), 200);
}

#[tokio::test]
async fn expiry_absolute_and_idle() {
    let h = start().await;
    let grant = g("task-a", NOW, N1, &["a.example.test"]);
    let t = mint(&key(), &grant).unwrap();
    assert_eq!(status(&get(&h, "a.example.test", &t).await), 200);
    // Idle past the TTL: dropped, and the same token cannot reinstall it (replay).
    h.clock.0.store(NOW + grant.idle + 1, Ordering::SeqCst);
    let r = get(&h, "a.example.test", &t).await;
    assert_eq!(status(&r), 407, "{r}");
    assert!(
        r.contains("no valid task token") || r.contains("retired"),
        "{r}"
    );
    let r = get(&h, "a.example.test", &t).await;
    assert!(r.contains("grant was already retired"), "{r}");
    // Past exp: expired, regardless of ledger state.
    let t2 = mint(&key(), &g("task-b", NOW, N2, &["a.example.test"])).unwrap();
    h.clock.0.store(NOW + 3_600_000, Ordering::SeqCst);
    let r = get(&h, "a.example.test", &t2).await;
    assert!(r.contains("grant has expired"), "{r}");
    // Not yet valid (iat in the future beyond skew).
    let t3 = mint(
        &key(),
        &g("task-c", NOW + 3_600_000 + 120_000, N3, &["a.example.test"]),
    )
    .unwrap();
    let r = get(&h, "a.example.test", &t3).await;
    assert!(r.contains("grant is not valid yet"), "{r}");
}

#[tokio::test]
async fn absolute_expiry_drops_an_installed_grant() {
    let h = start().await;
    let mut grant = g("task-a", NOW, N1, &["a.example.test"]);
    grant.exp = NOW + 1000;
    grant.idle = 1000;
    let t = mint(&key(), &grant).unwrap();
    assert_eq!(status(&get(&h, "a.example.test", &t).await), 200);
    h.clock.0.store(NOW + 999, Ordering::SeqCst);
    assert_eq!(status(&get(&h, "a.example.test", &t).await), 200);
    h.clock.0.store(NOW + 1000, Ordering::SeqCst);
    assert_eq!(h.proxy.sweep(), 1);
    let r = get(&h, "a.example.test", &t).await;
    assert!(r.contains("grant has expired"), "{r}");
}

#[tokio::test]
async fn newer_grant_supersedes_and_older_is_refused() {
    let h = start().await;
    let old = mint(&key(), &g("task-a", NOW, N1, &["a.example.test"])).unwrap();
    let new = mint(&key(), &g("task-a", NOW + 5, N2, &["b.example.test"])).unwrap();
    assert_eq!(status(&get(&h, "a.example.test", &old).await), 200);
    assert_eq!(status(&get(&h, "b.example.test", &new).await), 200);
    let r = get(&h, "a.example.test", &old).await;
    assert_eq!(status(&r), 407, "{r}");
    assert!(r.contains("grant was already retired"), "{r}");
    // The new grant still covers only its own hosts.
    assert_eq!(status(&get(&h, "a.example.test", &new).await), 403);
    // A grant minted before the current one, never seen, is superseded.
    let older = mint(&key(), &g("task-a", NOW + 1, N3, &["a.example.test"])).unwrap();
    let r = get(&h, "a.example.test", &older).await;
    assert!(r.contains("superseded"), "{r}");
}

#[tokio::test]
async fn revoke_endpoint_drops_the_grant_and_blocks_older_ones() {
    let h = start().await;
    let t = mint(&key(), &g("task-a", NOW, N1, &["a.example.test"])).unwrap();
    assert_eq!(status(&get(&h, "a.example.test", &t).await), 200);
    // A grant token is not a revoke; neither is a forged revoke.
    assert_eq!(status(&post_revoke(&h, &t).await), 403);
    let forged = mint(
        &GrantKey::from_bytes([1; 32]),
        &revoke("task-a", NOW + 10, N2),
    )
    .unwrap();
    assert_eq!(status(&post_revoke(&h, &forged).await), 403);
    assert_eq!(status(&get(&h, "a.example.test", &t).await), 200);

    let rv = mint(&key(), &revoke("task-a", NOW + 10, N2)).unwrap();
    assert_eq!(status(&post_revoke(&h, &rv).await), 204);
    let r = get(&h, "a.example.test", &t).await;
    assert_eq!(status(&r), 407, "{r}");
    // Replaying the revoke is harmless.
    assert_eq!(status(&post_revoke(&h, &rv).await), 204);
    // A grant issued before the revoke, never seen, is refused as revoked.
    let before = mint(&key(), &g("task-a", NOW + 5, N3, &["a.example.test"])).unwrap();
    let r = get(&h, "a.example.test", &before).await;
    assert!(r.contains("grant was revoked"), "{r}");
    // A revoke token is not a proxy credential.
    let r = get(&h, "a.example.test", &rv).await;
    assert!(r.contains("token is not a grant"), "{r}");
    assert!(h
        .logs
        .lock()
        .unwrap()
        .iter()
        .any(|v| v["event"] == "egress.revoked" && v["taskId"] == "task-a"));
}

#[tokio::test]
async fn revoke_closes_an_open_tunnel() {
    let h = start().await;
    let t = mint(&key(), &g("task-a", NOW, N1, &["a.example.test"])).unwrap();
    let mut s = TcpStream::connect(h.addr).await.unwrap();
    s.write_all(
        format!(
            "CONNECT a.example.test:443 HTTP/1.1\r\nHost: a.example.test:443\r\nProxy-Authorization: Basic {}\r\n\r\n",
            b64(format!("task:{t}").as_bytes())
        )
        .as_bytes(),
    )
    .await
    .unwrap();
    let mut head = [0u8; 12];
    s.read_exact(&mut head).await.unwrap();
    assert_eq!(&head, b"HTTP/1.1 200");
    let rv = mint(&key(), &revoke("task-a", NOW + 1, N2)).unwrap();
    assert_eq!(status(&post_revoke(&h, &rv).await), 204);
    let mut rest = Vec::new();
    let n = tokio::time::timeout(Duration::from_secs(5), s.read_to_end(&mut rest))
        .await
        .expect("tunnel stayed open after revoke");
    // Closed (EOF or reset) rather than left open: the timeout above is the assertion.
    drop(n);
}

#[tokio::test]
async fn without_a_key_signed_tokens_are_refused() {
    let h = start_with(None).await;
    let t = mint(&key(), &g("task-a", NOW, N1, &["a.example.test"])).unwrap();
    let r = get(&h, "a.example.test", &t).await;
    assert_eq!(status(&r), 407, "{r}");
    assert!(r.contains("signed grants are not enabled"), "{r}");
}

#[tokio::test]
async fn key_file_missing_then_written() {
    let dir = std::env::temp_dir().join(format!("egress-key-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("grant.key");
    let _ = std::fs::remove_file(&path);
    let h = start_with(Some(KeySource::File(path.clone()))).await;
    let t = mint(&key(), &g("task-a", NOW, N1, &["a.example.test"])).unwrap();
    let r = get(&h, "a.example.test", &t).await;
    assert_eq!(status(&r), 503, "{r}");
    assert!(refusal_reasons(&h)
        .iter()
        .any(|r| r == "grant key is not available"));
    // The unavailable-key log names the file, never key material.
    std::fs::write(&path, format!("{}\n", "42".repeat(32))).unwrap();
    // Reload happens at most every few seconds; a fresh proxy reads it at once.
    let h2 = start_with(Some(KeySource::File(path.clone()))).await;
    assert_eq!(status(&get(&h2, "a.example.test", &t).await), 200);
    assert!(!logs_text(&h2).contains(&"42".repeat(32)));
    std::fs::remove_dir_all(&dir).ok();
}
