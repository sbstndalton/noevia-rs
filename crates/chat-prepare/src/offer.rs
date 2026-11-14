//! The tools a turn offers the model (slice P6a): chat.cjs's `requestToolboxes` (~523-536), the
//! block from `automaticTools` to `resolveFor(model)` (~1203-1324) without the hybrid route's
//! private-tools move and the Inference machines, and from `activeTools` to `routeNotes`
//! (~1641-1679), with the native executor's frame fields `tools`, `routes`, `writes`, `public`,
//! `automaticOnlyWrites`, `editTools`, `routeNote` and `templateGate` (~1726-1729).
//!
//! In chat.cjs's order, with the seams the caller answers in between:
//! 1. [`select`]: the turn's boxes (the project's or the default list, the account's connected
//!    connectors, the turn's own picks, the Diary add-on and sign-in filters), what Automatic adds
//!    for the message (#1550/#1692), the external-provider strip (rule 2 of provider-egress.cjs;
//!    the caller's `strip_private` is that strip's verdict on the provider) and the ungranted
//!    cloud route's strip (#1646).
//! 2. The tool router (`chatToolRouter.select(selectedBoxes, message)`): the embedding ranking
//!    (`tool-router.cjs` `route`) is the caller's answer, `None` when it did not route; [`routing`]
//!    is `chat-tool-routing.cjs` `select` around it: nothing to narrow with one box, and only boxes
//!    it was shown are kept, so the ranking can never add one the turn does not carry.
//! 3. [`offer`]: the additions kept ahead of the routed boxes, the writes only an addition
//!    reaches (asked per call, #1550), the account's blocks, `resolveTools` for the answering
//!    model, `more_tools` when routing narrowed, the `tools_scope` event and the frame's facts.
//! 4. The tool gate (`toolGate.evaluate(message, resolved.tools, ...)`) is the caller's; a
//!    decision other than `none` keeps the turn in Node today. [`Offer::route_note`] takes
//!    whether it ran and picked none.
//!
//! A move of the answering model after the gate (the tool-capable role, the long-context
//! profile) resolves again: [`offer`] with the new model. Recorded in
//! `tests/fixtures/node-tools.v1.json`, written by `tests/record_tools.cjs`, which runs those lines
//! of the pinned chat.cjs over a real `createToolboxes`.

use js_json::{wtf16, JValue};

use crate::history::Entry;
use crate::route_notes::{tool_route_notes, NotesInput};
use crate::toolboxes::{
    box_names, resolve_tools, NameNotText, Resolve, Resolved, Toolbox, Writes, CONNECTOR_BOXES,
    DEFAULT_TOOLBOXES, PROJECT_DOCS_BOX, SPECIALIST_PREFIX,
};
use crate::tools_mode::{self, Additions, PRIVATE_CORE_TOOLS};

/// `project-edit-target.cjs` `EDIT_TOOLS`.
pub const EDIT_TOOLS: [&str; 2] = ["project_append_file", "project_replace_text"];
/// The tool a narrowed turn may call once for the rest of its project's tools.
const MORE_TOOLS_JSON: &str = r#"{"type":"function","function":{"name":"more_tools","description":"Call this only if none of the offered tools can do the user's task. It makes all of this project's other tools available for your next step.","parameters":{"type":"object","properties":{}}}}"#;
/// `automaticHead`: the defaults and the Project documents box, resolved before any addition.
const HEAD: [&str; 2] = [DEFAULT_TOOLBOXES[0], PROJECT_DOCS_BOX];

