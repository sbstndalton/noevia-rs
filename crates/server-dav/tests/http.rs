use axum::body::Body;
use axum::http::{Request, Response};
use hyper::body::Incoming;
use hyper::service::service_fn;
use hyper_util::rt::TokioIo;
use server_dav::{
    Backend, BackendFuture, Capabilities, Config, DavService, DenyAll, Entry, Fault, File,
    Identity, Scope, WriteCondition,
};
use std::collections::HashMap;
use std::convert::Infallible;
use std::io;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinHandle;

type TestResult = Result<(), Box<dyn std::error::Error + Send + Sync>>;
const A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

#[derive(Default)]
struct State {
    items: HashMap<String, String>,
    allowed: bool,
    slow: bool,
    slow_write: bool,
    fail_read: bool,
    children: usize,
    mkdir: bool,
}
struct Synthetic(Mutex<State>, tokio::sync::Notify);
impl Synthetic {
    fn new() -> Arc<Self> {
        let mut items = HashMap::new();
        items.insert("entry.md".into(), "Synthetic alice".into());
        Arc::new(Self(
            Mutex::new(State {
                items,
                allowed: true,
                ..State::default()
            }),
            tokio::sync::Notify::new(),
        ))
    }
    fn change(&self, f: impl FnOnce(&mut State)) {
        if let Ok(mut state) = self.0.lock() {
            f(&mut state);
        }
    }
}
impl Backend for Synthetic {
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            mkdir: self.0.lock().is_ok_and(|s| s.mkdir),
        }
    }
    fn authenticate<'a>(
        &'a self,
        user: &'a str,
        password: &'a str,
        scope: Scope,
    ) -> BackendFuture<'a, Result<Option<Identity>, Fault>> {
        Box::pin(async move {
            Ok(
                (user == "alice" && password == "synthetic" && scope == Scope::Lan).then(|| {
                    Identity {
                        user_id: "alice".into(),
                        credential_id: "device".into(),
                    }
                }),
            )
        })
    }
    fn allowed<'a>(
        &'a self,
        identity: &'a Identity,
        scope: Scope,
    ) -> BackendFuture<'a, Result<bool, Fault>> {
        Box::pin(async move {
            let state = self.0.lock().map_err(|_| Fault::Unavailable)?;
            Ok(identity.user_id == "alice" && scope == Scope::Lan && state.allowed)
        })
    }
    fn list<'a>(
        &'a self,
        identity: &'a Identity,
        parent: &'a str,
    ) -> BackendFuture<'a, Result<Vec<Entry>, Fault>> {
        Box::pin(async move {
            if identity.user_id != "alice" {
                return Err(Fault::Forbidden);
            }
            let state = self.0.lock().map_err(|_| Fault::Unavailable)?;
            if !state.allowed {
                return Err(Fault::Forbidden);
            }
            if !parent.is_empty() {
                return Ok(Vec::new());
            }
            if state.children > 0 {
                return Ok((0..state.children)
                    .map(|n| Entry {
                        path: format!("child{n}.md"),
                        name: format!("child{n}.md"),
                        is_dir: false,
                        modified_unix: None,
                    })
                    .collect());
            }
            Ok(state
                .items
                .keys()
                .map(|name| Entry {
                    path: name.clone(),
                    name: name.clone(),
                    is_dir: false,
                    modified_unix: None,
                })
                .collect())
        })
    }
    fn read<'a>(
        &'a self,
        identity: &'a Identity,
        path: &'a str,
    ) -> BackendFuture<'a, Result<Option<File>, Fault>> {
        Box::pin(async move {
            if identity.user_id != "alice" || path == "link.md" {
                return Err(Fault::Forbidden);
            }
            let slow = self.0.lock().map_err(|_| Fault::Unavailable)?.slow;
            if slow {
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            let state = self.0.lock().map_err(|_| Fault::Unavailable)?;
            if !state.allowed {
                return Err(Fault::Forbidden);
            }
            if state.fail_read {
                return Err(Fault::Unavailable);
            }
            Ok(state.items.get(path).cloned().map(|content| File {
                version: if content == "new" { B } else { A }.into(),
                content,
            }))
        })
    }
    fn write<'a>(
        &'a self,
        identity: &'a Identity,
        path: &'a str,
        content: &'a str,
        condition: WriteCondition,
    ) -> BackendFuture<'a, Result<String, Fault>> {
        Box::pin(async move {
            if identity.user_id != "alice" || path == "link.md" {
                return Err(Fault::Forbidden);
            }
            let slow_write = self.0.lock().map_err(|_| Fault::Unavailable)?.slow_write;
            if slow_write {
                self.1.notify_one();
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            let mut state = self.0.lock().map_err(|_| Fault::Unavailable)?;
            if !state.allowed {
                return Err(Fault::Forbidden);
            }
            let current = state
                .items
                .get(path)
                .map(|v| if v == "new" { B } else { A });
            match condition {
                WriteCondition::Create if current.is_some() => return Err(Fault::Conflict),
                WriteCondition::Match(v) if current != Some(v.as_str()) => {
                    return Err(Fault::Conflict)
                }
                _ => (),
            }
            state.items.insert(path.into(), content.into());
            Ok(if content == "new" { B } else { A }.into())
        })
    }
    fn mkdir<'a>(
        &'a self,
        identity: &'a Identity,
        path: &'a str,
    ) -> BackendFuture<'a, Result<(), Fault>> {
        Box::pin(async move {
            if identity.user_id != "alice" || path == "link" {
                return Err(Fault::Forbidden);
            }
            if !self.0.lock().map_err(|_| Fault::Unavailable)?.allowed {
                return Err(Fault::Forbidden);
            }
            Ok(())
        })
    }
}

