//! crates/code-harness service: core's own code-service.cjs answering Code mode's requests, as
//! tests/fixtures/service-node.v1.json recorded them (tools/gen-service-fixtures.cjs: the real
//! task stack against a scripted fake sandbox, routes/code.cjs's error mapping), replayed on the
//! Rust service: every answer, each task's journal, the log lines, the egress calls and every line
//! the sandbox received must match. Also parseRepos, parseUser and defaultHarnesses on the
//! recorded table.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]

use code_acp::Transport;
use code_harness::executor::{Egress, Engine, Fail, Lease};
use code_harness::service::{
    default_harnesses, parse_repos, parse_user, shares_code, Inference, Options, Project, Resolved,
    Service, APPROVAL_TIMEOUT,
};
use js_json::{wtf16, JValue};
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::path::Path;
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const FIXTURE: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/service-node.v1.json"
);

fn text(v: &JValue) -> String {
    wtf16::stringify(v).unwrap()
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

fn repo(base: &Path, name: &str) -> String {
    let r = base.join(name);
    fs::create_dir_all(&r).unwrap();
    git(&r, &["init", "-q", "--template=", "-b", "main"]);
    fs::write(r.join("a.txt"), "qa a\n").unwrap();
    git(&r, &["add", "."]);
    git(&r, &["commit", "-qm", "QA first"]);
    r.to_str().unwrap().to_string()
}

struct StubEgress {
    calls: Mutex<Vec<JValue>>,
}

impl Egress for StubEgress {
    fn endpoint(&self) -> Option<String> {
        Some("egress:3128".into())
    }
    fn grant(&self, task_id: &str, domains: &[String]) -> Result<JValue, Fail> {
        self.calls.lock().unwrap().push(JValue::obj([(
            "grant",
            JValue::obj([
                ("taskId", JValue::Str(task_id.into())),
                (
                    "domains",
                    JValue::Arr(domains.iter().map(|d| JValue::Str(d.clone())).collect()),
                ),
            ]),
        )]));
        Ok(JValue::obj([
            ("token", JValue::Str("qa-token".into())),
            ("id", JValue::Str("qa-grant".into())),
        ]))
    }
    fn revoke(&self, grant: &JValue) -> Result<JValue, Fail> {
        self.calls
            .lock()
            .unwrap()
            .push(JValue::obj([("revoke", grant.get("id").clone())]));
        Ok(JValue::obj([
            ("confirmed", JValue::Bool(true)),
            (
                "report",
                JValue::obj([
                    ("available", JValue::Bool(true)),
                    ("state", JValue::Str("closed".into())),
                    ("final", JValue::Bool(false)),
                    ("partial", JValue::Bool(false)),
                ]),
            ),
        ]))
    }
    fn forget(&self, _grant: &JValue, _snapshot_id: &str) -> Result<JValue, Fail> {
        Ok(JValue::obj([("confirmed", JValue::Bool(true))]))
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

/// The fake sandbox: plays the script on the first connection; every line it received.
fn sandbox(script: Vec<JValue>) -> (u16, std::thread::JoinHandle<Vec<String>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let handle = std::thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(20)))
            .unwrap();
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        let mut writer = stream.try_clone().unwrap();
        let mut lines = Vec::new();
        for step in script {
            if step.get("expect").as_bool() == Some(true) {
                let mut l = String::new();
                reader.read_line(&mut l).unwrap();
                lines.push(l.strip_suffix('\n').unwrap_or(&l).to_string());
            } else if let Some(chunk) = step.get("send").as_str() {
                std::thread::sleep(Duration::from_millis(5));
                writer.write_all(chunk.as_bytes()).unwrap();
            } else if step.get("expectClose").as_bool() == Some(true) {
                loop {
                    let mut l = String::new();
                    if reader.read_line(&mut l).unwrap_or(0) == 0 {
                        break;
                    }
                    lines.push(l.strip_suffix('\n').unwrap_or(&l).to_string());
                }
            }
        }
        lines
    });
    (port, handle)
}

fn norm(v: &JValue, base: &str) -> JValue {
    wtf16::parse(&text(v).replace(base, "<BASE>")).unwrap()
}