/// What chat.cjs reads for a turn's tools.
pub struct ToolTurn<'a> {
    /// The turn's project record; `null` without one.
    pub project: &'a JValue,
    /// `diaryExtras?.PROJECT_ID` (`None` without the module).
    pub diary_extras_id: Option<&'a str>,
    /// `body.message`.
    pub message: &'a str,
    /// `mappedHistory` ([`crate::history`]).
    pub history: &'a [Entry],
    /// `body.turnToolboxes` (only a list is read).
    pub turn_toolboxes: &'a JValue,
    /// `body.compactOnly` is truthy.
    pub compact_only: bool,
    /// `allToolboxes()`.
    pub boxes: &'a [Toolbox],
    /// `connectedBoxes(chatUser)`.
    pub connected: &'a [String],
    /// `chatUser && authService.diaryEnabled(chatUser.id)` (the Diary add-on is on).
    pub diary_enabled: bool,
    /// `oauthServerIds()`.
    pub oauth_ids: &'a [String],
    /// `accountReady(chatUser?.id, id)`: the account signed in to that server.
    pub account_ready: &'a dyn Fn(&str) -> bool,
    /// `automaticToolsReady(chatUser)(box) === true` (a throw is `false`).
    pub automatic_ready: &'a dyn Fn(&Toolbox) -> bool,
    /// The write classification over `boxes` (chat.cjs's `isWriteTool`).
    pub writes: &'a Writes<'a>,
    /// `automaticLocal`: the turn does not leave the box (no external provider, no cloud route, the
    /// default provider or an Inference machine).
    pub local: bool,
    /// `egress.stripPrivateToolboxes` removes the private boxes (the provider is external, or its
    /// verdict is unknown).
    pub strip_private: bool,
    /// `cloudUngranted()`: still on a hybrid cloud route without a grant (after the move to a
    /// local model, which is the routing's).
    pub cloud_ungranted: bool,
}

/// chat.cjs `offBoxTurn`: the request leaves this box (an external provider, a cloud route, or a
/// provider other than the default one that is not an Inference machine); `local` is its negation.
pub fn off_box(external: bool, cloud_route: bool, default_provider: bool, machine: bool) -> bool {
    external || cloud_route || (!default_provider && !machine)
}

/// The ids of a project's boxes and the connected connectors: `toolboxes-permitted.cjs`
/// `selectedToolboxIds`, by the port core's dav-parse.wasm runs (op 2); a fault carries none.
fn selected_toolbox_ids(project: &JValue, connected: &[String]) -> Vec<String> {
    let strings = |v: &[&str]| JValue::Arr(v.iter().map(|s| JValue::from(*s)).collect());
    let projection = if project.truthy() {
        let toolboxes = match project.get("toolboxes") {
            JValue::Arr(items) => JValue::Arr(
                items
                    .iter()
                    .map(|i| i.as_str().map_or(JValue::Null, JValue::from))
                    .collect(),
            ),
            _ => JValue::Null,
        };
        JValue::obj([
            (
                "auto",
                JValue::Bool(project.get("toolsMode").as_str() == Some("auto")),
            ),
            (
                "docsDefaulted",
                JValue::Bool(project.get("docsToolboxDefaulted") == &JValue::Bool(true)),
            ),
            ("toolboxes", toolboxes),
        ])
    } else {
        JValue::Null
    };
    let args = JValue::Arr(vec![
        projection,
        strings(&DEFAULT_TOOLBOXES),
        JValue::from(PROJECT_DOCS_BOX),
        strings(&CONNECTOR_BOXES),
        JValue::Arr(connected.iter().map(|s| JValue::from(s.as_str())).collect()),
    ]);
    let mut input = vec![2u8];
    input.extend_from_slice(wtf16::stringify(&args).unwrap_or_default().as_bytes());
    let (status, out) = toolboxes_permitted::call(&input);
    let reply = std::str::from_utf8(&out).ok().filter(|_| status == 0);
    match reply.and_then(|t| wtf16::parse(t).ok()) {
        Some(JValue::Obj(items)) => match items.as_slice() {
            [(k, JValue::Arr(ids))] if k == "ids" => ids
                .iter()
                .filter_map(|i| i.as_str().map(str::to_string))
                .collect(),
            _ => Vec::new(),
        },
        _ => Vec::new(),
    }
}

