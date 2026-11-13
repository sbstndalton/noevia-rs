//! The rest of a turn's model routing (slice P5c), after [`crate::roles`] (P5b) picked the model
//! and [`crate::offer`] (P6a) resolved its tools: noevia-core `server/chat.cjs` ~1330-1400.
//!
//! 1. The move to a tool-capable role (#1550, `toolCapableRoute` of `tools-mode.cjs`): an
//!    Automatic turn whose tool gate says a tool is needed, on Tools: Automatic, whose routed
//!    model's chat template cannot take tools, goes to the first of Smart, Code and Fast whose
//!    model can ([`tool_move_applies`], [`ToolMove`]). The template verdict
//!    (`templateToolsGate.allowsTools(modelManager, model, signal)`) is the caller's seam: it asks
//!    [`ToolMove::candidates`] in order and stops at the first that takes tools.
//! 2. The long-context profile (#1079): Context: High on the local default provider is served by
//!    the model's `<model>-long` entry when the served catalogue pairs one
//!    ([`long_profile_applies`], then the caller's `servedCatalogue()`, then [`long_pick`]); the
//!    pick is the `long-profile` crate's `run_json`, the same code dav-parse.wasm runs for
//!    `long-profile.cjs` `pick`, over the same JSON request.
//! 3. The offered tools follow the answering model: when the move or the profile changed it
//!    ([`model_changed`]), the caller calls [`crate::offer::offer`] again with the new model, as
//!    chat.cjs calls `resolveFor(model)` (its prefill measurement is asked for the new model).
//!
//! What the turn then sends of it: the meta event's `model`, `route` (the moved role),
//! `routingDecision` ([`Moved::decision`]) and `contextProfile` ([`LongPick::context_profile`]),
//! the status line after the meta event ([`status_event`]), the move's log line
//! ([`Moved::log_line`]) and the long-profile module's failure line ([`LongPick::log_line`]).
//!
//! Preconditions (later slices): no Inference machines (`chatMachines` null: every role is on
//! this machine, `hostOf` is the same for all, the move never changes the provider and
//! `resolveFor` measures no machine scope) and no routing modes (`routeTarget` null; the
//! `route_row` inputs keep chat.cjs's `!routeTarget?.row` condition for that slice). The model
//! values are text, as [`crate::offer::OfferInput::model`] requires. `routingDecision` is an object
//! or null, as `classifyFastOrSmart` answers it. A turn whose images are read (`readingImages`)
//! is not recorded here: [`status_event`] states chat.cjs's line for it.
//!
//! Recorded in `tests/fixtures/node-reroute.v1.json`, written by `tests/record_reroute.cjs`
//! (whole turns through `createChatHandler` with the seams stubbed and core's own
//! createLongProfiles over the pinned dav-parse.wasm); no named differences.

use js_json::{wtf16, JValue};

use crate::roles::{strict_eq, Role};
use crate::{tools_mode, Thrown};

/// `tools-mode.cjs` `FALLBACK_ROLES`: the roles tried, in order, when the routed one cannot take
/// tools.
pub const FALLBACK_ROLES: [Role; 3] = [Role::Smart, Role::Code, Role::Fast];

/// `gateSaysToolNeeded(gate)`: the gate's answer names a decision other than `'none'`. `gate` is
/// `null` when there is no gate or the turn is `compactOnly`.
pub fn gate_says_tool_needed(gate: &JValue) -> bool {
    let decision = gate.get("decision");
    gate.truthy() && decision.truthy() && decision.as_str() != Some("none")
}

/// What decides whether the move is considered.
#[derive(Debug, Clone, Copy)]
pub struct MoveTurn<'a> {
    /// The turn's project record; `null` without one.
    pub project: &'a JValue,
    /// `wantsAuto`.
    pub wants_auto: bool,
    /// `body.compactOnly` is truthy.
    pub compact_only: bool,
    /// `routeTarget?.row` is truthy (a routing mode's cloud route); false under the preconditions.
    pub route_row: bool,
    /// `provider.id === DEFAULT_PROVIDER_ID`.
    pub default_provider: bool,
    /// `localToolsGate`: `typeof modelManager?.props === 'function'`.
    pub local_tools_gate: bool,
    /// [`gate_says_tool_needed`] of the tool gate's answer.
    pub gate_needs_tool: bool,
}

