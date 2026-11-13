//! R6 C1: the auto-tune's private calibrator (`model_maint::autotune_context`) against the Node
//! oracle. Every trace in `tests/fixtures/private-calibrator/traces` (recorded from core's own
//! llamacpp-full-autotune tests by `record-node.cjs`, format in `FORMAT.md` beside it) is replayed:
//! the calibrator is driven with the recorded runner calls and must make the recorded dependency
//! calls in order, with the same arguments, and answer what Node answered.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]

use autotune_runner::{js, Abort, Call, Context, Fail, Hooks};
use js_json::{wtf16, JValue};
use noevia_server::model_maint::autotune_context::{Body, Engine, Limits, PrivateCalibrator};
use std::future::Future;
use std::path::PathBuf;
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::Duration;

const PAD: &str = "This is synthetic padding for a context allocation validation.\n";
const T0: f64 = 1_700_000_000_000.0;
/// How long a call may wait for its event to come up before the replay calls it a mismatch.
const WAIT: Duration = Duration::from_millis(1500);

/// The recorded traces; `TRACE_DIR` (with `TRACE`) replays another directory, such as edited copies
/// made to check that the replay catches a change.
fn dir() -> PathBuf {
    match std::env::var_os("TRACE_DIR") {
        Some(d) => PathBuf::from(d),
        None => PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/private-calibrator/traces"),
    }
}

/// `{ "$s": i }` → the string (a `strings` entry may be `{ prefix, pad, suffix }`).
fn resolve(v: &JValue, strings: &[String]) -> JValue {
    match v {
        JValue::Obj(items) => {
            if items.len() == 1 && items[0].0 == "$s" {
                let i = items[0].1.as_num().unwrap() as usize;
                return JValue::Str(strings[i].clone());
            }
            JValue::Obj(
                items
                    .iter()
                    .map(|(k, x)| (k.clone(), resolve(x, strings)))
                    .collect(),
            )
        }
        JValue::Arr(items) => JValue::Arr(items.iter().map(|x| resolve(x, strings)).collect()),
        other => other.clone(),
    }
}

fn string_of(entry: &JValue) -> String {
    match entry {
        JValue::Str(s) => s.clone(),
        o => format!(
            "{}{}{}",
            o.get("prefix").as_str().unwrap_or(""),
            PAD.repeat(o.get("pad").as_num().unwrap_or(0.0) as usize),
            o.get("suffix").as_str().unwrap_or("")
        ),
    }
}

fn text(v: &JValue) -> String {
    wtf16::stringify(&js::json_copy(v)).unwrap_or_else(|| "undefined".into())
}

/// One event in preorder, with the index just past its `during`.
#[derive(Debug, Clone)]
struct Ev {
    v: JValue,
    end: usize,
}

fn flatten(events: &[JValue], out: &mut Vec<Ev>) {
    for e in events {
        let at = out.len();
        out.push(Ev {
            v: e.clone(),
            end: 0,
        });
        flatten(js::items(e.get("during")), out);
        out[at].end = out.len();
    }
}

fn label(e: &JValue) -> String {
    match e.get("dep").as_str().unwrap_or("?") {
        "driver" => format!("driver {}", e.get("method").as_str().unwrap_or("?")),
        "hook" => format!("hook {}", e.get("name").as_str().unwrap_or("?")),
        d => d.to_string(),
    }
}

fn fail_of(t: &JValue) -> Fail {
    let f = t.get("flags");
    Fail {
        message: t.get("message").as_str().unwrap_or("").to_string(),
        cancelled: f.get("cancelled").truthy(),
        fatal: f.get("fatal").truthy(),
        coverage_lost: f.get("coverageLost").truthy(),
        foreign_timeout: f.get("foreignTimeout").truthy(),
        abort: f.get("name").as_str() == Some("AbortError"),
        ..Fail::default()
    }
}

