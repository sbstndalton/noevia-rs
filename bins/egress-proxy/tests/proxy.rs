//! End-to-end behaviour of the egress proxy against local synthetic upstreams: deny by
//! default, private-address refusal, DNS-rebinding refusal (connect only to the checked
//! address), header stripping, idle-TTL expiry, limits and upstream failure.
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

use egress::Grant;
use egress_proxy::{
    parse_grants, BoxFuture, Clock, Connector, Limits, LogSink, Proxy, ProxyBuilder, Resolver,
};
use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

const TOKEN: &str = "tok-synthetic-0123456789abcdef";
const PUBLIC: &str = "93.184.216.34";

/// Answers from a script: call n gets `answers[min(n, last)]`.
struct ScriptedResolver {
    answers: Vec<Vec<IpAddr>>,
    calls: Mutex<Vec<String>>,
}
impl Resolver for ScriptedResolver {
    fn resolve<'a>(&'a self, host: &'a str) -> BoxFuture<'a, io::Result<Vec<IpAddr>>> {
        let mut calls = self.calls.lock().unwrap();
        let n = calls.len().min(self.answers.len().saturating_sub(1));
        calls.push(host.to_owned());
        let out = self.answers.get(n).cloned().unwrap_or_default();
        Box::pin(async move { Ok(out) })
    }
}

/// Records the address the proxy asked for, then connects to a local upstream instead.
struct RecordingConnector {
    upstream: Option<SocketAddr>,
    asked: Mutex<Vec<SocketAddr>>,
}
impl Connector for RecordingConnector {
    fn connect(&self, addr: SocketAddr) -> BoxFuture<'_, io::Result<TcpStream>> {
        self.asked.lock().unwrap().push(addr);
        let up = self.upstream;
        Box::pin(async move {
            match up {
                Some(a) => TcpStream::connect(a).await,
                None => Err(io::Error::new(
                    io::ErrorKind::ConnectionRefused,
                    "synthetic",
                )),
            }
        })
    }
}

#[derive(Default)]
struct ManualClock(AtomicU64);
impl Clock for ManualClock {
    fn now_ms(&self) -> u64 {
        self.0.load(Ordering::SeqCst)
    }
}

struct Harness {
    addr: SocketAddr,
    proxy: Arc<Proxy>,
    resolver: Arc<ScriptedResolver>,
    connector: Arc<RecordingConnector>,
    clock: Arc<ManualClock>,
    logs: Arc<Mutex<Vec<Value>>>,
}

fn grant(domains: &[&str], ttl: u64) -> Grant {
    Grant {
        token: TOKEN.into(),
        task_id: "task-synthetic-1".into(),
        domains: domains.iter().map(|d| (*d).to_owned()).collect(),
        idle_ttl_ms: ttl,
    }
}

async fn start(
    grants: Vec<Grant>,
    answers: Vec<Vec<IpAddr>>,
    upstream: Option<SocketAddr>,
    limits: Limits,
) -> Harness {
    let resolver = Arc::new(ScriptedResolver {
        answers,
        calls: Mutex::default(),
    });
    let connector = Arc::new(RecordingConnector {
        upstream,
        asked: Mutex::default(),
    });
    let clock = Arc::new(ManualClock::default());
    let logs = Arc::new(Mutex::new(Vec::new()));
    let sink_logs = logs.clone();
    let log: LogSink = Arc::new(move |v| sink_logs.lock().unwrap().push(v));
    let proxy = ProxyBuilder::new(grants)
        .resolver(resolver.clone())
        .connector(connector.clone())
        .clock(clock.clone())
        .log(log)
        .limits(limits)
        .build();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(proxy.clone().serve(listener));
    Harness {
        addr,
        proxy,
        resolver,
        connector,
        clock,
        logs,
    }
}

fn ip(s: &str) -> IpAddr {
    s.parse().unwrap()
}

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

fn auth(token: &str) -> String {
    format!(
        "Proxy-Authorization: Basic {}\r\n",
        b64(format!("task:{token}").as_bytes())
    )
}

