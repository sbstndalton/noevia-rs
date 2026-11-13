//! core code-harness.cjs `createSession`: the per-task state and the four handlers an ACP agent
//! calls back into. Every action the agent reports is classified (code-actions) and either
//! allowed, refused outright, or put in front of a person with its full arguments; "Allow for this
//! task" stands only for the same action class, never for a delete or a push, and dies with the
//! task; every file the agent reads or writes through noevia is checked against the task's
//! worktree again at the moment of the read or write ([`crate::files`]); and everything is
//! recorded in the task's journal through [`Host::event`].
//!
//! The session's state is behind one lock that is never held while a person is being asked:
//! Node runs these handlers concurrently, and a permission waiting for an answer must not hold up
//! the agent's session updates.
//!
//! Strings are in the [`js_json::wtf16`] form; output batching and every cut count UTF-16 units
//! and UTF-8 bytes exactly as Node does (a surrogate pair split across two message chunks is
//! joined, a lone surrogate written as U+FFFD).

use crate::files;
use crate::tool_schemas::{reject_outcome, ExecutorGuard, GuardHooks, GuardOptions};
use crate::{meta, Thrown};
use code_actions::host as actions;
use js_json::{wtf16, JValue};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

/// What a job event keeps of a tool's text (code-harness.cjs `MAX_TEXT`).
pub const MAX_TEXT: usize = 4000;
/// One source file, not a database the agent found (`MAX_FILE_BYTES`).
pub const MAX_FILE_BYTES: u64 = 8 * 1024 * 1024;
const OUTPUT_BATCH_BYTES: usize = 1024;
const MAX_EARLY_FLUSHES: u32 = 30;
const MAX_ASSISTANT_OUTPUT_BYTES: usize = 32 * 1024;
const MAX_ASSISTANT_OUTPUT_EVENTS: u32 = 64;
const MAX_CODE_PLAN_ENTRIES: usize = 20;
const MAX_CODE_PLAN_ENTRY_BYTES: usize = 200;
const MAX_KNOWN_CALLS: usize = 200;
const MAX_EXITS: usize = 200;
const OUTSIDE: &str = "Outside this task\u{2019}s workspace";

/// A handler's refusal, as the ACP layer answers it: `code` -32602 (with `data`) is the agent's
/// own fault, anything else is internal.
#[derive(Debug, Clone, PartialEq)]
pub struct Refusal {
    pub code: Option<f64>,
    pub message: String,
    pub data: Option<JValue>,
}

impl Refusal {
    fn invalid(message: &str) -> Refusal {
        Refusal {
            code: Some(-32602.0),
            message: message.to_string(),
            data: None,
        }
    }
    fn thrown(t: Thrown) -> Refusal {
        let message = match t {
            Thrown::Error { message, .. } => message,
            Thrown::Value(v) => js_json::to_js_string(&v).unwrap_or_default(),
        };
        Refusal {
            code: None,
            message,
            data: None,
        }
    }
}

/// The task has ended (cancelled, failed, stopped by the guard): every approval it still has
/// waiting is refused as `aborted`, not left to time out.
#[derive(Default)]
pub struct Ended {
    ended: AtomicBool,
    hooks: Mutex<Vec<Box<dyn Fn() + Send + Sync>>>,
}

impl Ended {
    pub fn is_ended(&self) -> bool {
        self.ended.load(Ordering::SeqCst)
    }
    /// Runs `hook` when the task ends (at once if it has).
    pub fn on_end(&self, hook: Box<dyn Fn() + Send + Sync>) {
        if self.is_ended() {
            hook();
            return;
        }
        if let Ok(mut h) = self.hooks.lock() {
            h.push(hook);
        }
        if self.is_ended() {
            let hooks: Vec<_> = self.hooks.lock().map(|mut h| std::mem::take(&mut *h)).unwrap_or_default();
            for hook in hooks {
                hook();
            }
        }
    }
    fn end(&self) {
        if self.ended.swap(true, Ordering::SeqCst) {
            return;
        }
        let hooks: Vec<_> = self.hooks.lock().map(|mut h| std::mem::take(&mut *h)).unwrap_or_default();
        for hook in hooks {
            hook();
        }
    }
}

