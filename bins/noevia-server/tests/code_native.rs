//! Track R7 P6: the Code task actions end to end, through the whole front (`noevia_server::serve`,
//! the router `App` mounts, `code_routes::serve` behind it). A synthetic core on 127.0.0.1 answers
//! the task facts bridge (`owner: 'front'`), `/internal/code/engine`, the inference admission lease,
//! the model facts and the budget check; a synthetic sandbox speaks ACP on 127.0.0.1 (the agent edits
//! and commits in the task's tree itself); a synthetic model endpoint answers the role engine with
//! SSE (the Planner's plan, the reviewer's verdict, the auditor's write-up, the answers core's own
//! recording used); a synthetic verifier answers `CODE_VERIFY_ENDPOINT`. Answers whose shape is
//! Node's are compared with the recordings in crates/code-harness/tests/fixtures (ids, commits and
//! clocks normalized). QA-named data only; no network beyond loopback.
//!
//! Hermeticity: the test's own git calls run with HOME and the global and system git config
//! pointed away. The front's git (code_workspace::git::Git) does not take its environment from
//! `Config`: it passes `GIT_ENV_KEYS` (HOME, PATH, LANG, …) of the test process's own environment,
//! as the server does in production. A test cannot redirect that without mutating the process
//! environment for every test in the binary, which it does not do; so the front's worktree and
//! branch commands see the host's `~/.gitconfig` and `/etc/gitconfig` (the builder and CI have
//! neither with anything that changes these commands' results).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]

use noevia_server::config::Config;
use noevia_server::serve::{serve, Limits};
use serde_json::{json, Value};
use server_store::rusqlite::Connection;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::time::Instant;

const SCHEMA: &str = include_str!("../../../crates/server-store/tests/fixtures/node-schema.sql");
const SERVICE: &str =
    include_str!("../../../crates/code-harness/tests/fixtures/service-node.v1.json");
const PIPELINE: &str =
    include_str!("../../../crates/code-harness/tests/fixtures/pipeline-service-node.v1.json");
const GATE: &str =
    include_str!("../../../crates/code-harness/tests/fixtures/review-gate-node.v1.json");
const STORE_TOKEN: &str = "synthetic-store-token-0123456789abcdef";
const START: &str = "0123456789abcdef0123456789abcdef";
const OWNER: &str = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa";
const MEMBER: &str = "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb";
const OTHER: &str = "cccccccc-cccc-4ccc-8ccc-cccccccccccc";
const PROJECT: &str = "qa-project";
/// The recordings' first task id (their ids are counted, ours random).
const T1: &str = "00000000-0000-4000-8000-000000000001";
const DEADLINE: Duration = Duration::from_secs(45);
const LEASE_PATH: &str = "/internal/inference-admission/lease/";

fn fixture(text: &str) -> Value {
    serde_json::from_str(text).unwrap()
}

fn hex(s: &str) -> String {
    Sha256::digest(s.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn cookie(user: &str) -> String {
    format!("cowork_session=qa-token-{user}; cowork_csrf=qa-csrf")
}

fn git(cwd: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .env("HOME", "/nonexistent")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_AUTHOR_NAME", "QA")
        .env("GIT_AUTHOR_EMAIL", "qa@example.invalid")
        .env("GIT_COMMITTER_NAME", "QA")
        .env("GIT_COMMITTER_EMAIL", "qa@example.invalid")
        .output()
        .unwrap();
    assert!(out.status.success(), "git {args:?}");
    String::from_utf8(out.stdout).unwrap().trim().to_string()
}

// ── a small HTTP/1.1 stub server ────────────────────────────────────────────────────────────

/// One request: method, path, body.
type Seen = Arc<Mutex<Vec<(String, String, Value)>>>;
type Handler = Arc<dyn Fn(&str, &str, &Value) -> (u16, &'static str, String) + Send + Sync>;

async fn read_request(c: &mut TcpStream) -> Option<(String, String, Vec<u8>)> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 8192];
    loop {
        let n = c.read(&mut chunk).await.ok()?;
        if n == 0 {
            return None;
        }
        buf.extend_from_slice(&chunk[..n]);
        let Some(end) = buf.windows(4).position(|w| w == b"\r\n\r\n") else {
            continue;
        };
        let head = String::from_utf8_lossy(&buf[..end]).to_string();
        let want = head
            .lines()
            .find_map(|l| {
                l.to_ascii_lowercase()
                    .strip_prefix("content-length:")
                    .map(|v| v.trim().parse::<usize>().unwrap())
            })
            .unwrap_or(0);
        if buf.len() >= end + 4 + want {
            let mut first = head.lines().next()?.split(' ');
            let (m, p) = (first.next()?.to_string(), first.next()?.to_string());
            return Some((m, p, buf[end + 4..end + 4 + want].to_vec()));
        }
    }
}

async fn stub(seen: Seen, handler: Handler) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        while let Ok((mut c, _)) = listener.accept().await {
            let (seen, handler) = (Arc::clone(&seen), Arc::clone(&handler));
            tokio::spawn(async move {
                let Some((method, path, body)) = read_request(&mut c).await else {
                    return;
                };
                let body: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
                let (status, ctype, text) = handler(&method, &path, &body);
                seen.lock().unwrap().push((method, path, body));
                let out = format!("HTTP/1.1 {status} X\r\ncontent-type: {ctype}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{text}", text.len());
                let _ = c.write_all(out.as_bytes()).await;
            });
        }
    });
    addr
}

// ── the synthetic core ──────────────────────────────────────────────────────────────────────

/// What core answers: the engine facts' `role` (core gives it only with the pipeline or the
/// review on) and the leases it granted and got back.
#[derive(Default)]
struct CoreState {
    role: Option<Value>,
    leases: Vec<String>,
    released: Vec<String>,
}

