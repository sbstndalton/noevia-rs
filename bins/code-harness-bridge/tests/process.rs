//! The bridge as a process, with a scripted harness, against the JS reference as a live oracle.
//!
//! Each scenario runs the native binary. With `CODE_HARNESS_BRIDGE_ORACLE=<core>/code-sandbox/pi-acp-bridge.cjs`
//! (and `node` on PATH) it also runs the reference with the same harness script and client steps,
//! and the two transcripts (client messages, what the harness read, exit status, the logged
//! reason) must be equal. `CODE_HARNESS_BRIDGE_ORACLE_REQUIRED=1` makes a missing oracle a failure.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{channel, Receiver};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

const WAIT: Duration = Duration::from_secs(15);

/// A scripted harness: each line of `$FAKE_SCRIPT` is `say <json>` (print it), `hear` (read one
/// stdin line into `$FAKE_LOG`), `exit <n>`, or `rest` (copy stdin to the log until EOF).
const FAKE_HARNESS: &str = r#"#!/bin/sh
while IFS= read -r step <&3; do
  case "$step" in
    say\ *) printf '%s\n' "${step#say }" ;;
    hear) IFS= read -r line || exit 0; printf '%s\n' "$line" >> "$FAKE_LOG" ;;
    exit\ *) exit "${step#exit }" ;;
    rest) cat >> "$FAKE_LOG"; exit 0 ;;
  esac
done 3< "$FAKE_SCRIPT"
"#;

#[derive(Debug, PartialEq)]
struct Transcript {
    client: Vec<Value>,
    harness: Vec<Value>,
    exit: Option<i32>,
    reason: Option<String>,
}

struct Run {
    child: Child,
    stdin: Option<ChildStdin>,
    lines: Receiver<Value>,
    seen: Vec<Value>,
    stderr: std::thread::JoinHandle<String>,
    log: PathBuf,
}

enum Bridge {
    Native,
    Oracle(PathBuf),
}

fn oracle() -> Option<PathBuf> {
    let required = std::env::var("CODE_HARNESS_BRIDGE_ORACLE_REQUIRED").as_deref() == Ok("1");
    match std::env::var_os("CODE_HARNESS_BRIDGE_ORACLE").map(PathBuf::from) {
        Some(p) if p.is_file() => Some(p),
        other => {
            assert!(
                !required,
                "CODE_HARNESS_BRIDGE_ORACLE_REQUIRED=1 but no oracle at {other:?}"
            );
            eprintln!("SKIP oracle: set CODE_HARNESS_BRIDGE_ORACLE to core code-sandbox/pi-acp-bridge.cjs");
            None
        }
    }
}

fn start(bridge: &Bridge, dir: &Path, script: &[&str], env: &[(&str, &str)]) -> Run {
    let harness = dir.join("harness.sh");
    std::fs::write(&harness, FAKE_HARNESS).unwrap();
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&harness, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let script_path = dir.join("script");
    std::fs::write(&script_path, script.join("\n") + "\n").unwrap();
    let log = dir.join("harness.log");
    let _ = std::fs::remove_file(&log);
    let mut command = match bridge {
        Bridge::Native => Command::new(env!("CARGO_BIN_EXE_code-harness-bridge")),
        Bridge::Oracle(path) => {
            let mut c = Command::new("node");
            c.arg(path);
            c
        }
    };
    command
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("HOME", dir)
        .env("PI_COMMAND", &harness)
        .env("FAKE_SCRIPT", &script_path)
        .env("FAKE_LOG", &log)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (k, v) in env {
        command.env(k, v);
    }
    let mut child = command.spawn().expect("spawn the bridge");
    let stdin = child.stdin.take();
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let (tx, lines) = channel();
    std::thread::spawn(move || {
        let Some(stdout) = stdout else { return };
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            if let Ok(v) = serde_json::from_str::<Value>(&line) {
                if tx.send(v).is_err() {
                    break;
                }
            }
        }
    });
    let stderr = std::thread::spawn(move || {
        let mut text = String::new();
        if let Some(mut e) = stderr {
            let _ = std::io::Read::read_to_string(&mut e, &mut text);
        }
        text
    });
    Run {
        child,
        stdin,
        lines,
        seen: Vec::new(),
        stderr,
        log,
    }
}