/// What the session needs from the task around it (code-harness.cjs's `ctx`, `workspaces`,
/// `askApproval`, `log`, `now` and the guard's halt).
pub trait Host: Send + Sync {
    /// `ctx.event(type, data)`: appends to the task's journal; a failure is what it threw.
    fn event(&self, kind: &str, data: JValue) -> Result<(), String>;
    /// `askApproval(request, { signal })`: a person's answer (`approve`, `approve_all`, `deny`,
    /// `timeout`); `aborted` once `ended` ends.
    fn ask_approval(&self, request: &JValue, ended: &Arc<Ended>) -> String;
    /// `workspaces.contains(taskId, path)`: `None` when the task holds no workspace.
    fn contains(&self, path: &JValue) -> Option<bool>;
    /// `workspaces.get(taskId)?.path`.
    fn root(&self) -> Option<String>;
    /// The server log (`log(entry)`).
    fn log(&self, entry: JValue);
    fn now(&self) -> f64;
    /// The guard stopped the task: stop the agent (Node: `halt.abort()` on the next tick).
    fn halt(&self);
}

/// The task the session belongs to.
#[derive(Debug, Clone)]
pub struct Task {
    pub task_id: String,
    pub capabilities: Vec<String>,
    pub domains: Vec<String>,
    pub harness: String,
    pub model: JValue,
    pub workspace_path: String,
    /// The Executor guard (features.executorGuard), decided when the task started.
    pub guarded: bool,
}

struct Hooks {
    host: Arc<dyn Host>,
    ended: Arc<Ended>,
    task: Arc<Task>,
}

fn record_entry(host: &dyn Host, task: &Task, entry: Vec<(String, JValue)>) {
    let mut out = vec![
        ("at".to_string(), JValue::Num(host.now())),
        ("taskId".to_string(), JValue::Str(task.task_id.clone())),
        ("harness".to_string(), JValue::Str(task.harness.clone())),
        ("model".to_string(), task.model.clone()),
    ];
    for (k, v) in entry {
        wtf16::set(&mut out, &k, v);
    }
    host.log(JValue::Obj(out));
}

impl GuardHooks for Hooks {
    fn event(&mut self, kind: &str, data: &JValue) -> Result<(), Thrown> {
        self.host.event(kind, data.clone()).map_err(Thrown::error)
    }
    fn log(&mut self, entry: &JValue) -> Result<(), Thrown> {
        let items = match entry {
            JValue::Obj(items) => items.clone(),
            _ => Vec::new(),
        };
        record_entry(&*self.host, &self.task, items);
        Ok(())
    }
    fn on_blocked(&mut self) -> Result<(), Thrown> {
        // Pending approvals are refused now; the agent is stopped next, so the refusal that
        // reached the limit is still delivered to it first.
        self.ended.end();
        self.host.halt();
        Ok(())
    }
}

#[derive(Default)]
struct Counts {
    tools: u64,
    approvals: u64,
    allowed: u64,
    refused: u64,
    denied: u64,
}

struct State {
    blanket: HashMap<String, &'static str>,
    counts: Counts,
    exits: Vec<JValue>,
    names: HashMap<String, JValue>,
    /// `Map` toolCallId -> the call as announced so far, in insertion order (keys are the id's
    /// JSON text, so `1` and `"1"` stay apart as in a JS Map).
    calls: Vec<(String, JValue)>,
    reported_usage: JValue,
    reported_context: JValue,
    message_chunks: u64,
    output_bytes: usize,
    output_buffer: String,
    output_buffer_bytes: usize,
    output_truncated: bool,
    pending_high: Option<char>,
    truncation_recorded: bool,
    output_events: u32,
    early_flushes: u32,
    break_after_tool: bool,
    last_stage: Option<String>,
    last_stage_at: f64,
    pending_stage: Option<String>,
    guard: Option<ExecutorGuard<Hooks>>,
}

/// One task's session.
pub struct Session {
    host: Arc<dyn Host>,
    task: Arc<Task>,
    ended: Arc<Ended>,
    state: Mutex<State>,
}

fn s(text: &str) -> JValue {
    JValue::Str(text.to_string())
}

/// `x || fallback` for JS values.
fn or(v: &JValue, fallback: JValue) -> JValue {
    if v.truthy() {
        v.clone()
    } else {
        fallback
    }
}

/// `x ?? fallback`.
fn nullish_or(v: &JValue, fallback: JValue) -> JValue {
    if v.is_nullish() {
        fallback
    } else {
        v.clone()
    }
}

fn key_of(id: &JValue) -> String {
    wtf16::stringify(id).unwrap_or_default()
}

/// UTF-8 bytes of one character as Node writes it (a lone surrogate is U+FFFD, 3 bytes).
fn char_bytes(c: char) -> usize {
    if wtf16::lone_unit(c).is_some() {
        3
    } else {
        c.len_utf8()
    }
}

