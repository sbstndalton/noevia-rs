//! pipeline_host's adapters over the real pieces: a Planner task through the [`Service`] with the
//! real job store and lifecycle authority, the real worktree (a QA git repository), the real
//! executor (no harness transport, so the agent's turn stops where Node's would: "No coding
//! harness …") and the real cards; the role session is a stand-in and the verifier is absent.
//! Also the role engine adapter over crates/role-engine with stub dependencies, and the job
//! store's refusal of lifecycle rows without the authority.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]

use code_harness::pipeline_host::{EngineRoles, Flag, PipeHost, PipeVerify};
use code_harness::service::{
    default_harnesses, Options, Parts, Project, Repo, Resolved, Service, Setup,
};
use code_pipeline::driver::{self, Pin, Roles, Signal};
use js_json::{wtf16, JValue};
use std::collections::HashMap;
use std::path::Path;
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

fn s(t: &str) -> JValue {
    JValue::Str(t.into())
}

fn git(cwd: &Path, args: &[&str]) {
    let ok = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .env("HOME", "/nonexistent")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_AUTHOR_NAME", "QA")
        .env("GIT_AUTHOR_EMAIL", "qa@example.invalid")
        .env("GIT_COMMITTER_NAME", "QA")
        .env("GIT_COMMITTER_EMAIL", "qa@example.invalid")
        .status()
        .unwrap()
        .success();
    assert!(ok, "git {args:?}");
}

const PLAN: &str = r#"{"goal":"Add the QA greeting","context":[],"constraints":[],"investigation":[],"steps":[{"n":1,"do":"Edit qa.txt","done_when":"It greets"}],"capabilities":["read_repository","edit_file"],"approval_boundaries":[],"verification":["Run the QA tests."],"completion":"It greets.","non_goals":[]}"#;

struct QaRoles;
struct QaSession;

impl driver::Roles for QaRoles {
    fn pin_model(&self, _: &JValue, _: &Signal) -> Result<Pin, driver::Fail> {
        Ok(Pin::Pinned {
            model: s("qa-model"),
            machine: JValue::Null,
            session: Arc::new(QaSession),
        })
    }
}

impl driver::Session for QaSession {
    fn call(&self, _: &JValue, _: &Signal) -> Result<JValue, driver::Fail> {
        Ok(JValue::obj([
            ("ok", JValue::Bool(true)),
            ("text", s(PLAN)),
            ("corrected", JValue::Bool(false)),
        ]))
    }
}

/// The real adapters the service hands over; only the roles and the verifier are this test's.
struct Real;

impl Setup for Real {
    fn enabled(&self) -> bool {
        true
    }
    fn create(&self, parts: Parts, _: &Resolved) -> Option<driver::Deps> {
        Some(driver::Deps::new(
            parts.jobs,
            parts.workspaces,
            parts.harness,
            Some(Arc::new(QaRoles)),
            Some(Arc::new(PipeVerify {
                adapter: Arc::new(code_verify::Adapter::new(None)),
                workspaces: parts.tree,
            })),
            parts.approvals,
            Arc::new(PipeHost {
                flag: Arc::new(|f| f == Flag::Guard),
                engine_key: None,
                log: Arc::new(|_| {}),
                now: Arc::new(|| 5000.0),
            }),
        ))
    }
}

struct Fixture {
    _base: tempfile::TempDir,
    dir: String,
    service: Arc<Service>,
}

fn fixture() -> Fixture {
    let base = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(base.path()).unwrap();
    let repo = root.join("qa-repo");
    std::fs::create_dir_all(&repo).unwrap();
    git(&repo, &["init", "-q", "--template=", "-b", "main"]);
    std::fs::write(repo.join("qa.txt"), "qa\n").unwrap();
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-qm", "QA first"]);
    let dir = root.join("tenant");
    std::fs::create_dir_all(&dir).unwrap();
    let ids = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let service = Service::new(Options {
        repos: vec![Repo {
            id: "qa-repo".into(),
            path: repo.to_str().unwrap().into(),
        }],
        harnesses: default_harnesses(None, None),
        sandboxed: true,
        transport: None,
        tree_root: None,
        harness_user: None,
        egress: None,
        engine: Arc::new(code_harness::executor::Engine::default),
        inference: None,
        guard: Arc::new(|| false),
        log: Arc::new(|_| {}),
        now: Arc::new(|| 5000.0),
        timeout: Duration::from_secs(60),
        base_path: std::env::var("PATH").ok(),
        base_lang: None,
        new_id: Arc::new(move || {
            let n = ids.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
            format!("00000000-0000-4000-8000-{n:012}")
        }),
        epoch: "qa-epoch".into(),
        pipeline: Some(Arc::new(Real)),
    });
    Fixture {
        _base: base,
        dir: dir.to_str().unwrap().into(),
        service,
    }
}

fn start(f: &Fixture, body: &str) -> JValue {
    let record = JValue::obj([("id", s("qa-project"))]);
    let project = Project {
        id: "qa-project",
        record: &record,
    };
    let resolve = || Ok(Resolved::default());
    f.service
        .start(
            &f.dir,
            Some("qa-user"),
            &project,
            &resolve,
            &wtf16::parse(body).unwrap(),
        )
        .unwrap()
}