/// chat.cjs's condition for the move (#1550): an Automatic turn on this machine's engine with a
/// template gate, Tools: Automatic (`toolsModeOf(project) === 'auto'`, the Diary extras project
/// included) and a gate that says a tool is needed.
pub fn tool_move_applies(t: &MoveTurn<'_>) -> bool {
    t.wants_auto
        && !t.compact_only
        && !t.route_row
        && t.default_provider
        && t.local_tools_gate
        && tools_mode::automatic(t.project)
        && t.gate_needs_tool
}

/// A model the move asks the template gate about, and the role it answers as.
#[derive(Debug, Clone, PartialEq)]
pub struct Candidate {
    pub model: JValue,
    pub role: Role,
}

/// `toolCapableRoute({ model, routedRole, roles, allowsTools })` without `hostOf`.
#[derive(Debug, Clone, PartialEq)]
pub struct ToolMove {
    model: JValue,
    role: Role,
    candidates: Vec<Candidate>,
}

impl ToolMove {
    /// `roles` is `autoRoles()`, read again (the same value as P5b's within a turn).
    pub fn new(model: &JValue, routed: Role, roles: &JValue) -> ToolMove {
        let mut candidates = vec![Candidate {
            model: model.clone(),
            role: routed,
        }];
        for role in FALLBACK_ROLES {
            let candidate = roles.get(role.as_str());
            if !candidate.truthy() || role == routed || strict_eq(candidate, model) {
                continue;
            }
            candidates.push(Candidate {
                model: candidate.clone(),
                role,
            });
        }
        ToolMove {
            model: model.clone(),
            role: routed,
            candidates,
        }
    }

    /// The `allowsTools(model, role)` questions in order: the routed model first, then each
    /// fallback role with a model of its own. The caller stops at the first answered true.
    pub fn candidates(&self) -> &[Candidate] {
        &self.candidates
    }

    /// The outcome given the index of the first candidate that takes tools (`None`: none does).
    /// The routed model taking tools, or none doing so, keeps the turn where it is.
    pub fn settle(&self, first_allowed: Option<usize>) -> Moved {
        match first_allowed.and_then(|k| self.candidates.get(k).filter(|_| k > 0)) {
            Some(c) => Moved {
                rerouted: true,
                from: self.model.clone(),
                model: c.model.clone(),
                role: c.role,
            },
            None => Moved {
                rerouted: false,
                from: self.model.clone(),
                model: self.model.clone(),
                role: self.role,
            },
        }
    }
}

/// `moved`: where the turn goes.
#[derive(Debug, Clone, PartialEq)]
pub struct Moved {
    pub rerouted: bool,
    /// The routed model it leaves.
    from: JValue,
    /// `model` after the move.
    pub model: JValue,
    /// `routedRole` after the move.
    pub role: Role,
}

impl Moved {
    /// `routingDecision` after the move: `{ ...routingDecision, effectiveRole: routedRole }` when it
    /// is set and the turn moved, else as it was.
    pub fn decision(&self, decision: &JValue) -> JValue {
        let mut out = decision.clone();
        if self.rerouted {
            if let JValue::Obj(items) = &mut out {
                wtf16::set(items, "effectiveRole", JValue::from(self.role.as_str()));
            }
        }
        out
    }

    /// The `[tools] ... cannot take tools` line of a move (`None`: the turn did not move).
    pub fn log_line(&self) -> Result<Option<String>, Thrown> {
        if !self.rerouted {
            return Ok(None);
        }
        let text = |v: &JValue| js_json::to_js_string(v).map_err(|_| Thrown);
        Ok(Some(format!(
            "[tools] {} cannot take tools; this Automatic turn needs a tool and goes to {} ({})",
            text(&self.from)?,
            self.role.as_str(),
            text(&self.model)?
        )))
    }
}

/// `long-profile.cjs` `normalizeProfile(project?.contextProfile) === 'high'`.
pub fn context_high(project: &JValue) -> bool {
    project.get("contextProfile").as_str() == Some("high")
}

/// chat.cjs's condition for the long-context profile: the local default provider, no routing
/// mode's cloud route, Context: High. The caller then reads `servedCatalogue()` (an error is the
/// turn's, as Node's rejection is).
pub fn long_profile_applies(project: &JValue, default_provider: bool, route_row: bool) -> bool {
    default_provider && !route_row && context_high(project)
}