async fn serve<B: Backend>(
    backend: Arc<B>,
    config: Config,
) -> io::Result<(SocketAddr, JoinHandle<()>)> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    let app = Arc::new(DavService::new(backend, config));
    let task = tokio::spawn(async move {
        while let Ok((stream, peer)) = listener.accept().await {
            let app = Arc::clone(&app);
            tokio::spawn(async move {
                let service = service_fn(move |req: Request<Incoming>| {
                    let app = Arc::clone(&app);
                    async move {
                        let reply: Response<Body> = app.handle(req.map(Body::new), peer.ip()).await;
                        Ok::<Response<Body>, Infallible>(reply)
                    }
                });
                let _ = hyper::server::conn::http1::Builder::new()
                    .serve_connection(TokioIo::new(stream), service)
                    .await;
            });
        }
    });
    Ok((addr, task))
}

struct Answer {
    status: u16,
    headers: HashMap<String, String>,
    body: Vec<u8>,
}
async fn request(
    addr: SocketAddr,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
    body: &[u8],
) -> io::Result<Answer> {
    let mut stream = TcpStream::connect(addr).await?;
    let host = headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("Host"))
        .map_or("localhost", |(_, v)| *v);
    let auth = headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("Authorization"))
        .map_or("Basic YWxpY2U6c3ludGhldGlj", |(_, v)| *v);
    let mut wire = format!("{method} {path} HTTP/1.1\r\nHost: {host}\r\nAuthorization: {auth}\r\nContent-Length: {}\r\nConnection: close\r\n", body.len());
    for (key, value) in headers {
        if !key.eq_ignore_ascii_case("Host") && !key.eq_ignore_ascii_case("Authorization") {
            wire.push_str(&format!("{key}: {value}\r\n"));
        }
    }
    wire.push_str("\r\n");
    stream.write_all(wire.as_bytes()).await?;
    stream.write_all(body).await?;
    let mut bytes = Vec::new();
    tokio::time::timeout(Duration::from_secs(3), stream.read_to_end(&mut bytes))
        .await
        .map_err(|_| io::Error::other("read timeout"))??;
    let split = bytes
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .ok_or_else(|| io::Error::other("missing headers"))?;
    let header_text = std::str::from_utf8(
        bytes
            .get(..split)
            .ok_or_else(|| io::Error::other("header slice"))?,
    )
    .map_err(io::Error::other)?;
    let status = header_text
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .ok_or_else(|| io::Error::other("missing status"))?
        .parse::<u16>()
        .map_err(io::Error::other)?;
    let mut parsed = HashMap::new();
    for line in header_text.lines().skip(1) {
        if let Some((key, value)) = line.split_once(':') {
            parsed.insert(key.to_ascii_lowercase(), value.trim().to_owned());
        }
    }
    let body = bytes
        .get(split + 4..)
        .ok_or_else(|| io::Error::other("body slice"))?
        .to_vec();
    Ok(Answer {
        status,
        headers: parsed,
        body,
    })
}

