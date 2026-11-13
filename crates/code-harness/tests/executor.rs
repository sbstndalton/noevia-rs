//! crates/code-harness executor: whole Code task runs core's own code-harness.cjs `start` made in
//! tests/fixtures/executor-node.v1.json (tools/gen-executor-fixtures.cjs: jobs.cjs, code-workspace,
//! the pin files and code-acp over TCP against a scripted fake sandbox, a stub egress), replayed
//! through the Rust executor on server-projects' task store, the code-workspace port and code-acp,
//! against the same fake sandbox: the journal file, the workspace record, the egress calls, the
//! approval cards, the log lines and every line the sandbox received must match.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]

use code_acp::Transport;
use code_harness::executor::{Approvals, Deps, Egress, Engine, Executor, Fail, Jobs, StartRequest};
use code_harness::session::Ended;
use code_workspace::{chown_tree, git::Git, Options as WsOptions, Workspaces};
use js_json::{wtf16, JValue};
use server_projects::task_store::{NewJob, Options as StoreOptions, Store};
use std::collections::VecDeque;
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::net::{Shutdown, TcpListener};
use std::path::Path;
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const FIXTURE: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/executor-node.v1.json"
);
const TASK: &str = "00000000-0000-4000-8000-0000000000bb";

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

struct StoreJobs(Store);

fn fail(r: server_projects::task_store::Refusal) -> Fail {
    Fail::new(r.status, r.message)
}

impl Jobs for StoreJobs {
    fn create(&self, project_id: Option<&str>, capabilities: &[String]) -> Result<String, Fail> {
        self.0
            .create(NewJob {
                kind: "code",
                project_id,
                parent_id: None,
                capabilities,
            })
            .map_err(fail)
    }
    fn append(&self, id: &str, kind: &str, data: JValue) -> Result<(), Fail> {
        self.0.append(id, kind, data).map(|_| ()).map_err(fail)
    }
    fn status(&self, id: &str) -> Option<String> {
        self.0
            .get(id)
            .ok()
            .flatten()
            .and_then(|j| j.status.as_str().map(str::to_string))
    }
}

struct StubEgress {
    calls: Mutex<Vec<JValue>>,
    revoke: JValue,
    fails: bool,
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
        if self.fails {
            return Err(Fail::new(None, "QA egress down"));
        }
        Ok(self.revoke.clone())
    }
    fn forget(&self, grant: &JValue, snapshot_id: &str) -> Result<JValue, Fail> {
        self.calls.lock().unwrap().push(JValue::obj([(
            "forget",
            JValue::Arr(vec![
                grant.get("id").clone(),
                JValue::Str(snapshot_id.into()),
            ]),
        )]));
        Ok(JValue::obj([("confirmed", JValue::Bool(true))]))
    }
}

struct Scripted {
    answers: Mutex<VecDeque<String>>,
    asked: Mutex<Vec<JValue>>,
}

impl Approvals for Scripted {
    fn ask(&self, request: &JValue, _ended: &Arc<Ended>) -> String {
        self.asked.lock().unwrap().push(request.clone());
        self.answers
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_else(|| "deny".into())
    }
}

/// The fake sandbox: plays the script on the first connection; the lines it received.
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
            } else if step.get("close").as_bool() == Some(true) {
                stream.shutdown(Shutdown::Both).unwrap();
            } else if step.get("expectClose").as_bool() == Some(true) {
                let mut rest = String::new();
                let n = reader.read_line(&mut rest).unwrap_or(0);
                assert_eq!(n, 0, "more after the expected close: {rest}");
            }
        }
        lines
    });
    (port, handle)
}

fn norm(v: &JValue, tree: &str, base: &str) -> JValue {
    let mut t = text(v).replace(tree, "<TREE>").replace(base, "<BASE>");
    for key in ["uid", "gid"] {
        let pat = format!("\"{key}\":");
        let mut out = String::new();
        let mut rest = t.as_str();
        while let Some(i) = rest.find(&pat) {
            out.push_str(&rest[..i + pat.len()]);
            rest = &rest[i + pat.len()..];
            let digits = rest.chars().take_while(char::is_ascii_digit).count();
            if digits > 0 {
                out.push_str(&format!("\"<{key}>\""));
                rest = &rest[digits..];
            }
        }
        out.push_str(rest);
        t = out;
    }
    wtf16::parse(&t).unwrap()
}