/// `{ message, flags }` as the recorder writes an error (backend/name are not kept on a `Fail`).
fn error_json(e: &Fail) -> JValue {
    let mut flags = Vec::new();
    for (k, on) in [
        ("cancelled", e.cancelled),
        ("fatal", e.fatal),
        ("coverageLost", e.coverage_lost),
        ("foreignTimeout", e.foreign_timeout),
    ] {
        if on {
            flags.push((k.to_string(), JValue::Bool(true)));
        }
    }
    JValue::obj([
        ("message", JValue::from(e.message.as_str())),
        ("flags", JValue::Obj(flags)),
    ])
}

fn without_backend(v: &JValue) -> JValue {
    let mut v = js::json_copy(v);
    if let Some(f) = js::get_mut(&mut v, "flags") {
        js::del(f, "backend");
        js::del(f, "name");
    }
    v
}

type Ctx = PrivateCalibrator<ReplayEngine, ReplayHooks>;

struct Cursor {
    pos: usize,
    mismatch: Option<String>,
    clock: f64,
    streams: usize,
    pending: Vec<(usize, tokio::task::JoinHandle<Result<JValue, Fail>>)>,
}

struct Replay {
    events: Vec<Ev>,
    cur: Mutex<Cursor>,
    changed: Condvar,
    ctx: OnceLock<Ctx>,
    memory: Memory,
    rt: tokio::runtime::Handle,
}

impl Replay {
    fn fail(&self, c: &mut Cursor, why: String) {
        if c.mismatch.is_none() {
            c.mismatch = Some(why);
        }
        self.changed.notify_all();
    }

    /// Takes the next event for a dependency call `dep` (its arguments checked by `check`), then
    /// waits until everything recorded during it has happened. `None` after a mismatch.
    fn take(&self, what: &str, args: &[(&str, JValue)]) -> Option<JValue> {
        let mut c = self.cur.lock().unwrap();
        loop {
            if c.mismatch.is_some() {
                // Past a mismatch every deadline is over, so no loop of the calibrator spins on.
                c.clock = f64::INFINITY;
                return None;
            }
            let Some(head) = self.events.get(c.pos) else {
                let why = format!(
                    "extra call {what} {} after the trace ended",
                    text(&JValue::obj(args.iter().cloned()))
                );
                self.fail(&mut c, why);
                return None;
            };
            let dep = label(&head.v);
            if dep == what {
                let mut bad = Vec::new();
                for (k, v) in args {
                    let (want, got) = (text(head.v.get(k)), text(v));
                    if want != got {
                        bad.push(format!("{k}: node {want} / rust {got}"));
                    }
                }
                if !bad.is_empty() {
                    let why = format!("event {} {what}: {}", c.pos, bad.join("; "));
                    self.fail(&mut c, why);
                    return None;
                }
                let at = c.pos;
                c.pos += 1;
                self.changed.notify_all();
                // Wait for the events recorded during this call.
                let end = self.events[at].end;
                while c.pos < end && c.mismatch.is_none() {
                    let (g, t) = self.changed.wait_timeout(c, WAIT).unwrap();
                    c = g;
                    if t.timed_out() && c.pos < end {
                        let head = label(&self.events[c.pos].v);
                        let why = format!(
                            "event {at} {what}: stuck at event {} ({head}) inside it",
                            c.pos
                        );
                        self.fail(&mut c, why);
                    }
                }
                if c.mismatch.is_some() {
                    return None;
                }
                if what == "sleep" {
                    c.clock += head_ms(&self.events[at].v).max(1.0);
                }
                return Some(self.events[at].v.clone());
            }
            let (g, t) = self.changed.wait_timeout(c, WAIT).unwrap();
            c = g;
            if t.timed_out() && self.events.get(c.pos).map(|h| label(&h.v)) == Some(dep.clone()) {
                let why = format!(
                    "event {}: node {dep} / rust {what} {}",
                    c.pos,
                    text(&JValue::obj(args.iter().cloned()))
                );
                self.fail(&mut c, why);
            }
        }
    }

    /// The driver pump: runs the runner's recorded calls when they come up.
    fn pump(self: &Arc<Self>) {
        let mut c = self.cur.lock().unwrap();
        loop {
            if c.mismatch.is_some() {
                return;
            }
            let Some(head) = self.events.get(c.pos).cloned() else {
                return;
            };
            if head.v.get("dep").as_str() != Some("driver") {
                let (g, _) = self.changed.wait_timeout(c, WAIT).unwrap();
                c = g;
                continue;
            }
            let at = c.pos;
            c.pos += 1;
            self.changed.notify_all();
            drop(c);
            self.drive(at, &head.v);
            c = self.cur.lock().unwrap();
        }
    }