async fn raw_status(addr: SocketAddr, wire: &str) -> io::Result<u16> {
    let mut stream = TcpStream::connect(addr).await?;
    stream.write_all(wire.as_bytes()).await?;
    let mut bytes = Vec::new();
    tokio::time::timeout(Duration::from_secs(3), stream.read_to_end(&mut bytes))
        .await
        .map_err(|_| io::Error::other("read timeout"))??;
    let text = std::str::from_utf8(&bytes).map_err(io::Error::other)?;
    text.lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .ok_or_else(|| io::Error::other("missing status"))?
        .parse()
        .map_err(io::Error::other)
}

fn lan() -> io::Result<Config> {
    Config::new("http://localhost", Scope::Lan, None).map_err(io::Error::other)
}

#[tokio::test]
async fn reads_head_etag_scope_and_revocation() -> TestResult {
    let backend = Synthetic::new();
    let (addr, task) = serve(Arc::clone(&backend), lan()?).await?;
    let get = request(addr, "GET", "/dav/alice/entry.md", &[], b"").await?;
    assert_eq!(get.status, 200);
    assert_eq!(get.body, b"Synthetic alice");
    assert_eq!(
        get.headers.get("etag").map(String::as_str),
        Some(format!("\"{A}\"").as_str())
    );
    let head = request(addr, "HEAD", "/dav/alice/entry.md", &[], b"").await?;
    assert_eq!(head.status, 200);
    assert!(head.body.is_empty());
    assert_eq!(
        head.headers.get("content-length").map(String::as_str),
        Some("15")
    );
    let tag = format!("\"{A}\"");
    assert_eq!(
        request(
            addr,
            "GET",
            "/dav/alice/entry.md",
            &[("If-None-Match", &tag)],
            b""
        )
        .await?
        .status,
        304
    );
    assert_eq!(
        request(
            addr,
            "GET",
            "/dav/alice/entry.md",
            &[("Authorization", "Basic Ym9iOnN5bnRoZXRpYw==")],
            b""
        )
        .await?
        .status,
        401
    );
    let denied_head = request(
        addr,
        "HEAD",
        "/dav/alice/entry.md",
        &[("Authorization", "Basic Ym9iOnN5bnRoZXRpYw==")],
        b"",
    )
    .await?;
    assert_eq!(denied_head.status, 401);
    assert!(denied_head.body.is_empty());
    backend.change(|s| s.allowed = false);
    let revoked_head = request(addr, "HEAD", "/dav/alice/entry.md", &[], b"").await?;
    assert_eq!(revoked_head.status, 403);
    assert!(revoked_head.body.is_empty());
    assert_eq!(
        request(addr, "GET", "/dav/alice/entry.md", &[], b"")
            .await?
            .status,
        403
    );
    backend.change(|s| {
        s.allowed = true;
        s.fail_read = true;
    });
    let fault_head = request(addr, "HEAD", "/dav/alice/entry.md", &[], b"").await?;
    assert_eq!(fault_head.status, 502);
    assert!(fault_head.body.is_empty());
    task.abort();
    Ok(())
}

#[tokio::test]
async fn host_proxy_path_and_missing_capability_refuse() -> TestResult {
    let backend = Synthetic::new();
    let (addr, task) = serve(Arc::clone(&backend), lan()?).await?;
    assert_eq!(
        request(
            addr,
            "GET",
            "/dav/alice/entry.md",
            &[("Host", "wrong.example")],
            b""
        )
        .await?
        .status,
        421
    );
    for path in [
        "/dav/alice/%2eprivate.md",
        "/dav/alice/a%2fb.md",
        "/dav/alice/%5csecret.md",
        "/dav/alice/%zz",
        "/dav/alice/../x.md",
    ] {
        assert_eq!(
            request(addr, "GET", path, &[], b"").await?.status,
            400,
            "{path}"
        );
    }
    assert_eq!(
        request(addr, "GET", "/dav/alice/entry.md?x", &[], b"")
            .await?
            .status,
        404
    );
    let options = request(addr, "OPTIONS", "/dav/alice/", &[], b"").await?;
    assert_eq!(options.status, 200);
    assert!(!options.headers.contains_key("dav"));
    assert_eq!(
        request(addr, "DELETE", "/dav/alice/entry.md", &[], b"")
            .await?
            .status,
        405
    );
    assert_eq!(
        request(addr, "MKCOL", "/dav/alice/new/", &[], b"")
            .await?
            .status,
        405
    );
    task.abort();

    assert!(Config::new("http://files.example", Scope::Public, None).is_err());
    assert!(Config::new("https://files.example", Scope::Public, None).is_err());
    let cfg = Config::new("https://files.example", Scope::Public, Some("a".repeat(64)))
        .map_err(io::Error::other)?;
    let (addr, task) = serve(Arc::new(DenyAll), cfg).await?;
    assert_eq!(
        request(
            addr,
            "GET",
            "/dav/alice/entry.md",
            &[("Host", "files.example"), ("x-forwarded-proto", "https")],
            b""
        )
        .await?
        .status,
        403
    );
    assert_eq!(
        request(
            addr,
            "GET",
            "/dav/alice/entry.md",
            &[
                ("Host", "other.example"),
                ("x-forwarded-proto", "https"),
                ("x-cowork-dav-proxy-token", &"a".repeat(64))
            ],
            b""
        )
        .await?
        .status,
        421
    );
    assert_eq!(
        request(
            addr,
            "GET",
            "/dav/alice/entry.md",
            &[
                ("Host", "files.example"),
                ("x-forwarded-proto", "https"),
                ("x-cowork-dav-proxy-token", &"a".repeat(64))
            ],
            b""
        )
        .await?
        .status,
        401
    );
    task.abort();
    Ok(())
}

