//! The toolbox parts of noevia-core's `server/toolboxes.cjs` a turn's tool list is built from
//! (slice P6a): the core box, `estimateToolTokens`, `toolCapFor`, `toolTokenBudgetFor`,
//! `resolveTools` (with the coworker page tool of `specialist-page-save.cjs`), `readOnlyToolNames`
//! and `isWriteTool`. The executor (`executeToolCall`) and `sanitizeToolboxes` are not here.
//!
//! Pure: the boxes are `allToolboxes()` as the caller read them (the built-in boxes the
//! deployment offers, the connector boxes and the MCP boxes discovery found, in that order), the
//! prefill measurement and the MCP server's own read-only flags are the caller's answers.
//!
//! A box is typed here: its `id`, `label`, `source` and `server` are text, `inApp` a flag and
//! `reads` a list of names, as every box core builds has them. Its tools stay JS values, since the
//! frame carries them byte for byte and their size is measured on `JSON.stringify`. A tool whose
//! `function.name` is truthy but not a string is refused ([`NameNotText`]): Node would offer it
//! under a non-text name the executor cannot be handed.

use std::collections::HashSet;

use js_json::{number_to_string, wtf16, JValue};

/// `TOOL_PREAMBLE_TOKENS`: paid once per request that sends any tool.
pub const TOOL_PREAMBLE_TOKENS: f64 = 240.0;
/// `TOOL_CHARS_PER_TOKEN`.
pub const TOOL_CHARS_PER_TOKEN: f64 = 3.6;
/// `TOOL_CAP_DEFAULT`.
pub const TOOL_CAP_DEFAULT: usize = 24;
/// `TOOL_CAP_SMALL`: a model of at most 12B parameters by its name.
pub const TOOL_CAP_SMALL: usize = 12;
/// `TOOL_TOKEN_BUDGET_DEFAULT`.
pub const TOOL_TOKEN_BUDGET_DEFAULT: f64 = 8000.0;
/// `TOOL_TOKEN_BUDGET_SMALL`.
pub const TOOL_TOKEN_BUDGET_SMALL: f64 = 5000.0;
/// `TOOL_PREFILL_TARGET_MS`: what the caller asks `prefill.budgetFor` for.
pub const TOOL_PREFILL_TARGET_MS: u32 = 14000;
/// `DEFAULT_TOOLBOXES`.
pub const DEFAULT_TOOLBOXES: [&str; 1] = ["core"];
/// `CONNECTOR_BOXES`: chosen per account (a connected Google Drive), never by a project.
pub const CONNECTOR_BOXES: [&str; 1] = ["gdrive"];
/// `project-docs-default.cjs` `BOX`.
pub const PROJECT_DOCS_BOX: &str = "project-docs";
/// `specialist-page-save.cjs` `NAME`: offered only in a coworker's conversation, and a write
/// asked about every time (`neverStanding`).
pub const PAGE_SAVE: &str = "space_page_save";

/// specialist-page-save.cjs `neverStanding(name)` (`NEVER_STANDING`): asked about on every call,
/// never under a chat-wide grant or an account policy.
pub fn never_standing(name: &str) -> bool {
    name == PAGE_SAVE
}
/// `specialist-page-save.cjs` `PROJECT_PREFIX`.
pub const SPECIALIST_PREFIX: &str = "specialist-";

