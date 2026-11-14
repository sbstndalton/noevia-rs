//! The bridge's protocol, case by case from noevia-core `code-sandbox/pi-acp-bridge.test.cjs`
//! (the oracle), plus the reference bugs this port fixes.

use code_harness_bridge::{
    ask_timeout_ms, harness_args, js_number, posix_join, refusal_reason, Bridge, Harness,
    DEFAULT_ASK_TIMEOUT_MS, MAX_TIMER_MS, RUST_FAILED,
};
use serde_json::{json, Value};

#[derive(Default)]
struct Fake {
    spawned: Vec<(Option<String>, Vec<String>)>,
    writes: Vec<Value>,
    kills: usize,
    fail: Option<String>,
}

impl Harness for Fake {
    fn spawn(&mut self, cwd: Option<&str>, args: &[String]) -> Result<(), String> {
        self.spawned.push((cwd.map(str::to_owned), args.to_vec()));
        match &self.fail {
            Some(why) => Err(why.clone()),
            None => Ok(()),
        }
    }
    fn write(&mut self, command: &Value) {
        self.writes.push(command.clone());
    }
    fn kill(&mut self) {
        self.kills += 1;
    }
}

struct T {
    b: Bridge,
    h: Fake,
    out: Vec<Value>,
    logs: Vec<String>,
}

impl T {
    fn new() -> Self {
        Self::with(Fake::default())
    }
    fn with(h: Fake) -> Self {
        T {
            b: Bridge::new(Some("/task/home".to_owned()), 1000),
            h,
            out: Vec::new(),
            logs: Vec::new(),
        }
    }
    fn collect(&mut self) {
        self.out.extend(self.b.take_output());
        self.logs.extend(self.b.take_logs());
    }
    fn send(&mut self, v: Value) {
        self.b.client_line(&mut self.h, &v.to_string());
        self.b.end_chunk(&mut self.h);
        self.collect();
    }
    /// Several client lines in one input chunk.
    fn send_chunk(&mut self, lines: &[Value]) {
        for v in lines {
            self.b.client_line(&mut self.h, &v.to_string());
        }
        self.b.end_chunk(&mut self.h);
        self.collect();
    }
    fn emit(&mut self, v: Value) {
        self.b.harness_line(&mut self.h, &v.to_string());
        self.b.end_chunk(&mut self.h);
        self.collect();
    }
    fn reply(&self, id: i64) -> Option<&Value> {
        self.out
            .iter()
            .find(|m| m.get("id") == Some(&json!(id)) && m.get("method").is_none())
    }
    fn ui(&self, id: &str) -> Option<&Value> {
        self.h.writes.iter().find(|m| {
            m.get("type") == Some(&json!("extension_ui_response"))
                && m.get("id") == Some(&json!(id))
        })
    }
    fn uis(&self, id: &str) -> usize {
        self.h
            .writes
            .iter()
            .filter(|m| {
                m.get("type") == Some(&json!("extension_ui_response"))
                    && m.get("id") == Some(&json!(id))
            })
            .count()
    }
    fn requests(&self) -> Vec<Value> {
        self.out
            .iter()
            .filter(|m| m.get("method") == Some(&json!("session/request_permission")))
            .cloned()
            .collect()
    }
    fn open(&mut self) {
        self.send(json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {} }));
        self.send(json!({ "jsonrpc": "2.0", "id": 2, "method": "session/new", "params": { "cwd": "/task" } }));
    }
    fn prompt(&mut self, id: i64, text: &str) {
        self.send(json!({ "id": id, "method": "session/prompt",
            "params": { "sessionId": "pi-1", "prompt": [{ "type": "text", "text": text }] } }));
    }
    fn confirm(&mut self, ui: &str, call: &str, tool: &str, input: Value) {
        let message =
            json!({ "noevia": "tool_call", "toolCallId": call, "toolName": tool, "input": input })
                .to_string();
        self.emit(json!({ "type": "extension_ui_request", "id": ui, "method": "confirm", "message": message }));
    }
    fn ask_reason(&mut self, ui: &str, call: &str) {
        let placeholder =
            json!({ "noevia": "refusal_reason", "toolCallId": call, "toolName": "bash" })
                .to_string();
        self.emit(
            json!({ "type": "extension_ui_request", "id": ui, "method": "input",
            "title": "noevia refusal reason", "placeholder": placeholder }),
        );
    }
}

