//! Replays the Node runner's recorded traces (`tests/fixtures/traces`, recorded by
//! `tests/fixtures/record-node.cjs` from noevia-core's own auto-tune tests; format in
//! `tests/fixtures/README.md`) against the native runner: every dependency call it makes must be
//! the next recorded one with the same arguments, and every state-file save must be byte for byte
//! the one Node wrote.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::too_many_lines
)]

use autotune_runner::{
    Abort, Call, Config, Context, Deps, Fail, Hooks, Quirks, Release, Runner, Spawn, StageHooks,
    StartBody,
};
use js_json::{wtf16, JValue};
use sha2::{Digest, Sha256};
use std::future::Future;
use std::sync::{Arc, Mutex, OnceLock};

const T0: f64 = 1_700_000_000_000.0;

type BoxFut<'a, T> = std::pin::Pin<Box<dyn Future<Output = T> + Send + 'a>>;
const STALL: u32 = 20_000;

fn sha256(text: &str) -> String {
    Sha256::digest(text.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn get_mut<'a>(v: &'a mut JValue, key: &str) -> Option<&'a mut JValue> {
    match v {
        JValue::Obj(items) => items.iter_mut().find(|(k, _)| k == key).map(|(_, x)| x),
        _ => None,
    }
}

fn set(v: &mut JValue, key: &str, value: JValue) {
    if let JValue::Obj(items) = v {
        wtf16::set(items, key, value);
    }
}

fn items(v: &JValue) -> &[JValue] {
    match v {
        JValue::Arr(a) => a,
        _ => &[],
    }
}

fn text(v: &JValue) -> String {
    wtf16::stringify(v).unwrap_or_else(|| "<undefined>".into())
}

// ── decoding ────────────────────────────────────────────────────────────────

/// Replaces `{ "$string": i }` by the shared string.
fn unshare(v: &mut JValue, strings: &[JValue]) {
    match v {
        JValue::Obj(items) => {
            if items.len() == 1 && items[0].0 == "$string" {
                if let Some(i) = items[0].1.as_num() {
                    *v = strings[i as usize].clone();
                    return;
                }
            }
            for (_, x) in items.iter_mut() {
                unshare(x, strings);
            }
        }
        JValue::Arr(items) => items.iter_mut().for_each(|x| unshare(x, strings)),
        _ => {}
    }
}

struct Decode {
    bodies: std::collections::HashMap<String, JValue>,
    save: Option<String>,
}

/// Restores `{ same: true }` bodies and save deltas, in preorder.
fn decode(events: &mut [JValue], d: &mut Decode) {
    for e in events.iter_mut() {
        let dep = e.get("dep").as_str().unwrap_or("").to_string();
        if dep == "rawModels" || dep == "request" {
            let path = if dep == "rawModels" {
                "<rawModels>".to_string()
            } else {
                e.get("path").as_str().unwrap_or("").to_string()
            };
            if let Some(body) = get_mut(e, "result").and_then(|r| get_mut(r, "body")) {
                let same = matches!(body, JValue::Obj(items) if items.len() == 1 && items[0].0 == "same" && items[0].1 == JValue::Bool(true));
                if same {
                    *body = d.bodies.get(&path).cloned().expect("a previous body");
                } else {
                    d.bodies.insert(path, body.clone());
                }
            }
        }
        if dep == "save" {
            if let JValue::Str(s) = e.get("state") {
                d.save = Some(s.clone());
            } else if let JValue::Arr(edits) = e.get("delta").clone() {
                let prev = d.save.clone().expect("a previous save");
                let bytes = prev.as_bytes();
                let mut out: Vec<u8> = Vec::new();
                let mut at = 0usize;
                for edit in &edits {
                    let keep = edit.get("0").as_num();
                    let (keep, del, ins) = match edit {
                        JValue::Arr(x) => (
                            x[0].as_num().unwrap() as usize,
                            x[1].as_num().unwrap() as usize,
                            x[2].as_str().unwrap().to_string(),
                        ),
                        _ => panic!("delta edit {keep:?}"),
                    };
                    out.extend_from_slice(&bytes[at..at + keep]);
                    at += keep + del;
                    out.extend_from_slice(ins.as_bytes());
                }
                out.extend_from_slice(&bytes[at..]);
                let s = String::from_utf8(out).expect("utf-8 save");
                set(e, "state", JValue::Str(s.clone()));
                d.save = Some(s);
            }
        }
        if let Some(JValue::Arr(during)) = get_mut(e, "during") {
            decode(during, d);
        }
    }
}

/// The JSON a save writes parses back to the same value; the replay compares text, which also
/// catches key order.
fn load(path: &std::path::Path) -> JValue {
    let raw = std::fs::read_to_string(path).expect("trace");
    let mut t = wtf16::parse(&raw).expect("trace JSON");
    let strings = t.get("strings").clone();
    let strings = items(&strings).to_vec();
    let events = get_mut(&mut t, "events").expect("events");
    unshare(events, &strings);
    if let JValue::Arr(evs) = events {
        decode(
            evs,
            &mut Decode {
                bodies: Default::default(),
                save: None,
            },
        );
    }
    t
}

// ── the replay ──────────────────────────────────────────────────────────────

struct Frame {
    events: Vec<JValue>,
    pos: usize,
}

struct Shared {
    frames: Vec<Frame>,
    ini: Option<String>,
    slept: f64,
    failure: Option<String>,
    job_id: Option<String>,
    runs: Vec<tokio::task::JoinHandle<()>>,
    progress: u64,
    context_ids: u64,
    /// Drivers in progress per task (`None`: the harness's own future).
    in_driver: std::collections::HashMap<Option<tokio::task::Id>, u32>,
    /// Each calibrator's callbacks, by id, for a callback Node's calibrator made outside a call.
    hooks: std::collections::HashMap<u64, StageHooks<Replay>>,
}

struct Inner {
    config: JValue,
    shared: Mutex<Shared>,
    runner: OnceLock<Runner<Replay>>,
}

#[derive(Clone)]
struct Replay(Arc<Inner>);

fn fail_of(t: &JValue) -> Fail {
    let flags = t.get("flags");
    Fail {
        message: t.get("message").as_str().unwrap_or("").to_string(),
        cancelled: flags.get("cancelled").truthy(),
        abort: flags.get("name").as_str() == Some("AbortError"),
        fatal: flags.get("fatal").truthy(),
        public_message: flags.get("publicMessage").as_str().map(str::to_string),
        engine_failure: flags.get("engineFailure").truthy(),
        unsafe_: flags.get("unsafe").truthy(),
        foreign_timeout: flags.get("foreignTimeout").truthy(),
        idle_timeout: flags.get("idleTimeout").truthy(),
        coverage_lost: flags.get("coverageLost").truthy(),
        code: flags.get("code").as_str().map(str::to_string),
        status: flags.get("status").as_num().map(|n| n as u16),
    }
}