/// `requestToolboxes(automaticAdded)`.
pub fn request_toolboxes(t: &ToolTurn<'_>, added: &[String]) -> Vec<String> {
    let base = selected_toolbox_ids(t.project, t.connected);
    let added: Vec<String> = added
        .iter()
        .filter(|id| !base.contains(id))
        .cloned()
        .collect();
    let mut selected = if added.is_empty() {
        base
    } else {
        let all: Vec<String> = base.iter().chain(&added).cloned().collect();
        tools_mode::order_with_additions(&all, &added, &HEAD)
    };
    if let JValue::Arr(ids) = t.turn_toolboxes {
        for id in ids.iter().filter_map(JValue::as_str) {
            if t.boxes.iter().any(|b| b.id == id)
                && !CONNECTOR_BOXES.contains(&id)
                && !selected.iter().any(|s| s == id)
            {
                selected.push(id.to_string());
            }
        }
    }
    // Known Node behaviour, kept for parity (chat.cjs:536): only the FIRST "diary" is removed, so a
    // project list naming the Diary twice keeps the second one without the add-on. Being filed.
    if !t.diary_enabled {
        if let Some(k) = selected.iter().position(|s| s == "diary") {
            selected.remove(k);
        }
    }
    selected.retain(|id| !t.oauth_ids.contains(id) || (t.account_ready)(id));
    selected
}

/// The boxes a turn carries before routing.
#[derive(Debug, Clone, PartialEq)]
pub struct Selection {
    /// `automaticTools`: Tools: Automatic, outside the Diary extras project.
    pub automatic_tools: bool,
    /// `selectedBoxes`.
    pub selected: Vec<String>,
    /// `automaticAdded`, as kept after the strips.
    pub added: Vec<String>,
    /// `privateStripped`: the ungranted cloud route took a private box (or the project reader).
    pub private_stripped: bool,
}

/// `project?.id !== diaryExtras?.PROJECT_ID`.
fn outside_diary_extras(project: &JValue, extras: Option<&str>) -> bool {
    match (project.get("id"), extras) {
        (JValue::Str(id), Some(x)) => id != x,
        (JValue::Undefined, None) => false,
        _ => true,
    }
}

/// `project?.files?.length` is truthy.
fn has_files(project: &JValue) -> bool {
    match project.get("files") {
        JValue::Arr(items) => !items.is_empty(),
        JValue::Str(s) => !s.is_empty(),
        JValue::Obj(_) => project.get("files").get("length").truthy(),
        _ => false,
    }
}

/// Step 1: the turn's boxes, Automatic's additions and the provider and route strips.
pub fn select(t: &ToolTurn<'_>) -> Selection {
    let automatic_tools =
        tools_mode::automatic(t.project) && outside_diary_extras(t.project, t.diary_extras_id);
    let is_write = |n: &str| t.writes.is_write(n);
    let mut added = Vec::new();
    if automatic_tools && !t.compact_only {
        let carried = tools_mode::carried_public_boxes(t.history, t.boxes);
        let base = request_toolboxes(t, &[]);
        added = tools_mode::automatic_additions(
            t.message,
            &Additions {
                base: &base,
                boxes: t.boxes,
                is_write: &is_write,
                ready: t.automatic_ready,
                private_data: t.local,
                carried: &carried,
            },
        );
    }
    let mut selected = request_toolboxes(t, &added);
    if t.strip_private {
        selected.retain(|id| id != "diary");
    }
    let mut private_stripped = false;
    if t.cloud_ungranted {
        let before = selected.len();
        selected.retain(|id| !tools_mode::is_private_data_box(id));
        private_stripped = selected.len() < before
            || (has_files(t.project) && selected.iter().any(|id| id == "core"));
    }
    added.retain(|id| selected.contains(id));
    Selection {
        automatic_tools,
        selected,
        added,
        private_stripped,
    }
}

