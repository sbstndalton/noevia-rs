//! How a project or free chat picks its tools (Automatic or Manual) and what Automatic adds for one
//! message (core `server/tools-mode.cjs`, #1522, #1550, #1646, #1692, #1702, #1854).
//!
//! The regular expressions keep JavaScript's non-unicode `/i` semantics: `\b` and `\w` are ASCII,
//! `\s` is JavaScript's white space set, and literal `k`/`s` fold ASCII only ([`crate::text::js_fold`]).
//! `EXPLICIT_WEB_RE`'s `google(?!\s+(?:drive|…))` look-ahead is left out: the branch then needs
//! `\s+(?:it|this|that|for)\b`, whose first word can never be one the look-ahead excludes, so the
//! two always agree.
use crate::replay::{is_public_tool_name, PRIVATE_CORE_TOOLS};
use crate::text::{as_str, get, js_fold, trim};
use crate::Thrown;
use js_json::{wtf16, JValue};
use regex::Regex;
use std::collections::HashSet;
use std::future::Future;
use std::sync::OnceLock;

/// tools-mode.cjs `MAX_ADDED`.
pub const MAX_ADDED: usize = 3;
/// tools-mode.cjs `CARRY_USER_TURNS`.
pub const CARRY_USER_TURNS: usize = 3;
/// tools-mode.cjs `FALLBACK_ROLES`.
pub const FALLBACK_ROLES: [&str; 3] = ["smart", "code", "fast"];
/// tools-mode.cjs `PUBLIC_BOXES`.
pub const PUBLIC_BOXES: [&str; 2] = ["web-search", "offline-wikipedia"];
/// tools-mode.cjs `CLOUD_SAFE_BOXES`.
pub const CLOUD_SAFE_BOXES: [&str; 4] = ["core", "web-search", "web-crawl", "offline-wikipedia"];

const S: &str = r"\t\n\x0B\x0C\r \u{a0}\u{1680}\u{2000}-\u{200a}\u{2028}\u{2029}\u{202f}\u{205f}\u{3000}\u{feff}";
const B: &str = r"(?-u:\b)";
const W: &str = r"(?-u:[0-9A-Za-z_])";
const FILES_WORDS: &str = "files?|folders?|director(?:y|ies)|documents?|docs";
const NEXTCLOUD_APPS: [(&str, &str); 5] = [
    ("nextcloud-notes", "notes?"),
    ("nextcloud-calendar", "calendars?|appointments?|meetings?|events?|agenda|schedule"),
    ("nextcloud-tasks", "tasks?|to-?dos?|reminders?"),
    ("nextcloud-contacts", r"contacts?|address\s*books?"),
    ("nextcloud-mail", "e-?mails?|mail|inbox"),
];

