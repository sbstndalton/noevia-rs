//! Every scenario of tests/fixtures/code-pipeline-driver.v1.json, recorded from Node's
//! createCodePipeline by tools/gen-driver-fixtures.cjs, replayed through `Pipeline::start` with a
//! stand-in for every dependency that answers as Node's stub did. Each call the port makes is
//! checked against the next recorded one (name and arguments, as JSON text in the fixture's
//! tagged form), and the start result too; the job's journal follows from the `jobs.append`
//! calls, which include jobs.run's `job.started` and final event.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use code_pipeline::driver::{JOBS_SHA256, PLAN_CONSTRAINED_DECODING_SHA256, ROLE_ENGINE_SHA256};
use code_pipeline::sessions::{DOSSIER_ROLES, PLAN_ARTIFACT_SCHEMA, SHARED_FRAME, VERDICT_SCHEMA};
use code_pipeline::*;
use js_json::{wtf16, JValue};
use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::Duration;

const TAG: &str = "$js";

fn fixture() -> JValue {
    wtf16::parse(include_str!("fixtures/code-pipeline-driver.v1.json")).expect("fixture JSON")
}

fn s(t: &str) -> JValue {
    JValue::Str(t.into())
}

fn decode(v: &JValue) -> JValue {
    match v {
        JValue::Arr(items) => JValue::Arr(items.iter().map(decode).collect()),
        JValue::Obj(items) => {
            if let Some(tag) = v.get(TAG).as_str() {
                return match tag {
                    "undefined" => JValue::Undefined,
                    "NaN" => JValue::Num(f64::NAN),
                    "Infinity" => JValue::Num(f64::INFINITY),
                    "-Infinity" => JValue::Num(f64::NEG_INFINITY),
                    "-0" => JValue::Num(-0.0),
                    other => panic!("unknown tag {other}"),
                };
            }
            JValue::Obj(items.iter().map(|(k, x)| (k.clone(), decode(x))).collect())
        }
        other => other.clone(),
    }
}

fn encode(v: &JValue) -> JValue {
    let tag = |t: &str| JValue::obj([(TAG, s(t))]);
    match v {
        JValue::Undefined => tag("undefined"),
        JValue::Num(n) if n.is_nan() => tag("NaN"),
        JValue::Num(n) if *n == f64::INFINITY => tag("Infinity"),
        JValue::Num(n) if *n == f64::NEG_INFINITY => tag("-Infinity"),
        JValue::Num(n) if *n == 0.0 && n.is_sign_negative() => tag("-0"),
        JValue::Arr(items) => JValue::Arr(items.iter().map(encode).collect()),
        JValue::Obj(items) => {
            JValue::Obj(items.iter().map(|(k, x)| (k.clone(), encode(x))).collect())
        }
        other => other.clone(),
    }
}

fn text(v: &JValue) -> String {
    wtf16::stringify(v).unwrap_or_else(|| "<undefined>".into())
}

fn arr(items: Vec<JValue>) -> JValue {
    JValue::Arr(items)
}

type Emit<'a> = &'a mut dyn FnMut(&str, JValue) -> Result<(), Fail>;

/// Answers every dependency as the recorded stub did, checking each call on the way.
struct Replay {
    expected: Mutex<VecDeque<JValue>>,
    seen: Mutex<usize>,
    mismatch: Mutex<Option<String>>,
    handles: Mutex<HashMap<u64, JValue>>,
    last: Mutex<u64>,
    cancel: Signal,
}

impl Replay {
    fn fail(&self, why: String) {
        let mut m = self.mismatch.lock().unwrap();
        if m.is_none() {
            *m = Some(why);
        }
    }

    fn next(&self, name: &str, args: JValue) -> JValue {
        let exp = self.expected.lock().unwrap().pop_front();
        let i = {
            let mut n = self.seen.lock().unwrap();
            *n += 1;
            *n
        };
        let Some(exp) = exp else {
            self.fail(format!("extra call #{i} {name} {}", text(&encode(&args))));
            return JValue::obj([("throw", JValue::obj([("message", s("replay: extra call"))]))]);
        };
        let call = exp.get("call").as_str().unwrap_or("");
        let (want, got) = (text(exp.get("args")), text(&encode(&args)));
        if call != name || want != got {
            self.fail(format!(
                "call #{i}: expected {call} {want}\n          got {name} {got}"
            ));
            return JValue::obj([("throw", JValue::obj([("message", s("replay: mismatch"))]))]);
        }
        decode(exp.get("beh"))
    }