/// `routing` (after [`offer`], with the additions kept in it).
#[derive(Debug, Clone, PartialEq)]
pub struct Routing {
    pub ids: Vec<String>,
    pub routed: bool,
    pub narrowed: bool,
}

/// What [`offer`] reads besides the turn and its selection.
pub struct OfferInput<'a> {
    /// The ranking's boxes (`route(...).boxes` when it routed; `None`: the router is off, the
    /// embeddings failed or nothing matched).
    pub ranked: Option<Vec<String>>,
    /// The answering model.
    pub model: &'a str,
    /// `prefill.budgetFor(model, 14000, scope)`: the measured budget when a truthy number.
    pub prefill: &'a dyn Fn(&str, Option<&str>) -> Option<f64>,
    /// `measureScope`: `provider.id` when the provider is an Inference machine (`measuredOn`).
    pub machine_scope: Option<&'a str>,
    /// `toolPolicy.mode(chatUser?.id, name, isWrite) === 'block'`.
    pub policy_blocks: &'a dyn Fn(&str, bool) -> bool,
    /// `templateGate`: the default provider, and a model manager that reads `/props`.
    pub template_gate: bool,
}

/// The tools a turn offers and what the frame carries of them.
#[derive(Debug, Clone, PartialEq)]
pub struct Offer {
    pub routing: Routing,
    /// `automaticOnlyWrites`: writes only an addition reaches, asked about per call.
    pub automatic_only_writes: Vec<String>,
    /// `resolved` (its `tools` are what the tool gate is shown).
    pub resolved: Resolved,
    /// `activeTools`: the resolved tools, and `more_tools` when routing narrowed.
    pub active_tools: Vec<JValue>,
    /// The `tools_scope` event.
    pub scope: JValue,
    /// The frame's `writes`, `public` and `editTools`.
    pub writes: Vec<String>,
    pub public: Vec<String>,
    pub edit_tools: Vec<String>,
    pub template_gate: bool,
    /// The boxes the route notes count as offered (those with a tool in `activeTools`).
    offered: Vec<String>,
}

fn names_of(boxes: &[Toolbox], ids: &[String]) -> Result<Vec<String>, NameNotText> {
    let mut out = Vec::new();
    for b in boxes.iter().filter(|b| ids.contains(&b.id)) {
        out.extend(box_names(b)?.into_iter().map(str::to_string));
    }
    Ok(out)
}

/// `chat-tool-routing.cjs` `select(selectedBoxes, message)` given the ranking's answer: the rows it
/// ranks are the selected boxes this server offers; with at most one, or no ranking, it keeps the
/// selection, and so with a box whose tools it cannot read. Otherwise the ranked ids it was shown (`result.boxes.filter((id) =>
/// inSelection.has(id))`, unique as the ranking's set is), in its order, narrowed when fewer than
/// the boxes shown.
pub fn routing(t: &ToolTurn<'_>, sel: &Selection, ranked: Option<&[String]>) -> Routing {
    let rows: Vec<&String> = sel
        .selected
        .iter()
        .filter(|id| t.boxes.iter().any(|b| &&b.id == id))
        .collect();
    let mut shown: Vec<&String> = Vec::new();
    for id in &rows {
        if !shown.contains(id) {
            shown.push(id);
        }
    }
    // `boxText` reads `t.function` of every tool of every row (the last box of an id, as the
    // `available` map holds it) before any embedding: a null tool throws there, and the router
    // keeps the selection ("embeddings unavailable").
    let readable = rows.iter().all(|id| {
        t.boxes
            .iter()
            .rev()
            .find(|b| &&b.id == id)
            .is_none_or(|b| !b.tools.iter().any(JValue::is_nullish))
    });
    match ranked {
        Some(ranked) if rows.len() > 1 && readable && !ranked.is_empty() => {
            let mut ids: Vec<String> = Vec::new();
            for id in ranked {
                if shown.contains(&id) && !ids.contains(id) {
                    ids.push(id.clone());
                }
            }
            let narrowed = ids.len() < shown.len();
            Routing {
                ids,
                routed: true,
                narrowed,
            }
        }
        _ => Routing {
            ids: sel.selected.clone(),
            routed: false,
            narrowed: false,
        },
    }
}