#[test]
fn initialize_and_session_new_answer_as_the_reference() {
    let mut t = T::new();
    t.open();
    assert_eq!(
        t.reply(1),
        Some(
            &json!({ "jsonrpc": "2.0", "id": 1, "result": { "protocolVersion": 1,
            "agentCapabilities": { "loadSession": false },
            "agentInfo": { "name": "pi (noevia bridge)", "version": "1" }, "authMethods": [] } })
        )
    );
    assert_eq!(
        t.reply(2),
        Some(&json!({ "jsonrpc": "2.0", "id": 2, "result": { "sessionId": "pi-1" } }))
    );
    let (cwd, args) = t.h.spawned.first().cloned().unwrap_or_default();
    assert_eq!(cwd.as_deref(), Some("/task"));
    assert_eq!(args, harness_args(Some("/task/home")).unwrap_or_default());
    // One session per bridge.
    t.send(json!({ "id": 9, "method": "session/new", "params": {} }));
    assert_eq!(
        t.reply(9).and_then(|m| m.get("error")),
        Some(&json!({ "code": -32602, "message": "One session per bridge" }))
    );
    assert_eq!(t.h.spawned.len(), 1);
}

#[test]
fn harness_args_are_the_reviewed_surface() {
    assert_eq!(
        harness_args(Some("/task/home")).unwrap_or_default(),
        [
            "--mode",
            "rpc",
            "--offline",
            "--no-session",
            "--no-approve",
            "--no-context-files",
            "--no-extensions",
            "--extension",
            "/task/home/.pi/agent/extensions/noevia-gate.js",
            "--no-skills",
            "--no-prompt-templates",
            "--no-themes",
            "--tools",
            "read,bash,edit,write,grep,find,ls",
        ]
    );
    for bad in [None, Some(""), Some("relative/home")] {
        assert_eq!(
            harness_args(bad),
            Err("The pi bridge needs the task private HOME.".to_owned())
        );
    }
    assert_eq!(posix_join("/a/b/", ".pi/x"), "/a/b/.pi/x");
    assert_eq!(posix_join("/a/../b", ".pi/x"), "/b/.pi/x");
    assert_eq!(posix_join("/", ".pi/x"), "/.pi/x");
    assert_eq!(posix_join("//a/./b", "c/"), "/a/b/c/");
}

#[test]
fn session_new_without_an_absolute_home_is_an_internal_error_and_can_be_retried() {
    let mut t = T {
        b: Bridge::new(Some("rel".to_owned()), 1000),
        h: Fake::default(),
        out: vec![],
        logs: vec![],
    };
    t.send(json!({ "id": 2, "method": "session/new", "params": {} }));
    assert_eq!(
        t.reply(2).and_then(|m| m.get("error")),
        Some(&json!({ "code": -32603, "message": "The pi bridge needs the task private HOME." }))
    );
    assert!(t.h.spawned.is_empty());
    t.send(json!({ "id": 3, "method": "session/new", "params": {} }));
    assert_eq!(
        t.reply(3)
            .and_then(|m| m.get("error"))
            .and_then(|e| e.get("code")),
        Some(&json!(-32603))
    );
}