/// routes/code.cjs: the answer, or its error mapping.
fn routed(status: u16, r: Result<JValue, Fail>) -> JValue {
    match r {
        Ok(body) => JValue::obj([("status", JValue::Num(f64::from(status))), ("body", body)]),
        Err(e) => {
            let message = match (&e.public_message, e.status) {
                (Some(m), _) if !m.is_empty() => m.clone(),
                (_, Some(_)) => e.message.clone(),
                (_, None) => "Code request failed".into(),
            };
            JValue::obj([
                ("status", JValue::Num(f64::from(e.status.unwrap_or(500)))),
                ("body", JValue::obj([("error", JValue::Str(message))])),
            ])
        }
    }
}

fn ended(dir: &Path, task: &str) -> bool {
    fs::read_to_string(dir.join("jobs").join(format!("{task}.jsonl"))).is_ok_and(|t| {
        ["completed", "failed", "cancelled", "interrupted"]
            .iter()
            .any(|s| t.contains(&format!("\"type\":\"job.{s}\"")))
    })
}

#[test]
fn every_node_service_answer_replays() {
    let f = wtf16::parse(&fs::read_to_string(FIXTURE).unwrap()).unwrap();
    let JValue::Obj(all) = f.get("scenarios") else {
        panic!()
    };
    for (name, sc) in all {
        let dir = tempfile::tempdir().unwrap();
        let base = fs::canonicalize(dir.path()).unwrap();
        let b = base.to_str().unwrap().to_string();
        let repo_path = repo(&base, "qa-repo");
        let tenant = base.join("tenant");
        fs::create_dir_all(&tenant).unwrap();
        let t = tenant.to_str().unwrap().to_string();
        let JValue::Arr(script) = sc.get("script") else {
            panic!()
        };
        let sandboxed = (!script.is_empty()).then(|| sandbox(script.clone()));
        let port = sandboxed.as_ref().map_or(9, |(p, _)| *p);
        let egress = (sc.get("egress").as_bool() == Some(true)).then(|| {
            Arc::new(StubEgress {
                calls: Mutex::new(Vec::new()),
            })
        });
        let logs = Arc::new(Mutex::new(Vec::<JValue>::new()));
        let sink = logs.clone();
        let counter = Arc::new(AtomicUsize::new(0));
        let ids = counter.clone();
        let timeout = sc.get("timeoutMs").as_num().unwrap();
        let service = Service::new(Options {
            repos: vec![code_harness::service::Repo {
                id: "qa-repo".into(),
                path: repo_path,
            }],
            harnesses: default_harnesses(Some("opencode"), Some("0.0.0-qa")),
            sandboxed: true,
            transport: Some(Transport::Socket(format!("127.0.0.1:{port}"))),
            tree_root: None,
            harness_user: None,
            egress: egress.clone().map(|e| e as Arc<dyn Egress>),
            engine: Arc::new(|| Engine {
                base_url: Some("http://qa-engine.invalid:8080/v1".into()),
                api_key: Some("qa-engine-key".into()),
                model: Some("qa-model".into()),
                context_tokens: Some(8192.0),
            }),
            inference: sc
                .get("inference")
                .as_str()
                .map(|m| Arc::new(Gate(m.to_string())) as Arc<dyn Inference>),
            guard: Arc::new(|| false),
            log: Arc::new(move |e| sink.lock().unwrap().push(e)),
            now: Arc::new(|| 5000.0),
            timeout: Duration::from_millis(timeout as u64),
            base_path: f.get("env").get("PATH").as_str().map(str::to_string),
            base_lang: f.get("env").get("LANG").as_str().map(str::to_string),
            new_id: Arc::new(move || {
                let n = ids.fetch_add(1, Ordering::SeqCst) + 1;
                format!("00000000-0000-4000-8000-{n:012}")
            }),
            epoch: "qa-epoch".into(),
            pipeline: None,
        });
        let record = sc.get("project").clone();
        let project_id = record.get("id").as_str().unwrap().to_string();
        let project = Project {
            id: &project_id,
            record: &record,
        };
        let shared = shares_code(&record);
        let resolved = || Resolved {
            context: if shared {
                "QA shared context: the goal is a green test run.".to_string()
            } else {
                String::new()
            },
            ..Resolved::default()
        };
        let mut tasks: Vec<String> = Vec::new();
        let mut cards: Vec<String> = Vec::new();
        let sub = |v: &JValue, tasks: &[String], cards: &[String]| -> String {
            let mut out = v.as_str().unwrap_or("").to_string();
            for (i, id) in tasks.iter().enumerate() {
                out = out.replace(&format!("<T{}>", i + 1), id);
            }
            for (i, id) in cards.iter().enumerate() {
                out = out.replace(&format!("<A{}>", i + 1), id);
            }
            out
        };
        let JValue::Arr(ops) = sc.get("ops") else {
            panic!()
        };
        let JValue::Arr(want) = sc.get("answers") else {
            panic!()
        };
        for (i, op) in ops.iter().enumerate() {
            let got = if let JValue::Obj(_) = op.get("start") {
                let a = routed(
                    202,
                    service.start(
                        &t,
                        Some("qa-user"),
                        &project,
                        &|| Ok(resolved()),
                        op.get("start"),
                    ),
                );
                if let Some(id) = a.get("body").get("taskId").as_str() {
                    tasks.push(id.to_string());
                }
                a
            } else if let JValue::Arr(d) = op.get("decide") {
                let [task, decision, card] = [&d[0], &d[1], &d[2]].map(|v| sub(v, &tasks, &cards));
                routed(
                    200,
                    service.decide(&t, &project_id, &task, &decision, &card),
                )
            } else if !op.get("cancel").is_nullish() {
                let task = sub(op.get("cancel"), &tasks, &cards);
                routed(200, service.cancel(&t, &project_id, &task))
            } else if op.get("facts").as_bool() == Some(true) {
                routed(200, service.facts(&t))
            } else if !op.get("waitCard").is_nullish() {
                let task = sub(op.get("waitCard"), &tasks, &cards);
                let deadline = Instant::now() + Duration::from_secs(10);
                let mut card = None;
                while card.is_none() && Instant::now() < deadline {
                    card = service
                        .facts(&t)
                        .unwrap()
                        .get("approvals")
                        .get(&task)
                        .get("id")
                        .as_str()
                        .map(str::to_string);
                    if card.is_none() {
                        std::thread::sleep(Duration::from_millis(10));
                    }
                }
                if let Some(c) = &card {
                    if !cards.contains(c) {
                        cards.push(c.clone());
                    }
                }
                JValue::obj([(
                    "waited",
                    JValue::Str(if card.is_some() { "card" } else { "none" }.into()),
                )])
            } else if !op.get("waitEnd").is_nullish() {
                let task = sub(op.get("waitEnd"), &tasks, &cards);
                let deadline = Instant::now() + Duration::from_secs(10);
                while !ended(&tenant, &task) && Instant::now() < deadline {
                    std::thread::sleep(Duration::from_millis(10));
                }
                std::thread::sleep(Duration::from_millis(100));
                JValue::obj([(
                    "waited",
                    JValue::Str(
                        if ended(&tenant, &task) {
                            "ended"
                        } else {
                            "running"
                        }
                        .into(),
                    ),
                )])
            } else if let Some(ms) = op.get("sleep").as_num() {
                std::thread::sleep(Duration::from_millis(ms as u64));
                JValue::obj([("slept", JValue::Num(ms))])
            } else {
                panic!("unknown op {}", text(op))
            };
            assert_eq!(
                text(&norm(&got, &b)),
                text(&want[i]),
                "{name} op {i}: {}",
                text(op)
            );
        }
        std::thread::sleep(Duration::from_millis(100));
        for task in &tasks {
            let want = sc.get("journals").get(task);
            let rows = fs::read_to_string(tenant.join("jobs").join(format!("{task}.jsonl")));
            let JValue::Arr(want) = want else {
                assert!(rows.is_err(), "{name}: {task} has a journal");
                continue;
            };
            let got: Vec<JValue> = rows
                .unwrap()
                .lines()
                .map(|l| norm(&wtf16::parse(l).unwrap(), &b))
                .collect();
            for (i, (g, w)) in got.iter().zip(want.iter()).enumerate() {
                assert_eq!(text(g), text(w), "{name}: {task} journal row {}", i + 1);
            }
            assert_eq!(got.len(), want.len(), "{name}: {task} journal length");
        }
        let lines = sandboxed
            .map(|(_, h)| h.join().unwrap())
            .unwrap_or_default();
        let sent = JValue::Arr(
            lines
                .iter()
                .map(|l| norm(&JValue::Str(l.clone()), &b))
                .collect(),
        );
        assert_eq!(
            text(&sent),
            text(sc.get("sandboxLines")),
            "{name}: sandbox lines"
        );
        let logged = JValue::Arr(logs.lock().unwrap().clone());
        assert_eq!(
            text(&norm(&logged, &b)),
            text(sc.get("logs")),
            "{name}: logs"
        );
        let calls = JValue::Arr(
            egress
                .map(|e| e.calls.lock().unwrap().clone())
                .unwrap_or_default(),
        );
        assert_eq!(text(&calls), text(sc.get("egressCalls")), "{name}: egress");
    }
}