    fn drive(self: &Arc<Self>, at: usize, e: &JValue) {
        let ctx = self.ctx.get().unwrap().clone();
        let args = js::items(e.get("args")).to_vec();
        let arg = |i: usize| args.get(i).cloned().unwrap_or(JValue::Undefined);
        let method = e.get("method").as_str().unwrap_or("");
        let spawned: Option<tokio::task::JoinHandle<Result<JValue, Fail>>> = match method {
            "start" => {
                let model = arg(0).as_str().unwrap_or("").to_string();
                let budget = js::number(arg(1).get("promptBudgetSeconds"));
                Some(
                    self.rt
                        .spawn(async move { ctx.start(&model, budget).await }),
                )
            }
            "completion" => Some(
                self.rt
                    .spawn(async move { ctx.completion().await.map(|()| JValue::Null) }),
            ),
            "probe" => {
                let model = arg(0).as_str().unwrap_or("").to_string();
                let opts = arg(1);
                Some(self.rt.spawn(async move { ctx.probe(&model, opts).await }))
            }
            "status" => {
                let got = JValue::obj([(
                    "body",
                    JValue::obj([("job", ctx.status_job(arg(0).as_str().unwrap_or("")))]),
                )]);
                self.check_result(at, e, &Ok(got));
                None
            }
            "cancellationFacts" => {
                let got = ctx.job();
                let want = e.get("result").get("job");
                if text(want) != text(&got) {
                    let mut c = self.cur.lock().unwrap();
                    self.fail(
                        &mut c,
                        format!(
                            "event {at} driver cancellationFacts job: node {} / rust {}",
                            text(want),
                            text(&got)
                        ),
                    );
                }
                None
            }
            "compareAndAbort" | "cancel" => {
                if method == "cancel" || arg(0).as_str() == Some("<token>") {
                    ctx.abort();
                }
                None
            }
            "cancelProbe" => {
                ctx.cancel_probe();
                None
            }
            other => {
                let mut c = self.cur.lock().unwrap();
                self.fail(
                    &mut c,
                    format!("event {at}: driver method {other} has no Rust counterpart"),
                );
                None
            }
        };
        if let Some(h) = spawned {
            self.cur.lock().unwrap().pending.push((at, h));
        }
    }

    fn check_result(&self, at: usize, e: &JValue, got: &Result<JValue, Fail>) {
        let why = match (got, js::has(e, "throws")) {
            (Ok(v), false) => {
                let (want, got) = (text(e.get("result")), text(v));
                (want != got).then(|| format!("result: node {want} / rust {got}"))
            }
            (Err(f), true) => {
                let (want, got) = (
                    text(&without_backend(e.get("throws"))),
                    text(&error_json(f)),
                );
                (want != got).then(|| format!("throws: node {want} / rust {got}"))
            }
            (Ok(v), true) => Some(format!(
                "node threw {} / rust answered {}",
                text(e.get("throws")),
                text(v)
            )),
            (Err(f), false) => Some(format!(
                "node answered {} / rust threw {}",
                text(e.get("result")),
                f.message
            )),
        };
        if let Some(why) = why {
            let mut c = self.cur.lock().unwrap();
            self.fail(&mut c, format!("event {at} {}: {why}", label(e)));
        }
    }
}

/// `config.readMemory`: one value for every read, or (`"calls"`) each read recorded as an event.
enum Memory {
    Constant(Option<f64>),
    Calls,
}

fn head_ms(e: &JValue) -> f64 {
    e.get("ms").as_num().unwrap_or(0.0)
}

/// An async dependency's answer; past a mismatch it never settles (the replay has failed).
async fn later(e: Option<JValue>) -> Result<JValue, Fail> {
    if e.is_none() {
        std::future::pending::<()>().await;
    }
    answer(e)
}

