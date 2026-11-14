//! crates/code-harness review gate: direct Code tasks with the Planner review on, run by core's own
//! code-harness.cjs `start` with code-review.cjs `createPlannerReview` over a scripted role engine
//! (tests/fixtures/review-gate-node.v1.json, tools/gen-review-gate-fixtures.cjs), replayed through
//! the Rust executor with the same fake sandbox and an equivalent role engine: the journal file, the
//! approval cards (the `review_change` card among them), the harness log, the reviewer's log, every
//! pin and call (with the bounded review state) and what the sandbox received must match.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]

use code_acp::Transport;
use code_harness::executor::StartRequest;
use code_harness::executor::{
    Approvals, Deps, Egress, Engine, Executor, Fail, Holding, Jobs, Lease,
};
use code_harness::review_gate::{self, PlannerReview, ReviewRoles, Reviewer};
use code_harness::session::Ended;
use code_pipeline::driver::{self, Signal};
use code_pipeline::RoleFail;
use code_workspace::{chown_tree, git::Git, Options as WsOptions, Workspaces};
use js_json::{wtf16, JValue};
use server_projects::task_store::{NewJob, Options as StoreOptions, Store};
use std::collections::VecDeque;
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::path::Path;
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

const FIXTURE: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/review-gate-node.v1.json"
);
const TASK: &str = "00000000-0000-4000-8000-0000000000bb";

type Log = Arc<Mutex<Vec<JValue>>>;

fn text(v: &JValue) -> String {
    wtf16::stringify(v).unwrap()
}

fn s(t: &str) -> JValue {
    JValue::Str(t.into())
}

fn git(cwd: &Path, args: &[&str]) {
    let ok = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .env_clear()
        .env("PATH", std::env::var("PATH").unwrap_or_default())
        .env("HOME", "/nonexistent")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_AUTHOR_NAME", "QA")
        .env("GIT_AUTHOR_EMAIL", "qa@example.invalid")
        .env("GIT_COMMITTER_NAME", "QA")
        .env("GIT_COMMITTER_EMAIL", "qa@example.invalid")
        .env("GIT_AUTHOR_DATE", "2026-01-01T00:00:00Z")
        .env("GIT_COMMITTER_DATE", "2026-01-01T00:00:00Z")
        .status()
        .unwrap()
        .success();
    assert!(ok, "git {args:?}");
}

struct StoreJobs(Store);

impl Jobs for StoreJobs {
    fn create(&self, project_id: Option<&str>, capabilities: &[String]) -> Result<String, Fail> {
        self.0
            .create(NewJob {
                kind: "code",
                project_id,
                parent_id: None,
                capabilities,
            })
            .map_err(|r| Fail::new(r.status, r.message))
    }
    fn append(&self, id: &str, kind: &str, data: JValue) -> Result<(), Fail> {
        self.0
            .append(id, kind, data)
            .map(|_| ())
            .map_err(|r| Fail::new(r.status, r.message))
    }
    fn status(&self, id: &str) -> Option<String> {
        let job = self.0.get(id).ok().flatten()?;
        job.status.as_str().map(str::to_string)
    }
    fn get(&self, id: &str) -> Option<JValue> {
        let job = self.0.get(id).ok().flatten()?;
        Some(code_harness::pipeline_host::job_value(&job))
    }
}

struct StubEgress(JValue);

impl Egress for StubEgress {
    fn endpoint(&self) -> Option<String> {
        Some("egress:3128".into())
    }
    fn grant(&self, _task_id: &str, _domains: &[String]) -> Result<JValue, Fail> {
        Ok(JValue::obj([
            ("token", s("qa-token")),
            ("id", s("qa-grant")),
        ]))
    }
    fn revoke(&self, _grant: &JValue) -> Result<JValue, Fail> {
        Ok(self.0.clone())
    }
    fn forget(&self, _grant: &JValue, _snapshot: &str) -> Result<JValue, Fail> {
        Ok(JValue::obj([("confirmed", JValue::Bool(true))]))
    }
}

struct Scripted {
    answers: Mutex<VecDeque<String>>,
    asked: Log,
    executor: Arc<OnceLock<Arc<Executor>>>,
    /// Council p14: where the lease, the pin and the card went, in order (`None`: not watched).
    order: Option<Log>,
}

