//! crates/code-harness session: each script core's own code-harness.cjs session answered in
//! tests/fixtures/session-node.v1.json (tools/gen-session-fixtures.cjs: a stub agent calling the
//! session's handlers in a real worktree of a QA repository), replayed through the Rust session
//! in a fresh worktree made by the code-workspace port. Every answer, every journal call made
//! while the prompt ran, every approval card and log line, and the run's summary must match.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]

use code_harness::session::{Ended, Host, Refusal, Session, Task};
use code_workspace::{Claim, Options as WsOptions, Workspaces};
use js_json::{wtf16, JValue};
use std::collections::VecDeque;
use std::fs;
use std::path::Path;
use std::process::Command;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

const FIXTURE: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/session-node.v1.json"
);
const TASK: &str = "00000000-0000-4000-8000-0000000000aa";

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

struct TestHost {
    calls: Mutex<Vec<JValue>>,
    asked: Mutex<Vec<JValue>>,
    logs: Mutex<Vec<JValue>>,
    answers: Mutex<VecDeque<String>>,
    clock: AtomicU64,
    halted: AtomicBool,
    ws: Workspaces,
}

impl Host for TestHost {
    fn event(&self, kind: &str, data: JValue) -> Result<(), String> {
        self.calls.lock().unwrap().push(JValue::obj([
            ("fn", JValue::Str("event".into())),
            ("args", JValue::Arr(vec![JValue::Str(kind.into()), data])),
        ]));
        Ok(())
    }
    fn ask_approval(&self, request: &JValue, ended: &Arc<Ended>) -> String {
        self.asked.lock().unwrap().push(request.clone());
        if ended.is_ended() {
            return "aborted".into();
        }
        self.answers
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_else(|| "deny".into())
    }
    fn contains(&self, path: &JValue) -> Option<bool> {
        match path.as_str() {
            Some(p) => self.ws.contains(TASK, p).unwrap(),
            None => self.ws.contains(TASK, "").unwrap().map(|_| false),
        }
    }
    fn root(&self) -> Option<String> {
        self.ws
            .get(TASK)
            .unwrap()
            .get("path")
            .as_str()
            .map(str::to_string)
    }
    fn log(&self, entry: JValue) {
        self.logs.lock().unwrap().push(entry);
    }
    fn now(&self) -> f64 {
        f64::from_bits(self.clock.load(Ordering::SeqCst))
    }
    fn halt(&self) {
        self.halted.store(true, Ordering::SeqCst);
    }
}

fn answer(r: Result<JValue, Refusal>) -> JValue {
    match r {
        Ok(v) => JValue::obj([("ok", v)]),
        Err(e) => JValue::obj([(
            "error",
            JValue::obj([
                ("code", e.code.map_or(JValue::Null, JValue::Num)),
                ("message", JValue::Str(e.message)),
                ("data", e.data.unwrap_or(JValue::Null)),
            ]),
        )]),
    }
}

fn norm(v: &JValue, tree: &str, dir: &str) -> JValue {
    wtf16::parse(&text(v).replace(tree, "<TREE>").replace(dir, "<DIR>")).unwrap()
}

fn sub(v: &JValue, tree: &str) -> JValue {
    wtf16::parse(&text(v).replace("<TREE>", tree)).unwrap()
}