/// Sends `req`, reads until the proxy closes.
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

/// Reads a response head (through the blank line).
async fn read_head(s: &mut TcpStream) -> String {
    let mut head = Vec::new();
    let mut b = [0u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        let n = tokio::time::timeout(Duration::from_secs(5), s.read(&mut b))
            .await
            .unwrap()
            .unwrap();
        if n == 0 {
            break;
        }
        head.push(b[0]);
    }
    String::from_utf8_lossy(&head).into_owned()
}

/// An HTTP upstream that records each request head and answers `ok`.
async fn http_upstream() -> (SocketAddr, Arc<Mutex<Vec<String>>>) {
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let seen2 = seen.clone();
    tokio::spawn(async move {
        loop {
            let (mut s, _) = l.accept().await.unwrap();
            let seen = seen2.clone();
            tokio::spawn(async move {
                let head = read_head(&mut s).await;
                seen.lock().unwrap().push(head);
                s.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\nX-Upstream: 1\r\n\r\nok")
                    .await
                    .ok();
                s.shutdown().await.ok();
            });
        }
    });
    (addr, seen)
}

/// A TCP echo upstream.
async fn echo_upstream() -> SocketAddr {
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let (mut s, _) = l.accept().await.unwrap();
            tokio::spawn(async move {
                let (mut r, mut w) = s.split();
                tokio::io::copy(&mut r, &mut w).await.ok();
            });
        }
    });
    addr
}

fn refusals(h: &Harness) -> Vec<Value> {
    h.logs
        .lock()
        .unwrap()
        .iter()
        .filter(|v| v["event"] == "egress.refused")
        .cloned()
        .collect()
}

#[tokio::test]
async fn deny_by_default_without_a_valid_token() {
    let (up, seen) = http_upstream().await;
    let h = start(
        vec![grant(&["allowed.test"], 60_000)],
        vec![vec![ip(PUBLIC)]],
        Some(up),
        Limits::default(),
    )
    .await;

    let r = roundtrip(
        h.addr,
        "GET http://allowed.test/ HTTP/1.1\r\nHost: allowed.test\r\nConnection: close\r\n\r\n",
    )
    .await;
    assert!(
        r.starts_with("HTTP/1.1 407 Proxy Authentication Required\r\n"),
        "{r}"
    );
    assert!(r
        .to_ascii_lowercase()
        .contains("proxy-authenticate: basic realm=\"noevia task\""));
    assert!(r.ends_with("Refused: no valid task token\n"), "{r}");

    let wrong = auth("tok-synthetic-0123456789abcdeX");
    let r = roundtrip(
        h.addr,
        &format!("GET http://allowed.test/ HTTP/1.1\r\nHost: allowed.test\r\n{wrong}Connection: close\r\n\r\n"),
    )
    .await;
    assert!(r.starts_with("HTTP/1.1 407 "), "{r}");

    let r = roundtrip(
        h.addr,
        "CONNECT allowed.test:443 HTTP/1.1\r\nHost: allowed.test:443\r\n\r\n",
    )
    .await;
    assert!(
        r.starts_with("HTTP/1.1 407 Proxy Authentication Required\r\n"),
        "{r}"
    );
    assert!(r.to_ascii_lowercase().contains("content-length: 0"), "{r}");

    // Bearer and garbage are not Basic.
    let r = roundtrip(
        h.addr,
        &format!(
            "CONNECT allowed.test:443 HTTP/1.1\r\nProxy-Authorization: Bearer {TOKEN}\r\n\r\n"
        ),
    )
    .await;
    assert!(r.starts_with("HTTP/1.1 407 "), "{r}");

    assert!(h.resolver.calls.lock().unwrap().is_empty());
    assert!(h.connector.asked.lock().unwrap().is_empty());
    assert!(seen.lock().unwrap().is_empty());
    let logs = h.logs.lock().unwrap();
    assert!(
        logs.iter().all(|v| !v.to_string().contains(TOKEN)),
        "token leaked into logs"
    );
    assert_eq!(refusals_count(&logs, 407), 4);
}

