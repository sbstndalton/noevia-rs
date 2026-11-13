//! crates/code-harness review gate through the production wiring: a direct start on the Service,
//! whose host ([`Setup`]) says whether the review is on ([`Setup::review_enabled`]) and builds its
//! reviewer ([`Setup::reviewer`]), answered and cancelled through the Service's own card registry.
//! What the gate itself does is pinned by the Node recording (tests/review_gate.rs); these are Rust
//! tests because Node's code-service.cjs is not wired to the reviewer (index.cjs hands the reviewer
//! to code-harness.cjs directly), so there is no Node path through the service to record.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]

use code_acp::Transport;
use code_harness::executor::Engine;
use code_harness::review_gate::{Outcome, Reviewer};
use code_harness::service::{default_harnesses, Options, Parts, Project, Resolved, Service, Setup};
use code_pipeline::driver::{self, Signal};
use code_pipeline::Verdict;
use js_json::{wtf16, JValue};
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::path::Path;
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

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
        .status()
        .unwrap()
        .success();
    assert!(ok, "git {args:?}");
}

fn reply(id: u32, result: &str) -> String {
    format!("{{\"jsonrpc\":\"2.0\",\"id\":{id},\"result\":{result}}}\n")
}

/// The fake sandbox: the ACP handshake, then the agent commits NEW.md in the task's tree (named in
/// the `session/new` line) and ends its turn.
fn agent() -> (u16, std::thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let handle = std::thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(20)))
            .unwrap();
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        let mut writer = stream;
        let mut lines = Vec::new();
        let mut read = |lines: &mut Vec<String>| {
            let mut l = String::new();
            let n = reader.read_line(&mut l).unwrap_or(0);
            if n > 0 {
                lines.push(l);
            }
            n
        };
        read(&mut lines);
        read(&mut lines);
        let hello = r#"{"protocolVersion":1,"agentInfo":{"name":"qa-agent","version":"0.1"}}"#;
        writer.write_all(reply(1, hello).as_bytes()).unwrap();
        read(&mut lines);
        writer
            .write_all(reply(2, r#"{"sessionId":"qa-session"}"#).as_bytes())
            .unwrap();
        read(&mut lines);
        let tree = lines
            .iter()
            .map(|l| wtf16::parse(l.trim_end()).unwrap())
            .find(|m| m.get("method").as_str() == Some("session/new"))
            .map(|m| m.get("params").get("cwd").as_str().unwrap().to_string())
            .unwrap();
        fs::write(Path::new(&tree).join("NEW.md"), "written by the QA agent\n").unwrap();
        git(Path::new(&tree), &["add", "NEW.md"]);
        git(Path::new(&tree), &["commit", "-qm", "QA change"]);
        writer
            .write_all(reply(3, r#"{"stopReason":"end_turn"}"#).as_bytes())
            .unwrap();
        while read(&mut lines) > 0 {}
    });
    (port, handle)
}

/// A reviewer that approves, counting its reviews and keeping the states it was sent.
struct QaReviewer(Arc<Mutex<Vec<JValue>>>);

impl Reviewer for QaReviewer {
    fn review(&self, state: &JValue, _signal: &Signal) -> Outcome {
        self.0.lock().unwrap().push(state.clone());
        Ok((
            Verdict {
                verdict: "approve",
                summary: "QA change looks right.".encode_utf16().collect(),
                findings: Vec::new(),
            },
            false,
        ))
    }
}

/// The host: no pipeline, the review flag as given, the reviewer above (counted as it is built).
struct QaSetup {
    on: Box<dyn Fn() -> bool + Send + Sync>,
    built: Arc<AtomicUsize>,
    states: Arc<Mutex<Vec<JValue>>>,
}

impl Setup for QaSetup {
    fn enabled(&self) -> bool {
        false
    }
    fn create(&self, _parts: Parts, _resolved: &Resolved) -> Option<driver::Deps> {
        None
    }
    fn review_enabled(&self) -> bool {
        (self.on)()
    }
    fn reviewer(&self, resolved: &Resolved) -> Arc<dyn Reviewer> {
        assert_eq!(
            resolved.facts.as_ref().map(|f| f.get("qa").clone()),
            Some(JValue::Bool(true)),
            "the reviewer is built from what was resolved for this start"
        );
        self.built.fetch_add(1, Ordering::SeqCst);
        Arc::new(QaReviewer(self.states.clone()))
    }
}

struct Run {
    service: Arc<Service>,
    tenant: String,
    task: String,
    ids: Arc<Mutex<Vec<String>>>,
    built: Arc<AtomicUsize>,
    states: Arc<Mutex<Vec<JValue>>>,
    agent: std::thread::JoinHandle<()>,
    _dir: tempfile::TempDir,
}

/// One direct task started through `Service::start` with the review flag `on`.
fn start(on: Box<dyn Fn() -> bool + Send + Sync>) -> Run {
    let dir = tempfile::tempdir().unwrap();
    let base = fs::canonicalize(dir.path()).unwrap();
    let repo = base.join("qa-repo");
    fs::create_dir_all(&repo).unwrap();
    git(&repo, &["init", "-q", "--template=", "-b", "main"]);
    fs::write(repo.join("a.txt"), "qa a\n").unwrap();
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-qm", "QA first"]);
    let tenant = base.join("tenant");
    fs::create_dir_all(&tenant).unwrap();
    let (port, agent) = agent();
    let ids = Arc::new(Mutex::new(Vec::<String>::new()));
    let made = ids.clone();
    let (built, states) = (Arc::new(AtomicUsize::new(0)), Arc::default());
    let service = Service::new(Options {
        repos: vec![code_harness::service::Repo {
            id: "qa-repo".into(),
            path: repo.to_str().unwrap().to_string(),
        }],
        harnesses: default_harnesses(None, None),
        sandboxed: true,
        transport: Some(Transport::Socket(format!("127.0.0.1:{port}"))),
        tree_root: None,
        harness_user: None,
        egress: None,
        engine: Arc::new(Engine::default),
        inference: None,
        guard: Arc::new(|| false),
        log: Arc::new(|_| {}),
        now: Arc::new(|| 5000.0),
        timeout: Duration::from_secs(60),
        base_path: std::env::var("PATH").ok(),
        base_lang: None,
        new_id: Arc::new(move || {
            let id = server_projects::source_jobs::new_id();
            made.lock().unwrap().push(id.clone());
            id
        }),
        epoch: "qa-epoch".into(),
        pipeline: Some(Arc::new(QaSetup {
            on,
            built: built.clone(),
            states: Arc::clone(&states),
        })),
    });
    let t = tenant.to_str().unwrap().to_string();
    let record = JValue::obj([("id", JValue::Str("qa-project".into()))]);
    let project = Project {
        id: "qa-project",
        record: &record,
    };
    let resolve = || {
        Ok(Resolved {
            engine: Some(Engine {
                base_url: Some("http://qa-engine.invalid:8080/v1".into()),
                api_key: Some("qa-resolved-key".into()),
                model: Some("qa-model".into()),
                context_tokens: Some(8192.0),
            }),
            facts: Some(JValue::obj([("qa", JValue::Bool(true))])),
            ..Resolved::default()
        })
    };
    let body = wtf16::parse(
        r#"{"repository":"qa-repo","prompt":"QA: write NEW.md.","capabilities":["read_repository","edit_file"]}"#,
    )
    .unwrap();
    let started = service
        .start(&t, Some("qa-user"), &project, &resolve, &body)
        .unwrap();
    let task = started.get("taskId").as_str().unwrap().to_string();
    Run {
        service,
        tenant: t,
        task,
        ids,
        built,
        states,
        agent,
        _dir: dir,
    }
}

fn journal(run: &Run) -> Vec<JValue> {
    let file = Path::new(&run.tenant)
        .join("jobs")
        .join(format!("{}.jsonl", run.task));
    fs::read_to_string(file)
        .unwrap_or_default()
        .lines()
        .map(|l| wtf16::parse(l).unwrap())
        .collect()
}

fn rows<'a>(journal: &'a [JValue], kind: &str) -> Vec<&'a JValue> {
    journal
        .iter()
        .filter(|r| r.get("type").as_str() == Some(kind))
        .collect()
}