struct Res {
    url: Regex,
    explicit_web: Regex,
    web: Regex,
    nextcloud: Regex,
    wikipedia: Regex,
    /// Per app: (box, bare, possessive).
    apps: Vec<(&'static str, Regex, Regex)>,
    files_bare: Regex,
    files_possessive: Regex,
}

/// Translates the JS source's `\s` and `\b` to their JavaScript meaning, then folds `k`/`s`.
fn js(src: &str) -> Option<Regex> {
    let s = format!("[{S}]");
    let out = src.replace(r"\s", &s).replace(r"\b", B).replace(r"\w", W);
    Regex::new(&js_fold(&format!("(?i){out}"))).ok()
}

fn bare(words: &str) -> Option<Regex> {
    js(&format!(r"\b(?:{words})\b"))
}

fn possessive(words: &str) -> Option<Regex> {
    // `[\w-]` as one ASCII class.
    js(&format!(r"\b(?:my|our)\s+(?:(?-u:[0-9A-Za-z_-])+\s+){{0,2}}(?:{words})\b"))
}

fn res() -> Option<&'static Res> {
    static R: OnceLock<Option<Res>> = OnceLock::new();
    R.get_or_init(|| {
        let web = [
            r"\b(?:latest|newest)\b",
            r"\b(?:most\s+)?recent(?:ly)?\s+(?:news|releases?|versions?|updates?|events|changes)\b",
            r"\bcurrent(?:ly)?\s+(?:stable\s+)?(?:versions?|releases?|prices?|rates?|exchange\s+rates?|news|events|weather|status|president|prime\s+minister|ceo|champions?|lts)\b",
            r"\b(?:today|tonight|this\s+(?:week|month|year))'?s\s+(?:top\s+)?(?:news|headlines|weather|forecast|prices?|rates?|exchange\s+rates?|scores?|results|releases?)\b",
            r"\b(?:news|headlines|weather|forecast|exchange\s+rates?|stock\s+prices?|price\s+of)\b",
            r"\b(?:cite\s+(?:your\s+|the\s+)?sources?|with\s+sources?|source\s+urls?)\b",
            r"\blook\s*up\b",
        ]
        .join("|");
        let mut apps = Vec::new();
        for (b, w) in NEXTCLOUD_APPS {
            apps.push((b, bare(w)?, possessive(w)?));
        }
        Some(Res {
            // `[^\s<>"')\]]`: the class keeps its own white space set.
            url: js(&format!(r#"\bhttps?://[^{S}<>"')\]]+"#))?,
            explicit_web: js(r"\b(?:search(?:\s+(?:the|on\s+the))?\s+(?:web|internet|online)|web\s*search|(?:on|from)\s+the\s+(?:web|internet)|(?:search|look(?:\s+it)?\s*up|find\s+it)\s+online|google\s+(?:it|this|that|for)\b|browse\s+(?:the\s+)?(?:web|internet))\b")?,
            web: js(&web)?,
            nextcloud: js(r"\bnext\s?cloud\b")?,
            wikipedia: js(r"\bwikipedia\b")?,
            apps,
            files_bare: bare(FILES_WORDS)?,
            files_possessive: possessive(FILES_WORDS)?,
        })
    })
    .as_ref()
}

/// `toolsModeOf(project)`: an explicit `toolsMode` wins; a project holding its own list is Manual.
pub fn tools_mode_of(project: &JValue) -> &'static str {
    match as_str(get(project, "toolsMode")) {
        Some("auto") if project.truthy() => "auto",
        Some("manual") if project.truthy() => "manual",
        _ if project.truthy() && matches!(get(project, "toolboxes"), JValue::Arr(_)) => "manual",
        _ => "auto",
    }
}

/// `initialToolsMode(body)` (#1522): the same rule for a new project's request.
pub fn initial_tools_mode(body: &JValue) -> &'static str {
    tools_mode_of(body)
}

/// `gateSaysToolNeeded(gate)` (#1550).
pub fn gate_says_tool_needed(gate: &JValue) -> bool {
    let d = get(gate, "decision");
    gate.truthy() && d.truthy() && as_str(d) != Some("none")
}

/// `isPrivateDataBox(id)` (#1646): every box not on the cloud-safe allow-list.
pub fn is_private_data_box(id: &JValue) -> bool {
    !as_str(id).is_some_and(|i| CLOUD_SAFE_BOXES.contains(&i))
}

/// `PRIVATE_CORE_TOOLS.has(name)`.
pub fn is_private_core_tool(name: &str) -> bool {
    PRIVATE_CORE_TOOLS.contains(&name)
}

/// A string's UTF-16 code units, lone surrogates kept.
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

/// tool-gate.cjs `prefetchableSearch`: a fault answers false.
fn prefetchable(text: &str) -> bool {
    let u = units(text);
    let mut work = tool_gate::Work::for_request(u.len().saturating_mul(2));
    tool_gate::prefetchable(&u, &mut work).unwrap_or(false)
}

fn head(message: &JValue) -> Option<String> {
    as_str(message).map(|m| wtf16::slice(m, 4000))
}

