//! End-to-end tests of the front against a fake Node upstream: route ownership, streaming (SSE
//! flushes per event), client-abort propagation, header and X-Forwarded-For rules, the body cap,
//! the static bundle and /api/ready.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]

use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::body::Incoming;
use hyper::{Request, Response};
use noevia_server::config::Config;
use noevia_server::routes::{Owner, ROUTES};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, oneshot};

type UpBody = http_body_util::combinators::BoxBody<Bytes, std::convert::Infallible>;

/// What the fake Node saw and the knobs a test turns.
#[derive(Default)]
struct Upstream {
    hits: Mutex<Vec<String>>,
    /// The sender side of the current /api/stream response.
    stream: Mutex<Option<http_body_util::channel::Sender<Bytes>>>,
    /// Fires when the client's departure reached Node (a send on the stream failed).
    closed: Mutex<Option<oneshot::Sender<()>>>,
}

async fn upstream_service(
    up: Arc<Upstream>,
    req: Request<Incoming>,
) -> Result<Response<UpBody>, std::convert::Infallible> {
    let path = req.uri().path().to_string();
    let method = req.method().to_string();
    let probe = path == "/api/ready" && method == "GET";
    if !probe {
        up.hits.lock().unwrap().push(format!("{method} {path}"));
    }
    if probe {
        let b = Full::new(Bytes::from(r#"{"ready":true,"version":"node-v1"}"#));
        return Ok(Response::new(b.boxed()));
    }
    if path == "/api/stream" {
        let (tx, body) =
            http_body_util::channel::Channel::<Bytes, std::convert::Infallible>::new(1);
        *up.stream.lock().unwrap() = Some(tx);
        let res = Response::builder()
            .header("content-type", "text/event-stream")
            .header("x-upstream", "1")
            .body(body.boxed())
            .unwrap();
        return Ok(res);
    }
    let headers: HashMap<String, Vec<String>> =
        req.headers().iter().fold(HashMap::new(), |mut m, (k, v)| {
            m.entry(k.to_string())
                .or_default()
                .push(String::from_utf8_lossy(v.as_bytes()).into_owned());
            m
        });
    let body = req.into_body().collect().await.map(|c| c.to_bytes());
    let echo = serde_json::json!({
        "method": method,
        "path": path,
        "headers": headers,
        "bodyLen": body.as_ref().map(|b| b.len()).ok(),
    });
    let res = Response::builder()
        .header("content-type", "application/json")
        .header("x-upstream", "1")
        .header("set-cookie", "a=1; Path=/; HttpOnly")
        .header("set-cookie", "b=2; Path=/")
        .header("connection", "x-hop")
        .header("x-hop", "should-not-pass")
        .body(Full::new(Bytes::from(echo.to_string())).boxed())
        .unwrap();
    Ok(res)
}

async fn start_upstream() -> (Arc<Upstream>, SocketAddr) {
    let up = Arc::new(Upstream::default());
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let up2 = Arc::clone(&up);
    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                return;
            };
            let up3 = Arc::clone(&up2);
            tokio::spawn(async move {
                let svc =
                    hyper::service::service_fn(move |r| upstream_service(Arc::clone(&up3), r));
                let _ = hyper::server::conn::http1::Builder::new()
                    .serve_connection(hyper_util::rt::TokioIo::new(stream), svc)
                    .await;
            });
        }
    });
    (up, addr)
}