#[tokio::test]
async fn propfind_xml_depth_list_and_deadline_are_bounded() -> TestResult {
    let backend = Synthetic::new();
    let (addr, task) = serve(
        Arc::clone(&backend),
        lan()?.with_deadline(Duration::from_millis(20)),
    )
    .await?;
    let body = b"<propfind xmlns=\"DAV:\"><prop><getetag/><getlastmodified/><x:custom xmlns:x=\"urn:test\"/></prop></propfind>";
    let answer = request(addr, "PROPFIND", "/dav/alice/", &[("Depth", "1")], body).await?;
    assert_eq!(answer.status, 207);
    let text = String::from_utf8(answer.body)?;
    assert!(text.contains("entry.md"));
    assert!(text.contains("404 Not Found"));
    assert_eq!(
        request(addr, "PROPFIND", "/dav/alice/", &[], b"")
            .await?
            .status,
        403
    );
    assert_eq!(
        request(
            addr,
            "PROPFIND",
            "/dav/alice/",
            &[("Depth", "0")],
            b"<!DOCTYPE x>"
        )
        .await?
        .status,
        400
    );
    assert_eq!(
        request(
            addr,
            "PROPFIND",
            "/dav/alice/",
            &[("Depth", "0")],
            &vec![b'x'; 8193]
        )
        .await?
        .status,
        413
    );
    backend.change(|s| s.children = 101);
    assert_eq!(
        request(addr, "PROPFIND", "/dav/alice/", &[("Depth", "1")], b"")
            .await?
            .status,
        413
    );
    backend.change(|s| {
        s.children = 0;
        s.slow = true;
    });
    assert_eq!(
        request(addr, "GET", "/dav/alice/entry.md", &[], b"")
            .await?
            .status,
        502
    );
    task.abort();
    Ok(())
}

#[tokio::test]
async fn conditional_put_mkdir_and_symlink_boundary() -> TestResult {
    let backend = Synthetic::new();
    let (addr, task) = serve(Arc::clone(&backend), lan()?).await?;
    assert_eq!(
        request(addr, "PUT", "/dav/alice/entry.md", &[], b"new")
            .await?
            .status,
        428
    );
    assert_eq!(
        request(
            addr,
            "PUT",
            "/dav/alice/entry.md",
            &[("If-Match", &format!("\"{B}\""))],
            b"new"
        )
        .await?
        .status,
        412
    );
    assert_eq!(
        request(
            addr,
            "PUT",
            "/dav/alice/entry.md",
            &[("If-Match", &format!("\"{A}\""))],
            b"new"
        )
        .await?
        .status,
        204
    );
    assert_eq!(
        request(addr, "GET", "/dav/alice/entry.md", &[], b"")
            .await?
            .body,
        b"new"
    );
    assert_eq!(
        request(
            addr,
            "PUT",
            "/dav/alice/new.md",
            &[("If-None-Match", "*")],
            b"hello"
        )
        .await?
        .status,
        201
    );
    assert_eq!(
        request(
            addr,
            "PUT",
            "/dav/alice/new.md",
            &[("If-None-Match", "*")],
            b"lost"
        )
        .await?
        .status,
        412
    );
    assert_eq!(
        request(
            addr,
            "PUT",
            "/dav/alice/missing/new.md",
            &[("If-None-Match", "*")],
            b"hello"
        )
        .await?
        .status,
        409
    );
    assert_eq!(
        request(
            addr,
            "PUT",
            "/dav/alice/bad.txt",
            &[("If-None-Match", "*")],
            b"hello"
        )
        .await?
        .status,
        415
    );
    assert_eq!(
        request(
            addr,
            "PUT",
            "/dav/alice/invalid.md",
            &[("If-None-Match", "*")],
            &[0xff]
        )
        .await?
        .status,
        400
    );
    assert_eq!(
        request(
            addr,
            "PUT",
            "/dav/alice/huge.md",
            &[("If-None-Match", "*")],
            &vec![b'x'; 512 * 1024 + 1]
        )
        .await?
        .status,
        413
    );
    assert_eq!(
        request(
            addr,
            "PUT",
            "/dav/alice/link.md",
            &[("If-None-Match", "*")],
            b"no"
        )
        .await?
        .status,
        403
    );
    backend.change(|s| s.mkdir = true);
    assert_eq!(
        request(addr, "MKCOL", "/dav/alice/notes/", &[], b"")
            .await?
            .status,
        201
    );
    assert_eq!(
        request(addr, "MKCOL", "/dav/alice/notes/", &[], b"x")
            .await?
            .status,
        415
    );
    task.abort();
    Ok(())
}

