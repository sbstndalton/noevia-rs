//! Council p8-3 (noevia#2731 item 4): a provider row's turn through `POST /api/chat` of a real front with
//! `NOEVIA_RUST_CHAT_PROVIDERS=1`, so chat_native.rs `drive` runs chat_provider.rs's setup and the provider host.
//! A fake core on a local port hands off the recorded `plain` turn (node-turns.v1.json) re-pointed at a provider
//! row, with the frame facts core's `plainProviderRow` writes (no manager, no lease, no title, the hosted
//! limit); a loopback stub answers as the provider. Checked: the caps header names `providers`; the stub gets the
//! one round, without the local engine's lease header and without a key (the row has none); the reply and the
//! hosted limit reach the client. And a frame whose address is not the row's ends with the native failure text
//! before the stub is asked.
//!
//! Its own test binary: it sets the switch for the whole process. Synthetic data only; local ports only.
#![cfg(feature = "rust-storage-reads")]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]

use bytes::Bytes;
use chat_turn::js;
use http_body_util::{BodyExt, Full};
use hyper::body::Incoming;
use hyper::{Request, Response};
use js_json::{wtf16, JValue};
use noevia_server::config::Config;
use noevia_server::serve::{serve, Limits};
use server_store::rusqlite::Connection;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

const FIXTURE: &str = include_str!("../../../crates/chat-turn/tests/fixtures/node-turns.v1.json");
const SCHEMA: &str = include_str!("../../../crates/server-store/tests/fixtures/node-schema.sql");
const CODE: &str = "synthetic-chat-provider-code";
const ORIGIN: &str = "http://127.0.0.1:18021";
const STORE_TOKEN: &str = "synthetic-store-token-0123456789abcdef";
const START: &str = "0123456789abcdef0123456789abcdef";
const LEASE_PATH: &str = "/internal/inference-admission/lease/";
const HOSTED_SOURCE: &str = "Default for hosted providers";

type UpBody = http_body_util::combinators::BoxBody<Bytes, std::io::Error>;

fn full(status: u16, ctype: &str, text: String) -> Response<UpBody> {
    Response::builder()
        .status(status)
        .header("content-type", ctype)
        .body(Full::new(Bytes::from(text)).map_err(|e| match e {}).boxed())
        .unwrap()
}

fn json(status: u16, v: &JValue) -> Response<UpBody> {
    full(status, "application/json", js::stringify(v))
}

fn empty(status: u16) -> Response<UpBody> {
    Response::builder()
        .status(status)
        .body(Full::new(Bytes::new()).map_err(|e| match e {}).boxed())
        .unwrap()
}

async fn listen<F, Fut>(f: F) -> SocketAddr
where
    F: Fn(Request<Incoming>) -> Fut + Send + Sync + Clone + 'static,
    Fut: std::future::Future<Output = Result<Response<UpBody>, std::io::Error>> + Send + 'static,
{
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            let f = f.clone();
            tokio::spawn(async move {
                let svc = hyper::service::service_fn(move |r| f.clone()(r));
                let _ = hyper::server::conn::http1::Builder::new()
                    .serve_connection(hyper_util::rt::TokioIo::new(stream), svc)
                    .await;
            });
        }
    });
    addr
}

async fn read_req(req: Request<Incoming>) -> (String, String, HashMap<String, String>, String) {
    let method = req.method().to_string();
    let path = req.uri().path().to_string();
    let headers = req
        .headers()
        .iter()
        .map(|(k, v)| {
            (
                k.to_string(),
                String::from_utf8_lossy(v.as_bytes()).into_owned(),
            )
        })
        .collect();
    let body = req
        .into_body()
        .collect()
        .await
        .map(|c| c.to_bytes())
        .unwrap_or_default();
    (
        method,
        path,
        headers,
        String::from_utf8_lossy(&body).into_owned(),
    )
}

/// What the fake core and the provider stub saw.
#[derive(Default)]
struct Seen {
    /// The prepared frame core hands off next (set per turn).
    frame: Option<JValue>,
    /// The NDJSON event lines before it (the recording's).
    before: Vec<String>,
    caps: Vec<Option<String>>,
    /// Paths core was asked that it does not answer.
    unknown: Vec<String>,
    /// The provider's requests: path, its headers, its body.
    provider: Vec<(String, HashMap<String, String>, JValue)>,
    /// The provider's answer: the recorded round's frames.
    answer: Vec<JValue>,
}

