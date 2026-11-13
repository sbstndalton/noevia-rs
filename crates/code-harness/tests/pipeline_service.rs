//! The Planner preparation through the Rust Code service: core's code-service.cjs with a real
//! createCodePipeline over its own job store and approval cards, as
//! tests/fixtures/pipeline-service-node.v1.json recorded it (tools/gen-pipeline-service-fixtures.cjs),
//! replayed on [`Service`] with crate code-pipeline's driver over pipeline_host's job store
//! (lifecycle authority, inference lease) and the service's own cards, and the same scripted
//! stand-ins for the harness, workspaces, role session and verifier (no git, sandbox or model).
//! Every answer, each task's journal and the log lines must match: the preparation list and the
//! start's answer with the flag on and off, in the sandbox or not, a pipeline that cannot be built,
//! the plan card and the accept card answered through decide, a cancel on the plan card, the
//! one-task rule, the maintenance pause and a refused lease.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]

use code_harness::executor::{Fail, Lease};
use code_harness::service::{
    default_harnesses, Inference, Options, Parts, Project, Repo, Resolved, Service, Setup,
};
use code_pipeline::driver::{self, Connect, JobCtx, Pin, RunHandle, Signal};
use js_json::{wtf16, JValue};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const FIXTURE: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/pipeline-service-node.v1.json"
);

fn text(v: &JValue) -> String {
    wtf16::stringify(v).unwrap()
}

fn s(t: &str) -> JValue {
    JValue::Str(t.into())
}

type Logs = Arc<Mutex<Vec<JValue>>>;

struct Ws(JValue);

impl driver::Workspaces for Ws {
    fn project_snapshot(&self, _: &str) -> Result<JValue, driver::Fail> {
        Ok(self.0.get("snapshot").clone())
    }
    fn change(&self, _: &str) -> Result<JValue, driver::Fail> {
        Ok(self.0.get("change").clone())
    }
    fn branch_tip(&self, _: &str) -> Result<JValue, driver::Fail> {
        Ok(self.0.get("head").clone())
    }
    fn merge_preflight(&self, _: &str, _: &str) -> Result<JValue, driver::Fail> {
        Ok(JValue::obj([("ok", JValue::Bool(true)), ("baseBranch", s("main"))]))
    }
    fn merge_verified(&self, _: &str, _: &str) -> Result<JValue, driver::Fail> {
        self.merge_preflight("", "")
    }
}

struct Harness {
    standins: JValue,
    handles: Mutex<Vec<JValue>>,
}

impl driver::Harness for Harness {
    fn claim_run(&self, _: &JValue) -> Result<RunHandle, driver::Fail> {
        let mut h = self.handles.lock().unwrap();
        h.push(JValue::obj([
            ("workspace", self.standins.get("workspace").clone()),
            ("grant", JValue::Null),
            ("cleanupConfirmed", JValue::Bool(true)),
        ]));
        Ok(RunHandle(h.len() as u64))
    }
    fn handle(&self, h: RunHandle) -> JValue {
        self.handles.lock().unwrap()[h.0 as usize - 1].clone()
    }
    fn run_executor(
        &self,
        job: &JobCtx,
        h: RunHandle,
        opts: &JValue,
        _: &Connect,
    ) -> Result<(), driver::Fail> {
        let w = self.standins.get("workspace");
        let head = self.standins.get("head").clone();
        job.checkpoint(JValue::obj([
            ("branch", w.get("branch").clone()),
            ("task", s(&wtf16::slice(opts.get("label").as_str().unwrap(), 120))),
            ("baseSha", w.get("baseSha").clone()),
            ("headSha", head.clone()),
        ]))?;
        let mut handles = self.handles.lock().unwrap();
        if let JValue::Obj(items) = &mut handles[h.0 as usize - 1] {
            items.push((
                "released".into(),
                JValue::obj([("status", s("released")), ("headSha", head)]),
            ));
        }
        Ok(())
    }
    fn release_run(&self, _: RunHandle) {}
}