/// `wantsWeb(message)`.
pub fn wants_web(message: &JValue) -> bool {
    let Some(text) = head(message) else {
        return false;
    };
    let Some(r) = res() else {
        return false;
    };
    wants_web_text(r, &text)
}

fn wants_web_text(r: &Res, text: &str) -> bool {
    if trim(text).is_empty() {
        return false;
    }
    let has_url = r.url.is_match(text);
    let short = prefetchable(text);
    if has_url && !short {
        return r.explicit_web.is_match(text);
    }
    if r.explicit_web.is_match(text) || r.web.is_match(text) {
        return true;
    }
    has_url && short
}

/// `automaticBoxesFor(message)`: the box ids a message plainly needs, most specific first.
pub fn automatic_boxes_for(message: &JValue) -> Vec<&'static str> {
    let (Some(text), Some(r)) = (head(message), res()) else {
        return Vec::new();
    };
    if trim(&text).is_empty() {
        return Vec::new();
    }
    let mut out = Vec::new();
    let named = r.nextcloud.is_match(&text);
    let mut app = false;
    for (b, bare, poss) in &r.apps {
        if (named && bare.is_match(&text)) || poss.is_match(&text) {
            out.push(*b);
            app = true;
        }
    }
    if (named && (!app || r.files_bare.is_match(&text))) || r.files_possessive.is_match(&text) {
        out.push("nextcloud-files");
    }
    if r.wikipedia.is_match(&text) {
        out.push("offline-wikipedia");
    }
    let private = out.iter().any(|id| id.starts_with("nextcloud-"));
    if if private {
        r.explicit_web.is_match(&text)
    } else {
        wants_web_text(r, &text)
    } {
        out.push("web-search");
    }
    out
}

/// What [`automatic_additions`] reads besides the message and boxes.
pub struct Eligibility<'a> {
    /// `connectorBoxes`.
    pub connector_boxes: &'a HashSet<String>,
    /// `isWriteTool(name)`.
    pub is_write_tool: &'a dyn Fn(&str) -> bool,
    /// `ready(box) === true`; a throw is `false`.
    pub ready: &'a dyn Fn(&JValue) -> bool,
    /// Whether the answering model is local (#815).
    pub private_data: bool,
}

/// `(box.tools || []).map(...)`: a truthy non-list throws as Node's `.map` does.
fn tool_names(b: &JValue) -> Result<Vec<&str>, Thrown> {
    match get(b, "tools") {
        JValue::Arr(a) => Ok(a
            .iter()
            .filter_map(|t| {
                if !t.truthy() {
                    return None;
                }
                let f = get(t, "function");
                if !f.truthy() {
                    return None;
                }
                as_str(get(f, "name"))
            })
            .collect()),
        v if v.truthy() => Err(Thrown),
        _ => Ok(Vec::new()),
    }
}

/// `automaticEligible(box, …)`.
pub fn automatic_eligible(b: &JValue, e: &Eligibility<'_>) -> Result<bool, Thrown> {
    let Some(id) = as_str(get(b, "id")).filter(|_| b.truthy()) else {
        return Ok(false);
    };
    if e.connector_boxes.contains(id) {
        return Ok(false);
    }
    if !e.private_data && !PUBLIC_BOXES.contains(&id) {
        return Ok(false);
    }
    if !tool_names(b)?.iter().any(|n| !(e.is_write_tool)(n)) {
        return Ok(false);
    }
    Ok((e.ready)(b))
}

fn list(v: &JValue) -> &[JValue] {
    match v {
        JValue::Arr(a) => a,
        _ => &[],
    }
}