impl Approvals for Scripted {
    fn ask(&self, request: &JValue, ended: &Arc<Ended>) -> String {
        self.asked.lock().unwrap().push(request.clone());
        if let Some(order) = &self.order {
            order.lock().unwrap().push(s("card"));
        }
        let next = self.answers.lock().unwrap().pop_front();
        let answer = next.unwrap_or_else(|| "deny".into());
        if answer != "cancel" {
            return answer;
        }
        // The person cancels the task while this card waits: it is refused as `aborted`.
        self.executor.get().unwrap().cancel(TASK).unwrap();
        let deadline = Instant::now() + Duration::from_secs(20);
        while !ended.is_ended() {
            assert!(Instant::now() < deadline, "the card never ended");
            std::thread::sleep(Duration::from_millis(5));
        }
        "aborted".into()
    }
}

/// The scripted role engine: `pinModel({ taskId })` and its session's `call`.
struct QaRoles {
    sc: JValue,
    calls: Log,
    executor: Arc<OnceLock<Arc<Executor>>>,
    order: Option<Log>,
}

impl ReviewRoles for QaRoles {
    fn pin(&self, task_id: &str) -> Result<Arc<dyn driver::Session>, RoleFail> {
        if let Some(order) = &self.order {
            order.lock().unwrap().push(s("pin"));
        }
        let pin = JValue::obj([("taskId", s(task_id))]);
        self.calls.lock().unwrap().push(JValue::obj([("pin", pin)]));
        let refused = self.sc.get("pin");
        if refused.is_object() {
            return Err(RoleFail {
                code: refused.get("code").clone(),
                reason: refused.get("reason").clone(),
            });
        }
        Ok(Arc::new(QaSession {
            sc: self.sc.clone(),
            calls: self.calls.clone(),
            executor: self.executor.clone(),
        }))
    }
}

struct QaSession {
    sc: JValue,
    calls: Log,
    executor: Arc<OnceLock<Arc<Executor>>>,
}

impl driver::Session for QaSession {
    fn call(&self, req: &JValue, signal: &Signal) -> Result<JValue, driver::Fail> {
        let keys = [
            "role",
            "state",
            "schemaName",
            "constrain",
            "maxBytes",
            "maxTokens",
        ];
        let seen = JValue::obj(keys.map(|k| (k, req.get(k).clone())));
        let seen = wtf16::parse(&text(&seen)).unwrap();
        self.calls
            .lock()
            .unwrap()
            .push(JValue::obj([("call", seen)]));
        let flag = |k: &str| self.sc.get(k).as_bool() == Some(true);
        if flag("callCancels") {
            self.executor.get().unwrap().cancel(TASK).unwrap();
        }
        if flag("callWaits") || flag("callCancels") {
            assert!(
                signal.wait(Duration::from_secs(20)),
                "the call was never aborted"
            );
            return Ok(JValue::obj([
                ("ok", JValue::Bool(false)),
                ("code", s("aborted")),
                ("reason", s("QA aborted")),
            ]));
        }
        Ok(self.sc.get("call").clone())
    }
}