/// `CORE_TOOLS` as `JSON.stringify` writes them.
const CORE_TOOLS_JSON: &str = r#"[{"type":"function","function":{"name":"get_current_time","description":"Get the current date and time on the server, optionally in a specific IANA timezone (e.g. Europe/Berlin). Use whenever freshness, \"today\", or a timezone matters.","parameters":{"type":"object","properties":{"timezone":{"type":"string","description":"Optional IANA timezone name"}},"required":[]}}},{"type":"function","function":{"name":"read_project_file","description":"Read an attached source or enabled instruction skill by its name as listed. Skills support offset pagination. For PDFs use startPage/endPage (up to 5 pages) and offset to read beyond summaries; results include page and version references.","parameters":{"type":"object","properties":{"name":{"type":"string","description":"File name as listed (e.g. Folder/Text/notes.md), or just notes.md if unique"},"startPage":{"type":"integer","minimum":1},"endPage":{"type":"integer","minimum":1},"offset":{"type":"integer","minimum":0}},"required":["name"]}}}]"#;
/// `specialist-page-save.cjs` `TOOL`.
const PAGE_SAVE_JSON: &str = r#"{"type":"function","function":{"name":"space_page_save","description":"Save a finished draft as a page in one of your Spaces. The person reviews the full draft and approves before anything is saved. Use it when they ask you to write something down or keep it. Saving the same draft again returns the same page.","parameters":{"type":"object","properties":{"spaceId":{"type":"string","description":"Optional: the id of one of your Spaces. Leave it out to use your default Space."},"title":{"type":"string","description":"The page title, one line."},"content":{"type":"string","description":"The whole page, as Markdown."}},"required":["title","content"]}}}"#;

/// One toolbox of `allToolboxes()`.
#[derive(Debug, Clone, PartialEq)]
pub struct Toolbox {
    pub id: String,
    /// `label` (empty: `b.label || id` reads the id).
    pub label: String,
    /// `source` (`'builtin'` for noevia's own boxes; MCP boxes say `'mcp'`).
    pub source: String,
    /// `inApp === true`.
    pub in_app: bool,
    /// The MCP server the box's tools are called on (`typeof server === 'string' && server`).
    pub server: Option<String>,
    /// `tools`, as JS values.
    pub tools: Vec<JValue>,
    /// `reads`: the names this box declares read-only.
    pub reads: Vec<String>,
}

impl Toolbox {
    /// `toolbox-flags.cjs` `isInAppBox(box)`.
    pub fn is_in_app(&self) -> bool {
        self.source == "builtin" || self.in_app
    }

    /// Whether the box lists a tool of this name (`(b.tools || []).some((t) => t?.function?.name === name)`).
    pub fn offers(&self, name: &str) -> bool {
        self.tools
            .iter()
            .any(|t| name_value(t).as_str() == Some(name))
    }

    /// The names of its tools that are text (`.map((t) => t.function.name).filter((n) => typeof n === 'string')`).
    pub fn text_names(&self) -> impl Iterator<Item = &str> {
        self.tools.iter().filter_map(|t| name_value(t).as_str())
    }
}

/// The core box (`TOOLBOXES[0]`); `None` only if the constant could not be read.
pub fn core_box() -> Option<Toolbox> {
    let JValue::Arr(tools) = wtf16::parse(CORE_TOOLS_JSON).ok()? else {
        return None;
    };
    Some(Toolbox {
        id: "core".into(),
        label: "Core".into(),
        source: "builtin".into(),
        in_app: false,
        server: None,
        tools,
        reads: vec!["get_current_time".into(), "read_project_file".into()],
    })
}

/// A tool with a truthy `function.name` that is not text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NameNotText;

/// `tool && tool.function && tool.function.name`.
fn name_value(tool: &JValue) -> &JValue {
    tool.get("function").get("name")
}

/// The tool's name when it has one (`None` for a falsy name); [`NameNotText`] for another truthy value.
pub fn tool_name(tool: &JValue) -> Result<Option<&str>, NameNotText> {
    match name_value(tool) {
        JValue::Str(s) if !s.is_empty() => Ok(Some(s)),
        v if !v.truthy() => Ok(None),
        _ => Err(NameNotText),
    }
}

/// The names of a box's tools, falsy ones left out (`namesOf` in chat.cjs).
pub fn box_names(b: &Toolbox) -> Result<Vec<&str>, NameNotText> {
    let mut out = Vec::new();
    for t in &b.tools {
        if let Some(n) = tool_name(t)? {
            out.push(n);
        }
    }
    Ok(out)
}

/// `estimateToolTokens(tools)`: `Math.round(JSON.stringify(tools).length / 3.6)`, 0 for no tools.
pub fn estimate_tool_tokens(tools: &[JValue]) -> f64 {
    if tools.is_empty() {
        return 0.0;
    }
    let text = wtf16::stringify(&JValue::Arr(tools.to_vec())).unwrap_or_default();
    js_round(wtf16::len(&text) as f64 / TOOL_CHARS_PER_TOKEN)
}