fn wait_ended(run: &Run) -> JValue {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let all = journal(run);
        if let Some(end) = all.iter().rev().find(|r| {
            r.get("type")
                .as_str()
                .is_some_and(|t| ["job.completed", "job.failed", "job.cancelled"].contains(&t))
        }) {
            return end.clone();
        }
        assert!(Instant::now() < deadline, "the task never ended");
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// The `review_change` card's id, once it waits in the Service's registry.
fn review_card(run: &Run) -> String {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let asked = journal(run).into_iter().any(|r| {
            r.get("type").as_str() == Some("approval.requested")
                && r.get("data").get("action").as_str() == Some("review_change")
        });
        if asked {
            // The card's id is the last one made; a card not yet registered answers 409.
            let id = run.ids.lock().unwrap().last().cloned().unwrap();
            if id != run.task {
                return id;
            }
        }
        assert!(Instant::now() < deadline, "no review card");
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn with_the_flag_on_the_service_reviews_and_the_person_accepts() {
    let run = start(Box::new(|| true));
    let card = review_card(&run);
    let deadline = Instant::now() + Duration::from_secs(10);
    while run
        .service
        .decide(&run.tenant, "qa-project", &run.task, "approve", &card)
        .is_err()
    {
        assert!(Instant::now() < deadline, "the card never waited");
        std::thread::sleep(Duration::from_millis(10));
    }
    let end = wait_ended(&run);
    assert_eq!(end.get("type").as_str(), Some("job.completed"));
    let review = end.get("data").get("result").get("review");
    assert_eq!(review.get("reviewed"), &JValue::Bool(true));
    assert_eq!(review.get("verdict").as_str(), Some("approve"));
    assert_eq!(review.get("accepted"), &JValue::Bool(true));
    assert_eq!(review.get("decision").as_str(), Some("approve"));
    let all = journal(&run);
    assert_eq!(rows(&all, "review.requested").len(), 1);
    assert_eq!(rows(&all, "review.completed").len(), 1);
    assert_eq!(
        run.built.load(Ordering::SeqCst),
        1,
        "built once, at the start"
    );
    let states = run.states.lock().unwrap().clone();
    assert_eq!(states.len(), 1);
    assert_eq!(states[0].get("taskId").as_str(), Some(run.task.as_str()));
    assert_eq!(
        states[0].get("credentials").get("engine").as_str(),
        Some("qa-resolved-key"),
        "the key of the engine resolved for this start"
    );
    run.agent.join().unwrap();
}

#[test]
fn with_the_flag_off_or_unreadable_there_is_no_review() {
    for on in [
        Box::new(|| false) as Box<dyn Fn() -> bool + Send + Sync>,
        Box::new(|| panic!("QA flag reader")),
    ] {
        let run = start(on);
        let end = wait_ended(&run);
        assert_eq!(end.get("type").as_str(), Some("job.completed"));
        let result = end.get("data").get("result");
        assert!(result.is_object());
        assert_eq!(result.get("review"), &JValue::Undefined);
        let all = journal(&run);
        for kind in ["review.requested", "approval.requested"] {
            assert!(rows(&all, kind).is_empty(), "{kind}");
        }
        assert_eq!(run.built.load(Ordering::SeqCst), 0, "no reviewer is built");
        assert!(run.states.lock().unwrap().is_empty());
        run.agent.join().unwrap();
    }
}

/// A cancel while the `review_change` card waits: the registry refuses it as `aborted`, the task
/// is cancelled and nothing is accepted.
#[test]
fn a_cancel_at_the_review_card_is_aborted_not_accepted() {
    let run = start(Box::new(|| true));
    let card = review_card(&run);
    // The card waits (a cancel that lands before it is registered refuses it the same way).
    std::thread::sleep(Duration::from_millis(100));
    run.service
        .cancel(&run.tenant, "qa-project", &run.task)
        .unwrap();
    let end = wait_ended(&run);
    assert_eq!(end.get("type").as_str(), Some("job.cancelled"));
    let all = journal(&run);
    let decided = rows(&all, "approval.decided");
    assert_eq!(decided.len(), 1);
    assert_eq!(
        decided[0].get("data").get("decision").as_str(),
        Some("aborted")
    );
    assert_eq!(
        decided[0].get("data").get("action").as_str(),
        Some("review_change")
    );
    let review = end.get("data").get("result").get("review");
    assert_ne!(review.get("accepted"), &JValue::Bool(true));
    // The card is settled: a late yes is refused.
    let late = run
        .service
        .decide(&run.tenant, "qa-project", &run.task, "approve", &card)
        .err()
        .unwrap();
    assert_eq!(late.status, Some(409));
    run.agent.join().unwrap();
}