#[test]
fn a_prompt_turn_streams_and_ends_on_agent_settled() {
    let mut t = T::new();
    t.open();
    t.send(json!({ "id": 3, "method": "session/prompt", "params": { "sessionId": "pi-1",
        "prompt": [{ "type": "text", "text": "one" }, { "type": "image" }, { "type": "text", "text": "two" },
                   { "type": "text" }, { "type": "text", "text": 5 }] } }));
    assert_eq!(
        t.h.writes.last(),
        Some(&json!({ "id": "p1", "type": "prompt", "message": "one\ntwo\n\n5" }))
    );
    t.emit(json!({ "type": "response", "id": "p1", "success": true }));
    assert!(t.reply(3).is_none(), "no reply before the turn settles");
    t.emit(json!({ "type": "message_update", "assistantMessageEvent": { "type": "text_delta", "delta": "Hel" } }));
    t.emit(json!({ "type": "message_update", "assistantMessageEvent": { "type": "thinking_delta", "delta": 7 } }));
    t.emit(json!({ "type": "message_update", "assistantMessageEvent": { "type": "text_delta" } }));
    t.emit(json!({ "type": "message_update", "assistantMessageEvent": { "type": "toolcall_delta", "delta": "x" } }));
    t.emit(json!({ "type": "tool_execution_start", "toolCallId": "c1", "toolName": "bash", "args": { "command": "ls -la" } }));
    t.emit(json!({ "type": "tool_execution_end", "toolCallId": "c1", "isError": 1 }));
    t.emit(json!({ "type": "tool_execution_end" }));
    t.emit(json!({ "type": "agent_end" }));
    assert!(t.reply(3).is_none(), "agent_end is one run, not the turn");
    t.emit(json!({ "type": "agent_settled" }));
    assert_eq!(
        t.reply(3),
        Some(&json!({ "jsonrpc": "2.0", "id": 3, "result": { "stopReason": "end_turn" } }))
    );
    let updates: Vec<Value> = t
        .out
        .iter()
        .filter(|m| m.get("method") == Some(&json!("session/update")))
        .map(|m| m.get("params").cloned().unwrap_or(Value::Null))
        .collect();
    assert_eq!(
        updates,
        vec![
            json!({ "sessionId": "pi-1", "update": { "sessionUpdate": "agent_message_chunk", "content": { "type": "text", "text": "Hel" } } }),
            json!({ "sessionId": "pi-1", "update": { "sessionUpdate": "agent_thought_chunk", "content": { "type": "text", "text": "7" } } }),
            json!({ "sessionId": "pi-1", "update": { "sessionUpdate": "agent_message_chunk", "content": { "type": "text", "text": "" } } }),
            json!({ "sessionId": "pi-1", "update": { "sessionUpdate": "tool_call", "toolCallId": "c1", "title": "ls -la",
                "kind": "execute", "rawInput": { "command": "ls -la" }, "status": "in_progress" } }),
            json!({ "sessionId": "pi-1", "update": { "sessionUpdate": "tool_call_update", "toolCallId": "c1", "status": "failed" } }),
            json!({ "sessionId": "pi-1", "update": { "sessionUpdate": "tool_call_update", "toolCallId": "", "status": "completed" } }),
        ]
    );
    // A second turn gets the next command id.
    t.prompt(4, "again");
    assert_eq!(
        t.h.writes.last().and_then(|w| w.get("id")),
        Some(&json!("p2"))
    );
}

#[test]
fn prompt_errors_match_the_reference() {
    let mut t = T::new();
    t.prompt(3, "no session yet");
    assert_eq!(
        t.reply(3).and_then(|m| m.get("error")),
        Some(&json!({ "code": -32602, "message": "No such session" }))
    );
    t.open();
    t.send(json!({ "id": 4, "method": "session/prompt", "params": { "sessionId": "other", "prompt": [] } }));
    assert_eq!(
        t.reply(4)
            .and_then(|m| m.get("error"))
            .and_then(|e| e.get("message")),
        Some(&json!("No such session"))
    );
    t.prompt(5, "first");
    t.prompt(6, "second");
    assert_eq!(
        t.reply(6).and_then(|m| m.get("error")),
        Some(&json!({ "code": -32602, "message": "A prompt is already running" }))
    );
    // The harness refusing the command is a refusal turn.
    t.emit(json!({ "type": "response", "id": "p1", "success": false, "error": "busy" }));
    assert_eq!(
        t.reply(5).and_then(|m| m.get("result")),
        Some(&json!({ "stopReason": "refusal" }))
    );
}

#[test]
fn cancel_aborts_and_the_turn_ends_cancelled() {
    let mut t = T::new();
    t.open();
    t.prompt(3, "go");
    t.emit(json!({ "type": "response", "id": "p1", "success": true }));
    t.send(json!({ "jsonrpc": "2.0", "id": 77, "method": "session/cancel", "params": { "sessionId": "pi-1" } }));
    assert_eq!(t.h.writes.last(), Some(&json!({ "type": "abort" })));
    assert!(t.reply(77).is_none(), "a cancel is never answered");
    t.emit(json!({ "type": "agent_settled" }));
    assert_eq!(
        t.reply(3).and_then(|m| m.get("result")),
        Some(&json!({ "stopReason": "cancelled" }))
    );
}