    fn set(&self, beh: &JValue, handle: Option<u64>) {
        if let JValue::Obj(items) = beh.get("set") {
            let id = handle.unwrap_or(*self.last.lock().unwrap());
            let mut hs = self.handles.lock().unwrap();
            let h = hs.entry(id).or_insert(JValue::Obj(Vec::new()));
            if let JValue::Obj(fields) = h {
                for (k, v) in items {
                    wtf16::set(fields, k, v.clone());
                }
            }
        }
    }

    /// cancel, checkpoint, wait, emit, set (Node's stub order).
    fn act(
        &self,
        beh: &JValue,
        signal: Option<&Signal>,
        ctx: Option<&JobCtx>,
        emit: Option<Emit>,
        handle: Option<u64>,
    ) -> Result<(), Fail> {
        if beh.get("cancel") == &JValue::Bool(true) {
            self.cancel.abort();
        }
        if beh.get("checkpoint").truthy() {
            if let Some(c) = ctx {
                c.checkpoint(beh.get("checkpoint").clone())?;
            }
        }
        if beh.get("wait") == &JValue::Bool(true) {
            let fired = signal.is_some_and(|sg| sg.wait(Duration::from_secs(10)));
            if !fired {
                self.fail("a waiting stub's signal never fired".into());
            }
        }
        if let (JValue::Arr(list), Some(emit)) = (beh.get("emit"), emit) {
            for e in list {
                emit(e.at(0).as_str().unwrap_or(""), e.at(1))?;
            }
        }
        self.set(beh, handle);
        Ok(())
    }

    fn settle(beh: &JValue) -> Result<JValue, Fail> {
        let t = beh.get("throw");
        if t.truthy() {
            return Err(Fail {
                message: t.get("message").as_str().unwrap_or("").into(),
                public_message: t.get("publicMessage").as_str().map(str::to_string),
                status: None,
                result: t.get("result").clone(),
            });
        }
        Ok(beh.get("ret").clone())
    }

    fn sync(&self, name: &str, args: JValue) -> Result<JValue, Fail> {
        let beh = self.next(name, args);
        self.set(&beh, None);
        Self::settle(&beh)
    }
}

/// An array element (`undefined` past the end).
trait Index {
    fn at(&self, i: usize) -> JValue;
}
impl Index for JValue {
    fn at(&self, i: usize) -> JValue {
        match self {
            JValue::Arr(a) => a.get(i).cloned().unwrap_or(JValue::Undefined),
            _ => JValue::Undefined,
        }
    }
}

struct R(Arc<Replay>);

impl Jobs for R {
    fn create(&self, req: &JValue) -> Result<String, Fail> {
        Ok(self
            .0
            .sync("jobs.create", arr(vec![req.clone()]))?
            .as_str()
            .unwrap_or("")
            .into())
    }
    fn append(&self, id: &str, kind: &str, data: JValue) -> Result<(), Fail> {
        self.0
            .sync("jobs.append", arr(vec![s(id), s(kind), data]))
            .map(|_| ())
    }
    fn append_authority(&self, id: &str, kind: &str, data: JValue) -> Result<(), Fail> {
        self.0
            .sync("jobs.appendAuthority", arr(vec![s(id), s(kind), data]))
            .map(|_| ())
    }
    fn get(&self, id: &str) -> Result<JValue, Fail> {
        self.0.sync("jobs.get", arr(vec![s(id)]))
    }
}