impl Run {
    fn send(&mut self, v: Value) {
        if let Some(stdin) = self.stdin.as_mut() {
            let _ = stdin.write_all(format!("{v}\n").as_bytes());
            let _ = stdin.flush();
        }
    }

    /// Wait for a client message matching `want`; every message read is kept in order.
    fn wait_for(&mut self, what: &str, want: impl Fn(&Value) -> bool) -> Value {
        if let Some(found) = self.seen.iter().find(|m| want(*m)) {
            return found.clone();
        }
        let deadline = Instant::now() + WAIT;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            match self.lines.recv_timeout(left) {
                Ok(v) => {
                    self.seen.push(v.clone());
                    if want(&v) {
                        return v;
                    }
                }
                Err(_) => panic!("timed out waiting for {what}; saw {:?}", self.seen),
            }
        }
    }

    /// Wait until the harness has read `n` lines (the client sees no sign of some of them).
    fn wait_harness(&self, n: usize) {
        let deadline = Instant::now() + WAIT;
        while std::fs::read_to_string(&self.log).map_or(0, |t| t.lines().count()) < n {
            assert!(
                Instant::now() < deadline,
                "the harness never read {n} lines"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    fn close(&mut self) {
        self.stdin = None;
    }

    fn finish(mut self) -> Transcript {
        let deadline = Instant::now() + WAIT;
        let status = loop {
            match self.child.try_wait() {
                Ok(Some(s)) => break Some(s),
                Ok(None) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(20))
                }
                _ => {
                    let _ = self.child.kill();
                    break None;
                }
            }
        };
        assert!(
            status.is_some(),
            "the bridge did not exit by itself; saw {:?}",
            self.seen
        );
        while let Ok(v) = self.lines.recv_timeout(Duration::from_millis(500)) {
            self.seen.push(v);
        }
        let stderr = self.stderr.join().unwrap_or_default();
        // The reason line, without the implementation's own prefix.
        let reason = stderr
            .lines()
            .rev()
            .find_map(|l| l.split_once(": ").map(|(_, r)| r.to_owned()))
            .filter(|r| r.starts_with("pi ") || r.contains("closing the session"));
        let harness = std::fs::read_to_string(&self.log)
            .unwrap_or_default()
            .lines()
            .filter_map(|l| serde_json::from_str(l).ok())
            .collect();
        Transcript {
            client: self.seen,
            harness,
            exit: status.and_then(|s| s.code()),
            reason,
        }
    }
}

fn is_reply(id: i64) -> impl Fn(&Value) -> bool {
    move |m: &Value| m.get("id") == Some(&json!(id)) && m.get("method").is_none()
}

fn is_permission(m: &Value) -> bool {
    m.get("method") == Some(&json!("session/request_permission"))
}

fn open(run: &mut Run, dir: &Path) {
    run.send(json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {} }));
    run.wait_for("initialize", is_reply(1));
    run.send(
        json!({ "jsonrpc": "2.0", "id": 2, "method": "session/new", "params": { "cwd": dir } }),
    );
    run.wait_for("session/new", is_reply(2));
}

fn prompt(run: &mut Run, id: i64, text: &str) {
    run.send(
        json!({ "jsonrpc": "2.0", "id": id, "method": "session/prompt",
        "params": { "sessionId": "pi-1", "prompt": [{ "type": "text", "text": text }] } }),
    );
}

fn confirm_line(ui: &str, call: &str, tool: &str, input: &Value) -> String {
    let message =
        json!({ "noevia": "tool_call", "toolCallId": call, "toolName": tool, "input": input })
            .to_string();
    format!(
        "say {}",
        json!({ "type": "extension_ui_request", "id": ui, "method": "confirm", "message": message })
    )
}

type Scenario = fn(&Bridge, &Path) -> Transcript;