/// `Math.round(x)` for a non-negative `x` (halves round up).
fn js_round(x: f64) -> f64 {
    let f = x.floor();
    if x - f >= 0.5 {
        f + 1.0
    } else {
        f
    }
}

/// `/(\d+(?:\.\d+)?)\s*[bB]\b/.exec(String(model || ''))`: the first parameter count in the name.
fn parameter_count(model: &str) -> Option<f64> {
    let chars: Vec<char> = model.chars().collect();
    let digit = |i: usize| chars.get(i).is_some_and(char::is_ascii_digit);
    let word = |i: usize| {
        chars
            .get(i)
            .is_some_and(|c| c.is_ascii_alphanumeric() || *c == '_')
    };
    for start in 0..chars.len() {
        // A start inside a run of digits continues exactly as the run's first digit did.
        if !digit(start) || (start > 0 && digit(start - 1)) {
            continue;
        }
        let mut end = start;
        while digit(end) {
            end += 1;
        }
        // Greedy digits, then the optional fraction, then back off: a shorter run never reaches a
        // `b` (the next character would be a digit), so only these two ends can match.
        let mut ends = vec![end];
        if chars.get(end) == Some(&'.') && digit(end + 1) {
            let mut f = end + 1;
            while digit(f) {
                f += 1;
            }
            ends.insert(0, f);
        }
        for e in ends {
            let mut k = e;
            while chars.get(k).is_some_and(|&c| crate::js::is_js_space(c)) {
                k += 1;
            }
            if matches!(chars.get(k), Some('b' | 'B')) && !word(k + 1) {
                let text: String = chars.get(start..e)?.iter().collect();
                return text.parse().ok();
            }
        }
    }
    None
}

/// `toolCapFor(model)`.
pub fn tool_cap_for(model: &str) -> usize {
    match parameter_count(model) {
        Some(n) if n <= 12.0 => TOOL_CAP_SMALL,
        _ => TOOL_CAP_DEFAULT,
    }
}

/// `toolTokenBudgetFor(model, scope)`. `measured` is `prefill.budgetFor(model, 14000[, scope])`
/// when it is a truthy number (the caller's answer; `None` otherwise).
pub fn tool_token_budget_for(model: &str, measured: Option<f64>) -> f64 {
    if let Some(m) = measured.filter(|m| *m != 0.0 && !m.is_nan()) {
        return m.clamp(1500.0, 16000.0);
    }
    match parameter_count(model) {
        Some(n) if n <= 12.0 => TOOL_TOKEN_BUDGET_SMALL,
        _ => TOOL_TOKEN_BUDGET_DEFAULT,
    }
}

/// `resolveTools`' answer.
#[derive(Debug, Clone, PartialEq)]
pub struct Resolved {
    pub tools: Vec<JValue>,
    /// Each dropped name with its reason, as Node logs it.
    pub dropped: Vec<String>,
    /// The ids of the boxes found, in selection order.
    pub boxes: Vec<String>,
    pub cap: usize,
    pub budget: f64,
    /// `estTokens`: the preamble (with any tool) and the kept tools' cost.
    pub est_tokens: f64,
    /// `routes`: offered name -> the MCP server of the box that won it, in tool order.
    pub routes: Vec<(String, String)>,
}

/// What `resolveTools` reads besides the selection.
pub struct Resolve<'a> {
    /// `allToolboxes()`.
    pub boxes: &'a [Toolbox],
    pub model: &'a str,
    /// `skip(name)`: the account blocked it (chat.cjs's `blocked`).
    pub skip: &'a dyn Fn(&str) -> bool,
    /// `specialistTools.appliesTo(project)`: the turn's project is a coworker's.
    pub specialist: bool,
    /// See [`tool_token_budget_for`].
    pub measured: Option<f64>,
}

