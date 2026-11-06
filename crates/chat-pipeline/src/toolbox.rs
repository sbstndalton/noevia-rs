//! Which tools a turn offers (core chat.cjs `requestToolboxes`, the selection after the route is
//! settled, `automaticOnlyWrites`, `more_tools`, and toolboxes.cjs `resolveTools`, `toolCapFor`,
//! `toolTokenBudgetFor`, `estimateToolTokens`).
//!
//! The project's own list is the `toolboxes-permitted` crate (Node already reaches it through
//! `dav-parse.wasm`); a fault there carries no box, as `carried(undefined)` does.
use crate::text::{as_str, get};
use crate::tools_mode::order_with_additions;
use crate::Thrown;
use js_json::{wtf16, JValue};
use regex::Regex;
use std::collections::HashSet;
use std::sync::OnceLock;

/// toolboxes.cjs `TOOL_PREAMBLE_TOKENS`.
pub const TOOL_PREAMBLE_TOKENS: f64 = 240.0;
const TOOL_CHARS_PER_TOKEN: f64 = 3.6;
const TOOL_CAP_DEFAULT: usize = 24;
const TOOL_CAP_SMALL: usize = 12;
const TOOL_TOKEN_BUDGET_DEFAULT: f64 = 8000.0;
const TOOL_TOKEN_BUDGET_SMALL: f64 = 5000.0;
/// project-docs-default.cjs `BOX`.
pub const DOCS_BOX: &str = "project-docs";

const S: &str = r"\t\n\x0B\x0C\r \u{a0}\u{1680}\u{2000}-\u{200a}\u{2028}\u{2029}\u{202f}\u{205f}\u{3000}\u{feff}";

/// JS `Math.round`.
fn js_round(x: f64) -> f64 {
    (x + 0.5).floor()
}

/// `estimateToolTokens(tools)`: the marginal cost of these tools (the preamble is charged once by the caller).
pub fn estimate_tool_tokens(tools: &[JValue]) -> f64 {
    if tools.is_empty() {
        return 0.0;
    }
    let text = wtf16::stringify(&JValue::Arr(tools.to_vec())).unwrap_or_default();
    js_round(wtf16::len(&text) as f64 / TOOL_CHARS_PER_TOKEN)
}

/// The parameter count a model id names (`/(\d+(?:\.\d+)?)\s*[bB]\b/`), when it is at most 12.
fn small_model(model: &str) -> bool {
    static R: OnceLock<Option<Regex>> = OnceLock::new();
    let Some(re) = R
        .get_or_init(|| Regex::new(&format!(r"([0-9]+(?:\.[0-9]+)?)[{S}]*[bB](?-u:\b)")).ok())
        .as_ref()
    else {
        return false;
    };
    re.captures(model)
        .and_then(|c| c.get(1))
        .and_then(|m| m.as_str().parse::<f64>().ok())
        .is_some_and(|n| n <= 12.0)
}

/// `toolCapFor(model)`.
pub fn tool_cap_for(model: &str) -> usize {
    if small_model(model) {
        TOOL_CAP_SMALL
    } else {
        TOOL_CAP_DEFAULT
    }
}

/// `toolTokenBudgetFor(model, scope)`: `measured` is `prefill.budgetFor(...)` (falsy: not measured yet).
pub fn tool_token_budget_for(model: &str, measured: Option<f64>) -> f64 {
    match measured {
        Some(m) if m != 0.0 && !m.is_nan() => m.clamp(1500.0, 16000.0),
        _ if small_model(model) => TOOL_TOKEN_BUDGET_SMALL,
        _ => TOOL_TOKEN_BUDGET_DEFAULT,
    }
}

/// `resolveTools`' answer.
#[derive(Debug, Clone, PartialEq)]
pub struct Resolved {
    /// The tools offered, in selection order.
    pub tools: Vec<JValue>,
    /// Why each left-out tool was left out.
    pub dropped: Vec<String>,
    /// The box ids found.
    pub boxes: Vec<String>,
    pub cap: usize,
    pub budget: f64,
    pub est_tokens: f64,
    /// Offered name → its MCP server (#865), in offer order.
    pub routes: Vec<(String, String)>,
}

