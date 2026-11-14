//! The wire once the route is known (#778 routing modes, slice P5d, part 2): noevia-core
//! `server/chat.cjs` ~1014-1027 (#815 the linked brain block dropped and #1587 the time part's
//! zone left out on an off-box turn), ~1179-1222 (#1646 an ungranted hybrid cloud route that
//! would carry a private-data box moves the turn local) and ~1262-1296 (#1703 the replayed
//! history rebuilt with the off-box filter once the route is settled, #1646 no private-data box
//! and no reader hint on a still-ungranted cloud route).
//!
//! [`OffBox`] owns chat.cjs's order and asks the caller for every seam ([`Seam`]), as
//! [`crate::modes::Modes`] does. It starts from the [`Prepared`] wire, the
//! [`crate::modes::RouteTarget`] P5d's first part decided and the provider the turn answers
//! through (`{ ...row, external: true }` or `getProvider(...)`, with
//! `egress.isExternalProvider`'s answer). Then, in order:
//!
//! 1. Off the box ([`off_box_turn`]): the brain block goes (`sys = sysWithoutBrain`, the wire
//!    rebuilt from the replay) and the time part is swapped for the zoneless one
//!    (`sys.replace(timePart, zoneless)`, the first match, in `sys` and the system message).
//! 2. On an ungranted hybrid cloud route (`cloudUngranted()`) of a turn that is not `compactOnly`:
//!    [`Seam::Wanted`], whether this turn needs a private-data box (its own picks, what Automatic
//!    would add, or the tool gate's pick: the caller's, from its toolboxes and gate). When it does,
//!    `rerouteLocalForPrivateTools`: [`Seam::AutoRoles`] (a fresh `autoRoles()`), and for an
//!    Automatic turn with a local model (`roles[routedRole] || smart || fast || code`) the target
//!    becomes `{ route: 'local', reason: 'sensitive-rule', detail: 'private-tools' }`,
//!    [`Seam::DefaultProvider`] (`getProvider(DEFAULT_PROVIDER_ID)` and its external answer), the
//!    codes-only [`Seam::Log`] entry (a throw is swallowed) and the `[routing]` [`Seam::Line`].
//!    A moved turn answers through the default provider with no ChatGPT connection and the plain
//!    fetch ([`Settled::moved`]); its model is [`Settled::model`].
//! 3. [`Seam::Settled`]: the provider once the Inference-machine rules (#1840/#1849) ran, the
//!    caller's; then [`Seam::Boxes`], `requestToolboxes(automaticAdded)` after
//!    `egress.stripPrivateToolboxes`, asked with whether the turn is on the box (`automaticLocal`).
//! 4. Off the box: the replay is rebuilt from `replaySource` with `offBox: true`; the current
//!    message keeps what was added to it (the attached files block), the wire keeps its head.
//! 5. Still `cloudUngranted()`: every private-data box is taken out ([`Settled::private_stripped`],
//!    also set when the project has files and `core` is offered: `read_project_file` is blocked
//!    later, the caller's), and the reader is taken out of the prompt: [`SKILL_INDEX_LEAD`] becomes
//!    [`SKILL_INDEX_LEAD_NO_READER`], both read-file hints go, in `sys` and in every wire message
//!    with text content (the first match each).
//!
//! Note the order Node keeps: steps 1 and 2 run before the move, so a moved turn still lost the
//! brain block and the zone (the route was cloud when they ran) while its history is the on-box
//! replay. [`Prepared::parts`] is left as built (Node's `sysParts` keeps the brain block too).
//!
//! Later parts (not here): `holdForCloud` (a round-time rule), the egress and ChatGPT checks, the
//! private-tool refusal (`egressToolRefusal`) and the per-round system note, the route notes.
//!
//! Recorded in `tests/fixtures/node-offbox.v1.json`, written by `tests/record_offbox.cjs`; no
//! named differences.

use js_json::JValue;

