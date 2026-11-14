//! code-machine.cjs's relay against its own test (core server/code-machine-1853.test.cjs: "the
//! relay: a live task token only, two paths only, the machine's model forced, and nothing after the
//! run", the #1876 bind, close and slot cases) and the rest of the module's answers, read from the
//! Node source: every status, `relay_error` body, refusal text and log event. A fake machine stands in
//! for chat-machines.cjs. Synthetic names only.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use code_engine_relay::bind::{self, Bind};
use code_engine_relay::{
    BodyFail, BodySource, Chunk, Forward, Machines, Options, Ready, Relay, Reply, Request, Target,
    Upstream,
};
use js_json::{wtf16, JValue};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;

const MODEL: &str = "synthetic-coder-1853";

type Sender = mpsc::UnboundedSender<Result<Vec<u8>, ()>>;

fn target() -> Target {
    Target {
        machine_id: "qa-m".into(),
        machine_name: "QA machine".into(),
        base_url: "http://qa-machine.invalid:8080".into(),
        model: MODEL.into(),
    }
}

/// The fake machine's upstream: chunks from a channel; records being dropped (the request aborted).
struct Up {
    status: u16,
    content_type: Option<String>,
    rx: mpsc::UnboundedReceiver<Result<Vec<u8>, ()>>,
    dropped: Arc<AtomicBool>,
}

impl Drop for Up {
    fn drop(&mut self) {
        self.dropped.store(true, Ordering::SeqCst);
    }
}

impl Upstream for Up {
    fn status(&self) -> u16 {
        self.status
    }
    fn content_type(&self) -> Option<String> {
        self.content_type.clone()
    }
    async fn next(&mut self) -> Option<Result<Vec<u8>, ()>> {
        self.rx.recv().await
    }
}

#[derive(Default)]
struct Fake {
    ready: Mutex<Option<Ready>>,
    /// The connection fails (`connectError`).
    refuse_connect: AtomicBool,
    /// The machine never sends its headers.
    never_answer: AtomicBool,
    sent: Mutex<Vec<Forward>>,
    /// Each answer's channel: its first chunk is sent, then the stream stays open until dropped.
    senders: Mutex<Vec<Sender>>,
    dropped: Mutex<Vec<Arc<AtomicBool>>>,
    turns: AtomicUsize,
    released: Arc<AtomicUsize>,
    context: Option<f64>,
}

impl Machines for Fake {
    type Upstream = Up;
    type Error = String;
    async fn ready(&self, _t: &Target) -> Ready {
        self.ready.lock().unwrap().clone().unwrap_or_else(Ready::ok)
    }
    async fn context_tokens(&self, _t: &Target) -> Option<f64> {
        self.context
    }
    fn begin_turn(&self, machine_id: &str) -> Option<Box<dyn FnOnce() + Send>> {
        assert_eq!(machine_id, "qa-m");
        self.turns.fetch_add(1, Ordering::SeqCst);
        let released = Arc::clone(&self.released);
        Some(Box::new(move || {
            released.fetch_add(1, Ordering::SeqCst);
        }))
    }
    async fn fetch(&self, request: Forward) -> Result<Up, String> {
        self.sent.lock().unwrap().push(request);
        if self.refuse_connect.load(Ordering::SeqCst) {
            return Err("ECONNREFUSED".into());
        }
        if self.never_answer.load(Ordering::SeqCst) {
            std::future::pending::<()>().await;
        }
        let (tx, rx) = mpsc::unbounded_channel();
        tx.send(Ok(b"data: {\"choices\":[]}\n\n".to_vec())).unwrap();
        self.senders.lock().unwrap().push(tx);
        let dropped = Arc::new(AtomicBool::new(false));
        self.dropped.lock().unwrap().push(Arc::clone(&dropped));
        Ok(Up {
            status: 200,
            content_type: Some("text/event-stream".into()),
            rx,
            dropped,
        })
    }
    fn connect_error(&self, t: &Target, e: Option<&String>) -> String {
        match e {
            Some(e) => format!("{} could not be reached ({e}).", t.machine_name),
            None => format!("{} did not answer in time.", t.machine_name),
        }
    }
}

struct Rig {
    fake: Arc<Fake>,
    relay: Relay<Fake>,
    where_: Arc<Mutex<Option<Target>>>,
    logs: Arc<Mutex<Vec<JValue>>>,
}

fn rig_with(f: impl FnOnce(&mut Options<Fake>), fake: Fake) -> Rig {
    let fake = Arc::new(fake);
    let where_ = Arc::new(Mutex::new(Some(target())));
    let logs: Arc<Mutex<Vec<JValue>>> = Arc::default();
    let w = Arc::clone(&where_);
    let l = Arc::clone(&logs);
    let mut o = Options::new(
        Arc::clone(&fake),
        Arc::new(move |_: &str| w.lock().unwrap().clone()),
        Some("http://egress:8095/v1".into()),
    );
    o.log = Arc::new(move |e| l.lock().unwrap().push(e));
    f(&mut o);
    Rig {
        fake,
        relay: Relay::new(o),
        where_,
        logs,
    }
}