fn js_string(v: &JValue) -> String {
    js_json::to_js_string(v).unwrap_or_default()
}

/// `resolveTools({ toolsMode: 'manual', toolboxes: ids }, model, skip, { measureScope })` over exactly
/// these ids: each box's tools in turn, the first box winning a name, then the count cap and the token
/// budget, whichever binds first. `available` is `allToolboxes()`. A box without a tool list, or a
/// falsy entry the search reads, throws as Node's does.
pub fn resolve_tools(
    ids: &[String],
    available: &[JValue],
    model: &str,
    skip: &dyn Fn(&JValue) -> bool,
    measured: Option<f64>,
) -> Result<Resolved, Thrown> {
    let mut boxes = Vec::new();
    let mut candidates: Vec<&JValue> = Vec::new();
    let mut seen: Vec<&JValue> = Vec::new();
    let mut server_of: Vec<(&JValue, String)> = Vec::new();
    for id in ids {
        let mut found = None;
        for b in available {
            if matches!(b, JValue::Null | JValue::Undefined) {
                return Err(Thrown);
            }
            if as_str(get(b, "id")) == Some(id.as_str()) {
                found = Some(b);
                break;
            }
        }
        let Some(b) = found else {
            continue;
        };
        boxes.push(id.clone());
        let JValue::Arr(tools) = get(b, "tools") else {
            return Err(Thrown);
        };
        for tool in tools {
            let f = get(tool, "function");
            let name = if tool.truthy() && f.truthy() {
                get(f, "name")
            } else {
                &JValue::Undefined
            };
            if !name.truthy() || seen.iter().any(|s| same(s, name)) || skip(name) {
                continue;
            }
            seen.push(name);
            candidates.push(tool);
            if let Some(server) = as_str(get(b, "server")).filter(|s| !s.is_empty()) {
                match server_of.iter_mut().find(|(n, _)| same(n, name)) {
                    Some(slot) => slot.1 = server.to_owned(),
                    None => server_of.push((name, server.to_owned())),
                }
            }
        }
    }
    let cap = tool_cap_for(model);
    let budget = tool_token_budget_for(model, measured);
    let mut tools = Vec::new();
    let mut dropped = Vec::new();
    let mut spent = if candidates.is_empty() {
        0.0
    } else {
        TOOL_PREAMBLE_TOKENS
    };
    for tool in candidates {
        let cost = estimate_tool_tokens(std::slice::from_ref(tool));
        let name = js_string(get(get(tool, "function"), "name"));
        if tools.len() >= cap {
            dropped.push(format!("{name} (over {cap}-tool cap)"));
            continue;
        }
        if spent + cost > budget {
            dropped.push(format!(
                "{name} (~{} tok, over {} budget)",
                js_json::number_to_string(cost),
                js_json::number_to_string(budget)
            ));
            continue;
        }
        tools.push(tool.clone());
        spent += cost;
    }
    let mut routes: Vec<(String, String)> = Vec::new();
    for tool in &tools {
        let name = get(get(tool, "function"), "name");
        if let Some((_, server)) = server_of.iter().find(|(n, _)| same(n, name)) {
            let key = js_string(name);
            match routes.iter_mut().find(|(k, _)| *k == key) {
                Some(slot) => slot.1 = server.clone(),
                None => routes.push((key, server.clone())),
            }
        }
    }
    Ok(Resolved {
        tools,
        dropped,
        boxes,
        cap,
        budget,
        est_tokens: spent,
        routes,
    })
}