impl Workspaces for R {
    fn project_snapshot(&self, id: &str) -> Result<JValue, Fail> {
        self.0.sync("workspaces.projectSnapshot", arr(vec![s(id)]))
    }
    fn change(&self, id: &str) -> Result<JValue, Fail> {
        self.0.sync("workspaces.change", arr(vec![s(id)]))
    }
    fn branch_tip(&self, id: &str) -> Result<JValue, Fail> {
        self.0.sync("workspaces.branchTip", arr(vec![s(id)]))
    }
    fn merge_preflight(&self, id: &str, head: &str) -> Result<JValue, Fail> {
        let o = JValue::obj([("headSha", s(head))]);
        self.0
            .sync("workspaces.mergePreflight", arr(vec![s(id), o]))
    }
    fn merge_verified(&self, id: &str, head: &str) -> Result<JValue, Fail> {
        let o = JValue::obj([("headSha", s(head))]);
        self.0.sync("workspaces.mergeVerified", arr(vec![s(id), o]))
    }
}

impl Harness for R {
    fn claim_run(&self, req: &JValue) -> Result<RunHandle, Fail> {
        let beh = self.0.next("harness.claimRun", arr(vec![req.clone()]));
        self.0.act(&beh, None, None, None, None)?;
        let ret = Replay::settle(&beh)?;
        let id = ret.as_num().unwrap_or(0.0) as u64;
        self.0
            .handles
            .lock()
            .unwrap()
            .insert(id, beh.get("handle").clone());
        *self.0.last.lock().unwrap() = id;
        Ok(RunHandle(id))
    }
    fn handle(&self, h: RunHandle) -> JValue {
        self.0
            .handles
            .lock()
            .unwrap()
            .get(&h.0)
            .cloned()
            .unwrap_or(JValue::Undefined)
    }
    fn run_executor(
        &self,
        job: &JobCtx,
        h: RunHandle,
        opts: &JValue,
        _connect: &Connect,
        signal: &Signal,
    ) -> Result<(), Fail> {
        let args = arr(vec![JValue::Num(h.0 as f64), opts.clone()]);
        let beh = self.0.next("harness.runExecutor", args);
        self.0.act(&beh, Some(signal), Some(job), None, Some(h.0))?;
        Replay::settle(&beh).map(|_| ())
    }
    fn release_run(&self, h: RunHandle) {
        let beh = self
            .0
            .next("harness.releaseRun", arr(vec![JValue::Num(h.0 as f64)]));
        let _ = self.0.act(&beh, None, None, None, None);
    }
}

struct RSession(Arc<Replay>);

impl Session for RSession {
    fn call(&self, req: &JValue, signal: &Signal) -> Result<JValue, Fail> {
        let beh = self.0.next("session.call", req.clone());
        self.0.act(&beh, Some(signal), None, None, None)?;
        Replay::settle(&beh)
    }
}

impl Roles for R {
    fn pin_model(&self, req: &JValue, signal: &Signal) -> Result<Pin, Fail> {
        let beh = self.0.next("roles.pinModel", arr(vec![req.clone()]));
        self.0.act(&beh, Some(signal), None, None, None)?;
        let r = Replay::settle(&beh)?;
        Ok(if r.get("ok").truthy() {
            Pin::Pinned {
                model: r.get("session").get("model").clone(),
                machine: r.get("session").get("machine").clone(),
                session: Arc::new(RSession(Arc::clone(&self.0))),
            }
        } else {
            Pin::Refused {
                reason: r.get("reason").clone(),
            }
        })
    }
}

impl Verify for R {
    fn available(&self) -> bool {
        self.0.sync("verify.available", arr(vec![])).ok() == Some(JValue::Bool(true))
    }
    fn run(
        &self,
        req: &JValue,
        emit: &mut dyn FnMut(&str, JValue) -> Result<(), Fail>,
        signal: &Signal,
    ) -> Result<JValue, Fail> {
        let beh = self.0.next("verify.run", arr(vec![req.clone()]));
        self.0.act(&beh, Some(signal), None, Some(emit), None)?;
        Replay::settle(&beh)
    }
}

impl Approvals for R {
    fn ask(&self, request: &JValue, signal: &Signal) -> Result<JValue, Fail> {
        let beh = self.0.next("askApproval", arr(vec![request.clone()]));
        self.0.act(&beh, Some(signal), None, None, None)?;
        Replay::settle(&beh)
    }
}