struct Roles(JValue);
struct Session(JValue);

impl driver::Roles for Roles {
    fn pin_model(&self, _: &JValue, _: &Signal) -> Result<Pin, driver::Fail> {
        Ok(Pin::Pinned {
            model: s("qa-model"),
            machine: JValue::Undefined,
            session: Arc::new(Session(self.0.clone())),
        })
    }
}

impl driver::Session for Session {
    fn call(&self, req: &JValue, _: &Signal) -> Result<JValue, driver::Fail> {
        let answer = self.0.get(req.get("role").as_str().unwrap());
        Ok(JValue::obj([
            ("ok", JValue::Bool(true)),
            ("text", s(&text(answer))),
            ("corrected", JValue::Bool(false)),
        ]))
    }
}

struct Verify;

impl driver::Verify for Verify {
    fn available(&self) -> bool {
        true
    }
    fn run(
        &self,
        a: &JValue,
        emit: &mut dyn FnMut(&str, JValue) -> Result<(), driver::Fail>,
        _: &Signal,
    ) -> Result<JValue, driver::Fail> {
        let report = JValue::obj([
            ("kind", s("test-report")),
            ("passed", JValue::Bool(true)),
            ("exitCode", JValue::Num(0.0)),
            ("signal", JValue::Null),
            ("timedOut", JValue::Bool(false)),
            ("tail", s("QA ok")),
            ("tailBytes", JValue::Num(5.0)),
            ("totalBytes", JValue::Num(5.0)),
            ("truncated", JValue::Bool(false)),
            ("taskId", a.get("taskId").clone()),
            ("headSha", a.get("headSha").clone()),
            ("revision", a.get("revision").clone()),
            ("repo", a.get("repo").clone()),
            ("durationMs", JValue::Num(7.0)),
        ]);
        emit(
            "step.started",
            JValue::obj([("id", s("tests")), ("title", s("Run the tests"))]),
        )?;
        emit("artifact.created", report.clone())?;
        emit(
            "step.completed",
            JValue::obj([("id", s("tests")), ("failed", JValue::Bool(false))]),
        )?;
        Ok(JValue::obj([
            ("status", s("passed")),
            ("report", report),
            ("error", JValue::Null),
        ]))
    }
}

struct Host(Logs);

impl driver::Host for Host {
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
        Some("qa-engine-key".into())
    }
    fn log(&self, entry: JValue) {
        self.0.lock().unwrap().push(entry);
    }
    fn now(&self) -> f64 {
        5000.0
    }
}

struct QaSetup {
    enabled: bool,
    create: bool,
    standins: JValue,
    logs: Logs,
}

impl Setup for QaSetup {
    fn enabled(&self) -> bool {
        self.enabled
    }
    fn create(&self, parts: Parts, _: &Resolved) -> Option<driver::Deps> {
        let st = &self.standins;
        self.create.then(|| {
            driver::Deps::new(
                parts.jobs,
                Arc::new(Ws(st.clone())),
                Arc::new(Harness {
                    standins: st.clone(),
                    handles: Mutex::new(Vec::new()),
                }),
                Some(Arc::new(Roles(st.get("answers").clone()))),
                Some(Arc::new(Verify)),
                parts.approvals,
                Arc::new(Host(self.logs.clone())),
            )
        })
    }
}

struct Gate(String);

impl Inference for Gate {
    fn enter(&self) -> Result<Lease, Fail> {
        if self.0 == "refuses" {
            return Err(Fail::new(Some(503), "QA gate held"));
        }
        Ok(Box::new(()) as Lease)
    }
    fn paused(&self) -> bool {
        self.0 == "paused"
    }
}

/// routes/code.cjs's answer.
fn route(status: f64, r: Result<JValue, Fail>) -> JValue {
    match r {
        Ok(body) => JValue::obj([("status", JValue::Num(status)), ("body", body)]),
        Err(e) => {
            let message = match (&e.public_message, e.status) {
                (Some(m), _) if !m.is_empty() => m.clone(),
                (_, Some(_)) => e.message.clone(),
                _ => "Code request failed".into(),
            };
            JValue::obj([
                ("status", JValue::Num(f64::from(e.status.unwrap_or(500)))),
                ("body", JValue::obj([("error", s(&message))])),
            ])
        }
    }
}