fn rows(f: &Fixture, task: &str) -> Vec<JValue> {
    std::fs::read_to_string(format!("{}/jobs/{task}.jsonl", f.dir))
        .unwrap_or_default()
        .lines()
        .map(|l| wtf16::parse(l).unwrap())
        .collect()
}

fn wait_end(f: &Fixture, task: &str) -> Vec<JValue> {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let all = rows(f, task);
        let done = all.iter().any(|r| {
            matches!(
                r.get("type").as_str(),
                Some("job.completed" | "job.failed" | "job.cancelled")
            )
        });
        if done || Instant::now() > deadline {
            std::thread::sleep(Duration::from_millis(50));
            return rows(f, task);
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn stages(rows: &[JValue]) -> Vec<String> {
    rows.iter()
        .filter(|r| r.get("type").as_str() == Some("task.stage"))
        .map(|r| r.get("data").get("to").as_str().unwrap().to_string())
        .collect()
}

fn workspace_status(f: &Fixture, task: &str) -> String {
    let record = std::fs::read_to_string(format!("{}/code-workspaces/{task}.json", f.dir))
        .unwrap_or_default();
    wtf16::parse(&record)
        .map(|r| r.get("status").as_str().unwrap_or("").to_string())
        .unwrap_or_default()
}

#[test]
fn a_planner_task_runs_through_the_real_store_worktree_and_executor() {
    let f = fixture();
    let facts = f.service.facts(&f.dir).unwrap();
    let JValue::Arr(offered) = facts.get("promptPreparation") else {
        panic!()
    };
    assert_eq!(offered.last().unwrap().get("id").as_str(), Some("planner"));
    let started = start(
        &f,
        r#"{"repository":"qa-repo","prompt":"QA: greet.","promptPreparation":"planner"}"#,
    );
    let task = started.get("taskId").as_str().unwrap().to_string();
    assert_eq!(started.get("promptPreparation").as_str(), Some("planner"));
    assert!(started.get("branch").as_str().unwrap().contains(&task[..8]));
    let rows = wait_end(&f, &task);
    assert_eq!(stages(&rows), ["planned", "implementing", "blocked"]);
    // The Planner read the real repository's file list.
    let pinned = rows.iter().find(|r| {
        r.get("type").as_str() == Some("step.started")
            && r.get("data").get("id").as_str() == Some("harness.config")
    });
    assert!(pinned.is_some(), "the executor's turn began");
    let last = rows.last().unwrap();
    assert_eq!(last.get("type").as_str(), Some("job.failed"));
    let error = last.get("data").get("error").as_str().unwrap();
    assert!(error.starts_with("The coding agent stopped: "), "{error}");
    // The executor gave its claim back.
    assert_ne!(workspace_status(&f, &task), "held");
    // The lifecycle rows are the authority's only: a plain append is refused.
    let jobs = code_harness::pipeline_host::PipeJobs {
        store: Arc::new(server_projects::task_store::Store::new(
            Path::new(&f.dir),
            server_projects::task_store::Options::default(),
        )),
        authority: Arc::new(
            server_projects::task_store::Store::new(
                &Path::new(&f.dir).join("other"),
                server_projects::task_store::Options::default(),
            )
            .claim_lifecycle_authority()
            .unwrap(),
        ),
        live: Arc::new(Mutex::new(HashMap::new())),
        start: Arc::new(code_harness::pipeline_host::StartState {
            signal: Signal::new(),
            phase: Mutex::new(code_harness::pipeline_host::Phase::Queued),
            task: Mutex::new(None),
            gate: None,
            lease: Mutex::new(None),
            log: Arc::new(|_| {}),
            now: Arc::new(|| 5000.0),
        }),
    };
    let refused = driver::Jobs::append(&jobs, &task, "task.stage", JValue::Obj(Vec::new()));
    assert_eq!(refused.unwrap_err().status, Some(403));
    // An authority another store issued is no authority here.
    let other = driver::Jobs::append_authority(&jobs, &task, "task.stage", JValue::Obj(Vec::new()));
    assert!(other.is_err());
    let job = driver::Jobs::get(&jobs, &task).unwrap();
    assert_eq!(job.get("status").as_str(), Some("failed"));
    assert_eq!(job.get("revision"), &JValue::Null);
    assert!(matches!(job.get("stages"), JValue::Arr(a) if a.len() == 3));
}

#[test]
fn a_cancel_on_the_plan_card_stops_the_pipeline_and_gives_the_worktree_back() {
    let f = fixture();
    let started = start(
        &f,
        r#"{"repository":"qa-repo","prompt":"QA: greet.","promptPreparation":"planner","approvePlan":true}"#,
    );
    let task = started.get("taskId").as_str().unwrap().to_string();
    let deadline = Instant::now() + Duration::from_secs(20);
    let card = loop {
        let c = f
            .service
            .facts(&f.dir)
            .unwrap()
            .get("approvals")
            .get(&task)
            .clone();
        if c.is_object() || Instant::now() > deadline {
            break c;
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    assert_eq!(card.get("action").as_str(), Some("approve_plan"));
    assert_eq!(workspace_status(&f, &task), "held");
    let view = f.service.cancel(&f.dir, "qa-project", &task).unwrap();
    assert_eq!(view.get("status").as_str(), Some("waiting_approval"));
    let rows = wait_end(&f, &task);
    assert_eq!(stages(&rows), ["planned", "blocked"]);
    let decided = rows
        .iter()
        .find(|r| r.get("type").as_str() == Some("approval.decided"))
        .unwrap();
    assert_eq!(
        decided.get("data").get("decision").as_str(),
        Some("aborted")
    );
    assert_eq!(
        rows.last().unwrap().get("type").as_str(),
        Some("job.cancelled")
    );
    assert_ne!(workspace_status(&f, &task), "held");
    let late = f
        .service
        .decide(
            &f.dir,
            "qa-project",
            &task,
            "approve",
            card.get("id").as_str().unwrap(),
        )
        .unwrap_err();
    assert_eq!(late.status, Some(409));
}

// ── The role engine adapter ──

struct NoBody;

impl role_engine::Body for NoBody {
    async fn read(&mut self) -> Result<Option<Vec<u8>>, role_engine::TransportError> {
        Ok(None)
    }
    fn cancel(&mut self) {}
}

struct QaDeps {
    external: bool,
}

impl role_engine::Deps for QaDeps {
    type Body = NoBody;
    fn engine(&self) -> Option<role_engine::Endpoint> {
        Some(role_engine::Endpoint {
            base_url: Some("http://qa-engine.invalid:8080/v1".into()),
            model: Some("qa-model".into()),
            external: self.external,
            ..role_engine::Endpoint::default()
        })
    }
    async fn fetch(
        &self,
        _: role_engine::Request,
        _: role_engine::Cancel,
    ) -> Result<role_engine::Response<NoBody>, role_engine::TransportError> {
        Err(role_engine::TransportError {
            detail: "QA: no engine here".into(),
        })
    }
    async fn keep(&self) -> Option<Vec<String>> {
        Some(Vec::new())
    }
    async fn loaded_models(&self) -> Option<Result<JValue, ()>> {
        Some(Ok(JValue::Arr(vec![s("qa-model")])))
    }
    fn system_roles(&self) -> HashMap<String, String> {
        HashMap::new()
    }
    fn now(&self) -> f64 {
        5000.0
    }
}

fn roles(external: bool) -> EngineRoles<QaDeps> {
    EngineRoles {
        engine: Arc::new(role_engine::RoleEngine::new(QaDeps { external })),
        blocking: Arc::new(role_engine::Blocking::new().unwrap()),
    }
}

#[test]
fn the_role_engine_adapter_pins_refuses_and_calls() {
    let req = JValue::obj([
        ("taskId", s("qa-task")),
        ("model", JValue::Null),
        ("thinking", JValue::Bool(false)),
        ("roles", JValue::Arr(vec![s("planner"), s("reviewer")])),
    ]);
    match roles(true).pin_model(&req, &Signal::new()).unwrap() {
        Pin::Refused { reason } => assert_eq!(
            reason.as_str(),
            Some("The task pipeline runs only on a local model.")
        ),
        Pin::Pinned { .. } => panic!("an external engine is never pinned"),
    }
    let Pin::Pinned {
        model,
        machine,
        session,
    } = roles(false).pin_model(&req, &Signal::new()).unwrap()
    else {
        panic!("pinned")
    };
    assert_eq!(
        (model.as_str(), &machine),
        (Some("qa-model"), &JValue::Null)
    );
    let call = JValue::obj([
        ("role", s("auditor")),
        ("state", JValue::obj([("taskId", s("qa-task"))])),
        ("instructions", s("QA")),
        ("schema", JValue::obj([("type", s("object"))])),
    ]);
    let answer = session.call(&call, &Signal::new()).unwrap();
    assert_eq!(answer.get("ok"), &JValue::Bool(false));
    assert_eq!(answer.get("code").as_str(), Some("role_not_pinned"));
    let call = JValue::obj([
        ("role", s("planner")),
        (
            "state",
            JValue::obj([
                ("taskId", s("qa-task")),
                ("tenantId", s("qa-tenant")),
                ("request", s("QA: greet.")),
                ("revision", JValue::Num(0.0)),
            ]),
        ),
        ("instructions", s("QA")),
        ("schema", JValue::obj([("type", s("object"))])),
    ]);
    let answer = session.call(&call, &Signal::new()).unwrap();
    assert_eq!(answer.get("ok"), &JValue::Bool(false));
    assert_eq!(answer.get("code").as_str(), Some("error"));
    // A cancelled signal reaches the call as the engine's own cancel.
    let gone = Signal::new();
    gone.abort();
    let answer = session.call(&call, &gone).unwrap();
    assert_eq!(answer.get("code").as_str(), Some("aborted"));
}