type Shared = Arc<Mutex<Seen>>;

async fn core_service(s: Shared, req: Request<Incoming>) -> Result<Response<UpBody>, std::io::Error> {
    let (method, path, headers, _body) = read_req(req).await;
    let ok = |v: JValue| Ok(json(200, &v));
    if path == "/internal/chat/gate" {
        return Ok(empty(204));
    }
    if let Some(rest) = path.strip_prefix(LEASE_PATH) {
        if method == "PUT" {
            return Ok(json(
                201,
                &js::obj([
                    ("lease", js::s(rest)),
                    ("ttlMs", js::n(30_000.0)),
                    ("headers", js::obj([("x-noevia-lease", js::s("qalease1"))])),
                ]),
            ));
        }
        return Ok(if rest.ends_with("/renew") {
            empty(200)
        } else {
            empty(204)
        });
    }
    let mut seen = s.lock().unwrap();
    match path.as_str() {
        "/internal/chat/turn" => {
            seen.caps
                .push(headers.get("x-noevia-chat-handoff-caps").cloned());
            let mut lines = seen.before.clone();
            let frame = seen.frame.clone().expect("a frame for this turn");
            lines.push(format!("{}\n", js::stringify(&frame)));
            Ok(full(200, "application/x-ndjson", lines.concat()))
        }
        "/internal/chat/native/tools-allowed" => ok(js::obj([("allowed", JValue::Bool(true))])),
        "/internal/chat/native/busy" => ok(js::obj([("busy", JValue::Bool(false))])),
        "/internal/chat/native/title-auto-named" => {
            ok(js::obj([("autoNamed", JValue::Bool(true))]))
        }
        "/internal/chat/skills/scrub" => ok(js::obj([
            ("changed", JValue::Arr(Vec::new())),
            ("removed", JValue::Arr(Vec::new())),
        ])),
        "/internal/chat/usage"
        | "/internal/chat/prefill"
        | "/internal/chat/native/title-save"
        | "/internal/chat/native/tools-unsupported"
        | "/internal/chat/native/gate-record" => Ok(empty(204)),
        _ => {
            seen.unknown.push(format!("{method} {path}"));
            Ok(json(404, &js::obj([("error", js::s("not found"))])))
        }
    }
}

async fn provider_service(s: Shared, req: Request<Incoming>) -> Result<Response<UpBody>, std::io::Error> {
    let (_method, path, headers, body) = read_req(req).await;
    let mut seen = s.lock().unwrap();
    seen.provider.push((
        path,
        headers,
        wtf16::parse(&body).unwrap_or(JValue::Null),
    ));
    let mut out = String::new();
    for f in &seen.answer {
        if f.get("done") == &JValue::Bool(true) {
            out.push_str("data: [DONE]\n\n");
        } else {
            out.push_str(&format!("data: {}\n\n", js::stringify(f)));
        }
    }
    Ok(full(200, "text/event-stream", out))
}

struct Front {
    _dir: tempfile::TempDir,
    data: PathBuf,
    addr: SocketAddr,
    jar: String,
    csrf: String,
    user: String,
}