/// `automaticAdditions({ message, base, boxes, connectorBoxes, isWriteTool, ready, privateData, max, carried })`
/// (#1550, #1692, #1702).
pub fn automatic_additions(
    message: &JValue,
    base: &[JValue],
    boxes: &JValue,
    e: &Eligibility<'_>,
    max: usize,
    carried: &JValue,
) -> Result<Vec<String>, Thrown> {
    let have: Vec<&JValue> = base.iter().collect();
    let mut by_id: Vec<(&str, &JValue)> = Vec::new();
    for b in list(boxes) {
        if let (true, Some(id)) = (b.truthy(), as_str(get(b, "id"))) {
            // A Map keeps the first key's position and the last value.
            match by_id.iter_mut().find(|(k, _)| *k == id) {
                Some(slot) => slot.1 = b,
                None => by_id.push((id, b)),
            }
        }
    }
    let own = automatic_boxes_for(message);
    let private_turn = own.iter().any(|id| id.starts_with("nextcloud-"))
        || base
            .iter()
            .any(|id| as_str(id).is_some_and(|s| s.starts_with("nextcloud-")));
    let keep: Vec<&str> = if private_turn {
        Vec::new()
    } else {
        list(carried)
            .iter()
            .filter_map(as_str)
            .filter(|id| PUBLIC_BOXES.contains(id))
            .collect()
    };
    let mut out: Vec<String> = Vec::new();
    for id in own.iter().copied().chain(keep) {
        if out.len() >= max {
            break;
        }
        if have.iter().any(|h| as_str(h) == Some(id)) || out.iter().any(|o| o == id) {
            continue;
        }
        let Some((_, b)) = by_id.iter().find(|(k, _)| *k == id) else {
            continue;
        };
        if automatic_eligible(b, e)? {
            out.push(id.to_owned());
        }
    }
    Ok(out)
}

/// `carriedPublicBoxes({ history, boxes })` (#1692, #1702).
pub fn carried_public_boxes(history: &JValue, boxes: &JValue) -> Result<Vec<String>, Thrown> {
    let mut out: Vec<String> = Vec::new();
    let mut add = |id: &str, out: &mut Vec<String>| {
        if PUBLIC_BOXES.contains(&id) && !out.iter().any(|o| o == id) {
            out.push(id.to_owned());
        }
    };
    let mut users = 0;
    for h in list(history).iter().rev() {
        if users >= CARRY_USER_TURNS {
            break;
        }
        if !matches!(h, JValue::Obj(_) | JValue::Arr(_)) {
            continue;
        }
        let role = as_str(get(h, "role"));
        if role == Some("user") && as_str(get(h, "content")).is_some() {
            users += 1;
            let ids = automatic_boxes_for(get(h, "content"));
            if ids.iter().any(|id| id.starts_with("nextcloud-")) {
                return Ok(Vec::new());
            }
            for id in ids {
                add(id, &mut out);
            }
        } else if matches!(role, Some("tool" | "function")) {
            let name = get(h, "name");
            if !is_public_tool_name(name, boxes) {
                return Ok(Vec::new());
            }
            for b in list(boxes) {
                if !b.truthy() {
                    continue;
                }
                let offers = match get(b, "tools") {
                    JValue::Arr(a) => a.iter().any(|t| {
                        let n = get(get(t, "function"), "name");
                        !n.is_nullish() && n == name
                    }),
                    v if v.truthy() => return Err(Thrown),
                    _ => false,
                };
                if offers {
                    if let Some(id) = as_str(get(b, "id")) {
                        add(id, &mut out);
                    }
                }
            }
        }
    }
    Ok(out)
}

/// `orderWithAdditions(ids, added, head)`: the head boxes, then the additions, then the rest.
pub fn order_with_additions(ids: &[String], added: &[String], head: &[String]) -> Vec<String> {
    let is_head = |id: &String| head.contains(id) && !added.contains(id);
    ids.iter()
        .filter(|i| is_head(i))
        .chain(added.iter().filter(|a| ids.contains(a)))
        .chain(ids.iter().filter(|i| !is_head(i) && !added.contains(i)))
        .cloned()
        .collect()
}