fn outcome(e: &JValue) -> Result<JValue, Fail> {
    if !matches!(e.get("throws"), JValue::Undefined) {
        return Err(fail_of(e.get("throws")));
    }
    Ok(e.get("result").clone())
}

/// Token-like fields normalized as the recorder writes them.
fn normalize(v: &JValue) -> JValue {
    match v {
        JValue::Obj(items) => JValue::Obj(
            items
                .iter()
                .map(|(k, x)| {
                    let x = if matches!(
                        k.as_str(),
                        "token" | "generation" | "rowToken" | "preparedToken"
                    ) && x.as_str().is_some_and(|s| s != "<undefined>")
                    {
                        JValue::from("<token>")
                    } else {
                        normalize(x)
                    };
                    (k.clone(), x)
                })
                .filter(|(_, x)| !matches!(x, JValue::Undefined))
                .collect(),
        ),
        JValue::Arr(items) => JValue::Arr(items.iter().map(normalize).collect()),
        JValue::Str(s) if s == "<undefined>" => JValue::Undefined,
        other => other.clone(),
    }
}

impl Replay {
    fn sh(&self) -> std::sync::MutexGuard<'_, Shared> {
        self.0
            .shared
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn runner(&self) -> Runner<Replay> {
        self.0.runner.get().expect("runner").clone()
    }

    fn mismatch(&self, msg: String) -> Fail {
        let mut sh = self.sh();
        if sh.failure.is_none() {
            let depth = sh.frames.len();
            let pos = sh.frames.last().map_or(0, |f| f.pos);
            sh.failure = Some(format!("{msg} (frame depth {depth}, event {pos})"));
        }
        Fail::fatal("REPLAY MISMATCH")
    }

    fn failed(&self) -> bool {
        self.sh().failure.is_some()
    }

    fn peek(&self) -> Option<JValue> {
        let sh = self.sh();
        let f = sh.frames.last()?;
        f.events.get(f.pos).cloned()
    }

    fn pop(&self) -> Option<JValue> {
        let mut sh = self.sh();
        sh.progress += 1;
        let f = sh.frames.last_mut()?;
        let e = f.events.get(f.pos).cloned();
        f.pos += 1;
        e
    }

    fn self_driven(dep: &str) -> bool {
        matches!(dep, "driver" | "ini")
    }