/// The fake sandbox: plays the script on the first connection; the lines it received. A
/// `commit` step commits NEW.md in the task's tree (named in the `session/new` line it heard).
fn sandbox(
    script: Vec<JValue>,
    on_cancel: Box<dyn Fn() + Send>,
) -> (u16, std::thread::JoinHandle<Vec<String>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let handle = std::thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(20)))
            .unwrap();
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        let mut writer = stream;
        let mut lines: Vec<String> = Vec::new();
        let read = |reader: &mut BufReader<_>, lines: &mut Vec<String>| {
            let mut l = String::new();
            let n = reader.read_line(&mut l).unwrap_or(0);
            if n > 0 {
                lines.push(l.strip_suffix('\n').unwrap_or(&l).to_string());
            }
            n
        };
        for step in script {
            if step.get("expect").as_bool() == Some(true) {
                read(&mut reader, &mut lines);
            } else if let Some(chunk) = step.get("send").as_str() {
                std::thread::sleep(Duration::from_millis(5));
                writer.write_all(chunk.as_bytes()).unwrap();
            } else if step.get("commit").as_bool() == Some(true) {
                let tree = lines
                    .iter()
                    .map(|l| wtf16::parse(l).unwrap())
                    .find(|m| m.get("method").as_str() == Some("session/new"))
                    .map(|m| m.get("params").get("cwd").as_str().unwrap().to_string())
                    .unwrap();
                git(Path::new(&tree), &["add", "NEW.md"]);
                git(Path::new(&tree), &["commit", "-qm", "QA change"]);
            } else if step.get("lock").as_bool() == Some(true) {
                let tree = lines
                    .iter()
                    .map(|l| wtf16::parse(l).unwrap())
                    .find(|m| m.get("method").as_str() == Some("session/new"))
                    .map(|m| m.get("params").get("cwd").as_str().unwrap().to_string())
                    .unwrap();
                git(Path::new(&tree), &["worktree", "lock", &tree]);
            } else if step.get("cancel").as_bool() == Some(true) {
                std::thread::sleep(Duration::from_millis(20));
                on_cancel();
            } else if step.get("expectClose").as_bool() == Some(true) {
                while read(&mut reader, &mut lines) > 0 {}
            }
        }
        lines
    });
    (port, handle)
}

fn norm(v: &JValue, tree: &str, base: &str) -> JValue {
    wtf16::parse(&text(v).replace(tree, "<TREE>").replace(base, "<BASE>")).unwrap()
}

/// Council p14: the run's lease, watched. `run_ended` is where core's runExecutor `finally` closes
/// a machine task's relay lease (before the review gate); the drop is the task's end.
struct OrderLease(Log);

impl Holding for OrderLease {
    fn run_ended(&mut self) {
        self.0.lock().unwrap().push(s("run ended"));
    }
}

impl Drop for OrderLease {
    fn drop(&mut self) {
        self.0.lock().unwrap().push(s("lease dropped"));
    }
}

/// One scenario through the Rust executor: what Node's recorder records.
fn replay(name: &str, sc: &JValue) {
    replay_with(name, sc, None);
}