/// Why [`offer`] built nothing; the turn stays in Node.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refused {
    /// [`NameNotText`]: Node would offer a tool under a non-text name.
    NameNotText,
    /// Node throws: `routeNotes` reads `tool.function` of a `null` or `undefined` tool of an
    /// offered box that no offered tool matched before it.
    Thrown,
}

impl From<NameNotText> for Refused {
    fn from(_: NameNotText) -> Refused {
        Refused::NameNotText
    }
}

/// Step 3: the routed selection resolved for the answering model.
pub fn offer(t: &ToolTurn<'_>, sel: &Selection, o: OfferInput<'_>) -> Result<Offer, Refused> {
    let mut routing = routing(t, sel, o.ranked.as_deref());
    if routing.routed && !sel.added.is_empty() {
        let mut all = routing.ids.clone();
        all.extend(
            sel.added
                .iter()
                .filter(|id| !routing.ids.contains(id))
                .cloned(),
        );
        let ids = tools_mode::order_with_additions(&all, &sel.added, &HEAD);
        routing.narrowed = ids.len() < sel.selected.len();
        routing.ids = ids;
    }
    let mut automatic_only_writes: Vec<String> = Vec::new();
    if !sel.added.is_empty() {
        let rest: Vec<String> = sel
            .selected
            .iter()
            .filter(|id| !sel.added.contains(id))
            .cloned()
            .collect();
        let elsewhere = names_of(t.boxes, &rest)?;
        for name in names_of(t.boxes, &sel.added)? {
            if !elsewhere.contains(&name)
                && t.writes.is_write(&name)
                && !automatic_only_writes.contains(&name)
            {
                automatic_only_writes.push(name);
            }
        }
    }
    let blocked = |name: &str| {
        (o.policy_blocks)(name, t.writes.is_write(name))
            || (t.cloud_ungranted && PRIVATE_CORE_TOOLS.contains(&name))
    };
    let ids = if routing.routed {
        &routing.ids
    } else {
        &sel.selected
    };
    let specialist = t
        .project
        .get("id")
        .as_str()
        .is_some_and(|id| id.starts_with(SPECIALIST_PREFIX));
    let resolved = resolve_tools(
        ids,
        &Resolve {
            boxes: t.boxes,
            model: o.model,
            skip: &blocked,
            specialist,
            measured: (o.prefill)(o.model, o.machine_scope),
        },
    )?;
    let mut active_tools = resolved.tools.clone();
    if routing.narrowed {
        if let Ok(more) = wtf16::parse(MORE_TOOLS_JSON) {
            active_tools.push(more);
        }
    }
    let scope_ids = if routing.narrowed || (!sel.added.is_empty() && routing.routed) {
        Some(&routing.ids)
    } else if !sel.added.is_empty() {
        Some(&sel.selected)
    } else {
        None
    };
    let scope = scope_event(t.boxes, scope_ids.map(Vec::as_slice));
    let names: Vec<&str> = active_tools
        .iter()
        .filter_map(|tool| tool.get("function").get("name").as_str())
        .collect();
    // routeNotes' `offered`: a box this server does not offer is kept, one of its own only with a
    // tool in activeTools (`tools.some((tool) => names.has(tool.function?.name))`).
    let mut offered = Vec::new();
    for id in ids {
        let mut keep = true;
        if let Some(b) = t.boxes.iter().find(|b| &b.id == id) {
            keep = false;
            for tool in &b.tools {
                if tool.is_nullish() {
                    return Err(Refused::Thrown);
                }
                if let Some(n) = tool.get("function").get("name").as_str() {
                    if names.contains(&n) {
                        keep = true;
                        break;
                    }
                }
            }
        }
        if keep {
            offered.push(id.clone());
        }
    }
    let pick = |f: &dyn Fn(&str) -> bool| {
        names
            .iter()
            .filter(|n| f(n))
            .map(|n| n.to_string())
            .collect()
    };
    let writes = pick(&|n| t.writes.is_write(n));
    let public = pick(&|n| tools_mode::is_public_tool_name(Some(n), t.boxes));
    let edit_tools = pick(&|n| EDIT_TOOLS.contains(&n));
    Ok(Offer {
        routing,
        automatic_only_writes,
        resolved,
        active_tools,
        scope,
        writes,
        public,
        edit_tools,
        template_gate: o.template_gate,
        offered,
    })
}