impl Host for R {
    fn merge(&self) -> bool {
        self.0.sync("flag.merge", arr(vec![])).ok() == Some(JValue::Bool(true))
    }
    fn constrain(&self) -> bool {
        self.0.sync("flag.constrain", arr(vec![])).ok() == Some(JValue::Bool(true))
    }
    fn guard(&self) -> bool {
        self.0.sync("flag.guard", arr(vec![])).ok() == Some(JValue::Bool(true))
    }
    fn plan_constrain(&self) -> bool {
        self.0.sync("flag.planConstrain", arr(vec![])).ok() == Some(JValue::Bool(true))
    }
    fn engine_key(&self) -> Option<String> {
        let r = self.0.sync("engineKey", arr(vec![])).ok()?;
        r.as_str().map(str::to_string)
    }
    fn log(&self, entry: JValue) {
        let _ = self.0.sync("log", arr(vec![entry]));
    }
    fn now(&self) -> f64 {
        let r = self.0.sync("now", arr(vec![]));
        r.ok().and_then(|v| v.as_num()).unwrap_or(0.0)
    }
}

fn strings(v: &JValue) -> Vec<String> {
    match v {
        JValue::Arr(a) => a
            .iter()
            .filter_map(|x| x.as_str().map(str::to_string))
            .collect(),
        _ => Vec::new(),
    }
}

fn opt_str(v: &JValue) -> Option<String> {
    v.as_str().map(str::to_string)
}

/// Runs one scenario; `Err` says how the port differed.
fn replay(sc: &JValue) -> Result<(), String> {
    let calls = match sc.get("calls") {
        JValue::Arr(c) => c.clone(),
        _ => Vec::new(),
    };
    let cancel = Signal::new();
    let r = Arc::new(Replay {
        expected: Mutex::new(calls.into()),
        seen: Mutex::new(0),
        mismatch: Mutex::new(None),
        handles: Mutex::new(HashMap::new()),
        last: Mutex::new(0),
        cancel: cancel.clone(),
    });
    let o = decode(sc.get("options"));
    let mut deps = Deps::new(
        Arc::new(R(Arc::clone(&r))),
        Arc::new(R(Arc::clone(&r))),
        Arc::new(R(Arc::clone(&r))),
        if o.get("noRoles") == &JValue::Bool(true) {
            None
        } else {
            Some(Arc::new(R(Arc::clone(&r))))
        },
        Some(Arc::new(R(Arc::clone(&r)))),
        Arc::new(R(Arc::clone(&r))),
        Arc::new(R(Arc::clone(&r))),
    );
    let dl = o.get("deadlines");
    let ms = |k: &str, d: u64| dl.get(k).as_num().map_or(d, |x| x as u64);
    deps.deadlines = Deadlines {
        plan: ms("plan", DEADLINES.plan),
        implement: ms("implement", DEADLINES.implement),
        verify: ms("verify", DEADLINES.verify),
        review: ms("review", DEADLINES.review),
        audit: ms("audit", DEADLINES.audit),
    };
    if let Some(m) = o.get("maxLoops").as_num() {
        deps.max_loops = m as u32;
    }
    if let Some(a) = o.get("auditWriteUp").as_bool() {
        deps.audit_write_up = a;
    }
    if let Some(m) = o.get("planDeadlineMs").as_num() {
        deps.plan_deadline_ms = m as u64;
    }
    if let Some(m) = o.get("reviewDeadlineMs").as_num() {
        deps.review_deadline_ms = m as u64;
    }
    let t = decode(sc.get("task"));
    let task = Task {
        project_id: opt_str(t.get("projectId")),
        repo_id: t.get("repo").get("id").as_str().unwrap_or("").into(),
        repo_path: t.get("repo").get("path").as_str().unwrap_or("").into(),
        prompt: t.get("prompt").as_str().unwrap_or("").into(),
        capabilities: strings(t.get("capabilities")),
        domains: strings(t.get("domains")),
        harness: opt_str(t.get("harness")),
        connect: (o.get("connect") == &JValue::Bool(true)).then(|| Arc::new(()) as Connect),
        model: opt_str(t.get("model")),
        sandbox_kind: opt_str(t.get("sandboxKind")),
        context: t.get("context").as_str().unwrap_or("").into(),
        tenant_id: opt_str(t.get("tenantId")),
        approve_plan: t.get("approvePlan") == &JValue::Bool(true),
    };
    let pipeline = Pipeline::new(deps);
    let started = pipeline.start(task, cancel);
    let got = match &started {
        Ok(st) => JValue::obj([(
            "ret",
            JValue::obj([
                ("taskId", s(&st.task_id)),
                ("branch", st.branch.clone()),
                ("workspace", st.workspace.clone()),
            ]),
        )]),
        Err(f) => {
            let mut e = vec![("message", s(&f.message))];
            if let Some(p) = &f.public_message {
                e.push(("publicMessage", s(p)));
            }
            if f.result != JValue::Undefined {
                e.push(("result", f.result.clone()));
            }
            if let Some(st) = f.status {
                e.push(("status", JValue::Num(f64::from(st))));
            }
            JValue::obj([("throw", JValue::obj(e))])
        }
    };
    if let Ok(st) = started {
        if let Some(th) = st.thread {
            th.join()
                .map_err(|_| "the pipeline thread panicked".to_string())?;
        }
    }
    if let Some(m) = r.mismatch.lock().unwrap().take() {
        return Err(m);
    }
    if text(&encode(&got)) != text(sc.get("start")) {
        return Err(format!(
            "start: expected {}\n       got {}",
            text(sc.get("start")),
            text(&encode(&got))
        ));
    }
    let left = r.expected.lock().unwrap().len();
    if left > 0 {
        let next = r
            .expected
            .lock()
            .unwrap()
            .front()
            .map(text)
            .unwrap_or_default();
        return Err(format!("{left} recorded calls never made; next {next}"));
    }
    Ok(())
}