async fn front(core: SocketAddr) -> Front {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().to_path_buf();
    let c = Connection::open(server_store::db_path(&data)).unwrap();
    c.execute_batch(SCHEMA).unwrap();
    c.execute(
        "INSERT INTO settings VALUES('setup_code_hash', ?1)",
        [server_auth::js::digest(CODE)],
    )
    .unwrap();
    let mut env: HashMap<&str, String> = HashMap::new();
    env.insert("NOEVIA_LEGACY_UPSTREAM", format!("http://{core}"));
    env.insert("UI_PORT", "1".into());
    env.insert("UI_DATA_DIR", data.display().to_string());
    env.insert("NOEVIA_WEB_DIST", data.join("dist").display().to_string());
    env.insert("PUBLIC_ORIGIN", ORIGIN.into());
    env.insert("NOEVIA_START_ID", START.into());
    env.insert("NOEVIA_PROJECT_STORE_TOKEN", STORE_TOKEN.into());
    env.insert("NOEVIA_CHAT_EXECUTOR", "native".into());
    let app = noevia_server::App::new(Config::from_lookup(|k| env.get(k).cloned()).unwrap());
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(serve(
        listener,
        app,
        Limits::default(),
        std::future::pending(),
    ));
    let setup = format!(
        r#"{{"setupCode":"{CODE}","username":"owner","password":"synthetic chat provider password","diaryEnabled":false}}"#
    );
    let mut s = TcpStream::connect(addr).await.unwrap();
    let req = format!("POST /api/setup/complete HTTP/1.1\r\nhost: front.test\r\nconnection: close\r\norigin: {ORIGIN}\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n{setup}", setup.len());
    s.write_all(req.as_bytes()).await.unwrap();
    let mut out = Vec::new();
    s.read_to_end(&mut out).await.unwrap();
    let text = String::from_utf8(out).unwrap();
    assert!(text.starts_with("HTTP/1.1 201"), "{text}");
    let cookies: Vec<String> = text
        .lines()
        .filter_map(|l| {
            l.strip_prefix("set-cookie: ")
                .map(|v| v.split(';').next().unwrap().to_string())
        })
        .collect();
    let csrf = cookies[1].split_once('=').unwrap().1.to_string();
    let jar = cookies.join("; ");
    let user: String = c
        .query_row("SELECT id FROM users", [], |r| r.get(0))
        .unwrap();
    Front {
        _dir: dir,
        data,
        addr,
        jar,
        csrf,
        user,
    }
}

/// POST /api/chat: its status and its whole SSE body (the turn ends within 30 s or the test fails).
async fn chat(f: &Front, body: &str) -> (u16, String) {
    let stream = TcpStream::connect(f.addr).await.unwrap();
    let (mut send, conn) =
        hyper::client::conn::http1::handshake(hyper_util::rt::TokioIo::new(stream))
            .await
            .unwrap();
    let conn = tokio::spawn(async move {
        let _ = conn.await;
    });
    let req = Request::builder()
        .method("POST")
        .uri("/api/chat")
        .header("host", "front.test")
        .header("origin", ORIGIN)
        .header("content-type", "application/json")
        .header("cookie", &f.jar)
        .header("x-csrf-token", &f.csrf)
        .body(Full::new(Bytes::from(body.to_string())))
        .unwrap();
    let res = send.send_request(req).await.unwrap();
    let status = res.status().as_u16();
    let collected = tokio::time::timeout(Duration::from_secs(30), res.into_body().collect())
        .await
        .expect("the turn did not end within 30 s");
    let bytes = collected.map(|c| c.to_bytes()).unwrap_or_default();
    drop(send);
    conn.abort();
    (status, String::from_utf8_lossy(&bytes).into_owned())
}

/// The recorded `plain` case's prepared frame as core's `plainProviderRow` would write it for `url`.
fn provider_frame(prepared: &JValue, url: &str) -> JValue {
    let mut p = prepared.clone();
    js::set(
        &mut p,
        "provider",
        js::obj([
            ("id", js::s("qa-provider")),
            ("label", js::s("QA provider")),
            ("localEngine", JValue::Bool(false)),
            ("capabilities", JValue::Null),
        ]),
    );
    js::set(&mut p, "upstreamUrl", js::s(url));
    #[allow(clippy::cast_precision_loss)]
    let now = noevia_server::identity::now_ms() as f64;
    js::set(&mut p, "startedAt", js::n(now));
    js::set(&mut p, "managerEnabled", JValue::Bool(false));
    js::set(&mut p, "modelLease", JValue::Bool(false));
    js::set(&mut p, "templateGate", JValue::Bool(false));
    js::set(
        &mut p,
        "runtimeLimit",
        js::obj([("tokens", js::n(32768.0)), ("source", js::s(HOSTED_SOURCE))]),
    );
    let mut chat = p.get("chat").clone();
    let mut title = chat.get("title").clone();
    js::set(&mut title, "enabled", JValue::Bool(false));
    js::set(&mut chat, "title", title);
    js::set(&mut p, "chat", chat);
    p
}

