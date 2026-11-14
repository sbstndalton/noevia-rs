//! Track R11 (S12): POST /api/models/pull through the whole front. With `NOEVIA_RUST_MODEL_WRITES`
//! unset it is relayed to a synthetic Node; with `rust` the front answers it. A scripted Node
//! follows core's contract (noevia-core track/R11-pull-bridge): the auto-roles invalidation, the
//! trusted scan clear, the inference admission lease, and the download tracker bridge
//! `POST /internal/models/downloads/{requested,rejected}` (`{"model":"<checkpoint>"}`, one key, a
//! non-empty string of at most 512; 200 `{"ok":true}`, 404 without a tracker, 400 for any other
//! body). A scripted llama.cpp router answers `POST /models`. The recorded pulls core's own code
//! answered (crates/model-pull/tests/fixtures/node-model-pull.v1.json) must be answered alike.
//! QA-named data only; no engine, no model loads, no network beyond 127.0.0.1.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]

use js_json::{wtf16, JValue};
use noevia_server::{
    config::Config,
    serve::{serve, Limits},
    App,
};
use server_store::rusqlite::Connection;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

const A: &str = "aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa";
const B: &str = "bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb";
const ADMIN: &str = "Cookie: cowork_session=admin-token; cowork_csrf=csrf\r\n";
const MEMBER: &str = "Cookie: cowork_session=member-token; cowork_csrf=csrf\r\n";
const SECRET: &str = "synthetic-store-token-0123456789abcdef";
const ROUTER_KEY: &str = "qa-router-key";