fn core_handler(state: Arc<Mutex<CoreState>>) -> Handler {
    Arc::new(move |method, path, body| {
        let ok = |v: Value| (200, "application/json", v.to_string());
        let mut st = state.lock().unwrap();
        match path {
            "/internal/tasks/facts" => ok(json!({"enabled": true, "owner": "front"})),
            "/internal/code/engine" => {
                if body["projectId"] != PROJECT {
                    return (
                        404,
                        "application/json",
                        json!({"error": "project not found"}).to_string(),
                    );
                }
                let mut facts = json!({
                    "engine": {"baseUrl": "http://qa-engine.invalid:8080/v1", "apiKey": null, "model": "qa-model", "contextTokens": 8192},
                    "machine": false, "paused": false, "context": "",
                });
                if let Some(role) = &st.role {
                    facts["role"] = role.clone();
                }
                ok(facts)
            }
            "/internal/models/facts" => ok(json!({"sidecarNames": []})),
            "/internal/model-maintenance/load-refusal" => ok(json!({"refusal": null})),
            p if p.starts_with(LEASE_PATH) => {
                let id = p[LEASE_PATH.len()..].split('/').next().unwrap().to_string();
                match method {
                    "PUT" => {
                        st.leases.push(id.clone());
                        (
                            201,
                            "application/json",
                            json!({"lease": id, "ttlMs": 30_000, "headers": {}}).to_string(),
                        )
                    }
                    "DELETE" => {
                        st.released.push(id);
                        (204, "application/json", String::new())
                    }
                    _ => ok(json!({})),
                }
            }
            // Anything else is a request the front relayed to Node.
            p => (202, "application/json", json!({"relayed": p}).to_string()),
        }
    })
}

// ── the synthetic model endpoint (the role engine's) ───────────────────────────────────────

fn sse(answer: &Value) -> String {
    let delta = json!({"choices": [{"delta": {"content": answer.to_string()}}]});
    let stop = json!({"choices": [{"delta": {}, "finish_reason": "stop"}]});
    format!("data: {delta}\n\ndata: {stop}\n\ndata: [DONE]\n\n")
}

/// The roles as core's recording answered them (`standins.answers`), by the instructions sent.
fn model_handler(roles: Arc<Mutex<Vec<&'static str>>>) -> Handler {
    let answers = fixture(PIPELINE)["standins"]["answers"].clone();
    Arc::new(move |method, path, body| {
        if method == "GET" && path == "/models" {
            let list = json!({"data": [{"id": "qa-model", "status": {"value": "loaded"}}]});
            return (200, "application/json", list.to_string());
        }
        assert_eq!(path, "/v1/chat/completions");
        let text = body.to_string();
        let role = if text.contains("You are the Auditor.") {
            "auditor"
        } else if text.contains("reviewing a finished code change") {
            "reviewer"
        } else if text.contains("Write a bounded execution plan") {
            "planner"
        } else {
            panic!("an unknown role call: {text}")
        };
        roles.lock().unwrap().push(role);
        (200, "text/event-stream", sse(&answers[role]))
    })
}

// ── the synthetic verifier ──────────────────────────────────────────────────────────────────

async fn verifier(asked: Seen) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        while let Ok((c, _)) = listener.accept().await {
            let asked = Arc::clone(&asked);
            tokio::spawn(async move {
                let (r, mut w) = c.into_split();
                let mut line = String::new();
                if BufReader::new(r).read_line(&mut line).await.unwrap_or(0) == 0 {
                    return;
                }
                let q: Value = serde_json::from_str(&line).unwrap();
                let a = json!({"noevia": "verify-result", "nonce": q["nonce"], "ok": true, "headSha": q["headSha"],
                    "exitCode": 0, "signal": null, "timedOut": false, "tail": "QA ok", "totalBytes": 5});
                asked
                    .lock()
                    .unwrap()
                    .push(("verify".into(), String::new(), q));
                let _ = w.write_all(format!("{a}\n").as_bytes()).await;
                let _ = w.shutdown().await;
            });
        }
    });
    addr
}

// ── the synthetic sandbox (ACP over the socket) ─────────────────────────────────────────────

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Agent {
    /// Edits src/qa.js in its tree, commits it and ends the turn.
    Commit,
    /// Asks the recording's `npm test` permission and waits until the front hangs up.
    Ask,
}

#[derive(Default)]
struct Sandbox {
    prompts: usize,
    /// Connections accepted and not yet closed.
    open: usize,
}

fn rpc(id: &Value, result: Value) -> String {
    format!(
        "{}\n",
        json!({"jsonrpc": "2.0", "id": id, "result": result})
    )
}

async fn sandbox(mode: Arc<Mutex<Agent>>, state: Arc<Mutex<Sandbox>>) -> SocketAddr {
    let ask = fixture(SERVICE)["scenarios"]["cancel-while-waiting"]["script"]
        .as_array()
        .unwrap()
        .iter()
        .find_map(|s| {
            s["send"]
                .as_str()
                .filter(|l| l.contains("request_permission"))
        })
        .unwrap()
        .to_string();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        while let Ok((c, _)) = listener.accept().await {
            state.lock().unwrap().open += 1;
            let (mode, state, ask) = (Arc::clone(&mode), Arc::clone(&state), ask.clone());
            tokio::spawn(async move {
                let (r, mut w) = c.into_split();
                let mut lines = BufReader::new(r).lines();
                let mut tree = String::new();
                while let Ok(Some(line)) = lines.next_line().await {
                    let m: Value = serde_json::from_str(&line).unwrap_or(Value::Null);
                    let out = match m["method"].as_str() {
                        Some("initialize") => rpc(
                            &m["id"],
                            json!({"protocolVersion": 1, "agentInfo": {"name": "qa-agent", "version": "0.1"}}),
                        ),
                        Some("session/new") => {
                            tree = m["params"]["cwd"].as_str().unwrap().to_string();
                            rpc(&m["id"], json!({"sessionId": "qa-session"}))
                        }
                        Some("session/prompt") => {
                            state.lock().unwrap().prompts += 1;
                            let mode = *mode.lock().unwrap();
                            if mode == Agent::Ask {
                                ask.clone()
                            } else {
                                let t = PathBuf::from(&tree);
                                tokio::task::spawn_blocking(move || {
                                    std::fs::write(
                                        t.join("src/qa.js"),
                                        "console.log('QA greeting');\n",
                                    )
                                    .unwrap();
                                    git(&t, &["add", "src/qa.js"]);
                                    git(&t, &["commit", "-qm", "QA greeting"]);
                                })
                                .await
                                .unwrap();
                                rpc(&m["id"], json!({"stopReason": "end_turn"}))
                            }
                        }
                        _ => continue,
                    };
                    if w.write_all(out.as_bytes()).await.is_err() {
                        break;
                    }
                }
                state.lock().unwrap().open -= 1;
            });
        }
    });
    addr
}

// ── the front ───────────────────────────────────────────────────────────────────────────────