    /// Runs the driver and `ini` events at the head of the top frame.
    fn pump(&self) -> BoxFut<'_, ()> {
        Box::pin(async move {
            while let Some(e) = self.peek() {
                let dep = e.get("dep").as_str().unwrap_or("");
                if !Self::self_driven(dep) || self.failed() {
                    return;
                }
                self.pop();
                self.self_driven_event(&e).await;
            }
        })
    }

    fn self_driven_event<'a>(&'a self, e: &'a JValue) -> BoxFut<'a, ()> {
        Box::pin(async move {
            match e.get("dep").as_str() {
                Some("ini") => self.sh().ini = e.get("text").as_str().map(str::to_string),
                Some("driver") => self.driver(e).await,
                _ => {}
            }
        })
    }

    /// Takes the next event, which must be `dep` and satisfy `check`; synchronously (no pump).
    fn take_sync(
        &self,
        dep: &str,
        check: impl FnOnce(&JValue) -> Result<(), String>,
    ) -> Result<JValue, Fail> {
        if self.failed() {
            return Err(Fail::fatal("REPLAY MISMATCH"));
        }
        let Some(e) = self.peek() else {
            return Err(self.mismatch(format!("native called {dep}, the trace has ended")));
        };
        if e.get("dep").as_str() == Some("driver") {
            return Err(self.mismatch(format!(
                "native made the synchronous call {dep} while Node's test call {} was due",
                text(e.get("method"))
            )));
        }
        if e.get("dep").as_str() != Some(dep) {
            return Err(self.mismatch(format!(
                "native called {dep}; Node next did {}",
                text(&e).chars().take(600).collect::<String>()
            )));
        }
        if let Err(why) = check(&e) {
            return Err(self.mismatch(format!("{dep}: {why}")));
        }
        self.pop();
        Ok(e)
    }

    /// A dependency call: pending drivers first, the matching event, its `during`, then the drivers
    /// that follow it (they ran before the runner resumed in Node).
    // Boxed with an explicit type: the replay recurses through the runner (a callback's calls
    // come back here), which an opaque `async fn` type cannot describe.
    fn call<'a>(
        &'a self,
        dep: &'a str,
        check: impl FnOnce(&JValue) -> Result<(), String> + Send + 'a,
        hooks: Option<&'a StageHooks<Replay>>,
    ) -> BoxFut<'a, Result<JValue, Fail>> {
        Box::pin(async move {
            self.pump().await;
            let e = self.take_sync(dep, check)?;
            if let JValue::Arr(during) = e.get("during") {
                self.drain(during.clone(), hooks).await?;
            }
            // A run task resumes synchronously after this call; test calls recorded right after
            // it ran before that in Node. Not for a call a driver made: the next driver is the
            // test's next call after this one returns.
            let task = tokio::task::try_id();
            if task.is_some() && self.sh().in_driver.get(&task).copied().unwrap_or(0) == 0 {
                self.pump().await;
            }
            if self.failed() {
                return Err(Fail::fatal("REPLAY MISMATCH"));
            }
            Ok(e)
        })
    }

    /// Plays a `during` list to its end: self-driven events here, the runner's own calls by
    /// whichever task makes them.
    fn drain<'a>(
        &'a self,
        events: Vec<JValue>,
        hooks: Option<&'a StageHooks<Replay>>,
    ) -> BoxFut<'a, Result<(), Fail>> {
        Box::pin(self.drain_inner(events, hooks))
    }

    async fn drain_inner(
        &self,
        events: Vec<JValue>,
        hooks: Option<&StageHooks<Replay>>,
    ) -> Result<(), Fail> {
        let depth = {
            let mut sh = self.sh();
            sh.frames.push(Frame { events, pos: 0 });
            sh.frames.len()
        };
        let mut stall = 0;
        loop {
            if self.failed() {
                break;
            }
            let (top, head) = {
                let sh = self.sh();
                let f = sh.frames.last().expect("frame");
                (sh.frames.len(), f.events.get(f.pos).cloned())
            };
            if top != depth {
                // A nested call is in flight on another task: let it finish.
                stall += 1;
                if stall > STALL {
                    self.mismatch("stalled with a nested call in flight".into());
                    break;
                }
                tokio::task::yield_now().await;
                continue;
            }
            let Some(head) = head else { break };
            let dep = head.get("dep").as_str().unwrap_or("").to_string();
            match dep.as_str() {
                "driver" | "ini" => {
                    self.pop();
                    self.self_driven_event(&head).await;
                    stall = 0;
                }
                "context.callback" => {
                    let id = head.get("id").as_num().unwrap_or(-1.0) as u64;
                    let registered = self.sh().hooks.get(&id).cloned();
                    let Some(h) = hooks.cloned().or(registered) else {
                        self.mismatch("a calibrator callback from an unknown calibrator".into());
                        break;
                    };
                    self.pop();
                    let got = callback(&h, &head).await;
                    let done = self.take_sync("context.callbackDone", |_| Ok(()));
                    if let Ok(done) = done {
                        let want = outcome(&done);
                        let same = match (&got, &want) {
                            (Ok(a), Ok(b)) => normalize(a) == normalize(b),
                            (Err(a), Err(b)) => a.message == b.message,
                            _ => false,
                        };
                        if !same {
                            self.mismatch(format!(
                                "callback {} answered {got:?}, Node {want:?}",
                                head.get("name").as_str().unwrap_or("")
                            ));
                        }
                    }
                    stall = 0;
                }
                "maintenance.callback" => {
                    self.pop();
                    let got = self.runner().recover_trial().await;
                    if let Ok(done) = self.take_sync("maintenance.callbackDone", |_| Ok(())) {
                        let want = outcome(&done);
                        if got.is_ok() != want.is_ok() {
                            self.mismatch(format!("recovery answered {got:?}, Node {want:?}"));
                        }
                    }
                    stall = 0;
                }
                "timer" => {
                    self.mismatch("a runner timer fired in Node".into());
                    break;
                }
                _ => {
                    stall += 1;
                    if stall > STALL {
                        self.mismatch(format!(
                            "native never made the call Node made next: {}",
                            text(&head).chars().take(600).collect::<String>()
                        ));
                        break;
                    }
                    tokio::task::yield_now().await;
                }
            }
        }
        let mut sh = self.sh();
        if sh.frames.len() == depth {
            sh.frames.pop();
        }
        Ok(())
    }

    fn token_arg(&self, arg: &JValue, current: impl FnOnce() -> String) -> String {
        match arg.as_str() {
            Some("<token>") => current(),
            Some("<stale-token>") => "0".repeat(64),
            Some(other) => other.to_string(),
            None => String::new(),
        }
    }

    /// One call the test made on the actor, compared with what Node answered.
    fn driver<'a>(&'a self, e: &'a JValue) -> BoxFut<'a, ()> {
        Box::pin(self.driver_inner(e))
    }

    async fn driver_inner(&self, e: &JValue) {
        let task = tokio::task::try_id();
        *self.sh().in_driver.entry(task).or_insert(0) += 1;
        self.driver_body(e).await;
        if let Some(n) = self.sh().in_driver.get_mut(&task) {
            *n -= 1;
        }
    }

    async fn driver_body(&self, e: &JValue) {
        let r = self.runner();
        let args = items(e.get("args")).to_vec();
        let arg = |i: usize| args.get(i).cloned().unwrap_or(JValue::Undefined);
        let method = e.get("method").as_str().unwrap_or("").to_string();
        let got: Result<JValue, Fail> = match method.as_str() {
            "start" => {
                self.sh().job_id = e
                    .get("result")
                    .get("body")
                    .get("id")
                    .as_str()
                    .map(str::to_string);
                let opts = arg(1);
                let present = |k: &str| {
                    (!matches!(opts.get(k), JValue::Undefined)).then(|| opts.get(k).clone())
                };
                let body = StartBody {
                    confirm_pause: present("confirmPause"),
                    prompt_budget_seconds: present("promptBudgetSeconds"),
                    untuned: opts.get("untuned").truthy(),
                    mode: present("mode"),
                };
                let model = arg(0).as_str().unwrap_or("").to_string();
                r.start(&model, body).await.map(|(status, body)| {
                    JValue::obj([
                        ("ok", JValue::Bool(status < 300)),
                        ("status", JValue::Num(f64::from(status))),
                        ("body", body),
                    ])
                })
            }
            "compareAndAbort" => {
                let t = self.token_arg(&arg(0), || {
                    r.cancellation_facts()
                        .get("token")
                        .as_str()
                        .unwrap_or("")
                        .to_string()
                });
                Ok(r.compare_and_abort(&t))
            }
            "compareAndClear" => {
                let t = self.token_arg(&arg(0), || {
                    r.clear_facts()
                        .get("token")
                        .as_str()
                        .unwrap_or("")
                        .to_string()
                });
                r.compare_and_clear(&t)
            }
            "commitSettings" => {
                let model = arg(0).as_str().unwrap_or("").to_string();
                let t = self.token_arg(&arg(2), || {
                    r.settings_facts(&model)
                        .get("token")
                        .as_str()
                        .unwrap_or("")
                        .to_string()
                });
                r.commit_settings(&model, arg(1) == JValue::Bool(true), &t)
            }
            "prepareResume" => {
                let t = self.token_arg(&arg(0), || {
                    r.resume_facts()
                        .get("token")
                        .as_str()
                        .unwrap_or("")
                        .to_string()
                });
                let o = arg(1);
                r.prepare_resume(
                    &t,
                    o.get("stage").as_str().unwrap_or(""),
                    o.get("selectors"),
                )
                .await
            }
            "compareAndResume" => {
                let t = self.token_arg(&arg(0), || {
                    r.resume_facts()
                        .get("token")
                        .as_str()
                        .unwrap_or("")
                        .to_string()
                });
                r.compare_and_resume(&t, &arg(1))
            }
            "recover" => r.recover().await.map(|()| JValue::Undefined),
            "flushReload" => r
                .flush_reload_public(arg(0).as_str())
                .await
                .map(|()| JValue::Undefined),
            "scanFacts" => r.scan_facts_public().await,
            other => {
                self.mismatch(format!("unknown driver method {other}"));
                return;
            }
        };
        let want = outcome(e);
        let same = match (&got, &want) {
            (Ok(a), Ok(b)) => text(&normalize(a)) == text(&normalize(b)),
            (Err(a), Err(b)) => a.message == b.message,
            _ => false,
        };
        if !same {
            let show = |r: &Result<JValue, Fail>| match r {
                Ok(v) => text(&normalize(v)),
                Err(f) => format!("throws {:?}", f.message),
            };
            self.mismatch(format!(
                "driver {method}: {}",
                diff(&show(&got), &show(&want))
            ));
        }
    }

    fn ini(&self) -> String {
        self.sh().ini.clone().unwrap_or_default()
    }

    fn config(&self) -> &JValue {
        &self.0.config
    }

    fn has_maintenance(&self, name: &str) -> bool {
        items(self.config().get("maintenance"))
            .iter()
            .any(|n| n.as_str() == Some(name))
    }
}

