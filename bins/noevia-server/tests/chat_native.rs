//! Track R8 end to end: `POST /api/chat` through a real front whose executor switch is `native`, against
//! the recorded Node turns (`crates/chat-turn/tests/fixtures/node-turns.v1.json`). A fake Core on a local
//! port answers the turn with the recorded hand-off (`handoff.raw`, its `prepared` frame pointed at a fake
//! engine), the loopback bridges and the admission latch from the recording; the fake engine answers each
//! round with the recorded bytes. The client's SSE frames, the engine request bodies, the approval cards,
//! the tool runs and the account's state files must be the recording's.
//!
//! What is the front's own and not stubbed: the tool policy (the account's `tool_policies` rows, set
//! from the case's `input.policy`), the tombstones, the transcript (seeded with the turn's user message,
//! as core's preparation leaves it), the state and partial files, the lease's lifetime.
//! What the stubs answer, from the case's `stubs`: the card's question opened and its answer (this test's
//! [`Approvals`]), the chat-wide grant and its redeem (core's grant and execute bridges, D17), the template
//! tools gate and the engine-busy signal (core's bridges).
//!
//! The rewrite before comparing, applied to what the front produced (the recording ran on a fixed clock,
//! counted uuids and user `qa-user`; the front runs on the real ones). Nothing else is rewritten:
//! - a number under `updatedAt`, `observedAt`, `compactedAt`, `savedAt` or `startedAt` that lies inside
//!   this case's run reads as the recording's fixed instant;
//! - a number under `timeToFirstToken` or `elapsedMs` below a minute reads as 0;
//! - the signed-in user's id reads as `qa-user`;
//! - a uuid v4 string, bare or after `ap-`, reads as `<uuid>` (on both sides).
//!
//! Synthetic data only; local ports only; no model runs.
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
use noevia_server::chat_native::Approvals;
use noevia_server::config::Config;
use noevia_server::serve::{serve, Limits};
use server_store::rusqlite::Connection;
use std::collections::{BTreeMap, HashMap};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Notify;

const FIXTURE: &str = include_str!("../../../crates/chat-turn/tests/fixtures/node-turns.v1.json");
const SCHEMA: &str = include_str!("../../../crates/server-store/tests/fixtures/node-schema.sql");
const POLICIES: &str = "CREATE TABLE IF NOT EXISTS tool_policies(user_id TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE, tool TEXT NOT NULL, mode TEXT NOT NULL CHECK(mode IN ('allow','ask','block')), updated_at INTEGER NOT NULL, PRIMARY KEY(user_id, tool))";
const CODE: &str = "synthetic-chat-native-code";
const ORIGIN: &str = "http://127.0.0.1:18021";
const STORE_TOKEN: &str = "synthetic-store-token-0123456789abcdef";
const START: &str = "0123456789abcdef0123456789abcdef";
const FIXED_NOW: f64 = 1_794_571_200_000.0;
const LEASE: &str = "qalease1";
const LEASE_PATH: &str = "/internal/inference-admission/lease/";
/// How long the client keeps reading after the recording says it leaves: what the front sends before it
/// sees the client go is still the client's.
const QUIET: Duration = Duration::from_millis(300);

/// Recorded prepared cases this test does not run, and why. Every other recorded prepared case runs.
const SKIPPED: &[(&str, &str)] = &[
    (
        "engine_unreachable",
        "`throws` carries Node's own transport error text (connect ECONNREFUSED qa-engine.invalid); a refused local socket gives the front's HTTP client's wording, which the event quotes",
    ),
    (
        "edit_allowed_by_policy",
        "a write tool's stored policy is only ever ask or block (tool-policy.cjs mode, policy_leaves::mode): the recorder's stub allows a write to exercise chat.cjs's allow branch, which the crate replay covers",
    ),
    (
        "edit_allow_then_ask",
        "the same: a write is never allowed by the account's policy, so its change to ask cannot happen; the crate replay covers the branch",
    ),
];

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

fn arr(v: &JValue) -> Vec<JValue> {
    match v {
        JValue::Arr(a) => a.clone(),
        _ => Vec::new(),
    }
}

fn base64(text: &str) -> Vec<u8> {
    const ABC: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = Vec::new();
    let mut acc = 0u32;
    let mut bits = 0;
    for b in text.bytes().filter(|b| *b != b'=') {
        acc = (acc << 6) | ABC.iter().position(|x| *x == b).unwrap() as u32;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
            acc &= (1 << bits) - 1;
        }
    }
    out
}

/// The client going away, as the recording has it: `leave` is fired by the fake that reaches the
/// recorded leaving point, `gone` once the client's connection is closed.
#[derive(Default)]
struct Leaving {
    leave: Notify,
    gone: Notify,
    is_gone: AtomicBool,
}

