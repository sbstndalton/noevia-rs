//! Routing modes (#778, slice P5d, first part): where an Automatic turn goes when the account
//! (or the chat's project, #1007) chose a mode: `local`, `cloud` or `hybrid`. noevia-core
//! `server/chat.cjs` ~929-990 and `server/routing-modes.cjs` `resolveRoute`, `chatFlags`,
//! `cloudModel` and `hasDiaryContent`, then `offBoxTurn` (chat.cjs:54) and the meta event's
//! `routing` field.
//!
//! [`Modes`] owns chat.cjs's order and asks the caller for every seam ([`Seam`]), as
//! [`crate::roles::Routing`] does: `routingModes.enabled()`, `settings(project)`, the chat's list
//! for its remembered flags (`getProject(listProjectId)?.chats` or `freeChats()`), the cloud
//! provider row (`getProvider`), the fresh `autoRoles()` read, the hybrid route's pre-rules
//! (`preRule` of the message, of the attachment names and of the sent text), the sensitivity
//! check (`routingModes.sensitivity(routerInput(...))`), and the routing question: the event wire
//! opened, a choice id (`crypto.randomUUID()`), `openChoice` (port contract S9, only when wired),
//! the `route_pending` event, `awaitChoice`; then the codes-only `log` entry, the remembered
//! choice (`setChatFlags`, then the `route_remembered` event). It runs between P5b's `wantsAuto`
//! read and its provider read: a turn with a [`RouteTarget::row`] answers through that row
//! (`{ ...row, external: true }`, no `getProvider` of its own), and its model is
//! [`cloud_model`] of the routed role.
//!
//! Seams of this part that later slices port: `preRule` (the deterministic secret, IBAN, card and
//! health rules) and `routerInput` (what the sensitivity check reads) are the caller's;
//! [`PreText::Sent`] is the text [`sent_text`] joins. After the log entry and the `[routing]`
//! line ([`Seam::Line`]) the turn's abort is asked ([`Seam::Aborted`], chat.cjs's
//! `chatSignal.signal.aborted`), ahead of the cancel refusal and of the remembered choice: an
//! aborted turn ends its response and nothing else ([`Decided::aborted`]): no refusal, no flag
//! written, no event sent. Recorded turns abort while the sensitivity check or the routing
//! question runs (the response's `close`).
//!
//! The wire changes once the route is known (the off-box zone of the time part, the brain block
//! dropped (#815), the history filter, the hybrid route's private-tools move and read-file hints)
//! are [`crate::offbox`]. Later parts (not here): the route notes, tool results held from the
//! cloud (`holdForCloud`), the egress and ChatGPT checks.
//!
//! Recorded in `tests/fixtures/node-modes.v1.json`, written by `tests/record_modes.cjs` (whole
//! turns through `createChatHandler` with the routingModes seams stubbed); no named differences.

use js_json::{to_js_string, wtf16, JValue};

use crate::roles::{strict_eq, Halt, Refusal};
use crate::Thrown;

/// What the routing reads of the turn.
#[derive(Debug, Clone, Copy)]
pub struct ModesTurn<'a> {
    /// `wantsAuto` (P5b). False: nothing is asked and the turn has no route target.
    pub wants_auto: bool,
    /// The project record, `null` without one.
    pub project: &'a JValue,
    /// `body.projectId && project`: the chat lives in the project's list (else in the free chats).
    pub list_project: bool,
    /// `chatId` (`null` without one).
    pub chat_id: &'a JValue,
    /// `body.spaceId`.
    pub space_id: &'a JValue,
    pub message: &'a str,
    /// `attachmentNames`, see [`attachment_names`]; `None` when reading them threw (the turn
    /// throws once the roles are read).
    pub attachments: Option<&'a [String]>,
    /// `mappedHistory`.
    pub history: &'a JValue,
    /// `body.compactOnly` is truthy.
    pub compact_only: bool,
    pub default_provider_id: &'a str,
    /// `diaryExtras.PROJECT_ID`.
    pub diary_project_id: &'a str,
    /// `requestScope.getStore()?.workspace?.userId` is truthy.
    pub user: bool,
    /// `typeof routingModes.openChoice === 'function'`.
    pub open_choice: bool,
}