#[test]
fn the_configuration_parsers_answer_as_nodes() {
    let f = wtf16::parse(&fs::read_to_string(FIXTURE).unwrap()).unwrap();
    let tables = f.get("tables");
    let dir = tempfile::tempdir().unwrap();
    let base = fs::canonicalize(dir.path()).unwrap();
    let b = base.to_str().unwrap().to_string();
    let a = repo(&base, "qa-a");
    repo(&base, "qa-b");
    fs::create_dir_all(base.join("not-a-repo")).unwrap();
    std::os::unix::fs::symlink(&a, base.join("link-a")).unwrap();
    let JValue::Arr(rows) = tables.get("repos") else {
        panic!()
    };
    for row in rows {
        let JValue::Arr(pair) = row else { panic!() };
        let raw = pair[0].as_str().unwrap().replace("<BASE>", &b);
        let got = JValue::Arr(
            parse_repos(&raw)
                .into_iter()
                .map(|r| {
                    JValue::obj([
                        ("id", JValue::Str(r.id)),
                        ("path", JValue::Str(r.path.replace(&b, "<BASE>"))),
                    ])
                })
                .collect(),
        );
        assert_eq!(text(&got), text(&pair[1]), "parseRepos {raw:?}");
    }
    let JValue::Arr(rows) = tables.get("users") else {
        panic!()
    };
    for row in rows {
        let JValue::Arr(pair) = row else { panic!() };
        let raw = pair[0].as_str().unwrap();
        let got = parse_user(Some(raw)).map_or(JValue::Null, |i| {
            JValue::obj([
                ("uid", JValue::Num(f64::from(i.uid))),
                ("gid", JValue::Num(f64::from(i.gid))),
            ])
        });
        assert_eq!(text(&got), text(&pair[1]), "parseUser {raw:?}");
    }
    let JValue::Arr(rows) = tables.get("harnesses") else {
        panic!()
    };
    for row in rows {
        let JValue::Arr(pair) = row else { panic!() };
        let env = &pair[0];
        let got = JValue::Arr(
            default_harnesses(
                env.get("CODE_HARNESS_NAME").as_str(),
                env.get("CODE_HARNESS_VERSION").as_str(),
            )
            .into_iter()
            .map(|h| {
                JValue::obj([
                    ("id", JValue::Str(h.id)),
                    ("label", JValue::Str(h.label)),
                    ("version", h.version.map_or(JValue::Null, JValue::Str)),
                ])
            })
            .collect(),
        );
        assert_eq!(text(&got), text(&pair[1]), "defaultHarnesses {}", text(env));
    }
    assert_eq!(APPROVAL_TIMEOUT, Duration::from_secs(300));
}