fn rig() -> Rig {
    rig_with(|_| {}, Fake::default())
}

fn req(method: &str, url: &str, token: Option<&str>, body: &str) -> Request<Vec<u8>> {
    Request {
        method: method.into(),
        url: url.into(),
        authorization: token.map(|t| format!("Bearer {t}")),
        accept: None,
        body: body.as_bytes().to_vec(),
    }
}

/// A Json reply's status and parsed body; anything else panics.
fn json<U>(r: Reply<U>) -> (u16, JValue) {
    match r {
        Reply::Json { status, body } => (status, wtf16::parse(&body).unwrap()),
        Reply::Stream(_) => panic!("a stream, not JSON"),
        Reply::Cut => panic!("cut, not JSON"),
    }
}

fn error(status: u16, message: &str, code: Option<&str>) -> (u16, JValue) {
    let mut e = vec![
        ("message".to_string(), JValue::Str(message.into())),
        ("type".to_string(), JValue::Str("relay_error".into())),
    ];
    if let Some(c) = code {
        e.push(("code".into(), JValue::Str(c.into())));
    }
    (status, JValue::obj([("error", JValue::Obj(e))]))
}

fn events(r: &Rig) -> Vec<String> {
    r.logs
        .lock()
        .unwrap()
        .iter()
        .map(|e| wtf16::stringify(e).unwrap())
        .collect()
}

const CHAT: &str = "/v1/chat/completions";

