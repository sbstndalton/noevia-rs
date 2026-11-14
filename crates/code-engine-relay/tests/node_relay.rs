//! The relay against core's own recording (council p11, p9-1 CONTINUE 2; noevia#2676/#2720 item
//! 4): tests/fixtures/relay-node.v1.json is written by tools/record-relay.cjs, which drives core's
//! real `createEngineRelay` (code-machine.cjs) over node:http through a table of scenarios: every
//! `open` answer or refusal, every request's status, content type and body, what reached the
//! machine (URL, `Accept`, the body with the model forced), the log events, the machine turns and
//! the `ready` calls. This test replays the same steps through [`Relay`] with a fake machine that
//! answers exactly as the recorder's scripted one and compares each outcome.
//!
//! Without the fixture (not recorded yet: the recorder needs node and core, the N5 builder or CI)
//! the replay says so and passes, unless `CODE_RELAY_ORACLE_REQUIRED=1`, which CI sets once the
//! fixture is committed. Synthetic names only.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use code_engine_relay::{
    target_key, Chunk, Forward, Machines, Options, Ready, Relay, Relayed, Reply, Request, Target,
    Upstream,
};
use js_json::{wtf16, JValue};
use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::mpsc;

fn fixture() -> Option<JValue> {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/relay-node.v1.json");
    match std::fs::read_to_string(&path) {
        Ok(text) => Some(wtf16::parse(&text).expect("the fixture is JSON")),
        Err(_) => {
            assert!(
                std::env::var("CODE_RELAY_ORACLE_REQUIRED").as_deref() != Ok("1"),
                "CODE_RELAY_ORACLE_REQUIRED=1 but {} is missing: record it with tools/record-relay.cjs",
                path.display()
            );
            eprintln!(
                "node_relay: {} not recorded yet (tools/record-relay.cjs); nothing replayed",
                path.display()
            );
            None
        }
    }
}

fn s(v: &JValue) -> String {
    v.as_str().unwrap_or_default().to_string()
}

fn target_of(v: &JValue) -> Option<Target> {
    if v.is_nullish() {
        return None;
    }
    Some(Target {
        machine_id: s(v.get("machine").get("id")),
        machine_name: s(v.get("machine").get("name")),
        base_url: s(v.get("baseUrl")),
        model: s(v.get("model")),
    })
}

fn obj(pairs: Vec<(&str, JValue)>) -> JValue {
    JValue::obj(pairs)
}

fn num(n: usize) -> JValue {
    JValue::Num(n as f64)
}

fn text(v: &str) -> JValue {
    JValue::Str(v.to_string())
}

/// JSON equality as `JSON.stringify` output compares: object keys in any order, an `undefined`
/// key the same as a missing one.
fn same(a: &JValue, b: &JValue) -> bool {
    match (a, b) {
        (JValue::Obj(x), JValue::Obj(y)) => {
            let defined = |o: &Vec<(String, JValue)>| {
                o.iter()
                    .filter(|(_, v)| !matches!(v, JValue::Undefined))
                    .count()
            };
            defined(x) == defined(y)
                && x.iter()
                    .filter(|(_, v)| !matches!(v, JValue::Undefined))
                    .all(|(k, v)| same(v, b.get(k)))
        }
        (JValue::Arr(x), JValue::Arr(y)) => {
            x.len() == y.len() && x.iter().zip(y).all(|(p, q)| same(p, q))
        }
        _ => a == b,
    }
}

fn show(v: &JValue) -> String {
    wtf16::stringify(v).unwrap_or_default()
}

// ---- the fake machine: the recorder's scriptedMachines ----------------------------------------

#[derive(Clone)]
struct Script {
    status: u16,
    content_type: Option<String>,
    chunks: Vec<String>,
    hold: bool,
    fail: Option<Option<String>>,
    hang: bool,
}