async fn callback(h: &StageHooks<Replay>, e: &JValue) -> Result<JValue, Fail> {
    let arg = e.get("arg").clone();
    match e.get("name").as_str().unwrap_or("") {
        "onUpdate" => {
            h.on_update(&arg);
            Ok(JValue::Undefined)
        }
        "onWrite" => {
            h.on_write(arg.as_str().unwrap_or(""));
            Ok(JValue::Undefined)
        }
        "applyUnlocked" => h.apply_unlocked(arg).await,
        "foreignHeld" => h.foreign_held(arg).await.map(JValue::Arr),
        "foreignWait" => h
            .foreign_wait(Fail::new(arg.get("message").as_str().unwrap_or("")))
            .await
            .map(|()| JValue::Undefined),
        "contextCurrent" => Ok(JValue::Bool(h.context_current())),
        other => Err(Fail::new(format!("unknown callback {other}"))),
    }
}

/// Both texts around their first difference.
fn diff(native: &str, node: &str) -> String {
    let at = native
        .char_indices()
        .zip(node.chars())
        .find(|((_, a), b)| a != b)
        .map_or(native.len().min(node.len()), |((i, _), _)| i);
    let from = native[..at]
        .char_indices()
        .rev()
        .nth(250)
        .map_or(0, |(i, _)| i);
    let window = |s: &str| {
        s.get(from..)
            .unwrap_or("")
            .chars()
            .take(500)
            .collect::<String>()
    };
    format!(
        "differs at {at}:\n    native …{}…\n    Node   …{}…",
        window(native),
        window(node)
    )
}

fn eq(field: &str, want: &JValue, got: &JValue) -> Result<(), String> {
    let (w, g) = (normalize(want), normalize(got));
    if w == g {
        Ok(())
    } else {
        Err(format!("{field} {}", diff(&text(&g), &text(&w))))
    }
}

struct ReplayRelease(Replay);

impl Release for ReplayRelease {
    fn set_reason(&self, reason: &str) {
        let _ = self.0.take_sync("release.setReason", |e| {
            eq("reason", e.get("reason"), &JValue::from(reason))
        });
    }
    fn release(&self) {
        let _ = self.0.take_sync("release", |_| Ok(()));
    }
}

struct ReplayContext {
    replay: Replay,
    id: f64,
    hooks: StageHooks<Replay>,
}

impl ReplayContext {
    fn same_id(&self, e: &JValue) -> Result<(), String> {
        if e.get("id").as_num() == Some(self.id) {
            Ok(())
        } else {
            Err(format!(
                "calibrator {} / Node {}",
                self.id,
                text(e.get("id"))
            ))
        }
    }
}

impl Context for ReplayContext {
    async fn start(&self, model: &str, prompt_budget_seconds: f64) -> Result<JValue, Fail> {
        let opts = JValue::obj([
            ("confirmPause", JValue::Bool(true)),
            ("promptBudgetSeconds", JValue::Num(prompt_budget_seconds)),
        ]);
        let e = self
            .replay
            .call(
                "context.start",
                |e| {
                    self.same_id(e)?;
                    eq("model", e.get("model"), &JValue::from(model))?;
                    eq("opts", e.get("opts"), &opts)
                },
                Some(&self.hooks),
            )
            .await?;
        outcome(&e)
    }
    async fn completion(&self) -> Result<(), Fail> {
        let e = self
            .replay
            .call("context.completion", |e| self.same_id(e), Some(&self.hooks))
            .await?;
        outcome(&e).map(|_| ())
    }
    fn status_job(&self, model: &str) -> JValue {
        match self.replay.take_sync("context.status", |e| {
            self.same_id(e)?;
            eq("model", e.get("model"), &JValue::from(model))
        }) {
            Ok(e) => e.get("result").get("body").get("job").clone(),
            Err(_) => JValue::Undefined,
        }
    }
    fn abort(&self) {
        let _ = self
            .replay
            .take_sync("context.cancellationFacts", |e| self.same_id(e));
        let _ = self
            .replay
            .take_sync("context.compareAndAbort", |e| self.same_id(e));
    }
    async fn probe(&self, model: &str, opts: JValue) -> Result<JValue, Fail> {
        let e = self
            .replay
            .call(
                "context.probe",
                |e| {
                    self.same_id(e)?;
                    eq("model", e.get("model"), &JValue::from(model))?;
                    eq("opts", e.get("opts"), &opts)
                },
                Some(&self.hooks),
            )
            .await?;
        outcome(&e)
    }
    fn cancel_probe(&self) {
        let _ = self
            .replay
            .take_sync("context.cancelProbe", |e| self.same_id(e));
    }
}

/// model-system.cjs's deprecated Laya name rule (the fixtures have no roles file).
fn laya(token: &str) -> bool {
    let t = token.trim().to_lowercase();
    t.strip_prefix("laya")
        .is_some_and(|rest| rest.is_empty() || rest.starts_with(['_', '.', '-']))
}