/// A dependency answer: `result`, or the recorded throw.
fn answer(e: Option<JValue>) -> Result<JValue, Fail> {
    match e {
        None => Err(Fail::new("replay mismatch")),
        Some(e) if js::has(&e, "throws") => Err(fail_of(e.get("throws"))),
        Some(e) => Ok(js::json_copy(e.get("result"))),
    }
}

#[derive(Clone)]
struct ReplayEngine(Arc<Replay>);
#[derive(Clone)]
struct ReplayHooks(Arc<Replay>);

struct ReplayBody {
    r: Arc<Replay>,
    id: usize,
    ok: bool,
    status: f64,
    body: bool,
}

impl Body for ReplayBody {
    fn ok(&self) -> bool {
        self.ok
    }
    fn status(&self) -> f64 {
        self.status
    }
    fn has_body(&self) -> bool {
        self.body
    }
    fn read(&mut self) -> impl Future<Output = Result<Option<Vec<u8>>, Fail>> + Send {
        let e = self
            .r
            .take("stream.read", &[("stream", JValue::Num(self.id as f64))]);
        async move {
            let v = later(e).await?;
            if v.get("done").truthy() {
                return Ok(None);
            }
            Ok(Some(
                v.get("text").as_str().unwrap_or("").as_bytes().to_vec(),
            ))
        }
    }
}

fn call_args(call: &Call, abort: &Option<Abort>) -> Vec<(&'static str, JValue)> {
    vec![
        ("path", JValue::from(call.path.as_str())),
        ("method", JValue::from(call.method)),
        ("body", call.body.clone().unwrap_or(JValue::Null)),
        ("timeoutMs", JValue::Num(call.timeout_ms as f64)),
        ("signal", JValue::Bool(abort.is_some())),
    ]
}

impl Engine for ReplayEngine {
    type Body = ReplayBody;
    fn raw_models(
        &self,
        _abort: Option<Abort>,
    ) -> impl Future<Output = Result<JValue, Fail>> + Send {
        let e = self.0.take("rawModels", &[]);
        async move { later(e).await }
    }
    fn request(
        &self,
        call: Call,
        abort: Option<Abort>,
    ) -> impl Future<Output = Result<JValue, Fail>> + Send {
        let e = self.0.take("request", &call_args(&call, &abort));
        async move { later(e).await }
    }
    fn stream(
        &self,
        path: &str,
        body: JValue,
        _abort: Abort,
    ) -> impl Future<Output = Result<ReplayBody, Fail>> + Send {
        let id = {
            let mut c = self.0.cur.lock().unwrap();
            c.streams += 1;
            c.streams - 1
        };
        let e = self.0.take(
            "stream",
            &[
                ("path", JValue::from(path)),
                ("method", JValue::from("POST")),
                ("body", body),
            ],
        );
        let r = Arc::clone(&self.0);
        async move {
            let v = later(e).await?;
            Ok(ReplayBody {
                r,
                id,
                ok: v.get("ok").truthy(),
                status: v.get("status").as_num().unwrap_or(0.0),
                body: v.get("body").truthy(),
            })
        }
    }
    fn presets_get(&self, model: &str) -> JValue {
        answer(
            self.0
                .take("presets.get", &[("model", JValue::from(model))]),
        )
        .unwrap_or(JValue::Null)
    }
    fn presets_snapshot(&self) -> (String, String) {
        let v = answer(self.0.take("presets.snapshot", &[])).unwrap_or(JValue::Null);
        (
            v.get("text").as_str().unwrap_or("").to_string(),
            v.get("revision").as_str().unwrap_or("").to_string(),
        )
    }
    fn presets_commit(&self, candidate: JValue) -> impl Future<Output = Result<(), Fail>> + Send {
        let e = self.0.take("presets.commit", &[("candidate", candidate)]);
        async move { later(e).await.map(|_| ()) }
    }
    fn invalidate_baseline(&self, tag: &str) -> impl Future<Output = Result<bool, Fail>> + Send {
        let e = self
            .0
            .take("invalidateBaseline", &[("tag", JValue::from(tag))]);
        async move { later(e).await.map(|v| v.truthy()) }
    }
    fn conservative_for(&self, model: &str) -> impl Future<Output = Result<JValue, Fail>> + Send {
        let e = self
            .0
            .take("conservativeFor", &[("model", JValue::from(model))]);
        async move { later(e).await }
    }
    fn identity_for(&self, model: &str) -> impl Future<Output = Result<JValue, Fail>> + Send {
        let e = self
            .0
            .take("identityFor", &[("model", JValue::from(model))]);
        async move { later(e).await }
    }
    fn held(&self) -> bool {
        answer(self.0.take("held", &[])).is_ok_and(|v| v.truthy())
    }
    fn read_memory(&self) -> Option<f64> {
        match self.0.memory {
            Memory::Constant(v) => v,
            Memory::Calls => answer(self.0.take("readMemory", &[]))
                .ok()
                .and_then(|v| v.as_num()),
        }
    }
    fn now(&self) -> f64 {
        self.0.cur.lock().unwrap().clock
    }
    fn sleep(&self, ms: f64) -> impl Future<Output = ()> + Send {
        let e = self.0.take("sleep", &[("ms", JValue::Num(ms))]);
        async move {
            let _ = later(e).await;
        }
    }
    fn tick(&self, _ms: f64) -> impl Future<Output = ()> + Send {
        std::future::pending()
    }
    fn random_int(&self, min: u32, max: u32) -> u32 {
        let v = answer(self.0.take(
            "randomInt",
            &[
                ("min", JValue::Num(f64::from(min))),
                ("max", JValue::Num(f64::from(max))),
            ],
        ));
        v.ok().and_then(|v| v.as_num()).unwrap_or(0.0) as u32
    }
    fn random_uuid(&self) -> String {
        let v = answer(self.0.take("randomUUID", &[]));
        v.ok()
            .and_then(|v| v.as_str().map(str::to_string))
            .unwrap_or_default()
    }
}