#[tokio::test]
async fn timed_out_write_is_ambiguous_and_cancelled() -> TestResult {
    let backend = Synthetic::new();
    backend.change(|s| s.slow_write = true);
    let (addr, task) = serve(
        Arc::clone(&backend),
        lan()?.with_deadline(Duration::from_millis(20)),
    )
    .await?;
    let answer = request(
        addr,
        "PUT",
        "/dav/alice/new.md",
        &[("If-None-Match", "*")],
        b"late",
    )
    .await?;
    assert_eq!(answer.status, 503);
    assert_eq!(
        answer.headers.get("retry-after").map(String::as_str),
        Some("5")
    );
    tokio::time::sleep(Duration::from_millis(120)).await;
    let has_new = backend
        .0
        .lock()
        .map_err(|_| io::Error::other("state lock"))?
        .items
        .contains_key("new.md");
    assert!(
        !has_new,
        "the timed-out backend future must not mutate later"
    );
    task.abort();
    Ok(())
}

#[tokio::test]
async fn duplicate_security_headers_and_lookup_cardinality_refuse() -> TestResult {
    let backend = Synthetic::new();
    let (addr, task) = serve(Arc::clone(&backend), lan()?).await?;
    let auth = "Authorization: Basic YWxpY2U6c3ludGhldGlj\r\n";
    for extra in [
        "Host: localhost\r\n",
        auth,
        "If-Match: \"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\"\r\nIf-Match: \"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\"\r\n",
        "If-None-Match: *\r\nIf-None-Match: *\r\n",
        "x-cowork-dav-proxy-token: a\r\nx-cowork-dav-proxy-token: a\r\n",
        "x-forwarded-proto: https\r\nx-forwarded-proto: https\r\n",
    ] {
        let wire = format!("PUT /dav/alice/new.md HTTP/1.1\r\nHost: localhost\r\n{auth}{extra}Content-Length: 0\r\nConnection: close\r\n\r\n");
        assert_eq!(raw_status(addr, &wire).await?, 400, "{extra}");
    }
    backend.change(|s| s.children = 1025);
    assert_eq!(
        request(addr, "GET", "/dav/alice/child0.md", &[], b"")
            .await?
            .status,
        413
    );
    task.abort();
    Ok(())
}

#[tokio::test]
async fn revocation_during_backend_write_is_enforced_at_mutation() -> TestResult {
    let backend = Synthetic::new();
    backend.change(|s| s.slow_write = true);
    let (addr, task) = serve(
        Arc::clone(&backend),
        lan()?.with_deadline(Duration::from_millis(250)),
    )
    .await?;
    let started = backend.1.notified();
    let pending = tokio::spawn(request(
        addr,
        "PUT",
        "/dav/alice/new.md",
        &[("If-None-Match", "*")],
        b"late",
    ));
    tokio::time::timeout(Duration::from_secs(1), started).await?;
    backend.change(|s| s.allowed = false);
    let answer = pending.await??;
    assert_eq!(answer.status, 403);
    assert!(!backend
        .0
        .lock()
        .map_err(|_| io::Error::other("state lock"))?
        .items
        .contains_key("new.md"));
    task.abort();
    Ok(())
}