fn hex(s: &str) -> String {
    Sha256::digest(s.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn seed(dir: &Path) {
    let db = Connection::open(dir.join("cowork.db")).unwrap();
    db.execute_batch(include_str!(
        "../../../crates/server-store/tests/fixtures/node-schema.sql"
    ))
    .unwrap();
    let now = noevia_server::identity::now_ms();
    for (uid, role, token) in [(A, "admin", "admin-token"), (B, "member", "member-token")] {
        db.execute("INSERT INTO users(id,username,username_norm,display_name,role,password_hash,webauthn_user_id,created_at,updated_at) VALUES(?1,?1,?1,'QA',?2,'x',?1,1,1)", (uid, role)).unwrap();
        db.execute(
            "INSERT INTO sessions VALUES(?1,?2,?3,?4,?4,?5,'ua','127.0.0.1')",
            (hex(token), uid, hex("csrf"), now, now + 86_400_000),
        )
        .unwrap();
    }
}

async fn post(addr: SocketAddr, who: &str, body: &[u8]) -> (u16, JValue) {
    post_at(addr, "/api/models/pull", who, body).await
}

async fn post_at(addr: SocketAddr, path: &str, who: &str, body: &[u8]) -> (u16, JValue) {
    send_raw(addr, path, who, body.len(), body, Duration::from_secs(60)).await
}

/// A POST that declares `length` body bytes and sends `body` (fewer, to show it is not read), the
/// answer awaited for at most `wait`.
async fn send_raw(
    addr: SocketAddr,
    path: &str,
    who: &str,
    length: usize,
    body: &[u8],
    wait: Duration,
) -> (u16, JValue) {
    let mut s = TcpStream::connect(addr).await.unwrap();
    let mut req = format!(
        "POST {path} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n{who}Origin: http://{addr}\r\nX-CSRF-Token: csrf\r\nContent-Type: application/json\r\nContent-Length: {length}\r\n\r\n"
    )
    .into_bytes();
    req.extend_from_slice(body);
    s.write_all(&req).await.unwrap();
    let mut bytes = Vec::new();
    tokio::time::timeout(wait, s.read_to_end(&mut bytes))
        .await
        .expect("answered in time")
        .unwrap();
    let text = String::from_utf8_lossy(&bytes).into_owned();
    let (h, b) = text.split_once("\r\n\r\n").unwrap();
    (
        h.split_whitespace().nth(1).unwrap().parse().unwrap(),
        wtf16::parse(b).unwrap_or(JValue::Null),
    )
}

/// One request a stub saw: `"<who> <METHOD> <target>"`, its lower-cased head and its body.
#[derive(Clone, Debug)]
struct Seen {
    line: String,
    head: String,
    body: String,
}

type Log = Arc<Mutex<Vec<Seen>>>;

/// An HTTP/1.1 stub: `route(method, target, head, body)` answers (status, body, delay before the
/// answer in ms); every request goes to the shared log as it arrives (one log for Node and the
/// router, so their order shows).
async fn stub(
    who: &'static str,
    log: Log,
    route: impl Fn(&str, &str, &str, &str) -> (u16, String, u64) + Send + Sync + 'static,
) -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let route = Arc::new(route);
    tokio::spawn(async move {
        while let Ok((mut c, _)) = listener.accept().await {
            let (log, route) = (Arc::clone(&log), Arc::clone(&route));
            tokio::spawn(async move {
                let mut buf = Vec::new();
                let mut chunk = [0u8; 8192];
                let (head, body) = loop {
                    let Ok(n) = c.read(&mut chunk).await else {
                        return;
                    };
                    if n == 0 {
                        return;
                    }
                    buf.extend_from_slice(&chunk[..n]);
                    let t = String::from_utf8_lossy(&buf).into_owned();
                    if let Some((h, b)) = t.split_once("\r\n\r\n") {
                        let want = h
                            .lines()
                            .find_map(|l| {
                                l.to_ascii_lowercase()
                                    .strip_prefix("content-length: ")
                                    .map(|v| v.trim().parse::<usize>().unwrap())
                            })
                            .unwrap_or(0);
                        if b.len() >= want {
                            break (h.to_owned(), b.to_owned());
                        }
                    }
                };
                let mut words = head.split_whitespace();
                let (method, target) = (
                    words.next().unwrap_or("").to_owned(),
                    words.next().unwrap_or("").to_owned(),
                );
                log.lock().unwrap().push(Seen {
                    line: format!("{who} {method} {target}"),
                    head: head.to_ascii_lowercase(),
                    body: body.clone(),
                });
                let (code, text, delay_ms) =
                    route(&method, &target, &head.to_ascii_lowercase(), &body);
                tokio::time::sleep(Duration::from_millis(delay_ms)).await;
                let out = format!("HTTP/1.1 {code} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{text}", text.len());
                let _ = c.write_all(out.as_bytes()).await;
            });
        }
    });
    port
}

/// How the scripted Node answers the bridges that may fail.
#[derive(Clone)]
struct NodeScript {
    cleared: bool,
    /// `None`: core's contract; `Some(status)`: that status for `requested`.
    requested: Option<u16>,
    /// How long `requested` takes to answer (core records it first, as its bridge does).
    requested_delay_ms: u64,
    /// `None`: the lease is granted; `Some(status)`: core's latch refuses it with that status.
    lease: Option<u16>,
}

/// The tracker bridge's deadline in these tests (the front's is 5 s): every test runs with it.
const TRACKER_TIMEOUT: Duration = Duration::from_millis(1500);

/// core's tracker bridge: exactly `{"model": "<1..512 chars>"}`.
fn tracker_body_ok(body: &str) -> bool {
    match wtf16::parse(body) {
        Ok(JValue::Obj(items)) => {
            items.len() == 1
                && items[0].0 == "model"
                && items[0]
                    .1
                    .as_str()
                    .is_some_and(|m| !m.is_empty() && wtf16::len(m) <= 512)
        }
        _ => false,
    }
}

async fn node(log: Log, script: Arc<Mutex<NodeScript>>) -> u16 {
    stub("node", log, move |method, target, head, body| {
        let s = script.lock().unwrap().clone();
        let trusted = head.contains(&format!(
            "x-noevia-store-token: {}",
            SECRET.to_ascii_lowercase()
        ));
        if method == "PUT" && target.starts_with("/internal/inference-admission/lease/") {
            if let Some(code) = s.lease {
                return (
                    code,
                    r#"{"error":"QA: model maintenance is in progress"}"#.into(),
                    0,
                );
            }
            let id = target.rsplit('/').next().unwrap_or("").to_owned();
            return (
                201,
                format!(
                    r#"{{"lease":"{id}","ttlMs":60000,"headers":{{"x-noevia-lease":"qaLease1"}}}}"#
                ),
                0,
            );
        }
        if target.starts_with("/internal/inference-admission/lease/") {
            return (if method == "DELETE" { 204 } else { 200 }, "{}".into(), 0);
        }
        match target {
            "/internal/auto-roles/invalidate" => (200, "{}".into(), 0),
            "/internal/model-manager-scan/clear" => {
                (200, format!(r#"{{"cleared":{}}}"#, s.cleared), 0)
            }
            "/internal/models/downloads/requested" | "/internal/models/downloads/rejected" => {
                if !trusted {
                    return (403, r#"{"error":"forbidden"}"#.into(), 0);
                }
                if !tracker_body_ok(body) {
                    return (400, r#"{"error":"invalid body"}"#.into(), 0);
                }
                let requested = target.ends_with("/requested");
                let delay = if requested { s.requested_delay_ms } else { 0 };
                match s.requested.filter(|_| requested) {
                    Some(404) => (
                        404,
                        r#"{"error":"This server has no download tracker."}"#.into(),
                        delay,
                    ),
                    Some(code) => (code, r#"{"error":"qa"}"#.into(), delay),
                    None => (200, r#"{"ok":true}"#.into(), delay),
                }
            }
            _ => (202, r#"{"relayed":true}"#.into(), 0),
        }
    })
    .await
}

/// The router's `POST /models` answer: (status, body text).
type RouterScript = Arc<Mutex<(u16, String)>>;

async fn router(log: Log, script: RouterScript) -> u16 {
    stub("router", log, move |method, target, _, _| {
        // `/v1/models`: a router configured at `<base>/v1/v1` (core trims one `/v1`).
        if method == "POST" && (target == "/models" || target == "/v1/models") {
            let (code, text) = script.lock().unwrap().clone();
            return (code, text, 0);
        }
        (404, "{}".into(), 0)
    })
    .await
}

struct Front {
    addr: SocketAddr,
    log: Log,
    node: Arc<Mutex<NodeScript>>,
    router: RouterScript,
    _dir: tempfile::TempDir,
}

async fn front(on: bool, kind: Option<&str>) -> Front {
    front_at(on, kind, "/v1").await
}

/// A front whose router is configured at `http://127.0.0.1:<port><suffix>`.
async fn front_at(on: bool, kind: Option<&str>, suffix: &str) -> Front {
    noevia_server::model_pull_post::set_tracker_timeout_for_tests(TRACKER_TIMEOUT);
    let dir = tempfile::tempdir().unwrap();
    seed(dir.path());
    // Row H: a Node start is registered with this process's admission authority before any lease.
    engine_admission::authority::global()
        .node_session("model-pull-post-test", 1)
        .unwrap();
    let log: Log = Arc::default();
    let node_script = Arc::new(Mutex::new(NodeScript {
        cleared: true,
        requested: None,
        requested_delay_ms: 0,
        lease: None,
    }));
    let router_script: RouterScript = Arc::new(Mutex::new((200, "{}".into())));
    let node_port = node(Arc::clone(&log), Arc::clone(&node_script)).await;
    let router_port = router(Arc::clone(&log), Arc::clone(&router_script)).await;
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let mut env: HashMap<String, String> = [
        (
            "NOEVIA_LEGACY_UPSTREAM",
            format!("http://127.0.0.1:{node_port}"),
        ),
        ("UI_DATA_DIR", dir.path().display().to_string()),
        ("PUBLIC_ORIGIN", format!("http://{addr}")),
        ("NOEVIA_PROJECT_STORE_TOKEN", SECRET.to_owned()),
        (
            "MODEL_MANAGER_BASE_URL",
            format!("http://127.0.0.1:{router_port}{suffix}"),
        ),
        ("MODEL_MANAGER_API_KEY", ROUTER_KEY.to_owned()),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_owned(), v))
    .collect();
    if let Some(kind) = kind {
        env.insert("MODEL_MANAGER_KIND".into(), kind.into());
    }
    if on {
        env.insert("NOEVIA_RUST_MODEL_WRITES".into(), "rust".into());
    }
    let app = App::new(Config::from_lookup(|k| env.get(k).cloned()).unwrap());
    tokio::spawn(serve(
        listener,
        app,
        Limits::default(),
        std::future::pending(),
    ));
    Front {
        addr,
        log,
        node: node_script,
        router: router_script,
        _dir: dir,
    }
}

/// The calls a pull made, in order, without the lease's renewals.
fn steps(f: &Front) -> Vec<String> {
    f.log
        .lock()
        .unwrap()
        .iter()
        .map(|s| s.line.clone())
        .filter(|l| !l.ends_with("/renew"))
        .map(|l| {
            if l.contains("/internal/inference-admission/lease/") {
                l.split(" /internal").next().unwrap().to_owned() + " lease"
            } else {
                l
            }
        })
        .collect()
}

fn take(f: &Front) -> Vec<String> {
    let s = steps(f);
    f.log.lock().unwrap().clear();
    s
}

/// As `take`, after waiting (at most 5 s) for `n` steps: a refused ticket is given back by a spawned
/// task (engine-admission), so its DELETE can reach the scripted core after the answer.
async fn take_at_least(f: &Front, n: usize) -> Vec<String> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while steps(f).len() < n && std::time::Instant::now() < deadline {
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    take(f)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn switch_unset_the_pull_is_relayed() {
    let f = front(false, Some("llamacpp")).await;
    let (status, body) = post(f.addr, ADMIN, br#"{"checkpoint":"qa-org/qa-model:Q8_0"}"#).await;
    assert_eq!(
        (status, body),
        (202, JValue::obj([("relayed", JValue::Bool(true))]))
    );
    assert_eq!(take(&f), ["node POST /api/models/pull"]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_accepted_pull_in_core_s_order() {
    let f = front(true, Some("llamacpp")).await;
    *f.router.lock().unwrap() = (200, r#"{"status":"downloading"}"#.into());
    let (status, body) = post(f.addr, ADMIN, br#"{"checkpoint":"qa-org/qa-model:Q8_0"}"#).await;
    assert_eq!(status, 200);
    assert_eq!(
        body,
        JValue::obj([
            ("jobId", JValue::from("qa-org/qa-model:Q8_0")),
            ("modelName", JValue::from("qa-org/qa-model:Q8_0")),
        ])
    );
    let log = f.log.lock().unwrap().clone();
    let lease = log
        .iter()
        .find(|s| {
            s.line
                .starts_with("node PUT /internal/inference-admission/lease/")
        })
        .unwrap();
    let ask = wtf16::parse(&lease.body).unwrap();
    assert_eq!(
        ask.get("embedding"),
        &JValue::Bool(true),
        "the maintenance-only ask"
    );
    assert_eq!(ask.get("model").as_str(), Some("model-pull"));
    let requested = log
        .iter()
        .find(|s| s.line == "node POST /internal/models/downloads/requested")
        .unwrap();
    assert_eq!(requested.body, r#"{"model":"qa-org/qa-model:Q8_0"}"#);
    let pull = log
        .iter()
        .find(|s| s.line == "router POST /models")
        .unwrap();
    assert_eq!(pull.body, r#"{"model":"qa-org/qa-model:Q8_0"}"#);
    assert!(pull
        .head
        .contains(&format!("authorization: bearer {ROUTER_KEY}")));
    assert!(!pull.head.contains("x-noevia-maintenance") && !pull.head.contains("x-noevia-lease"));
    assert_eq!(
        take(&f),
        [
            "node POST /internal/auto-roles/invalidate",
            "node POST /internal/model-manager-scan/clear",
            "node PUT lease",
            "node POST /internal/models/downloads/requested",
            "router POST /models",
            "node DELETE lease",
            "node POST /internal/auto-roles/invalidate",
        ]
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_refused_pull_is_recorded_rejected_after_the_router() {
    let f = front(true, Some("llamacpp")).await;
    *f.router.lock().unwrap() = (409, r#"{"error":"QA: already downloading"}"#.into());
    let (status, body) = post(f.addr, ADMIN, br#"{"checkpoint":"qa-org/qa-model"}"#).await;
    assert_eq!(
        (status, body),
        (
            502,
            JValue::obj([("error", JValue::from("QA: already downloading"))])
        )
    );
    assert_eq!(
        take(&f),
        [
            "node POST /internal/auto-roles/invalidate",
            "node POST /internal/model-manager-scan/clear",
            "node PUT lease",
            "node POST /internal/models/downloads/requested",
            "router POST /models",
            "node POST /internal/models/downloads/rejected",
            "node DELETE lease",
            "node POST /internal/auto-roles/invalidate",
        ]
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_download_core_does_not_record_is_refused_before_the_router() {
    let f = front(true, Some("llamacpp")).await;
    // A 5xx may come after core recorded the request: a `rejected` goes before the lease is given
    // back. A 4xx is the bridge refusing before it records: nothing more is sent.
    for (code, rejected) in [(500, true), (502, true), (404, false), (400, false)] {
        f.node.lock().unwrap().requested = Some(code);
        let (status, body) = post(f.addr, ADMIN, br#"{"checkpoint":"qa-org/qa-model:Q8_0"}"#).await;
        assert_eq!(
            (status, body),
            (
                503,
                JValue::obj([("error", JValue::from("The download could not be recorded."))])
            ),
            "{code}"
        );
        let steps = take(&f);
        let mut want = vec![
            "node POST /internal/auto-roles/invalidate",
            "node POST /internal/model-manager-scan/clear",
            "node PUT lease",
            "node POST /internal/models/downloads/requested",
        ];
        if rejected {
            want.push("node POST /internal/models/downloads/rejected");
        }
        want.extend([
            "node DELETE lease",
            "node POST /internal/auto-roles/invalidate",
        ]);
        assert_eq!(steps, want, "{code}");
    }
    // A checkpoint the bridge does not take (core's test reads this array as a string).
    f.node.lock().unwrap().requested = None;
    let (status, _) = post(f.addr, ADMIN, br#"{"checkpoint":["qa-org/qa-model"]}"#).await;
    assert_eq!(status, 503);
    let steps = take(&f);
    assert!(
        !steps
            .iter()
            .any(|s| s.starts_with("router") || s.contains("downloads")),
        "{steps:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_unconfirmed_scan_clear_is_refused_before_anything() {
    let f = front(true, Some("llamacpp")).await;
    f.node.lock().unwrap().cleared = false;
    let (status, body) = post(f.addr, ADMIN, br#"{"checkpoint":"qa-org/qa-model:Q8_0"}"#).await;
    assert_eq!(
        (status, body),
        (
            502,
            JValue::obj([(
                "error",
                JValue::from("The model management service is not responding.")
            )])
        )
    );
    assert_eq!(
        take(&f),
        [
            "node POST /internal/auto-roles/invalidate",
            "node POST /internal/model-manager-scan/clear",
            "node POST /internal/auto-roles/invalidate",
        ]
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_member_is_refused() {
    let f = front(true, Some("llamacpp")).await;
    let (status, body) = post(f.addr, MEMBER, br#"{"checkpoint":"qa-org/qa-model:Q8_0"}"#).await;
    assert_eq!(
        (status, body),
        (
            403,
            JValue::obj([("error", JValue::from("administrator required"))])
        )
    );
    assert_eq!(
        take(&f),
        [
            "node POST /internal/auto-roles/invalidate",
            "node POST /internal/auto-roles/invalidate",
        ]
    );
}

/// A recorded case this front can replay: an administrator's POST, a body as text the port can
/// hold, a router answer it can give (not a throw or an overflow), a checkpoint the bridge takes.
fn replayable(c: &JValue, kind: &str) -> bool {
    let request = c.get("request").as_str().unwrap_or("\u{0}none");
    let script_ok = match c.get("script") {
        JValue::Arr(items) => items
            .first()
            .is_none_or(|s| s.get("reply").is_object() && s.get("reply").get("error").is_nullish()),
        _ => true,
    };
    let tracked = match c.get("downloads") {
        JValue::Arr(d) => d.iter().all(|x| x.get("model").as_str().is_some()),
        _ => true,
    };
    c.get("kind").as_str() == Some(kind)
        && c.get("role").as_str() == Some("admin")
        && c.get("method").as_str() == Some("POST")
        && c.get("barrier").is_nullish()
        && request != "\u{0}none"
        && !request.contains("1e400")
        && script_ok
        && tracked
}

async fn replay(kind: &str, env_kind: Option<&str>) -> usize {
    let fixture = wtf16::parse(include_str!(
        "../../../crates/model-pull/tests/fixtures/node-model-pull.v1.json"
    ))
    .unwrap();
    let JValue::Arr(cases) = fixture.get("pull") else {
        panic!()
    };
    let f = front(true, env_kind).await;
    let mut n = 0;
    for c in cases.iter().filter(|c| replayable(c, kind)) {
        if let JValue::Arr(items) = c.get("script") {
            if let Some(r) = items.first().map(|s| s.get("reply")) {
                let text = match r.get("body") {
                    JValue::Undefined => String::new(),
                    JValue::Str(s) => s.clone(),
                    b => wtf16::stringify(b).unwrap(),
                };
                *f.router.lock().unwrap() = (r.get("status").as_num().unwrap() as u16, text);
            }
        }
        let request = c.get("request").as_str().unwrap();
        let (status, body) = post(f.addr, ADMIN, request.as_bytes()).await;
        let shown = wtf16::stringify(c).unwrap();
        assert_eq!(JValue::Num(f64::from(status)), *c.get("status"), "{shown}");
        assert_eq!(&body, c.get("body"), "{shown}");
        let log = f.log.lock().unwrap().clone();
        f.log.lock().unwrap().clear();
        let count = |line: &str| log.iter().filter(|s| s.line == line).count() as f64;
        assert_eq!(
            count("node POST /internal/auto-roles/invalidate"),
            c.get("invalidated").as_num().unwrap_or(0.0),
            "{shown}"
        );
        assert_eq!(
            count("node POST /internal/model-manager-scan/clear"),
            c.get("scanCleared").as_num().unwrap_or(0.0),
            "{shown}"
        );
        // The router call's body is core's; the tracker calls match its state file.
        let sent: Vec<String> = log
            .iter()
            .filter(|s| s.line == "router POST /models")
            .map(|s| s.body.clone())
            .collect();
        let recorded: Vec<String> = match c.get("calls") {
            JValue::Arr(calls) => calls
                .iter()
                .filter_map(|x| x.get("send").get("body").as_str().map(str::to_owned))
                .collect(),
            _ => vec![],
        };
        assert_eq!(sent, recorded, "{shown}");
        let leases = |m: &str| {
            log.iter()
                .filter(|s| {
                    s.line
                        .starts_with(&format!("node {m} /internal/inference-admission/lease/"))
                        && !s.line.ends_with("/renew")
                })
                .count()
        };
        assert_eq!(leases("PUT"), leases("DELETE"), "{shown}");
        let requested = count("node POST /internal/models/downloads/requested");
        let rejected = count("node POST /internal/models/downloads/rejected");
        match c.get("downloads") {
            JValue::Arr(d) if !d.is_empty() => {
                assert_eq!(requested, 1.0, "{shown}");
                let want = if d[0].get("status").as_str() == Some("rejected") {
                    1.0
                } else {
                    0.0
                };
                assert_eq!(rejected, want, "{shown}");
            }
            _ => assert_eq!(requested + rejected, 0.0, "{shown}"),
        }
        n += 1;
    }
    assert!(n > 0);
    n
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn recorded_llamacpp_pulls_are_answered_as_core_did() {
    // Accepted, rejected (502), an invalid checkpoint (502 with the manager's 400 text), checkpoint
    // required, invalid JSON and the throws core answers: all in the recording.
    let n = replay("llamacpp", Some("llamacpp")).await;
    assert!(n > 100, "{n}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn recorded_pulls_without_a_manager_are_answered_as_core_did() {
    let n = replay("none", None).await;
    assert!(n > 20, "{n}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_requested_call_past_its_deadline_is_rejected_and_the_router_not_asked() {
    let f = front(true, Some("llamacpp")).await;
    {
        let mut n = f.node.lock().unwrap();
        // Core records the request at once and answers after the front's deadline.
        n.requested_delay_ms = 3 * TRACKER_TIMEOUT.as_millis() as u64;
    }
    let started = std::time::Instant::now();
    let (status, body) = post(f.addr, ADMIN, br#"{"checkpoint":"qa-org/qa-slow:Q8_0"}"#).await;
    assert_eq!(
        (status, body),
        (
            503,
            JValue::obj([("error", JValue::from("The download could not be recorded."))])
        )
    );
    assert!(
        started.elapsed() < 2 * TRACKER_TIMEOUT + Duration::from_millis(500),
        "answered at the deadline, not core's late answer: {:?}",
        started.elapsed()
    );
    let log = f.log.lock().unwrap().clone();
    let rejected = log
        .iter()
        .find(|s| s.line == "node POST /internal/models/downloads/rejected")
        .expect("a rejected call");
    assert_eq!(rejected.body, r#"{"model":"qa-org/qa-slow:Q8_0"}"#);
    assert_eq!(
        take(&f),
        [
            "node POST /internal/auto-roles/invalidate",
            "node POST /internal/model-manager-scan/clear",
            "node PUT lease",
            "node POST /internal/models/downloads/requested",
            "node POST /internal/models/downloads/rejected",
            "node DELETE lease",
            "node POST /internal/auto-roles/invalidate",
        ]
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_lease_core_refuses_is_an_internal_error_without_tracker_or_router() {
    let f = front(true, Some("llamacpp")).await;
    // Core's maintenance gate closed: its latch answers the lease with 503 and its reason.
    f.node.lock().unwrap().lease = Some(503);
    let (status, body) = post(f.addr, ADMIN, br#"{"checkpoint":"qa-org/qa-model:Q8_0"}"#).await;
    assert_eq!(
        (status, body),
        (
            503,
            JValue::obj([("error", JValue::from("Internal error"))])
        )
    );
    let steps = take_at_least(&f, 5).await;
    assert!(
        !steps
            .iter()
            .any(|s| s.starts_with("router") || s.contains("downloads")),
        "{steps:?}"
    );
    assert_eq!(
        steps,
        [
            "node POST /internal/auto-roles/invalidate",
            "node POST /internal/model-manager-scan/clear",
            "node PUT lease",
            // The authority gives its refused ticket back (engine-admission's guard).
            "node DELETE lease",
            "node POST /internal/auto-roles/invalidate",
        ]
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_checkpoint_over_512_units_is_not_recorded_and_512_is() {
    let f = front(true, Some("llamacpp")).await;
    *f.router.lock().unwrap() = (200, r#"{"status":"downloading"}"#.into());
    let at = |units: usize| format!("qa-org/{}", "q".repeat(units - "qa-org/".len()));
    let long = at(513);
    let (status, body) = post(
        f.addr,
        ADMIN,
        format!(r#"{{"checkpoint":"{long}"}}"#).as_bytes(),
    )
    .await;
    assert_eq!(
        (status, body),
        (
            503,
            JValue::obj([("error", JValue::from("The download could not be recorded."))])
        )
    );
    let steps = take(&f);
    assert!(
        !steps
            .iter()
            .any(|s| s.starts_with("router") || s.contains("downloads")),
        "{steps:?}"
    );
    assert!(steps.contains(&"node DELETE lease".to_owned()), "{steps:?}");
    let edge = at(512);
    let (status, body) = post(
        f.addr,
        ADMIN,
        format!(r#"{{"checkpoint":"{edge}"}}"#).as_bytes(),
    )
    .await;
    assert_eq!(status, 200, "{}", wtf16::stringify(&body).unwrap());
    assert_eq!(body.get("jobId").as_str(), Some(edge.as_str()));
    let steps = take(&f);
    assert!(
        steps.contains(&"router POST /models".to_owned()),
        "{steps:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_member_s_body_is_never_read() {
    let f = front(true, Some("llamacpp")).await;
    // A body declared at the cap and never sent: an answer that waited for it would not come.
    let (status, body) = send_raw(
        f.addr,
        "/api/models/pull",
        MEMBER,
        1024 * 1024,
        b"",
        Duration::from_secs(10),
    )
    .await;
    assert_eq!(
        (status, body),
        (
            403,
            JValue::obj([("error", JValue::from("administrator required"))])
        )
    );
    assert_eq!(
        take(&f),
        [
            "node POST /internal/auto-roles/invalidate",
            "node POST /internal/auto-roles/invalidate",
        ]
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn dot_segment_spellings_reach_the_front_s_pull() {
    let f = front(true, Some("llamacpp")).await;
    *f.router.lock().unwrap() = (200, r#"{"status":"downloading"}"#.into());
    for path in [
        "/api/models/x/../pull",
        "/api/models/x/%2e%2e/pull",
        "/api/x/../models/./pull",
    ] {
        let (status, body) = post_at(
            f.addr,
            path,
            ADMIN,
            br#"{"checkpoint":"qa-org/qa-model:Q8_0"}"#,
        )
        .await;
        assert_eq!(
            (status, body),
            (
                200,
                JValue::obj([
                    ("jobId", JValue::from("qa-org/qa-model:Q8_0")),
                    ("modelName", JValue::from("qa-org/qa-model:Q8_0")),
                ])
            ),
            "{path}"
        );
        let steps = take(&f);
        assert!(
            !steps.iter().any(|s| s.contains("/api/models")),
            "not relayed to core: {path} {steps:?}"
        );
        assert!(
            steps.contains(&"router POST /models".to_owned()),
            "{path} {steps:?}"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_router_base_is_trimmed_once_as_core_does() {
    // core: `.replace(/\/+$/, '').replace(/\/v1$/, '')`: `<base>/v1/v1/` is `<base>/v1`.
    let f = front_at(true, Some("llamacpp"), "/v1/v1/").await;
    *f.router.lock().unwrap() = (200, r#"{"status":"downloading"}"#.into());
    let (status, body) = post(f.addr, ADMIN, br#"{"checkpoint":"qa-org/qa-model:Q8_0"}"#).await;
    assert_eq!(status, 200, "{}", wtf16::stringify(&body).unwrap());
    let steps = take(&f);
    assert!(
        steps.contains(&"router POST /v1/models".to_owned()),
        "{steps:?}"
    );
}