/// [`replay`], with the host's inference gate handing out an [`OrderLease`] on `order` when given
/// (Node's journal, cards and logs are the same with a lease held: it writes nothing of its own).
fn replay_with(name: &str, sc: &JValue, order: Option<Log>) {
    let dir = tempfile::tempdir().unwrap();
    let base = fs::canonicalize(dir.path()).unwrap();
    let b = base.to_str().unwrap().to_string();
    let repo = base.join("qa-repo");
    fs::create_dir_all(&repo).unwrap();
    git(&repo, &["init", "-q", "--template=", "-b", "main"]);
    fs::write(repo.join("a.txt"), "qa a\n").unwrap();
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-qm", "QA first"]);
    let tenant = base.join("tenant");
    fs::create_dir_all(&tenant).unwrap();
    let store = Store::with(
        &tenant,
        StoreOptions {
            kinds: Some(vec!["code".into()]),
            durable: false,
            retain_ms: 14.0 * 86_400_000.0,
            max_jobs: 50,
        },
        Box::new(|| 5000.0),
        Box::new(|| TASK.to_string()),
    );
    let workspaces = Arc::new(Workspaces::with(
        WsOptions {
            dir: tenant.to_str().unwrap().to_string(),
            tree_root: None,
            owner: None,
            noevia: None,
            mode: None,
            epoch: "qa-epoch".into(),
            change_max_buffer: None,
        },
        Box::new(|| 5000.0),
        Git::default(),
        Box::new(chown_tree),
    ));
    let strings = |k: &str| -> Vec<String> {
        match sc.get(k) {
            JValue::Arr(a) => a.iter().map(|x| x.as_str().unwrap().to_string()).collect(),
            _ => Vec::new(),
        }
    };
    let (asked, logs, review_logs, calls): (Log, Log, Log, Log) = Default::default();
    let slot: Arc<OnceLock<Arc<Executor>>> = Arc::new(OnceLock::new());
    let approvals = Arc::new(Scripted {
        answers: Mutex::new(strings("answers").into()),
        asked: asked.clone(),
        executor: slot.clone(),
        order: order.clone(),
    });
    // `engineThrowsAfter`: the host's engine panics from that call on.
    let throws_after = sc.get("engineThrowsAfter").as_num().map(|n| n as usize);
    let engine_calls = AtomicUsize::new(0);
    let JValue::Arr(script) = sc.get("script") else {
        panic!()
    };
    let canceller = slot.clone();
    let (port, agent) = sandbox(
        script.clone(),
        Box::new(move || canceller.get().unwrap().cancel(TASK).unwrap()),
    );
    let sink = logs.clone();
    let jobs = Arc::new(StoreJobs(store));
    let executor = Executor::new(Deps {
        jobs: jobs.clone(),
        workspaces,
        egress: sc
            .get("egress")
            .is_object()
            .then(|| Arc::new(StubEgress(sc.get("egress").clone())) as Arc<dyn Egress>),
        approvals,
        engine: Box::new(move || {
            let n = engine_calls.fetch_add(1, Ordering::SeqCst);
            if throws_after.is_some_and(|after| n >= after) {
                panic!("QA engine");
            }
            Engine {
                base_url: Some("http://qa-engine.invalid:8080/v1".into()),
                api_key: Some("qa-engine-key".into()),
                model: Some("qa-model".into()),
                context_tokens: Some(8192.0),
            }
        }),
        transport: Some(Transport::Socket(format!("127.0.0.1:{port}"))),
        log: Box::new(move |e| sink.lock().unwrap().push(e)),
        now: Box::new(|| 5000.0),
        inference: order.clone().map(|o| {
            Box::new(move || Ok::<Lease, Fail>(Box::new(OrderLease(o.clone()))))
                as Box<dyn Fn() -> Result<Lease, Fail> + Send + Sync>
        }),
        guard: Box::new(|| false),
        owner: None,
        base_path: Some("/usr/local/bin:/usr/bin:/bin".into()),
        base_lang: Some("C.UTF-8".into()),
    });
    let _ = slot.set(executor.clone());
    let throws = sc.get("flagThrows").as_bool() == Some(true);
    let make = || -> Arc<dyn Reviewer> {
        let sink = review_logs.clone();
        let mut review = PlannerReview::new(
            Arc::new(QaRoles {
                sc: sc.clone(),
                calls: calls.clone(),
                executor: slot.clone(),
                order: order.clone(),
            }),
            Arc::new(move |e| sink.lock().unwrap().push(e)),
        );
        if let Some(ms) = sc.get("deadlineMs").as_num() {
            review.deadline_ms = ms as u64;
        }
        Arc::new(review)
    };
    let enabled = || {
        if throws {
            panic!("QA flag reader")
        } else {
            true
        }
    };
    let review = review_gate::decide(&enabled, &make);
    assert_eq!(review.is_none(), throws, "{name}: the flag");
    let started = executor
        .start(StartRequest {
            project_id: Some("qa-project".into()),
            repo_path: repo.to_str().unwrap().to_string(),
            prompt: "Write NEW.md for the QA test.".into(),
            capabilities: strings("capabilities"),
            domains: strings("domains"),
            harness: "opencode".into(),
            sandbox_kind: "sandbox".into(),
            prompt_preparation: "direct".into(),
            tenant_id: Some("qa-tenant".into()),
            review,
            ..StartRequest::default()
        })
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(30);
    while !matches!(
        jobs.status(TASK).as_deref(),
        Some("completed" | "failed" | "cancelled")
    ) || executor.is_live(TASK)
    {
        assert!(Instant::now() < deadline, "{name}: the task never ended");
        std::thread::sleep(Duration::from_millis(10));
    }
    let lines = agent.join().unwrap();
    let tree = started.get("workspace").as_str().unwrap().to_string();
    let same = |what: &str, got: JValue, want: &JValue| {
        assert_eq!(text(&norm(&got, &tree, &b)), text(want), "{name}: {what}");
    };
    same("started", started.clone(), sc.get("started"));
    let rows = fs::read_to_string(tenant.join("jobs").join(format!("{TASK}.jsonl"))).unwrap();
    let journal: Vec<JValue> = rows.lines().map(|l| wtf16::parse(l).unwrap()).collect();
    let JValue::Arr(want) = sc.get("journal") else {
        panic!()
    };
    for (i, (got, want)) in journal.iter().zip(want.iter()).enumerate() {
        same(&format!("journal row {}", i + 1), got.clone(), want);
    }
    assert_eq!(journal.len(), want.len(), "{name}: journal length");
    let all = |l: &Log| JValue::Arr(l.lock().unwrap().clone());
    same("approval cards", all(&asked), sc.get("asked"));
    same("harness log", all(&logs), sc.get("logs"));
    same("reviewer log", all(&review_logs), sc.get("reviewLogs"));
    same("role engine calls", all(&calls), sc.get("reviewCalls"));
    let sent = JValue::Arr(lines.iter().map(|l| s(l)).collect());
    same("what the sandbox received", sent, sc.get("sandboxLines"));
}