fn refusals_count(logs: &[Value], status: u16) -> usize {
    logs.iter()
        .filter(|v| v["event"] == "egress.refused" && v["status"] == status)
        .count()
}

#[tokio::test]
async fn hosts_and_ports_off_the_grant_are_forbidden() {
    let h = start(
        vec![grant(&["allowed.test"], 60_000)],
        vec![vec![ip(PUBLIC)]],
        None,
        Limits::default(),
    )
    .await;
    let a = auth(TOKEN);
    for (target, reason) in [
        (
            "notallowed.test:443",
            "host is not on this task\u{2019}s list",
        ),
        (
            "allowed.test.evil.test:443",
            "host is not on this task\u{2019}s list",
        ),
        ("allowed.test:22", "port 22 is not allowed"),
        ("allowed.test:8443", "port 8443 is not allowed"),
    ] {
        let r = roundtrip(h.addr, &format!("CONNECT {target} HTTP/1.1\r\n{a}\r\n")).await;
        assert!(r.starts_with("HTTP/1.1 403 Forbidden\r\n"), "{target}: {r}");
        let last = refusals(&h).pop().unwrap();
        assert_eq!(last["reason"], reason);
        assert_eq!(last["taskId"], "task-synthetic-1");
    }
    let r = roundtrip(
        h.addr,
        &format!(
            "GET http://evil.test/ HTTP/1.1\r\nHost: evil.test\r\n{a}Connection: close\r\n\r\n"
        ),
    )
    .await;
    assert!(r.starts_with("HTTP/1.1 403 Forbidden\r\n"), "{r}");
    assert!(
        r.ends_with("Refused: host is not on this task\u{2019}s list\n"),
        "{r}"
    );
    assert!(h.resolver.calls.lock().unwrap().is_empty());
    assert!(h.connector.asked.lock().unwrap().is_empty());
}

#[tokio::test]
async fn private_addresses_are_refused() {
    // Every answer must be public: one private record among public ones refuses the host.
    let h = start(
        vec![grant(
            &["mixed.test", "127.0.0.1", "::ffff:127.0.0.1", "nx.test"],
            60_000,
        )],
        vec![vec![ip(PUBLIC), ip("10.0.0.7")], vec![]],
        None,
        Limits::default(),
    )
    .await;
    let a = auth(TOKEN);
    let r = roundtrip(
        h.addr,
        &format!("CONNECT mixed.test:443 HTTP/1.1\r\n{a}\r\n"),
    )
    .await;
    assert!(r.starts_with("HTTP/1.1 403 Forbidden\r\n"), "{r}");
    assert_eq!(
        refusals(&h).pop().unwrap()["reason"],
        "host resolves to a private address"
    );

    // IP literals are judged as written and never resolved.
    for target in ["127.0.0.1:443", "[::ffff:127.0.0.1]:443"] {
        let r = roundtrip(h.addr, &format!("CONNECT {target} HTTP/1.1\r\n{a}\r\n")).await;
        assert!(r.starts_with("HTTP/1.1 403 Forbidden\r\n"), "{target}: {r}");
    }
    let r = roundtrip(
        h.addr,
        &format!(
            "GET http://127.0.0.1/ HTTP/1.1\r\nHost: 127.0.0.1\r\n{a}Connection: close\r\n\r\n"
        ),
    )
    .await;
    assert!(
        r.ends_with("Refused: host resolves to a private address\n"),
        "{r}"
    );

    // No answers at all is a 502.
    let r = roundtrip(h.addr, &format!("CONNECT nx.test:443 HTTP/1.1\r\n{a}\r\n")).await;
    assert!(r.starts_with("HTTP/1.1 502 Bad Gateway\r\n"), "{r}");
    assert_eq!(
        refusals(&h).pop().unwrap()["reason"],
        "host does not resolve"
    );

    assert_eq!(
        *h.resolver.calls.lock().unwrap(),
        vec!["mixed.test".to_owned(), "nx.test".to_owned()]
    );
    assert!(h.connector.asked.lock().unwrap().is_empty());
}