/// SameValueZero for the names a `Set` or `Map` holds.
fn same(a: &JValue, b: &JValue) -> bool {
    match (a, b) {
        (JValue::Num(x), JValue::Num(y)) => x == y || (x.is_nan() && y.is_nan()),
        (JValue::Arr(_) | JValue::Obj(_), _) | (_, JValue::Arr(_) | JValue::Obj(_)) => {
            std::ptr::eq(a, b)
        }
        _ => a == b,
    }
}

/// What `requestToolboxes` reads of the request and the account.
pub struct Request<'a> {
    /// The project (`null` for a free chat).
    pub project: &'a JValue,
    /// `DEFAULT_TOOLBOXES`.
    pub defaults: &'a [String],
    /// `CONNECTOR_BOXES`.
    pub connectors: &'a HashSet<String>,
    /// `connectedBoxes(chatUser)`.
    pub connected: &'a [String],
    /// `allToolboxes()` ids, in order.
    pub offered: &'a [String],
    /// `body.turnToolboxes`.
    pub turn_toolboxes: &'a JValue,
    /// `authService.diaryEnabled(chatUser.id)` for a signed-in user (false otherwise).
    pub diary_enabled: bool,
    /// `oauthServerIds()`.
    pub oauth: &'a HashSet<String>,
    /// `accountReady(chatUser?.id, id)`.
    pub account_ready: &'a dyn Fn(&str) -> bool,
}

fn units(s: &str) -> Vec<u16> {
    let mut out = Vec::with_capacity(s.len());
    for c in s.chars() {
        match wtf16::lone_unit(c) {
            Some(u) => out.push(u),
            None => out.extend(c.encode_utf16(&mut [0; 2]).iter()),
        }
    }
    out
}

fn ids_projection(v: &JValue) -> Option<Vec<toolboxes_permitted::Id>> {
    match v {
        JValue::Arr(a) => Some(a.iter().map(|x| as_str(x).map(units)).collect()),
        _ => None,
    }
}

/// toolboxes-permitted.cjs `selectedToolboxIds`: the project's list without connectors, plus the
/// connected ones; a fault (a projection that cannot be built, an exhausted budget) carries none.
pub fn selected_toolbox_ids(r: &Request<'_>) -> Vec<String> {
    let p = r.project;
    let project = if p.truthy() {
        let toolboxes = match get(p, "toolboxes") {
            JValue::Arr(_) => match ids_projection(get(p, "toolboxes")) {
                Some(t) => Some(t),
                None => return Vec::new(),
            },
            _ => None,
        };
        Some(toolboxes_permitted::Project {
            auto: as_str(get(p, "toolsMode")) == Some("auto"),
            docs_defaulted: get(p, "docsToolboxDefaulted") == &JValue::Bool(true),
            toolboxes,
        })
    } else {
        None
    };
    let defaults: Vec<toolboxes_permitted::Id> =
        r.defaults.iter().map(|d| Some(units(d))).collect();
    let connector: HashSet<Vec<u16>> = r.connectors.iter().map(|c| units(c)).collect();
    let connected: Vec<toolboxes_permitted::Id> =
        r.connected.iter().map(|c| Some(units(c))).collect();
    let mut work = toolboxes_permitted::Work::new(1 << 40);
    match toolboxes_permitted::selected_ids(
        project.as_ref(),
        &defaults,
        &units(DOCS_BOX),
        &connector,
        &connected,
        &mut work,
    ) {
        Ok(ids) => ids
            .into_iter()
            .filter_map(|id| id.map(|u| crate::text::from_units(&u)))
            .collect(),
        Err(_) => Vec::new(),
    }
}

/// chat.cjs `automaticHead`: the default boxes and the Project documents box.
pub fn automatic_head(defaults: &[String]) -> Vec<String> {
    let mut out = defaults.to_vec();
    out.push(DOCS_BOX.to_owned());
    out
}