fn full_turn(bridge: &Bridge, dir: &Path) -> Transcript {
    let confirm = confirm_line("ui1", "c9", "bash", &json!({ "command": "make test" }));
    let script = [
        "hear",
        r#"say {"type":"response","id":"p1","command":"prompt","success":true}"#,
        r#"say {"type":"message_update","assistantMessageEvent":{"type":"text_delta","delta":"Hello"}}"#,
        r#"say {"type":"message_update","assistantMessageEvent":{"type":"thinking_delta","delta":"hm"}}"#,
        confirm.as_str(),
        "hear",
        r#"say {"type":"tool_execution_start","toolCallId":"c9","toolName":"bash","args":{"command":"make test"}}"#,
        r#"say {"type":"tool_execution_end","toolCallId":"c9","isError":false}"#,
        r#"say {"type":"agent_end"}"#,
        r#"say {"type":"agent_settled"}"#,
        "rest",
    ];
    let mut run = start(bridge, dir, &script, &[]);
    open(&mut run, dir);
    prompt(&mut run, 3, "run the tests");
    let ask = run.wait_for("the permission request", is_permission);
    run.send(json!({ "jsonrpc": "2.0", "id": ask.get("id"),
        "result": { "outcome": { "outcome": "selected", "optionId": "allow_once" } } }));
    run.wait_for("the turn's end", is_reply(3));
    run.close();
    run.finish()
}

fn dies_before_ack(bridge: &Bridge, dir: &Path) -> Transcript {
    let mut run = start(bridge, dir, &["hear", "exit 3"], &[]);
    open(&mut run, dir);
    prompt(&mut run, 3, "hi");
    run.wait_for("the refusal", is_reply(3));
    // stdin stays open: the bridge exits on its own.
    run.finish()
}

fn refusal_reason_round_trip(bridge: &Bridge, dir: &Path) -> Transcript {
    let confirm = confirm_line("ui1", "c1", "write", &json!({ "path": "../x" }));
    let placeholder =
        json!({ "noevia": "refusal_reason", "toolCallId": "c1", "toolName": "write" }).to_string();
    let input = format!(
        "say {}",
        json!({ "type": "extension_ui_request", "id": "ui2", "method": "input", "placeholder": placeholder })
    );
    let script = [
        confirm.as_str(),
        "hear",
        input.as_str(),
        "hear",
        input.as_str(),
        "hear",
        "rest",
    ];
    let mut run = start(bridge, dir, &script, &[]);
    open(&mut run, dir);
    let ask = run.wait_for("the permission request", is_permission);
    run.send(json!({ "jsonrpc": "2.0", "id": ask.get("id"),
        "error": { "code": -32602, "message": "Outside this task's workspace" } }));
    run.wait_harness(3);
    run.close();
    run.finish()
}

fn ask_times_out(bridge: &Bridge, dir: &Path) -> Transcript {
    let confirm = confirm_line("ui1", "c1", "bash", &json!({ "command": "ls" }));
    let mut run = start(
        bridge,
        dir,
        &[confirm.as_str(), "hear", "rest"],
        &[("PI_BRIDGE_ASK_TIMEOUT_MS", "300")],
    );
    open(&mut run, dir);
    let ask = run.wait_for("the permission request", is_permission);
    run.wait_harness(1);
    // Long after the timeout: dropped, never applied.
    run.send(json!({ "jsonrpc": "2.0", "id": ask.get("id"),
        "result": { "outcome": { "outcome": "selected", "optionId": "allow_once" } } }));
    std::thread::sleep(Duration::from_millis(200));
    run.close();
    run.finish()
}

fn spawn_failure(bridge: &Bridge, dir: &Path) -> Transcript {
    let missing = dir.join("no-such-harness");
    let missing = missing.to_string_lossy().into_owned();
    let mut run = start(bridge, dir, &[], &[("PI_COMMAND", missing.as_str())]);
    open(&mut run, dir);
    run.close();
    run.finish()
}

fn unknown_method_and_cancel(bridge: &Bridge, dir: &Path) -> Transcript {
    let script = [
        "hear",
        r#"say {"type":"response","id":"p1","success":true}"#,
        "hear",
        r#"say {"type":"agent_settled"}"#,
        "rest",
    ];
    let mut run = start(bridge, dir, &script, &[]);
    run.send(json!({ "jsonrpc": "2.0", "id": 7, "method": "session/load", "params": {} }));
    run.wait_for("method not found", is_reply(7));
    open(&mut run, dir);
    prompt(&mut run, 3, "long job");
    run.send(
        json!({ "jsonrpc": "2.0", "method": "session/cancel", "params": { "sessionId": "pi-1" } }),
    );
    run.wait_for("the cancelled turn", is_reply(3));
    run.close();
    run.finish()
}