#[test]
fn an_oversized_line_from_the_harness_fails_the_session_with_a_json_rpc_error() {
    let mut t = T::new();
    t.open();
    t.prompt(3, "hi");
    t.emit(json!({ "type": "response", "id": "p1", "success": true }));
    t.b.harness_overflow(&mut t.h, 16 * 1024 * 1024 + 1);
    t.b.end_chunk(&mut t.h);
    t.collect();
    assert_eq!(
        t.reply(3).and_then(|m| m.get("result")),
        Some(&json!({ "stopReason": "refusal" }))
    );
    let message = "pi's output exceeded the 16777217-byte line buffer; closing the session";
    assert!(t.out.contains(
        &json!({ "jsonrpc": "2.0", "id": null, "error": { "code": -32000, "message": message } })
    ));
    assert_eq!(t.logs, vec![message.to_owned()]);
    assert_eq!(t.h.kills, 1);
    assert!(t.b.output_closed() && t.b.closed());
    // Nothing is read after the session ended.
    let before = t.out.len();
    t.send(json!({ "id": 9, "method": "initialize" }));
    t.emit(json!({ "type": "message_update", "assistantMessageEvent": { "type": "text_delta", "delta": "late" } }));
    assert_eq!(t.out.len(), before);
}

#[test]
fn a_failed_session_answers_a_prompt_never_acknowledged() {
    let mut t = T::new();
    t.open();
    t.prompt(3, "hi");
    t.b.client_overflow(&mut t.h, 20);
    t.b.end_chunk(&mut t.h);
    t.collect();
    assert_eq!(
        t.reply(3).and_then(|m| m.get("result")),
        Some(&json!({ "stopReason": "refusal" }))
    );
    assert_eq!(
        t.logs,
        vec![
            "the client's message exceeded the 20-byte line buffer; closing the session".to_owned()
        ]
    );
    assert_eq!(t.h.kills, 1);
}

#[test]
fn the_harness_exiting_before_its_ack_is_a_refusal_and_ends_the_session() {
    let mut t = T::new();
    t.open();
    t.prompt(3, "hi");
    assert!(t.reply(3).is_none());
    t.b.harness_gone(&mut t.h, "exited (1)");
    t.b.end_chunk(&mut t.h);
    t.collect();
    assert_eq!(
        t.reply(3).and_then(|m| m.get("result")),
        Some(&json!({ "stopReason": "refusal" }))
    );
    assert!(t.b.output_closed());
    assert!(!t.b.harness_running());
    assert_eq!(t.logs, vec!["pi exited (1)".to_owned()]);
    assert_eq!(t.h.kills, 0, "a gone harness is not signalled");
}

#[test]
fn a_spawn_failure_answers_session_new_then_ends_the_session() {
    let mut t = T::with(Fake {
        fail: Some("spawn pi ENOENT".to_owned()),
        ..Fake::default()
    });
    t.open();
    assert_eq!(
        t.reply(2).and_then(|m| m.get("result")),
        Some(&json!({ "sessionId": "pi-1" }))
    );
    assert_eq!(t.logs, vec!["pi failed: spawn pi ENOENT".to_owned()]);
    assert!(t.b.output_closed());
    assert_eq!(t.h.kills, 0);
}

#[test]
fn an_unanswered_ask_times_out_as_a_refusal_and_a_late_allow_is_dropped() {
    let mut t = T::new();
    t.open();
    t.confirm("ui1", "c1", "bash", json!({ "command": "ls" }));
    let timers = t.b.take_timers();
    assert_eq!(timers, vec![("b1".to_owned(), 1000)]);
    let request = t.requests().first().cloned().unwrap_or(Value::Null);
    assert_eq!(
        request,
        json!({ "jsonrpc": "2.0", "id": "b1", "method": "session/request_permission", "params": {
            "sessionId": "pi-1",
            "toolCall": { "toolCallId": "c1", "title": "ls", "kind": "execute", "rawInput": { "command": "ls" } },
            "options": [
                { "optionId": "allow_once", "name": "Allow once", "kind": "allow_once" },
                { "optionId": "reject_once", "name": "Decline", "kind": "reject_once" },
            ] } })
    );
    t.b.ask_timed_out(&mut t.h, "b1");
    t.b.end_chunk(&mut t.h);
    assert_eq!(
        t.ui("ui1"),
        Some(&json!({ "type": "extension_ui_response", "id": "ui1", "confirmed": false }))
    );
    t.send(json!({ "id": "b1", "result": { "outcome": { "outcome": "selected", "optionId": "allow_once" } } }));
    assert_eq!(t.uis("ui1"), 1, "only the timeout answer was ever sent");
}