/// llamacpp-presets.cjs `parse(text)` and `get(model)`.
fn presets_get(ini: &str, model: &str) -> JValue {
    let header = regex::Regex::new(&format!(
        r"^[{S}]*\[([^\]\r\n]+)\][{S}]*(?:[;#].*)?$",
        S = JS_S
    ))
    .unwrap();
    let option = regex::Regex::new(&format!(
        r"^[{S}]*([^={S}]+)[{S}]*=[{S}]*(.*?)[{S}]*$",
        S = JS_S
    ))
    .unwrap();
    let mut sections: Vec<(String, JValue)> = Vec::new();
    for line in ini.split('\n') {
        if let Some(c) = header.captures(line) {
            sections.push((c[1].to_string(), JValue::Obj(Vec::new())));
        } else if let (Some(c), Some((_, opts))) = (option.captures(line), sections.last_mut()) {
            let key: Vec<u16> = c[1].encode_utf16().collect();
            if let Some(f) = llamacpp_presets::canonical(&key) {
                set(opts, f.name, JValue::from(&c[2]));
            }
        }
    }
    let find = |name: &str| {
        sections
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, o)| o.clone())
    };
    let section = find(model);
    JValue::obj([
        ("model", JValue::from(model)),
        ("revision", JValue::from(sha256(ini))),
        ("exists", JValue::Bool(section.is_some())),
        ("options", section.unwrap_or(JValue::Obj(Vec::new()))),
        ("defaults", find("*").unwrap_or(JValue::Obj(Vec::new()))),
    ])
}

const JS_S: &str = r"\t\n\x0B\x0C\r \x{a0}\x{1680}\x{2000}-\x{200a}\x{2028}\x{2029}\x{202f}\x{205f}\x{3000}\x{feff}";

impl Deps for Replay {
    type Release = ReplayRelease;
    type Context = ReplayContext;

    async fn raw_models(&self) -> Result<JValue, Fail> {
        let e = self.call("rawModels", |_| Ok(()), None).await?;
        outcome(&e)
    }

    async fn request(&self, call: Call, _abort: Option<Abort>) -> Result<JValue, Fail> {
        let e = self
            .call(
                "request",
                |e| {
                    eq("path", e.get("path"), &JValue::from(call.path.as_str()))?;
                    let method = e.get("method").as_str().unwrap_or("GET");
                    if method != call.method {
                        return Err(format!("method {} / Node {method}", call.method));
                    }
                    let body = call.body.clone().unwrap_or(JValue::Null);
                    let want = if e.get("body").is_nullish() {
                        JValue::Null
                    } else {
                        e.get("body").clone()
                    };
                    eq(&format!("body of {}", call.path), &want, &body)?;
                    eq(
                        "timeoutMs",
                        e.get("timeoutMs"),
                        &JValue::Num(call.timeout_ms as f64),
                    )
                },
                None,
            )
            .await?;
        outcome(&e)
    }

    fn presets_get(&self, model: &str) -> JValue {
        presets_get(&self.ini(), model)
    }

    fn presets_snapshot(&self) -> (String, String) {
        let ini = self.ini();
        let rev = sha256(&ini);
        (ini, rev)
    }

    async fn presets_commit(&self, candidate: JValue, options: JValue) -> Result<(), Fail> {
        let e = self
            .call(
                "presets.commit",
                |e| {
                    eq("candidate", e.get("candidate"), &candidate)?;
                    eq("options", e.get("options"), &options)
                },
                None,
            )
            .await?;
        if let Some(t) = e.get("iniAfter").as_str() {
            self.sh().ini = Some(t.to_string());
        }
        outcome(&e).map(|_| ())
    }

    async fn apply_unlocked(&self, body: JValue, write_options: JValue) -> Result<JValue, Fail> {
        let e = self
            .call(
                "applyUnlocked",
                |e| {
                    eq("body", e.get("body"), &body)?;
                    eq("writeOptions", e.get("writeOptions"), &write_options)
                },
                None,
            )
            .await?;
        if let Some(t) = e.get("iniAfter").as_str() {
            self.sh().ini = Some(t.to_string());
        }
        outcome(&e)
    }

    fn revision_of(&self, text: &str) -> String {
        sha256(text)
    }

    async fn identity_for(&self, model: &str) -> Result<Option<JValue>, Fail> {
        let e = self
            .call(
                "identityFor",
                |e| eq("model", e.get("model"), &JValue::from(model)),
                None,
            )
            .await?;
        outcome(&e).map(|v| (!v.is_nullish()).then_some(v))
    }

    async fn plan_facts(&self, model: &str) -> Result<JValue, Fail> {
        let e = self
            .call(
                "planFacts",
                |e| eq("model", e.get("model"), &JValue::from(model)),
                None,
            )
            .await?;
        outcome(&e).map(|v| if v.is_nullish() { JValue::Null } else { v })
    }

    async fn sampling_for(&self, model: &str) -> Result<JValue, Fail> {
        if !self.config().get("samplingFor").truthy() {
            return Ok(JValue::Null);
        }
        let e = self
            .call(
                "samplingFor",
                |e| eq("model", e.get("model"), &JValue::from(model)),
                None,
            )
            .await?;
        outcome(&e).map(|v| if v.is_nullish() { JValue::Null } else { v })
    }

    fn file_missing(&self, row: &JValue) -> bool {
        let id = js_json::to_js_string(row.get("id")).unwrap_or_default();
        self.config().get("fileMissing").get(&id).truthy()
    }

    fn is_system_model(&self, id: &str, path: Option<&str>) -> bool {
        laya(id)
            || path.is_some_and(|p| {
                p.split(['/', '\\']).filter(|s| !s.is_empty()).any(|seg| {
                    let t = if seg.to_lowercase().ends_with(".gguf") {
                        &seg[..seg.len() - 5]
                    } else {
                        seg
                    };
                    laya(t)
                })
            })
    }

    fn is_sidecar_model(&self, _id: &str) -> bool {
        false
    }

    fn quirks(&self, model: &str) -> Quirks {
        let re = regex::Regex::new(r"(?i)(?:^|[^a-z0-9])gpt[-_. ]?oss(?:\d|[^a-z0-9]|$)").unwrap();
        if re.is_match(model) {
            Quirks {
                harmony: true,
                reasoning_effort: Some("low".into()),
                quality_budget: Some(512),
            }
        } else {
            Quirks::default()
        }
    }

    fn read_memory(&self) -> Option<f64> {
        self.config().get("readMemory").as_num()
    }

    fn budget_gib(&self) -> Result<f64, Fail> {
        match self.config().get("budgetGib") {
            JValue::Num(n) => Ok(*n),
            JValue::Undefined => Ok(16.0),
            other => Err(fail_of(other.get("throws"))),
        }
    }

    fn ladder(&self, native: u64) -> Result<Vec<u64>, Fail> {
        let native = native as f64;
        let ceiling = if native > 0.0 { native } else { 131_072.0 };
        let mut grid: Vec<f64> = llamacpp_autoconfig::CTX_CANDIDATES
            .iter()
            .copied()
            .chain((native > 0.0).then_some(native))
            .filter(|&c| c >= 4096.0 && c <= ceiling)
            .collect();
        grid.sort_by(f64::total_cmp);
        grid.dedup();
        Ok(grid.into_iter().map(|c| c as u64).collect())
    }