/// Which text a pre-rule reads.
#[derive(Debug, Clone, PartialEq)]
pub enum PreText {
    Message(String),
    /// `attachmentNames.join('\n')` (joined here from [`ModesTurn::attachments`]).
    Attachments(String),
    /// `sentText`, see [`sent_text`].
    Sent,
}

/// What [`Modes`] asks of its caller next.
#[derive(Debug, Clone, PartialEq)]
pub enum Seam {
    /// `routingModes.enabled()`.
    Enabled,
    /// `routingModes.settings(project)`.
    Settings,
    /// The chat list the remembered flags are read from: `Some(id)`: `getProject(id)?.chats`;
    /// `None`: `freeChats()`.
    ChatList(Option<JValue>),
    /// `getProvider(id)`: the cloud provider's row.
    Provider(JValue),
    /// `autoRoles()`.
    AutoRoles,
    /// `rm.preRule(text)`: a flag code or `null`.
    PreRule(PreText),
    /// `routingModes.sensitivity(routerInput(...))`.
    Sensitivity,
    /// The event wire opened if it is not yet (`earlyStream`).
    OpenStream,
    /// `crypto.randomUUID()`.
    ChoiceId,
    /// `routingModes.openChoice({ id, userId, chatId })`.
    OpenChoice(String),
    /// Send an event.
    Send(JValue),
    /// `routingModes.awaitChoice({ id, userId, chatId, abortSignal })`.
    AwaitChoice(String),
    /// `routingModes.log(entry)`; its failure is ignored.
    Log(JValue),
    /// The `[routing] ...` console line, written right after the log entry (also before the
    /// cancel refusal).
    Line(String),
    /// `chatSignal.signal.aborted`: answered `true` when the turn was aborted.
    Aborted,
    /// `routingModes.setChatFlags(listProjectId, chatId, patch)`; its failure is ignored.
    SetChatFlags {
        project_id: JValue,
        chat_id: JValue,
        patch: JValue,
    },
}

/// The caller's answer to the last [`Seam`]; [`Reply::Start`] first. `Err(Thrown)`: the call
/// threw (or its promise rejected).
#[derive(Debug, Clone, PartialEq)]
pub enum Reply {
    Start,
    Value(Result<JValue, Thrown>),
    /// `crypto.randomUUID()`'s id.
    Id(String),
    /// A seam whose answer is not read (`OpenStream`, `Send`, `Log`, `Line`, `SetChatFlags`).
    Done,
}

/// `routeTarget`.
#[derive(Debug, Clone, PartialEq)]
pub struct RouteTarget {
    /// `'local'` or `'cloud'`.
    pub route: &'static str,
    pub reason: JValue,
    /// The check's cause code, for `check-unavailable`.
    pub detail: Option<JValue>,
    /// `settings.cloud`.
    pub cloud: JValue,
    /// The cloud provider's row on a cloud route.
    pub row: Option<JValue>,
    /// #1646: a hybrid cloud route without a grant.
    pub hybrid_ungranted: bool,
}

impl RouteTarget {
    /// The meta event's `routing`.
    pub fn meta_routing(&self) -> JValue {
        let mut items = vec![
            ("route".to_string(), JValue::from(self.route)),
            ("reason".to_string(), self.reason.clone()),
        ];
        if let Some(d) = self.detail.as_ref().filter(|d| d.truthy()) {
            items.push(("detail".to_string(), d.clone()));
        }
        JValue::Obj(items)
    }