#[test]
fn an_allow_is_confirmed_once_and_a_second_reply_is_dropped() {
    let mut t = T::new();
    t.open();
    t.confirm("ui1", "c1", "write", json!({ "path": "a.txt" }));
    let allow = json!({ "id": "b1", "result": { "outcome": { "outcome": "selected", "optionId": "allow_once" } } });
    t.send_chunk(&[
        allow.clone(),
        json!({ "id": "b1", "result": { "outcome": { "outcome": "cancelled" } } }),
    ]);
    assert_eq!(
        t.ui("ui1"),
        Some(&json!({ "type": "extension_ui_response", "id": "ui1", "confirmed": true }))
    );
    assert_eq!(t.uis("ui1"), 1);
    // The timer firing afterwards changes nothing.
    t.b.ask_timed_out(&mut t.h, "b1");
    t.b.end_chunk(&mut t.h);
    assert_eq!(t.uis("ui1"), 1);
}

#[test]
fn refusal_reasons_reach_the_gate_once_and_a_plain_decline_has_none() {
    let mut t = T::new();
    t.open();
    let reason = "noevia refused this malformed tool call (violation 1 of 3). Correct the arguments and call the tool again. Missing command at $.rawInput.command.";
    t.confirm("ui1", "c1", "bash", json!({}));
    t.send(json!({ "id": "b1", "result": { "outcome": { "outcome": "selected", "optionId": "reject_once",
        "_meta": { "noevia": { "reason": reason, "violation": { "count": 1 } } } } } }));
    assert_eq!(
        t.ui("ui1").and_then(|m| m.get("confirmed")),
        Some(&json!(false))
    );
    t.ask_reason("ui2", "c1");
    assert_eq!(
        t.ui("ui2").and_then(|m| m.get("value")),
        Some(&json!(reason))
    );
    t.ask_reason("ui3", "c1");
    assert_eq!(
        t.ui("ui3").and_then(|m| m.get("cancelled")),
        Some(&json!(true)),
        "read once"
    );

    t.confirm("ui4", "c2", "bash", json!({}));
    t.send(json!({ "id": "b2", "error": { "code": -32602, "message": "Outside this task’s workspace" } }));
    assert_eq!(
        t.ui("ui4").and_then(|m| m.get("confirmed")),
        Some(&json!(false))
    );
    t.ask_reason("ui5", "c2");
    assert_eq!(
        t.ui("ui5").and_then(|m| m.get("value")),
        Some(&json!("Outside this task’s workspace"))
    );

    t.confirm("ui6", "c3", "bash", json!({}));
    t.send(json!({ "id": "b3", "result": { "outcome": { "outcome": "selected", "optionId": "reject_once" } } }));
    t.ask_reason("ui7", "c3");
    assert_eq!(
        t.ui("ui7").and_then(|m| m.get("cancelled")),
        Some(&json!(true))
    );

    t.confirm("ui8", "c4", "bash", json!({}));
    t.send(json!({ "id": "b4", "result": { "outcome": { "outcome": "selected", "optionId": "allow_once",
        "_meta": { "noevia": { "reason": "ignored" } } } } }));
    assert_eq!(
        t.ui("ui8").and_then(|m| m.get("confirmed")),
        Some(&json!(true))
    );
    t.emit(json!({ "type": "extension_ui_request", "id": "ui9", "method": "input", "title": "Name?", "placeholder": "x" }));
    assert_eq!(
        t.ui("ui9"),
        Some(&json!({ "type": "extension_ui_response", "id": "ui9", "cancelled": true }))
    );
}

#[test]
fn the_refusal_store_keeps_the_newest_fifty() {
    let mut t = T::new();
    t.open();
    for n in 1..=51 {
        t.confirm(&format!("u{n}"), &format!("c{n}"), "bash", json!({}));
        t.send(json!({ "id": format!("b{n}"), "error": { "message": format!("no {n}") } }));
    }
    t.ask_reason("q1", "c1");
    assert_eq!(
        t.ui("q1").and_then(|m| m.get("cancelled")),
        Some(&json!(true)),
        "the oldest went first"
    );
    t.ask_reason("q2", "c2");
    assert_eq!(
        t.ui("q2").and_then(|m| m.get("value")),
        Some(&json!("no 2"))
    );
    t.ask_reason("q51", "c51");
    assert_eq!(
        t.ui("q51").and_then(|m| m.get("value")),
        Some(&json!("no 51"))
    );
}