use crate::history::{self, Entry, ReplayOptions};
use crate::modes::{off_box_turn, RouteTarget};
use crate::skills::SKILL_INDEX_LEAD;
use crate::tools_mode::is_private_data_box;
use crate::{js, time, Prepared};

/// chat.cjs SKILL_INDEX_LEAD_NO_READER.
pub const SKILL_INDEX_LEAD_NO_READER: &str =
    "Available skills (their files cannot be loaded in this reply; do not guess their contents):";
/// rag.cjs READ_FILE_HINT.
pub const RAG_READ_FILE_HINT: &str =
    " Use read_project_file with PDF startPage/endPage and offset for pages beyond the summary.";

/// What the off-box rules read of the turn.
#[derive(Clone, Copy)]
pub struct OffBoxTurn<'a> {
    /// `wantsAuto`: a Manual turn never moves.
    pub wants_auto: bool,
    /// `routedRole` (`null` for a Manual turn).
    pub routed_role: Option<&'a str>,
    /// `body.compactOnly` is truthy.
    pub compact_only: bool,
    pub default_provider_id: &'a str,
    /// `brainBlock` (`''`: none).
    pub brain_block: &'a str,
    /// `currentNow()` and `serverTimeZone()` (as for [`crate::Turn`]).
    pub now_ms: i64,
    pub time_zone: &'a str,
    /// `replaySource`: `mappedHistory`, or what the skill scrub left of it.
    pub replay_source: &'a [Entry],
    /// `message` (the turn's text; unused for `compactOnly`).
    pub message: &'a str,
    pub carry_orphan_tool: &'a (dyn Fn(&str) -> bool + Sync),
    pub known_tool: &'a (dyn Fn(&str) -> bool + Sync),
    /// `project?.files?.length` is truthy.
    pub project_files: bool,
}

/// What the caller is asked.
#[derive(Debug, Clone, PartialEq)]
pub enum Seam {
    /// Whether this turn needs a private-data box: [`Reply::Wanted`].
    Wanted,
    /// `autoRoles()`: [`Reply::Roles`].
    AutoRoles,
    /// `getProvider(DEFAULT_PROVIDER_ID)` and `egress.isExternalProvider` of it: [`Reply::Provider`].
    DefaultProvider,
    /// `routingModes.log(entry)`: [`Reply::Done`] (a throw is swallowed: also `Done`).
    Log(JValue),
    /// `console.log(line)`: [`Reply::Done`].
    Line(String),
    /// The provider once the Inference-machine rules ran: [`Reply::Provider`].
    Settled,
    /// `requestToolboxes(automaticAdded)` after the egress strip; `local` is `automaticLocal`:
    /// [`Reply::Boxes`].
    Boxes { local: bool },
}

#[derive(Debug, Clone, PartialEq)]
pub enum Reply {
    /// The first step.
    Start,
    Wanted(bool),
    Roles(JValue),
    Provider { row: JValue, external: bool },
    Done,
    Boxes(Vec<String>),
}