/// `typeof text === 'string' ? text.slice(0, MAX_TEXT) : null`.
fn clip(text: &JValue) -> JValue {
    match text {
        JValue::Str(t) => JValue::Str(wtf16::slice(t, MAX_TEXT)),
        _ => JValue::Null,
    }
}

/// code-plan.cjs `clipUtf8`: whole characters up to `max` bytes, a lone surrogate as U+FFFD.
fn clip_utf8(text: &str, max: usize) -> String {
    let mut out = String::new();
    let mut bytes = 0;
    for c in text.chars() {
        let c = if wtf16::lone_unit(c).is_some() { '\u{FFFD}' } else { c };
        if bytes + c.len_utf8() > max {
            break;
        }
        bytes += c.len_utf8();
        out.push(c);
    }
    out
}

/// code-plan.cjs `boundCodePlan`: `{ subQuestions, truncated }`.
fn bound_code_plan(entries: &[JValue], truncated: bool) -> (JValue, bool) {
    let mut truncated = truncated || entries.len() > MAX_CODE_PLAN_ENTRIES;
    let questions = entries
        .iter()
        .take(MAX_CODE_PLAN_ENTRIES)
        .map(|entry| {
            let original = if entry.is_nullish() {
                String::new()
            } else {
                js_json::to_js_string(entry).unwrap_or_default()
            };
            let text = clip_utf8(&original, MAX_CODE_PLAN_ENTRY_BYTES);
            if text != original {
                truncated = true;
            }
            JValue::Str(text)
        })
        .collect();
    (JValue::Arr(questions), truncated)
}

/// JS `Number(v)` for a read's `line`/`limit`.
fn js_number(v: &JValue) -> f64 {
    match v {
        JValue::Num(n) => *n,
        JValue::Null => 0.0,
        JValue::Bool(b) => f64::from(u8::from(*b)),
        JValue::Str(_) | JValue::Arr(_) => {
            let t = js_json::to_js_string(v).unwrap_or_default();
            let t = t.trim_matches(|c: char| c.is_whitespace() || c == '\u{feff}');
            if t.is_empty() {
                0.0
            } else if let Some(hex) = t.strip_prefix("0x").or_else(|| t.strip_prefix("0X")) {
                u64::from_str_radix(hex, 16).map_or(f64::NAN, |n| n as f64)
            } else if t.chars().all(|c| c.is_ascii_digit() || matches!(c, '.' | 'e' | 'E' | '+' | '-')) {
                t.parse::<f64>().unwrap_or(f64::NAN)
            } else if t == "Infinity" || t == "+Infinity" {
                f64::INFINITY
            } else if t == "-Infinity" {
                f64::NEG_INFINITY
            } else {
                f64::NAN
            }
        }
        JValue::Undefined | JValue::Obj(_) => f64::NAN,
    }
}

/// JS `Array.prototype.slice(start, end)` index for a length.
fn slice_index(i: f64, len: usize) -> usize {
    let len_f = len as f64;
    let i = if i.is_nan() { 0.0 } else { i.trunc() };
    let i = if i < 0.0 { (len_f + i).max(0.0) } else { i.min(len_f) };
    i as usize
}