#[test]
fn every_scenario_replays() {
    let f = fixture();
    let JValue::Arr(scenarios) = f.get("scenarios") else {
        panic!("no scenarios")
    };
    assert!(scenarios.len() >= 70, "{} scenarios", scenarios.len());
    let mut failures = Vec::new();
    for sc in scenarios {
        if let Err(e) = replay(sc) {
            failures.push(format!("{}: {e}", sc.get("name").as_str().unwrap_or("?")));
        }
    }
    assert!(failures.is_empty(), "\n{}", failures.join("\n\n"));
}

#[test]
fn sources_and_constants_match() {
    let f = fixture();
    let src = f.get("source");
    for (file, sha) in [
        ("code-pipeline.cjs", CODE_PIPELINE_SHA256),
        ("planner-plan.cjs", PLANNER_PLAN_SHA256),
        ("code-review.cjs", CODE_REVIEW_SHA256),
        ("auditor-report.cjs", AUDITOR_REPORT_SHA256),
        ("role-context.cjs", ROLE_CONTEXT_SHA256),
        ("code-review-verdict.cjs", CODE_REVIEW_VERDICT_SHA256),
        ("jobs.cjs", JOBS_SHA256),
        ("role-engine.cjs", ROLE_ENGINE_SHA256),
        (
            "plan-constrained-decoding.cjs",
            PLAN_CONSTRAINED_DECODING_SHA256,
        ),
    ] {
        assert_eq!(src.get(file).as_str(), Some(sha), "{file}");
    }
    let c = f.get("constants");
    assert_eq!(c.get("SHARED_FRAME").as_str(), Some(SHARED_FRAME));
    assert_eq!(
        c.get("PLAN_ARTIFACT_SCHEMA").as_str(),
        Some(PLAN_ARTIFACT_SCHEMA)
    );
    assert_eq!(c.get("VERDICT_SCHEMA").as_str(), Some(VERDICT_SCHEMA));
    let roles: Vec<String> = strings(c.get("DOSSIER_ROLES"));
    assert_eq!(roles, DOSSIER_ROLES.map(str::to_string));
}

#[test]
fn a_stage_deadline_fires_its_signal_and_a_cancel_reaches_it() {
    let job = Signal::new();
    let d = Deadline::start(&job, 20);
    assert!(d.signal.wait(Duration::from_secs(5)));
    assert!(d.timed_out());
    let d = Deadline::start(&job, 60_000);
    job.abort();
    assert!(d.signal.wait(Duration::from_secs(5)));
    assert!(!d.timed_out());
}
