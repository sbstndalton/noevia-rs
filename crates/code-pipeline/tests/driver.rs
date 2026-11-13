//! Every scenario of tests/fixtures/code-pipeline-driver.v1.json, recorded from Node's
//! createCodePipeline by tools/gen-driver-fixtures.cjs, replayed through `Pipeline::start` with a
//! stand-in for every dependency that answers as Node's stub did. Each call the port makes is
//! checked against the next recorded one (name and arguments, as JSON text in the fixture's
//! tagged form), and the start result too. The appends the store took (jobs.run's own
//! `job.started` and terminal rows among them, recorded from the store's writes) must make the
//! recorded journal, and the job's status, error, result, revision and stages must follow from
//! them as they do in the recorded job. Below the replay, stand-in tests for what Node cannot
//! show: a panicking host, a harness reaching for the lifecycle authority, the stage signal, and
//! the network list's cap.
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
    /// The appends the store took, in order (`job.created` first).
    journal: Mutex<Vec<(String, JValue)>>,
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

    fn appended(&self, kind: &str, data: JValue) {
        self.journal.lock().unwrap().push((kind.into(), data));
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
        let id = self.0.sync("jobs.create", arr(vec![req.clone()]))?;
        self.0.appended("job.created", JValue::Undefined);
        Ok(id.as_str().unwrap_or("").into())
    }
    fn append(&self, id: &str, kind: &str, data: JValue) -> Result<(), Fail> {
        if kind == "task.stage" || kind == "task.revision" {
            self.0.fail(format!("{kind} reached the plain append"));
        }
        self.0
            .sync("jobs.append", arr(vec![s(id), s(kind), data.clone()]))?;
        self.0.appended(kind, data);
        Ok(())
    }
    fn append_authority(&self, id: &str, kind: &str, data: JValue) -> Result<(), Fail> {
        self.0.sync(
            "jobs.appendAuthority",
            arr(vec![s(id), s(kind), data.clone()]),
        )?;
        self.0.appended(kind, data);
        Ok(())
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
    ) -> Result<(), Fail> {
        let args = arr(vec![JValue::Num(h.0 as f64), opts.clone()]);
        let beh = self.0.next("harness.runExecutor", args);
        self.0
            .act(&beh, Some(job.signal()), Some(job), None, Some(h.0))?;
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
        journal: Mutex::new(Vec::new()),
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
    let journal = r.journal.lock().unwrap().clone();
    check_journal(sc, &journal)
}

/// The kinds the store rebuilds (or makes) before writing: only their type is compared.
const REBUILT: [&str; 6] = [
    "job.created",
    "task.stage",
    "task.revision",
    "review.requested",
    "review.completed",
    "review.failed",
];