#[test]
fn other_dialogs_and_malformed_confirms_are_refused_and_notifications_ignored() {
    let mut t = T::new();
    t.open();
    for (id, method) in [("s", "select"), ("e", "editor")] {
        t.emit(json!({ "type": "extension_ui_request", "id": id, "method": method }));
        assert_eq!(
            t.ui(id),
            Some(&json!({ "type": "extension_ui_response", "id": id, "cancelled": true }))
        );
    }
    t.emit(json!({ "type": "extension_ui_request", "id": "n", "method": "notify", "message": "hello" }));
    assert!(t.ui("n").is_none());
    for (id, message) in [
        ("m1", json!("not json")),
        (
            "m2",
            json!(json!({ "noevia": "other", "toolName": "bash" }).to_string()),
        ),
        (
            "m3",
            json!(json!({ "noevia": "tool_call", "toolName": "" }).to_string()),
        ),
        ("m4", json!(json!({ "noevia": "tool_call" }).to_string())),
        ("m5", json!("null")),
    ] {
        t.emit(json!({ "type": "extension_ui_request", "id": id, "method": "confirm", "message": message }));
        assert_eq!(
            t.ui(id),
            Some(&json!({ "type": "extension_ui_response", "id": id, "confirmed": false })),
            "{id}"
        );
    }
    // No id stays no id (JSON.stringify drops an undefined one).
    t.emit(json!({ "type": "extension_ui_request", "method": "select" }));
    assert_eq!(
        t.h.writes.last(),
        Some(&json!({ "type": "extension_ui_response", "cancelled": true }))
    );
    assert!(t.requests().is_empty(), "none of these reached the client");
}

#[test]
fn an_outside_the_workspace_read_is_flagged_for_noevia() {
    let mut t = T::new();
    t.open();
    let message = json!({ "noevia": "tool_call", "toolCallId": "b", "toolName": "read",
        "input": { "path": "/etc/passwd" }, "outsideWorkspace": true })
    .to_string();
    t.emit(json!({ "type": "extension_ui_request", "id": "ui1", "method": "confirm", "message": message }));
    let call = t
        .requests()
        .first()
        .and_then(|r| r.pointer("/params/toolCall").cloned());
    assert_eq!(
        call,
        Some(
            json!({ "toolCallId": "b", "title": "read outside the workspace", "kind": "other",
            "rawInput": { "path": "/etc/passwd", "noeviaOutsideWorkspace": true }, "locations": [{ "path": "/etc/passwd" }] })
        )
    );
}

#[test]
fn a_tool_call_the_shared_rules_refuse_ends_the_session_with_the_fixed_message() {
    let mut t = T::new();
    t.open();
    let message = json!({ "noevia": "tool_call", "toolName": { "toString": 1 } }).to_string();
    t.emit(json!({ "type": "extension_ui_request", "id": "ui1", "method": "confirm", "message": message }));
    assert!(t.out.contains(&json!({ "jsonrpc": "2.0", "id": null, "error": { "code": -32000, "message": RUST_FAILED } })));
    assert!(t.b.closed());
    assert!(t.requests().is_empty(), "never a permission");
}

#[test]
fn client_end_refuses_open_questions_and_stops_the_harness() {
    let mut t = T::new();
    t.open();
    t.confirm("ui1", "c1", "bash", json!({ "command": "rm -rf x" }));
    t.b.client_end(&mut t.h);
    t.b.end_chunk(&mut t.h);
    assert_eq!(
        t.ui("ui1").and_then(|m| m.get("confirmed")),
        Some(&json!(false))
    );
    assert_eq!(t.h.kills, 1);
    assert!(!t.b.closed(), "the session ends when the harness is gone");
    t.prompt(3, "x");
    t.b.harness_gone(&mut t.h, "exited (SIGTERM)");
    t.b.end_chunk(&mut t.h);
    t.collect();
    assert_eq!(
        t.reply(3).and_then(|m| m.get("result")),
        Some(&json!({ "stopReason": "refusal" }))
    );
    assert_eq!(t.logs, vec!["pi exited (SIGTERM)".to_owned()]);
}