impl Session {
    pub fn new(host: Arc<dyn Host>, task: Task) -> Session {
        let task = Arc::new(task);
        let ended = Arc::new(Ended::default());
        let guard = task.guarded.then(|| {
            ExecutorGuard::new(
                Hooks {
                    host: host.clone(),
                    ended: ended.clone(),
                    task: task.clone(),
                },
                GuardOptions {
                    max_bytes: MAX_FILE_BYTES as f64,
                    ..GuardOptions::default()
                },
            )
        });
        Session {
            host,
            task,
            ended,
            state: Mutex::new(State {
                blanket: HashMap::new(),
                counts: Counts::default(),
                exits: Vec::new(),
                names: HashMap::new(),
                calls: Vec::new(),
                reported_usage: JValue::Null,
                reported_context: JValue::Null,
                message_chunks: 0,
                output_bytes: 0,
                output_buffer: String::new(),
                output_buffer_bytes: 0,
                output_truncated: false,
                pending_high: None,
                truncation_recorded: false,
                output_events: 0,
                early_flushes: 0,
                break_after_tool: false,
                last_stage: None,
                last_stage_at: f64::NEG_INFINITY,
                pending_stage: None,
                guard,
            }),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// The task's end: what it still has waiting is refused as `aborted`.
    pub fn end(&self) {
        self.ended.end();
    }

    pub fn ended(&self) -> &Arc<Ended> {
        &self.ended
    }

    /// Whether the guard stopped the task, and the error the task then ends with.
    pub fn blocked(&self) -> Option<crate::tool_schemas::BlockedError> {
        self.lock().guard.as_ref().and_then(ExecutorGuard::blocked_error)
    }

    fn record(&self, entry: Vec<(&str, JValue)>) {
        record_entry(
            &*self.host,
            &self.task,
            entry.into_iter().map(|(k, v)| (k.to_string(), v)).collect(),
        );
    }

    fn event(&self, kind: &str, data: JValue) -> Result<(), Refusal> {
        self.host.event(kind, data).map_err(|message| Refusal {
            code: None,
            message,
            data: None,
        })
    }

    fn containment(&self, classified: &JValue) -> JValue {
        let JValue::Arr(paths) = classified.get("paths") else {
            return JValue::Null;
        };
        if paths.is_empty() {
            return JValue::Null;
        }
        let verdicts: Vec<Option<bool>> = paths.iter().map(|p| self.host.contains(p)).collect();
        if verdicts.contains(&Some(false)) {
            JValue::Bool(false)
        } else if verdicts.iter().all(|v| *v == Some(true)) {
            JValue::Bool(true)
        } else {
            JValue::Null
        }
    }

    fn reject(options: &JValue, reason: &JValue) -> Result<JValue, Refusal> {
        reject_outcome(&actions::pick_option(options, "reject_once"), reason).map_err(Refusal::thrown)
    }

    /// `session/request_permission`: the selected outcome.
    pub fn request_permission(&self, params: &JValue) -> Result<JValue, Refusal> {
        let stated = match params.get("toolCall") {
            JValue::Undefined => JValue::Obj(Vec::new()),
            v => v.clone(),
        };
        let options = match params.get("options") {
            JValue::Undefined => JValue::Arr(Vec::new()),
            v => v.clone(),
        };
        let mut st = self.lock();
        let announced = if stated.truthy() && stated.get("toolCallId").truthy() {
            let key = key_of(stated.get("toolCallId"));
            st.calls.iter().find(|(k, _)| *k == key).map(|(_, v)| v.clone())
        } else {
            None
        };
        let tool_call = match (&announced, &stated) {
            (Some(a), JValue::Obj(fields)) => {
                let mut merged = match a {
                    JValue::Obj(items) => items.clone(),
                    _ => Vec::new(),
                };
                for (k, v) in fields {
                    if !v.is_nullish() {
                        wtf16::set(&mut merged, k, v.clone());
                    }
                }
                JValue::Obj(merged)
            }
            (Some(a), _) => a.clone(),
            (None, _) => stated.clone(),
        };
        if let Some(guard) = st.guard.as_mut() {
            let action = actions::classify(&tool_call).get("action").clone();
            match guard.permission(&tool_call, &action) {
                Ok(Some(malformed)) => {
                    st.counts.approvals += 1;
                    st.counts.denied += 1;
                    drop(st);
                    return Self::reject(&options, &malformed);
                }
                Ok(None) => {}
                Err(t) => return Err(Refusal::thrown(t)),
            }
        }
        let mut classified = actions::classify(&tool_call);
        // The announcement explains the call; it cannot excuse it.
        if announced.is_some() && classified.get("approval").as_str() == Some("never") {
            let own = actions::classify(&stated);
            if *own.get("unverified") == JValue::Bool(true) {
                classified = own;
            } else if own.get("approval").as_str() != Some("never") {
                let standable = *classified.get("standable") != JValue::Bool(false)
                    && *own.get("standable") != JValue::Bool(false);
                if let JValue::Obj(items) = &mut classified {
                    wtf16::set(items, "approval", own.get("approval").clone());
                    wtf16::set(items, "standable", JValue::Bool(standable));
                }
            }
        }
        st.counts.approvals += 1;
        let in_workspace = self.containment(&classified);
        let capabilities = JValue::Arr(self.task.capabilities.iter().map(|c| s(c)).collect());
        let domains = JValue::Arr(self.task.domains.iter().map(|d| s(d)).collect());
        let verdict = actions::decide(&classified, &capabilities, &domains, in_workspace.as_bool());
        let action = classified.get("action").clone();
        let reason = verdict.get("reason").clone();
        match verdict.get("decision").as_str() {
            Some("deny") => {
                st.counts.denied += 1;
                drop(st);
                self.record(vec![
                    ("event", s("code.refused")),
                    ("action", action.clone()),
                    ("reason", reason.clone()),
                ]);
                self.event(
                    "approval.decided",
                    JValue::obj([
                        ("decision", s("denied")),
                        ("action", action),
                        ("reason", reason.clone()),
                        ("automatic", JValue::Bool(true)),
                    ]),
                )?;
                return Self::reject(&options, &reason);
            }
            Some("allow") => {
                st.counts.allowed += 1;
                return Ok(actions::pick_option(&options, "allow_once"));
            }
            _ => {}
        }
        let no_standing = *classified.get("standable") == JValue::Bool(false)
            || *verdict.get("standable") == JValue::Bool(false);
        let action_key = js_json::to_js_string(&action).unwrap_or_default();
        if !no_standing {
            match st.blanket.get(&action_key).copied() {
                Some("allow") => {
                    st.counts.allowed += 1;
                    return Ok(actions::pick_option(&options, "allow_once"));
                }
                Some(_) => {
                    st.counts.refused += 1;
                    drop(st);
                    return Self::reject(&options, &s("The person refused this kind of action for this task."));
                }
                None => {}
            }
        }
        drop(st);
        let request = JValue::obj([
            ("taskId", s(&self.task.task_id)),
            ("action", action.clone()),
            ("title", or(tool_call.get("title"), s(""))),
            ("kind", or(tool_call.get("kind"), s(""))),
            ("command", classified.get("command").clone()),
            ("paths", classified.get("paths").clone()),
            ("reason", reason),
            ("arguments", nullish_or(tool_call.get("rawInput"), JValue::Null)),
            ("diff", diff_of(&tool_call)),
        ]);
        self.event("approval.requested", request.clone())?;
        let answer = if self.ended.is_ended() {
            "aborted".to_string()
        } else {
            self.host.ask_approval(&request, &self.ended)
        };
        self.event(
            "approval.decided",
            JValue::obj([("decision", s(&answer)), ("action", action.clone())]),
        )?;
        self.record(vec![
            ("event", s("code.decided")),
            ("action", action.clone()),
            ("decision", s(&answer)),
        ]);
        let mut st = self.lock();
        if answer == "approve" || answer == "approve_all" {
            if answer == "approve_all" && can_stand(&action_key) && !no_standing {
                st.blanket.insert(action_key, "allow");
            }
            st.counts.allowed += 1;
            return Ok(actions::pick_option(&options, "allow_once"));
        }
        st.counts.refused += 1;
        drop(st);
        let why = match answer.as_str() {
            "timeout" => "Nobody answered the approval request in time, so the action was not run.",
            "aborted" => "The task was stopped before this action was approved, so it was not run.",
            _ => "The person declined this action.",
        };
        Self::reject(&options, &s(why))
    }

    /// noevia's file API for the agent, inside the worktree or not at all: the path contains()
    /// judged, resolved against the worktree.
    fn inside(&self, target: &JValue) -> Result<String, Refusal> {
        let refuse = || {
            self.record(vec![
                ("event", s("code.refused")),
                ("reason", s("path outside the workspace")),
            ]);
            Refusal::invalid(OUTSIDE)
        };
        if self.host.contains(target) != Some(true) {
            return Err(refuse());
        }
        let Some(root) = self.host.root().filter(|r| !r.is_empty()) else {
            return Err(refuse());
        };
        let target = target.as_str().unwrap_or("");
        Ok(if target.starts_with('/') {
            normalize(target)
        } else {
            normalize(&format!("{root}/{target}"))
        })
    }

    fn guard_fs(&self, write: bool, params: &JValue) -> Result<(), Refusal> {
        let mut st = self.lock();
        let Some(guard) = st.guard.as_mut() else {
            return Ok(());
        };
        let checked = if write {
            guard.write_text_file(params)
        } else {
            guard.read_text_file(params)
        };
        match checked {
            Ok(None) => Ok(()),
            Ok(Some(e)) => Err(Refusal {
                code: Some(f64::from(e.code)),
                message: e.message,
                data: Some(e.data),
            }),
            Err(t) => Err(Refusal::thrown(t)),
        }
    }

    /// `fs/read_text_file`: `{ content }`, optionally a line window.
    pub fn read_text_file(&self, params: &JValue) -> Result<JValue, Refusal> {
        self.guard_fs(false, params)?;
        let target = self.inside(params.get("path"))?;
        let root = self.host.root();
        let content = files::read(&target, MAX_FILE_BYTES, root.as_deref()).map_err(file_refusal)?;
        let line = nullish_or(params.get("line"), JValue::Null);
        let limit = nullish_or(params.get("limit"), JValue::Null);
        if line == JValue::Null && limit == JValue::Null {
            return Ok(JValue::obj([("content", JValue::Str(content))]));
        }
        let all: Vec<&str> = content.split('\n').collect();
        let n = js_number(&line);
        let from = ((if n.is_nan() || n == 0.0 { 1.0 } else { n }) - 1.0).max(0.0);
        let start = slice_index(from, all.len());
        let end = if limit.truthy() {
            slice_index(from + js_number(&limit), all.len())
        } else {
            all.len()
        };
        let window = all.get(start..end.max(start)).unwrap_or(&[]).join("\n");
        Ok(JValue::obj([("content", JValue::Str(window))]))
    }

    /// `fs/write_text_file`: checked again at write time, recorded as `tool.completed`.
    pub fn write_text_file(&self, params: &JValue) -> Result<JValue, Refusal> {
        self.guard_fs(true, params)?;
        let content = params.get("content");
        let text = if content.is_nullish() {
            String::new()
        } else {
            js_json::to_js_string(content).unwrap_or_default()
        };
        let bytes: usize = text.chars().map(char_bytes).sum();
        if bytes as u64 > MAX_FILE_BYTES {
            return Err(Refusal::invalid("File too large"));
        }
        let target = self.inside(params.get("path"))?;
        let root = self.host.root();
        files::write(&target, &text, root.as_deref()).map_err(file_refusal)?;
        self.event(
            "tool.completed",
            JValue::obj([
                ("name", s("write_file")),
                ("path", params.get("path").clone()),
                ("bytes", JValue::Num(bytes as f64)),
            ]),
        )?;
        Ok(JValue::Null)
    }

    /// `session/update`. A failure (the journal refusing) ends the update where Node's throw did.
    pub fn session_update(&self, update: &JValue) -> Result<(), Refusal> {
        let kind = or(update.get("sessionUpdate"), update.get("type").clone());
        let mut st = self.lock();
        match kind.as_str() {
            Some("tool_call") => {
                if (!st.output_buffer.is_empty() || st.pending_high.is_some())
                    && st.early_flushes < MAX_EARLY_FLUSHES
                {
                    self.append_output(&mut st, "", true)?;
                    self.emit_output(&mut st)?;
                    st.early_flushes += 1;
                }
                if st.output_bytes > 0 {
                    st.break_after_tool = true;
                }
                st.counts.tools += 1;
                known(&mut st, update);
                let classified = actions::classify(update);
                let name = or(update.get("title"), or(update.get("kind"), s("tool")));
                if update.get("toolCallId").truthy() {
                    st.names.insert(key_of(update.get("toolCallId")), name.clone());
                }
                drop(st);
                self.event(
                    "tool.started",
                    JValue::obj([
                        ("id", update.get("toolCallId").clone()),
                        ("name", name),
                        ("action", classified.get("action").clone()),
                        ("kind", or(update.get("kind"), JValue::Null)),
                    ]),
                )?;
            }
            Some("tool_call_update") => {
                if st.output_bytes > 0 {
                    st.break_after_tool = true;
                }
                let status = update.get("status").as_str();
                let done = matches!(status, Some("completed" | "failed"));
                let id = update.get("toolCallId");
                if done {
                    let key = key_of(id);
                    st.calls.retain(|(k, _)| *k != key);
                } else {
                    known(&mut st, update);
                }
                if done {
                    let exit = meta::read_exit_code(update);
                    if exit != JValue::Null && st.exits.len() < MAX_EXITS {
                        let name = st.names.get(&key_of(id)).cloned().filter(JValue::truthy).unwrap_or(JValue::Null);
                        st.exits.push(JValue::obj([
                            ("id", nullish_or(id, JValue::Null)),
                            ("name", name),
                            ("exitCode", exit.clone()),
                        ]));
                    }
                    drop(st);
                    self.event(
                        "tool.completed",
                        JValue::obj([
                            ("id", id.clone()),
                            ("failed", JValue::Bool(status == Some("failed"))),
                            ("exitCode", exit),
                            ("content", summarize_content(update.get("content"))),
                        ]),
                    )?;
                }
            }
            Some("plan") => {
                drop(st);
                let entries: Vec<JValue> = match update.get("entries") {
                    JValue::Arr(e) => e.clone(),
                    _ => Vec::new(),
                };
                let contents: Vec<JValue> = entries
                    .iter()
                    .take(MAX_CODE_PLAN_ENTRIES)
                    .map(|e| nullish_or(e.get("content"), s("")))
                    .collect();
                let (questions, truncated) = bound_code_plan(&contents, entries.len() > MAX_CODE_PLAN_ENTRIES);
                self.event(
                    "plan.proposed",
                    JValue::obj([
                        ("question", JValue::Null),
                        ("subQuestions", questions),
                        ("truncated", JValue::Bool(truncated)),
                    ]),
                )?;
            }
            Some("usage_update") => {
                let reported = [update, update.get("usage"), update.get("_meta")]
                    .into_iter()
                    .map(meta::read_usage)
                    .find(JValue::truthy);
                if let Some(r) = reported {
                    st.reported_usage = r;
                }
                let window = meta::read_context(update);
                if window.truthy() {
                    st.reported_context = window;
                }
            }
            Some(k @ ("agent_message_chunk" | "agent_thought_chunk")) => {
                let thought = k == "agent_thought_chunk";
                if !thought {
                    st.message_chunks += 1;
                    let content = update.get("content");
                    if content.get("type").as_str() == Some("text") {
                        if let Some(text) = content.get("text").as_str().filter(|t| !t.is_empty()) {
                            if st.break_after_tool {
                                self.append_output(&mut st, "\n\n", false)?;
                            }
                            st.break_after_tool = false;
                            let text = text.to_string();
                            self.append_output(&mut st, &text, false)?;
                        }
                    }
                }
                // Thoughts are progress, not stored reasoning.
                self.set_stage(&mut st, if thought { "thinking" } else { "writing" })?;
            }
            _ => {}
        }
        Ok(())
    }

    fn emit_output(&self, st: &mut State) -> Result<(), Refusal> {
        if st.output_buffer.is_empty() && (!st.output_truncated || st.truncation_recorded) {
            return Ok(());
        }
        if st.output_events >= MAX_ASSISTANT_OUTPUT_EVENTS {
            return Err(Refusal {
                code: None,
                message: "Assistant output event limit reached".into(),
                data: None,
            });
        }
        let data = JValue::obj([
            ("text", JValue::Str(st.output_buffer.clone())),
            ("truncated", JValue::Bool(st.output_truncated && !st.truncation_recorded)),
        ]);
        self.event("assistant.output", data)?;
        st.output_events += 1;
        st.output_buffer.clear();
        st.output_buffer_bytes = 0;
        if st.output_truncated {
            st.truncation_recorded = true;
        }
        Ok(())
    }

    fn append_output(&self, st: &mut State, text: &str, last: bool) -> Result<(), Refusal> {
        if st.output_truncated {
            return Ok(());
        }
        let mut source: Vec<char> = Vec::new();
        let mut rest = text.chars().peekable();
        if let Some(high) = st.pending_high.take() {
            // A JS string joins a high surrogate and the low one after it into one character.
            let joined = rest.peek().copied().and_then(|c| {
                let low = wtf16::lone_unit(c).filter(|u| (0xDC00..=0xDFFF).contains(u))?;
                let hi = wtf16::lone_unit(high)?;
                char::from_u32(0x10000 + ((u32::from(hi) - 0xD800) << 10) + (u32::from(low) - 0xDC00))
            });
            match joined {
                Some(c) => {
                    rest.next();
                    source.push(c);
                }
                None => source.push(high),
            }
        }
        source.extend(rest);
        if !last {
            if let Some(&c) = source.last() {
                if wtf16::lone_unit(c).is_some_and(|u| (0xD800..=0xDBFF).contains(&u)) {
                    st.pending_high = Some(c);
                    source.pop();
                }
            }
        }
        for c in source {
            let c = if wtf16::lone_unit(c).is_some() { '\u{FFFD}' } else { c };
            let bytes = c.len_utf8();
            if st.output_bytes + bytes > MAX_ASSISTANT_OUTPUT_BYTES {
                st.output_truncated = true;
                self.emit_output(st)?;
                break;
            }
            if st.output_buffer_bytes + bytes > OUTPUT_BATCH_BYTES {
                self.emit_output(st)?;
            }
            st.output_buffer.push(c);
            st.output_buffer_bytes += bytes;
            st.output_bytes += bytes;
            if st.output_buffer_bytes == OUTPUT_BATCH_BYTES {
                self.emit_output(st)?;
            }
        }
        Ok(())
    }

    fn set_stage(&self, st: &mut State, stage: &str) -> Result<(), Refusal> {
        if st.last_stage.as_deref() == Some(stage) {
            st.pending_stage = None;
            return Ok(());
        }
        let at = self.host.now();
        if at - st.last_stage_at < 500.0 {
            st.pending_stage = Some(stage.to_string());
            return Ok(());
        }
        self.event("progress", JValue::obj([("stage", s(stage))]))?;
        st.last_stage = Some(stage.to_string());
        st.last_stage_at = at;
        st.pending_stage = None;
        Ok(())
    }

    /// `flushOutput`: what is buffered goes out, and a stage that was held back.
    pub fn flush_output(&self) -> Result<(), Refusal> {
        let mut st = self.lock();
        self.append_output(&mut st, "", true)?;
        self.emit_output(&mut st)?;
        if let Some(stage) = st.pending_stage.take() {
            if st.last_stage.as_deref() != Some(stage.as_str()) {
                self.event("progress", JValue::obj([("stage", s(&stage))]))?;
            }
        }
        Ok(())
    }

    /// `{ tools, approvals, allowed, refused, denied, workspace, violations? }`.
    pub fn summary(&self) -> JValue {
        let st = self.lock();
        let c = &st.counts;
        let mut out = vec![
            ("tools".to_string(), JValue::Num(c.tools as f64)),
            ("approvals".to_string(), JValue::Num(c.approvals as f64)),
            ("allowed".to_string(), JValue::Num(c.allowed as f64)),
            ("refused".to_string(), JValue::Num(c.refused as f64)),
            ("denied".to_string(), JValue::Num(c.denied as f64)),
            ("workspace".to_string(), s(&self.task.workspace_path)),
        ];
        if let Some(g) = &st.guard {
            out.push(("violations".into(), JValue::Num(g.violations() as f64)));
        }
        JValue::Obj(out)
    }

    /// What the run can say about itself: code-meta.cjs `summarize` over what the harness streamed.
    pub fn meta(&self, agent: &JValue, usage: &JValue) -> JValue {
        let st = self.lock();
        meta::summarize(&JValue::obj([
            ("agent", or(agent, JValue::Obj(Vec::new()))),
            ("usage", or(&st.reported_usage, usage.clone())),
            ("context", st.reported_context.clone()),
            ("exits", JValue::Arr(st.exits.clone())),
            ("messageChunks", JValue::Num(st.message_chunks as f64)),
        ]))
    }
}

fn file_refusal(e: files::FileRefusal) -> Refusal {
    Refusal {
        code: e.code.map(|c| c as f64),
        message: e.message,
        data: None,
    }
}

/// `path.resolve` of an absolute path.
fn normalize(p: &str) -> String {
    let mut parts: Vec<&str> = Vec::new();
    for part in p.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            other => parts.push(other),
        }
    }
    format!("/{}", parts.join("/"))
}