impl Leaving {
    async fn wait_gone(&self) {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        while !self.is_gone.load(Ordering::SeqCst) {
            if tokio::time::timeout_at(deadline, self.gone.notified())
                .await
                .is_err()
            {
                return;
            }
        }
        // The front notices the closed socket a moment later.
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

/// What the fakes answer for the case under test, and what they saw.
struct Script {
    case: JValue,
    engine: String,
    started: f64,
    user: String,
    data: PathBuf,
    loaded: bool,
    round: usize,
    bridge: Vec<JValue>,
    /// The stubs the fakes answer (see the module doc); the rest of `stubs` is the front's own.
    stubs: Vec<JValue>,
    /// "METHOD path" of every call to the fake Core.
    calls: Vec<String>,
    /// Contract breaks seen by the fakes (a fake answering in a server task cannot fail the test).
    errors: Vec<String>,
    handoff_header: Option<String>,
    engine_bodies: Vec<String>,
    engine_leases: Vec<Option<String>>,
    asked: Vec<JValue>,
    executions: Vec<String>,
    /// The ids of the model leases asked for (PUT with a `model`).
    model_leases: Vec<String>,
    /// Set: the turn answers one line and then nothing; the flag is raised when the front drops it.
    quiet: Option<Arc<AtomicBool>>,
    /// Engine streams held open by a `stall` or `leave`.
    held: Vec<http_body_util::channel::Sender<Bytes, std::io::Error>>,
    leaving: Arc<Leaving>,
    /// The chats core's revoke bridge was asked to revoke (Node's revokeChatGrant).
    revokes: Vec<String>,
    /// Set: core's revoke bridge fails, so the front revokes at its own registry instead.
    revoke_fails: bool,
}

impl Default for Script {
    fn default() -> Self {
        Script {
            case: JValue::Null,
            engine: String::new(),
            started: 0.0,
            user: String::new(),
            data: PathBuf::new(),
            loaded: false,
            round: 0,
            bridge: Vec::new(),
            stubs: Vec::new(),
            calls: Vec::new(),
            errors: Vec::new(),
            handoff_header: None,
            engine_bodies: Vec::new(),
            engine_leases: Vec::new(),
            asked: Vec::new(),
            executions: Vec::new(),
            model_leases: Vec::new(),
            quiet: None,
            held: Vec::new(),
            leaving: Arc::default(),
            revokes: Vec::new(),
            revoke_fails: false,
        }
    }
}

type Shared = Arc<Mutex<Script>>;

/// How many of the recorded engine requests core made before the hand-off.
fn pre_cut_requests(case: &JValue) -> usize {
    case.get("handoff")
        .get("requests")
        .as_num()
        .map_or(0, |n| n as usize)
}

/// Every model lease the turn took was given back (a DELETE of its id).
fn model_lease_released(sc: &Script) -> bool {
    !sc.model_leases.is_empty()
        && sc.model_leases.iter().all(|id| {
            let gone = format!("DELETE {LEASE_PATH}{id}");
            sc.calls.iter().any(|c| c == &gone)
        })
}

/// The handle the fake core's grant bridge gives (one per case is enough: each is redeemed at once).
const GRANT_HANDLE: &str = "0123456789abcdef0123456789abcdef";

fn ok_granted(granted: bool) -> Response<UpBody> {
    json(
        200,
        &if granted {
            js::obj([
                ("granted", JValue::Bool(true)),
                ("handle", js::s(GRANT_HANDLE)),
            ])
        } else {
            js::obj([("granted", JValue::Bool(false))])
        },
    )
}

/// Recorded cases also run against the front's own approval registry, and the answer the client gives
/// each card through the public decision route.
const REGISTRY_CASES: &[(&str, &str)] = &[
    ("tool_approved", "approve"),
    ("tool_declined", "deny"),
    ("edit_approved", "approve_all"),
    // The client leaves when the card shows: the question is withdrawn (#679), a late answer finds nothing.
    ("approval_left", "leave"),
    // #2446: approve_all on the coworker page save's once card approves that call only, no grant is stored.
    ("page_save_approved", "approve_all"),
];

/// Recorded cases also run with core's revoke bridge failing.
const REVOKE_FAILS: &[&str] = &["grant_redeem_refused"];

/// The stubs this test's fakes answer.
const ANSWERED: &[&str] = &[
    "chatWideApproved",
    "openApproval",
    "awaitApproval",
    "redeemChatGrant",
    "allowsTools",
    "inferenceBusy",
    "supervise",
];

impl Script {
    /// The next recorded stub `op`, or a contract break.
    fn stub(&mut self, op: &str) -> Option<JValue> {
        match self
            .stubs
            .iter()
            .position(|s| s.get("op").as_str() == Some(op))
        {
            Some(at) => Some(self.stubs.remove(at)),
            None => {
                self.errors
                    .push(format!("{op} asked more often than Node asked it"));
                None
            }
        }
    }
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

/// The hand-off on the front's NDJSON wire (chat port S7, core rust-chat-wire.cjs: `JSON.stringify(event)`
/// and a line break where the recording's event stream has `data: <same>` and a blank line): the recorded
/// lines, the `prepared` one pointed at the fake engine and started now.
fn handoff(sc: &Script) -> Response<UpBody> {
    let mut lines: Vec<String> = arr(sc.case.get("handoff").get("raw"))
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
    let last = lines.pop().unwrap();
    let mut prepared = wtf16::parse(last.trim_end()).unwrap();
    js::set(&mut prepared, "upstreamUrl", js::s(sc.engine.clone()));
    js::set(&mut prepared, "startedAt", js::n(sc.started));
    lines.push(format!("{}\n", js::stringify(&prepared)));
    let (mut tx, b) = http_body_util::channel::Channel::<Bytes, std::io::Error>::new(64);
    tokio::spawn(async move {
        for l in lines {
            if tx.send_data(Bytes::from(l)).await.is_err() {
                return;
            }
        }
    });
    Response::builder()
        .status(200)
        .header("content-type", "application/x-ndjson")
        .header("cache-control", "no-store")
        .body(b.boxed())
        .unwrap()
}

/// A turn answer that sends one line and then waits forever; dropping it raises `dropped`.
struct Quiet {
    first: Option<Bytes>,
    dropped: Arc<AtomicBool>,
}

impl http_body::Body for Quiet {
    type Data = Bytes;
    type Error = std::io::Error;

    fn poll_frame(
        mut self: std::pin::Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Result<http_body::Frame<Bytes>, Self::Error>>> {
        match self.first.take() {
            Some(b) => std::task::Poll::Ready(Some(Ok(http_body::Frame::data(b)))),
            None => std::task::Poll::Pending,
        }
    }
}

impl Drop for Quiet {
    fn drop(&mut self) {
        self.dropped.store(true, Ordering::SeqCst);
    }
}

/// Review B1: a client that leaves while core prepares and writes nothing ends core's turn at once (its
/// stream is dropped, as the relay of a Node turn drops it), not at core's next line.
async fn a_client_leaving_while_core_is_quiet_ends_cores_stream(
    case: &JValue,
) -> Result<(), String> {
    let dropped = Arc::new(AtomicBool::new(false));
    let script: Shared = Arc::new(Mutex::new(Script {
        case: case.clone(),
        quiet: Some(Arc::clone(&dropped)),
        ..Script::default()
    }));
    let s = Arc::clone(&script);
    let core = listen(move |r| core_service(Arc::clone(&s), r)).await;
    let f = front(core).await;
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
        .body(Full::new(Bytes::from(js::stringify(
            case.get("input").get("body"),
        ))))
        .unwrap();
    let res = send.send_request(req).await.unwrap();
    let mut body = res.into_body();
    let first = tokio::time::timeout(Duration::from_secs(10), body.frame()).await;
    if !matches!(first, Ok(Some(Ok(_)))) {
        return Err(format!("quiet core: no first frame ({first:?})"));
    }
    drop(body);
    drop(send);
    conn.abort();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while !dropped.load(Ordering::SeqCst) {
        if tokio::time::Instant::now() >= deadline {
            return Err("quiet core: the front kept core's stream after the client left".into());
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    Ok(())
}

fn health(sc: &Script) -> JValue {
    let input = sc.case.get("input");
    let model = input
        .get("project")
        .get("model")
        .as_str()
        .unwrap_or("qa-model")
        .to_string();
    let rows = if sc.loaded {
        vec![js::obj([
            ("model_name", js::s(model)),
            ("loaded", JValue::Bool(true)),
            (
                "recipe_options",
                js::obj([("ctx_size", input.get("ctxSize").clone())]),
            ),
        ])]
    } else {
        Vec::new()
    };
    js::obj([
        ("ok", JValue::Bool(true)),
        ("body", js::obj([("all_models_loaded", JValue::Arr(rows))])),
    ])
}

/// The recorded tool bridge call: every field Node passed, the user's id read as `qa-user`.
fn tool_bridge(sc: &mut Script, op: &str, sent: &JValue) -> Option<JValue> {
    if sc.bridge.is_empty() {
        sc.errors
            .push(format!("an unrecorded {op}: {}", js::stringify(sent)));
        return None;
    }
    let next = sc.bridge.remove(0);
    if next.get("op").as_str() != Some(op) {
        sc.errors.push(format!(
            "{op} asked where Node asked {}",
            js::stringify(&next)
        ));
        return None;
    }
    // The fields of a recorded call that are core's answer, not what the front sends.
    let answers: &[&str] = match op {
        "skills-revoked" => &["gone"],
        "skills-read" => &["loaded", "ledger"],
        "skills-scrub" => &["changed", "removed"],
        _ => &[],
    };
    if let JValue::Obj(fields) = &next {
        for (k, v) in fields {
            if matches!(
                k.as_str(),
                "op" | "text" | "failed" | "threw" | "left" | "found" | "fenced" | "notRun"
            ) || answers.contains(&k.as_str())
            {
                continue;
            }
            let mut got = sent.get(k).clone();
            settle(&mut got, &sc.user, (0.0, 0.0));
            let mut want = v.clone();
            settle(&mut want, "", (0.0, 0.0));
            if js::stringify(&got) != js::stringify(&want) {
                sc.errors.push(format!(
                    "{op} {k}: front {} node {}",
                    js::stringify(&got),
                    js::stringify(&want)
                ));
            }
        }
    }
    Some(next)
}

async fn core_service(
    s: Shared,
    req: Request<Incoming>,
) -> Result<Response<UpBody>, std::io::Error> {
    let hold = req.uri().query() == Some("hold=1");
    let (method, path, headers, body) = read_req(req).await;
    let sent = if body.is_empty() {
        JValue::Null
    } else {
        wtf16::parse(&body).unwrap_or(JValue::Null)
    };
    let mut sc = s.lock().unwrap();
    sc.calls.push(format!("{method} {path}"));
    let input = sc.case.get("input").clone();
    // The bridge contract (rust-chat-native.cjs): every native, usage and prefill body names the user.
    let bridged = path.starts_with("/internal/chat/native/")
        || path == "/internal/chat/usage"
        || path == "/internal/chat/prefill"
        || path.starts_with("/internal/chat/tools/");
    if bridged {
        if headers.get("x-noevia-store-token").map(String::as_str) != Some(STORE_TOKEN) {
            sc.errors.push(format!("{path}: without the store token"));
        }
        if sent.get("user").as_str() != Some(sc.user.as_str()) {
            sc.errors
                .push(format!("{path}: user {}", js::stringify(sent.get("user"))));
        }
    }
    if path == "/internal/chat/gate" {
        if hold {
            let (mut tx, b) = http_body_util::channel::Channel::<Bytes, std::io::Error>::new(1);
            tokio::spawn(async move {
                let _ = tx.send_data(Bytes::from_static(b"held\n")).await;
                while tx.send_data(Bytes::new()).await.is_ok() {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            });
            return Ok(Response::builder()
                .status(200)
                .header("x-noevia-gate-lease", "lease-1")
                .body(b.boxed())
                .unwrap());
        }
        return Ok(empty(204));
    }
    if let Some(rest) = path.strip_prefix(LEASE_PATH) {
        if method == "PUT" {
            // The model's lease ({model}) is the one the case refuses; the chat route's own admission
            // ({gate}, chat port S8) is granted.
            let for_model = !matches!(sent.get("model"), JValue::Undefined);
            if for_model {
                sc.model_leases.push(rest.to_string());
            }
            let refusal = input.get("leaseRefusal");
            if refusal.is_object() && for_model {
                return Ok(json(
                    503,
                    &js::obj([
                        ("error", refusal.get("publicMessage").clone()),
                        ("code", refusal.get("code").clone()),
                    ]),
                ));
            }
            return Ok(json(
                201,
                &js::obj([
                    ("lease", js::s(rest)),
                    ("ttlMs", js::n(30_000.0)),
                    ("headers", js::obj([("x-noevia-lease", js::s(LEASE))])),
                ]),
            ));
        }
        return Ok(if rest.ends_with("/renew") {
            empty(200)
        } else {
            empty(204)
        });
    }
    let ok = |v: JValue| Ok(json(200, &v));
    match path.as_str() {
        "/internal/chat/turn" if sc.quiet.is_some() => {
            let dropped = sc.quiet.clone().unwrap();
            Ok(Response::builder()
                .status(200)
                .header("content-type", "application/x-ndjson")
                .body(
                    Quiet {
                        first: Some(Bytes::from_static(
                            b"{\"type\":\"meta\",\"model\":\"qa-model\"}\n",
                        )),
                        dropped,
                    }
                    .boxed(),
                )
                .unwrap())
        }
        "/internal/chat/turn" => {
            sc.handoff_header = headers.get("x-noevia-chat-handoff").cloned();
            if headers
                .get("x-noevia-chat-handoff-caps")
                .map(String::as_str)
                != Some(chat_turn::handoff::HANDOFF_CAPS)
            {
                sc.errors.push(format!(
                    "turn: caps header {:?}",
                    headers.get("x-noevia-chat-handoff-caps")
                ));
            }
            if headers.get("x-noevia-chat-wire").map(String::as_str) != Some("ndjson/1") {
                sc.errors.push(format!(
                    "turn: wire header {:?}",
                    headers.get("x-noevia-chat-wire")
                ));
            }
            // A policy the account changed once core's preparation ended (`input.policyAfterCut`, R15): stored as
            // the prepared frame goes out, so the front reads it from the cut on.
            if let JValue::Obj(after) = sc.case.get("input").get("policyAfterCut").clone() {
                for (tool, mode) in after {
                    match mode.as_str() {
                        Some(mode) => set_policy(&sc.data, &sc.user, &tool, mode),
                        None => sc
                            .errors
                            .push(format!("policyAfterCut {tool}: not one mode")),
                    }
                }
            }
            Ok(handoff(&sc))
        }
        "/internal/chat/native/health" => ok(health(&sc)),
        "/internal/chat/native/load" => {
            if sent.get("lease").as_str() != Some(LEASE) {
                sc.errors
                    .push(format!("load: lease {}", js::stringify(sent.get("lease"))));
            }
            sc.loaded = true;
            ok(js::obj([("ok", JValue::Bool(true))]))
        }
        "/internal/chat/native/tools-allowed" => {
            let allowed = sc
                .stub("allowsTools")
                .is_none_or(|s| s.get("answer") != &JValue::Bool(false));
            ok(js::obj([("allowed", JValue::Bool(allowed))]))
        }
        // Track R15: core's step supervisor, answered from the recorded decide (its question checked).
        "/internal/chat/native/supervise" => {
            let Some(rec) = sc.stub("supervise") else {
                sc.errors.push("supervise: not asked in Node".into());
                return ok(js::obj([("action", js::s("continue"))]));
            };
            for k in ["round", "goal", "wrote", "outputs"] {
                if js::stringify(sent.get(k)) != js::stringify(rec.get(k)) {
                    sc.errors.push(format!(
                        "supervise: {k} {} (Node {})",
                        js::stringify(sent.get(k)),
                        js::stringify(rec.get(k))
                    ));
                }
            }
            ok(js::obj([("action", rec.get("answer").clone())]))
        }
        "/internal/chat/native/busy" => {
            // At the title the turn holds the route's admission lease and the model's: two counts.
            if sent.get("own").as_num() != Some(2.0) {
                sc.errors
                    .push(format!("busy: own {}", js::stringify(sent.get("own"))));
            }
            let busy = sc
                .stub("inferenceBusy")
                .is_some_and(|s| s.get("answer") == &JValue::Bool(true));
            ok(js::obj([("busy", JValue::Bool(busy))]))
        }
        "/internal/chat/native/title-auto-named" => {
            let named = input.get("titles").get("autoNamed") != &JValue::Bool(false);
            ok(js::obj([("autoNamed", JValue::Bool(named))]))
        }
        "/internal/chat/tools/target" => {
            let Some(next) = tool_bridge(&mut sc, "target", &sent) else {
                return Ok(json(500, &js::obj([("error", js::s("unrecorded"))])));
            };
            let found = next.get("found");
            ok(if found.is_object() {
                found.clone()
            } else {
                js::obj(Vec::<(String, JValue)>::new())
            })
        }
        "/internal/chat/skills/revoked" => {
            let Some(next) = tool_bridge(&mut sc, "skills-revoked", &sent) else {
                return Ok(json(500, &js::obj([("error", js::s("unrecorded"))])));
            };
            ok(js::obj([("gone", next.get("gone").clone())]))
        }
        "/internal/chat/skills/read" => {
            let Some(next) = tool_bridge(&mut sc, "skills-read", &sent) else {
                return Ok(json(500, &js::obj([("error", js::s("unrecorded"))])));
            };
            // Core records the read in the account's skill ledger, as Node's turn did.
            if next.get("ledger").is_object() {
                if let Ok(dir) = server_projects::store::user_dir(&sc.data, &sc.user) {
                    let _ = std::fs::write(
                        dir.join("skill-history.json"),
                        js::stringify(next.get("ledger")),
                    );
                }
            }
            ok(js::obj([("loaded", next.get("loaded").clone())]))
        }
        "/internal/chat/skills/scrub" => {
            let Some(next) = tool_bridge(&mut sc, "skills-scrub", &sent) else {
                return Ok(json(500, &js::obj([("error", js::s("unrecorded"))])));
            };
            ok(js::obj([
                ("changed", next.get("changed").clone()),
                ("removed", next.get("removed").clone()),
            ]))
        }
        "/internal/chat/tools/edit-target" => {
            let Some(next) = tool_bridge(&mut sc, "edit-target", &sent) else {
                return Ok(json(500, &js::obj([("error", js::s("unrecorded"))])));
            };
            ok(next.get("found").clone())
        }
        "/internal/chat/tools/grant" => {
            // Node's chatWideApproved (D17): the recorded answer, under a handle the execute call carries.
            let Some(stub) = sc.stub("chatWideApproved") else {
                sc.errors.push("grant: Node asked no grant check".into());
                return Ok(ok_granted(false));
            };
            if stub.get("chatId").as_str() != sent.get("chat").as_str()
                || stub.get("scope").as_str() != sent.get("scope").as_str()
                || !sent.get("binding").is_object()
                || sent.get("exchangeKey").as_str().is_none_or(str::is_empty)
            {
                sc.errors.push(format!(
                    "grant: asked {}, Node asked {}",
                    js::stringify(&sent),
                    js::stringify(&stub)
                ));
            }
            let granted = stub.get("answer") == &JValue::Bool(true);
            if input.get("approvalFront").get("leaveInGrant") == &JValue::Bool(true) {
                // The client leaves while the check is out; the answer still comes once it has gone.
                let leaving = Arc::clone(&sc.leaving);
                drop(sc);
                let (mut tx, b) = http_body_util::channel::Channel::<Bytes, std::io::Error>::new(1);
                tokio::spawn(async move {
                    leaving.leave.notify_one();
                    leaving.wait_gone().await;
                    let answer = if granted {
                        js::obj([
                            ("granted", JValue::Bool(true)),
                            ("handle", js::s(GRANT_HANDLE)),
                        ])
                    } else {
                        js::obj([("granted", JValue::Bool(false))])
                    };
                    let _ = tx.send_data(Bytes::from(js::stringify(&answer))).await;
                });
                return Ok(Response::builder()
                    .status(200)
                    .header("content-type", "application/json")
                    .body(b.boxed())
                    .unwrap());
            }
            Ok(ok_granted(granted))
        }
        "/internal/chat/tools/revoke" => {
            sc.revokes
                .push(sent.get("chat").as_str().unwrap_or("").to_string());
            if sc.revoke_fails {
                return Ok(json(500, &js::obj([("error", js::s("QA revoke failure"))])));
            }
            ok(js::obj([("revoked", JValue::Bool(true))]))
        }
        "/internal/chat/tools/execute" => {
            if sent.get("grant").is_object() {
                if sent.get("grant").get("handle").as_str() != Some(GRANT_HANDLE) {
                    sc.errors.push(format!(
                        "execute: grant {}",
                        js::stringify(sent.get("grant"))
                    ));
                }
                let redeemed = sc.stub("redeemChatGrant").unwrap_or(JValue::Null);
                match redeemed.get("answer").as_str() {
                    Some("refused") => return ok(js::obj([("redeem", js::s("refused"))])),
                    Some("unavailable") => return ok(js::obj([("redeem", js::s("unavailable"))])),
                    _ => {}
                }
            }
            let Some(next) = tool_bridge(&mut sc, "execute", &sent) else {
                return Ok(json(500, &js::obj([("error", js::s("unrecorded"))])));
            };
            if (next.get("fenced") == &JValue::Bool(true)) != sent.get("grant").is_object() {
                sc.errors.push(format!(
                    "execute: grant {} where Node's run was fenced: {}",
                    js::stringify(sent.get("grant")),
                    next.get("fenced") == &JValue::Bool(true)
                ));
            }
            sc.executions
                .push(next.get("name").as_str().unwrap_or("").to_string());
            if let Some(message) = next.get("threw").as_str() {
                return Ok(json(500, &js::obj([("error", js::s(message))])));
            }
            let mut answer = vec![
                ("text", next.get("text").clone()),
                (
                    "failed",
                    JValue::Bool(next.get("failed") == &JValue::Bool(true)),
                ),
            ];
            if next.get("notRun") == &JValue::Bool(true) {
                answer.push(("notRun", JValue::Bool(true)));
            }
            let answer = js::obj(answer);
            if next.get("left") == &JValue::Bool(true) {
                // The client leaves while the tool runs; the run still finishes and answers.
                let leaving = Arc::clone(&sc.leaving);
                drop(sc);
                let (mut tx, b) = http_body_util::channel::Channel::<Bytes, std::io::Error>::new(1);
                tokio::spawn(async move {
                    leaving.leave.notify_one();
                    leaving.wait_gone().await;
                    let _ = tx.send_data(Bytes::from(js::stringify(&answer))).await;
                });
                return Ok(Response::builder()
                    .status(200)
                    .header("content-type", "application/json")
                    .body(b.boxed())
                    .unwrap());
            }
            ok(answer)
        }
        "/internal/chat/usage"
        | "/internal/chat/prefill"
        | "/internal/chat/native/title-save"
        | "/internal/chat/native/tools-unsupported" => Ok(empty(204)),
        _ => Ok(json(404, &js::obj([("error", js::s("not found"))]))),
    }
}

async fn engine_service(
    s: Shared,
    req: Request<Incoming>,
) -> Result<Response<UpBody>, std::io::Error> {
    let (_, _, headers, body) = read_req(req).await;
    let (round, chunks) = {
        let mut sc = s.lock().unwrap();
        sc.engine_bodies.push(body);
        sc.engine_leases
            .push(headers.get("x-noevia-lease").cloned());
        let i = sc.round;
        sc.round += 1;
        let input = sc.case.get("input").clone();
        let Some(round) = arr(input.get("rounds")).get(i).cloned() else {
            sc.errors.push(format!("no engine round {}", i + 1));
            return Err(std::io::Error::other("no such round"));
        };
        let chunks = arr(input.get("chunks"))
            .get(i)
            .cloned()
            .unwrap_or(JValue::Null);
        (round, chunks)
    };
    if round.get("throws").as_str().is_some() {
        // The connection closes without an answer.
        return Err(std::io::Error::other("the engine went away"));
    }
    if let Some(status) = round.get("status").as_num().filter(|n| *n != 200.0) {
        let text = round.get("body").as_str().unwrap_or("").to_string();
        return Ok(full(status as u16, "application/json", text));
    }
    if round.get("json").is_object() {
        return Ok(json(200, round.get("json")));
    }
    let (mut tx, b) = http_body_util::channel::Channel::<Bytes, std::io::Error>::new(64);
    let s2 = Arc::clone(&s);
    tokio::spawn(async move {
        for c in arr(&chunks) {
            match &c {
                JValue::Str(m) if m == "wait" => {
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
                JValue::Str(m) if m == "stall" || m == "leave" => {
                    let mut sc = s2.lock().unwrap();
                    if m == "leave" {
                        sc.leaving.leave.notify_one();
                    }
                    // Held open: nothing more is sent.
                    sc.held.push(tx);
                    return;
                }
                JValue::Str(_) => return,
                // The recorder's clock moves on (a skill check is due once a second): so does this one.
                _ if c.get("tick").as_num().is_some() => {
                    let ms = c.get("tick").as_num().unwrap_or(0.0).max(0.0);
                    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
                    tokio::time::sleep(Duration::from_millis(ms as u64 + 20)).await;
                }
                _ => {
                    let bytes = match c.get("text").as_str() {
                        Some(t) => t.as_bytes().to_vec(),
                        None => base64(c.get("b64").as_str().unwrap()),
                    };
                    if tx.send_data(Bytes::from(bytes)).await.is_err() {
                        return;
                    }
                }
            }
        }
    });
    Ok(Response::builder()
        .status(200)
        .header("content-type", "text/event-stream")
        .body(b.boxed())
        .unwrap())
}

/// The approval registry the test installs: native_turns.rs's binding; the chat-wide grant and the
/// card's answer from the case's stubs. A card the recording left open (`left`) makes the client leave
/// and never answers; a policy the case turns to `block` on the card is written as the card answers.
struct TestApprovals(Arc<Mutex<Option<Shared>>>);

impl TestApprovals {
    fn script(&self) -> Shared {
        self.0.lock().unwrap().clone().unwrap()
    }
}

impl Approvals for TestApprovals {
    fn binding(&self, user: &str, chat: &str, scope: &str, _holder: &JValue) -> Option<JValue> {
        let context = js::obj([
            ("canonicalChatId", js::s(chat)),
            ("holder", js::obj([("kind", js::s("absent"))])),
            ("generation", js::s("0")),
            ("start", js::s(START)),
            ("tombstoned", JValue::Bool(false)),
        ]);
        Some(js::obj([
            ("user", js::s(user)),
            ("canonicalChatId", js::s(chat)),
            ("scope", js::s(scope)),
            ("context", context),
        ]))
    }

    fn open(&self, _user: &str, _request: &JValue) -> bool {
        let s = self.script();
        let mut sc = s.lock().unwrap();
        sc.stub("openApproval")
            .is_some_and(|stub| stub.get("answer") == &JValue::Bool(true))
    }

    fn wait(
        &self,
        _user: &str,
        request: &JValue,
    ) -> Pin<Box<dyn std::future::Future<Output = String> + Send>> {
        let s = self.script();
        let mut sc = s.lock().unwrap();
        sc.asked.push(request.clone());
        let stub = sc.stub("awaitApproval").unwrap_or(JValue::Null);
        if stub.get("left") == &JValue::Bool(true) || stub.get("answer").as_str() == Some("aborted")
        {
            sc.leaving.leave.notify_one();
            return Box::pin(std::future::pending());
        }
        // A policy that changes while the card is open (`input.policy[tool]: [before, after]`).
        if let JValue::Obj(policy) = sc.case.get("input").get("policy").clone() {
            for (tool, modes) in policy {
                if let Some(after) = arr(&modes).get(1).and_then(|m| m.as_str()) {
                    set_policy(&sc.data, &sc.user, &tool, after);
                }
            }
        }
        let answer = stub.get("answer").as_str().unwrap_or("approve").to_string();
        Box::pin(async move { answer })
    }

    fn cancel(&self, _user: &str, _id: &str) {}
}

fn set_policy(data: &Path, user: &str, tool: &str, mode: &str) {
    let c = Connection::open(server_store::db_path(data)).unwrap();
    c.busy_timeout(Duration::from_secs(5)).unwrap();
    c.execute_batch(POLICIES).unwrap();
    c.execute(
        "INSERT INTO tool_policies(user_id,tool,mode,updated_at) VALUES(?1,?2,?3,1) ON CONFLICT(user_id,tool) DO UPDATE SET mode=excluded.mode",
        [user, tool, mode],
    )
    .unwrap();
}

struct Front {
    _dir: tempfile::TempDir,
    data: PathBuf,
    addr: SocketAddr,
    jar: String,
    csrf: String,
    user: String,
    /// The front's own approval registry (S9), to read what a card's answer left behind.
    approvals: Arc<noevia_server::approval_registry::Registry>,
}

async fn front(core: SocketAddr) -> Front {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().to_path_buf();
    let c = Connection::open(server_store::db_path(&data)).unwrap();
    c.execute_batch(SCHEMA).unwrap();
    c.execute_batch(POLICIES).unwrap();
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
    let approvals = Arc::clone(&app.approvals);
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(serve(
        listener,
        app,
        Limits::default(),
        std::future::pending(),
    ));
    let setup = format!(
        r#"{{"setupCode":"{CODE}","username":"owner","password":"synthetic chat native password","diaryEnabled":false}}"#
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
        approvals,
    }
}

/// POST /api/chat and its SSE body: all of it, or what came until the client left (`leave` fired and
/// then [`QUIET`] without a frame).
/// With `decide`, each card the stream shows is answered with it through POST /api/tool-approvals/{id}
/// (its id and the route's status are returned).
async fn chat(
    f: &Front,
    body: &str,
    leaving: &Leaving,
    decide: Option<&str>,
) -> (u16, String, Vec<(String, u16)>) {
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
    let mut body = res.into_body();
    let mut out = Vec::new();
    let mut decided: Vec<(String, u16)> = Vec::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
    let mut left = false;
    loop {
        let next = if left {
            match tokio::time::timeout(QUIET, body.frame()).await {
                Ok(f) => f,
                Err(_) => break,
            }
        } else {
            tokio::select! {
                f = body.frame() => f,
                () = leaving.leave.notified() => { left = true; continue; }
                () = tokio::time::sleep_until(deadline) => panic!("the turn did not end within 60 s"),
            }
        };
        match next {
            Some(Ok(frame)) => {
                if let Ok(data) = frame.into_data() {
                    out.extend_from_slice(&data);
                }
            }
            _ => break,
        }
        if let Some(decision) = decide {
            let text = String::from_utf8_lossy(&out).to_string();
            for frame in text.split("\n\n") {
                let Some(data) = frame.strip_prefix("data: ") else {
                    continue;
                };
                let Ok(event) = wtf16::parse(data) else {
                    continue;
                };
                let Some(id) = event.get("id").as_str().map(str::to_string) else {
                    continue;
                };
                if event.get("type").as_str() == Some("tool_pending")
                    && !decided.iter().any(|(d, _)| *d == id)
                {
                    let status = if decision == "leave" {
                        leaving.leave.notify_one();
                        0
                    } else {
                        decide_card(f, &id, decision).await
                    };
                    decided.push((id, status));
                }
            }
        }
    }
    drop(body);
    drop(send);
    conn.abort();
    if left {
        leaving.is_gone.store(true, Ordering::SeqCst);
        leaving.gone.notify_one();
    }
    (status, String::from_utf8(out).unwrap(), decided)
}

/// POST /api/tool-approvals/{id} as the page sends it: the route's status.
async fn decide_card(f: &Front, id: &str, decision: &str) -> u16 {
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
        .uri(format!("/api/tool-approvals/{id}"))
        .header("host", "front.test")
        .header("origin", ORIGIN)
        .header("content-type", "application/json")
        .header("cookie", &f.jar)
        .header("x-csrf-token", &f.csrf)
        .body(Full::new(Bytes::from(js::stringify(&js::obj([(
            "decision",
            js::s(decision),
        )])))))
        .unwrap();
    let status = tokio::time::timeout(Duration::from_secs(10), send.send_request(req))
        .await
        .map_or(0, |r| r.map_or(0, |r| r.status().as_u16()));
    drop(send);
    conn.abort();
    status
}

/// The `data:` frames, keep-alives and other comments removed.
fn frames(sse: &str) -> Vec<String> {
    sse.split("\n\n")
        .filter_map(|f| f.strip_prefix("data: "))
        .map(str::to_string)
        .collect()
}

fn is_uuid_v4(s: &str) -> bool {
    let b = s.as_bytes();
    s.len() == 36
        && [8, 13, 18, 23].iter().all(|i| b[*i] == b'-')
        && b[14] == b'4'
        && s.bytes().all(|c| c == b'-' || c.is_ascii_hexdigit())
}

/// The rewrite the module doc lists. `user` empty: the recorded side (uuids only).
fn settle(v: &mut JValue, user: &str, window: (f64, f64)) {
    match v {
        JValue::Obj(pairs) => {
            for (k, x) in pairs.iter_mut() {
                if let JValue::Num(n) = x {
                    let at = matches!(
                        k.as_str(),
                        "updatedAt" | "observedAt" | "compactedAt" | "savedAt" | "startedAt"
                    );
                    if at && *n >= window.0 && *n <= window.1 {
                        *n = FIXED_NOW;
                    }
                    if matches!(k.as_str(), "timeToFirstToken" | "elapsedMs")
                        && *n >= 0.0
                        && *n < 60_000.0
                        && !user.is_empty()
                    {
                        *n = 0.0;
                    }
                } else {
                    settle(x, user, window);
                }
            }
        }
        JValue::Arr(items) => items.iter_mut().for_each(|x| settle(x, user, window)),
        JValue::Str(s) => {
            if !user.is_empty() && s == user {
                *s = "qa-user".into();
            } else if is_uuid_v4(s) {
                *s = "<uuid>".into();
            } else if s.strip_prefix("ap-").is_some_and(is_uuid_v4) {
                *s = "ap-<uuid>".into();
            }
        }
        _ => {}
    }
}

/// The state files in the account's directory: `context-*.json` and `chat-partials/*`.
fn state_files(dir: &Path, user: &str, window: (f64, f64)) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return out;
    };
    let mut add = |name: String, path: &Path| {
        let mut v = wtf16::parse(&std::fs::read_to_string(path).unwrap()).unwrap();
        settle(&mut v, user, window);
        out.insert(name, js::stringify(&v));
    };
    for e in entries.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        // The skill ledger is core's (written through the skills bridges), listed as Node's run lists it.
        if (name.starts_with("context-") && name.ends_with(".json")) || name == "skill-history.json"
        {
            add(name, &e.path());
        } else if name == "chat-partials" {
            for p in std::fs::read_dir(e.path()).unwrap().flatten() {
                let n = p.file_name().to_string_lossy().into_owned();
                if n.ends_with(".json") {
                    add(format!("chat-partials/{n}"), &p.path());
                }
            }
        }
    }
    out
}

fn now_ms() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as f64
}

fn texts(list: &[JValue], user: &str, window: (f64, f64)) -> Vec<String> {
    list.iter()
        .map(|v| {
            let mut v = v.clone();
            settle(&mut v, user, window);
            js::stringify(&v)
        })
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn the_native_executor_answers_as_the_recorded_node_turns() {
    let fixture = wtf16::parse(FIXTURE).unwrap();
    let cases: Vec<JValue> = arr(fixture.get("cases"))
        .into_iter()
        .filter(|c| c.get("handoff").get("prepared").is_object())
        .collect();
    for (name, why) in SKIPPED {
        assert!(
            cases.iter().any(|c| c.get("name").as_str() == Some(*name)),
            "{name}: skipped but not recorded"
        );
        eprintln!("SKIPPED {name}: {why}");
    }
    // The front's admission authority needs a Node start to issue leases under.
    engine_admission::authority::global()
        .node_session("qa-chat-native-boot", 1)
        .unwrap();
    let current: Arc<Mutex<Option<Shared>>> = Arc::default();
    let scripted = || {
        noevia_server::chat_native::install_approvals(Some(Arc::new(TestApprovals(Arc::clone(
            &current,
        )))));
    };
    scripted();

    let only = std::env::var("CHAT_NATIVE_CASE").ok();
    let mut ran = Vec::new();
    let mut failures = Vec::new();
    for case in &cases {
        let name = case.get("name").as_str().unwrap().to_string();
        if SKIPPED.iter().any(|(n, _)| *n == name) || only.as_ref().is_some_and(|o| *o != name) {
            continue;
        }
        match run_case(&fixture, case, &current, None, false).await {
            Ok(()) => ran.push(name),
            Err(e) => {
                eprintln!("DIFFERS {name}: {e}");
                failures.push(format!("{name}: {e}"));
            }
        }
    }
    // The front's own approval registry (S9) instead of the scripted answers: the client answers each card
    // through POST /api/tool-approvals/{id}, as the page does, and the turn is still the recorded one.
    noevia_server::chat_native::install_approvals(None);
    for (name, decision) in REGISTRY_CASES {
        if only.as_ref().is_some_and(|o| o != name) {
            continue;
        }
        let case = cases
            .iter()
            .find(|c| c.get("name").as_str() == Some(name))
            .unwrap();
        match run_case(&fixture, case, &current, Some(decision), false).await {
            Ok(()) => ran.push(format!("{name} (front registry)")),
            Err(e) => {
                eprintln!("DIFFERS {name} (front registry): {e}");
                failures.push(format!("{name} (front registry): {e}"));
            }
        }
    }
    scripted();
    // Core's revoke bridge failing: the front revokes at its own registry, which asks Node to acknowledge.
    for name in REVOKE_FAILS {
        if only.as_ref().is_some_and(|o| o != name) {
            continue;
        }
        let case = cases
            .iter()
            .find(|c| c.get("name").as_str() == Some(name))
            .unwrap();
        match run_case(&fixture, case, &current, None, true).await {
            Ok(()) => ran.push(format!("{name} (revoke bridge fails)")),
            Err(e) => {
                eprintln!("DIFFERS {name} (revoke bridge fails): {e}");
                failures.push(format!("{name} (revoke bridge fails): {e}"));
            }
        }
    }
    if only.is_none() {
        if let Err(e) = a_client_leaving_while_core_is_quiet_ends_cores_stream(&cases[0]).await {
            failures.push(e);
        }
    }
    eprintln!("ran and matched ({}): {ran:?}", ran.len());
    assert!(
        failures.is_empty(),
        "{} differ:\n{}",
        failures.len(),
        failures.join("\n\n")
    );
    if only.is_none() {
        assert_eq!(
            ran.len() + SKIPPED.len(),
            cases.len() + REGISTRY_CASES.len() + REVOKE_FAILS.len()
        );
    }
}

async fn run_case(
    fixture: &JValue,
    case: &JValue,
    current: &Arc<Mutex<Option<Shared>>>,
    decide: Option<&str>,
    revoke_fails: bool,
) -> Result<(), String> {
    let input = case.get("input").clone();
    // With the front's registry its cards are the registry's: their recorded stubs are not asked for.
    let stubs = arr(case.get("stubs"))
        .into_iter()
        .filter(|s| ANSWERED.contains(&s.get("op").as_str().unwrap_or("")))
        .filter(|s| {
            decide.is_none()
                || !matches!(s.get("op").as_str(), Some("openApproval" | "awaitApproval"))
        })
        .collect();
    let script: Shared = Arc::new(Mutex::new(Script {
        case: case.clone(),
        loaded: input.get("loaded") != &JValue::Bool(false),
        bridge: arr(case.get("bridge")),
        stubs,
        // The engine requests core made before the cut (a Vision role's description) are core's own.
        round: pre_cut_requests(case),
        revoke_fails,
        ..Script::default()
    }));
    *current.lock().unwrap() = Some(Arc::clone(&script));
    let s = Arc::clone(&script);
    let core = listen(move |r| core_service(Arc::clone(&s), r)).await;
    let s = Arc::clone(&script);
    let engine = listen(move |r| engine_service(Arc::clone(&s), r)).await;
    let f = front(core).await;

    // The account as core's preparation leaves it: its directory, its tool policies, the turn's user
    // message last in the transcript (unless the case records a store that does not have it).
    server_projects::store::ensure_user_dir(&f.data, &f.user).unwrap();
    // What core's preparation wrote before the cut (a pinned skill's ledger record), as core leaves it.
    if let JValue::Obj(files) = case.get("handoff").get("files") {
        let dir = server_projects::store::user_dir(&f.data, &f.user).unwrap();
        for (name, content) in files {
            std::fs::write(dir.join(name), js::stringify(content)).unwrap();
        }
    }
    if let JValue::Obj(policy) = input.get("policy") {
        for (tool, mode) in policy {
            let first = match mode {
                JValue::Arr(m) => m[0].as_str().unwrap().to_string(),
                m => m.as_str().unwrap().to_string(),
            };
            set_policy(&f.data, &f.user, tool, &first);
        }
    }
    let body = input.get("body").clone();
    let chat_id = body.get("chatId").as_str().map(str::to_string);
    if let Some(chat) = &chat_id {
        if input.get("appendOutcome").as_str().is_none() {
            let mut history = arr(body.get("history"));
            history.push(js::obj([
                ("role", js::s("user")),
                ("content", body.get("message").clone()),
            ]));
            server_projects::transcript::write(
                &f.data,
                &f.user,
                chat,
                &history,
                &server_projects::transcript::Base::Any,
                None,
            )
            .unwrap();
        }
    }

    let t0 = now_ms();
    let leaving = {
        let mut sc = script.lock().unwrap();
        sc.engine = format!("http://{engine}/v1/chat/completions");
        sc.started = t0;
        sc.user = f.user.clone();
        sc.data = f.data.clone();
        sc.calls.clear();
        Arc::clone(&sc.leaving)
    };
    let (status, sse, decided) = chat(&f, &js::stringify(&body), &leaving, decide).await;
    if status != 200 {
        return Err(format!("status {status}: {sse}"));
    }
    if let Some(decision) = decide {
        let cards = arr(case.get("approvals")).len();
        if decided.len() != cards
            || (decision != "leave" && decided.iter().any(|(_, status)| *status != 200))
        {
            return Err(format!(
                "{decision}: the cards answered {decided:?}, recorded {cards}"
            ));
        }
        // approve_all on a once card stores no grant. (This harness's chat is held by no list, so its holder is
        // absent and no card's approve_all may grant it either, as in Node; the registry's own tests cover a held
        // chat, and chat_native's unit test that `once` reaches the question.)
        if decision == "approve_all" {
            let card = arr(case.get("approvals"))[0].clone();
            let chat = card.get("chatId").as_str().unwrap_or("").to_string();
            let scope = card.get("scope").as_str().unwrap_or("").to_string();
            let context = server_projects::chat_move::grant_context(&f.data, &f.user, &chat, START)
                .map_err(|e| format!("grant context: {e:?}"))?;
            let binding = serde_json::json!({"user": f.user, "canonicalChatId": chat, "scope": scope,
                "context": context.json()});
            let held = matches!(
                f.approvals
                    .check(&f.user, &chat, &scope, Some(&binding), None),
                Ok(noevia_server::approval_registry::GrantUse::Ticket(Some(_)))
            );
            if held {
                return Err(format!(
                    "approve_all on card {}: a grant is held",
                    js::stringify(&card)
                ));
            }
        }
        // An answered card's question is gone: a second answer finds nothing (404).
        for (id, _) in &decided {
            let again = decide_card(
                &f,
                id,
                if decision == "leave" {
                    "approve_all"
                } else {
                    decision
                },
            )
            .await;
            if again != 404 {
                return Err(format!("{id} answered again: {again}"));
            }
        }
    }

    // The turn's end (the lease back, the save) runs as the stream ends, or after the client left;
    // wait for the files to settle, at most 5 s.
    let dir = server_projects::store::user_dir(&f.data, &f.user).unwrap();
    let want_files: BTreeMap<String, String> = match case.get("files") {
        JValue::Obj(files) => files
            .iter()
            .map(|(k, v)| {
                let mut v = v.clone();
                settle(&mut v, "", (0.0, 0.0));
                (k.clone(), js::stringify(&v))
            })
            .collect(),
        _ => BTreeMap::new(),
    };
    let leased = !input.get("leaseRefusal").is_object();
    // The reply a leaving client's turn kept, where Node's store took it: saved at the turn's very
    // end (after the lease went back), so the wait below includes it.
    let want_kept = arr(case.get("stores"))
        .into_iter()
        .find(|s| s.get("op").as_str() == Some("transcripts.appendIfLast"))
        .filter(|_| chat_id.is_some() && input.get("appendOutcome").as_str().is_none())
        .map(|kept| texts(&[kept.get("entry").clone()], "", (0.0, 0.0)));
    let kept_now = |window: (f64, f64)| -> Result<Vec<String>, String> {
        let chat = chat_id.as_deref().unwrap_or_default();
        let now = server_projects::transcript::read(&f.data, &f.user, chat)
            .map_err(|e| format!("the transcript: {e:?}"))?;
        let last = arr(&now.history).last().cloned().unwrap_or(JValue::Null);
        Ok(texts(&[last], &f.user, window))
    };
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    let got_files = loop {
        let window = (t0, now_ms());
        let got = state_files(&dir, &f.user, window);
        let kept = want_kept
            .as_ref()
            .is_none_or(|want| kept_now(window).ok().as_ref() == Some(want));
        let released = !leased || model_lease_released(&script.lock().unwrap());
        if (got == want_files && released && kept) || tokio::time::Instant::now() >= deadline {
            break got;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    };
    let window = (t0, now_ms());
    let sc = script.lock().unwrap();
    if sc.handoff_header.as_deref() != Some(STORE_TOKEN) {
        return Err(format!(
            "the turn's hand-off header was {:?}",
            sc.handoff_header
        ));
    }

    // (a) The client's frames, before and after the cut.
    let want = texts(&arr(case.get("events")), "", (0.0, 0.0));
    let got: Vec<String> = frames(&sse)
        .iter()
        .map(|t| {
            let mut v = wtf16::parse(t).unwrap_or_else(|_| js::s(format!("<unparsed> {t}")));
            settle(&mut v, &f.user, window);
            js::stringify(&v)
        })
        .collect();
    if got != want {
        let i = got
            .iter()
            .zip(&want)
            .position(|(a, b)| a != b)
            .unwrap_or(got.len().min(want.len()));
        return Err(format!(
            "event {i} of {} (recorded {}):\n front: {}\n node:  {}",
            got.len(),
            want.len(),
            got.get(i).map_or("<none>", String::as_str),
            want.get(i).map_or("<none>", String::as_str)
        ));
    }

    // (b) The engine request bodies, each with the turn's lease.
    let recorded: Vec<JValue> = arr(case.get("requests"))
        .into_iter()
        .skip(pre_cut_requests(case))
        .collect();
    if sc.engine_bodies.len() != recorded.len() {
        return Err(format!(
            "{} engine requests, recorded {}",
            sc.engine_bodies.len(),
            recorded.len()
        ));
    }
    for (i, (sent, r)) in sc.engine_bodies.iter().zip(&recorded).enumerate() {
        let mut want = r.get("body").clone();
        if let Some(key) = want.get("tools").get("$tools").as_str() {
            let tools = fixture.get("tools").get(key).clone();
            js::set(&mut want, "tools", tools);
        }
        let want = js::stringify(&want);
        if *sent != want {
            return Err(format!(
                "engine request {i} body:\n front: {sent}\n node:  {want}"
            ));
        }
        if sc.engine_leases[i].as_deref() != Some(LEASE) {
            return Err(format!(
                "engine request {i} lease header: {:?}",
                sc.engine_leases[i]
            ));
        }
    }

    // The fakes' contract checks, the approval cards, the tools run, every recorded answer asked for.
    if !sc.errors.is_empty() {
        return Err(format!("bridge contract: {:?}", sc.errors));
    }
    // The grant revokes Node's turn made, asked of core's revoke bridge (Node's revokeChatGrant).
    let want_revokes: Vec<String> = arr(case.get("stores"))
        .iter()
        .filter(|s| s.get("op").as_str() == Some("revokeChatGrant"))
        .map(|s| s.get("chatId").as_str().unwrap_or("").to_string())
        .collect();
    if sc.revokes != want_revokes {
        return Err(format!(
            "grant revokes: front {:?} node {want_revokes:?}",
            sc.revokes
        ));
    }
    // With that bridge failing, the front's own revoke asks Node to acknowledge it.
    if revoke_fails
        && !want_revokes.is_empty()
        && !sc
            .calls
            .iter()
            .any(|c| c == "POST /internal/approvals/revoke")
    {
        return Err(format!(
            "the front's fallback revoke asked no acknowledgement: {:?}",
            sc.calls
        ));
    }
    let asked = texts(&sc.asked, &f.user, window);
    let want = texts(&arr(case.get("approvals")), "", (0.0, 0.0));
    if decide.is_none() && asked != want {
        return Err(format!(
            "approval requests:\n front: {asked:?}\n node:  {want:?}"
        ));
    }
    let want: Vec<String> = arr(case.get("executions"))
        .iter()
        .map(|e| e.as_str().unwrap().to_string())
        .collect();
    // The client left while a tool ran: core is asked to abort it (its answer is still taken).
    let aborted = sc
        .calls
        .iter()
        .any(|c| c == "POST /internal/chat/tools/abort");
    if aborted != input.get("leaveInTool").as_str().is_some() {
        return Err(format!("tool abort asked: {aborted}"));
    }
    // The recorder logs the runs of the tools it stubs (Drive, the QA documents box); core's own project
    // file readers and the coworker page save run for real there and are seen in the bridge calls instead.
    let ran: Vec<String> = sc
        .executions
        .iter()
        .filter(|n| {
            !matches!(
                n.as_str(),
                "read_project_file" | "project_read_file" | "space_page_save"
            )
        })
        .cloned()
        .collect();
    if ran != want {
        return Err(format!(
            "tools run: front {:?} node {want:?}",
            sc.executions
        ));
    }
    if !sc.bridge.is_empty() || !sc.stubs.is_empty() {
        return Err(format!(
            "never asked for: bridge {:?} stubs {:?}",
            texts(&sc.bridge, "", (0.0, 0.0)),
            texts(&sc.stubs, "", (0.0, 0.0))
        ));
    }
    if leased && !model_lease_released(&sc) {
        return Err(format!("the lease was never given back: {:?}", sc.calls));
    }

    // (c) The account's state files.
    if got_files != want_files {
        return Err(format!(
            "workspace files:\n front: {got_files:?}\n node:  {want_files:?}"
        ));
    }

    if let Some(want) = &want_kept {
        let got = kept_now(window)?;
        if got != *want {
            return Err(format!(
                "the kept reply:\n front: {got:?}\n node:  {want:?}"
            ));
        }
    }
    Ok(())
}