/// chat.cjs `requestToolboxes(automaticAdded)`: the boxes this request carries, Automatic's additions
/// after the head, the turn's own picks, then the Diary add-on and sign-in filters.
pub fn request_toolboxes(r: &Request<'_>, automatic_added: &[String]) -> Vec<String> {
    let base = selected_toolbox_ids(r);
    let added: Vec<String> = automatic_added
        .iter()
        .filter(|id| !base.contains(id))
        .cloned()
        .collect();
    let mut selected = if added.is_empty() {
        base
    } else {
        let mut all = base;
        all.extend(added.iter().cloned());
        order_with_additions(&all, &added, &automatic_head(r.defaults))
    };
    if let JValue::Arr(turn) = r.turn_toolboxes {
        for id in turn.iter().filter_map(as_str) {
            if r.offered.iter().any(|o| o == id)
                && !r.connectors.contains(id)
                && !selected.iter().any(|s| s == id)
            {
                selected.push(id.to_owned());
            }
        }
    }
    if !r.diary_enabled {
        if let Some(k) = selected.iter().position(|s| s == "diary") {
            selected.remove(k);
        }
    }
    // Node asks back to front; `accountReady` is a pure read, so the order does not show.
    selected.retain(|id| !r.oauth.contains(id) || (r.account_ready)(id));
    selected
}

/// chat.cjs `automaticOnlyWrites` (#1550): writes reachable only through a box Automatic added.
pub fn automatic_only_writes(
    boxes: &[JValue],
    selected: &[String],
    added: &[String],
    is_write_tool: &dyn Fn(&str) -> bool,
) -> Result<HashSet<String>, Thrown> {
    let mut out = HashSet::new();
    if added.is_empty() {
        return Ok(out);
    }
    let names_of = |ids: &dyn Fn(&str) -> bool| -> Result<Vec<String>, Thrown> {
        let mut names = Vec::new();
        for b in boxes {
            if matches!(b, JValue::Null | JValue::Undefined) {
                return Err(Thrown);
            }
            if !as_str(get(b, "id")).is_some_and(ids) {
                continue;
            }
            match get(b, "tools") {
                JValue::Arr(tools) => names.extend(tools.iter().filter_map(|t| {
                    let n = get(get(t, "function"), "name");
                    n.truthy().then(|| js_string(n))
                })),
                v if v.truthy() => return Err(Thrown),
                _ => {}
            }
        }
        Ok(names)
    };
    let elsewhere: HashSet<String> =
        names_of(&|id| selected.iter().any(|s| s == id) && !added.iter().any(|a| a == id))?
            .into_iter()
            .collect();
    for name in names_of(&|id| added.iter().any(|a| a == id))? {
        if !elsewhere.contains(&name) && is_write_tool(&name) {
            out.insert(name);
        }
    }
    Ok(out)
}

/// The tool router's answer (`chatToolRouter.select(ids, message)`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Routing {
    pub ids: Vec<String>,
    pub routed: bool,
    pub narrowed: bool,
}

/// chat.cjs after `chatToolRouter.select`: routing never drops a box this message asked for, and the
/// default boxes stay ahead of the additions.
pub fn keep_additions(
    routing: Routing,
    added: &[String],
    selected: &[String],
    head: &[String],
) -> Routing {
    if !routing.routed || added.is_empty() {
        return routing;
    }
    let mut all = routing.ids.clone();
    all.extend(added.iter().filter(|id| !routing.ids.contains(id)).cloned());
    let ids = order_with_additions(&all, added, head);
    let narrowed = ids.len() < selected.len();
    Routing {
        ids,
        narrowed,
        ..routing
    }
}

/// The `more_tools` tool a narrowed turn offers (chat.cjs: all of the project's other tools next step).
pub fn more_tools_tool() -> JValue {
    let s = |v: &str| JValue::Str(v.to_owned());
    JValue::obj([
        ("type", s("function")),
        (
            "function",
            JValue::obj([
                ("name", s("more_tools")),
                ("description", s("Call this only if none of the offered tools can do the user's task. It makes all of this project's other tools available for your next step.")),
                ("parameters", JValue::obj([("type", s("object")), ("properties", JValue::obj(Vec::<(&str, JValue)>::new()))])),
            ]),
        ),
    ])
}