    /// The provider the turn answers through: `{ ...row, external: true }`.
    pub fn provider(&self) -> Option<JValue> {
        let mut row = self.row.clone()?;
        if let JValue::Obj(items) = &mut row {
            wtf16::set(items, "external", JValue::Bool(true));
        }
        Some(row)
    }
}

/// The decision, once made.
#[derive(Debug, Clone, PartialEq)]
pub struct Decided {
    /// `None`: routing modes did not apply (flag off, no mode, a Manual turn) or the turn was
    /// aborted.
    pub target: Option<RouteTarget>,
    /// The turn was aborted after the decision: end the response (when not yet ended) and return.
    pub aborted: bool,
}

/// One step of [`Modes`].
#[derive(Debug, Clone, PartialEq)]
pub enum Step {
    Ask(Seam),
    Done(Result<Decided, Halt>),
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Stage {
    Start,
    Enabled,
    Settings,
    List,
    Provider,
    Roles,
    PreMessage,
    PreAttachments,
    PreSent,
    Check,
    Stream,
    Id,
    Opened,
    Pending,
    Choice,
    Log,
    Line,
    Aborted,
    Flags,
    Remembered,
    Over,
}

/// The outcome of `resolveRoute`.
#[derive(Debug, Clone)]
struct Outcome {
    cloud: bool,
    cancel: bool,
    reason: &'static str,
    flag: JValue,
    cause: JValue,
    remember: Option<&'static str>,
}

/// chat.cjs's routing modes of one turn, in its order (see the module doc).
pub struct Modes<'a> {
    t: ModesTurn<'a>,
    stage: Stage,
    settings: JValue,
    force_local: bool,
    allow_cloud: bool,
    row: JValue,
    has_cloud: bool,
    hard_local: bool,
    has_local: bool,
    flag: JValue,
    cause: JValue,
    id: String,
    out: Outcome,
}

fn prop(v: &JValue, key: &str) -> Result<JValue, Halt> {
    if v.is_nullish() {
        return Err(Halt::Thrown);
    }
    Ok(v.get(key).clone())
}

impl<'a> Modes<'a> {
    pub fn new(t: ModesTurn<'a>) -> Self {
        Modes {
            t,
            stage: Stage::Start,
            settings: JValue::Null,
            force_local: false,
            allow_cloud: false,
            row: JValue::Null,
            has_cloud: false,
            hard_local: false,
            has_local: false,
            flag: JValue::Null,
            cause: JValue::Null,
            id: String::new(),
            out: Outcome {
                cloud: false,
                cancel: false,
                reason: "",
                flag: JValue::Null,
                cause: JValue::Null,
                remember: None,
            },
        }
    }

    /// The next seam, given the answer to the last one. A reply that does not fit the seam asked,
    /// or a step after [`Step::Done`], ends with [`Halt::OutOfOrder`].
    pub fn step(&mut self, reply: Reply) -> Step {
        match self.advance(reply) {
            Ok(step) => step,
            Err(halt) => {
                self.stage = Stage::Over;
                Step::Done(Err(halt))
            }
        }
    }

    fn ask(&mut self, stage: Stage, seam: Seam) -> Result<Step, Halt> {
        self.stage = stage;
        Ok(Step::Ask(seam))
    }

    fn mode(&self) -> Option<&str> {
        self.settings.get("mode").as_str()
    }