#[test]
fn every_node_session_replays() {
    let f = wtf16::parse(&fs::read_to_string(FIXTURE).unwrap()).unwrap();
    let JValue::Obj(all) = f.get("scenarios") else {
        panic!()
    };
    for (name, sc) in all {
        let dir = tempfile::tempdir().unwrap();
        let base = fs::canonicalize(dir.path()).unwrap();
        let repo = base.join("qa-repo");
        fs::create_dir_all(&repo).unwrap();
        git(&repo, &["init", "-q", "--template=", "-b", "main"]);
        fs::write(repo.join("a.txt"), "qa a\n").unwrap();
        fs::write(repo.join("lines.txt"), "l1\nl2\nl3\nl4\nl5\n").unwrap();
        git(&repo, &["add", "."]);
        git(&repo, &["commit", "-qm", "QA first"]);
        let tenant = base.join("tenant");
        fs::create_dir_all(&tenant).unwrap();
        let ws = Workspaces::new(WsOptions {
            dir: tenant.to_str().unwrap().to_string(),
            tree_root: None,
            owner: None,
            noevia: None,
            mode: None,
            epoch: "qa-epoch".into(),
            change_max_buffer: None,
        });
        let claimed = ws
            .claim(Claim {
                task_id: TASK,
                repo_path: repo.to_str().unwrap(),
                pinned: &[JValue::Str("opencode.json".into())],
                ..Claim::default()
            })
            .unwrap();
        let tree = claimed.get("path").as_str().unwrap().to_string();
        let dir_s = tenant.to_str().unwrap().to_string();
        let strings = |k: &str| -> Vec<String> {
            match sc.get(k) {
                JValue::Arr(a) => a.iter().map(|x| x.as_str().unwrap().to_string()).collect(),
                _ => Vec::new(),
            }
        };
        let host = Arc::new(TestHost {
            calls: Mutex::new(Vec::new()),
            asked: Mutex::new(Vec::new()),
            logs: Mutex::new(Vec::new()),
            answers: Mutex::new(strings("answers").into()),
            clock: AtomicU64::new(0f64.to_bits()),
            halted: AtomicBool::new(false),
            ws,
        });
        let session = Session::new(
            host.clone(),
            Task {
                task_id: TASK.into(),
                capabilities: strings("capabilities"),
                domains: strings("domains"),
                harness: "opencode".into(),
                model: JValue::Str("qa-model".into()),
                workspace_path: tree.clone(),
                guarded: sc.get("guarded").as_bool() == Some(true),
            },
        );
        let JValue::Arr(script) = sc.get("script") else {
            panic!()
        };
        let JValue::Arr(want) = sc.get("results") else {
            panic!()
        };
        for (i, step) in script.iter().enumerate() {
            let got = if let Some(t) = step.get("clock").as_num() {
                host.clock.store(t.to_bits(), Ordering::SeqCst);
                JValue::Null
            } else if !matches!(step.get("update"), JValue::Undefined) {
                let _ = session.session_update(&sub(step.get("update"), &tree));
                JValue::Null
            } else if step.get("cancel").as_bool() == Some(true) {
                // The task's cancel: its signal aborts, which ends the session.
                session.end();
                JValue::Null
            } else if let JValue::Obj(_) = step.get("fs") {
                let fs_step = step.get("fs");
                if let Some(rel) = fs_step.get("write").as_str() {
                    fs::write(
                        Path::new(&tree).join(rel),
                        fs_step.get("text").as_str().unwrap(),
                    )
                    .unwrap();
                }
                if let Some(rel) = fs_step.get("symlink").as_str() {
                    std::os::unix::fs::symlink(
                        fs_step.get("to").as_str().unwrap(),
                        Path::new(&tree).join(rel),
                    )
                    .unwrap();
                }
                JValue::Null
            } else {
                let params = match step.get("params") {
                    JValue::Undefined => JValue::Obj(Vec::new()),
                    p => sub(p, &tree),
                };
                let r = match step.get("call").as_str().unwrap() {
                    "requestPermission" => session.request_permission(&params),
                    "readTextFile" => session.read_text_file(&params),
                    "writeTextFile" => session.write_text_file(&params),
                    other => panic!("unknown call {other}"),
                };
                answer(r)
            };
            assert_eq!(
                text(&norm(&got, &tree, &dir_s)),
                text(&want[i]),
                "{name} step {i}: {}",
                text(step)
            );
        }
        // Node's recording stopped when the prompt returned, before runExecutor's flushOutput.
        let calls = JValue::Arr(host.calls.lock().unwrap().clone());
        assert_eq!(
            text(&norm(&calls, &tree, &dir_s)),
            text(sc.get("calls")),
            "{name}: journal calls"
        );
        // What only the final flush sends.
        let before = host.calls.lock().unwrap().len();
        let _ = session.flush_output();
        let flushed = JValue::Arr(host.calls.lock().unwrap()[before..].to_vec());
        assert_eq!(
            text(&norm(&flushed, &tree, &dir_s)),
            text(sc.get("flushCalls")),
            "{name}: the final flush"
        );
        let asked = JValue::Arr(host.asked.lock().unwrap().clone());
        assert_eq!(
            text(&norm(&asked, &tree, &dir_s)),
            text(sc.get("asked")),
            "{name}: approval cards"
        );
        let logs = JValue::Arr(host.logs.lock().unwrap().clone());
        assert_eq!(
            text(&norm(&logs, &tree, &dir_s)),
            text(sc.get("logs")),
            "{name}: log lines"
        );
        let job = sc.get("job");
        match session.blocked() {
            Some(blocked) => {
                assert_eq!(job.get("status").as_str(), Some("failed"), "{name}");
                assert_eq!(
                    job.get("error").as_str(),
                    Some(blocked.message.as_str()),
                    "{name}"
                );
                assert_eq!(
                    text(&blocked.result),
                    text(job.get("result")),
                    "{name}: blocked result"
                );
                // The halt comes a moment after the refusal that reached the limit.
                let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
                while !host.halted.load(Ordering::SeqCst) && std::time::Instant::now() < deadline {
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
                assert!(
                    host.halted.load(Ordering::SeqCst),
                    "{name}: the agent was not stopped"
                );
            }
            None => {
                let result = job.get("result");
                let summary = norm(&session.summary(), &tree, &dir_s);
                let JValue::Obj(items) = &summary else {
                    panic!()
                };
                for (k, v) in items {
                    assert_eq!(text(v), text(result.get(k)), "{name}: summary {k}");
                }
                let agent = JValue::obj([
                    ("name", JValue::Str("qa-agent".into())),
                    ("version", JValue::Str("1".into())),
                ]);
                assert_eq!(
                    text(&session.meta(&agent, &JValue::Null)),
                    text(result.get("meta")),
                    "{name}: meta"
                );
            }
        }
    }
}