#[test]
fn every_node_review_gate_run_replays() {
    let f = wtf16::parse(&fs::read_to_string(FIXTURE).unwrap()).unwrap();
    let JValue::Obj(all) = f.get("scenarios") else {
        panic!()
    };
    assert_eq!(all.len(), 15);
    for (name, sc) in all {
        eprintln!("scenario {name}");
        replay(name, sc);
    }
}

#[test]
fn the_fixture_was_recorded_from_these_sources() {
    let f = wtf16::parse(&fs::read_to_string(FIXTURE).unwrap()).unwrap();
    let sources = f.get("sources");
    assert_eq!(
        sources.get("code-harness.cjs").as_str(),
        Some(review_gate::CODE_HARNESS_SHA256)
    );
    assert_eq!(
        sources.get("code-review.cjs").as_str(),
        Some(review_gate::CODE_REVIEW_SHA256)
    );
}

/// Without a role engine the review is Node's `unavailable`; the flag decides alone whether there
/// is a gate (a reviewer that cannot be built still gets one: Node's answer for a role engine that
/// throws, not "no reviewer configured").
#[test]
fn a_reviewer_without_a_role_engine_says_so() {
    let signal = Signal::new();
    let out = review_gate::Unconfigured.review(&JValue::Null, &signal);
    let fail = out.err().unwrap();
    assert_eq!(fail.code, s("unavailable"));
    assert_eq!(
        fail.reason,
        s("No reviewer model is configured on this server.")
    );
    let make = || -> Arc<dyn Reviewer> { panic!("QA make") };
    let built = review_gate::decide(&|| true, &make).unwrap();
    let fail = built.review(&JValue::Null, &signal).err().unwrap();
    assert_eq!(
        (fail.code, fail.reason),
        (s("error"), s("The reviewer could not be reached."))
    );
    assert!(review_gate::decide(&|| false, &make).is_none());
}

/// Council p14: the run's part of the lease ends when the agent's run does, before the review is
/// pinned and its card asked (core closes a machine task's relay lease in runExecutor's `finally`,
/// and reviewGate runs after `work`); the lease itself goes with the task. The replay is otherwise
/// Node's own, unchanged by the lease. A run that was cancelled ends its lease part with no pin.
#[test]
fn the_run_s_lease_part_ends_before_the_review_and_the_lease_with_the_task() {
    let f = wtf16::parse(&fs::read_to_string(FIXTURE).unwrap()).unwrap();
    for (name, want) in [
        (
            "approved-and-accepted",
            vec!["run ended", "pin", "card", "lease dropped"],
        ),
        (
            "pin-refused",
            vec!["run ended", "pin", "card", "lease dropped"],
        ),
        ("cancelled-run", vec!["run ended", "lease dropped"]),
    ] {
        let sc = f.get("scenarios").get(name).clone();
        assert!(sc.is_object(), "{name}: no such scenario");
        let order: Log = Log::default();
        replay_with(name, &sc, Some(order.clone()));
        // The lease is dropped when the task's thread ends, just after it stops being live.
        let deadline = Instant::now() + Duration::from_secs(10);
        while !order
            .lock()
            .unwrap()
            .iter()
            .any(|e| e.as_str() == Some("lease dropped"))
        {
            assert!(
                Instant::now() < deadline,
                "{name}: the lease was never given back"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
        let got: Vec<String> = order
            .lock()
            .unwrap()
            .iter()
            .map(|e| e.as_str().unwrap().to_string())
            .collect();
        assert_eq!(got, want, "{name}");
    }
}