/// The long-context profile's outcome.
#[derive(Debug, Clone, PartialEq)]
pub struct LongPick {
    /// `model` after it: the long entry, or the model itself.
    pub model: JValue,
    /// `contextProfile`: `'high'` when the model has (or is) a long profile, else `'low'`.
    pub context_profile: &'static str,
    /// `contextSwitch`: the model changed and its row does not say the long entry is loaded.
    pub context_switch: bool,
    /// The long-profile module refused the request (its reason code); the pick is then the model.
    pub unavailable: Option<String>,
}

impl LongPick {
    /// `createLongProfiles`' `[long-profile] pick unavailable: <reason>` (logged once per handler
    /// for the same text).
    pub fn log_line(&self) -> Option<String> {
        self.unavailable
            .as_ref()
            .map(|r| format!("[long-profile] pick unavailable: {r}"))
    }
}

/// dav-parse-wasm.cjs `longProfilePick` over `long_profile::run_json`: `(model, long)`, or the
/// refusal's code.
fn pick_request(model: &JValue, pairs: Vec<JValue>) -> Result<(JValue, bool), String> {
    let request = JValue::obj([
        ("op", JValue::from("pick")),
        ("model", model.clone()),
        ("profile", JValue::from("high")),
        ("pairs", JValue::Arr(pairs)),
    ]);
    // `JSON.stringify` of JSON data never fails; its text is well formed (a lone surrogate is an
    // escape, which the module's JSON reader refuses as Node's wasm does).
    let text = wtf16::stringify(&request).ok_or_else(|| "input".to_string())?;
    let (status, reply) = long_profile::run_json(&text);
    let reply = wtf16::parse(&reply).map_err(|_| "reply".to_string())?;
    if status != 0 {
        return Err(reply.get("error").as_str().unwrap_or("unknown").to_string());
    }
    match (reply.get("model"), reply.get("long")) {
        (m @ JValue::Str(_), JValue::Bool(long)) => Ok((m.clone(), *long)),
        _ => Err("reply".to_string()),
    }
}

/// The long-context profile for `model` given `servedCatalogue()`'s answer (`null` or anything but
/// a list: no rows). A `null` or `undefined` row throws, as Node's `m.longVariant` does.
pub fn long_pick(model: &JValue, catalogue: &JValue) -> Result<LongPick, Thrown> {
    let rows: &[JValue] = match catalogue {
        JValue::Arr(rows) => rows,
        _ => &[],
    };
    let mut pairs = Vec::new();
    for m in rows {
        if m.is_nullish() {
            return Err(Thrown);
        }
        let long = m.get("longVariant");
        if long.as_str().is_some() {
            pairs.push(JValue::obj([
                ("base", m.get("name").clone()),
                ("long", long.clone()),
            ]));
        }
    }
    let (picked, long, unavailable) = match pick_request(model, pairs) {
        Ok((picked, long)) => (picked, long, None),
        Err(reason) => (model.clone(), false, Some(reason)),
    };
    let mut out = LongPick {
        model: model.clone(),
        context_profile: if long { "high" } else { "low" },
        context_switch: false,
        unavailable,
    };
    if long && !strict_eq(&picked, model) {
        out.context_switch = rows
            .iter()
            .find(|m| strict_eq(m.get("name"), model))
            .is_none_or(|m| m.get("longLoaded") != &JValue::Bool(true));
        out.model = picked;
    }
    Ok(out)
}

/// `model !== resolvedFor`: the tools were resolved for another model and are resolved again.
pub fn model_changed(model: &JValue, resolved_for: &JValue) -> bool {
    !strict_eq(model, resolved_for)
}

/// The status line chat.cjs sends after the meta event: the long-context switch's (#1079) unless
/// the turn reads images, else the image or the plain preparing line.
pub fn status_event(context_switch: bool, reading_images: bool) -> JValue {
    let (id, text) = if context_switch && !reading_images {
        (
            "loadingLongContext",
            "Loading the high-context profile — switching models may take a moment…",
        )
    } else if reading_images {
        (
            "readingImages",
            "Reading image sources — model loading and visual processing may take a moment…",
        )
    } else {
        ("preparing", "Preparing response…")
    };
    JValue::obj([
        ("type", JValue::from("status")),
        ("id", JValue::from(id)),
        ("text", JValue::from(text)),
    ])
}