    fn advance(&mut self, reply: Reply) -> Result<Step, Halt> {
        let stage = std::mem::replace(&mut self.stage, Stage::Over);
        match (stage, reply) {
            (Stage::Start, Reply::Start) => {
                if !self.t.wants_auto {
                    return Ok(self.none());
                }
                self.ask(Stage::Enabled, Seam::Enabled)
            }
            (Stage::Enabled, Reply::Value(v)) => {
                if v? != JValue::Bool(true) {
                    return Ok(self.none());
                }
                self.ask(Stage::Settings, Seam::Settings)
            }
            (Stage::Settings, Reply::Value(v)) => {
                self.settings = v?;
                if !self.settings.truthy() || !self.settings.get("mode").truthy() {
                    return Ok(self.none());
                }
                let id = self
                    .t
                    .list_project
                    .then(|| self.t.project.get("id").clone());
                self.ask(Stage::List, Seam::ChatList(id))
            }
            (Stage::List, Reply::Value(list)) => {
                let list = list?;
                let chat = match &list {
                    JValue::Arr(items) => items
                        .iter()
                        .find(|c| c.truthy() && strict_eq(c.get("id"), self.t.chat_id)),
                    _ => None,
                };
                let flag = |k: &str| chat.is_some_and(|c| c.get(k) == &JValue::Bool(true));
                self.force_local = flag("forceLocal");
                self.allow_cloud = flag("allowCloud");
                let id = prop(&prop(&self.settings, "cloud")?, "providerId")?;
                if id.truthy() && id.as_str() != Some(self.t.default_provider_id) {
                    return self.ask(Stage::Provider, Seam::Provider(id));
                }
                self.after_provider(JValue::Null)
            }
            (Stage::Provider, Reply::Value(row)) => self.after_provider(row?),
            (Stage::Roles, Reply::Value(roles)) => {
                let roles = roles?;
                self.has_local = roles.truthy()
                    && (prop(&roles, "fast")?.truthy() || prop(&roles, "smart")?.truthy());
                if self.mode() == Some("cloud")
                    && !self.has_cloud
                    && !self.hard_local
                    && !self.force_local
                {
                    return Err(refused(
                        "Cloud routing has no cloud provider and model yet. Choose them in Settings → Models & routing.",
                    ));
                }
                if self.t.attachments.is_none() {
                    return Err(Halt::Thrown);
                }
                if self.mode() == Some("hybrid") {
                    let m = PreText::Message(self.t.message.to_string());
                    return self.ask(Stage::PreMessage, Seam::PreRule(m));
                }
                self.resolve()
            }
            (Stage::PreMessage, Reply::Value(f)) => self.pre(f?, Stage::PreAttachments),
            (Stage::PreAttachments, Reply::Value(f)) => self.pre(f?, Stage::PreSent),
            (Stage::PreSent, Reply::Value(f)) => self.pre(f?, Stage::Over),
            (Stage::Check, Reply::Value(v)) => {
                let v = v?;
                if !v.truthy() || prop(&v, "flagged")? != JValue::Bool(false) {
                    let f = v.opt("flag");
                    self.flag = if f.truthy() {
                        f.clone()
                    } else {
                        "unavailable".into()
                    };
                    let c = v.opt("cause");
                    self.cause = if c.truthy() { c.clone() } else { JValue::Null };
                }
                self.flagged()
            }
            // `ask` throwing (the wire, the id, the event) is caught: no answer, fail closed.
            (Stage::Stream | Stage::Id | Stage::Pending, Reply::Value(Err(Thrown))) => {
                self.answered(&JValue::Null)
            }
            (Stage::Stream, Reply::Done) => self.ask(Stage::Id, Seam::ChoiceId),
            (Stage::Id, Reply::Id(id)) => {
                self.id = id.clone();
                if self.t.open_choice {
                    return self.ask(Stage::Opened, Seam::OpenChoice(id));
                }
                self.pending()
            }
            (Stage::Opened, Reply::Value(v)) => match v {
                Ok(v) if v.truthy() => self.pending(),
                Ok(_) => self.answered(&JValue::obj([("choice", JValue::from("timeout"))])),
                Err(Thrown) => self.answered(&JValue::Null),
            },
            (Stage::Pending, Reply::Done) => {
                let id = self.id.clone();
                self.ask(Stage::Choice, Seam::AwaitChoice(id))
            }
            (Stage::Choice, Reply::Value(v)) => self.answered(&v.unwrap_or(JValue::Null)),
            (Stage::Log, Reply::Done) => {
                let line = self.log_line()?;
                self.ask(Stage::Line, Seam::Line(line))
            }
            (Stage::Line, Reply::Done) => self.ask(Stage::Aborted, Seam::Aborted),
            (Stage::Aborted, Reply::Value(aborted)) => {
                if aborted?.truthy() {
                    return Ok(self.finish(None, true));
                }
                if self.out.cancel {
                    return Err(refused(
                        "Kept local, but no local model is set for Auto. Nothing was sent.",
                    ));
                }
                match self.out.remember {
                    Some(r) if self.t.chat_id.truthy() => {
                        let project_id = if self.t.list_project {
                            self.t.project.get("id").clone()
                        } else {
                            JValue::Null
                        };
                        self.ask(
                            Stage::Flags,
                            Seam::SetChatFlags {
                                project_id,
                                chat_id: self.t.chat_id.clone(),
                                patch: remember_patch(r),
                            },
                        )
                    }
                    _ => Ok(self.target()),
                }
            }
            (Stage::Flags, Reply::Done) => {
                let mut ev = vec![("type".to_string(), JValue::from("route_remembered"))];
                if let JValue::Obj(p) = remember_patch(self.out.remember.unwrap_or("cloud")) {
                    ev.extend(p);
                }
                self.ask(Stage::Remembered, Seam::Send(JValue::Obj(ev)))
            }
            (Stage::Remembered, Reply::Done) => Ok(self.target()),
            _ => Err(Halt::OutOfOrder),
        }
    }