/// `send({ type: 'tools_scope', text, boxes })`.
fn scope_event(boxes: &[Toolbox], ids: Option<&[String]>) -> JValue {
    let found = |id: &str| boxes.iter().find(|b| b.id == id);
    let label = |id: &str| match found(id) {
        Some(b) if !b.label.is_empty() => b.label.clone(),
        _ => id.to_string(),
    };
    let mut event = vec![
        ("type".to_string(), JValue::from("tools_scope")),
        (
            "text".to_string(),
            JValue::from(ids.map_or(String::new(), |ids| {
                ids.iter()
                    .map(|id| label(id))
                    .collect::<Vec<_>>()
                    .join(", ")
            })),
        ),
    ];
    if let Some(ids) = ids {
        let info = ids.iter().map(|id| {
            JValue::obj([
                ("id", JValue::from(id.as_str())),
                ("label", JValue::from(label(id))),
                (
                    "inApp",
                    JValue::Bool(found(id).is_some_and(Toolbox::is_in_app)),
                ),
            ])
        });
        event.push(("boxes".to_string(), JValue::Arr(info.collect())));
    }
    JValue::Obj(event)
}

impl Offer {
    /// `routeNotes()`; `gate_none`: the tool gate ran on this request and picked no tool.
    pub fn route_note(&self, t: &ToolTurn<'_>, sel: &Selection, gate_none: bool) -> String {
        let on_server: Vec<String> = t.boxes.iter().map(|b| b.id.clone()).collect();
        tool_route_notes(&NotesInput {
            message: t.message,
            offered: &self.offered,
            on_server: &on_server,
            automatic: sel.automatic_tools,
            local_model: t.local,
            gate_none,
        })
    }

    /// The native executor's frame fields of the turn's tools, in the frame's order.
    pub fn frame_fields(&self, route_note: String) -> Vec<(&'static str, JValue)> {
        let texts =
            |v: &[String]| JValue::Arr(v.iter().map(|s| JValue::from(s.as_str())).collect());
        let routes =
            self.resolved.routes.iter().map(|(n, s)| {
                JValue::Arr(vec![JValue::from(n.as_str()), JValue::from(s.as_str())])
            });
        vec![
            ("tools", JValue::Arr(self.active_tools.clone())),
            ("routes", JValue::Arr(routes.collect())),
            ("writes", texts(&self.writes)),
            ("public", texts(&self.public)),
            ("automaticOnlyWrites", texts(&self.automatic_only_writes)),
            ("editTools", texts(&self.edit_tools)),
            // chat.cjs: `names.filter((n) => specialistPageSave.neverStanding(n))`, always present
            // here (rust-chat-native.cjs preparedTurn keeps it only when non-empty).
            (
                "onceTools",
                JValue::Arr(
                    self.active_tools
                        .iter()
                        .filter_map(|t| t.get("function").get("name").as_str())
                        .filter(|n| crate::toolboxes::never_standing(n))
                        .map(JValue::from)
                        .collect(),
                ),
            ),
            ("routeNote", JValue::from(route_note)),
            ("templateGate", JValue::Bool(self.template_gate)),
        ]
    }
}