fn script_of(v: &JValue) -> Script {
    let fail = match v.get("fail") {
        JValue::Undefined | JValue::Null => None,
        f => Some(f.get("code").as_str().map(str::to_string)),
    };
    Script {
        status: v.get("status").as_num().map_or(200, |n| n as u16),
        content_type: v.get("contentType").as_str().map(str::to_string),
        chunks: match v.get("chunks") {
            JValue::Arr(c) => c.iter().map(s).collect(),
            _ => Vec::new(),
        },
        hold: v.get("hold").as_bool().unwrap_or(false),
        fail,
        hang: v.get("hang").as_bool().unwrap_or(false),
    }
}

struct Up {
    status: u16,
    content_type: Option<String>,
    chunks: VecDeque<Vec<u8>>,
    /// A held answer stays open after its chunks until the scenario's release drops the sender.
    held: Option<mpsc::UnboundedReceiver<()>>,
}

impl Upstream for Up {
    fn status(&self) -> u16 {
        self.status
    }
    fn content_type(&self) -> Option<String> {
        self.content_type.clone()
    }
    async fn next(&mut self) -> Option<Result<Vec<u8>, ()>> {
        if let Some(c) = self.chunks.pop_front() {
            return Some(Ok(c));
        }
        if let Some(rx) = self.held.as_mut() {
            let _ = rx.recv().await;
        }
        None
    }
}

/// What a forwarded request failed with: the error's `code`, if it had one.
struct Fail(Option<String>);

struct World {
    sent: Vec<Forward>,
    turns: Vec<String>,
    ready_calls: Vec<String>,
    hold_next: usize,
    held: Vec<mpsc::UnboundedSender<()>>,
}

struct Fake {
    readies: Vec<Ready>,
    ready_at: Mutex<usize>,
    context: JValue,
    script: Script,
    world: Arc<Mutex<World>>,
}

fn ready_of(v: &JValue) -> Ready {
    Ready {
        ok: v.get("ok").as_bool().unwrap_or(false),
        code: v.get("code").as_str().map(str::to_string),
        error: v.get("error").as_str().map(str::to_string),
    }
}

impl Machines for Fake {
    type Upstream = Up;
    type Error = Fail;

    async fn ready(&self, target: &Target) -> Ready {
        self.world
            .lock()
            .unwrap()
            .ready_calls
            .push(target.machine_id.clone());
        let mut at = self.ready_at.lock().unwrap();
        let r = self.readies[(*at).min(self.readies.len() - 1)].clone();
        *at += 1;
        r
    }

    async fn context_tokens(&self, _target: &Target) -> Option<f64> {
        // 'throw' and null are both "no context" (Node: `catch { contextTokens = null }`).
        self.context.as_num()
    }

    fn begin_turn(&self, machine_id: &str) -> Option<Box<dyn FnOnce() + Send>> {
        self.world
            .lock()
            .unwrap()
            .turns
            .push(format!("begin:{machine_id}"));
        let world = Arc::clone(&self.world);
        let id = machine_id.to_string();
        Some(Box::new(move || {
            world.lock().unwrap().turns.push(format!("end:{id}"));
        }))
    }

    async fn fetch(&self, request: Forward) -> Result<Up, Fail> {
        let held = {
            let mut w = self.world.lock().unwrap();
            w.sent.push(request);
            if let Some(code) = &self.script.fail {
                return Err(Fail(code.clone()));
            }
            if self.script.hold && w.hold_next > 0 && !self.script.hang {
                w.hold_next -= 1;
                let (tx, rx) = mpsc::unbounded_channel();
                w.held.push(tx);
                Some(rx)
            } else {
                None
            }
        };
        if self.script.hang {
            std::future::pending::<()>().await;
        }
        Ok(Up {
            status: self.script.status,
            content_type: self.script.content_type.clone(),
            chunks: if self.script.status == 204 {
                VecDeque::new()
            } else {
                self.script
                    .chunks
                    .iter()
                    .map(|c| c.as_bytes().to_vec())
                    .collect()
            },
            held,
        })
    }

    fn connect_error(&self, target: &Target, error: Option<&Fail>) -> String {
        // The recorder's `connectText`: the relay passes no error when the request's time ran out
        // (Node: a TimeoutError).
        match error {
            None => format!("QA connect: timeout on {}", target.machine_id),
            Some(Fail(code)) => format!(
                "QA connect: {} on {}",
                code.as_deref().unwrap_or("error"),
                target.machine_id
            ),
        }
    }
}