/// The appends the port made against the recorded journal, and the recorded job's terminal and
/// lifecycle fields against what those appends fold to (jobs.cjs `foldEvent`).
fn check_journal(sc: &JValue, got: &[(String, JValue)]) -> Result<(), String> {
    let want = match sc.get("journal") {
        JValue::Arr(rows) => rows.clone(),
        JValue::Undefined => {
            return if got.is_empty() {
                Ok(())
            } else {
                Err(format!("{} appends, but Node made no job", got.len()))
            }
        }
        _ => return Err("journal is not a list".into()),
    };
    if want.len() != got.len() {
        let kinds: Vec<&str> = got.iter().map(|(k, _)| k.as_str()).collect();
        return Err(format!(
            "journal: {} rows recorded, {} appended: {kinds:?}",
            want.len(),
            got.len()
        ));
    }
    for (i, (row, (kind, data))) in want.iter().zip(got).enumerate() {
        if row.get("type").as_str() != Some(kind) {
            return Err(format!(
                "journal row {i}: {} recorded, {kind} appended",
                text(row.get("type"))
            ));
        }
        if !REBUILT.contains(&kind.as_str()) && text(row.get("data")) != text(data) {
            return Err(format!(
                "journal row {i} ({kind}): {} recorded\n          {} appended",
                text(row.get("data")),
                text(data)
            ));
        }
    }
    let (mut status, mut result, mut error) = (s("queued"), JValue::Null, JValue::Null);
    let (mut revision, mut stages) = (None, Vec::new());
    for (kind, data) in got {
        let or_null = |v: &JValue| {
            if v.is_nullish() {
                JValue::Null
            } else {
                v.clone()
            }
        };
        match kind.as_str() {
            "job.started" => status = s("running"),
            "job.completed" | "job.cancelled" => {
                status = s(&kind[4..]);
                result = or_null(data.get("result"));
            }
            "job.failed" => {
                status = s("failed");
                error = if data.get("error").is_nullish() {
                    s("failed")
                } else {
                    data.get("error").clone()
                };
                result = or_null(data.get("result"));
            }
            "task.revision" => {
                revision = Some((data.get("n").clone(), data.get("headSha").clone()))
            }
            "task.stage" => stages.push(data.get("to").clone()),
            _ => {}
        }
    }
    let job = decode(sc.get("job"));
    for (field, folded) in [("status", &status), ("result", &result), ("error", &error)] {
        if text(job.get(field)) != text(folded) {
            return Err(format!(
                "job.{field}: {} recorded, {} from the appends",
                text(job.get(field)),
                text(folded)
            ));
        }
    }
    let recorded: Vec<String> = match job.get("stages") {
        JValue::Arr(st) => st.iter().map(|x| text(x.get("to"))).collect(),
        _ => Vec::new(),
    };
    let folded: Vec<String> = stages.iter().map(text).collect();
    if recorded != folded {
        return Err(format!(
            "job.stages: {recorded:?} recorded, {folded:?} appended"
        ));
    }
    if let Some((n, head)) = revision {
        let rev = job.get("revision");
        if text(rev.get("n")) != text(&n) || text(rev.get("headSha")) != text(&head) {
            return Err(format!("job.revision: {} recorded", text(rev)));
        }
    }
    Ok(())
}