/// `nextcloudStorageConnected(storage)`.
pub fn nextcloud_storage_connected(storage: &JValue) -> bool {
    storage.truthy()
        && matches!(as_str(get(storage, "kind")), Some("nextcloud" | "webdav"))
        && get(storage, "username").truthy()
        && get(storage, "secret").truthy()
}

/// `createAutomaticToolsReady({ getStorage, originAllowed })(user)`: whether a box is ready for this
/// account. A box on the `nextcloud` server needs the account's own connection with an allowed origin;
/// the storage is read at most once, and only when such a box is asked about. Fails closed.
pub struct ToolsReady<'a> {
    user_id: Option<String>,
    get_storage: &'a dyn Fn(&str) -> Result<JValue, Thrown>,
    origin_allowed: &'a dyn Fn(&JValue) -> Result<bool, Thrown>,
    nextcloud: std::cell::Cell<Option<bool>>,
}

impl<'a> ToolsReady<'a> {
    /// The checker for `user` (`{ id }` or null).
    pub fn new(
        user: &JValue,
        get_storage: &'a dyn Fn(&str) -> Result<JValue, Thrown>,
        origin_allowed: &'a dyn Fn(&JValue) -> Result<bool, Thrown>,
    ) -> Self {
        let user_id = if user.truthy() {
            as_str(get(user, "id")).map(str::to_owned)
        } else {
            None
        };
        ToolsReady {
            user_id,
            get_storage,
            origin_allowed,
            nextcloud: std::cell::Cell::new(None),
        }
    }

    fn nextcloud_ready(&self) -> bool {
        if let Some(v) = self.nextcloud.get() {
            return v;
        }
        let v = self.user_id.as_deref().is_some_and(|id| {
            (self.get_storage)(id).is_ok_and(|s| {
                nextcloud_storage_connected(&s)
                    && (self.origin_allowed)(get(&s, "baseUrl")) == Ok(true)
            })
        });
        self.nextcloud.set(Some(v));
        v
    }

    /// `(box) => !box || box.server !== 'nextcloud' || nextcloudReady()`.
    pub fn ready(&self, b: &JValue) -> bool {
        !b.truthy() || as_str(get(b, "server")) != Some("nextcloud") || self.nextcloud_ready()
    }
}

/// `toolCapableRoute`'s answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Moved {
    /// Whether the turn moves to another role.
    pub rerouted: bool,
    /// The model it answers on.
    pub model: String,
    /// Its role.
    pub role: Option<String>,
}

/// `toolCapableRoute({ model, routedRole, roles, allowsTools, hostOf })` (#1550, #1854): keep a model that
/// can take tools, else the first tool-capable fallback role. `roles` is in `FALLBACK_ROLES` lookup form.
/// `allows_tools` is awaited in turn; a rejected promise is `Err` and ends the route as Node's throw does.
pub async fn tool_capable_route<F, Fut>(
    model: &str,
    routed_role: Option<&str>,
    roles: &dyn Fn(&str) -> Option<String>,
    mut allows_tools: F,
    host_of: &dyn Fn(Option<&str>) -> String,
) -> Result<Moved, Thrown>
where
    F: FnMut(String, Option<String>) -> Fut,
    Fut: Future<Output = Result<bool, Thrown>>,
{
    let kept = || Moved {
        rerouted: false,
        model: model.to_owned(),
        role: routed_role.map(str::to_owned),
    };
    if allows_tools(model.to_owned(), routed_role.map(str::to_owned)).await? {
        return Ok(kept());
    }
    for role in FALLBACK_ROLES {
        let Some(candidate) = roles(role).filter(|c| !c.is_empty()) else {
            continue;
        };
        if Some(role) == routed_role
            || (candidate == model && host_of(Some(role)) == host_of(routed_role))
        {
            continue;
        }
        if allows_tools(candidate.clone(), Some(role.to_owned())).await? {
            return Ok(Moved {
                rerouted: true,
                model: candidate,
                role: Some(role.to_owned()),
            });
        }
    }
    Ok(kept())
}
