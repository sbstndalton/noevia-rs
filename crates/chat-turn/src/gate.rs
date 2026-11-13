//! One tool call of a native turn, from the model's request to the result the model and the client
//! get: chat.cjs's tool loop (lines ~2250-2600) for the turns core hands off. Those carry no
//! coworker-page tool, no provenance policy and only the local default provider
//! (rust-chat-native.cjs `refusal`), so the parts of the loop for those are not here. A loaded
//! instruction skill revoked meanwhile ([`crate::skills`], #272) refuses the call before it starts,
//! after the card's answer and before the run.
//!
//! In Node's order: the tool-exchange checks and the per-turn result cache (tool-exchange.cjs, the
//! `tool-exchange` crate), a write after a declined one, an oversized call, the account's policy, the
//! card's target (a project file edit's stored file, #648, or a Drive write's, #659, through the core
//! bridges), the chat's permission binding and grant, the approval card and its answer, the checks
//! after the answer (policy, the edit's file again, the write's target, the binding, the edit's file
//! once more), the call through the core bridge (an edit pinned to its file's digest), the audit, then
//! the model's copy (reduced and framed) and the chip's `tool_result` with its flags.
//!
//! chat.cjs's approval target string (`approvalTarget()`, compared with the one shown before the run)
//! is built only of values fixed before the card (the card's target, kind, edit file and account), so
//! it cannot differ here and is not rebuilt.
//!
//! Port contract S9 (the front's approval registry): the card's question is opened before the card is shown
//! (`notAsked` when it cannot be, or when its wait cannot be answered), the chat-wide grant is core's single-use
//! handle for Node's grant check, and a refusal that revokes the grant waits for the revoke. The grant is redeemed
//! by core's execute bridge in the same request that runs the call behind Node's dispatch fence (D17 in the
//! track's progress file): a refused redeem revokes and refuses as a changed binding, an unchecked one refuses as
//! `notChecked`, and a fence closed at the outbound write comes back as `notRun`.
//!
//! Named difference (D17): the last checks before the run (the edit's file once more, the client gone, the
//! policy for dispatch) come before the redeem here and after it in chat.cjs. Only which refusal wins when the
//! redeem and one of those checks both fail differs; markWriteAttempt and the round's write mark are set after a
//! redeemed (or grant-less) call, as Node sets them only after its redeem.
//!
//! Named difference: a project file edit whose file the bridge gave no answer for at all
//! ([`EditTarget::Failed`]: core unreachable, a timeout, a refusal of the call; Node has no bridge and
//! `resolveEditTarget` never throws there, so this case is the executor's own) is refused with the
//! lookup sentence (`The file this would change could not be looked up`) and audited `edit-target`,
//! before the card and after it alike; it never revokes the chat's grant (a transport fault is not
//! a changed file). Only extra refusals: never a run Node would refuse.

use std::collections::{HashMap, HashSet};
use std::sync::Mutex;

use js_json::JValue;

use crate::host::{EditTarget, GrantRun, Host, Ran, Target, ToolRun};
use crate::js;
use crate::skills::Skills;

pub const CHECK_FAULT: &str = "ERROR: the tool call could not be checked; tool was not run.";
pub const ERROR_FAULT: &str = "ERROR: the tool failed and its error could not be read.";
const PERMISSION_UNVERIFIED: &str = "ERROR: the chat permission context could not be verified; the tool was not run. Nothing was changed. Ask again.";
const TARGET_CHANGED: &str =
    "ERROR: the write target changed after approval; the tool was not run. Ask again.";
const BINDING_CHANGED: &str = "ERROR: the chat or permission target changed after approval; the tool was not run. Nothing was changed. Ask again.";
const CANCELLED: &str = "ERROR: exchange cancelled; tool was not run.";
const PERMISSION_CHANGED: &str = "ERROR: permission changed before dispatch; tool was not run.";
const EDIT_TARGET_MOVED: &str =
    "ERROR: the project target changed before dispatch; tool was not run. Ask again.";
/// rust-chat-native.cjs (and chat.cjs for a write target): a lookup that could not be made.
const LOOKUP_FAILED: &str = "The file this would change could not be looked up";

/// A project file edit's file, as resolved before its card (#648).
#[derive(Debug, Clone, PartialEq, Eq)]
struct Pin {
    path: String,
    account: Option<String>,
    digest: String,
}