    fn cache_ram_limits(&self) -> (f64, f64) {
        // inference-budget.cjs defaults (the tests set neither variable).
        (1024.0, 2048.0)
    }

    fn allow_below_q5(&self) -> bool {
        self.config().get("allowBelowQ5Kv").truthy()
    }

    async fn hold_when_idle(
        &self,
        reason: &str,
        timeout_ms: u64,
        _abort: Abort,
    ) -> Result<ReplayRelease, Fail> {
        let e = self
            .call(
                "holdWhenIdle",
                |e| {
                    eq("reason", e.get("reason"), &JValue::from(reason))?;
                    eq(
                        "timeoutMs",
                        e.get("timeoutMs"),
                        &JValue::Num(timeout_ms as f64),
                    )
                },
                None,
            )
            .await?;
        outcome(&e).map(|_| ReplayRelease(self.clone()))
    }

    fn assert_covered(&self) -> Result<(), Fail> {
        if !self.has_maintenance("assertCovered") {
            return Ok(());
        }
        let e = self.take_sync("maintenance.assertCovered", |_| Ok(()))?;
        outcome(&e).map(|_| ())
    }

    fn coverage_lost(&self) -> bool {
        if !self.has_maintenance("coverageSignal") {
            return false;
        }
        self.take_sync("maintenance.coverageSignal", |_| Ok(()))
            .is_ok_and(|e| e.get("result").get("aborted") == &JValue::Bool(true))
    }

    fn recoverable(&self) -> bool {
        self.has_maintenance("requireRecovery")
    }

    fn require_recovery(&self, key: &str, message: &str) {
        let _ = self.take_sync("maintenance.requireRecovery", |e| {
            let args = e.get("args");
            let (k, m) = if args.truthy() {
                (
                    items(args).first().cloned().unwrap_or(JValue::Undefined),
                    items(args).get(2).cloned().unwrap_or(JValue::Undefined),
                )
            } else {
                (e.get("key").clone(), e.get("message").clone())
            };
            eq("key", &k, &JValue::from(key))?;
            eq("message", &m, &JValue::from(message))
        });
    }

    async fn recover_now(&self) {
        if self.has_maintenance("recoverNow") {
            let _ = self.call("maintenance.recoverNow", |_| Ok(()), None).await;
        }
    }

    async fn invalidate_baseline(&self, tag: &str) -> bool {
        self.call(
            "invalidateBaseline",
            |e| eq("tag", e.get("tag"), &JValue::from(tag)),
            None,
        )
        .await
        .is_ok_and(|e| e.get("result").truthy())
    }

    fn reload_router(&self) -> Option<impl Future<Output = Result<JValue, Fail>> + Send> {
        if !self.config().get("routerReload").truthy() {
            return None;
        }
        let me = self.clone();
        Some(async move {
            let e = me.call("reloadRouter", |_| Ok(()), None).await?;
            outcome(&e)
        })
    }

    fn context(&self, hooks: StageHooks<Replay>, fill_only: bool) -> ReplayContext {
        let id = {
            let mut sh = self.sh();
            sh.context_ids += 1;
            sh.context_ids as f64
        };
        let names: Vec<&str> = if fill_only {
            vec!["applyUnlocked", "onWrite", "onUpdate"]
        } else {
            vec![
                "contextCurrent",
                "applyUnlocked",
                "foreignHeld",
                "foreignWait",
                "onWrite",
                "onUpdate",
            ]
        };
        let recorded = self.take_sync("context.create", |_| Ok(()));
        let id = recorded
            .as_ref()
            .ok()
            .and_then(|e| e.get("id").as_num())
            .unwrap_or(id);
        if let Ok(e) = &recorded {
            let mut want: Vec<String> = items(e.get("callbacks"))
                .iter()
                .filter_map(|n| n.as_str().map(str::to_string))
                .collect();
            let mut got: Vec<String> = names.iter().map(|s| s.to_string()).collect();
            want.sort();
            got.sort();
            if want != got {
                self.mismatch(format!("calibrator callbacks {got:?} / Node {want:?}"));
            }
        }
        self.sh().hooks.insert(id as u64, hooks.clone());
        ReplayContext {
            replay: self.clone(),
            id,
            hooks,
        }
    }

    async fn on_result(&self, model: &str, result: &JValue) -> Result<(), Fail> {
        let e = self
            .call(
                "onResult",
                |e| {
                    eq("model", e.get("model"), &JValue::from(model))?;
                    eq("result", e.get("result"), result)
                },
                None,
            )
            .await?;
        if !matches!(e.get("throws"), JValue::Undefined) {
            return Err(fail_of(e.get("throws")));
        }
        Ok(())
    }

    async fn judge(&self, request: JValue) -> Result<JValue, Fail> {
        let e = self
            .call("decide", |e| decide_check(e, "judge", &request), None)
            .await?;
        self.check_native(&e);
        outcome(&e).map(|v| if v.is_nullish() { JValue::Null } else { v })
    }

    fn plan(&self, request: &JValue) -> Result<JValue, Fail> {
        let e = self.take_sync("decide", |e| decide_check(e, "plan", request))?;
        self.check_native(&e);
        outcome(&e)
    }

    fn contention(&self, request: &JValue) -> Result<JValue, Fail> {
        let e = self.take_sync("decide", |e| decide_check(e, "contention", request))?;
        self.check_native(&e);
        outcome(&e)
    }

    fn template_caps(&self, template: &str) -> Result<JValue, Fail> {
        let e = self.take_sync("decide", |e| {
            decide_check(e, "templateCaps", &JValue::from(template))
        })?;
        self.check_native(&e);
        outcome(&e)
    }

    fn serving_verdict(&self, status: f64, body_text: &str) -> Result<JValue, Fail> {
        let request = JValue::obj([
            ("status", JValue::Num(status)),
            ("bodyText", JValue::from(body_text)),
        ]);
        let e = self.take_sync("decide", |e| decide_check(e, "servingVerdict", &request))?;
        self.check_native(&e);
        outcome(&e)
    }

    fn scan(&self, request: JValue) -> Result<JValue, Fail> {
        let (code, out) =
            autotune_plan::scan::run_json(&wtf16::stringify(&request).unwrap_or_default());
        if code != 0 {
            return Err(Fail::new(out));
        }
        Ok(wtf16::parse(&out).unwrap_or(JValue::Null))
    }