fn journal(dir: &str, task: &str) -> Option<Vec<JValue>> {
    let text = std::fs::read_to_string(format!("{dir}/jobs/{task}.jsonl")).ok()?;
    Some(
        text.lines()
            .filter(|l| !l.is_empty())
            .map(|l| wtf16::parse(l).unwrap())
            .collect(),
    )
}

fn ended(dir: &str, task: &str) -> bool {
    journal(dir, task).is_some_and(|rows| {
        rows.iter().any(|r| {
            matches!(
                r.get("type").as_str(),
                Some("job.completed" | "job.failed" | "job.cancelled" | "job.interrupted")
            )
        })
    })
}

#[test]
fn the_fixture_was_recorded_from_these_sources() {
    let fixture = wtf16::parse(&std::fs::read_to_string(FIXTURE).unwrap()).unwrap();
    let sources = fixture.get("sources");
    let pinned = [
        ("code-service.cjs", code_harness::pipeline_host::CODE_SERVICE_SHA256),
        ("code-pipeline.cjs", code_harness::pipeline_host::CODE_PIPELINE_SHA256),
        ("jobs.cjs", code_pipeline::driver::JOBS_SHA256),
    ];
    for (file, sha) in pinned {
        assert_eq!(sources.get(file).as_str(), Some(sha), "{file}");
    }
}