#[tokio::test(flavor = "multi_thread")]
async fn a_provider_row_turn_runs_on_the_provider_host_with_the_switch_on() {
    std::env::set_var(noevia_server::chat_provider::SWITCH, "1");
    std::env::remove_var("DEFAULT_PROVIDER_ID");
    let fixture = wtf16::parse(FIXTURE).unwrap();
    let JValue::Arr(cases) = fixture.get("cases").clone() else {
        panic!("no cases")
    };
    let case = cases
        .iter()
        .find(|c| c.get("name").as_str() == Some("plain"))
        .expect("the plain case")
        .clone();
    let JValue::Arr(raw) = case.get("handoff").get("raw").clone() else {
        panic!("no hand-off")
    };
    let mut lines: Vec<String> = raw
        .iter()
        .map(|l| {
            let sse = l.as_str().unwrap();
            let json = sse
                .strip_prefix("data: ")
                .and_then(|t| t.strip_suffix("\n\n"))
                .unwrap_or_else(|| panic!("not one event frame: {sse:?}"));
            format!("{json}\n")
        })
        .collect();
    let prepared = wtf16::parse(lines.pop().unwrap().trim_end()).unwrap();
    let JValue::Arr(rounds) = case.get("input").get("rounds").clone() else {
        panic!("no rounds")
    };
    let JValue::Arr(answer) = rounds[0].get("frames").clone() else {
        panic!("no frames")
    };

    let seen: Shared = Arc::new(Mutex::new(Seen {
        before: lines,
        answer,
        ..Seen::default()
    }));
    let s = Arc::clone(&seen);
    let core = listen(move |r| core_service(Arc::clone(&s), r)).await;
    let s = Arc::clone(&seen);
    let provider = listen(move |r| provider_service(Arc::clone(&s), r)).await;
    let f = front(core).await;
    let base = format!("http://{provider}/v1");
    let url = format!("{base}/chat/completions");
    // The administrator's own row (not held to the member origin list), without a key.
    let dir = f.data.join("users").join(&f.user);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("providers.json"),
        format!(
            r#"{{"providers":[{{"id":"qa-provider","label":"QA provider","baseUrl":"{base}","apiKey":""}}]}}"#
        ),
    )
    .unwrap();
    // The account as core's preparation leaves it: its directory and the user message in the transcript.
    server_projects::store::ensure_user_dir(&f.data, &f.user).unwrap();
    let body = case.get("input").get("body").clone();
    let chat_id = body.get("chatId").as_str().unwrap().to_string();
    server_projects::transcript::write(
        &f.data,
        &f.user,
        &chat_id,
        &[js::obj([
            ("role", js::s("user")),
            ("content", body.get("message").clone()),
        ])],
        &server_projects::transcript::Base::Any,
        None,
    )
    .unwrap();

    // The row's turn: run here, on the provider host.
    seen.lock().unwrap().frame = Some(provider_frame(&prepared, &url));
    let (status, sse) = chat(&f, &js::stringify(&body)).await;
    assert_eq!(status, 200, "{sse}");
    {
        let s = seen.lock().unwrap();
        let caps = format!("{},providers", chat_turn::handoff::HANDOFF_CAPS);
        assert_eq!(s.caps, vec![Some(caps)]);
        assert_eq!(s.provider.len(), 1, "one round; core asked {:?}; {sse}", s.unknown);
        let (path, headers, sent) = &s.provider[0];
        assert_eq!(path, "/v1/chat/completions");
        assert_eq!(sent.get("model").as_str(), Some("qa-model"));
        assert!(!headers.contains_key("authorization"), "the row has no key");
        assert!(!headers.contains_key("x-noevia-lease"), "no local lease on a provider row");
    }
    assert!(sse.contains("Hello from "), "{sse}");
    assert!(sse.contains("QA."), "{sse}");
    assert!(sse.contains(HOSTED_SOURCE), "the hosted limit reaches the context event: {sse}");
    assert!(!sse.contains(chat_turn::native::FAILED_TEXT), "{sse}");

    // A frame whose address is not the row's: the native failure text, the provider never asked.
    seen.lock().unwrap().frame = Some(provider_frame(
        &prepared,
        "http://127.0.0.1:9/v1/chat/completions",
    ));
    let (status, sse) = chat(&f, &js::stringify(&body)).await;
    assert_eq!(status, 200, "{sse}");
    assert!(sse.contains(chat_turn::native::FAILED_TEXT), "{sse}");
    assert!(!sse.contains("Hello from "), "{sse}");
    assert_eq!(seen.lock().unwrap().provider.len(), 1);
    std::env::remove_var(noevia_server::chat_provider::SWITCH);
}