    fn none(&mut self) -> Step {
        self.finish(None, false)
    }

    fn finish(&mut self, target: Option<RouteTarget>, aborted: bool) -> Step {
        self.stage = Stage::Over;
        Step::Done(Ok(Decided { target, aborted }))
    }

    fn after_provider(&mut self, row: JValue) -> Result<Step, Halt> {
        let cloud = prop(&self.settings, "cloud")?;
        let id = prop(&cloud, "providerId")?;
        self.row = row;
        self.has_cloud = self.row.truthy()
            && strict_eq(&prop(&self.row, "id")?, &id)
            && cloud_model(&cloud, "smart")?.truthy();
        let space = self.t.space_id.as_str();
        self.hard_local = space.is_some_and(|s| s.starts_with("diary"))
            || (self.t.project.truthy()
                && self.t.project.get("id").as_str() == Some(self.t.diary_project_id))
            || has_diary_content(self.t.history)?;
        self.ask(Stage::Roles, Seam::AutoRoles)
    }

    /// A pre-rule's answer: the first truthy one is `preFlag`.
    fn pre(&mut self, flag: JValue, next: Stage) -> Result<Step, Halt> {
        if flag.truthy() {
            self.flag = flag;
            return self.resolve();
        }
        match next {
            Stage::PreAttachments => {
                let a = PreText::Attachments(self.t.attachments.unwrap_or_default().join("\n"));
                self.ask(next, Seam::PreRule(a))
            }
            Stage::PreSent => self.ask(next, Seam::PreRule(PreText::Sent)),
            _ => self.resolve(),
        }
    }

    fn local(&mut self, reason: &'static str) -> Result<Step, Halt> {
        self.out.cancel = !self.has_local;
        self.out.reason = reason;
        if self.out.cancel {
            // `{ cancel: true, reason }`: no flag, cause or remembered choice.
            self.out.flag = JValue::Null;
            self.out.cause = JValue::Null;
            self.out.remember = None;
        }
        self.log()
    }

    fn cloud(&mut self, reason: &'static str) -> Result<Step, Halt> {
        self.out.cloud = true;
        self.out.reason = reason;
        self.log()
    }