impl Hooks for ReplayHooks {
    fn context_current(&self) -> bool {
        answer(self.0.take("hook contextCurrent", &[])).is_ok_and(|v| v.truthy())
    }
    fn apply_unlocked(&self, body: JValue) -> impl Future<Output = Result<JValue, Fail>> + Send {
        let e = self.0.take("hook applyUnlocked", &[("arg", body)]);
        async move { later(e).await }
    }
    fn foreign_held(&self, rows: JValue) -> impl Future<Output = Result<Vec<JValue>, Fail>> + Send {
        let e = self.0.take("hook foreignHeld", &[("arg", rows)]);
        async move { later(e).await.map(|v| js::items(&v).to_vec()) }
    }
    fn foreign_wait(&self, legacy: Fail) -> impl Future<Output = Result<(), Fail>> + Send {
        let e = self
            .0
            .take("hook foreignWait", &[("arg", error_json(&legacy))]);
        async move { later(e).await.map(|_| ()) }
    }
    fn on_write(&self, revision: &str) {
        let _ = self
            .0
            .take("hook onWrite", &[("arg", JValue::from(revision))]);
    }
    fn on_update(&self, c: &JValue) {
        let _ = self.0.take("hook onUpdate", &[("arg", c.clone())]);
    }
}

/// Replays one trace; `Err` names the first difference.
fn replay(path: &std::path::Path) -> Result<(), String> {
    let raw = std::fs::read_to_string(path).unwrap();
    let t = wtf16::parse(&raw).map_err(|e| format!("parse: {e:?}"))?;
    let strings: Vec<String> = js::items(t.get("strings")).iter().map(string_of).collect();
    let events = resolve(t.get("events"), &strings);
    let mut flat = Vec::new();
    flatten(js::items(&events), &mut flat);
    let config = t.get("config");
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(6)
        .enable_all()
        .build()
        .unwrap();
    let replay = Arc::new(Replay {
        events: flat,
        cur: Mutex::new(Cursor {
            pos: 0,
            mismatch: None,
            clock: T0,
            streams: 0,
            pending: Vec::new(),
        }),
        changed: Condvar::new(),
        ctx: OnceLock::new(),
        memory: match config.get("readMemory") {
            JValue::Str(s) if s == "calls" => Memory::Calls,
            v => Memory::Constant(v.as_num()),
        },
        rt: rt.handle().clone(),
    });
    let fill = t.get("kind").as_str() == Some("fill");
    let floor = config.get("memoryFloorGib").as_num().unwrap_or(2.0);
    let limits = Limits::from_timeouts(config.get("timeouts"));
    // The calibrator is made on the runtime (its creation reads the models.ini revision).
    let ctx = {
        let (e, h) = (
            ReplayEngine(Arc::clone(&replay)),
            ReplayHooks(Arc::clone(&replay)),
        );
        rt.block_on(async move { PrivateCalibrator::new(e, h, fill, floor, limits) })
    };
    let _ = replay.ctx.set(ctx);
    let pump = {
        let r = Arc::clone(&replay);
        std::thread::spawn(move || r.pump())
    };
    let _ = pump.join();
    // The runner's awaited calls settle; their answers are compared.
    let pending = std::mem::take(&mut replay.cur.lock().unwrap().pending);
    for (at, h) in pending {
        if replay.cur.lock().unwrap().mismatch.is_some() {
            h.abort();
            continue;
        }
        let got = rt.block_on(async { tokio::time::timeout(Duration::from_secs(10), h).await });
        match got {
            Ok(Ok(r)) => replay.check_result(at, &replay.events[at].v.clone(), &r),
            Ok(Err(e)) => return Err(format!("event {at}: the call panicked: {e}")),
            Err(_) => {
                let mut c = replay.cur.lock().unwrap();
                let why = format!("event {at} {}: never settled", label(&replay.events[at].v));
                replay.fail(&mut c, why);
            }
        }
    }
    let c = replay.cur.lock().unwrap();
    let outcome = if let Some(m) = &c.mismatch {
        Err(m.clone())
    } else if c.pos < replay.events.len() {
        Err(format!(
            "stopped at event {} ({}) of {}",
            c.pos,
            label(&replay.events[c.pos].v),
            replay.events.len()
        ))
    } else {
        Ok(())
    };
    drop(c);
    rt.shutdown_background();
    outcome
}