#[derive(Clone, Copy, Default)]
struct Opts {
    executor: bool,
    pipeline: bool,
    review: bool,
    /// Track R11: `NOEVIA_RUST_ENGINE_FACTS`.
    facts: Option<&'static str>,
    /// The registry puts the Code role on an inference machine.
    code_machine: bool,
}

struct Rig {
    _dir: tempfile::TempDir,
    data: PathBuf,
    repo: PathBuf,
    addr: SocketAddr,
    core: Arc<Mutex<CoreState>>,
    core_seen: Seen,
    roles: Arc<Mutex<Vec<&'static str>>>,
    verified: Seen,
    agent: Arc<Mutex<Agent>>,
    sandbox: Arc<Mutex<Sandbox>>,
}

fn seed(data: &Path) {
    let db = Connection::open(server_store::db_path(data)).unwrap();
    db.execute_batch(SCHEMA).unwrap();
    let now = noevia_server::identity::now_ms();
    for (uid, name, role) in [
        (OWNER, "QA Owner", "admin"),
        (MEMBER, "QA Member", "member"),
        (OTHER, "QA Other", "admin"),
    ] {
        db.execute(
            "INSERT INTO users(id,username,username_norm,display_name,role,password_hash,webauthn_user_id,created_at,updated_at) VALUES(?1,?2,lower(?2),?2,?3,'x',?1,1,1)",
            (uid, name, role),
        )
        .unwrap();
        db.execute(
            "INSERT INTO sessions VALUES(?1,?2,?3,?4,?4,?5,'ua','127.0.0.1')",
            (
                hex(&format!("qa-token-{uid}")),
                uid,
                hex("qa-csrf"),
                now,
                now + 86_400_000,
            ),
        )
        .unwrap();
        // Every account has a project of the same id: the tenants differ, not the names.
        let home = data.join("users").join(uid);
        std::fs::create_dir_all(&home).unwrap();
        std::fs::write(
            home.join("projects.json"),
            format!(r#"{{"projects":[{{"id":"{PROJECT}","name":"QA project","goal":"g","instructions":"i","pinned":false,"archived":false,"sourceFolders":[],"memories":[],"files":[],"routing":"auto","modes":["chat","code"],"toolboxes":["core"],"toolsMode":"auto","chats":[],"createdAt":1,"updatedAt":2}}]}}"#),
        )
        .unwrap();
    }
}

async fn rig(o: Opts) -> Rig {
    // The front's admission authority issues leases under a Node start.
    let _ = engine_admission::authority::global().node_session("qa-code-native-boot", 1);
    let dir = tempfile::tempdir().unwrap();
    let base = std::fs::canonicalize(dir.path()).unwrap();
    let data = base.join("data");
    std::fs::create_dir_all(&data).unwrap();
    seed(&data);
    if o.code_machine {
        Connection::open(server_store::db_path(&data))
            .unwrap()
            .execute(
                "INSERT OR REPLACE INTO settings(key,value) VALUES('inference:machines',?1)",
                [r#"{"version":1,"machines":[{"id":"qa-m","name":"QA machine","address":"http://qa-machine.invalid"}],"assignments":{"chatCode":{"machine":"qa-m","port":8080,"model":"qa-code"}}}"#],
            )
            .unwrap();
    }
    let repo = base.join("qa-repo");
    std::fs::create_dir_all(repo.join("src")).unwrap();
    git(&repo, &["init", "-q", "--template=", "-b", "main"]);
    std::fs::write(repo.join("README.md"), "The QA project.").unwrap();
    std::fs::write(repo.join("src/qa.js"), "console.log('QA');\n").unwrap();
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-qm", "QA first"]);

    let core = Arc::new(Mutex::new(CoreState::default()));
    if o.pipeline || o.review {
        core.lock().unwrap().role = Some(
            json!({"baseUrl": null, "apiKey": null, "model": "qa-model", "external": false, "jsonSchema": false}),
        );
    }
    let core_seen: Seen = Arc::default();
    let core_addr = stub(Arc::clone(&core_seen), core_handler(Arc::clone(&core))).await;
    let roles = Arc::default();
    let model_addr = stub(Arc::default(), model_handler(Arc::clone(&roles))).await;
    if let Some(role) = core.lock().unwrap().role.as_mut() {
        role["baseUrl"] = json!(format!("http://{model_addr}/v1"));
    }
    let verified: Seen = Arc::default();
    let verify_addr = verifier(Arc::clone(&verified)).await;
    let (agent, sandbox_state) = (Arc::new(Mutex::new(Agent::Commit)), Arc::default());
    let sandbox_addr = sandbox(Arc::clone(&agent), Arc::clone(&sandbox_state)).await;

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let mut env: HashMap<&str, String> = HashMap::from([
        ("NOEVIA_LEGACY_UPSTREAM", format!("http://{core_addr}")),
        ("UI_PORT", "1".into()),
        ("UI_DATA_DIR", data.display().to_string()),
        ("NOEVIA_WEB_DIST", data.join("dist").display().to_string()),
        ("PUBLIC_ORIGIN", format!("http://{addr}")),
        ("NOEVIA_START_ID", START.into()),
        ("NOEVIA_PROJECT_STORE_TOKEN", STORE_TOKEN.into()),
        ("PATH", std::env::var("PATH").unwrap_or_default()),
        ("HOME", base.display().to_string()),
        ("CODE_REPOS", format!("qa-repo|{}", repo.display())),
        ("CODE_HARNESS_ENDPOINT", sandbox_addr.to_string()),
        ("CODE_HARNESS_VERSION", "0.0.0-qa".into()),
        ("CODE_VERIFY_ENDPOINT", verify_addr.to_string()),
    ]);
    if o.executor {
        env.insert("NOEVIA_CODE_EXECUTOR", "rust".into());
    }
    if o.pipeline {
        env.insert("NOEVIA_FEATURE_CODE_HARNESS", "true".into());
        env.insert("NOEVIA_FEATURE_CODE_PIPELINE", "true".into());
    }
    if o.review {
        env.insert("NOEVIA_FEATURE_PLANNER_REVIEW", "true".into());
    }
    if let Some(mode) = o.facts {
        env.insert("NOEVIA_RUST_ENGINE_FACTS", mode.into());
    }
    let app = noevia_server::App::new(Config::from_lookup(|k| env.get(k).cloned()).unwrap());
    tokio::spawn(serve(
        listener,
        app,
        Limits::default(),
        std::future::pending(),
    ));
    Rig {
        _dir: dir,
        data,
        repo,
        addr,
        core,
        core_seen,
        roles,
        verified,
        agent,
        sandbox: sandbox_state,
    }
}

fn dechunk(mut rest: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    while let Some(at) = rest.windows(2).position(|w| w == b"\r\n") {
        let n = usize::from_str_radix(String::from_utf8_lossy(&rest[..at]).trim(), 16).unwrap_or(0);
        if n == 0 {
            break;
        }
        out.extend_from_slice(&rest[at + 2..at + 2 + n]);
        rest = &rest[at + 4 + n..];
    }
    out
}

impl Rig {
    /// One request as `user` (with the CSRF pair), answered in full.
    async fn call(
        &self,
        user: &str,
        method: &str,
        path: &str,
        body: Option<Value>,
    ) -> (u16, Value) {
        let body = body.map(|b| b.to_string()).unwrap_or_default();
        let mut s = TcpStream::connect(self.addr).await.unwrap();
        let req = format!(
            "{method} {path} HTTP/1.1\r\nHost: {a}\r\nConnection: close\r\nCookie: {c}\r\nOrigin: http://{a}\r\nX-CSRF-Token: qa-csrf\r\nContent-Type: application/json\r\nContent-Length: {n}\r\n\r\n{body}",
            a = self.addr,
            c = cookie(user),
            n = body.len()
        );
        s.write_all(req.as_bytes()).await.unwrap();
        let mut out = Vec::new();
        let _ = s.read_to_end(&mut out).await;
        let end = out.windows(4).position(|w| w == b"\r\n\r\n").unwrap();
        let head = String::from_utf8_lossy(&out[..end]).to_ascii_lowercase();
        let status = head[9..12].parse().unwrap();
        let raw = &out[end + 4..];
        let raw = if head.contains("transfer-encoding: chunked") {
            dechunk(raw)
        } else {
            raw.to_vec()
        };
        (status, serde_json::from_slice(&raw).unwrap_or(Value::Null))
    }

    fn code(&self, tail: &str) -> String {
        format!("/api/projects/{PROJECT}/code{tail}")
    }

    async fn start(&self, body: Value) -> (u16, Value) {
        self.call(OWNER, "POST", &self.code(""), Some(body)).await
    }

    async fn view(&self, user: &str, task: &str) -> (u16, Value) {
        self.call(user, "GET", &self.code(&format!("/{task}")), None)
            .await
    }

    /// Polls the owner's view of `task` until `done` holds, within the deadline.
    async fn until(&self, task: &str, what: &str, done: impl Fn(&Value) -> bool) -> Value {
        let deadline = Instant::now() + DEADLINE;
        loop {
            let (status, v) = self.view(OWNER, task).await;
            if status == 200 && done(&v) {
                return v;
            }
            assert!(
                Instant::now() < deadline,
                "{what}: never came; last {status} {v}"
            );
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }

    async fn card(&self, task: &str, action: &str) -> Value {
        self.until(task, action, |v| v["approval"]["action"] == action)
            .await["approval"]
            .clone()
    }

    async fn ended(&self, task: &str) -> Value {
        self.until(task, "the end", |v| {
            matches!(
                v["status"].as_str(),
                Some("completed" | "failed" | "cancelled" | "interrupted")
            )
        })
        .await
    }

    fn journal(&self, task: &str) -> Vec<Value> {
        let file = self
            .data
            .join("users")
            .join(OWNER)
            .join("jobs")
            .join(format!("{task}.jsonl"));
        std::fs::read_to_string(file)
            .unwrap()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect()
    }

    fn last(&self, task: &str, kind: &str) -> Value {
        self.journal(task)
            .into_iter()
            .rev()
            .find(|r| r["type"] == kind)
            .unwrap()["data"]
            .clone()
    }

    fn base_sha(&self) -> String {
        git(&self.repo, &["rev-parse", "main"])
    }

    /// Every lease the tasks took was given back, and every sandbox connection closed.
    async fn quiet(&self) {
        let deadline = Instant::now() + DEADLINE;
        loop {
            let (leases, released) = {
                let c = self.core.lock().unwrap();
                (c.leases.clone(), c.released.clone())
            };
            let open = self.sandbox.lock().unwrap().open;
            if open == 0 && leases.iter().all(|l| released.contains(l)) {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "leases {leases:?} released {released:?}, {open} sandbox connection(s) open"
            );
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }

    fn engine_asked(&self) -> usize {
        self.core_seen
            .lock()
            .unwrap()
            .iter()
            .filter(|(_, p, _)| p == "/internal/code/engine")
            .count()
    }
}

/// A Node answer made comparable: `pairs` replaced throughout (longest first), every clock 5000.
fn norm(v: &Value, pairs: &[(&str, &str)]) -> Value {
    let mut text = v.to_string();
    for (from, to) in pairs {
        text = text.replace(from, to);
    }
    let mut v: Value = serde_json::from_str(&text).unwrap();
    fn clocks(v: &mut Value) {
        match v {
            Value::Object(m) => {
                for (k, x) in m.iter_mut() {
                    if matches!(k.as_str(), "at" | "createdAt" | "updatedAt") && x.is_number() {
                        *x = json!(5000);
                    } else {
                        clocks(x);
                    }
                }
            }
            Value::Array(a) => a.iter_mut().for_each(clocks),
            _ => {}
        }
    }
    clocks(&mut v);
    v
}

fn keys(v: &Value) -> Vec<String> {
    let mut k: Vec<String> = v.as_object().unwrap().keys().cloned().collect();
    k.sort();
    k
}

async fn within(f: impl std::future::Future<Output = ()>) {
    tokio::time::timeout(Duration::from_secs(150), f)
        .await
        .expect("the test ran past its deadline");
}

const RUN: &str = "QA: run the tests.";
const GREET: &str = "QA: add the greeting.";

// ── 1. switch unset ─────────────────────────────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn switch_unset_the_actions_are_relayed_to_core_as_before() {
    within(async {
        let r = rig(Opts::default()).await;
        let task = "0123abcd-0123-4abc-8def-0123456789ab";
        let start = json!({"repository": "qa-repo", "prompt": RUN});
        let decide = json!({"decision": "approve", "approvalId": "qa-card"});
        for (path, body) in [
            (r.code(""), Some(start.clone())),
            (r.code(&format!("/{task}/approve")), Some(decide.clone())),
            (r.code(&format!("/{task}/cancel")), None),
        ] {
            let (status, answer) = r.call(OWNER, "POST", &path, body.clone()).await;
            assert_eq!(
                (status, &answer),
                (202, &json!({"relayed": path})),
                "{path}"
            );
            let relayed: Vec<Value> = r
                .core_seen
                .lock()
                .unwrap()
                .iter()
                .filter(|(m, p, _)| m == "POST" && *p == path)
                .map(|(_, _, b)| b.clone())
                .collect();
            assert_eq!(
                relayed,
                vec![body.unwrap_or(Value::Null)],
                "{path}: relayed once, the body as sent"
            );
        }
        // Nothing was answered natively: no facts, no engine, no lease, no sandbox.
        let seen = r.core_seen.lock().unwrap().clone();
        let native = |p: &str| {
            p.starts_with("/internal/code/")
                || p == "/internal/tasks/facts"
                || p.starts_with(LEASE_PATH)
        };
        assert!(seen.iter().all(|(_, p, _)| !native(p)), "{seen:?}");
        assert_eq!(r.sandbox.lock().unwrap().prompts, 0);
        assert!(!r.data.join("users").join(OWNER).join("jobs").exists());
    })
    .await;
}

// ── 2. switch on, a direct task, two more tenants ───────────────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_direct_task_runs_cancels_and_stays_in_its_tenant() {
    within(async {
        let r = rig(Opts {
            executor: true,
            ..Opts::default()
        })
        .await;
        *r.agent.lock().unwrap() = Agent::Ask;
        let recorded = &fixture(SERVICE)["scenarios"]["cancel-while-waiting"];
        let answers = recorded["answers"].as_array().unwrap();

        // The panel's facts are the front's, in Node's shape. The recording is code-service.cjs
        // `facts()`, the loopback bridge's answer, which names the waiting cards (`approvals`).
        // That map never reached the browser: Node's own GET /api/projects/{id}/code (routes/
        // code.cjs before noevia-core 38084d2 moved the reads to the front) answered
        // `{ repositories, capabilities, defaultCapabilities, harnesses, promptPreparation,
        // sandboxed, network, networkState, tasks: service.list(...) }`, each card inside its task
        // (`view(job, pendingFor(id))`); task_routes.rs `state` answers the same.
        let (status, state) = r.call(OWNER, "GET", &r.code(""), None).await;
        assert_eq!(status, 200);
        let mut node = answers[6]["body"].clone();
        assert_eq!(
            node["approvals"],
            json!({}),
            "the bridge's facts name no card yet"
        );
        node.as_object_mut().unwrap().remove("approvals");
        node["tasks"] = json!([]);
        assert_eq!(state, node, "the facts as Node's GET answered them");

        let (status, started) = r
            .start(json!({"repository": "qa-repo", "prompt": RUN}))
            .await;
        assert_eq!(status, 202, "{started}");
        let task = started["taskId"].as_str().unwrap().to_string();
        let workspace = r
            .data
            .join("users")
            .join(OWNER)
            .join("code-workspaces")
            .join(&task);
        assert_eq!(started["workspace"], json!(workspace.display().to_string()));
        let tenant = r.data.join("users").join(OWNER).display().to_string();
        let pairs = [(tenant.as_str(), "<BASE>/tenant"), (task.as_str(), T1)];
        assert_eq!(
            norm(&started, &pairs),
            answers[0]["body"],
            "the start answer as Node's"
        );

        // The harness turn runs: the agent asked, and the card waits for the owner.
        let card = r.card(&task, "execute_command").await;
        assert_eq!(r.sandbox.lock().unwrap().prompts, 1);
        assert_eq!(r.engine_asked(), 1, "one engine-facts call per start");
        let card_id = card["id"].as_str().unwrap().to_string();

        // The owner's panel holds the task with its card, and no `approvals` map beside it.
        let (status, mine) = r.call(OWNER, "GET", &r.code(""), None).await;
        assert_eq!(status, 200);
        assert!(mine.get("approvals").is_none(), "{mine}");
        assert_eq!(mine["tasks"].as_array().unwrap().len(), 1, "{mine}");
        assert_eq!(mine["tasks"][0]["id"], json!(task));
        assert_eq!(mine["tasks"][0]["approval"], card);

        // The member is refused before anything (the reads too); the other administrator finds nothing.
        let decide = json!({"decision": "approve", "approvalId": card_id});
        let other_task = r.code(&format!("/{task}"));
        for (method, tail, body) in [
            ("GET", String::new(), None),
            ("GET", format!("/{task}"), None),
            (
                "POST",
                String::new(),
                Some(json!({"repository": "qa-repo", "prompt": RUN})),
            ),
            ("POST", format!("/{task}/approve"), Some(decide.clone())),
            ("POST", format!("/{task}/cancel"), None),
        ] {
            let (status, answer) = r.call(MEMBER, method, &r.code(&tail), body).await;
            assert_eq!(
                (status, answer),
                (403, json!({"error": "Administrator required"})),
                "{tail}"
            );
        }
        for tail in [format!("/{task}/approve"), format!("/{task}/cancel")] {
            let body = tail.ends_with("approve").then(|| decide.clone());
            let (status, answer) = r.call(OTHER, "POST", &r.code(&tail), body).await;
            assert_eq!(
                (status, answer),
                (404, json!({"error": "Task not found"})),
                "{tail}"
            );
        }
        assert_eq!(
            r.call(OTHER, "GET", &other_task, None).await,
            (404, json!({"error": "Task not found"}))
        );
        // Their panel is the empty one: no task, and no card of the owner's anywhere in it.
        let (status, theirs) = r.call(OTHER, "GET", &r.code(""), None).await;
        assert_eq!(
            (status, &theirs),
            (200, &state),
            "no task or card of the owner's"
        );
        assert!(!theirs.to_string().contains(&card_id), "{theirs}");
        assert_eq!(
            r.view(OWNER, &task).await.1["approval"]["id"],
            json!(card_id),
            "the card still waits"
        );

        // The cancel answers the view from before it took effect (D9), as Node's.
        let (status, view) = r
            .call(OWNER, "POST", &r.code(&format!("/{task}/cancel")), None)
            .await;
        let sha = r.base_sha();
        let pairs = [
            (task.as_str(), T1),
            (sha.as_str(), "f0505304b2a32e3f95f60a9b0db10997573ab0a1"),
        ];
        assert_eq!(
            (status, norm(&view, &pairs)),
            (200, answers[2]["body"].clone())
        );
        assert_eq!(r.ended(&task).await["status"], "cancelled");
        let (status, late) = r
            .call(
                OWNER,
                "POST",
                &r.code(&format!("/{task}/approve")),
                Some(decide),
            )
            .await;
        assert_eq!((status, late), (409, answers[4]["body"].clone()));
        let (status, view) = r
            .call(OWNER, "POST", &r.code(&format!("/{task}/cancel")), None)
            .await;
        assert_eq!(
            (status, norm(&view, &pairs)),
            (200, answers[5]["body"].clone()),
            "cancelled, as Node's"
        );
        r.quiet().await;
        assert_eq!(
            r.core.lock().unwrap().leases.len(),
            1,
            "the run held one lease"
        );
    })
    .await;
}

// ── 3. the Planner pipeline ─────────────────────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_planner_plans_runs_verifies_reviews_and_is_accepted_or_cancelled() {
    within(async {
        let r = rig(Opts { executor: true, pipeline: true, ..Opts::default() }).await;
        let recorded = fixture(PIPELINE);
        let flow = &recorded["scenarios"]["planner-flow"];
        let answers = flow["answers"].as_array().unwrap();
        let journal = flow["journals"][T1].as_array().unwrap();

        // The Planner preparation is offered, exactly as Node lists it.
        let (status, state) = r.call(OWNER, "GET", &r.code(""), None).await;
        assert_eq!(status, 200);
        assert_eq!(state["promptPreparation"], answers[0]["body"]["promptPreparation"]);

        let (status, started) = r
            .start(json!({"repository": "qa-repo", "prompt": GREET, "promptPreparation": "planner", "approvePlan": true}))
            .await;
        assert_eq!(status, 202, "{started}");
        let task = started["taskId"].as_str().unwrap().to_string();
        let branch = started["branch"].as_str().unwrap().to_string();
        let base = r.base_sha();
        // The recording's tree and branch are its stand-ins' (`/qa/work/qa-task`, `noevia/qa-task`).
        let tree = started["workspace"].as_str().unwrap().to_string();
        let pairs = [(tree.as_str(), "/qa/work/qa-task"), (branch.as_str(), "noevia/qa-task"), (task.as_str(), T1)];
        assert_eq!(norm(&started, &pairs), answers[1]["body"], "the start answer as Node's");

        // approve_plan: Node's card (the same plan hashes the same).
        let card = r.card(&task, "approve_plan").await;
        let card_id = card["id"].as_str().unwrap().to_string();
        let node_card = &answers[3]["body"]["approvals"][T1];
        let pairs = [(card_id.as_str(), "00000000-0000-4000-8000-000000000002"), (task.as_str(), T1)];
        assert_eq!(&norm(&card, &pairs), node_card);
        assert_eq!(*r.roles.lock().unwrap(), vec!["planner"]);
        let decide = |id: &str| json!({"decision": "approve", "approvalId": id});
        let approve = r.code(&format!("/{task}/approve"));
        let (status, ok) = r.call(OWNER, "POST", &approve, Some(decide(&card_id))).await;
        assert_eq!((status, &ok), (200, &answers[4]["body"]));

        // The agent commits; the verifier measures that commit; the Planner reviews; the auditor writes up.
        let card = r.card(&task, "review_change").await;
        let head = git(&r.repo, &["rev-parse", &branch]);
        assert_ne!(head, base);
        assert_eq!(r.sandbox.lock().unwrap().prompts, 1);
        let asked = r.verified.lock().unwrap().clone();
        assert_eq!(asked.len(), 1);
        assert_eq!((asked[0].2["repo"].clone(), asked[0].2["headSha"].clone()), (json!("qa-repo"), json!(head)));
        assert_eq!(*r.roles.lock().unwrap(), vec!["planner", "reviewer", "auditor"]);
        let pairs = [
            (branch.as_str(), "noevia/qa-task"),
            (task.as_str(), T1),
            (base.as_str(), "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"),
            (head.as_str(), "1111111111111111111111111111111111111111"),
        ];
        // Measured, not recorded: the verifier's clock (`durationMs`), and so the completeness
        // report's hash over the test report. The report (completeness-report.cjs over the job's
        // steps, artifacts, plan and checkpoint) is built from the journal up to the `reviewing`
        // stage, which carries its hash: its inputs must be Node's (the clock normalized), and the
        // card must carry the hash of exactly that report. Two differences are the recording's
        // stand-ins, not the port's: the stand-in harness writes no `harness.config` step and one
        // checkpoint where code-harness.cjs writes its start and identity checkpoints (so the last
        // checkpoint is compared, not every one), and the stand-in verifier no `measuredBy`
        // (code-verify.cjs adds 'verifier').
        let ours_rows = r.journal(&task);
        let gate = |rows: &[Value]| rows.iter().position(|j| j["type"] == "task.stage" && j["data"]["to"] == "reviewing").unwrap();
        let (mine, theirs) = (gate(&ours_rows), gate(journal.as_slice()));
        let inputs = |rows: &[Value]| -> Vec<Value> {
            rows.iter()
                .filter(|j| {
                    let t = j["type"].as_str().unwrap_or("");
                    (t.starts_with("step.") && j["data"]["id"] != "harness.config")
                        || matches!(t, "artifact.created" | "task.stage" | "task.revision")
                })
                .map(|j| {
                    let mut d = j["data"].clone();
                    if d["kind"] == "test-report" {
                        assert!(d["durationMs"].is_u64(), "{d}");
                        d["durationMs"] = json!(7);
                        d.as_object_mut().unwrap().remove("measuredBy");
                    }
                    d.as_object_mut().unwrap().remove("reportHash");
                    json!([j["type"], d])
                })
                .collect()
        };
        let window: Vec<Value> = ours_rows[..=mine].iter().map(|j| norm(j, &pairs)).collect();
        assert_eq!(inputs(&window), inputs(&journal[..=theirs]), "what the report is built from, as Node's");
        // The checkpoint the report reads is the job's last: its task, branch and commits as Node's.
        let last_checkpoint = |rows: &[Value]| {
            let c = &rows.iter().rev().find(|j| j["type"] == "checkpoint.created").unwrap()["data"];
            json!([c["task"], c["branch"], c["baseSha"], c["headSha"]])
        };
        assert_eq!(last_checkpoint(&window), last_checkpoint(&journal[..=theirs]), "the checkpoint, as Node's");
        let measured = ours_rows.iter().find(|j| j["data"]["kind"] == "test-report").unwrap();
        assert_eq!(measured["data"]["measuredBy"], "verifier", "code-verify.cjs's mark");
        let hash = ours_rows[mine]["data"]["reportHash"].clone();
        assert!(hash.as_str().is_some_and(|h| h.len() == 64 && h.bytes().all(|b| b.is_ascii_hexdigit())), "{hash}");
        // The hash is the report of exactly the journal compared above: the front's own lines up
        // to the gate, replayed into a fresh store (whose report hash replays Node's in
        // server-projects' task_store fixtures), must hash the same at the same stage request.
        let replayed = {
            let text = std::fs::read_to_string(r.data.join("users").join(OWNER).join("jobs").join(format!("{task}.jsonl"))).unwrap();
            let dir = tempfile::tempdir().unwrap();
            std::fs::create_dir_all(dir.path().join("jobs")).unwrap();
            let lines: String = text.split_inclusive('\n').take(mine).collect();
            std::fs::write(dir.path().join("jobs").join(format!("{task}.jsonl")), lines).unwrap();
            let store = server_projects::task_store::Store::new(
                dir.path(),
                server_projects::task_store::Options { kinds: Some(vec!["code".into()]), ..Default::default() },
            );
            let authority = store.claim_lifecycle_authority().unwrap();
            let expected = js_json::JValue::Arr(code_pipeline::EXPECTED_ARTIFACTS.iter().map(|a| js_json::JValue::Str((*a).to_owned())).collect());
            let at = &ours_rows[mine]["data"];
            let request = js_json::JValue::obj([
                ("from", js_json::JValue::Str(at["from"].as_str().unwrap().to_owned())),
                ("to", js_json::JValue::Str("reviewing".into())),
                ("revision", js_json::JValue::Num(at["revision"].as_f64().unwrap())),
                ("reason", js_json::JValue::Str("Tests passed".into())),
                ("expectedArtifacts", expected),
            ]);
            let row = store.append_authority(&task, "task.stage", request, &authority).unwrap();
            let row: Value = serde_json::from_str(&js_json::wtf16::stringify(&row).unwrap()).unwrap();
            row["data"]["reportHash"].clone()
        };
        assert_eq!(replayed, hash, "the gate's hash is the report of the window compared with Node's");
        let mut ours = norm(&card, &pairs);
        assert_eq!(ours["evidence"]["completeness"], json!({"reportHash": hash}), "the card carries the gate's report hash");
        let node_review = journal.iter().find(|j| j["type"] == "approval.requested" && j["data"]["action"] == "review_change").unwrap();
        let mut node_review = node_review["data"].clone();
        node_review["id"] = ours["id"].clone();
        assert!(ours["evidence"]["tests"]["durationMs"].is_u64());
        for v in [&mut ours, &mut node_review] {
            v["evidence"]["tests"]["durationMs"] = json!(7);
            v["evidence"].as_object_mut().unwrap().remove("completeness");
        }
        assert_eq!(ours, node_review, "the review_change card as Node's");
        let (status, ok) = r.call(OWNER, "POST", &approve, Some(decide(card["id"].as_str().unwrap()))).await;
        assert_eq!((status, &ok), (200, &answers[6]["body"]));
        let end = r.ended(&task).await;
        assert_eq!(end["status"], "completed");
        let node_done = journal.iter().find(|j| j["type"] == "job.completed").unwrap();
        assert_eq!(norm(&r.last(&task, "job.completed"), &pairs), node_done["data"], "the result as Node's");
        let (status, late) = r.call(OWNER, "POST", &approve, Some(decide(card["id"].as_str().unwrap()))).await;
        assert_eq!((status, late), (409, answers[8]["body"].clone()));
        r.quiet().await;

        // A second Planner task, cancelled on its plan card.
        let cancel_case = &recorded["scenarios"]["cancel-on-plan-card"]["answers"];
        let (status, started) = r
            .start(json!({"repository": "qa-repo", "prompt": GREET, "promptPreparation": "planner", "approvePlan": true}))
            .await;
        assert_eq!(status, 202, "{started}");
        let task = started["taskId"].as_str().unwrap().to_string();
        let branch = started["branch"].as_str().unwrap().to_string();
        let card = r.card(&task, "approve_plan").await;
        let base = r.base_sha();
        let pairs = [(branch.as_str(), "noevia/qa-task"), (task.as_str(), T1), (base.as_str(), "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb")];
        let (status, view) = r.call(OWNER, "POST", &r.code(&format!("/{task}/cancel")), None).await;
        assert_eq!((status, norm(&view, &pairs)), (200, cancel_case[2]["body"].clone()));
        assert_eq!(r.ended(&task).await["status"], "cancelled");
        let late = r.call(OWNER, "POST", &r.code(&format!("/{task}/approve")), Some(decide(card["id"].as_str().unwrap()))).await;
        assert_eq!(late, (409, cancel_case[4]["body"].clone()));
        let (status, view) = r.call(OWNER, "POST", &r.code(&format!("/{task}/cancel")), None).await;
        assert_eq!((status, norm(&view, &pairs)), (200, cancel_case[5]["body"].clone()), "cancelled and blocked, as Node's");
        assert_eq!(r.sandbox.lock().unwrap().prompts, 1, "the cancelled plan never ran the agent");
        r.quiet().await;
    })
    .await;
}

// ── 4. the Planner review of a direct task ──────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_reviewed_direct_task_that_is_denied_is_not_accepted() {
    within(async {
        let r = rig(Opts { executor: true, review: true, ..Opts::default() }).await;
        let recorded = fixture(GATE);
        // Node's run of the same: the reviewer approves, the owner denies the card.
        let denied = &recorded["scenarios"]["reviewed-and-denied"];
        assert_eq!(denied["answers"], json!(["deny"]));
        let (status, started) = r.start(json!({"repository": "qa-repo", "prompt": RUN, "capabilities": ["read_repository", "edit_file"]})).await;
        assert_eq!(status, 202, "{started}");
        let task = started["taskId"].as_str().unwrap().to_string();
        let branch = started["branch"].as_str().unwrap().to_string();
        let card = r.card(&task, "review_change").await;
        assert_eq!(*r.roles.lock().unwrap(), vec!["reviewer"], "the review alone pins and calls");
        let head = card["arguments"]["headSha"].as_str().unwrap().to_string();
        assert_eq!(head, git(&r.repo, &["rev-parse", &branch]));
        let node_card = &denied["asked"][0];
        for k in ["action", "title", "kind", "command", "reason", "diff"] {
            assert_eq!(card[k], node_card[k], "{k}");
        }
        assert_eq!(card["paths"], json!(["src/qa.js"]));
        assert_eq!(keys(&card["arguments"]), keys(&node_card["arguments"]));
        assert_eq!(card["review"]["verdict"], "approve");
        let node_head = node_card["arguments"]["headSha"].as_str().unwrap();
        let node_base = node_card["arguments"]["baseSha"].as_str().unwrap();
        let base = card["arguments"]["baseSha"].as_str().unwrap().to_string();
        let shas = [(head.as_str(), node_head), (base.as_str(), node_base)];
        let mut review = norm(&card["review"], &shas);
        // The summary is the role's answer: ours the stand-in reviewer's (pipeline recording),
        // Node's the review-gate recording's own stand-in.
        let stub = fixture(PIPELINE)["standins"]["answers"]["reviewer"]["summary"].clone();
        assert_eq!(review["summary"], stub, "the reviewer's own summary");
        review["summary"] = node_card["review"]["summary"].clone();
        assert_eq!(review, node_card["review"], "the reviewer's verdict on the card as Node's (its summary is the role answer's)");

        let (status, ok) = r.call(OWNER, "POST", &r.code(&format!("/{task}/approve")), Some(json!({"decision": "deny", "approvalId": card["id"]}))).await;
        assert_eq!((status, ok), (200, json!({"ok": true})));
        let end = r.ended(&task).await;
        assert_eq!(end["status"], "completed");
        let node_journal = denied["journal"].as_array().unwrap();
        let node_last = node_journal.last().unwrap();
        assert_eq!(node_last["type"], "job.completed", "Node completes a denied review too");
        let result = r.last(&task, "job.completed")["result"].clone();
        let mut node = node_last["data"]["result"].clone();
        assert_eq!(
            node["review"],
            json!({"reviewed": true, "verdict": "approve", "accepted": false, "decision": "deny", "headSha": node_head}),
            "Node's denied review: not accepted"
        );
        assert_eq!(norm(&result["review"], &shas), node["review"], "denied: not accepted, as Node's");
        // The rest of the result as Node's (the tree, branch and identity are this run's).
        for k in ["branch", "workspace", "identityHash", "review"] {
            node[k] = result[k].clone();
        }
        assert_eq!(result, node, "the result as Node's");
        let node_decided = node_journal.iter().rev().find(|j| j["type"] == "approval.decided").unwrap();
        assert_eq!(r.last(&task, "approval.decided"), node_decided["data"], "the decision as Node's");
        r.quiet().await;
    })
    .await;
}