/// The turn once its route is settled.
#[derive(Debug, Clone, PartialEq)]
pub struct Settled {
    pub target: Option<RouteTarget>,
    pub provider: JValue,
    pub external: bool,
    /// The private-tools move ran: the default provider, `chatgptProvider = false`, the plain fetch.
    pub moved: bool,
    /// The moved turn's model (`localModel`).
    pub model: Option<JValue>,
    /// `brainBlock` was cleared (step 1).
    pub brain_dropped: bool,
    pub prepared: Prepared,
    /// `selectedBoxes` after step 5.
    pub boxes: Vec<String>,
    pub private_stripped: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Step {
    Ask(Seam),
    Done(Box<Settled>),
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum At {
    Start,
    Wanted,
    Roles,
    Provider,
    Log,
    Line,
    Settled,
    Boxes,
    Finished,
}

/// The step machine (see the module doc).
pub struct OffBox<'a> {
    t: OffBoxTurn<'a>,
    at: At,
    out: Settled,
}

fn cloud_ungranted(target: Option<&RouteTarget>) -> bool {
    target.is_some_and(|t| t.route == "cloud" && t.hybrid_ungranted)
}

/// `system ? [{ role: 'system', content: system }, ...history] : history`.
fn wire(system: &str, history: &[JValue]) -> Vec<JValue> {
    let mut out = Vec::with_capacity(history.len() + 1);
    if !system.is_empty() {
        out.push(js::message("system", system.to_string()));
    }
    out.extend(history.iter().cloned());
    out
}

fn content_mut(m: &mut JValue) -> Option<&mut String> {
    let JValue::Obj(items) = m else { return None };
    match items.iter_mut().find(|(k, _)| k == "content") {
        Some((_, JValue::Str(s))) => Some(s),
        _ => None,
    }
}

/// `text.replace(from, to)`: the first match.
fn replace_first(text: &mut String, from: &str, to: &str) {
    if let Some(at) = text.find(from) {
        text.replace_range(at..at + from.len(), to);
    }
}

/// `dropReader(text)`.
fn drop_reader(text: &mut String) {
    replace_first(text, SKILL_INDEX_LEAD, SKILL_INDEX_LEAD_NO_READER);
    replace_first(text, RAG_READ_FILE_HINT, "");
    replace_first(text, crate::sources::READ_FILE_HINT, "");
}

impl<'a> OffBox<'a> {
    pub fn new(
        t: OffBoxTurn<'a>,
        prepared: Prepared,
        target: Option<RouteTarget>,
        provider: JValue,
        external: bool,
    ) -> Self {
        OffBox {
            t,
            at: At::Start,
            out: Settled {
                target,
                provider,
                external,
                moved: false,
                model: None,
                brain_dropped: false,
                prepared,
                boxes: Vec::new(),
                private_stripped: false,
            },
        }
    }

    fn off_box(&self) -> bool {
        off_box_turn(
            self.out.external,
            self.out.target.as_ref(),
            &self.out.provider,
            self.t.default_provider_id,
        )
    }

    fn ask(&mut self, at: At, seam: Seam) -> Step {
        self.at = at;
        Step::Ask(seam)
    }

    /// Steps 1 and 2 up to the first seam.
    fn start(&mut self) -> Step {
        if self.off_box() {
            let p = &mut self.out.prepared;
            if !self.t.brain_block.is_empty() {
                // `sysParts.filter((part) => part !== brainBlock).join('\n\n')`
                let kept: Vec<&str> = p
                    .parts
                    .parts()
                    .iter()
                    .map(|(_, text)| text.as_str())
                    .filter(|text| *text != self.t.brain_block)
                    .collect();
                p.system = kept.join("\n\n");
                p.messages = wire(&p.system, &p.history);
                self.out.brain_dropped = true;
            }
            let holds_time = p.messages.first().is_some_and(|m| {
                m.get("role").as_str() == Some("system")
                    && m.get("content")
                        .as_str()
                        .is_some_and(|c| c.contains(p.time_part.as_str()))
            });
            if holds_time {
                if let Some(zoneless) = time::part(self.t.now_ms, self.t.time_zone, false) {
                    replace_first(&mut p.system, &p.time_part, &zoneless);
                    if let Some(c) = p.messages.first_mut().and_then(content_mut) {
                        replace_first(c, &p.time_part, &zoneless);
                    }
                }
            }
        }
        if cloud_ungranted(self.out.target.as_ref()) && !self.t.compact_only {
            return self.ask(At::Wanted, Seam::Wanted);
        }
        self.ask(At::Settled, Seam::Settled)
    }

    /// `rerouteLocalForPrivateTools`, once the roles are read.
    fn reroute(&mut self, roles: &JValue) -> Step {
        let role = |name: &str| roles.opt(name).clone();
        let local = [
            self.t.routed_role.map_or(JValue::Undefined, role),
            role("smart"),
            role("fast"),
            role("code"),
        ]
        .into_iter()
        .find(JValue::truthy);
        let (true, Some(local), Some(target)) =
            (self.t.wants_auto && roles.truthy(), local, self.out.target.as_ref())
        else {
            return self.ask(At::Settled, Seam::Settled);
        };
        self.out.target = Some(RouteTarget {
            route: "local",
            reason: JValue::from("sensitive-rule"),
            detail: Some(JValue::from("private-tools")),
            cloud: target.cloud.clone(),
            row: None,
            hybrid_ungranted: false,
        });
        self.out.moved = true;
        self.out.model = Some(local);
        self.ask(At::Provider, Seam::DefaultProvider)
    }