/// Traces that differ from Node on purpose (or where Node's behaviour cannot be derived from the
/// trace), by file name, each with its reason. A listed trace must still differ, so a stale entry
/// is noticed; every other trace must replay exactly.
const NAMED_DIFFERENCES: &[(&str, &str)] = &[];

#[test]
fn every_recorded_private_calibrator_replays() {
    let mut names: Vec<_> = std::fs::read_dir(dir())
        .unwrap()
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|x| x == "json"))
        .collect();
    names.sort();
    let only = std::env::var("TRACE").ok();
    let (mut passed, mut named, mut failed) = (0, 0, Vec::new());
    let mut scenarios = 0;
    for p in &names {
        let name = p.file_name().unwrap().to_string_lossy().to_string();
        if only.as_deref().is_some_and(|o| !name.contains(o)) {
            continue;
        }
        scenarios += usize::from(name.starts_with('S'));
        let known = NAMED_DIFFERENCES.iter().find(|(n, _)| *n == name);
        match (replay(p), known) {
            (Ok(()), None) => passed += 1,
            (Err(why), None) => failed.push(format!("{name}: {why}")),
            (Err(why), Some((_, reason))) => {
                named += 1;
                println!("NAMED {name}: {reason} (differs: {why})");
            }
            (Ok(()), Some(_)) => failed.push(format!(
                "{name}: listed in NAMED_DIFFERENCES but replays exactly"
            )),
        }
    }
    println!(
        "replayed {passed} of {} traces ({scenarios} scenario traces; {named} named differences)",
        passed + named + failed.len()
    );
    for f in &failed {
        println!("FAIL {f}");
    }
    assert!(
        names.len() >= 300 || only.is_some(),
        "the trace set is missing"
    );
    assert!(
        scenarios >= 30 || only.is_some(),
        "the scenario (S*) traces are missing"
    );
    assert!(
        failed.is_empty(),
        "{} of {} traces differ from Node",
        failed.len(),
        passed + named + failed.len()
    );
}