const SCENARIOS: [(&str, Scenario); 6] = [
    ("full_turn", full_turn),
    ("dies_before_ack", dies_before_ack),
    ("refusal_reason_round_trip", refusal_reason_round_trip),
    ("ask_times_out", ask_times_out),
    ("spawn_failure", spawn_failure),
    ("unknown_method_and_cancel", unknown_method_and_cancel),
];

fn tempdir() -> tempfile::TempDir {
    tempfile::tempdir().expect("tempdir")
}

/// What each scenario must show natively (independent of the oracle).
fn check(name: &str, t: &Transcript) {
    let reply = |id: i64| t.client.iter().find(|m| is_reply(id)(*m)).cloned();
    let stop = |id: i64| reply(id).and_then(|m| m.pointer("/result/stopReason").cloned());
    assert_eq!(t.exit, Some(0), "{name}: {t:?}");
    match name {
        "full_turn" => {
            assert_eq!(stop(3), Some(json!("end_turn")), "{t:?}");
            assert_eq!(
                t.harness.first(),
                Some(&json!({ "id": "p1", "type": "prompt", "message": "run the tests" }))
            );
            assert_eq!(
                t.harness.get(1),
                Some(&json!({ "type": "extension_ui_response", "id": "ui1", "confirmed": true }))
            );
            assert_eq!(t.reason.as_deref(), Some("pi exited (SIGTERM)"), "{t:?}");
        }
        "dies_before_ack" => {
            assert_eq!(stop(3), Some(json!("refusal")));
            assert_eq!(t.reason.as_deref(), Some("pi exited (3)"));
        }
        "refusal_reason_round_trip" => assert_eq!(
            t.harness,
            vec![
                json!({ "type": "extension_ui_response", "id": "ui1", "confirmed": false }),
                json!({ "type": "extension_ui_response", "id": "ui2", "value": "Outside this task's workspace" }),
                json!({ "type": "extension_ui_response", "id": "ui2", "cancelled": true }),
            ]
        ),
        "ask_times_out" => assert_eq!(
            t.harness,
            vec![json!({ "type": "extension_ui_response", "id": "ui1", "confirmed": false })]
        ),
        "spawn_failure" => {
            assert_eq!(
                reply(2).and_then(|m| m.pointer("/result/sessionId").cloned()),
                Some(json!("pi-1"))
            );
            let reason = t.reason.clone().unwrap_or_default();
            assert!(
                reason.starts_with("pi failed: spawn ") && reason.ends_with(" ENOENT"),
                "{reason}"
            );
        }
        "unknown_method_and_cancel" => {
            assert_eq!(
                reply(7).and_then(|m| m.pointer("/error/code").cloned()),
                Some(json!(-32601))
            );
            assert_eq!(stop(3), Some(json!("cancelled")));
            assert_eq!(t.harness.get(1), Some(&json!({ "type": "abort" })));
        }
        _ => {}
    }
}

#[test]
fn scenarios_hold_natively() {
    for (name, scenario) in SCENARIOS {
        let dir = tempdir();
        let t = scenario(&Bridge::Native, dir.path());
        check(name, &t);
    }
}

#[test]
fn scenarios_match_the_js_reference() {
    let Some(path) = oracle() else { return };
    let mut agreed = 0;
    for (name, scenario) in SCENARIOS {
        let native_dir = tempdir();
        let oracle_dir = tempdir();
        let mut native = scenario(&Bridge::Native, native_dir.path());
        let mut reference = scenario(&Bridge::Oracle(path.clone()), oracle_dir.path());
        check(name, &reference);
        // The temporary directories differ: compare paths as one placeholder.
        for (t, dir) in [
            (&mut native, native_dir.path()),
            (&mut reference, oracle_dir.path()),
        ] {
            let d = dir.to_string_lossy().into_owned();
            let fix = |v: &Value| -> Value {
                serde_json::from_str(&v.to_string().replace(&d, "<dir>")).unwrap_or(Value::Null)
            };
            t.client = t.client.iter().map(fix).collect();
            t.harness = t.harness.iter().map(fix).collect();
            t.reason = t.reason.as_ref().map(|r| r.replace(&d, "<dir>"));
        }
        assert_eq!(native, reference, "{name}");
        agreed += 1;
    }
    eprintln!("oracle: {agreed} scenarios agree with {}", path.display());
    assert_eq!(agreed, SCENARIOS.len());
}