/// A harness that accepts the connection and never answers: the task stays running.
fn silent_agent() -> (u16, std::thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    listener.set_nonblocking(true).unwrap();
    let held = std::thread::spawn(move || {
        let mut open = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(20);
        while Instant::now() < deadline {
            match listener.accept() {
                Ok((s, _)) => open.push(s),
                Err(_) => std::thread::sleep(Duration::from_millis(10)),
            }
            if open.len() >= 2 {
                break;
            }
        }
        std::thread::sleep(Duration::from_millis(500));
    });
    (port, held)
}

fn quiet_service(base: &Path, port: u16) -> (Arc<Service>, String, Arc<Mutex<Vec<JValue>>>) {
    let repo_path = repo(base, "qa-repo");
    let tenant = base.join("tenant");
    fs::create_dir_all(&tenant).unwrap();
    let logs = Arc::new(Mutex::new(Vec::<JValue>::new()));
    let sink = logs.clone();
    let service = Service::new(Options {
        repos: vec![code_harness::service::Repo {
            id: "qa-repo".into(),
            path: repo_path,
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
        log: Arc::new(move |e| sink.lock().unwrap().push(e)),
        now: Arc::new(|| 5000.0),
        timeout: Duration::from_secs(60),
        base_path: std::env::var("PATH").ok(),
        base_lang: None,
        new_id: Arc::new(server_projects::source_jobs::new_id),
        epoch: "qa-epoch".into(),
        pipeline: None,
    });
    (service, tenant.to_str().unwrap().to_string(), logs)
}

fn journal(dir: &str, task: &str) -> Vec<JValue> {
    fs::read_to_string(Path::new(dir).join("jobs").join(format!("{task}.jsonl")))
        .unwrap_or_default()
        .lines()
        .map(|l| wtf16::parse(l).unwrap())
        .collect()
}

fn wait_ended(dir: &str, task: &str) -> String {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let rows = journal(dir, task);
        if let Some(end) = rows.iter().rev().find_map(|r| {
            r.get("type")
                .as_str()
                .filter(|t| ["job.completed", "job.failed", "job.cancelled"].contains(t))
                .map(str::to_string)
        }) {
            return end;
        }
        assert!(Instant::now() < deadline, "{task} never ended");
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// Node's one-task check and the job's creation are one synchronous step; here two concurrent
/// starts for one project give one task and one 409.
#[test]
fn two_concurrent_starts_give_one_task() {
    let dir = tempfile::tempdir().unwrap();
    let base = fs::canonicalize(dir.path()).unwrap();
    let (port, agent) = silent_agent();
    let (service, t, _) = quiet_service(&base, port);
    let record = JValue::obj([("id", JValue::Str("qa-project".into()))]);
    let body = wtf16::parse(r#"{"repository":"qa-repo","prompt":"QA: one at a time."}"#).unwrap();
    let starts: Vec<_> = (0..2)
        .map(|_| {
            let (service, t, record, body) =
                (service.clone(), t.clone(), record.clone(), body.clone());
            std::thread::spawn(move || {
                let project = Project {
                    id: "qa-project",
                    record: &record,
                };
                // Slow to resolve, so both starts are past their body checks together.
                let resolve = || {
                    std::thread::sleep(Duration::from_millis(200));
                    Ok(Resolved {
                        engine: Some(Engine {
                            base_url: Some("http://qa-engine.invalid:8080/v1".into()),
                            model: Some("qa-model".into()),
                            ..Engine::default()
                        }),
                        ..Resolved::default()
                    })
                };
                service.start(&t, Some("qa-user"), &project, &resolve, &body)
            })
        })
        .collect();
    let results: Vec<_> = starts.into_iter().map(|h| h.join().unwrap()).collect();
    let started: Vec<_> = results.iter().filter_map(|r| r.as_ref().ok()).collect();
    let refused: Vec<_> = results.iter().filter_map(|r| r.as_ref().err()).collect();
    assert_eq!(started.len(), 1, "{results:?}");
    assert_eq!(refused.len(), 1);
    assert_eq!(refused[0].status, Some(409));
    assert_eq!(
        refused[0].message,
        "This project already has a task running."
    );
    let task = started[0].get("taskId").as_str().unwrap().to_string();
    service.cancel(&t, "qa-project", &task).unwrap();
    assert_eq!(wait_ended(&t, &task), "job.cancelled");
    agent.join().unwrap();
}

type LostHook = Arc<Mutex<Option<Box<dyn FnOnce() + Send>>>>;

struct Watched {
    entered: Arc<AtomicUsize>,
    held: Arc<AtomicUsize>,
    lost: LostHook,
}

struct WatchedLease(Arc<AtomicUsize>, LostHook);

impl code_harness::executor::Holding for WatchedLease {
    fn on_lost(&mut self, hook: Box<dyn FnOnce() + Send>) {
        *self.1.lock().unwrap() = Some(hook);
    }
}

impl Drop for WatchedLease {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

impl Inference for Watched {
    fn enter(&self) -> Result<Lease, Fail> {
        self.entered.fetch_add(1, Ordering::SeqCst);
        self.held.fetch_add(1, Ordering::SeqCst);
        Ok(Box::new(WatchedLease(self.held.clone(), self.lost.clone())) as Lease)
    }
    fn paused(&self) -> bool {
        false
    }
}

/// The front's path: the engine and the gate resolved for this start are the ones the run uses,
/// and a lease that stops covering the run stops it.
#[test]
fn a_start_runs_on_what_was_resolved_for_it_and_a_lost_lease_stops_it() {
    let dir = tempfile::tempdir().unwrap();
    let base = fs::canonicalize(dir.path()).unwrap();
    let (port, agent) = silent_agent();
    let (service, t, logs) = quiet_service(&base, port);
    let gate = Arc::new(Watched {
        entered: Arc::new(AtomicUsize::new(0)),
        held: Arc::new(AtomicUsize::new(0)),
        lost: Arc::new(Mutex::new(None)),
    });
    let record = JValue::obj([("id", JValue::Str("qa-project".into()))]);
    let project = Project {
        id: "qa-project",
        record: &record,
    };
    let g = gate.clone();
    let resolve = move || {
        Ok(Resolved {
            engine: Some(Engine {
                base_url: Some("http://qa-resolved.invalid:9090/v1".into()),
                api_key: None,
                model: Some("qa-resolved-model".into()),
                context_tokens: Some(4096.0),
            }),
            inference: Some(g.clone() as Arc<dyn Inference>),
            context: String::new(),
            facts: None,
        })
    };
    let body = wtf16::parse(r#"{"repository":"qa-repo","prompt":"QA: resolved."}"#).unwrap();
    let started = service
        .start(&t, Some("qa-user"), &project, &resolve, &body)
        .unwrap();
    let task = started.get("taskId").as_str().unwrap().to_string();
    assert_eq!(gate.entered.load(Ordering::SeqCst), 1);
    assert_eq!(gate.held.load(Ordering::SeqCst), 1, "held while it runs");
    // The pin files name the resolved engine and model.
    let deadline = Instant::now() + Duration::from_secs(20);
    let pinned = loop {
        let rows = journal(&t, &task);
        if let Some(p) = rows.iter().find(|r| {
            r.get("type").as_str() == Some("step.completed")
                && r.get("data").get("id").as_str() == Some("harness.config")
        }) {
            break p.get("data").clone();
        }
        assert!(Instant::now() < deadline, "no pin step");
        std::thread::sleep(Duration::from_millis(10));
    };
    let pinned = wtf16::stringify(&pinned).unwrap();
    assert!(pinned.contains("qa-resolved-model"), "{pinned}");
    assert!(
        pinned.contains("http://qa-resolved.invalid:9090/v1"),
        "{pinned}"
    );
    // The authority ends the lease: the run stops, and says why in the log.
    let hook = gate
        .lost
        .lock()
        .unwrap()
        .take()
        .expect("the executor watches the lease");
    hook();
    assert_eq!(wait_ended(&t, &task), "job.cancelled");
    let deadline = Instant::now() + Duration::from_secs(5);
    while gate.held.load(Ordering::SeqCst) != 0 {
        assert!(
            Instant::now() < deadline,
            "the lease was kept after the run"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(logs
        .lock()
        .unwrap()
        .iter()
        .any(|l| l.get("event").as_str() == Some("code.lease_lost")
            && l.get("taskId").as_str() == Some(&task)));
    agent.join().unwrap();
}