#[tokio::test]
async fn rebinding_connects_only_to_the_checked_address() {
    // First lookup is public, every later one private (a rebinding name).
    let echo = echo_upstream().await;
    let h = start(
        vec![grant(&["rebind.test"], 60_000)],
        vec![vec![ip(PUBLIC), ip("151.101.1.1")], vec![ip("127.0.0.1")]],
        Some(echo),
        Limits::default(),
    )
    .await;
    let a = auth(TOKEN);
    let mut s = TcpStream::connect(h.addr).await.unwrap();
    s.write_all(format!("CONNECT rebind.test:443 HTTP/1.1\r\n{a}\r\n").as_bytes())
        .await
        .unwrap();
    let head = read_head(&mut s).await;
    assert!(
        head.starts_with("HTTP/1.1 200 Connection Established\r\n"),
        "{head}"
    );
    s.write_all(b"ping").await.unwrap();
    let mut buf = [0u8; 4];
    tokio::time::timeout(Duration::from_secs(5), s.read_exact(&mut buf))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(&buf, b"ping");
    // One lookup; the connection went to the first checked answer, not a fresh resolution.
    assert_eq!(h.resolver.calls.lock().unwrap().len(), 1);
    assert_eq!(
        *h.connector.asked.lock().unwrap(),
        vec![SocketAddr::new(ip(PUBLIC), 443)]
    );
    drop(s);

    // The next request resolves again, sees the private answer, and is refused.
    let r = roundtrip(
        h.addr,
        &format!("CONNECT rebind.test:443 HTTP/1.1\r\n{a}\r\n"),
    )
    .await;
    assert!(r.starts_with("HTTP/1.1 403 Forbidden\r\n"), "{r}");
    assert_eq!(h.connector.asked.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn proxy_credentials_and_hop_by_hop_headers_never_travel_upstream() {
    let (up, seen) = http_upstream().await;
    let h = start(
        vec![grant(&["allowed.test"], 60_000)],
        vec![vec![ip(PUBLIC)]],
        Some(up),
        Limits::default(),
    )
    .await;
    let a = auth(TOKEN);
    let r = roundtrip(
        h.addr,
        &format!(
            "GET http://Allowed.test/path?q=1 HTTP/1.1\r\nHost: wrong.test\r\n{a}\
             Proxy-Connection: keep-alive\r\nConnection: close, X-Strip-Me\r\nX-Strip-Me: 1\r\n\
             Keep-Alive: timeout=5\r\nTE: trailers\r\nX-Keep: yes\r\n\r\n"
        ),
    )
    .await;
    assert!(r.starts_with("HTTP/1.1 200 OK\r\n"), "{r}");
    assert!(r.ends_with("ok"), "{r}");
    assert!(r.to_ascii_lowercase().contains("x-upstream: 1"));
    let seen = seen.lock().unwrap();
    assert_eq!(seen.len(), 1);
    let head = seen[0].to_ascii_lowercase();
    assert!(head.starts_with("get /path?q=1 http/1.1\r\n"), "{head}");
    assert!(head.contains("\r\nhost: allowed.test\r\n"), "{head}");
    assert!(head.contains("\r\nx-keep: yes\r\n"), "{head}");
    for gone in [
        "proxy-authorization",
        "proxy-connection",
        "x-strip-me",
        "keep-alive:",
        "\r\nte:",
        "basic ",
    ] {
        assert!(!head.contains(gone), "{gone} reached upstream: {head}");
    }
    assert!(!seen[0].contains(&b64(format!("task:{TOKEN}").as_bytes())));
    assert_eq!(
        *h.connector.asked.lock().unwrap(),
        vec![SocketAddr::new(ip(PUBLIC), 80)]
    );
    let allowed: Vec<Value> = h
        .logs
        .lock()
        .unwrap()
        .iter()
        .filter(|v| v["event"] == "egress.allowed")
        .cloned()
        .collect();
    assert_eq!(allowed.len(), 1);
    assert_eq!(allowed[0]["method"], "GET");
    assert_eq!(allowed[0]["host"], "allowed.test");
}

#[tokio::test]
async fn idle_tokens_expire_and_take_their_tunnels_with_them() {
    let echo = echo_upstream().await;
    let h = start(
        vec![grant(&["allowed.test"], 1_000)],
        vec![vec![ip(PUBLIC)]],
        Some(echo),
        Limits::default(),
    )
    .await;
    let a = auth(TOKEN);
    let mut s = TcpStream::connect(h.addr).await.unwrap();
    s.write_all(format!("CONNECT allowed.test:443 HTTP/1.1\r\n{a}\r\n").as_bytes())
        .await
        .unwrap();
    assert!(read_head(&mut s).await.starts_with("HTTP/1.1 200 "));

    // Traffic at t=500 refreshes the idle clock; at t=1400 the grant is idle 900 ms: alive.
    h.clock.0.store(500, Ordering::SeqCst);
    s.write_all(b"ping").await.unwrap();
    let mut buf = [0u8; 4];
    s.read_exact(&mut buf).await.unwrap();
    h.clock.0.store(1_400, Ordering::SeqCst);
    assert_eq!(h.proxy.sweep(), 0);

    // Idle past the TTL: swept, and the live tunnel is closed.
    h.clock.0.store(2_600, Ordering::SeqCst);
    assert_eq!(h.proxy.sweep(), 1);
    let mut rest = Vec::new();
    let n = tokio::time::timeout(Duration::from_secs(5), s.read_to_end(&mut rest))
        .await
        .expect("tunnel outlived its grant");
    assert!(n.is_err() || rest.is_empty());
    assert!(h
        .logs
        .lock()
        .unwrap()
        .iter()
        .any(|v| v["event"] == "egress.expired" && v["taskId"] == "task-synthetic-1"));

    // The token is dead for good.
    let r = roundtrip(
        h.addr,
        &format!("CONNECT allowed.test:443 HTTP/1.1\r\n{a}\r\n"),
    )
    .await;
    assert!(r.starts_with("HTTP/1.1 407 "), "{r}");
}

#[tokio::test]
async fn an_idle_token_is_refused_even_before_the_sweeper_runs() {
    let h = start(
        vec![grant(&["allowed.test"], 1_000)],
        vec![vec![ip(PUBLIC)]],
        None,
        Limits::default(),
    )
    .await;
    h.clock.0.store(1_001, Ordering::SeqCst);
    let a = auth(TOKEN);
    let r = roundtrip(
        h.addr,
        &format!("CONNECT allowed.test:443 HTTP/1.1\r\n{a}\r\n"),
    )
    .await;
    assert!(r.starts_with("HTTP/1.1 407 "), "{r}");
}

#[tokio::test]
async fn malformed_and_unsupported_requests_are_bad_requests() {
    let h = start(
        vec![grant(&["allowed.test"], 60_000)],
        vec![vec![ip(PUBLIC)]],
        None,
        Limits::default(),
    )
    .await;
    let a = auth(TOKEN);
    // Origin-form is not a proxy request.
    let r = roundtrip(
        h.addr,
        &format!("GET /x HTTP/1.1\r\nHost: allowed.test\r\n{a}Connection: close\r\n\r\n"),
    )
    .await;
    assert!(r.starts_with("HTTP/1.1 400 Bad Request\r\n"), "{r}");
    assert!(r.ends_with("Refused: unreadable target\n"), "{r}");
    // Absolute https:// is refused rather than downgraded to plaintext.
    let r = roundtrip(h.addr, &format!("GET https://allowed.test/ HTTP/1.1\r\nHost: allowed.test\r\n{a}Connection: close\r\n\r\n")).await;
    assert!(r.starts_with("HTTP/1.1 400 Bad Request\r\n"), "{r}");
    assert!(
        r.ends_with("Refused: unsupported protocol \"https:\"\n"),
        "{r}"
    );
    // Garbage never reaches the policy.
    let r = roundtrip(h.addr, "NOT A REQUEST\r\n\r\n").await;
    assert!(r.starts_with("HTTP/1.1 400 "), "{r}");
    // Oversized heads are cut off.
    let big = "a".repeat(40 * 1024);
    let r = roundtrip(
        h.addr,
        &format!("GET http://allowed.test/ HTTP/1.1\r\nX-Big: {big}\r\n{a}\r\n"),
    )
    .await;
    assert!(r.starts_with("HTTP/1.1 431 "), "{r}");
    assert!(h.connector.asked.lock().unwrap().is_empty());
}

#[tokio::test]
async fn upstream_failure_is_a_bad_gateway() {
    let h = start(
        vec![grant(&["allowed.test"], 60_000)],
        vec![vec![ip(PUBLIC)]],
        None,
        Limits::default(),
    )
    .await;
    let a = auth(TOKEN);
    let r = roundtrip(
        h.addr,
        &format!("CONNECT allowed.test:443 HTTP/1.1\r\n{a}\r\n"),
    )
    .await;
    assert!(r.starts_with("HTTP/1.1 502 Bad Gateway\r\n"), "{r}");
    let r = roundtrip(
        h.addr,
        &format!("GET http://allowed.test/ HTTP/1.1\r\n{a}Connection: close\r\n\r\n"),
    )
    .await;
    assert!(r.starts_with("HTTP/1.1 502 Bad Gateway\r\n"), "{r}");
    assert!(r.ends_with("Upstream failed\n"), "{r}");
}

#[tokio::test]
async fn connections_beyond_the_limit_are_closed() {
    let h = start(
        vec![grant(&["allowed.test"], 60_000)],
        vec![vec![ip(PUBLIC)]],
        None,
        Limits {
            max_connections: 1,
            ..Limits::default()
        },
    )
    .await;
    let _held = TcpStream::connect(h.addr).await.unwrap();
    // Once the first connection holds the only slot, the next is closed without a reply.
    let limited = || {
        h.logs
            .lock()
            .unwrap()
            .iter()
            .any(|v| v["event"] == "egress.connection_limit")
    };
    for _ in 0..50 {
        let r = roundtrip(h.addr, "CONNECT allowed.test:443 HTTP/1.1\r\n\r\n").await;
        if limited() {
            assert!(r.is_empty(), "{r}");
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(limited());
}

#[test]
fn grants_file_is_strict() {
    let ok = parse_grants(
        r#"[{"token":"tok-synthetic-0123456789","task":"t1","domains":["a.test"],"expiresIdleMs":5000},
            {"token":"tok-synthetic-abcdefghij","task":"t2","domains":[]}]"#,
    )
    .unwrap();
    assert_eq!(ok.len(), 2);
    assert_eq!(ok[0].idle_ttl_ms, 5000);
    assert_eq!(ok[1].idle_ttl_ms, egress::DEFAULT_TOKEN_TTL_MS);
    let single =
        parse_grants(r#"{"token":"tok-synthetic-0123456789","task":"t1","domains":["a.test"]}"#)
            .unwrap();
    assert_eq!(single[0].domains, vec!["a.test".to_owned()]);
    for bad in [
        r#"{"token":"short","task":"t1","domains":[]}"#,
        r#"{"token":"tok-synthetic-0123456789","task":"","domains":[]}"#,
        r#"{"token":"tok-synthetic-0123456789","task":"t1","domain":["a.test"]}"#,
        r#"{"token":"tok-synthetic-0123456789","task":"t1","domains":[1]}"#,
        r#"{"token":"tok-synthetic-0123456789","task":"t1","domains":[],"expiresIdleMs":0}"#,
        r#"{"token":"tok-synthetic-0123456789","task":"t1","domains":[],"expiresIdleMs":-5}"#,
        r#"[{"token":"tok-synthetic-0123456789","task":"t1","domains":[]},{"token":"tok-synthetic-0123456789","task":"t2","domains":[]}]"#,
        r#"[{"token":"tok-synthetic-0123456789","task":"t1","domains":[]},{"token":"tok-synthetic-abcdefghij","task":"t1","domains":[]}]"#,
        r#""just a string""#,
        "not json",
    ] {
        assert!(parse_grants(bad).is_err(), "accepted {bad}");
    }
}