#[test]
fn every_scenario_replays() {
    let f = fixture();
    let JValue::Arr(scenarios) = f.get("scenarios") else {
        panic!("no scenarios")
    };
    assert!(scenarios.len() >= 95, "{} scenarios", scenarios.len());
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

// ── Stand-ins for what Node cannot show ─────────────────────────────────────────────────────

const PLAN: &str = r#"{"goal":"Add the QA greeting","context":["The QA project."],"constraints":["Keep it small."],"investigation":["Read src/qa.js."],"steps":[{"n":1,"do":"Edit src/qa.js","done_when":"The QA greeting prints"}],"capabilities":["read_repository","edit_file"],"approval_boundaries":["Every write asks."],"verification":["Run the QA tests."],"completion":"The QA greeting prints.","non_goals":["No other QA files."]}"#;

type Exec = fn(&JobCtx) -> Result<(), Fail>;

/// A plain host: every append is taken, the tests always fail, and the executor does `exec`.
struct Plain {
    journal: Mutex<Vec<(String, JValue)>>,
    released: Mutex<Vec<u64>>,
    handles: Mutex<HashMap<u64, JValue>>,
    panic_pin: bool,
    exec: Exec,
}

impl Plain {
    fn new(panic_pin: bool, exec: Exec) -> Arc<Plain> {
        Arc::new(Plain {
            journal: Mutex::new(Vec::new()),
            released: Mutex::new(Vec::new()),
            handles: Mutex::new(HashMap::new()),
            panic_pin,
            exec,
        })
    }
    fn last(&self) -> (String, JValue) {
        self.journal.lock().unwrap().last().cloned().unwrap()
    }
}

struct P(Arc<Plain>);

impl Jobs for P {
    fn create(&self, _: &JValue) -> Result<String, Fail> {
        Ok("00000000-0000-4000-8000-0000000000aa".into())
    }
    fn append(&self, _: &str, kind: &str, data: JValue) -> Result<(), Fail> {
        assert!(kind != "task.stage" && kind != "task.revision");
        self.0.journal.lock().unwrap().push((kind.into(), data));
        Ok(())
    }
    fn append_authority(&self, _: &str, kind: &str, data: JValue) -> Result<(), Fail> {
        self.0.journal.lock().unwrap().push((kind.into(), data));
        Ok(())
    }
    fn get(&self, _: &str) -> Result<JValue, Fail> {
        Ok(JValue::obj([("status", s("running"))]))
    }
}

impl Workspaces for P {
    fn project_snapshot(&self, _: &str) -> Result<JValue, Fail> {
        Ok(JValue::obj([
            ("files", arr(vec![s("README.md")])),
            ("readme", JValue::Null),
        ]))
    }
    fn change(&self, _: &str) -> Result<JValue, Fail> {
        Ok(JValue::Null)
    }
    fn branch_tip(&self, _: &str) -> Result<JValue, Fail> {
        Ok(JValue::Null)
    }
    fn merge_preflight(&self, _: &str, _: &str) -> Result<JValue, Fail> {
        Ok(JValue::Null)
    }
    fn merge_verified(&self, _: &str, _: &str) -> Result<JValue, Fail> {
        Ok(JValue::Null)
    }
}

impl Harness for P {
    fn claim_run(&self, _: &JValue) -> Result<RunHandle, Fail> {
        let mut hs = self.0.handles.lock().unwrap();
        let id = hs.len() as u64 + 1;
        let ws = JValue::obj([
            ("branch", s("noevia/qa-plain")),
            ("path", s("/qa/work/plain")),
            ("baseSha", s(&"b".repeat(40))),
        ]);
        hs.insert(
            id,
            JValue::obj([("workspace", ws), ("cleanupConfirmed", JValue::Bool(true))]),
        );
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
        _: &JValue,
        _: &Connect,
    ) -> Result<(), Fail> {
        (self.0.exec)(job)?;
        let head = format!("{:040x}", h.0);
        let mut hs = self.0.handles.lock().unwrap();
        if let Some(JValue::Obj(fields)) = hs.get_mut(&h.0) {
            let released = JValue::obj([("status", s("released")), ("headSha", s(&head))]);
            wtf16::set(fields, "released", released);
            let network = JValue::obj([("allowed", JValue::Num(h.0 as f64))]);
            wtf16::set(fields, "network", network);
        }
        Ok(())
    }
    fn release_run(&self, h: RunHandle) {
        self.0.released.lock().unwrap().push(h.0);
    }
}

struct PSession;

impl Session for PSession {
    fn call(&self, _: &JValue, _: &Signal) -> Result<JValue, Fail> {
        Ok(JValue::obj([
            ("ok", JValue::Bool(true)),
            ("text", s(PLAN)),
            ("corrected", JValue::Bool(false)),
        ]))
    }
}

impl Roles for P {
    fn pin_model(&self, _: &JValue, _: &Signal) -> Result<Pin, Fail> {
        assert!(!self.0.panic_pin, "QA host panics in pinModel");
        Ok(Pin::Pinned {
            model: s("qa-model"),
            machine: JValue::Undefined,
            session: Arc::new(PSession),
        })
    }
}

impl Verify for P {
    fn available(&self) -> bool {
        true
    }
    fn run(
        &self,
        _: &JValue,
        _: &mut dyn FnMut(&str, JValue) -> Result<(), Fail>,
        _: &Signal,
    ) -> Result<JValue, Fail> {
        Ok(JValue::obj([
            ("status", s("failed")),
            ("report", JValue::obj([("exitCode", JValue::Num(1.0))])),
        ]))
    }
}

impl Approvals for P {
    fn ask(&self, _: &JValue, _: &Signal) -> Result<JValue, Fail> {
        Ok(s("deny"))
    }
}

impl Host for P {
    fn merge(&self) -> bool {
        false
    }
    fn constrain(&self) -> bool {
        false
    }
    fn guard(&self) -> bool {
        false
    }
    fn plan_constrain(&self) -> bool {
        false
    }
    fn engine_key(&self) -> Option<String> {
        None
    }
    fn log(&self, _: JValue) {}
    fn now(&self) -> f64 {
        0.0
    }
}

fn plain_run(p: &Arc<Plain>, tune: impl FnOnce(&mut Deps)) -> Signal {
    let mut deps = Deps::new(
        Arc::new(P(Arc::clone(p))),
        Arc::new(P(Arc::clone(p))),
        Arc::new(P(Arc::clone(p))),
        Some(Arc::new(P(Arc::clone(p)))),
        Some(Arc::new(P(Arc::clone(p)))),
        Arc::new(P(Arc::clone(p))),
        Arc::new(P(Arc::clone(p))),
    );
    tune(&mut deps);
    let task = Task {
        repo_id: "qa-repo".into(),
        repo_path: "/qa/repos/qa".into(),
        prompt: "Add the QA greeting.".into(),
        capabilities: vec!["read_repository".into(), "edit_file".into()],
        connect: Some(Arc::new(()) as Connect),
        project_id: Some("qa-project".into()),
        tenant_id: Some("qa-tenant".into()),
        ..Task::default()
    };
    let cancel = Signal::new();
    let started = Pipeline::new(deps).start(task, cancel.clone()).unwrap();
    started.thread.unwrap().join().unwrap();
    cancel
}

fn ok_exec(_: &JobCtx) -> Result<(), Fail> {
    Ok(())
}

#[test]
fn a_panicking_host_fails_the_job_and_gives_the_claim_back() {
    let p = Plain::new(true, ok_exec);
    plain_run(&p, |_| {});
    let (kind, data) = p.last();
    assert_eq!(kind, "job.failed");
    assert_eq!(
        text(&data),
        r#"{"error":"failed","result":null}"#,
        "the Executor's precedent"
    );
    assert_eq!(*p.released.lock().unwrap(), vec![1]);
}

fn grabs_authority(job: &JobCtx) -> Result<(), Fail> {
    let r = job.event("task.stage", JValue::obj([("to", s("merged"))]));
    let e = r.expect_err("refused");
    assert_eq!(e.status, Some(403));
    assert_eq!(e.message, "Task lifecycle events are server-only");
    job.event("task.revision", JValue::obj([("n", JValue::Num(9.0))]))
}

#[test]
fn a_harness_cannot_write_the_lifecycle() {
    let p = Plain::new(false, grabs_authority);
    plain_run(&p, |_| {});
    let journal = p.journal.lock().unwrap().clone();
    let stages: Vec<String> = journal
        .iter()
        .filter(|(k, _)| k == "task.stage")
        .map(|(_, d)| text(d.get("to")))
        .collect();
    assert_eq!(
        stages,
        [r#""planned""#, r#""implementing""#, r#""blocked""#]
    );
    assert!(!journal.iter().any(|(k, _)| k == "task.revision"));
    let (kind, data) = p.last();
    assert_eq!(kind, "job.failed");
    assert_eq!(
        data.get("error").as_str(),
        Some("The coding agent stopped: Task lifecycle events are server-only")
    );
}

fn waits_on_its_signal(job: &JobCtx) -> Result<(), Fail> {
    if job.signal().wait(Duration::from_secs(10)) {
        Err(Fail::new("aborted"))
    } else {
        Ok(())
    }
}

#[test]
fn the_executor_gets_the_stage_signal() {
    let p = Plain::new(false, waits_on_its_signal);
    let cancel = plain_run(&p, |d| d.deadlines.implement = 30);
    assert!(!cancel.is_aborted(), "the job itself was never cancelled");
    let (kind, data) = p.last();
    assert_eq!(kind, "job.failed");
    assert_eq!(
        data.get("error").as_str(),
        Some("The implement stage did not finish within 1 minute.")
    );
}

#[test]
fn the_network_list_keeps_the_last_32_rounds() {
    let p = Plain::new(false, ok_exec);
    plain_run(&p, |d| d.max_loops = 40);
    let (kind, data) = p.last();
    assert_eq!(kind, "job.failed");
    let JValue::Arr(net) = data.get("result").get("network") else {
        panic!("no network list: {}", text(&data))
    };
    assert_eq!(net.len(), 32);
    assert_eq!(net[0].get("revision").as_num(), Some(10.0));
    assert_eq!(net[31].get("revision").as_num(), Some(41.0));
    assert_eq!(net[31].get("allowed").as_num(), Some(41.0));
}

#[test]
fn a_deadline_past_the_timer_range_fires_at_once_as_in_node() {
    let job = Signal::new();
    let d = Deadline::start(&job, u64::MAX);
    assert!(d.signal.wait(Duration::from_secs(5)));
    assert!(d.timed_out());
    let d = Deadline::start(&job, 60_000);
    assert!(!d.finish(), "finished before it fired");
    assert!(!d.timed_out());
}