#[test]
fn the_planner_preparation_answers_as_core_recorded_it() {
    let fixture = wtf16::parse(&std::fs::read_to_string(FIXTURE).unwrap()).unwrap();
    let standins = fixture.get("standins").clone();
    let JValue::Obj(scenarios) = fixture.get("scenarios") else {
        panic!("no scenarios")
    };
    assert!(scenarios.len() >= 10);
    for (name, sc) in scenarios {
        let base = tempfile::tempdir().unwrap();
        let dir = base.path().join("tenant");
        std::fs::create_dir_all(&dir).unwrap();
        let dir = dir.to_str().unwrap().to_string();
        let logs: Logs = Arc::new(Mutex::new(Vec::new()));
        let ids = Arc::new(AtomicUsize::new(0));
        let service_logs = logs.clone();
        let service = Service::new(Options {
            repos: vec![Repo {
                id: "qa-repo".into(),
                path: "/qa/repos/qa-repo".into(),
            }],
            harnesses: default_harnesses(Some("opencode"), Some("0.0.0-qa")),
            sandboxed: sc.get("sandboxKind").as_str() == Some("sandbox"),
            transport: None,
            tree_root: None,
            harness_user: None,
            egress: None,
            engine: Arc::new(code_harness::executor::Engine::default),
            inference: sc
                .get("inference")
                .as_str()
                .map(|g| Arc::new(Gate(g.into())) as Arc<dyn Inference>),
            guard: Arc::new(|| false),
            log: Arc::new(move |e| service_logs.lock().unwrap().push(e)),
            now: Arc::new(|| 5000.0),
            timeout: Duration::from_secs(60),
            base_path: None,
            base_lang: None,
            new_id: Arc::new(move || {
                let n = ids.fetch_add(1, Ordering::SeqCst) + 1;
                format!("00000000-0000-4000-8000-{n:012}")
            }),
            epoch: "qa-epoch".into(),
            pipeline: Some(Arc::new(QaSetup {
                enabled: sc.get("enabled").as_bool() == Some(true),
                create: sc.get("noCreate").as_bool() != Some(true),
                standins: standins.clone(),
                logs: logs.clone(),
            })),
        });
        let record = JValue::obj([("id", s("qa-project"))]);
        let project = Project {
            id: "qa-project",
            record: &record,
        };
        let resolve = || {
            Ok(Resolved {
                context: "QA shared context.".into(),
                ..Resolved::default()
            })
        };
        let (mut tasks, mut cards): (Vec<String>, Vec<String>) = (Vec::new(), Vec::new());
        let sub = |v: &JValue, tasks: &[String], cards: &[String]| -> String {
            let v = v.as_str().unwrap();
            let pick = |list: &[String], n: &str| {
                list.get(n.parse::<usize>().unwrap() - 1)
                    .cloned()
                    .unwrap_or_else(|| "none".into())
            };
            match v.strip_prefix("<T").or(v.strip_prefix("<A")) {
                Some(rest) if v.starts_with("<T") => pick(tasks, rest.trim_end_matches('>')),
                Some(rest) => pick(cards, rest.trim_end_matches('>')),
                None => v.to_string(),
            }
        };
        let JValue::Arr(ops) = sc.get("ops") else {
            panic!("{name}: no ops")
        };
        let mut answers = Vec::new();
        for op in ops {
            let answer = if op.get("start").is_object() {
                let a = route(
                    202.0,
                    service.start(&dir, Some("qa-user"), &project, &resolve, op.get("start")),
                );
                if let Some(t) = a.get("body").get("taskId").as_str() {
                    if !tasks.iter().any(|x| x == t) {
                        tasks.push(t.to_string());
                    }
                }
                a
            } else if let JValue::Arr(d) = op.get("decide") {
                let (t, decision, card) = (
                    sub(&d[0], &tasks, &cards),
                    sub(&d[1], &tasks, &cards),
                    sub(&d[2], &tasks, &cards),
                );
                route(
                    200.0,
                    service.decide(&dir, "qa-project", &t, &decision, &card),
                )
            } else if op.get("cancel").as_str().is_some() {
                let t = sub(op.get("cancel"), &tasks, &cards);
                route(200.0, service.cancel(&dir, "qa-project", &t))
            } else if op.get("facts").truthy() {
                route(200.0, service.facts(&dir))
            } else if op.get("waitCard").as_str().is_some() {
                let t = sub(op.get("waitCard"), &tasks, &cards);
                let deadline = Instant::now() + Duration::from_secs(10);
                let mut card = JValue::Null;
                while Instant::now() < deadline {
                    let c = service.facts(&dir).unwrap().get("approvals").get(&t).clone();
                    if c.is_object() && !cards.iter().any(|k| Some(k.as_str()) == c.get("id").as_str()) {
                        card = c;
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(10));
                }
                if let Some(id) = card.get("id").as_str() {
                    cards.push(id.to_string());
                }
                JValue::obj([(
                    "waited",
                    if card.is_object() { card.get("action").clone() } else { s("none") },
                )])
            } else if op.get("waitEnd").as_str().is_some() {
                let t = sub(op.get("waitEnd"), &tasks, &cards);
                let deadline = Instant::now() + Duration::from_secs(10);
                while !ended(&dir, &t) && Instant::now() < deadline {
                    std::thread::sleep(Duration::from_millis(10));
                }
                std::thread::sleep(Duration::from_millis(50));
                JValue::obj([("waited", s(if ended(&dir, &t) { "ended" } else { "running" }))])
            } else {
                panic!("{name}: unknown op {}", text(op))
            };
            answers.push(answer);
        }
        let JValue::Arr(want) = sc.get("answers") else {
            panic!("{name}: no answers")
        };
        for (i, (got, want)) in answers.iter().zip(want).enumerate() {
            assert_eq!(text(got), text(want), "{name}: answer {i} to {}", text(&ops[i]));
        }
        assert_eq!(answers.len(), want.len(), "{name}");
        for t in &tasks {
            let got = journal(&dir, t).map_or(JValue::Null, JValue::Arr);
            assert_eq!(text(&got), text(sc.get("journals").get(t)), "{name}: journal of {t}");
        }
        let got = JValue::Arr(logs.lock().unwrap().clone());
        assert_eq!(text(&got), text(sc.get("logs")), "{name}: logs");
    }
}