impl Pin {
    /// chat.cjs: `again.path !== editTarget || (again.account || null) !== editAccount`.
    fn same(&self, again: &EditTarget) -> bool {
        matches!(again, EditTarget::Found { path, account, .. }
            if *path == self.path && account.as_deref().filter(|a| !a.is_empty()) == self.account.as_deref())
    }
}

/// One chat's recent writes: (fingerprint, when), oldest first.
type Writes = Vec<(String, f64)>;

/// recent-writes.cjs: the writes that succeeded per account and chat, for a card's repeat flag (#658).
#[derive(Debug, Default)]
pub struct RecentWrites {
    chats: Mutex<Vec<(String, Writes)>>,
}

const RECENT_TTL_MS: f64 = 6.0 * 60.0 * 60.0 * 1000.0;
const RECENT_PER_CHAT: usize = 20;
const RECENT_MAX_CHATS: usize = 500;

impl RecentWrites {
    fn key(user: &str, chat: Option<&str>) -> Option<String> {
        match chat {
            Some(c) if !user.is_empty() && !c.is_empty() => Some(format!("{user}\u{0}{c}")),
            _ => None,
        }
    }

    pub fn record(&self, user: &str, chat: Option<&str>, fp: &str, now: f64) {
        let Some(key) = Self::key(user, chat) else {
            return;
        };
        let Ok(mut chats) = self.chats.lock() else {
            return;
        };
        let mut list: Vec<(String, f64)> = chats
            .iter()
            .position(|(k, _)| *k == key)
            .map(|at| chats.remove(at).1)
            .unwrap_or_default()
            .into_iter()
            .filter(|(_, at)| now - at <= RECENT_TTL_MS)
            .collect();
        list.push((fp.to_string(), now));
        let excess = list.len().saturating_sub(RECENT_PER_CHAT);
        list.drain(..excess);
        chats.push((key, list));
        let over = chats.len().saturating_sub(RECENT_MAX_CHATS);
        chats.drain(..over);
    }

    /// recent-writes.cjs `has`: through `live()`, which drops the chat's expired writes (and the chat).
    pub fn has(&self, user: &str, chat: Option<&str>, fp: &str, now: f64) -> bool {
        let Some(key) = Self::key(user, chat) else {
            return false;
        };
        let Ok(mut chats) = self.chats.lock() else {
            return false;
        };
        let Some(at) = chats.iter().position(|(k, _)| *k == key) else {
            return false;
        };
        let live: Writes = chats
            .get(at)
            .map(|(_, l)| {
                l.iter()
                    .filter(|(_, t)| now - t <= RECENT_TTL_MS)
                    .cloned()
                    .collect()
            })
            .unwrap_or_default();
        let found = live.iter().any(|(f, _)| f == fp);
        if live.is_empty() {
            chats.remove(at);
        } else if let Some(entry) = chats.get_mut(at) {
            entry.1 = live;
        }
        found
    }
}

/// recent-writes.cjs `canonical`: JSON with object keys sorted.
fn canonical(v: &JValue) -> String {
    match v {
        JValue::Arr(items) => format!(
            "[{}]",
            items.iter().map(canonical).collect::<Vec<_>>().join(",")
        ),
        JValue::Obj(items) => {
            let mut keys: Vec<&(String, JValue)> = items.iter().collect();
            // Object.keys(...).sort(): UTF-16 code unit order.
            keys.sort_by_key(|k| js::to_units(&k.0));
            format!(
                "{{{}}}",
                keys.iter()
                    .map(|(k, v)| format!("{}:{}", js::stringify(&js::s(k.clone())), canonical(v)))
                    .collect::<Vec<_>>()
                    .join(",")
            )
        }
        JValue::Undefined => "null".into(),
        other => js::stringify(other),
    }
}

/// recent-writes.cjs `fingerprint(name, target, rawArgs)`.
pub fn fingerprint(name: &str, target: Option<&str>, raw_args: &str) -> String {
    let args = match js_json::wtf16::parse(if raw_args.is_empty() { "{}" } else { raw_args }) {
        Ok(v) => canonical(&v),
        Err(_) => raw_args.to_string(),
    };
    let target = target.filter(|t| !t.is_empty()).map_or(JValue::Null, js::s);
    context_fit::fingerprint(&JValue::Arr(vec![js::s(name), target, js::s(args)]))
}

/// One call of a round, as the loop holds it.
#[derive(Debug, Clone, PartialEq)]
pub struct Call {
    pub index: f64,
    pub id: JValue,
    pub name: String,
    pub args: String,
    pub too_large: bool,
}