/// The announced call, merged by id; bounded; the request's own fields win later.
fn known(st: &mut State, update: &JValue) {
    let id = update.get("toolCallId");
    if !id.truthy() {
        return;
    }
    let key = key_of(id);
    let fields: Vec<(String, JValue)> = match update {
        JValue::Obj(items) => items
            .iter()
            .filter(|(k, v)| !v.is_nullish() && k != "sessionUpdate")
            .cloned()
            .collect(),
        _ => Vec::new(),
    };
    let at = st.calls.iter().position(|(k, _)| *k == key);
    if at.is_none() && st.calls.len() >= MAX_KNOWN_CALLS && !st.calls.is_empty() {
        st.calls.remove(0);
    }
    match st.calls.iter_mut().find(|(k, _)| *k == key) {
        Some((_, v)) => {
            let mut merged = match v {
                JValue::Obj(items) => items.clone(),
                _ => Vec::new(),
            };
            for (k, val) in fields {
                wtf16::set(&mut merged, &k, val);
            }
            *v = JValue::Obj(merged);
        }
        None => {
            let mut merged = Vec::new();
            for (k, val) in fields {
                wtf16::set(&mut merged, &k, val);
            }
            st.calls.push((key, JValue::Obj(merged)));
        }
    }
}

/// Delete and git push never get a standing approval.
fn can_stand(action: &str) -> bool {
    action != "delete" && action != "git_push"
}

fn diff_of(tool_call: &JValue) -> JValue {
    let JValue::Arr(blocks) = tool_call.get("content") else {
        return JValue::Null;
    };
    blocks
        .iter()
        .find(|b| b.truthy() && b.get("type").as_str() == Some("diff") && b.get("path").truthy())
        .map_or(JValue::Null, |b| {
            JValue::obj([
                ("path", b.get("path").clone()),
                ("oldText", clip(b.get("oldText"))),
                ("newText", clip(b.get("newText"))),
            ])
        })
}

fn summarize_content(content: &JValue) -> JValue {
    let JValue::Arr(blocks) = content else {
        return JValue::Null;
    };
    let texts: Vec<String> = blocks
        .iter()
        .filter_map(|b| b.truthy().then(|| b.get("text").as_str().map(str::to_string)).flatten())
        .filter(|t| !t.is_empty())
        .collect();
    let joined = clip(&JValue::Str(texts.join("\n")));
    or(&joined, JValue::Null)
}