#[test]
fn unknown_and_prototype_named_methods_are_method_not_found() {
    // The reference dies on the prototype names (noevia bug filed); here they are unknown methods.
    let mut t = T::new();
    for (n, method) in [
        "nope",
        "toString",
        "constructor",
        "__proto__",
        "hasOwnProperty",
        "",
    ]
    .iter()
    .enumerate()
    {
        let id = n as i64 + 10;
        t.send(json!({ "id": id, "method": method }));
        assert_eq!(
            t.reply(id),
            Some(
                &json!({ "jsonrpc": "2.0", "id": id, "error": { "code": -32601, "message": "Method not found" } })
            ),
            "{method}"
        );
    }
    let before = t.out.len();
    t.send(json!({ "method": "nope" }));
    t.send(json!(null));
    t.send(json!([1, 2]));
    t.send(json!("x"));
    t.send(json!({ "id": "b99", "result": {} }));
    t.send(json!({ "id": null }));
    assert_eq!(
        t.out.len(),
        before,
        "notifications, non-objects and stray replies get no answer"
    );
    t.emit(json!(null));
    t.emit(json!(5));
    assert!(!t.b.closed());
}

#[test]
fn method_not_found_goes_out_before_the_replies_of_the_same_chunk() {
    let mut t = T::new();
    t.send_chunk(&[
        json!({ "id": 1, "method": "initialize" }),
        json!({ "id": 2, "method": "nope" }),
    ]);
    let ids: Vec<Value> = t.out.iter().filter_map(|m| m.get("id").cloned()).collect();
    assert_eq!(
        ids,
        vec![json!(2), json!(1)],
        "the reference's handler replies are promise reactions"
    );
    t.send(json!({ "method": "initialize" }));
    assert_eq!(
        t.out.last().map(|m| m.get("id").is_none()),
        Some(true),
        "an id-less call is answered without an id"
    );
}

#[test]
fn refusal_reason_rules() {
    let r = |o: Value, e: Value| refusal_reason(Some(&o), Some(&e));
    assert_eq!(
        r(
            json!({ "_meta": { "noevia": { "reason": "A" } } }),
            json!({ "message": "B" })
        ),
        "A"
    );
    assert_eq!(
        r(
            json!({ "_meta": { "noevia": { "reason": "  " } } }),
            json!({ "message": "B" })
        ),
        "B"
    );
    assert_eq!(
        r(
            json!({ "_meta": { "noevia": { "reason": 5 } } }),
            json!({ "message": "\u{feff}" })
        ),
        ""
    );
    assert_eq!(r(json!(["x"]), json!(null)), "");
    assert_eq!(refusal_reason(None, None), "");
    let long = "é".repeat(5000);
    assert_eq!(
        r(json!({}), json!({ "message": long })).chars().count(),
        4000
    );
    // Never half a surrogate pair.
    let emoji = format!("a{}", "😀".repeat(2500));
    let cut = r(json!({}), json!({ "message": emoji }));
    assert_eq!(cut.encode_utf16().count(), 3999);
}

#[test]
fn ask_timeout_follows_number_and_nodes_timer_range() {
    assert_eq!(ask_timeout_ms(None), DEFAULT_ASK_TIMEOUT_MS);
    for (raw, want) in [
        ("", DEFAULT_ASK_TIMEOUT_MS),
        ("0", DEFAULT_ASK_TIMEOUT_MS),
        ("-5", DEFAULT_ASK_TIMEOUT_MS),
        ("abc", DEFAULT_ASK_TIMEOUT_MS),
        ("inf", DEFAULT_ASK_TIMEOUT_MS),
        ("NaN", DEFAULT_ASK_TIMEOUT_MS),
        (" 250 ", 250),
        ("1e3", 1000),
        ("0x10", 16),
        ("0.5", 1),
        ("12.9", 12),
        ("Infinity", MAX_TIMER_MS),
        ("3000000000", MAX_TIMER_MS),
    ] {
        assert_eq!(ask_timeout_ms(Some(raw)), want, "{raw:?}");
    }
    assert_eq!(js_number("0b101"), Some(5.0));
    assert_eq!(js_number("0x"), None);
    assert_eq!(js_number("+.5"), Some(0.5));
    assert_eq!(js_number("1_000"), None);
}