#[test]
fn every_node_task_run_replays() {
    let f = wtf16::parse(&fs::read_to_string(FIXTURE).unwrap()).unwrap();
    let JValue::Obj(all) = f.get("scenarios") else {
        panic!()
    };
    for (name, sc) in all {
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
        let egress_spec = sc.get("egress");
        let egress: Option<Arc<StubEgress>> = match egress_spec {
            JValue::Null => None,
            v => Some(Arc::new(StubEgress {
                calls: Mutex::new(Vec::new()),
                revoke: v.clone(),
                fails: v.as_str() == Some("fails"),
            })),
        };
        let strings = |k: &str| -> Vec<String> {
            match sc.get(k) {
                JValue::Arr(a) => a.iter().map(|x| x.as_str().unwrap().to_string()).collect(),
                _ => Vec::new(),
            }
        };
        let approvals = Arc::new(Scripted {
            answers: Mutex::new(strings("answers").into()),
            asked: Mutex::new(Vec::new()),
        });
        let logs = Arc::new(Mutex::new(Vec::<JValue>::new()));
        let JValue::Arr(script) = sc.get("script") else {
            panic!()
        };
        let (port, box_thread) = sandbox(script.clone());
        let sink = logs.clone();
        let jobs = Arc::new(StoreJobs(store));
        let executor = Executor::new(Deps {
            jobs: jobs.clone(),
            workspaces: workspaces.clone(),
            egress: egress.clone().map(|e| e as Arc<dyn Egress>),
            approvals: approvals.clone(),
            engine: Box::new(|| Engine {
                base_url: Some("http://qa-engine.invalid:8080/v1".into()),
                api_key: Some("qa-engine-key".into()),
                model: Some("qa-model".into()),
                context_tokens: Some(8192.0),
            }),
            transport: Transport::Socket(format!("127.0.0.1:{port}")),
            log: Box::new(move |e| sink.lock().unwrap().push(e)),
            now: Box::new(|| 5000.0),
            guard: Box::new(|| false),
            owner: None,
            base_path: f.get("env").get("PATH").as_str().map(str::to_string),
            base_lang: f.get("env").get("LANG").as_str().map(str::to_string),
        });
        let started = executor
            .start(StartRequest {
                project_id: Some("qa-project".into()),
                repo_path: repo.to_str().unwrap().to_string(),
                prompt: "Write NEW.md for the QA test.".into(),
                capabilities: strings("capabilities"),
                domains: strings("domains"),
                harness: "opencode".into(),
                model: None,
                sandbox_kind: "sandbox".into(),
                prompt_preparation: "direct".into(),
                context: String::new(),
                tenant_id: Some("qa-tenant".into()),
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
        let lines = box_thread.join().unwrap();
        let tree = started.get("workspace").as_str().unwrap().to_string();
        assert_eq!(
            text(&norm(&started, &tree, &b)),
            text(sc.get("started")),
            "{name}: started"
        );
        let journal: Vec<JValue> =
            fs::read_to_string(tenant.join("jobs").join(format!("{TASK}.jsonl")))
                .unwrap()
                .lines()
                .map(|l| norm(&wtf16::parse(l).unwrap(), &tree, &b))
                .collect();
        let JValue::Arr(want) = sc.get("journal") else {
            panic!()
        };
        if std::env::var("QA_SHOW").is_ok() {
            for r in &journal {
                eprintln!(
                    "{name} rust: {} {}",
                    text(r.get("seq")),
                    text(r.get("type"))
                );
            }
        }
        for (i, (got, want)) in journal.iter().zip(want.iter()).enumerate() {
            assert_eq!(text(got), text(want), "{name}: journal row {}", i + 1);
        }
        assert_eq!(journal.len(), want.len(), "{name}: journal length");
        let record = wtf16::parse(
            &fs::read_to_string(tenant.join("code-workspaces").join(format!("{TASK}.json")))
                .unwrap(),
        )
        .unwrap();
        assert_eq!(
            text(&norm(&record, &tree, &b)),
            text(sc.get("record")),
            "{name}: workspace record"
        );
        let calls = JValue::Arr(
            egress
                .map(|e| e.calls.lock().unwrap().clone())
                .unwrap_or_default(),
        );
        assert_eq!(
            text(&calls),
            text(sc.get("egressCalls")),
            "{name}: egress calls"
        );
        let asked = JValue::Arr(approvals.asked.lock().unwrap().clone());
        assert_eq!(
            text(&norm(&asked, &tree, &b)),
            text(sc.get("asked")),
            "{name}: approval cards"
        );
        let logged = JValue::Arr(logs.lock().unwrap().clone());
        assert_eq!(
            text(&norm(&logged, &tree, &b)),
            text(sc.get("logs")),
            "{name}: log lines"
        );
        let sent: Vec<JValue> = lines
            .iter()
            .map(|l| norm(&JValue::Str(l.clone()), &tree, &b))
            .collect();
        assert_eq!(
            text(&JValue::Arr(sent)),
            text(sc.get("sandboxLines")),
            "{name}: what the sandbox received"
        );
    }
}