    fn long_pairs(&self, rows: &JValue) -> JValue {
        let request = JValue::obj([("rows", rows.clone())]);
        self.take_sync("decide", |e| decide_check(e, "longPairs", &request))
            .map(|e| {
                self.check_native(&e);
                e.get("result").clone()
            })
            .unwrap_or(JValue::Arr(Vec::new()))
    }

    fn long_section(&self, text: &str, row: &JValue) -> JValue {
        let request = JValue::obj([("text", JValue::from(text)), ("row", row.clone())]);
        self.take_sync("decide", |e| decide_check(e, "longSection", &request))
            .map(|e| {
                self.check_native(&e);
                e.get("result").clone()
            })
            .unwrap_or(JValue::Null)
    }

    fn now(&self) -> f64 {
        T0 + self.sh().slept
    }

    async fn sleep(&self, ms: u64) {
        if self
            .call(
                "sleep",
                |e| eq("ms", e.get("ms"), &JValue::Num(ms as f64)),
                None,
            )
            .await
            .is_ok()
        {
            self.sh().slept += (ms as f64).max(1.0);
        }
    }

    async fn tick(&self, _ms: u64) {
        std::future::pending::<()>().await;
    }

    fn spawn(&self, kind: Spawn, task: impl Future<Output = ()> + Send + 'static) {
        if kind == Spawn::Run {
            let handle = tokio::spawn(task);
            self.sh().runs.push(handle);
        }
    }

    fn random_uuid(&self) -> String {
        self.sh()
            .job_id
            .clone()
            .unwrap_or_else(|| "00000000-0000-4000-8000-000000000000".into())
    }

    fn save(&self, state: &str) -> Result<(), Fail> {
        let e = self.take_sync("save", |e| {
            if !matches!(e.get("throws"), JValue::Undefined) {
                return Ok(());
            }
            let want = e.get("state").as_str().unwrap_or("");
            if want == state {
                return Ok(());
            }
            let at = want
                .bytes()
                .zip(state.bytes())
                .position(|(a, b)| a != b)
                .unwrap_or(want.len().min(state.len()));
            let from = at.saturating_sub(300);
            let cut = |s: &str| {
                s.get(from..(at + 300).min(s.len()))
                    .unwrap_or("")
                    .to_string()
            };
            Err(format!(
                "save differs at byte {at}:\n  native …{}…\n  Node   …{}…",
                cut(state),
                cut(want)
            ))
        })?;
        if !matches!(e.get("throws"), JValue::Undefined) {
            return Err(fail_of(e.get("throws")));
        }
        Ok(())
    }
}

fn decide_check(e: &JValue, f: &str, request: &JValue) -> Result<(), String> {
    if e.get("fn").as_str() != Some(f) {
        return Err(format!("decision {f} / Node {}", text(e.get("fn"))));
    }
    eq(&format!("{f} request"), e.get("request"), request)
}

/// A section's `key = value` line of models.ini (presets.files' `model` / `mmproj`).
fn ini_value(ini: &str, section: &str, key: &str) -> Option<String> {
    let mut inside = false;
    for line in ini.split('\n') {
        let t = line.trim();
        if t.starts_with('[') {
            inside = t.trim_start_matches('[').split(']').next() == Some(section);
            continue;
        }
        if inside {
            if let Some((k, v)) = t.split_once('=') {
                if k.trim() == key {
                    return Some(v.trim().to_string()).filter(|v| !v.is_empty());
                }
            }
        }
    }
    None
}

impl Replay {
    /// For a decision Node took with its own implementation (`injected: false`), the crate's
    /// production answer must be the one Node recorded.
    fn check_native(&self, e: &JValue) {
        if e.get("injected") != &JValue::Bool(false) {
            return;
        }
        // A Node test that patches the WASM module itself (trace 130's "synthetic planner fault")
        // records its fault as the default implementation's: not a decision the crate takes.
        if e.get("throws")
            .get("message")
            .as_str()
            .is_some_and(|m| m.starts_with("synthetic "))
        {
            return;
        }
        let request = e.get("request");
        let ini = self.ini();
        let file_of = |row: &JValue| {
            autotune_runner::model_path_from_args(row.get("status").get("args"))
                .or_else(|| ini_value(&ini, row.get("id").as_str()?, "model"))
        };
        let native: Result<JValue, Fail> = match e.get("fn").as_str().unwrap_or("") {
            "plan" => autotune_runner::decide::plan(request),
            "contention" => autotune_runner::decide::contention(request),
            "templateCaps" => {
                autotune_runner::decide::template_caps(request.as_str().unwrap_or(""))
            }
            "servingVerdict" => autotune_runner::decide::serving_verdict(
                request.get("status").as_num().unwrap_or(f64::NAN),
                request.get("bodyText").as_str().unwrap_or(""),
            ),
            "judge" => Ok(autotune_runner::decide::judge(request)),
            "longPairs" => Ok(autotune_runner::decide::long_pairs(
                request.get("rows"),
                file_of,
            )),
            "longSection" => {
                let row = request.get("row");
                let mmproj = autotune_runner::decide::arg_of(
                    row.get("status").get("args"),
                    &["--mmproj", "-mm"],
                )
                .or_else(|| ini_value(&ini, row.get("id").as_str()?, "mmproj"));
                Ok(autotune_runner::decide::long_section(
                    request.get("text").as_str().unwrap_or(""),
                    row.get("id"),
                    file_of(row),
                    mmproj,
                ))
            }
            _ => return,
        };
        let want = outcome(e);
        let same = match (&native, &want) {
            (Ok(a), Ok(b)) => normalize(a) == normalize(b),
            (Err(_), Err(_)) => true,
            _ => false,
        };
        if !same {
            self.mismatch(format!(
                "native {} differs from Node's: native {:?} / Node {:?}",
                text(e.get("fn")),
                native.map(|v| text(&v)),
                want.map(|v| text(&v))
            ));
        }
    }
}