fn dist_dir(tag: &str) -> PathBuf {
    let dir =
        std::env::temp_dir().join(format!("noevia-server-front-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("assets")).unwrap();
    std::fs::write(
        dir.join("index.html"),
        format!("<!doctype html><title>noevia</title>{}", "x".repeat(2000)),
    )
    .unwrap();
    std::fs::write(
        dir.join("assets/app-abc123.js"),
        "console.log(1);".repeat(200),
    )
    .unwrap();
    std::fs::write(dir.join("theme.js"), "t()").unwrap();
    std::fs::write(dir.join("version.json"), r#"{"version":"rel-1","web":"w"}"#).unwrap();
    std::fs::write(dir.join("logo.png"), [0x89u8, b'P', b'N', b'G']).unwrap();
    dir
}

async fn start_front(upstream: SocketAddr, dist: &Path, trust_proxy: bool) -> SocketAddr {
    let mut env = HashMap::new();
    env.insert("NOEVIA_LEGACY_UPSTREAM", format!("http://{upstream}"));
    env.insert("UI_PORT", "1".to_string());
    env.insert("NOEVIA_WEB_DIST", dist.display().to_string());
    if trust_proxy {
        env.insert("TRUST_PROXY", "true".to_string());
    }
    let config = Config::from_lookup(|k| env.get(k).cloned()).unwrap();
    let app = noevia_server::App::new(config);
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let svc = noevia_server::router(app).into_make_service_with_connect_info::<SocketAddr>();
    tokio::spawn(async move {
        let _ = axum::serve(listener, svc).await;
    });
    addr
}

struct Client {
    send: hyper::client::conn::http1::SendRequest<UpBody>,
    conn: tokio::task::JoinHandle<()>,
}

impl Drop for Client {
    fn drop(&mut self) {
        self.conn.abort();
    }
}

async fn client(addr: SocketAddr) -> Client {
    let stream = TcpStream::connect(addr).await.unwrap();
    let (send, conn) = hyper::client::conn::http1::handshake(hyper_util::rt::TokioIo::new(stream))
        .await
        .unwrap();
    let conn = tokio::spawn(async move {
        let _ = conn.await;
    });
    Client { send, conn }
}

async fn call(
    addr: SocketAddr,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
    body: &[u8],
) -> (Response<Incoming>, Client) {
    let mut c = client(addr).await;
    let mut b = Request::builder()
        .method(method)
        .uri(path)
        .header("host", "front.test");
    for (k, v) in headers {
        b = b.header(*k, *v);
    }
    let req = b
        .body(Full::new(Bytes::copy_from_slice(body)).boxed())
        .unwrap();
    let res = c.send.send_request(req).await.unwrap();
    (res, c)
}

async fn text(res: Response<Incoming>) -> String {
    String::from_utf8(res.into_body().collect().await.unwrap().to_bytes().to_vec()).unwrap()
}

fn sample(route: &str) -> String {
    let mut out = Vec::new();
    for seg in route.replace("{*?}", "").split('/') {
        out.push(if seg.starts_with('{') {
            "x1".to_string()
        } else {
            seg.to_string()
        });
    }
    out.join("/")
}

#[tokio::test]
async fn every_node_route_reaches_node_and_every_rust_route_does_not() {
    let (up, up_addr) = start_upstream().await;
    let dist = dist_dir("routes");
    let front = start_front(up_addr, &dist, false).await;
    let mut node_routes = 0;
    for r in ROUTES {
        let path = sample(r.path);
        if r.catch_all {
            for p in ["/", "/assets/app-abc123.js", "/c/abc", "/nope"] {
                let (res, _c) = call(front, "GET", p, &[], b"").await;
                assert!(
                    res.headers().get("x-upstream").is_none(),
                    "{p} reached Node"
                );
            }
            continue;
        }
        match r.owner {
            Owner::Rust => {
                let method = r.methods.first().copied().unwrap_or("GET");
                let (res, _c) = call(front, method, &path, &[], b"").await;
                assert!(
                    res.headers().get("x-upstream").is_none(),
                    "{path} is rust-owned but reached Node"
                );
                assert_eq!(res.status(), 200, "{path}");
            }
            Owner::Node => {
                node_routes += 1;
                for method in ["GET", "POST"] {
                    let (res, _c) = call(front, method, &path, &[], b"").await;
                    assert_eq!(
                        res.headers().get("x-upstream").map(|v| v.as_bytes()),
                        Some(&b"1"[..]),
                        "{method} {path} did not reach Node"
                    );
                }
            }
        }
    }
    assert!(node_routes > 100);
    // A rust route's other methods are Node's (Node answers POST /api/ready, HEAD on it, ...).
    for (m, p) in [
        ("POST", "/api/ready"),
        ("HEAD", "/api/ready"),
        ("POST", "/"),
        ("DELETE", "/c/x"),
    ] {
        let (res, _c) = call(front, m, p, &[], b"").await;
        assert!(res.headers().get("x-upstream").is_some(), "{m} {p}");
    }
    let hits = up.hits.lock().unwrap().clone();
    assert!(
        !hits.iter().any(|h| h == "GET /" || h == "GET /api/ready"),
        "{hits:?}"
    );
}

#[tokio::test]
async fn sse_events_flush_one_by_one() {
    let (up, up_addr) = start_upstream().await;
    let front = start_front(up_addr, &dist_dir("sse"), false).await;
    let (res, _c) = call(front, "POST", "/api/stream", &[], b"{}").await;
    assert_eq!(res.headers()["content-type"], "text/event-stream");
    let mut body = res.into_body();
    let mut tx = up.stream.lock().unwrap().take().unwrap();
    for i in 0..3 {
        let event = format!("data: {{\"n\":{i}}}\n\n");
        tx.send_data(Bytes::from(event.clone())).await.unwrap();
        // The event must arrive while Node is still holding the stream open.
        let frame = tokio::time::timeout(Duration::from_secs(3), body.frame())
            .await
            .expect("event was buffered, not flushed")
            .unwrap()
            .unwrap();
        assert_eq!(frame.into_data().unwrap(), Bytes::from(event));
    }
    drop(tx);
    assert!(tokio::time::timeout(Duration::from_secs(3), body.frame())
        .await
        .unwrap()
        .is_none());
}

#[tokio::test]
async fn client_abort_reaches_node() {
    let (up, up_addr) = start_upstream().await;
    let front = start_front(up_addr, &dist_dir("abort"), false).await;
    let (closed_tx, closed_rx) = oneshot::channel();
    *up.closed.lock().unwrap() = Some(closed_tx);
    let (res, c) = call(front, "POST", "/api/stream", &[], b"{}").await;
    let mut body = res.into_body();
    let mut tx = up.stream.lock().unwrap().take().unwrap();
    tx.send_data(Bytes::from_static(b"data: 1\n\n"))
        .await
        .unwrap();
    let _ = body.frame().await;
    // The browser goes away mid-stream.
    drop(body);
    drop(c);
    let up2 = Arc::clone(&up);
    let (done_tx, mut done_rx) = mpsc::channel::<()>(1);
    tokio::spawn(async move {
        // Node keeps writing; once the front has closed its socket a write fails.
        for _ in 0..200 {
            if tx
                .send_data(Bytes::from_static(b"data: more\n\n"))
                .await
                .is_err()
            {
                if let Some(s) = up2.closed.lock().unwrap().take() {
                    let _ = s.send(());
                }
                let _ = done_tx.send(()).await;
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    });
    tokio::time::timeout(Duration::from_secs(5), closed_rx)
        .await
        .expect("Node never saw the client leave")
        .unwrap();
    assert!(done_rx.recv().await.is_some());
}

#[tokio::test]
async fn headers_cookies_and_forwarded_for() {
    let (_up, up_addr) = start_upstream().await;
    let dist = dist_dir("headers");
    for trust in [false, true] {
        let front = start_front(up_addr, &dist, trust).await;
        let (res, _c) = call(
            front,
            "POST",
            "/api/auth/login/password?x=1",
            &[
                ("x-forwarded-for", "6.6.6.6"),
                ("cookie", "cowork_session=s1"),
                ("x-csrf-token", "t1"),
                ("origin", "http://front.test"),
                ("connection", "keep-alive, x-private"),
                ("x-private", "hop"),
            ],
            b"{\"u\":1}",
        )
        .await;
        let cookies: Vec<_> = res
            .headers()
            .get_all("set-cookie")
            .iter()
            .map(|v| v.to_str().unwrap().to_string())
            .collect();
        assert_eq!(cookies, vec!["a=1; Path=/; HttpOnly", "b=2; Path=/"]);
        assert!(res.headers().get("x-hop").is_none());
        let echo: serde_json::Value = serde_json::from_str(&text(res).await).unwrap();
        assert_eq!(echo["path"], "/api/auth/login/password");
        assert_eq!(echo["bodyLen"], 7);
        let h = &echo["headers"];
        assert_eq!(h["host"][0], "front.test");
        assert_eq!(h["cookie"][0], "cowork_session=s1");
        assert_eq!(h["x-csrf-token"][0], "t1");
        assert_eq!(h["origin"][0], "http://front.test");
        assert!(h.get("x-private").is_none());
        if trust {
            // The client's own entry is the rightmost one the front received: that is what
            // Node would have used facing the client; the front sends exactly it.
            assert_eq!(h["x-forwarded-for"], serde_json::json!(["6.6.6.6"]));
        } else {
            assert!(h.get("x-forwarded-for").is_none(), "{h}");
        }
    }
    // With TRUST_PROXY, a spoofed prefix never survives: only the rightmost entry is sent.
    let front = start_front(up_addr, &dist, true).await;
    let (res, _c) = call(
        front,
        "GET",
        "/api/workspace",
        &[("x-forwarded-for", "1.1.1.1, 203.0.113.7")],
        b"",
    )
    .await;
    let echo: serde_json::Value = serde_json::from_str(&text(res).await).unwrap();
    assert_eq!(
        echo["headers"]["x-forwarded-for"],
        serde_json::json!(["203.0.113.7"])
    );
    let (res, _c) = call(front, "GET", "/api/workspace", &[], b"").await;
    let echo: serde_json::Value = serde_json::from_str(&text(res).await).unwrap();
    assert_eq!(
        echo["headers"]["x-forwarded-for"],
        serde_json::json!(["127.0.0.1"])
    );
}

#[tokio::test]
async fn request_body_cap() {
    let (up, up_addr) = start_upstream().await;
    let front = start_front(up_addr, &dist_dir("cap"), false).await;
    let cap = noevia_server::legacy_proxy::BODY_CAP;
    // Declared too large: refused without touching Node.
    let (res, _c) = call(
        front,
        "POST",
        "/api/chat",
        &[("content-length", &(cap + 1).to_string())],
        &[],
    )
    .await;
    assert_eq!(res.status(), 413);
    assert_eq!(res.headers()["x-noevia-api"], "1");
    assert_eq!(text(res).await, r#"{"error":"Request exceeds size limit"}"#);
    assert!(up.hits.lock().unwrap().is_empty());
    // Streamed (chunked) past the cap: cut.
    let mut c = client(front).await;
    let (mut tx, body) =
        http_body_util::channel::Channel::<Bytes, std::convert::Infallible>::new(4);
    let req = Request::post("/api/chat")
        .header("host", "front.test")
        .body(body.boxed())
        .unwrap();
    let pending = tokio::spawn(async move { c.send.send_request(req).await.map(|r| r.status()) });
    let chunk = Bytes::from(vec![b'a'; 1024 * 1024]);
    for _ in 0..(cap / (1024 * 1024) + 2) {
        if tx.send_data(chunk.clone()).await.is_err() {
            break;
        }
    }
    drop(tx);
    let status = tokio::time::timeout(Duration::from_secs(20), pending)
        .await
        .unwrap()
        .unwrap();
    assert!(
        matches!(status, Ok(s) if s == 413) || status.is_err(),
        "{status:?}"
    );
    // Exactly the cap passes.
    let (res, _c) = call(front, "POST", "/api/import", &[], &vec![b'b'; cap as usize]).await;
    assert_eq!(res.status(), 200);
    let echo: serde_json::Value = serde_json::from_str(&text(res).await).unwrap();
    assert_eq!(echo["bodyLen"], cap);
}

#[tokio::test]
async fn node_down_is_a_502_and_ready_is_a_503() {
    // A port nothing listens on.
    let free = TcpListener::bind("127.0.0.1:0")
        .await
        .unwrap()
        .local_addr()
        .unwrap();
    let dist = dist_dir("down");
    std::fs::remove_file(dist.join("version.json")).unwrap();
    let front = start_front(free, &dist, false).await;
    let (res, _c) = call(front, "GET", "/api/workspace", &[], b"").await;
    assert_eq!(res.status(), 502);
    let (res, _c) = call(front, "GET", "/api/ready", &[], b"").await;
    assert_eq!(res.status(), 503);
    assert_eq!(res.headers()["x-noevia-api"], "1");
    assert_eq!(text(res).await, r#"{"ready":false,"version":"unknown"}"#);
    // The bundle is still served.
    let (res, _c) = call(front, "GET", "/c/x", &[], b"").await;
    assert_eq!(res.status(), 200);
}

#[tokio::test]
async fn ready_reports_node_readiness_and_the_served_version() {
    let (_up, up_addr) = start_upstream().await;
    let dist = dist_dir("ready");
    let front = start_front(up_addr, &dist, false).await;
    let (res, _c) = call(front, "GET", "/api/ready", &[], b"").await;
    let h = res.headers().clone();
    assert_eq!(text(res).await, r#"{"ready":true,"version":"rel-1"}"#);
    for (k, v) in [
        ("content-type", "application/json"),
        ("cache-control", "no-store"),
        ("x-noevia-api", "1"),
        ("x-content-type-options", "nosniff"),
        ("referrer-policy", "same-origin"),
        ("x-frame-options", "DENY"),
        ("content-security-policy", noevia_server::reply::CSP),
    ] {
        assert_eq!(h[k], v, "{k}");
    }
    std::fs::remove_file(dist.join("version.json")).unwrap();
    let front = start_front(up_addr, &dist, false).await;
    let (res, _c) = call(front, "GET", "/api/ready", &[], b"").await;
    assert_eq!(text(res).await, r#"{"ready":true,"version":"node-v1"}"#);
}

#[tokio::test]
async fn static_bundle_like_static_files_cjs() {
    let (up, up_addr) = start_upstream().await;
    let dist = dist_dir("static");
    let front = start_front(up_addr, &dist, false).await;

    let (res, _c) = call(
        front,
        "GET",
        "/assets/app-abc123.js",
        &[("accept-encoding", "gzip, br")],
        b"",
    )
    .await;
    assert_eq!(res.status(), 200);
    let h = res.headers().clone();
    assert_eq!(h["cache-control"], "public, max-age=31536000, immutable");
    assert_eq!(h["content-type"], "text/javascript; charset=utf-8");
    assert_eq!(h["content-encoding"], "br");
    assert_eq!(h["vary"], "Accept-Encoding");
    assert_eq!(h["x-frame-options"], "DENY");
    assert!(h.get("x-noevia-api").is_none());
    let etag = h["etag"].to_str().unwrap().to_string();
    assert!(etag.ends_with("-br\"") && etag.len() == 27 + 5, "{etag}");
    let br = res.into_body().collect().await.unwrap().to_bytes();
    let mut plain = Vec::new();
    brotli::BrotliDecompress(&mut &br[..], &mut plain).unwrap();
    assert_eq!(plain, "console.log(1);".repeat(200).into_bytes());

    // Revalidation, including a weak tag and a list.
    let (res, _c) = call(
        front,
        "GET",
        "/assets/app-abc123.js",
        &[
            ("accept-encoding", "br"),
            ("if-none-match", &format!("\"zz\", W/{etag}")),
        ],
        b"",
    )
    .await;
    assert_eq!(res.status(), 304);
    assert_eq!(res.headers()["etag"], etag.as_str());

    let (res, _c) = call(
        front,
        "GET",
        "/assets/app-abc123.js",
        &[("accept-encoding", "br;q=0, gzip")],
        b"",
    )
    .await;
    assert_eq!(res.headers()["content-encoding"], "gzip");
    let gz = res.into_body().collect().await.unwrap().to_bytes();
    let mut plain = String::new();
    std::io::Read::read_to_string(&mut flate2::read::GzDecoder::new(&gz[..]), &mut plain).unwrap();
    assert_eq!(plain, "console.log(1);".repeat(200));

    let (res, _c) = call(front, "GET", "/theme.js", &[("accept-encoding", "br")], b"").await;
    assert_eq!(res.headers()["cache-control"], "no-cache");
    assert!(res.headers().get("content-encoding").is_none());
    assert!(res.headers().get("vary").is_none());
    assert_eq!(text(res).await, "t()");

    let (res, _c) = call(front, "GET", "/version.json", &[], b"").await;
    assert_eq!(res.headers()["cache-control"], "no-store");
    assert_eq!(res.headers()["content-type"], "application/json");

    let (res, _c) = call(front, "GET", "/logo.png", &[], b"").await;
    assert_eq!(res.headers()["content-type"], "image/png");

    // The shell, for / and for a client place, never cached.
    for p in ["/", "/c/abc", "/settings/models", "/p/x/chats"] {
        let (res, _c) = call(front, "GET", p, &[], b"").await;
        assert_eq!(res.status(), 200, "{p}");
        assert_eq!(res.headers()["cache-control"], "no-store", "{p}");
        assert_eq!(res.headers()["content-type"], "text/html; charset=utf-8");
        assert!(text(res).await.starts_with("<!doctype html>"));
    }
    let (res, _c) = call(front, "HEAD", "/", &[], b"").await;
    assert_eq!(res.status(), 200);
    assert!(res.headers().get("content-length").is_some());
    assert!(res
        .into_body()
        .collect()
        .await
        .unwrap()
        .to_bytes()
        .is_empty());

    // Not a file, not a place: JSON 404 (with the security headers, without X-Noevia-API).
    for p in ["/nope", "/assets/missing.js", "/assets/", "/index.html/"] {
        let (res, _c) = call(front, "GET", p, &[], b"").await;
        assert_eq!(res.status(), 404, "{p}");
        assert_eq!(res.headers()["cache-control"], "no-store");
        assert_eq!(
            res.headers()["content-security-policy"],
            noevia_server::reply::CSP
        );
        assert_eq!(text(res).await, r#"{"error":"not found"}"#);
    }
    assert!(up.hits.lock().unwrap().is_empty());

    // Node's own pages, and anything Node's URL parsing might read differently, go to Node.
    for p in [
        "/about",
        "/privacy",
        "/device",
        "/.well-known/webauthn",
        "/a/%2e%2e/index.html",
        "/a//b",
    ] {
        let (res, _c) = call(front, "GET", p, &[], b"").await;
        assert!(res.headers().get("x-upstream").is_some(), "{p}");
    }
    // A changed file is picked up (cache keyed on mtime and size).
    std::fs::write(dist.join("theme.js"), "t2()").unwrap();
    let (res, _c) = call(front, "GET", "/theme.js", &[], b"").await;
    assert_eq!(text(res).await, "t2()");
}

#[tokio::test]
async fn missing_bundle_is_a_read_error_like_node() {
    let (_up, up_addr) = start_upstream().await;
    let dist =
        std::env::temp_dir().join(format!("noevia-server-front-none-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dist);
    let front = start_front(up_addr, &dist, false).await;
    let (res, _c) = call(front, "GET", "/c/x", &[], b"").await;
    assert_eq!(res.status(), 500);
    assert_eq!(text(res).await, r#"{"error":"read error"}"#);
    let (res, _c) = call(front, "GET", "/nope", &[], b"").await;
    assert_eq!(res.status(), 404);
}