// ---- the replay ---------------------------------------------------------------------------------

/// A whole answer as the sandbox would read it: `{ status, contentType, cacheControl, body }`, or
/// `{ cut: true }`.
async fn answer(reply: Reply<Up>) -> JValue {
    match reply {
        Reply::Json { status, body } => obj(vec![
            ("status", JValue::Num(f64::from(status))),
            ("contentType", text("application/json")),
            ("cacheControl", text("no-store")),
            ("body", JValue::Str(body)),
        ]),
        Reply::Stream(r) => drain(r).await,
        Reply::Cut => obj(vec![("cut", JValue::Bool(true))]),
    }
}

async fn drain(mut r: Relayed<Up>) -> JValue {
    let status = r.status();
    let content_type = r.content_type().to_string();
    let mut body = Vec::new();
    loop {
        match tokio::time::timeout(Duration::from_secs(5), r.next())
            .await
            .expect("the relayed answer ends within 5 s")
        {
            Chunk::Data(d) => body.extend_from_slice(&d),
            Chunk::End => break,
            Chunk::Cut => return obj(vec![("cut", JValue::Bool(true))]),
        }
    }
    obj(vec![
        ("status", JValue::Num(f64::from(status))),
        ("contentType", JValue::Str(content_type)),
        ("cacheControl", text("no-store")),
        ("body", JValue::Str(String::from_utf8_lossy(&body).into_owned())),
    ])
}

fn forwarded(sent: &[Forward]) -> JValue {
    JValue::Arr(
        sent.iter()
            .map(|f| {
                obj(vec![
                    ("url", text(&f.url)),
                    ("method", text("POST")),
                    ("contentType", text("application/json")),
                    ("accept", text(&f.accept)),
                    ("body", text(&f.body)),
                ])
            })
            .collect(),
    )
}

fn auth_of(spec: &str, tokens: &[String]) -> Option<String> {
    let last = tokens.last().cloned().unwrap_or_else(|| "ncr_none".into());
    match spec {
        "none" => None,
        "lease" => Some(format!("Bearer {last}")),
        "basic" => Some(format!("Basic {last}")),
        "lowercase" => Some(format!("bearer {last}")),
        "tab" => Some(format!("Bearer\t{last}")),
        "stale" => Some("Bearer ncr_QA_never_minted_token_000000000".into()),
        other => {
            let n: usize = other
                .strip_prefix("lease:")
                .and_then(|n| n.parse().ok())
                .unwrap_or_else(|| panic!("unknown auth {other}"));
            Some(format!("Bearer {}", tokens[n]))
        }
    }
}

const CHAT: &str = r#"{"model":"whatever-the-harness-says","stream":true,"messages":[{"role":"user","content":"QA hello"}]}"#;