/// The toolbox router (chat-tool-routing.cjs `select(ids, message)`), awaited before the first model call.
#[allow(async_fn_in_trait)]
pub trait Router {
    /// Ranks and may narrow `ids`; never adds one.
    async fn select(&self, ids: &[String], message: &JValue) -> Routing;
}

/// What the selection reads besides [`Request`].
pub struct Turn<'a> {
    pub request: Request<'a>,
    /// `allToolboxes()`.
    pub boxes: &'a [JValue],
    pub message: &'a JValue,
    /// `mappedHistory`.
    pub history: &'a JValue,
    /// `toolsModeOf(project) === 'auto'` and not the Diary extras project.
    pub automatic_tools: bool,
    pub compact_only: bool,
    /// `!offBoxTurn(...)`: the answering model is this machine's.
    pub automatic_local: bool,
    /// `egress.stripPrivateToolboxes`: an external provider under the strict rule.
    pub external_strict: bool,
    /// `cloudUngranted()` (#1646).
    pub cloud_ungranted: bool,
    /// `project?.files?.length` is truthy.
    pub project_has_files: bool,
    pub is_write_tool: &'a dyn Fn(&str) -> bool,
    /// `automaticToolsReady(chatUser)(box) === true`.
    pub ready: &'a dyn Fn(&JValue) -> bool,
    /// `toolPolicy.mode(user, name, isWriteTool(name)) === 'block'`.
    pub policy_blocked: &'a dyn Fn(&JValue) -> bool,
}

/// The selection a turn goes on with.
#[derive(Debug, Clone, PartialEq)]
pub struct Selected {
    pub selected: Vec<String>,
    pub added: Vec<String>,
    pub routing: Routing,
    pub automatic_only_writes: HashSet<String>,
    /// #1646: private-data boxes were taken off (the prompt then drops the reader hints).
    pub private_stripped: bool,
}

impl Selected {
    /// The ids `resolveFor(model)` resolves.
    pub fn ids(&self) -> &[String] {
        if self.routing.routed {
            &self.routing.ids
        } else {
            &self.selected
        }
    }
}

/// chat.cjs `blocked(name)`: the account's block, and on an ungranted cloud route the project reader.
pub fn blocked(t: &Turn<'_>, name: &JValue) -> bool {
    (t.policy_blocked)(name)
        || (t.cloud_ungranted && as_str(name).is_some_and(crate::tools_mode::is_private_core_tool))
}

/// chat.cjs from `automaticAdded` to `automaticOnlyWrites`: Automatic's additions, the carried selection,
/// the external-provider and #1646 strips, and the router's ranking.
pub async fn select(t: &Turn<'_>, router: &impl Router) -> Result<Selected, Thrown> {
    let boxes = JValue::Arr(t.boxes.to_vec());
    let mut added: Vec<String> = Vec::new();
    if t.automatic_tools && !t.compact_only {
        let carried = crate::tools_mode::carried_public_boxes(t.history, &boxes)?;
        let base: Vec<JValue> = request_toolboxes(&t.request, &[])
            .into_iter()
            .map(JValue::Str)
            .collect();
        let e = crate::tools_mode::Eligibility {
            connector_boxes: t.request.connectors,
            is_write_tool: t.is_write_tool,
            ready: t.ready,
            private_data: t.automatic_local,
        };
        let carried = JValue::Arr(carried.into_iter().map(JValue::Str).collect());
        added = crate::tools_mode::automatic_additions(
            t.message,
            &base,
            &boxes,
            &e,
            crate::tools_mode::MAX_ADDED,
            &carried,
        )?;
    }
    let mut selected = request_toolboxes(&t.request, &added);
    if t.external_strict {
        selected.retain(|id| !provider_egress::PRIVATE_TOOLBOXES.contains(&id.as_str()));
    }
    let mut private_stripped = false;
    if t.cloud_ungranted {
        let before = selected.len();
        selected.retain(|id| !crate::tools_mode::is_private_data_box(&JValue::Str(id.clone())));
        private_stripped = selected.len() != before;
        if t.project_has_files && selected.iter().any(|id| id == "core") {
            private_stripped = true;
        }
    }
    added.retain(|id| selected.contains(id));
    let routing = router.select(&selected, t.message).await;
    let routing = keep_additions(
        routing,
        &added,
        &selected,
        &automatic_head(t.request.defaults),
    );
    let automatic_only_writes = automatic_only_writes(t.boxes, &selected, &added, t.is_write_tool)?;
    Ok(Selected {
        selected,
        added,
        routing,
        automatic_only_writes,
        private_stripped,
    })
}