    pub fn step(&mut self, reply: Reply) -> Step {
        match (self.at, reply) {
            (At::Start, Reply::Start) => self.start(),
            (At::Wanted, Reply::Wanted(false)) => self.ask(At::Settled, Seam::Settled),
            (At::Wanted, Reply::Wanted(true)) => self.ask(At::Roles, Seam::AutoRoles),
            (At::Roles, Reply::Roles(roles)) => self.reroute(&roles),
            (At::Provider, Reply::Provider { row, external }) => {
                self.out.provider = row;
                self.out.external = external;
                let entry = JValue::obj([
                    ("mode", JValue::from("hybrid")),
                    ("route", JValue::from("local")),
                    ("reason", JValue::from("sensitive-rule")),
                    ("flag", JValue::from("private-tools")),
                ]);
                self.ask(At::Log, Seam::Log(entry))
            }
            (At::Log, Reply::Done) => self.ask(
                At::Line,
                Seam::Line(
                    "[routing] mode=hybrid route=local reason=sensitive-rule flag=private-tools"
                        .to_string(),
                ),
            ),
            (At::Line, Reply::Done) => self.ask(At::Settled, Seam::Settled),
            (At::Settled, Reply::Provider { row, external }) => {
                self.out.provider = row;
                self.out.external = external;
                let local = !self.off_box();
                self.ask(At::Boxes, Seam::Boxes { local })
            }
            (At::Boxes, Reply::Boxes(boxes)) => {
                self.out.boxes = boxes;
                self.finish();
                self.at = At::Finished;
                Step::Done(Box::new(self.out.clone()))
            }
            (at, reply) => panic!("offbox: {reply:?} does not answer the step at {at:?}"),
        }
    }

    /// Steps 4 and 5.
    fn finish(&mut self) {
        if self.off_box() {
            let p = &mut self.out.prepared;
            let opts = ReplayOptions {
                carry_orphan_tool: self.t.carry_orphan_tool,
                off_box: true,
                known_tool: self.t.known_tool,
            };
            let message = (!self.t.compact_only).then_some(self.t.message);
            let mut filtered = history::normalize(self.t.replay_source, message, &opts);
            if !self.t.compact_only {
                if let (Some(last), Some(current)) = (filtered.last_mut(), p.history.last()) {
                    *last = current.clone();
                }
            }
            let head = p.messages.len().saturating_sub(p.history.len());
            p.messages.truncate(head);
            p.messages.extend(filtered.iter().cloned());
            p.history = filtered;
        }
        if cloud_ungranted(self.out.target.as_ref()) {
            let before = self.out.boxes.len();
            self.out.boxes.retain(|id| !is_private_data_box(id));
            self.out.private_stripped = self.out.boxes.len() != before
                || (self.t.project_files && self.out.boxes.iter().any(|id| id == "core"));
            let p = &mut self.out.prepared;
            drop_reader(&mut p.system);
            for m in &mut p.messages {
                if let Some(c) = content_mut(m) {
                    drop_reader(c);
                }
            }
        }
    }
}

/// Drives [`OffBox`] to the end with the caller's answers (for a caller with synchronous seams).
pub fn settle(
    mut machine: OffBox<'_>,
    mut answer: impl FnMut(&Seam) -> Reply,
) -> Box<Settled> {
    let mut step = machine.step(Reply::Start);
    loop {
        match step {
            Step::Done(settled) => return settled,
            Step::Ask(seam) => {
                let reply = answer(&seam);
                step = machine.step(reply);
            }
        }
    }
}