/// `resolveTools({ ...project, toolsMode: 'manual', toolboxes: ids }, model, skip, ...)`, the call
/// chat.cjs makes: `projectToolboxIds` of a Manual project is its list as it is, so `ids` is what
/// is resolved.
pub fn resolve_tools(ids: &[String], r: &Resolve<'_>) -> Result<Resolved, NameNotText> {
    let mut boxes = Vec::new();
    let mut candidates: Vec<JValue> = Vec::new();
    let mut seen: HashSet<&str> = HashSet::new();
    let mut server_of: Vec<(&str, &str)> = Vec::new();
    for id in ids {
        let Some(b) = r.boxes.iter().find(|b| &b.id == id) else {
            continue;
        };
        boxes.push(b.id.clone());
        for tool in &b.tools {
            let Some(name) = tool_name(tool)? else {
                continue;
            };
            if seen.contains(name) || (r.skip)(name) {
                continue; // first box wins a name clash
            }
            seen.insert(name);
            candidates.push(tool.clone());
            if let Some(server) = b.server.as_deref().filter(|s| !s.is_empty()) {
                server_of.push((name, server));
            }
        }
    }
    if r.specialist && !seen.contains(PAGE_SAVE) && !(r.skip)(PAGE_SAVE) {
        if let Ok(tool) = wtf16::parse(PAGE_SAVE_JSON) {
            candidates.insert(0, tool);
        }
    }
    let cap = tool_cap_for(r.model);
    let budget = tool_token_budget_for(r.model, r.measured);
    let mut tools: Vec<JValue> = Vec::new();
    let mut dropped = Vec::new();
    let mut spent = if candidates.is_empty() {
        0.0
    } else {
        TOOL_PREAMBLE_TOKENS
    };
    for tool in candidates {
        let cost = estimate_tool_tokens(std::slice::from_ref(&tool));
        let name = name_value(&tool).as_str().unwrap_or_default().to_string();
        if tools.len() >= cap {
            dropped.push(format!("{name} (over {cap}-tool cap)"));
        } else if spent + cost > budget {
            let (c, b) = (number_to_string(cost), number_to_string(budget));
            dropped.push(format!("{name} (~{c} tok, over {b} budget)"));
        } else {
            tools.push(tool);
            spent += cost;
        }
    }
    let mut routes = Vec::new();
    for tool in &tools {
        let name = name_value(tool).as_str().unwrap_or_default();
        if let Some((_, server)) = server_of.iter().find(|(n, _)| *n == name) {
            routes.push((name.to_string(), (*server).to_string()));
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

/// `readOnlyToolNames()` and `isWriteTool(name)` over one `allToolboxes()`.
pub struct Writes<'a> {
    read_only: HashSet<&'a str>,
    /// `mcpTools().get(name)?.readOnly === false`: the server itself says the tool writes.
    server_says_write: &'a dyn Fn(&str) -> bool,
}

impl<'a> Writes<'a> {
    pub fn new(boxes: &'a [Toolbox], server_says_write: &'a dyn Fn(&str) -> bool) -> Writes<'a> {
        let mut writes: HashSet<&str> = HashSet::new();
        for b in boxes {
            for name in b.text_names().filter(|n| !n.is_empty()) {
                if !b.reads.iter().any(|r| r == name) {
                    writes.insert(name);
                }
            }
        }
        // If two boxes reuse a name and either declares it a write, the approval gate stays.
        let read_only = boxes
            .iter()
            .flat_map(|b| b.reads.iter().map(String::as_str))
            .filter(|n| !writes.contains(n))
            .collect();
        Writes {
            read_only,
            server_says_write,
        }
    }

    /// `readOnlyToolNames().has(name)`.
    pub fn read_only(&self, name: &str) -> bool {
        self.read_only.contains(name)
    }

    /// chat.cjs's `isWriteTool`: the coworker page save always, else toolboxes.cjs `isWriteTool`
    /// (unknown means write; a read-only name the MCP server says writes is a write).
    pub fn is_write(&self, name: &str) -> bool {
        name == PAGE_SAVE || !self.read_only(name) || (self.server_says_write)(name)
    }
}