/// chat.cjs `resolveFor(model)`.
pub fn resolve_for(
    t: &Turn<'_>,
    s: &Selected,
    model: &str,
    measured: Option<f64>,
) -> Result<Resolved, Thrown> {
    resolve_tools(s.ids(), t.boxes, model, &|n| blocked(t, n), measured)
}

/// The tools the first round offers: the resolved list, plus `more_tools` when routing narrowed it.
pub fn active_tools(s: &Selected, r: &Resolved) -> Vec<JValue> {
    let mut out = r.tools.clone();
    if s.routing.narrowed {
        out.push(more_tools_tool());
    }
    out
}

/// The `tools_scope` event: the boxes the reply was given when routing narrowed them or Automatic added some.
pub fn tools_scope_event(t: &Turn<'_>, s: &Selected) -> JValue {
    let scope: Option<&[String]> = if s.routing.narrowed {
        Some(&s.routing.ids)
    } else if !s.added.is_empty() {
        Some(s.ids())
    } else {
        None
    };
    let find = |id: &str| t.boxes.iter().find(|b| as_str(get(b, "id")) == Some(id));
    let label = |id: &str| match find(id).map(|b| get(b, "label")) {
        Some(l) if l.truthy() => l.clone(),
        _ => JValue::Str(id.to_owned()),
    };
    let s_ = |v: &str| JValue::Str(v.to_owned());
    let mut ev = vec![("type", s_("tools_scope"))];
    match scope {
        Some(ids) => {
            let text = ids
                .iter()
                .map(|id| js_string(&label(id)))
                .collect::<Vec<_>>()
                .join(", ");
            ev.push(("text", JValue::Str(text)));
            let boxes = ids
                .iter()
                .map(|id| {
                    let b = find(id);
                    let in_app = b.is_some_and(|b| {
                        b.truthy()
                            && (as_str(get(b, "source")) == Some("builtin")
                                || get(b, "inApp") == &JValue::Bool(true))
                    });
                    JValue::obj([
                        ("id", s_(id)),
                        ("label", label(id)),
                        ("inApp", JValue::Bool(in_app)),
                    ])
                })
                .collect();
            ev.push(("boxes", JValue::Arr(boxes)));
        }
        None => ev.push(("text", s_(""))),
    }
    JValue::obj(ev)
}

/// The `more_tools` call (#865 routes): the project's whole selection resolved, its reply text, and its routes.
pub fn widen(
    t: &Turn<'_>,
    s: &Selected,
    model: &str,
    measured: Option<f64>,
) -> Result<(Resolved, String), Thrown> {
    let full = resolve_tools(&s.selected, t.boxes, model, &|n| blocked(t, n), measured)?;
    let names = full
        .tools
        .iter()
        .map(|tool| js_string(get(get(tool, "function"), "name")))
        .collect::<Vec<_>>()
        .join(", ");
    let reply = format!("More tools are now available: {names}. Continue with the task.");
    Ok((full, reply))
}

/// The `more_tools` reply once the list is already widened.
pub const ALREADY_WIDE: &str = "All of this project's tools are already available.";