    /// `resolveRoute` up to the check.
    fn resolve(&mut self) -> Result<Step, Halt> {
        if self.hard_local {
            self.out.flag = "diary".into();
            return self.local("sensitive-rule");
        }
        if self.force_local {
            return self.local("force-local");
        }
        if self.mode() == Some("local") || !self.has_cloud {
            return self.local("mode");
        }
        if self.mode() == Some("cloud") {
            return self.cloud("mode");
        }
        if !self.flag.truthy() {
            return self.ask(Stage::Check, Seam::Sensitivity);
        }
        self.flagged()
    }

    /// `resolveRoute` once the flag is known.
    fn flagged(&mut self) -> Result<Step, Halt> {
        if !self.flag.truthy() {
            return self.cloud("mode");
        }
        self.out.flag = self.flag.clone();
        if self.flag.as_str() == Some("unavailable") {
            if self.allow_cloud {
                return self.cloud("remembered");
            }
            self.out.cause = self.cause.clone();
            return self.local("check-unavailable");
        }
        if self.allow_cloud {
            return self.cloud("remembered");
        }
        let when = if self.t.compact_only {
            JValue::from("local")
        } else {
            prop(&self.settings, "whenSensitive")?
        };
        if when.as_str() == Some("local") {
            return self.local("sensitive-rule");
        }
        if !self.t.user {
            return self.answered(&JValue::obj([("choice", JValue::from("aborted"))]));
        }
        self.ask(Stage::Stream, Seam::OpenStream)
    }

    fn pending(&mut self) -> Result<Step, Halt> {
        let ev = JValue::obj([
            ("type", JValue::from("route_pending")),
            ("id", JValue::from(self.id.as_str())),
            ("flag", self.flag.clone()),
        ]);
        self.ask(Stage::Pending, Seam::Send(ev))
    }

    /// The question's answer (`null` when `ask` threw).
    fn answered(&mut self, answer: &JValue) -> Result<Step, Halt> {
        let choice = if answer.is_nullish() {
            &JValue::Undefined
        } else {
            answer.get("choice")
        };
        let remember = !answer.is_nullish() && answer.get("remember").truthy();
        match choice.as_str() {
            Some("cloud") => {
                self.out.remember = remember.then_some("cloud");
                self.cloud("user-choice")
            }
            Some("local") => {
                self.out.remember = remember.then_some("local");
                self.local("user-choice")
            }
            _ => self.local("fail-closed"),
        }
    }

    fn log(&mut self) -> Result<Step, Halt> {
        let route = match (self.out.cancel && !self.out.cloud, self.out.cloud) {
            (_, true) => "cloud",
            (true, _) => "cancel",
            _ => "local",
        };
        let flag = if self.out.flag.truthy() {
            self.out.flag.clone()
        } else {
            JValue::Null
        };
        let entry = JValue::obj([
            ("mode", self.settings.get("mode").clone()),
            ("route", JValue::from(route)),
            ("reason", JValue::from(self.out.reason)),
            ("flag", flag),
        ]);
        self.ask(Stage::Log, Seam::Log(entry))
    }

    fn log_line(&self) -> Result<String, Halt> {
        let s = |v: &JValue| to_js_string(v).map_err(|_| Halt::Thrown);
        let route = if self.out.cloud {
            "cloud"
        } else if self.out.cancel {
            "cancel"
        } else {
            "local"
        };
        let mut line = format!(
            "[routing] mode={} route={route} reason={}",
            s(self.settings.get("mode"))?,
            self.out.reason
        );
        if self.out.flag.truthy() {
            line.push_str(&format!(" flag={}", s(&self.out.flag)?));
        }
        Ok(line)
    }

    fn target(&mut self) -> Step {
        let o = &self.out;
        let cloud = o.cloud;
        let detail = (o.reason == "check-unavailable" && o.cause.truthy()).then(|| o.cause.clone());
        let hybrid_ungranted = self.mode() == Some("hybrid")
            && cloud
            && !(self.allow_cloud || o.remember == Some("cloud") || o.reason == "user-choice");
        let target = RouteTarget {
            route: if cloud { "cloud" } else { "local" },
            reason: JValue::from(o.reason),
            detail,
            cloud: self.settings.get("cloud").clone(),
            row: cloud.then(|| self.row.clone()),
            hybrid_ungranted,
        };
        self.finish(Some(target), false)
    }
}