#[tokio::test]
async fn a_live_task_token_only_two_paths_only_the_model_forced_and_nothing_after_the_run() {
    let r = rig();
    let body = r#"{"model":"x","messages":[]}"#;
    let refused = error(401, "No running Code task holds this token.", None);
    assert_eq!(
        json(r.relay.handle(req("POST", CHAT, None, body)).await),
        refused
    );
    assert_eq!(
        json(
            r.relay
                .handle(req("POST", CHAT, Some("ncr_forged"), body))
                .await
        ),
        refused
    );
    let lease = r.relay.open("relay-1", None, None).await.unwrap();
    assert_eq!(lease.base_url, "http://egress:8095/v1");
    assert_eq!(lease.model, MODEL);
    assert_eq!(r.relay.active(), 1);
    let token = lease.token.clone();

    // GET /v1/models and /models: the one model the task runs on.
    for path in ["/v1/models", "/models", "/v1/models?x=1"] {
        let (status, list) = json(r.relay.handle(req("GET", path, Some(&token), "")).await);
        assert_eq!(status, 200, "{path}");
        assert_eq!(
            wtf16::stringify(&list).unwrap(),
            format!(
                r#"{{"object":"list","data":[{{"id":"{MODEL}","object":"model","owned_by":"noevia"}}]}}"#
            )
        );
    }
    // Two paths only (Node's test: /embeddings, /../props, /completions, /health; and a GET chat).
    for (method, path) in [
        ("POST", "/v1/embeddings"),
        ("POST", "/v1/../props"),
        ("POST", "/v1/completions"),
        ("GET", "/health"),
        ("GET", CHAT),
        ("POST", "/v1/models"),
        ("PUT", "/chat/completions"),
    ] {
        assert_eq!(
            json(r.relay.handle(req(method, path, Some(&token), body)).await),
            error(404, "Only chat completions are relayed.", None),
            "{method} {path}"
        );
    }
    assert!(r.fake.sent.lock().unwrap().is_empty(), "nothing was sent");

    // The chat completion goes to the machine with the machine's model, in the model key's place.
    let mut asked = req(
        "POST",
        "/chat/completions?stream=1",
        Some(&token),
        r#"{"stream":true,"model":"synthetic-elsewhere","messages":[{"role":"user","content":"QA-SYNTH"}]}"#,
    );
    asked.accept = Some("text/event-stream".into());
    let Reply::Stream(mut s) = r.relay.handle(asked).await else {
        panic!("not relayed");
    };
    assert_eq!((s.status(), s.content_type()), (200, "text/event-stream"));
    assert_eq!(
        s.next().await,
        Chunk::Data(b"data: {\"choices\":[]}\n\n".to_vec())
    );
    let sent = r.fake.sent.lock().unwrap()[0].clone();
    assert_eq!(
        sent,
        Forward {
            url: "http://qa-machine.invalid:8080/v1/chat/completions".into(),
            accept: "text/event-stream".into(),
            body: format!(
                r#"{{"stream":true,"model":"{MODEL}","messages":[{{"role":"user","content":"QA-SYNTH"}}]}}"#
            ),
        },
        "the machine's model in place; no token or credentials are part of a forward"
    );
    r.fake.senders.lock().unwrap().clear();
    assert_eq!(s.next().await, Chunk::End, "the machine ended its answer");
    drop(s);
    assert_eq!(r.fake.turns.load(Ordering::SeqCst), 1);
    assert_eq!(
        r.fake.released.load(Ordering::SeqCst),
        1,
        "the turn is given back"
    );

    // No model in the request: it is added at the end. Accept defaults to JSON.
    let Reply::Stream(s) = r
        .relay
        .handle(req(
            "POST",
            CHAT,
            Some(&token),
            r#"{"messages":[],"1":true}"#,
        ))
        .await
    else {
        panic!("not relayed");
    };
    drop(s);
    let sent = r.fake.sent.lock().unwrap()[1].clone();
    assert_eq!(sent.accept, "application/json");
    assert_eq!(
        sent.body,
        format!(r#"{{"1":true,"messages":[],"model":"{MODEL}"}}"#),
        "JS key order: an index key first"
    );

    // After the run the token is dead.
    lease.close();
    lease.close();
    assert_eq!(r.relay.active(), 0);
    assert_eq!(
        json(r.relay.handle(req("POST", CHAT, Some(&token), body)).await),
        refused
    );
    let log = events(&r);
    assert_eq!(
        log,
        [
            r#"{"event":"relay.opened","taskId":"relay-1","machine":"qa-m"}"#,
            r#"{"event":"relay.closed","taskId":"relay-1"}"#,
        ],
        "closed once"
    );
}

#[tokio::test]
async fn a_body_over_the_cap_or_not_a_json_object_is_refused_before_anything_is_sent() {
    let r = rig_with(|o| o.body_cap = 64, Fake::default());
    let lease = r.relay.open("relay-2", None, None).await.unwrap();
    let t = Some(lease.token.as_str());
    assert_eq!(
        json(
            r.relay
                .handle(req("POST", CHAT, t, &"x".repeat(1000)))
                .await
        ),
        error(413, "The request is too large.", None)
    );
    for text in ["", "{", "x", "{\"a\":1,}"] {
        assert_eq!(
            json(r.relay.handle(req("POST", CHAT, t, text)).await),
            error(400, "The request is not JSON.", None),
            "{text:?}"
        );
    }
    for text in ["[]", "null", "7", "\"s\"", "true"] {
        assert_eq!(
            json(r.relay.handle(req("POST", CHAT, t, text)).await),
            error(400, "The request is not a JSON object.", None),
            "{text:?}"
        );
    }
    /// A body whose connection fails while it is read.
    struct Broken;
    impl BodySource for Broken {
        async fn read(self, _cap: usize) -> Result<Vec<u8>, BodyFail> {
            Err(BodyFail::Failed)
        }
    }
    let broken = Request {
        method: "POST".into(),
        url: CHAT.into(),
        authorization: Some(format!("bearer  {}", lease.token)),
        accept: None,
        body: Broken,
    };
    assert_eq!(
        json(r.relay.handle(broken).await),
        error(400, "The request is not JSON.", None)
    );
    assert!(r.fake.sent.lock().unwrap().is_empty());
    assert_eq!(r.fake.turns.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn open_refuses_with_node_s_codes_and_words() {
    let r = rig_with(
        |_| {},
        Fake {
            context: Some(32768.0),
            ..Fake::default()
        },
    );
    let refused = |e: code_engine_relay::Refusal| (e.code, e.message);
    // The task names another model.
    assert_eq!(
        refused(r.relay.open("t", Some("synthetic-other"), None).await.unwrap_err()),
        (
            "model_mismatch".into(),
            format!("This task names synthetic-other, but the Code role runs {MODEL} on QA machine. One task runs on one model.")
        )
    );
    // `model && …`: an empty name is no name; the same model is fine.
    for model in [Some(""), Some(MODEL)] {
        let ok = r.relay.open("t", model, None).await.unwrap();
        assert_eq!(ok.context_tokens, Some(32768.0));
    }
    // Pinned elsewhere (or to this machine): the role moved.
    let moved = "The Code role moved to another machine or model during this task, and one task runs on one model. Start the task again.";
    for expect in [Some("qa-m|http://elsewhere.invalid|x"), None] {
        assert_eq!(
            refused(r.relay.open("t", None, Some(expect)).await.unwrap_err()),
            ("model_mismatch".into(), moved.into()),
            "{expect:?}"
        );
    }
    let key = code_engine_relay::target_key(Some(&target())).unwrap();
    assert!(r.relay.open("t", None, Some(Some(&key))).await.is_ok());
    // The machine is not ready: its code and text (no code: machineUnreachable).
    for (code, want) in [
        (Some("model_mismatch"), "model_mismatch"),
        (None, "machineUnreachable"),
        (Some(""), "machineUnreachable"),
    ] {
        *r.fake.ready.lock().unwrap() = Some(Ready {
            ok: false,
            code: code.map(str::to_string),
            error: Some("QA machine serves another model.".into()),
        });
        assert_eq!(
            refused(r.relay.open("t", None, None).await.unwrap_err()),
            (want.into(), "QA machine serves another model.".into())
        );
    }
    // No error text: `Error(undefined).message` is "".
    *r.fake.ready.lock().unwrap() = Some(Ready {
        ok: false,
        code: None,
        error: None,
    });
    assert_eq!(
        refused(r.relay.open("t", None, None).await.unwrap_err()),
        ("machineUnreachable".into(), String::new())
    );
    *r.fake.ready.lock().unwrap() = None;
    // No relay (unset, or a listener that could not bind).
    r.relay.disable();
    assert_eq!(r.relay.url(), None);
    assert_eq!(
        refused(r.relay.open("t", None, None).await.unwrap_err()),
        ("machineRelayOff".into(), "The Code role runs on QA machine, but the sandbox has no route to it: web needs CODE_ENGINE_RELAY_PORT (see the code-sandbox override). The task was not started.".into())
    );
    // The role moved back here while the task was starting.
    *r.where_.lock().unwrap() = None;
    assert_eq!(
        refused(r.relay.open("t", None, None).await.unwrap_err()),
        ("model_mismatch".into(), "The Code role moved back to this machine while the task was starting, and one task runs on one model. Start the task again.".into())
    );
    // Every lease opened above was dropped: each closed itself.
    assert_eq!(r.relay.active(), 0);
}

#[tokio::test]
async fn every_request_rechecks_the_role_and_the_machine() {
    let r = rig();
    let lease = r.relay.open("recheck-1", None, None).await.unwrap();
    let t = Some(lease.token.as_str());
    let body = r#"{"messages":[]}"#;
    // The machine stopped being ready: 502 with its code.
    *r.fake.ready.lock().unwrap() = Some(Ready {
        ok: false,
        code: Some("machineUnreachable".into()),
        error: Some("QA machine is not answering.".into()),
    });
    assert_eq!(
        json(r.relay.handle(req("POST", CHAT, t, body)).await),
        error(
            502,
            "QA machine is not answering.",
            Some("machineUnreachable")
        )
    );
    *r.fake.ready.lock().unwrap() = Some(Ready {
        ok: false,
        code: None,
        error: Some("QA machine is not answering.".into()),
    });
    assert_eq!(
        json(r.relay.handle(req("POST", CHAT, t, body)).await),
        error(502, "QA machine is not answering.", None)
    );
    // An answer without an error text: Node's JSON.stringify leaves `message` out (noevia#2676).
    *r.fake.ready.lock().unwrap() = Some(Ready {
        ok: false,
        code: Some("machineWrongModel".into()),
        error: None,
    });
    match r.relay.handle(req("POST", CHAT, t, body)).await {
        Reply::Json { status, body } => {
            assert_eq!(status, 502);
            assert_eq!(
                body,
                r#"{"error":{"type":"relay_error","code":"machineWrongModel"}}"#
            );
        }
        other => panic!("not JSON: {other:?}"),
    }
    *r.fake.ready.lock().unwrap() = None;
    // The connection fails: the #1840 text.
    r.fake.refuse_connect.store(true, Ordering::SeqCst);
    assert_eq!(
        json(r.relay.handle(req("POST", CHAT, t, body)).await),
        error(
            502,
            "QA machine could not be reached (ECONNREFUSED).",
            Some("machineUnreachable")
        )
    );
    r.fake.refuse_connect.store(false, Ordering::SeqCst);
    // The role moved to another model: 409, and nothing sent.
    let sent = r.fake.sent.lock().unwrap().len();
    *r.where_.lock().unwrap() = Some(Target {
        model: "synthetic-other".into(),
        ..target()
    });
    let moved = error(
        409,
        "The Code role moved to another machine or model during this task; one task runs on one model.",
        Some("model_mismatch"),
    );
    assert_eq!(
        json(r.relay.handle(req("POST", CHAT, t, body)).await),
        moved
    );
    // Moved back here: the same.
    *r.where_.lock().unwrap() = None;
    assert_eq!(
        json(r.relay.handle(req("POST", CHAT, t, body)).await),
        moved
    );
    assert_eq!(r.fake.sent.lock().unwrap().len(), sent);
    // A turn is counted only once the machine is ready, and every one was given back.
    assert_eq!(r.fake.turns.load(Ordering::SeqCst), 1);
    assert_eq!(r.fake.released.load(Ordering::SeqCst), 1);
    assert_eq!(
        events(&r),
        [
            r#"{"event":"relay.opened","taskId":"recheck-1","machine":"qa-m"}"#,
            r#"{"event":"relay.refused","taskId":"recheck-1","code":"machineUnreachable"}"#,
            r#"{"event":"relay.refused","taskId":"recheck-1"}"#,
            r#"{"event":"relay.refused","taskId":"recheck-1","code":"machineWrongModel"}"#,
            r#"{"event":"relay.unreachable","taskId":"recheck-1"}"#,
            r#"{"event":"relay.refused","taskId":"recheck-1","code":"model_mismatch"}"#,
            r#"{"event":"relay.refused","taskId":"recheck-1","code":"model_mismatch"}"#,
        ]
    );
}

#[tokio::test]
async fn close_cuts_the_lease_s_requests_in_flight() {
    // #1876 "close() aborts the lease's in-flight upstream requests".
    let r = rig();
    let lease = r.relay.open("abort-1", None, None).await.unwrap();
    let Reply::Stream(mut s) = r
        .relay
        .handle(req("POST", CHAT, Some(&lease.token), r#"{"stream":true}"#))
        .await
    else {
        panic!("not relayed");
    };
    assert!(matches!(s.next().await, Chunk::Data(_)));
    let dropped = Arc::clone(&r.fake.dropped.lock().unwrap()[0]);
    assert!(!dropped.load(Ordering::SeqCst));
    lease.close();
    let cut = tokio::time::timeout(Duration::from_secs(1), s.next())
        .await
        .expect("not cut within a second");
    assert_eq!(cut, Chunk::Cut);
    assert!(
        dropped.load(Ordering::SeqCst),
        "the machine's request was aborted"
    );
    assert_eq!(s.next().await, Chunk::End);

    // A request still waiting for the machine is cut too, without an answer.
    let r = rig();
    r.fake.never_answer.store(true, Ordering::SeqCst);
    let lease = Arc::new(r.relay.open("abort-2", None, None).await.unwrap());
    let closer = Arc::clone(&lease);
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(50)).await;
        closer.close();
    });
    let answer = tokio::time::timeout(
        Duration::from_secs(2),
        r.relay
            .handle(req("POST", CHAT, Some(&lease.token), r#"{"stream":true}"#)),
    )
    .await
    .expect("not cut");
    assert!(matches!(answer, Reply::Cut), "{answer:?}");
    assert_eq!(
        r.fake.released.load(Ordering::SeqCst),
        1,
        "its turn given back"
    );
}

#[tokio::test]
async fn one_task_holds_at_most_its_slots_and_a_stream_dropped_gives_its_slot_back() {
    let r = rig_with(|o| o.max_concurrent = 2, Fake::default());
    let lease = r.relay.open("slots-1", None, None).await.unwrap();
    let other = r.relay.open("slots-2", None, None).await.unwrap();
    let t = Some(lease.token.as_str());
    let body = r#"{"stream":true}"#;
    let mut held = Vec::new();
    for _ in 0..2 {
        match r.relay.handle(req("POST", CHAT, t, body)).await {
            Reply::Stream(s) => held.push(s),
            other => panic!("{other:?}"),
        }
    }
    let busy = error(429, "Too many model requests from this task at once.", None);
    assert_eq!(json(r.relay.handle(req("POST", CHAT, t, body)).await), busy);
    // The models list needs no slot; another task has its own.
    assert_eq!(
        json(r.relay.handle(req("GET", "/v1/models", t, "")).await).0,
        200
    );
    assert!(matches!(
        r.relay
            .handle(req("POST", CHAT, Some(&other.token), body))
            .await,
        Reply::Stream(_)
    ));
    // Refusals after the slot was taken give it back too.
    held.pop();
    assert_eq!(
        json(r.relay.handle(req("POST", CHAT, t, "[]")).await).0,
        400
    );
    assert!(matches!(
        r.relay.handle(req("POST", CHAT, t, body)).await,
        Reply::Stream(_)
    ));
    // Token-less requests take no slot at all (#1876 flood).
    for _ in 0..5 {
        assert_eq!(
            json(r.relay.handle(req("POST", CHAT, None, body)).await).0,
            401
        );
    }
}

#[tokio::test]
async fn a_request_past_its_time_ends_its_answer_or_never_reaches_the_machine() {
    // Node's AbortSignal.timeout: before the machine's headers the #1840 text (502); after them
    // the answer ends.
    let r = rig_with(
        |o| o.request_timeout = Duration::from_millis(100),
        Fake::default(),
    );
    let lease = r.relay.open("late-1", None, None).await.unwrap();
    let t = Some(lease.token.as_str());
    let Reply::Stream(mut s) = r.relay.handle(req("POST", CHAT, t, "{}")).await else {
        panic!("not relayed");
    };
    assert!(matches!(s.next().await, Chunk::Data(_)));
    let ended = tokio::time::timeout(Duration::from_secs(2), s.next())
        .await
        .unwrap();
    assert_eq!(ended, Chunk::End);
    r.fake.never_answer.store(true, Ordering::SeqCst);
    assert_eq!(
        json(r.relay.handle(req("POST", CHAT, t, "{}")).await),
        error(
            502,
            "QA machine did not answer in time.",
            Some("machineUnreachable")
        )
    );
}

// ── bind.rs: startEngineRelayFromEnv and bindProblem ─────────────────────────────────────────

fn env(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
    let pairs: Vec<(String, String)> = pairs
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    move |k| pairs.iter().find(|(x, _)| x == k).map(|(_, v)| v.clone())
}

#[test]
fn unset_is_no_relay_set_is_the_code_network_name() {
    let off = bind::from_env(env(&[])).unwrap();
    assert_eq!((off.port, off.url), (None, None));
    assert_eq!(off.bind, Bind::Lookup("egress".into()));
    // A bad host is a start error only with a port.
    assert_eq!(
        bind::from_env(env(&[
            ("CODE_ENGINE_RELAY_PORT", "8095"),
            ("CODE_EGRESS_HOST", "bad host")
        ])),
        Err("CODE_ENGINE_RELAY_HOST should be a host name, not \"bad host\"".into())
    );
    assert!(bind::from_env(env(&[("CODE_EGRESS_HOST", "bad host")])).is_ok());
    let on = bind::from_env(env(&[
        ("CODE_ENGINE_RELAY_PORT", "18953"),
        ("CODE_EGRESS_HOST", "egress"),
    ]))
    .unwrap();
    assert_eq!(on.url.as_deref(), Some("http://egress:18953/v1"));
    // `Number(port)`, an integer in 1..=65535.
    for (port, want) in [
        ("0", None),
        ("", None),
        ("-1", None),
        ("65536", None),
        ("8095.5", None),
        ("abc", None),
        ("inf", None),
        ("Infinity", None),
        (" 8095 ", Some(8095)),
        ("0x1F", Some(31)),
        ("1e3", Some(1000)),
        ("8095.0", Some(8095)),
        ("+80", Some(80)),
        ("65535", Some(65535)),
    ] {
        let r = bind::from_env(env(&[("CODE_ENGINE_RELAY_PORT", port)])).unwrap();
        assert_eq!(r.port, want, "{port:?}");
    }
    // CODE_ENGINE_RELAY_HOST, else CODE_EGRESS_HOST (an empty one falls through), trimmed.
    let host = |pairs: &[(&str, &str)]| bind::from_env(env(pairs)).unwrap().host;
    assert_eq!(
        host(&[
            ("CODE_ENGINE_RELAY_HOST", " relay.qa "),
            ("CODE_EGRESS_HOST", "egress-qa")
        ]),
        "relay.qa"
    );
    assert_eq!(
        host(&[
            ("CODE_ENGINE_RELAY_HOST", ""),
            ("CODE_EGRESS_HOST", "egress-qa")
        ]),
        "egress-qa"
    );
    // CODE_ENGINE_RELAY_BIND set (even empty) is explicit, trimmed.
    assert_eq!(
        bind::from_env(env(&[("CODE_ENGINE_RELAY_BIND", " 127.0.0.1 ")]))
            .unwrap()
            .bind,
        Bind::Explicit("127.0.0.1".into())
    );
    assert_eq!(
        bind::from_env(env(&[("CODE_ENGINE_RELAY_BIND", "")]))
            .unwrap()
            .bind,
        Bind::Explicit(String::new())
    );
}

#[test]
fn a_name_that_does_not_resolve_binds_loopback() {
    let (a, log) = Bind::Lookup("egress".into()).address(|h| {
        assert_eq!(h, "egress");
        Ok("10.9.9.9".into())
    });
    assert_eq!((a.as_str(), log), ("10.9.9.9", None));
    let (a, log) = Bind::Lookup("egress".into()).address(|_| Err("ENOTFOUND egress".into()));
    assert_eq!(a, "127.0.0.1");
    assert_eq!(
        wtf16::stringify(&log.unwrap()).unwrap(),
        r#"{"event":"relay.bind_fallback_loopback","host":"egress","reason":"ENOTFOUND egress"}"#
    );
    let (a, _) = Bind::Explicit("10.1.1.1".into()).address(|_| panic!("no lookup"));
    assert_eq!(a, "10.1.1.1");
}

#[test]
fn the_bind_address_must_be_one_of_this_machine_s_own() {
    // #1876 "CODE_ENGINE_RELAY_BIND is validated".
    let own: Vec<String> = ["127.0.0.1", "::1", "10.9.9.9", "fe80::1%eth0", "FD00::A"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    for (bad, why) in [
        ("0.0.0.0", "would listen on every interface"),
        ("::", "would listen on every interface"),
        ("0:0:0:0:0:0:0:0", "would listen on every interface"),
        ("::0", "would listen on every interface"),
        ("", "is empty"),
        ("  ", "is empty"),
        ("egress", "is not an IP address"),
        ("not-an-ip", "is not an IP address"),
        ("010.9.9.9", "is not an IP address"),
        ("10.9.9", "is not an IP address"),
        ("fe80::1%", "is not an IP address"),
        (
            "203.0.113.9",
            "is not one of this machine's interface addresses",
        ),
        (
            "10.9.9.8",
            "is not one of this machine's interface addresses",
        ),
    ] {
        assert_eq!(bind::bind_problem(bad, &own), Some(why), "{bad:?}");
    }
    for good in ["127.0.0.1", " 10.9.9.9 ", "::1", "fe80::1%eth1", "fd00::a"] {
        assert_eq!(bind::bind_problem(good, &own), None, "{good:?}");
    }
    assert_eq!(
        bind::bind_problem("127.0.0.1", &[]),
        Some("is not one of this machine's interface addresses"),
        "no interfaces read: nothing is ours"
    );
    assert_eq!(
        wtf16::stringify(&bind::rejected(
            18961,
            "0.0.0.0",
            "would listen on every interface"
        ))
        .unwrap(),
        r#"{"event":"relay.bind_rejected","port":18961,"bind":"0.0.0.0","reason":"would listen on every interface"}"#
    );
    assert_eq!(bind::SERVER_LIMITS.max_connections, 32);
    assert_eq!(bind::SERVER_LIMITS.headers_timeout, Duration::from_secs(10));
}

// ── listener.rs: the server and its #1876 limits ─────────────────────────────────────────────

async fn listening(
    r: Rig,
    limits: bind::Limits,
) -> (std::net::SocketAddr, Arc<Relay<Fake>>, Arc<Fake>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let relay = Arc::new(r.relay);
    tokio::spawn(code_engine_relay::listener::serve(
        listener,
        Arc::clone(&relay),
        limits,
        std::future::pending(),
    ));
    (addr, relay, r.fake)
}

/// One request, read until the server closes (or `within`): the raw answer.
async fn raw(addr: std::net::SocketAddr, text: &str, within: Duration) -> String {
    let mut s = TcpStream::connect(addr).await.unwrap();
    s.write_all(text.as_bytes()).await.unwrap();
    let mut out = Vec::new();
    let _ = tokio::time::timeout(within, s.read_to_end(&mut out)).await;
    String::from_utf8_lossy(&out).to_string()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_listener_answers_with_no_store_and_relays_the_stream() {
    let (addr, relay, fake) = listening(rig(), bind::SERVER_LIMITS).await;
    let refused = raw(
        addr,
        "POST /v1/chat/completions HTTP/1.1\r\nHost: x\r\nConnection: close\r\nContent-Length: 2\r\n\r\n{}",
        Duration::from_secs(5),
    )
    .await
    .to_ascii_lowercase();
    assert!(refused.starts_with("http/1.1 401"), "{refused}");
    assert!(refused.contains("cache-control: no-store"), "{refused}");
    assert!(
        refused.contains("content-type: application/json"),
        "{refused}"
    );
    assert!(
        refused.ends_with(
            r#"{"error":{"message":"no running code task holds this token.","type":"relay_error"}}"#
        ),
        "{refused}"
    );

    let lease = relay.open("listen-1", None, None).await.unwrap();
    let body = r#"{"model":"x","stream":true}"#;
    let mut s = TcpStream::connect(addr).await.unwrap();
    s.write_all(
        format!(
            "POST /v1/chat/completions HTTP/1.1\r\nHost: x\r\nAuthorization: Bearer {}\r\nAccept: text/event-stream\r\nContent-Length: {}\r\n\r\n{body}",
            lease.token,
            body.len()
        )
        .as_bytes(),
    )
    .await
    .unwrap();
    let mut got = Vec::new();
    let mut buf = [0u8; 4096];
    while !String::from_utf8_lossy(&got).contains("data: ") {
        let n = tokio::time::timeout(Duration::from_secs(5), s.read(&mut buf))
            .await
            .unwrap()
            .unwrap();
        assert!(n > 0, "closed early: {}", String::from_utf8_lossy(&got));
        got.extend_from_slice(&buf[..n]);
    }
    let head = String::from_utf8_lossy(&got).to_ascii_lowercase();
    assert!(head.starts_with("http/1.1 200"), "{head}");
    assert!(head.contains("content-type: text/event-stream"), "{head}");
    assert!(head.contains("cache-control: no-store"), "{head}");
    assert_eq!(
        fake.sent.lock().unwrap()[0].body,
        format!(r#"{{"model":"{MODEL}","stream":true}}"#)
    );
    // The lease closes mid-answer: the connection is cut, not ended.
    lease.close();
    let mut rest = Vec::new();
    let end = tokio::time::timeout(Duration::from_secs(2), s.read_to_end(&mut rest)).await;
    assert!(end.is_ok(), "the connection was not cut");
    assert!(
        !String::from_utf8_lossy(&rest).contains("0\r\n\r\n"),
        "a cut answer has no final chunk: {}",
        String::from_utf8_lossy(&rest)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_slow_head_is_dropped_and_connections_are_capped() {
    let limits = bind::Limits {
        headers_timeout: Duration::from_millis(300),
        request_timeout: Duration::from_millis(500),
        max_connections: 2,
        ..bind::SERVER_LIMITS
    };
    let (addr, relay, _fake) = listening(rig(), limits).await;
    // A head that never ends is dropped after the head timeout.
    let mut slow = TcpStream::connect(addr).await.unwrap();
    slow.write_all(b"POST /v1/chat/completions HTTP/1.1\r\nHost: x\r\n")
        .await
        .unwrap();
    let mut out = Vec::new();
    let closed = tokio::time::timeout(Duration::from_secs(3), slow.read_to_end(&mut out)).await;
    assert!(closed.is_ok(), "slowloris kept");

    // A body that never completes, with a live token: dropped after the request timeout, no answer.
    let lease = relay.open("slow-body", None, None).await.unwrap();
    let mut stalled = TcpStream::connect(addr).await.unwrap();
    stalled
        .write_all(
            format!(
                "POST /v1/chat/completions HTTP/1.1\r\nHost: x\r\nAuthorization: Bearer {}\r\nContent-Length: 100\r\n\r\n{{",
                lease.token
            )
            .as_bytes(),
        )
        .await
        .unwrap();
    let mut out = Vec::new();
    let closed = tokio::time::timeout(Duration::from_secs(3), stalled.read_to_end(&mut out)).await;
    assert!(closed.is_ok(), "a stalled body kept its connection");
    assert!(out.is_empty(), "{}", String::from_utf8_lossy(&out));

    // Two connections open: a third is closed at once.
    let _a = TcpStream::connect(addr).await.unwrap();
    let _b = TcpStream::connect(addr).await.unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;
    let over = raw(
        addr,
        "GET /v1/models HTTP/1.1\r\nHost: x\r\n\r\n",
        Duration::from_secs(2),
    )
    .await;
    assert_eq!(over, "", "the connection over the limit got an answer");
}

/// noevia#2720 item 1: the target is read for the lease's own account. Two accounts' tasks at once
/// never see each other's target: A's lease keeps relaying while B's roles put Code elsewhere, and
/// B's lease is refused on B's view only.
#[tokio::test]
async fn each_lease_is_rechecked_for_its_own_account() {
    let fake = Arc::new(Fake::default());
    let other = Target {
        machine_id: "qa-m2".into(),
        machine_name: "QA machine 2".into(),
        base_url: "http://qa-machine-2.invalid:8080".into(),
        model: MODEL.into(),
    };
    let views: Arc<Mutex<HashMap<String, Target>>> = Arc::default();
    views.lock().unwrap().insert("qa-a".into(), target());
    views.lock().unwrap().insert("qa-b".into(), target());
    let v = Arc::clone(&views);
    let o = Options::new(
        Arc::clone(&fake),
        Arc::new(move |owner: &str| v.lock().unwrap().get(owner).cloned()),
        Some("http://egress:8095/v1".into()),
    );
    let relay = Relay::new(o);
    let a = relay.open_for("qa-a", "own-a", None, None).await.unwrap();
    let b = relay.open_for("qa-b", "own-b", None, None).await.unwrap();
    // B's roles move Code to another machine; A's do not.
    views.lock().unwrap().insert("qa-b".into(), other);
    let body = r#"{"messages":[]}"#;
    let moved = error(
        409,
        "The Code role moved to another machine or model during this task; one task runs on one model.",
        Some("model_mismatch"),
    );
    assert_eq!(
        json(relay.handle(req("POST", CHAT, Some(&b.token), body)).await),
        moved
    );
    // A's request is not refused by B's view (it reaches the machine: the fake's fetch answers).
    let r = relay.handle(req("POST", CHAT, Some(&a.token), body)).await;
    assert!(
        matches!(&r, Reply::Stream(_)),
        "A's lease was re-checked against B's roles: {r:?}"
    );
    drop(r);
    // An account the target function does not know has no target: refused at open.
    assert!(relay.open_for("qa-c", "own-c", None, None).await.is_err());
    // The plain `open` is the empty owner.
    assert!(relay.open("own-d", None, None).await.is_err());
    a.close();
    b.close();
}

/// noevia#2720 item 2: the target function may block (a settings row, a roles file) without
/// stalling the listener: it runs on the blocking pool, so a current-thread runtime still makes
/// progress on other work while it waits.
#[tokio::test(flavor = "current_thread")]
async fn the_target_function_runs_off_the_async_tasks() {
    let fake = Arc::new(Fake::default());
    let seen: Arc<Mutex<Option<std::thread::ThreadId>>> = Arc::default();
    let s = Arc::clone(&seen);
    let o = Options::new(
        Arc::clone(&fake),
        Arc::new(move |_: &str| {
            *s.lock().unwrap() = Some(std::thread::current().id());
            Some(target())
        }),
        Some("http://egress:8095/v1".into()),
    );
    let relay = Relay::new(o);
    // The proof is the thread: the target function ran on the blocking pool, not on this
    // current-thread runtime's only worker (noevia#2733 item 4: no sleep, no tick count).
    let lease = relay.open_for("qa-a", "blocking-1", None, None).await.unwrap();
    let ran_on = *seen.lock().unwrap();
    assert!(ran_on.is_some(), "the target function was not called");
    assert_ne!(ran_on, Some(std::thread::current().id()));
    lease.close();
}
