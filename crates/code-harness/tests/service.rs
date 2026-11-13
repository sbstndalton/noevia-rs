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
        Ok(Box::new(()))
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
                    service.start(&t, Some("qa-user"), &project, resolved(), op.get("start")),
                );
                if let Some(id) = a.get("body").get("taskId").as_str() {
                    tasks.push(id.to_string());
                }
                a
            } else if let JValue::Arr(d) = op.get("decide") {
                let [task, decision, card] =
                    [&d[0], &d[1], &d[2]].map(|v| sub(v, &tasks, &cards));
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
                    JValue::Str(if ended(&tenant, &task) { "ended" } else { "running" }.into()),
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
        assert_eq!(text(&sent), text(sc.get("sandboxLines")), "{name}: sandbox lines");
        let logged = JValue::Arr(logs.lock().unwrap().clone());
        assert_eq!(text(&norm(&logged, &b)), text(sc.get("logs")), "{name}: logs");
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
