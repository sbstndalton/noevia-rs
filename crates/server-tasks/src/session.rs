//! ACP callbacks owned by one native code task. Announcements are snapshotted synchronously;
//! approval waits run concurrently and keep no session mutex across an await.
use crate::{
    acp::{HandlerFuture, Handlers, RpcError},
    files, meta,
    output::Output,
    policy, Decision,
};
use js_json::{wtf16, JValue};
use std::{
    future::Future,
    path::PathBuf,
    pin::Pin,
    sync::{Arc, Mutex},
};
use tokio::sync::watch;
pub type Answer = Pin<Box<dyn Future<Output = Decision> + Send + 'static>>;
pub trait Context: Send + Sync {
    fn active(&self) -> Result<(), RpcError>;
    fn event(&self, kind: &str, data: JValue) -> Result<(), RpcError>;
    /// Register before returning the future, in ACP receive order.
    fn approval(&self, request: JValue) -> Answer;
    fn now(&self) -> i64;
}
fn object(rows: Vec<(&str, JValue)>) -> JValue {
    JValue::Obj(rows.into_iter().map(|(k, v)| (k.into(), v)).collect())
}
fn text(s: &str) -> JValue {
    JValue::Str(s.into())
}
fn set(value: &mut JValue, key: &str, item: JValue) {
    if let JValue::Obj(rows) = value {
        if let Some(row) = rows.iter_mut().find(|(k, _)| k == key) {
            row.1 = item;
        } else {
            rows.push((key.into(), item));
        }
    }
}
fn same_id(a: &JValue, b: &JValue) -> bool {
    !matches!(a, JValue::Obj(_) | JValue::Arr(_)) && a == b
}
fn merge(base: &JValue, patch: &JValue, update: bool) -> JValue {
    let mut out = if base.is_object() {
        base.clone()
    } else {
        object(vec![])
    };
    if let JValue::Obj(rows) = patch {
        for (key, value) in rows {
            if !value.is_nullish() && (!update || key != "sessionUpdate") {
                set(&mut out, key, value.clone());
            }
        }
    }
    out
}
fn fallback(value: &JValue, default: &str) -> JValue {
    if value.truthy() {
        value.clone()
    } else {
        text(default)
    }
}
fn failure(message: &str) -> RpcError {
    RpcError {
        code: -32603,
        message: message.into(),
        data: None,
    }
}
fn reject(selected: JValue, reason: &str) -> JValue {
    let mut out = selected;
    set(
        &mut out,
        "_meta",
        object(vec![("noevia", object(vec![("reason", text(reason))]))]),
    );
    out
}
struct State {
    blanket: Vec<String>,
    calls: Vec<(JValue, JValue)>,
    names: Vec<(JValue, JValue)>,
    tools: u64,
    approvals: u64,
    allowed: u64,
    refused: u64,
    denied: u64,
    exits: Vec<JValue>,
    usage: JValue,
    context: JValue,
    chunks: u64,
    output: Output,
    stage: Option<String>,
    stage_at: Option<i64>,
    pending_stage: Option<String>,
}
impl Default for State {
    fn default() -> Self {
        Self {
            blanket: vec![],
            calls: vec![],
            names: vec![],
            tools: 0,
            approvals: 0,
            allowed: 0,
            refused: 0,
            denied: 0,
            exits: vec![],
            usage: JValue::Null,
            context: JValue::Null,
            chunks: 0,
            output: Output::default(),
            stage: None,
            stage_at: None,
            pending_stage: None,
        }
    }
}
pub struct Session {
    task: String,
    root: PathBuf,
    capabilities: Vec<String>,
    domains: Vec<String>,
    context: Arc<dyn Context>,
    state: Mutex<State>,
    ended: watch::Sender<bool>,
}
impl Session {
    pub fn new(
        task: String,
        root: PathBuf,
        capabilities: Vec<String>,
        domains: Vec<String>,
        context: Arc<dyn Context>,
    ) -> Self {
        let (ended, _) = watch::channel(false);
        Self {
            task,
            root,
            capabilities,
            domains,
            context,
            state: Mutex::new(State::default()),
            ended,
        }
    }
    pub fn end(&self) {
        self.ended.send_replace(true);
    }
    fn known(state: &mut State, update: &JValue) {
        let id = update.get("toolCallId");
        if !id.truthy() {
            return;
        }
        if let Some((_, known)) = state.calls.iter_mut().find(|(key, _)| same_id(key, id)) {
            *known = merge(known, update, true);
            return;
        }
        if state.calls.len() >= 200 {
            state.calls.remove(0);
        }
        state
            .calls
            .push((id.clone(), merge(&JValue::Null, update, true)));
    }
    fn permission(self: Arc<Self>, params: JValue) -> HandlerFuture<'static> {
        let prepared = self.prepare_permission(&params);
        match prepared {
            Err(error) => Box::pin(async move { Err(error) }),
            Ok(Prepared::Ready(answer)) => Box::pin(async move { Ok(answer) }),
            Ok(Prepared::Waiting {
                classified,
                options,
                answer,
                mut ended,
            }) => {
                Box::pin(async move {
                    let decision = tokio::select! {biased;answer=answer=>answer,_ = ended.changed()=>Decision::Aborted};
                    let action = classified.get("action").as_str().unwrap_or("");
                    self.context.event(
                        "approval.decided",
                        object(vec![
                            ("decision", text(decision.as_str())),
                            ("action", text(action)),
                        ]),
                    )?;
                    let mut state = self
                        .state
                        .lock()
                        .map_err(|_| failure("Task status is unavailable."))?;
                    if matches!(decision, Decision::Approve | Decision::ApproveAll) {
                        if decision == Decision::ApproveAll
                            && action != "delete"
                            && action != "git_push"
                            && classified.get("standable") != &JValue::Bool(false)
                            && !state.blanket.iter().any(|s| s == action)
                        {
                            state.blanket.push(action.into());
                        }
                        state.allowed += 1;
                        return Ok(policy::pick(&options, "allow_once"));
                    }
                    state.refused += 1;
                    let reason=match decision{Decision::Timeout=>"Nobody answered the approval request in time, so the action was not run.",Decision::Aborted=>"The task was stopped before this action was approved, so it was not run.",_=>"The person declined this action."};
                    Ok(reject(policy::pick(&options, "reject_once"), reason))
                })
            }
        }
    }
    fn prepare_permission(&self, params: &JValue) -> Result<Prepared, RpcError> {
        let stated = match params.get("toolCall") {
            JValue::Undefined => object(vec![]),
            v => v.clone(),
        };
        let options = match params.get("options") {
            JValue::Undefined => JValue::Arr(vec![]),
            v => v.clone(),
        };
        let mut state = self
            .state
            .lock()
            .map_err(|_| failure("Task status is unavailable."))?;
        let announced = if stated.get("toolCallId").truthy() {
            state
                .calls
                .iter()
                .find(|(id, _)| same_id(id, stated.get("toolCallId")))
                .map(|(_, call)| call.clone())
        } else {
            None
        };
        let tool = announced
            .as_ref()
            .map_or_else(|| stated.clone(), |base| merge(base, &stated, false));
        let mut classified = policy::classify(&tool);
        if announced.is_some() && classified.get("approval").as_str() == Some("never") {
            let own = policy::classify(&stated);
            if own.get("unverified") == &JValue::Bool(true) {
                classified = own;
            } else if own.get("approval").as_str() != Some("never") {
                set(&mut classified, "approval", own.get("approval").clone());
                let standable = classified.get("standable") != &JValue::Bool(false)
                    && own.get("standable") != &JValue::Bool(false);
                set(&mut classified, "standable", JValue::Bool(standable));
            }
        }
        state.approvals += 1;
        let inside = match classified.get("paths") {
            JValue::Arr(paths) if !paths.is_empty() => Some(
                paths
                    .iter()
                    .all(|p| p.as_str().is_some_and(|p| files::contains(&self.root, p))),
            ),
            _ => None,
        };
        let verdict = policy::decide(&classified, &self.capabilities, &self.domains, inside);
        let action = classified.get("action").clone();
        if verdict.get("decision").as_str() == Some("deny") {
            state.denied += 1;
            self.context.event(
                "approval.decided",
                object(vec![
                    ("decision", text("denied")),
                    ("action", action),
                    ("reason", verdict.get("reason").clone()),
                    ("automatic", JValue::Bool(true)),
                ]),
            )?;
            return Ok(Prepared::Ready(reject(
                policy::pick(&options, "reject_once"),
                verdict.get("reason").as_str().unwrap_or("refused"),
            )));
        }
        if verdict.get("decision").as_str() == Some("allow") {
            state.allowed += 1;
            return Ok(Prepared::Ready(policy::pick(&options, "allow_once")));
        }
        let no_standing = classified.get("standable") == &JValue::Bool(false)
            || verdict.get("standable") == &JValue::Bool(false);
        if !no_standing
            && state
                .blanket
                .iter()
                .any(|s| Some(s.as_str()) == action.as_str())
        {
            state.allowed += 1;
            return Ok(Prepared::Ready(policy::pick(&options, "allow_once")));
        }
        let diff = match tool.get("content") {
            JValue::Arr(rows) => rows
                .iter()
                .find(|row| row.get("type").as_str() == Some("diff") && row.get("path").truthy())
                .map(|row| {
                    object(vec![
                        ("path", row.get("path").clone()),
                        ("oldText", clip(row.get("oldText"), 4000)),
                        ("newText", clip(row.get("newText"), 4000)),
                    ])
                })
                .unwrap_or(JValue::Null),
            _ => JValue::Null,
        };
        let card = object(vec![
            ("taskId", text(&self.task)),
            ("action", action),
            ("title", fallback(tool.get("title"), "")),
            ("kind", fallback(tool.get("kind"), "")),
            ("command", classified.get("command").clone()),
            ("paths", classified.get("paths").clone()),
            ("reason", verdict.get("reason").clone()),
            (
                "arguments",
                if tool.get("rawInput").is_nullish() {
                    JValue::Null
                } else {
                    tool.get("rawInput").clone()
                },
            ),
            ("diff", diff),
        ]);
        self.context.event("approval.requested", card.clone())?;
        if no_standing {
            set(&mut classified, "standable", JValue::Bool(false));
        }
        let ended = self.ended.subscribe();
        let answer = if *ended.borrow() {
            Box::pin(async { Decision::Aborted }) as Answer
        } else {
            self.context.approval(card)
        };
        Ok(Prepared::Waiting {
            classified,
            options,
            answer,
            ended,
        })
    }
    fn file(&self, method: &str, params: &JValue) -> Result<JValue, RpcError> {
        self.context.active()?;
        let target = params.get("path").as_str().ok_or_else(|| RpcError {
            code: -32602,
            message: "Outside this task’s workspace".into(),
            data: None,
        })?;
        if method == "fs/read_text_file" {
            let content = files::read(&self.root, target, files::MAX_FILE_BYTES)?;
            let line = params.get("line");
            let limit = params.get("limit");
            if line.is_nullish() && limit.is_nullish() {
                return Ok(object(vec![("content", text(&content))]));
            }
            let all = content.split('\n').collect::<Vec<_>>();
            let n = number(line);
            let from = (if n == 0.0 || n.is_nan() { 1.0 } else { n } - 1.0).max(0.0);
            let start = index(from, all.len());
            let end = if limit.truthy() {
                index(from + number(limit), all.len())
            } else {
                all.len()
            };
            let slice = all.get(start..end).unwrap_or(&[]).join("\n");
            Ok(object(vec![("content", text(&slice))]))
        } else {
            let content = params.get("content");
            let content = if content.is_nullish() {
                String::new()
            } else {
                js_json::to_js_string(content)
                    .map_err(|_| failure("Cannot convert object to primitive value"))?
            };
            let bytes = files::write(&self.root, target, &content, files::MAX_FILE_BYTES)?;
            self.context.event(
                "tool.completed",
                object(vec![
                    ("name", text("write_file")),
                    ("path", text(target)),
                    ("bytes", JValue::Num(bytes as f64)),
                ]),
            )?;
            Ok(JValue::Null)
        }
    }
    fn stage(&self, state: &mut State, stage: &str) -> Result<(), RpcError> {
        if state.stage.as_deref() == Some(stage) {
            state.pending_stage = None;
            return Ok(());
        }
        let now = self.context.now();
        if state
            .stage_at
            .is_some_and(|at| now.saturating_sub(at) < 500)
        {
            state.pending_stage = Some(stage.into());
            return Ok(());
        }
        self.context
            .event("progress", object(vec![("stage", text(stage))]))?;
        state.stage = Some(stage.into());
        state.stage_at = Some(now);
        state.pending_stage = None;
        Ok(())
    }
    fn update_inner(&self, update: JValue) -> Result<(), RpcError> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| failure("Task status is unavailable."))?;
        let kind = if update.get("sessionUpdate").truthy() {
            update.get("sessionUpdate")
        } else {
            update.get("type")
        };
        let mut emit = |data| self.context.event("assistant.output", data);
        match kind.as_str().unwrap_or("") {
            "tool_call" => {
                state.output.tool(true, &mut emit).map_err(output_error)?;
                state.tools += 1;
                Self::known(&mut state, &update);
                let name = if update.get("title").truthy() {
                    update.get("title").clone()
                } else {
                    fallback(update.get("kind"), "tool")
                };
                if update.get("toolCallId").truthy() {
                    if let Some((_, value)) = state
                        .names
                        .iter_mut()
                        .find(|(id, _)| same_id(id, update.get("toolCallId")))
                    {
                        *value = name.clone();
                    } else {
                        state
                            .names
                            .push((update.get("toolCallId").clone(), name.clone()));
                    }
                }
                self.context.event(
                    "tool.started",
                    object(vec![
                        ("id", update.get("toolCallId").clone()),
                        ("name", name),
                        ("action", policy::classify(&update).get("action").clone()),
                        (
                            "kind",
                            if update.get("kind").truthy() {
                                update.get("kind").clone()
                            } else {
                                JValue::Null
                            },
                        ),
                    ]),
                )?;
            }
            "tool_call_update" => {
                state.output.tool(false, &mut emit).map_err(output_error)?;
                if matches!(update.get("status").as_str(), Some("completed" | "failed")) {
                    state
                        .calls
                        .retain(|(id, _)| !same_id(id, update.get("toolCallId")));
                    let exit = meta::exit(&update);
                    if exit != JValue::Null && state.exits.len() < 200 {
                        let name = state
                            .names
                            .iter()
                            .find(|(id, _)| same_id(id, update.get("toolCallId")))
                            .map(|(_, name)| name.clone())
                            .unwrap_or(JValue::Null);
                        state.exits.push(object(vec![
                            (
                                "id",
                                if update.get("toolCallId").is_nullish() {
                                    JValue::Null
                                } else {
                                    update.get("toolCallId").clone()
                                },
                            ),
                            ("name", name),
                            ("exitCode", exit.clone()),
                        ]));
                    }
                    let content = match update.get("content") {
                        JValue::Arr(rows) => {
                            let joined = rows
                                .iter()
                                .filter_map(|v| v.get("text").as_str())
                                .filter(|s| !s.is_empty())
                                .collect::<Vec<_>>()
                                .join("\n");
                            if joined.is_empty() {
                                JValue::Null
                            } else {
                                JValue::Str(wtf16::slice(&joined, 4000))
                            }
                        }
                        _ => JValue::Null,
                    };
                    self.context.event(
                        "tool.completed",
                        object(vec![
                            ("id", update.get("toolCallId").clone()),
                            (
                                "failed",
                                JValue::Bool(update.get("status").as_str() == Some("failed")),
                            ),
                            ("exitCode", exit),
                            ("content", content),
                        ]),
                    )?;
                } else {
                    Self::known(&mut state, &update);
                }
            }
            "plan" => {
                let entries = match update.get("entries") {
                    JValue::Arr(rows) => rows.as_slice(),
                    _ => &[],
                };
                self.context.event(
                    "plan.proposed",
                    object(vec![
                        ("question", JValue::Null),
                        (
                            "subQuestions",
                            JValue::Arr(
                                entries
                                    .iter()
                                    .take(20)
                                    .map(|v| {
                                        if v.get("content").is_nullish() {
                                            text("")
                                        } else {
                                            v.get("content").clone()
                                        }
                                    })
                                    .collect(),
                            ),
                        ),
                        ("truncated", JValue::Bool(entries.len() > 20)),
                    ]),
                )?;
            }
            "usage_update" => {
                for source in [&update, update.get("usage"), update.get("_meta")] {
                    let found = meta::usage(source);
                    if found != JValue::Null {
                        state.usage = found;
                        break;
                    }
                }
                let context = meta::context(&update);
                if context != JValue::Null {
                    state.context = context;
                }
            }
            "agent_message_chunk" | "agent_thought_chunk" => {
                if kind.as_str() == Some("agent_message_chunk") {
                    state.chunks += 1;
                    let content = update.get("content");
                    if content.get("type").as_str() == Some("text") {
                        if let Some(text) = content.get("text").as_str().filter(|s| !s.is_empty()) {
                            state
                                .output
                                .message(text, &mut emit)
                                .map_err(output_error)?;
                        }
                    }
                }
                self.stage(
                    &mut state,
                    if kind.as_str() == Some("agent_thought_chunk") {
                        "thinking"
                    } else {
                        "writing"
                    },
                )?;
            }
            _ => {}
        }
        Ok(())
    }
    pub fn flush(&self) -> Result<(), RpcError> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| failure("Task status is unavailable."))?;
        state
            .output
            .flush(&mut |data| self.context.event("assistant.output", data))
            .map_err(output_error)?;
        if let Some(stage) = state
            .pending_stage
            .take()
            .filter(|s| Some(s.as_str()) != state.stage.as_deref())
        {
            self.context
                .event("progress", object(vec![("stage", text(&stage))]))?;
        }
        Ok(())
    }
    pub fn summary(&self) -> Result<JValue, RpcError> {
        let state = self
            .state
            .lock()
            .map_err(|_| failure("Task status is unavailable."))?;
        Ok(object(vec![
            ("tools", JValue::Num(state.tools as f64)),
            ("approvals", JValue::Num(state.approvals as f64)),
            ("allowed", JValue::Num(state.allowed as f64)),
            ("refused", JValue::Num(state.refused as f64)),
            ("denied", JValue::Num(state.denied as f64)),
            ("workspace", text(&self.root.to_string_lossy())),
        ]))
    }
    pub fn metadata(&self, agent: JValue, usage: JValue) -> Result<JValue, RpcError> {
        let state = self
            .state
            .lock()
            .map_err(|_| failure("Task status is unavailable."))?;
        Ok(meta::summary(&object(vec![
            ("agent", agent),
            (
                "usage",
                if state.usage.truthy() {
                    state.usage.clone()
                } else {
                    usage
                },
            ),
            ("context", state.context.clone()),
            ("exits", JValue::Arr(state.exits.clone())),
            ("messageChunks", JValue::Num(state.chunks as f64)),
        ])))
    }
}
impl Handlers for Session {
    fn request(self: Arc<Self>, method: &str, params: JValue) -> HandlerFuture<'static> {
        match method {
            "session/request_permission" => self.permission(params),
            "fs/read_text_file" | "fs/write_text_file" => {
                let result = self.file(method, &params);
                Box::pin(async move { result })
            }
            _ => Box::pin(async { Err(failure("Method not supported by this client")) }),
        }
    }
    fn update(&self, update: JValue) {
        let _ = self.update_inner(update);
    }
}
impl Drop for Session {
    fn drop(&mut self) {
        self.ended.send_replace(true);
    }
}
enum Prepared {
    Ready(JValue),
    Waiting {
        classified: JValue,
        options: JValue,
        answer: Answer,
        ended: watch::Receiver<bool>,
    },
}
fn clip(value: &JValue, max: usize) -> JValue {
    value
        .as_str()
        .map_or(JValue::Null, |s| JValue::Str(wtf16::slice(s, max)))
}
fn output_error(error: crate::output::Error<RpcError>) -> RpcError {
    match error {
        crate::output::Error::Event(error) => error,
        crate::output::Error::Limit => failure("Assistant output event limit reached"),
    }
}
fn number(value: &JValue) -> f64 {
    match value {
        JValue::Null => 0.0,
        JValue::Undefined => f64::NAN,
        JValue::Bool(v) => {
            if *v {
                1.0
            } else {
                0.0
            }
        }
        JValue::Num(n) => *n,
        _ => js_json::to_js_string(value)
            .ok()
            .map_or(f64::NAN, |s| server_account::util::js_number(&s)),
    }
}
fn index(value: f64, len: usize) -> usize {
    if value.is_nan() {
        0
    } else if value < 0.0 {
        ((len as f64 + value.trunc()).max(0.0)) as usize
    } else {
        value.trunc().min(len as f64) as usize
    }
}
#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;
    struct Fake {
        answer: Decision,
        events: Mutex<Vec<JValue>>,
        cards: Mutex<Vec<JValue>>,
    }
    impl Context for Fake {
        fn active(&self) -> Result<(), RpcError> {
            Ok(())
        }
        fn event(&self, kind: &str, data: JValue) -> Result<(), RpcError> {
            if kind.starts_with("approval.") {
                self.events
                    .lock()
                    .unwrap()
                    .push(object(vec![("type", text(kind)), ("data", data)]));
            }
            Ok(())
        }
        fn approval(&self, request: JValue) -> Answer {
            self.cards.lock().unwrap().push(request);
            let answer = self.answer;
            Box::pin(async move { answer })
        }
        fn now(&self) -> i64 {
            1234
        }
    }
    #[tokio::test]
    async fn session_permissions_match_node_harness() {
        let fixture =
            wtf16::parse(include_str!("../tests/fixtures/permissions-node.json")).unwrap();
        let JValue::Arr(cases) = fixture.get("cases") else {
            panic!()
        };
        assert_eq!(cases.len(), 11);
        for (index, case) in cases.iter().enumerate() {
            let input = case.get("input");
            let answer = match input.get("answer").as_str().unwrap() {
                "approve" => Decision::Approve,
                "approve_all" => Decision::ApproveAll,
                "timeout" => Decision::Timeout,
                "aborted" => Decision::Aborted,
                _ => Decision::Deny,
            };
            let context = Arc::new(Fake {
                answer,
                events: Mutex::new(vec![]),
                cards: Mutex::new(vec![]),
            });
            let root = tempfile::tempdir().unwrap();
            let JValue::Arr(capabilities) = input.get("capabilities") else {
                panic!()
            };
            let session = Arc::new(Session::new(
                "11111111-1111-4111-8111-111111111111".into(),
                root.path().into(),
                capabilities
                    .iter()
                    .map(|s| s.as_str().unwrap().to_owned())
                    .collect(),
                vec![],
                context.clone(),
            ));
            if input.get("announced").is_object() {
                let mut announcement = input.get("announced").clone();
                set(&mut announcement, "sessionUpdate", text("tool_call"));
                session.update(announcement);
            }
            let options = JValue::Arr(vec![
                object(vec![
                    ("optionId", text("allow")),
                    ("kind", text("allow_once")),
                ]),
                object(vec![
                    ("optionId", text("reject")),
                    ("kind", text("reject_once")),
                ]),
            ]);
            let mut answers = vec![];
            for _ in 0..input.get("repeat").as_num().unwrap() as usize {
                answers.push(
                    session
                        .clone()
                        .request(
                            "session/request_permission",
                            object(vec![
                                ("toolCall", input.get("call").clone()),
                                ("options", options.clone()),
                            ]),
                        )
                        .await
                        .unwrap(),
                );
            }
            session.end();
            let output = object(vec![
                (
                    "events",
                    JValue::Arr(context.events.lock().unwrap().clone()),
                ),
                ("answers", JValue::Arr(answers)),
                ("cards", JValue::Arr(context.cards.lock().unwrap().clone())),
            ]);
            assert_eq!(output, *case.get("output"), "case {index}");
        }
    }
}