/// Plays one trace; `Err` is the first divergence.
async fn replay(path: &std::path::Path) -> Result<(), String> {
    let t = load(path);
    let config = t.get("config").clone();
    let c = Config {
        between_models_ms: config.get("betweenModelsMs").as_num().unwrap_or(1000.0) as u64,
        idle_timeout_ms: config.get("idleTimeoutMs").as_num().unwrap_or(300_000.0) as u64,
        memory_floor_gib: config.get("memoryFloorGib").as_num().unwrap_or(2.0),
        foreign_wait_ms: config.get("foreignWaitMs").as_num().unwrap_or(900_000.0) as u64,
        foreign_quiet_ms: config.get("foreignQuietMs").as_num().unwrap_or(30_000.0) as u64,
        foreign_poll_ms: config.get("foreignPollMs").as_num().unwrap_or(2000.0) as u64,
        services_reserve_mib: config.get("servicesReserveMib").as_num().unwrap_or(2560.0) as u64,
    };
    let events = items(t.get("events")).to_vec();
    let replay = Replay(Arc::new(Inner {
        config,
        shared: Mutex::new(Shared {
            frames: vec![Frame { events, pos: 0 }],
            ini: t.get("initial").get("ini").as_str().map(str::to_string),
            slept: 0.0,
            failure: None,
            job_id: None,
            runs: Vec::new(),
            progress: 0,
            context_ids: 0,
            in_driver: Default::default(),
            hooks: Default::default(),
        }),
        runner: OnceLock::new(),
    }));
    let state = t.get("initial").get("state").as_str().map(str::to_string);
    let runner = Runner::new(replay.clone(), c, state.as_deref())
        .map_err(|e| format!("construction: {}", e.message))?;
    let _ = replay.0.runner.set(runner);
    let mut stall = 0u32;
    loop {
        if let Some(f) = replay.sh().failure.clone() {
            return Err(f);
        }
        let (depth, head) = {
            let sh = replay.sh();
            let f = &sh.frames[0];
            (sh.frames.len(), f.events.get(f.pos).cloned())
        };
        let Some(head) = head else {
            if depth == 1 {
                break;
            }
            tokio::task::yield_now().await;
            continue;
        };
        if depth > 1 {
            stall += 1;
            if stall > STALL * 4 {
                return Err("stalled in a nested call".into());
            }
            tokio::task::yield_now().await;
            continue;
        }
        let dep = head.get("dep").as_str().unwrap_or("").to_string();
        let running = replay.sh().runs.iter().any(|h| !h.is_finished());
        match dep.as_str() {
            "driver" | "ini" if !running => {
                replay.pop();
                replay.self_driven_event(&head).await;
                stall = 0;
            }
            "driverAwait" => {
                let last = replay.sh().runs.pop();
                if let Some(h) = last {
                    let mut n = 0;
                    while !h.is_finished() {
                        n += 1;
                        if n > STALL * 4 || replay.failed() {
                            break;
                        }
                        tokio::task::yield_now().await;
                    }
                    if !h.is_finished() && !replay.failed() {
                        let next = replay.peek().map(|e| text(&e)).unwrap_or_default();
                        return Err(format!(
                            "the native run did not finish where Node's did; next: {}",
                            next.chars().take(400).collect::<String>()
                        ));
                    }
                }
                replay.pop();
                stall = 0;
            }
            "timer" => return Err("a runner timer fired in Node".into()),
            "context.callback" if !running => {
                // A calibrator's own late read (a cancelled stage's pending check): played as a
                // one-event frame so the callback and its `callbackDone` are taken in order.
                let rest: Vec<JValue> = {
                    let mut sh = replay.sh();
                    let f = &mut sh.frames[0];
                    let end = f.events[f.pos..]
                        .iter()
                        .position(|e| e.get("dep").as_str() == Some("context.callbackDone"))
                        .map_or(f.events.len(), |n| f.pos + n + 1);
                    let taken = f.events[f.pos..end].to_vec();
                    f.pos = end;
                    taken
                };
                replay.drain(rest, None).await.ok();
                stall = 0;
            }
            _ => {
                stall += 1;
                if stall > STALL * 4 {
                    return Err(format!(
                        "native never made the call Node made next: {}",
                        text(&head).chars().take(600).collect::<String>()
                    ));
                }
                tokio::task::yield_now().await;
            }
        }
    }
    if let Some(f) = replay.sh().failure.clone() {
        return Err(f);
    }
    Ok(())
}

fn traces() -> Vec<std::path::PathBuf> {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/traces");
    let mut v: Vec<_> = std::fs::read_dir(dir)
        .expect("traces")
        .map(|e| e.expect("entry").path())
        .collect();
    v.sort();
    v
}

/// Traces whose Node test injects a fake the trace format leaves derived, so a replay cannot
/// rebuild what Node read. Each is named with why; anything else must replay exactly.
const NOT_DERIVABLE: [(&str, &str); 2] = [
    (
        "037-545-the-bulk-untuned-list-skips-presets-whose-model-file-is-",
        "the test's presets object is a fake with no models.ini behind it (exists: true for every id)",
    ),
    (
        "132-entire-raw-evaluator-fault-stops-independent-kv-sizing-befor-",
        "the test replaces Node's sizing evaluator with one that throws; the native sizing is the crate's own and has no such fault",
    ),
];

#[test]
fn recorded_node_traces_replay() {
    let only = std::env::var("REPLAY_ONLY").ok();
    let mut results = Vec::new();
    for path in traces() {
        let name = path.file_name().unwrap().to_string_lossy().to_string();
        if only.as_ref().is_some_and(|o| !name.contains(o.as_str())) {
            continue;
        }
        let head = load(&path);
        if head.get("replayable") != &JValue::Bool(true) {
            continue;
        }
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap();
        let r = rt.block_on(replay(&path));
        results.push((name, r));
    }
    let excused = |name: &str| {
        NOT_DERIVABLE
            .iter()
            .any(|(prefix, _)| name.starts_with(prefix))
    };
    for (name, r) in &results {
        if excused(name) && r.is_ok() {
            eprintln!("note: {name} now replays; take it off NOT_DERIVABLE");
        }
    }
    let failed: Vec<_> = results
        .iter()
        .filter(|(n, r)| r.is_err() && !excused(n))
        .collect();
    for (name, r) in &failed {
        eprintln!("FAIL {name}\n  {}\n", r.as_ref().err().unwrap());
    }
    let passed = results.iter().filter(|(_, r)| r.is_ok()).count();
    eprintln!(
        "replayed {} traces: {passed} passed, {} failed, {} not derivable",
        results.len(),
        failed.len(),
        results.len() - passed - failed.len()
    );
    assert!(failed.is_empty());
    if only.is_none() {
        assert!(
            passed >= 149,
            "fewer traces replay than when the runner landed: {passed}"
        );
    }
}