fn refused(error: &str) -> Halt {
    Halt::Refused(Refusal {
        status: 409,
        error: error.into(),
    })
}

/// The remembered choice's chat flags.
fn remember_patch(r: &str) -> JValue {
    if r == "local" {
        JValue::obj([
            ("forceLocal", JValue::Bool(true)),
            ("allowCloud", JValue::Bool(false)),
        ])
    } else {
        JValue::obj([("allowCloud", JValue::Bool(true))])
    }
}

/// `cloudModel(cloud, role)`: the cloud model for a role, falling back smart, fast, code; `''`.
/// Throws as Node does when `cloud` is `null` or `undefined`.
pub fn cloud_model(cloud: &JValue, role: &str) -> Result<JValue, Halt> {
    for key in [role, "smart", "fast", "code"] {
        let v = prop(cloud, key)?;
        if v.truthy() {
            return Ok(v);
        }
    }
    Ok(JValue::from(""))
}

/// `hasDiaryContent(history)`: a Diary tool's result in the history (`/^diary[_-]/i`, whose
/// case folding without the `u` flag never maps a non-ASCII letter to an ASCII one). `String()`
/// of a name with its own `toString` throws.
pub fn has_diary_content(history: &JValue) -> Result<bool, Thrown> {
    let JValue::Arr(items) = history else {
        return Ok(false);
    };
    for h in items {
        let role = h.get("role").as_str();
        if !h.truthy() || !matches!(role, Some("tool") | Some("function")) {
            continue;
        }
        let name = h.get("name");
        let name = if name.truthy() {
            to_js_string(name).map_err(|_| Thrown)?
        } else {
            String::new()
        };
        let b = name.as_bytes();
        let head = b.get(..5).is_some_and(|h| h.eq_ignore_ascii_case(b"diary"));
        if head && matches!(b.get(5), Some(b'_' | b'-')) {
            return Ok(true);
        }
    }
    Ok(false)
}

/// `attachmentNames`: the project's asset and file names that are text. Throws as
/// Node does for a `null` entry or a truthy list that is not an array.
pub fn attachment_names(project: &JValue) -> Result<Vec<String>, Thrown> {
    let mut names = Vec::new();
    for key in ["assets", "files"] {
        let list = if project.truthy() {
            project.get(key)
        } else {
            &JValue::Undefined
        };
        match list {
            JValue::Arr(items) => {
                for item in items {
                    if item.is_nullish() {
                        return Err(Thrown);
                    }
                    if let Some(n) = item.get("name").as_str() {
                        names.push(n.to_string());
                    }
                }
            }
            v if v.truthy() => return Err(Thrown),
            _ => {}
        }
    }
    Ok(names)
}

/// `sentText`: the system message without the brain block, then each message's text content
/// (`''` for parts), joined with `'\n'`.
pub fn sent_text(sys_without_brain: &str, msgs: &[JValue]) -> String {
    let mut out = sys_without_brain.to_string();
    for m in msgs {
        out.push('\n');
        if let Some(c) = m.get("content").as_str() {
            out.push_str(c);
        }
    }
    out
}

/// `offBoxTurn`: the turn answers off this box (an external provider, a cloud route, or a
/// provider other than the default that is not an Inference machine).
pub fn off_box_turn(
    external: bool,
    target: Option<&RouteTarget>,
    provider: &JValue,
    default: &str,
) -> bool {
    external
        || target.is_some_and(|t| t.route == "cloud")
        || (provider.opt("id").as_str() != Some(default) && !provider.opt("machine").truthy())
}