/// What one call left behind for the turn.
#[derive(Debug, Clone, PartialEq)]
pub struct Outcome {
    /// The `tool_result` event.
    pub event: JValue,
    /// The `role: tool` message for the next round.
    pub message: JValue,
    pub applied: Option<(String, Option<String>)>,
    /// The call was a write and was about to run (#1456).
    pub wrote: bool,
    pub links: Vec<String>,
    /// The whole result of a call that succeeded, for the skill reads (chat.cjs `noteSkillRead`).
    pub read: Option<String>,
}

/// The turn's tool state shared across its rounds.
pub struct Tools<'a> {
    pub allowed: HashSet<String>,
    /// The offered tool names in the order chat.cjs's `allowedToolNames` Set holds them.
    pub allowed_order: Vec<String>,
    pub writes: HashSet<String>,
    pub public: HashSet<String>,
    pub automatic_only_writes: HashSet<String>,
    /// The offered project file edit tools (chat.cjs `projectEditTool`).
    pub edit_tools: HashSet<String>,
    /// The turn's instruction skills (#272), when it has a project.
    pub skills: Option<&'a Skills>,
    pub routes: Vec<(String, String)>,
    pub chat_id: Option<String>,
    pub chat_key: Option<String>,
    pub grant_scope: String,
    pub grant_holder: JValue,
    pub recent: HashSet<String>,
    pub project_id: Option<String>,
    pub exchange_key: String,
    pub decline_ends_reply: bool,
    pub recent_writes: &'a RecentWrites,
    pub reduce: &'a (dyn Fn(&str, usize) -> String + Sync),
    pub source_links: &'a (dyn Fn(&str) -> Vec<String> + Sync),
    pub tool_result_cap: usize,
    cache: HashMap<Vec<u16>, (String, bool)>,
    pub declined: Vec<String>,
    not_approved: HashSet<String>,
    not_run: HashSet<String>,
}

fn audit_denied<H: Host>(host: &H, name: &str, reason: &str) {
    host.audit(
        "tool.denied",
        &js::obj([("tool", js::s(name)), ("reason", js::s(reason))]),
    );
}

/// An edit whose file the bridge gave no answer for (see the module doc): refused, audited, no revocation.
fn lookup_failed<H: Host>(host: &H, name: &str) -> String {
    audit_denied(host, name, "edit-target");
    format!(
        "ERROR: {} {name} was not run and nothing was changed.",
        sentence(LOOKUP_FAILED)
    )
}

fn blocked_text(name: &str) -> String {
    format!("ERROR: {name} is blocked in this account's settings, so it was not run. Do not retry it; tell the user they can change it in Settings → Connectors.")
}

/// `String(error).replace(/\.?$/, '.')`.
fn sentence(text: &str) -> String {
    format!("{}.", text.strip_suffix('.').unwrap_or(text))
}

/// The gate's answer before the call would run.
enum Gate {
    Result(String),
    Run {
        permission: String,
        card_target: Option<String>,
        kind: Option<String>,
        /// For an asked call: the binding shown, the chat-wide grant's handle when one was used, and whether
        /// the call was asked about.
        binding: Option<(JValue, Option<String>, bool)>,
        edit: Option<Pin>,
    },
}