async fn replay(fx: &JValue, sc: &JValue) -> Vec<String> {
    let name = s(sc.get("name"));
    let targets = fx.get("targets");
    let readies = match sc.get("ready") {
        JValue::Arr(list) => list.iter().map(|k| ready_of(fx.get("readies").get(&s(k)))).collect(),
        k => vec![ready_of(fx.get("readies").get(&s(k)))],
    };
    let world = Arc::new(Mutex::new(World {
        sent: Vec::new(),
        turns: Vec::new(),
        ready_calls: Vec::new(),
        hold_next: 0,
        held: Vec::new(),
    }));
    let fake = Arc::new(Fake {
        readies,
        ready_at: Mutex::new(0),
        context: sc.get("context").clone(),
        script: script_of(fx.get("upstreams").get(&s(sc.get("upstream")))),
        world: Arc::clone(&world),
    });
    let current = Arc::new(Mutex::new(target_of(targets.get(&s(sc.get("target"))))));
    let reader = Arc::clone(&current);
    let o = sc.get("options");
    let url = match o.get("url") {
        JValue::Undefined => Some("http://qa-relay.invalid:8095/v1".to_string()),
        u => u.as_str().map(str::to_string),
    };
    let log = Arc::new(Mutex::new(Vec::new()));
    let log_in = Arc::clone(&log);
    let mut options = Options::new(
        Arc::clone(&fake),
        Arc::new(move |_: &str| reader.lock().unwrap().clone()),
        url,
    );
    options.log = Arc::new(move |e| log_in.lock().unwrap().push(e));
    if let Some(n) = o.get("bodyCap").as_num() {
        options.body_cap = n as usize;
    }
    if let Some(n) = o.get("maxConcurrent").as_num() {
        options.max_concurrent = n as usize;
    }
    if let Some(n) = o.get("requestTimeoutMs").as_num() {
        options.request_timeout = Duration::from_millis(n as u64);
    }
    let relay = Relay::new(options);

    let mut leases = Vec::new();
    let mut tokens: Vec<String> = Vec::new();
    let mut held: Vec<Relayed<Up>> = Vec::new();
    let mut out = Vec::new();
    let JValue::Arr(steps) = sc.get("steps") else {
        panic!("{name}: no steps");
    };
    for step in steps {
        if let JValue::Obj(_) = step.get("open") {
            let o = step.get("open");
            let model = o.get("model").as_str().map(str::to_string);
            let expect = match o.get("expect") {
                JValue::Undefined => None,
                k => Some(target_key(target_of(targets.get(&s(k))).as_ref())),
            };
            let got = relay
                .open(
                    &s(o.get("taskId")),
                    model.as_deref(),
                    expect.as_ref().map(|e| e.as_deref()),
                )
                .await;
            out.push(obj(vec![(
                "open",
                match got {
                    Ok(l) => {
                        let shape = l.token.len() == 36
                            && l.token.starts_with("ncr_")
                            && l.token[4..]
                                .chars()
                                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
                        let v = obj(vec![
                            ("ok", JValue::Bool(true)),
                            ("baseUrl", text(&l.base_url)),
                            ("model", text(&l.model)),
                            (
                                "contextTokens",
                                l.context_tokens.map_or(JValue::Null, JValue::Num),
                            ),
                            ("machine", text(&l.target.machine_id)),
                            ("tokenShape", JValue::Bool(shape)),
                        ]);
                        tokens.push(l.token.clone());
                        leases.push(l);
                        v
                    }
                    Err(r) => obj(vec![
                        ("ok", JValue::Bool(false)),
                        ("code", text(&r.code)),
                        ("message", text(&r.message)),
                    ]),
                },
            )]));
        } else if let JValue::Obj(_) = step.get("request") {
            let r = step.get("request");
            let body = match r.get("bodyBytes").as_num() {
                Some(n) => vec![b'a'; n as usize],
                None => s(r.get("body")).into_bytes(),
            };
            let before = world.lock().unwrap().sent.len();
            let reply = relay
                .handle(Request {
                    method: s(r.get("method")),
                    url: s(r.get("path")),
                    authorization: auth_of(&s(r.get("auth")), &tokens),
                    accept: r.get("accept").as_str().map(str::to_string),
                    body,
                })
                .await;
            let got = answer(reply).await;
            let sent = world.lock().unwrap().sent[before..].to_vec();
            out.push(obj(vec![("request", got), ("forwarded", forwarded(&sent))]));
        } else if let Some(n) = step.get("hold").as_num() {
            let n = n as usize;
            let before = world.lock().unwrap().sent.len();
            world.lock().unwrap().hold_next = n;
            for _ in 0..n {
                let reply = relay
                    .handle(Request {
                        method: "POST".into(),
                        url: "/v1/chat/completions".into(),
                        authorization: auth_of("lease", &tokens),
                        accept: None,
                        body: CHAT.as_bytes().to_vec(),
                    })
                    .await;
                match reply {
                    Reply::Stream(r) => held.push(r),
                    other => panic!("{name}: a held request was answered at once: {other:?}"),
                }
            }
            let forwarded = world.lock().unwrap().sent.len() - before;
            out.push(obj(vec![("hold", obj(vec![("forwarded", num(forwarded))]))]));
        } else if step.get("release").as_bool() == Some(true) {
            world.lock().unwrap().held.clear();
            let mut answers = Vec::new();
            for r in held.drain(..) {
                answers.push(drain(r).await);
            }
            out.push(obj(vec![("release", JValue::Arr(answers))]));
        } else if !matches!(step.get("move"), JValue::Undefined) {
            *current.lock().unwrap() = target_of(targets.get(&s(step.get("move"))));
            out.push(obj(vec![("move", step.get("move").clone())]));
        } else if let Some(n) = step.get("close").as_num() {
            let n = n as usize;
            leases[n].close();
            out.push(obj(vec![("close", num(n))]));
        } else if step.get("active").as_bool() == Some(true) {
            out.push(obj(vec![("active", num(relay.active()))]));
        } else {
            panic!("{name}: unknown step {}", show(step));
        }
    }
    world.lock().unwrap().held.clear();
    drop(held);
    for l in &leases {
        l.close();
    }

    let mut diffs = Vec::new();
    let JValue::Arr(want) = sc.get("outcomes") else {
        panic!("{name}: no outcomes");
    };
    if want.len() != out.len() {
        diffs.push(format!(
            "{name}: {} outcomes, Node has {}",
            out.len(),
            want.len()
        ));
    }
    for (i, (got, want)) in out.iter().zip(want).enumerate() {
        if !same(got, want) {
            diffs.push(format!(
                "{name} step {i}: {}\n  ours: {}\n  Node: {}",
                show(&steps[i]),
                show(got),
                show(want)
            ));
        }
    }
    let log = JValue::Arr(log.lock().unwrap().clone());
    // The recorder closes every lease at the end; so do we (an extra relay.closed for a lease the
    // steps already closed is not logged twice by either side).
    if !same(&log, sc.get("log")) {
        diffs.push(format!(
            "{name}: log\n  ours: {}\n  Node: {}",
            show(&log),
            show(sc.get("log"))
        ));
    }
    let w = world.lock().unwrap();
    let turns = JValue::Arr(w.turns.iter().map(|t| text(t)).collect());
    if !same(&turns, sc.get("turns")) {
        diffs.push(format!(
            "{name}: machine turns\n  ours: {}\n  Node: {}",
            show(&turns),
            show(sc.get("turns"))
        ));
    }
    let calls = JValue::Arr(w.ready_calls.iter().map(|t| text(t)).collect());
    if !same(&calls, sc.get("readyCalls")) {
        diffs.push(format!(
            "{name}: ready calls\n  ours: {}\n  Node: {}",
            show(&calls),
            show(sc.get("readyCalls"))
        ));
    }
    diffs
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_relay_answers_as_core_s_recording() {
    let Some(fx) = fixture() else {
        return;
    };
    assert_eq!(fx.get("version").as_num(), Some(1.0));
    let JValue::Arr(scenarios) = fx.get("relay") else {
        panic!("the fixture has no relay scenarios");
    };
    assert!(scenarios.len() >= 20, "{} relay scenarios", scenarios.len());
    let mut diffs = Vec::new();
    for sc in scenarios {
        // Node's answers always carry no-store, and each forwarded request is a POST of JSON: the
        // replay builds those from the relay's contract, so check the recording says the same.
        if let JValue::Arr(outs) = sc.get("outcomes") {
            for o in outs {
                let r = o.get("request");
                if r.get("cut").as_bool() != Some(true) && r.is_object() {
                    assert_eq!(r.get("cacheControl").as_str(), Some("no-store"), "{}", show(o));
                }
                if let JValue::Arr(f) = o.get("forwarded") {
                    for f in f {
                        assert_eq!(f.get("method").as_str(), Some("POST"));
                        assert_eq!(f.get("contentType").as_str(), Some("application/json"));
                    }
                }
            }
        }
        diffs.extend(replay(&fx, sc).await);
    }
    assert!(
        diffs.is_empty(),
        "{} differences from core's relay:\n{}",
        diffs.len(),
        diffs.join("\n")
    );
}