// ── 5. track R11: the engine facts from the front (NOEVIA_RUST_ENGINE_FACTS) ─────────────────

impl Rig {
    /// Starts a direct task and waits until its turn has run (a card waits, or it ended); then
    /// cancels what still waits.
    async fn run_once(&self) -> (u16, Value) {
        let (status, started) = self
            .start(json!({"repository": "qa-repo", "prompt": RUN}))
            .await;
        let Some(task) = started["taskId"].as_str().map(str::to_string) else {
            return (status, started);
        };
        let ended = |v: &Value| {
            matches!(
                v["status"].as_str(),
                Some("completed" | "failed" | "cancelled" | "interrupted")
            )
        };
        let seen = self
            .until(&task, "a card or the end", |v| {
                v["approval"].is_object() || ended(v)
            })
            .await;
        assert_ne!(seen["status"], json!("failed"), "{seen}");
        if !ended(&seen) {
            let (s, ok) = self
                .call(OWNER, "POST", &self.code(&format!("/{task}/cancel")), None)
                .await;
            assert_eq!(s, 200, "{ok}");
            self.ended(&task).await;
        }
        (status, started)
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_front_s_engine_facts_ask_core_only_for_the_tune_state() {
    within(async {
        // Unset, core's answer decides: its `machine: false` runs the task although the registry
        // puts the Code role on a machine.
        let r = rig(Opts {
            executor: true,
            code_machine: true,
            ..Opts::default()
        })
        .await;
        let (status, started) = r.run_once().await;
        assert_eq!(status, 202, "{started}");
        assert_eq!(r.engine_asked(), 1);

        // `rust`: the front reads the registry itself and refuses the machine's Code role before
        // asking core anything.
        let r = rig(Opts {
            executor: true,
            code_machine: true,
            facts: Some("rust"),
            ..Opts::default()
        })
        .await;
        let (status, refused) = r.run_once().await;
        assert_eq!(status, 409, "{refused}");
        assert!(
            refused.to_string().contains("inference machine"),
            "{refused}"
        );
        assert_eq!(r.engine_asked(), 0, "no engine facts asked of core");

        // `rust` with the Code role here, and `shadow`: core is asked once (the tune state; the
        // comparison) and the task runs.
        // The model the task's admission lease names tells whose facts decided: core's synthetic
        // answer names `qa-model`; the account's roles file names another (or none, when the last
        // loaded model, core's, decides).
        for (mode, roles, want) in [
            (
                "rust",
                Some(r#"{"code":"qa-front-model"}"#),
                "qa-front-model",
            ),
            ("rust", None, "qa-model"),
            ("shadow", Some(r#"{"code":"qa-front-model"}"#), "qa-model"),
        ] {
            let r = rig(Opts {
                executor: true,
                facts: Some(mode),
                ..Opts::default()
            })
            .await;
            if let Some(roles) = roles {
                std::fs::write(
                    r.data.join("users").join(OWNER).join("auto-roles.json"),
                    roles,
                )
                .unwrap();
            }
            let (status, started) = r.run_once().await;
            assert_eq!(status, 202, "{mode}: {started}");
            assert_eq!(r.sandbox.lock().unwrap().prompts, 1, "{mode}");
            assert_eq!(r.engine_asked(), 1, "{mode}");
            let leased: Vec<Value> = r
                .core_seen
                .lock()
                .unwrap()
                .iter()
                .filter(|(m, p, _)| m == "PUT" && p.starts_with(LEASE_PATH))
                .map(|(_, _, b)| b["model"].clone())
                .collect();
            assert!(!leased.is_empty(), "{mode}: no lease");
            assert!(
                leased.iter().all(|m| m == want),
                "{mode} {roles:?}: {leased:?}"
            );
        }
    })
    .await;
}