impl<'a> Tools<'a> {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        allowed: Vec<String>,
        writes: HashSet<String>,
        public: HashSet<String>,
        automatic_only_writes: HashSet<String>,
        routes: Vec<(String, String)>,
        chat: (
            Option<String>,
            Option<String>,
            String,
            JValue,
            HashSet<String>,
        ),
        project_id: Option<String>,
        exchange_key: String,
        decline_ends_reply: bool,
        recent_writes: &'a RecentWrites,
        reduce: &'a (dyn Fn(&str, usize) -> String + Sync),
        source_links: &'a (dyn Fn(&str) -> Vec<String> + Sync),
        tool_result_cap: usize,
    ) -> Self {
        let (chat_id, chat_key, grant_scope, grant_holder, recent) = chat;
        Tools {
            allowed: allowed.iter().cloned().collect(),
            allowed_order: allowed,
            writes,
            public,
            automatic_only_writes,
            edit_tools: HashSet::new(),
            skills: None,
            routes,
            chat_id,
            chat_key,
            grant_scope,
            grant_holder,
            recent,
            project_id,
            exchange_key,
            decline_ends_reply,
            recent_writes,
            reduce,
            source_links,
            tool_result_cap,
            cache: HashMap::new(),
            declined: Vec::new(),
            not_approved: HashSet::new(),
            not_run: HashSet::new(),
        }
    }

    fn route(&self, name: &str) -> Option<&str> {
        self.routes
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, s)| s.as_str())
    }

    fn is_write(&self, name: &str) -> bool {
        self.writes.contains(name)
    }

    /// Runs one call through the exchange and the gate and gives what the turn keeps of it.
    pub async fn call<H: Host>(&mut self, host: &H, call: &Call, offset: f64) -> Outcome {
        let mut ran_ok = false;
        let mut failed = false;
        let mut card_target = None;
        let mut write_print = None;
        let mut wrote = false;
        let name_units = js::to_units(&call.name);
        let args_units = (!call.args.is_empty()).then(|| js::to_units(&call.args));
        let checked = tool_exchange::check(
            host.left(),
            self.allowed.contains(&call.name),
            &name_units,
            args_units.as_deref(),
        );
        let result = match checked {
            Err(_) => CHECK_FAULT.to_string(),
            Ok(tool_exchange::Check::Answer(a)) => js::from_units(&a),
            Ok(tool_exchange::Check::Run(key)) => match self.cache.get(&key) {
                Some((r, _)) => r.clone(),
                None => {
                    let write = self.is_write(&call.name);
                    let r = match self
                        .gate(host, call, offset, &mut card_target, &mut write_print)
                        .await
                    {
                        Gate::Result(r) => r,
                        Gate::Run {
                            permission,
                            card_target: shown,
                            kind,
                            binding,
                            edit,
                        } => {
                            // A grant used without a card is redeemed by core's execute bridge (D17).
                            let grant = binding
                                .as_ref()
                                .and_then(|(_, handle, asked)| handle.clone().filter(|_| !*asked));
                            let mut final_binding = None;
                            match self
                                .after_answer(
                                    host,
                                    call,
                                    &permission,
                                    (shown.as_deref(), kind),
                                    binding,
                                    edit.as_ref(),
                                    &mut final_binding,
                                )
                                .await
                            {
                                Some(refused) => refused,
                                None => {
                                    let run = ToolRun {
                                        project_id: self.project_id.clone(),
                                        name: call.name.clone(),
                                        args: call.args.clone(),
                                        allowed: self.allowed_order.clone(),
                                        routes: self.routes.clone(),
                                        chat_key: self.chat_key.clone(),
                                        exchange_key: self.exchange_key.clone(),
                                        edit_target: edit.as_ref().map(|p| p.digest.clone()),
                                        grant: match (grant, &self.chat_id, final_binding) {
                                            (Some(handle), Some(chat), Some(binding)) => {
                                                Some(GrantRun {
                                                    handle,
                                                    chat: chat.clone(),
                                                    scope: self.grant_scope.clone(),
                                                    binding,
                                                })
                                            }
                                            _ => None,
                                        },
                                    };
                                    let ran = host.execute_tool(&run).await;
                                    // markWriteAttempt (a write may change what earlier reads saw) and the
                                    // round's write mark (#1456): chat.cjs sets both just before the call,
                                    // after the redeem, so a refused or unchecked redeem sets neither.
                                    if write && !matches!(ran, Ran::Refused | Ran::Unavailable) {
                                        self.cache.retain(|_, (_, w)| *w);
                                        wrote = true;
                                    }
                                    match ran {
                                        Ran::Refused => self
                                            .revoked(host, BINDING_CHANGED)
                                            .await
                                            .unwrap_or_default(),
                                        Ran::Unavailable => self.not_checked(host, &call.name),
                                        Ran::Done {
                                            text,
                                            not_run: true,
                                            ..
                                        } => {
                                            // The executor's fence closed at the outbound write: not run,
                                            // not counted, not a decline.
                                            self.not_run.insert(text.clone());
                                            audit_denied(host, &call.name, "unavailable");
                                            text
                                        }
                                        Ran::Done {
                                            text, failed: f, ..
                                        } => {
                                            ran_ok = true;
                                            failed = f;
                                            if write {
                                                let mut details = vec![
                                                    ("tool", js::s(call.name.clone())),
                                                    ("args", js::s(js::slice(&call.args, 500))),
                                                ];
                                                if let Some(p) = &edit {
                                                    details.push((
                                                        "target",
                                                        js::s(js::slice(&p.path, 500)),
                                                    ));
                                                }
                                                if let Some(s) = self.route(&call.name) {
                                                    details.push(("server", js::s(s)));
                                                }
                                                if f {
                                                    details.push(("failed", JValue::Bool(true)));
                                                }
                                                host.audit("tool.write", &js::obj(details));
                                            }
                                            text
                                        }
                                        Ran::Threw(message) => match tool_exchange::call_error(
                                            &name_units,
                                            &js::to_units(&message),
                                        ) {
                                            Ok(t) => js::from_units(&t),
                                            Err(_) => ERROR_FAULT.to_string(),
                                        },
                                    }
                                }
                            }
                        }
                    };
                    self.cache.insert(key, (r.clone(), write));
                    r
                }
            },
        };
        let for_model = (self.reduce)(&result, self.tool_result_cap);
        let framed = prompt_framing::framing::frame_untrusted(
            &js::to_units("tool result"),
            &name_units,
            &js::to_units(&for_model),
        );
        let is_error = result.starts_with("ERROR")
            && !result.get(5..).is_some_and(|rest| {
                rest.starts_with(|c: char| c.is_ascii_alphanumeric() || c == '_')
            });
        let applied = ran_ok && write_print.is_some() && !failed && !is_error;
        let user = host.user_id();
        if applied {
            if let Some(fp) = &write_print {
                self.recent_writes
                    .record(&user, self.chat_id.as_deref(), fp, host.now_ms());
            }
        }
        let mut event = vec![
            ("type", js::s("tool_result")),
            ("index", js::n(offset + call.index)),
            ("name", js::s(call.name.clone())),
            ("text", js::s(js::slice(&result, 300))),
        ];
        if applied {
            event.push(("applied", JValue::Bool(true)));
            if let Some(t) = &card_target {
                event.push(("target", js::s(t.clone())));
            }
        }
        if self.not_approved.contains(&result) {
            event.push(("declined", JValue::Bool(true)));
        }
        if self.not_run.contains(&result) {
            event.push(("notRun", JValue::Bool(true)));
        }
        if !applied && self.public.contains(&call.name) {
            event.push(("public", JValue::Bool(true)));
        }
        let ok = !failed && !is_error;
        let links = if ok {
            (self.source_links)(&result)
        } else {
            Vec::new()
        };
        Outcome {
            event: js::obj(event),
            message: js::obj([
                ("role", js::s("tool")),
                ("tool_call_id", call.id.clone()),
                ("content", js::s(js::from_units(&framed))),
            ]),
            applied: applied.then(|| (call.name.clone(), card_target.clone())),
            wrote,
            links,
            read: ok.then_some(result),
        }
    }

    /// chat.cjs `revokedSkills()` in the gate: `Some(result)` refusing the call when a loaded skill was revoked.
    /// A skills bridge that gave no answer refuses it unchecked (skills.rs).
    async fn skill_refusal<H: Host>(&self, host: &H, name: &str) -> Option<String> {
        let skills = self.skills?;
        if skills.revoked(host).await {
            if skills.failed() {
                return Some(CHECK_FAULT.to_string());
            }
            return Some(skills.refuse(host, name));
        }
        None
    }

    /// The gate up to (and with) the approval card: a result when the call does not run.
    async fn gate<H: Host>(
        &mut self,
        host: &H,
        call: &Call,
        offset: f64,
        card_target: &mut Option<String>,
        write_print: &mut Option<String>,
    ) -> Gate {
        let name = call.name.as_str();
        let write = self.is_write(name);
        // A call requested under a skill that has since been disabled or changed never runs (#272).
        if let Some(r) = self.skill_refusal(host, name).await {
            return Gate::Result(r);
        }
        if !self.declined.is_empty() && write {
            audit_denied(host, name, "earlier-decline");
            let r = format!("ERROR: {name} was not run because an earlier write in this reply was declined. Nothing was changed.");
            self.not_run.insert(r.clone());
            return Gate::Result(r);
        }
        if call.too_large {
            audit_denied(host, name, "arguments-too-large");
            return Gate::Result(format!(
                "ERROR: {} was not run: its name, id or arguments were larger than about 8 million characters, which is not a real tool call. Nothing was changed.",
                js::slice(name, 64)
            ));
        }
        let permission = host.tool_policy(name, write);
        if permission == "block" {
            audit_denied(host, name, "blocked");
            return Gate::Result(blocked_text(name));
        }
        let mut target_kind = None;
        let mut edit = None;
        if self.edit_tools.contains(name) {
            // #648: the one stored file the edit would change, before anyone is asked; the call is pinned to it.
            let refused = match self.edit_target(host, call).await {
                EditTarget::Found {
                    path,
                    account,
                    digest,
                } => {
                    *card_target = Some(path.clone());
                    edit = Some(Pin {
                        path,
                        account: account.filter(|a| !a.is_empty()),
                        digest,
                    });
                    None
                }
                EditTarget::Refused(error) => Some(error),
                EditTarget::Failed => Some(LOOKUP_FAILED.to_string()),
            };
            if let Some(error) = refused {
                audit_denied(host, name, "edit-target");
                return Gate::Result(format!(
                    "ERROR: {} {name} was not run and nothing was changed.",
                    sentence(&error)
                ));
            }
        } else if write {
            match host
                .write_target(name, &call.args, self.chat_key.as_deref())
                .await
            {
                Target::Refused(error) => {
                    audit_denied(host, name, "write-target");
                    return Gate::Result(format!(
                        "ERROR: {} {name} was not run and nothing was changed.",
                        sentence(&error)
                    ));
                }
                Target::Found { target, kind } => {
                    if !target.is_empty() {
                        *card_target = Some(target);
                        // `targetKind` is shown only when truthy, and compared as `kind || null`.
                        target_kind = kind.filter(|k| !k.is_empty());
                    }
                }
                Target::None => {}
            }
        }
        *write_print = write.then(|| fingerprint(name, card_target.as_deref(), &call.args));
        let mut shown = None;
        if permission == "ask" {
            let user = host.user_id();
            let binding = match host
                .permission_binding(
                    self.chat_id.as_deref(),
                    &self.grant_scope,
                    &self.grant_holder,
                )
                .await
            {
                // chat.cjs permissionContext: null only for a turn with no chat id; with one it binds or throws.
                Ok(JValue::Null) if self.chat_id.is_some() => {
                    return Gate::Result(PERMISSION_UNVERIFIED.into())
                }
                Ok(b) => b,
                Err(()) => return Gate::Result(PERMISSION_UNVERIFIED.into()),
            };
            // Port contract S9: core's grant bridge answers with a single-use handle for Node's grant (D17).
            let used_grant = host
                .chat_wide_approved(
                    self.chat_id.as_deref(),
                    &self.grant_scope,
                    &binding,
                    &self.exchange_key,
                )
                .await;
            let asked = used_grant.is_none() || self.automatic_only_writes.contains(name);
            if asked {
                let id = format!("ap-{}", host.uuid());
                let now = host.now_ms();
                let repeat = write_print.as_ref().is_some_and(|fp| {
                    self.recent_writes
                        .has(&user, self.chat_id.as_deref(), fp, now)
                        || self.recent.contains(fp)
                });
                // Port contract S9: the question is opened at the front's approval registry before the card is
                // shown, so the person's answer always finds it.
                let question = js::obj([
                    ("id", js::s(id.clone())),
                    ("userId", js::s(user.clone())),
                    ("chatId", self.chat_id.clone().map_or(JValue::Null, js::s)),
                    ("scope", js::s(self.grant_scope.clone())),
                    ("binding", binding.clone()),
                    ("once", JValue::Bool(false)),
                ]);
                if !host.open_approval(&question).await {
                    return Gate::Result(self.not_asked(host, name));
                }
                let mut pending = vec![
                    ("type", js::s("tool_pending")),
                    ("id", js::s(id.clone())),
                    ("index", js::n(offset + call.index)),
                    ("name", js::s(name)),
                    ("args", js::s(call.args.clone())),
                ];
                if let Some(t) = card_target.as_ref() {
                    pending.push(("target", js::s(t.clone())));
                }
                if let Some(k) = &target_kind {
                    pending.push(("targetKind", js::s(k.clone())));
                }
                if let Some(s) = self.route(name) {
                    pending.push(("server", js::s(s)));
                }
                if repeat {
                    pending.push(("repeatOf", JValue::Bool(true)));
                }
                host.emit(&js::obj(pending));
                let decision = host.await_approval(&question).await;
                if decision != "approve" {
                    if decision == "unavailable" {
                        return Gate::Result(self.not_asked(host, name));
                    }
                    let r = match decision.as_str() {
                        "aborted" => {
                            let r = format!("ERROR: the reply was stopped before {name} was approved, so it was not run. Nothing was changed.");
                            audit_denied(host, name, "aborted");
                            self.not_run.insert(r.clone());
                            return Gate::Result(r);
                        }
                        "timeout" => format!("ERROR: the user did not respond in time, so {name} was not run. Ask before trying again."),
                        _ => format!("ERROR: the user declined to run {name}. Do not retry it; ask what they would prefer."),
                    };
                    audit_denied(host, name, &decision);
                    self.not_approved.insert(r.clone());
                    if decision == "deny" && self.decline_ends_reply {
                        self.declined.push(name.to_string());
                    }
                    return Gate::Result(r);
                }
            }
            shown = Some((binding, used_grant, asked));
        }
        Gate::Run {
            permission,
            card_target: card_target.clone(),
            kind: target_kind,
            binding: shown,
            edit,
        }
    }

    async fn edit_target<H: Host>(&self, host: &H, call: &Call) -> EditTarget {
        host.edit_target(self.project_id.as_deref(), &call.name, &call.args)
            .await
    }

    /// The checks after the card's answer, before the call: `Some(result)` when it does not run.
    #[allow(clippy::too_many_arguments)]
    async fn after_answer<H: Host>(
        &mut self,
        host: &H,
        call: &Call,
        permission: &str,
        (shown, kind_was): (Option<&str>, Option<String>),
        asked_binding: Option<(JValue, Option<String>, bool)>,
        edit: Option<&Pin>,
        final_binding: &mut Option<JValue>,
    ) -> Option<String> {
        let name = call.name.as_str();
        let write = self.is_write(name);
        if host.left() {
            return Some(CANCELLED.into());
        }
        if host.tool_policy(name, write) == "block" {
            audit_denied(host, name, "blocked");
            return Some(blocked_text(name));
        }
        // A skill disabled while the card was open: the approval does not outlive it.
        if let Some(r) = self.skill_refusal(host, name).await {
            return Some(r);
        }
        // An edit whose name now lands on another file (or none): the approval was for the file shown.
        if let Some(pin) = edit {
            let again = self.edit_target(host, call).await;
            if again == EditTarget::Failed {
                return Some(lookup_failed(host, name));
            }
            if !pin.same(&again) {
                audit_denied(host, name, "edit-target-changed");
                let now = match &again {
                    EditTarget::Found { path, .. } => format!(
                        "is no longer the file this name refers to (it now means {})",
                        js::stringify(&js::s(path.clone()))
                    ),
                    _ => "can no longer be edited".to_string(),
                };
                return Some(format!(
                    "ERROR: the project's files changed after approval, so {} {now}. {name} was not run and nothing was changed. Ask again if the edit is still wanted.",
                    js::stringify(&js::s(pin.path.clone()))
                ));
            }
        } else if write {
            let (target, kind) = match host
                .write_target(name, &call.args, self.chat_key.as_deref())
                .await
            {
                Target::Refused(_) => return self.revoked(host, TARGET_CHANGED).await,
                Target::Found { target, kind } if !target.is_empty() => {
                    (Some(target), kind.filter(|k| !k.is_empty()))
                }
                Target::Found { kind, .. } => (None, kind.filter(|k| !k.is_empty())),
                Target::None => (None, None),
            };
            if target.as_deref() != shown || kind != kind_was {
                return self.revoked(host, TARGET_CHANGED).await;
            }
        }
        if permission == "ask" {
            let (binding, _, _) = asked_binding.unwrap_or((JValue::Null, None, true));
            let fresh = host
                .permission_binding(
                    self.chat_id.as_deref(),
                    &self.grant_scope,
                    &self.grant_holder,
                )
                .await;
            // chat-grant-context.cjs `matches`: both bindings there, the same user, scope, chat and context.
            let matches = |a: &JValue, b: &JValue| {
                a.is_object() && b.is_object() && js::stringify(a) == js::stringify(b)
            };
            // The grant itself is redeemed by core's execute bridge, with the binding read here (D17).
            let ok = match &fresh {
                Ok(now) => self.chat_id.is_none() || matches(&binding, now),
                Err(()) => false,
            };
            if !ok {
                return self.revoked(host, BINDING_CHANGED).await;
            }
            *final_binding = fresh.ok();
        }
        // The file may change during the final store read too.
        if let Some(pin) = edit {
            let now = self.edit_target(host, call).await;
            if now == EditTarget::Failed {
                return Some(lookup_failed(host, name));
            }
            if !pin.same(&now) {
                return self.revoked(host, EDIT_TARGET_MOVED).await;
            }
        }
        if host.left() {
            return Some(CANCELLED.into());
        }
        // Read as chat.cjs reads it: once for `block`, and again for `ask` when the call was not asked about.
        if host.tool_policy(name, write) == "block"
            || (permission != "ask" && host.tool_policy(name, write) == "ask")
        {
            return Some(PERMISSION_CHANGED.into());
        }
        if let Some(skills) = self.skills {
            if skills.revoked(host).await {
                let text = if skills.failed() {
                    CHECK_FAULT
                } else {
                    PERMISSION_CHANGED
                };
                return Some(text.into());
            }
        }
        None
    }

    /// chat.cjs `await revokeChatGrant(...)` before the refusal returns, so the next call's grant check
    /// cannot race it.
    async fn revoked<H: Host>(&self, host: &H, text: &str) -> Option<String> {
        host.revoke_grant(self.chat_id.as_deref()).await;
        Some(text.to_string())
    }

    /// S9's `notAsked`: the question could not be opened, or its wait could not be answered.
    fn not_asked<H: Host>(&mut self, host: &H, name: &str) -> String {
        let r = format!("ERROR: the approval for {name} could not be asked, so it was not run. Nothing was changed. Ask again.");
        audit_denied(host, name, "unavailable");
        self.not_run.insert(r.clone());
        r
    }

    /// S9's `notChecked`: the grant could not be redeemed at the front (or too late to act on).
    fn not_checked<H: Host>(&mut self, host: &H, name: &str) -> String {
        let r = format!("ERROR: the permission for {name} could not be checked, so it was not run. Nothing was changed. Ask again.");
        audit_denied(host, name, "unavailable");
        self.not_run.insert(r.clone());
        r
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
mod tests {
    use super::*;

    #[test]
    fn the_fingerprint_sorts_keys_and_keeps_bad_json() {
        let a = fingerprint("w", Some("t"), r#"{"b":1,"a":[{"d":2,"c":3}]}"#);
        let b = fingerprint("w", Some("t"), r#"{"a":[{"c":3,"d":2}],"b":1}"#);
        assert_eq!(a, b);
        assert_ne!(fingerprint("w", None, "{"), fingerprint("w", None, "{}"));
        assert_eq!(fingerprint("w", Some(""), "{}"), fingerprint("w", None, ""));
    }

    #[test]
    fn an_edit_is_the_same_file_only_with_the_same_path_and_account() {
        let found = |path: &str, account: Option<&str>| EditTarget::Found {
            path: path.into(),
            account: account.map(str::to_string),
            digest: "d".repeat(64),
        };
        let pin = Pin {
            path: "qa-notes.txt".into(),
            account: None,
            digest: "e".repeat(64),
        };
        // The digest is not compared (chat.cjs compares the path and `account || null`).
        assert!(pin.same(&found("qa-notes.txt", None)));
        assert!(pin.same(&found("qa-notes.txt", Some(""))));
        assert!(!pin.same(&found("qa-notes.txt", Some("qa-account"))));
        assert!(!pin.same(&found("QA Folder/Text/qa-notes.txt", None)));
        assert!(!pin.same(&EditTarget::Refused("gone".into())));
        assert!(!pin.same(&EditTarget::Failed));
        let moved = Pin {
            account: Some("qa-account".into()),
            ..pin
        };
        assert!(moved.same(&found("qa-notes.txt", Some("qa-account"))));
        assert!(!moved.same(&found("qa-notes.txt", None)));
    }

    #[test]
    fn recent_writes_expire_and_are_bounded_per_chat() {
        let r = RecentWrites::default();
        r.record("u", Some("c"), "fp", 0.0);
        assert!(r.has("u", Some("c"), "fp", 1.0));
        assert!(!r.has("u", Some("d"), "fp", 1.0));
        assert!(!r.has("u", Some("c"), "fp", RECENT_TTL_MS + 1.0));
        for i in 0..25 {
            r.record("u", Some("c"), &format!("f{i}"), 2.0);
        }
        assert!(!r.has("u", Some("c"), "f0", 3.0));
        assert!(r.has("u", Some("c"), "f24", 3.0));
        assert!(!r.has("u", None, "f24", 3.0));
    }
}
