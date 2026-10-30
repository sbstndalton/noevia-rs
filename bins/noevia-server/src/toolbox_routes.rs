//! `GET /api/toolboxes/permitted?projectId=&mode=chat|cowork` (sbstndalton/noevia#2202 round 2), moved
//! from core `routes/toolboxes.cjs` and `toolboxes-permitted.cjs`: the tools this account could use
//! THIS turn, with the state of every box and the permission of every tool (#237). Capability
//! `rust-toolboxes-permitted`; core's `rust-toolboxes-permitted.cjs` refuses the route with
//! `RUST_TOOLBOXES_PERMITTED_OWNED`. The rules are the `toolboxes-permitted` crate's, as before;
//! this module reads what they need and words the answer.
//!
//! Everything about the person is read **at the time of the request** from what the front owns, so
//! there is no cache and no stale window after a write of the front's (core cached the view for 30 s,
//! and its key left out the project's toolbox selection, the Drive connection and the MCP readiness
//! that this front changes without clearing it):
//!
//! - the project (tenant-scoped, `users/<id>/projects.json`): its tools mode and toolbox selection;
//! - the account's tool permissions (`tool_policies`), its Drive connection (gdrive.rs), its MCP
//!   sign-ins and keys (the credential broker), the Diary add-on preference, and the directory
//!   (which servers ask for a sign-in or a key).
//!
//! What only Node's MCP client knows - the offered boxes with their tools and whether each tool
//! writes, the configured boxes, the coding harness switch and whether a repository is registered -
//! comes from the discovery snapshot (mcp_discovery.rs). A snapshot that is not provably current is
//! not ready: the answer is a 503 asking to try again, never an old catalogue.
use crate::mcp_discovery::{BoxInfo, Catalogue};
use crate::{identity, reply, App};
use axum::body::Body;
use axum::http::{Request, Response, StatusCode};
use axum::response::IntoResponse;
use js_json::JValue;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use toolboxes_permitted::{Input, Permission, Policy, Project, Work};

pub const PERMITTED: &str = "/api/toolboxes/permitted";
pub const ROUTES: &[&str] = &[PERMITTED];

/// The box of a project's documents (core project-docs-default.cjs `BOX`).
const DOCS_BOX: &str = "project-docs";
const CODE_BOX: &str = "code";

/// core `REASONS`: the English fallbacks; `reasonCode` names each one for the client's catalogue.
const REASONS: [(&str, &str); 11] = [
    (
        "connect",
        "Connect this account in Settings → Connected apps first.",
    ),
    (
        "signIn",
        "Sign in to this service in Settings → Connected apps first.",
    ),
    ("diaryOff", "The Diary add-on is off for this account."),
    ("blocked", "Blocked in your tool permissions."),
    ("notConnected", "Not connected on this server right now."),
    (
        "codeNeedsCowork",
        "Switch this session to Cowork to use the coding harness.",
    ),
    (
        "codeAdminOnly",
        "The coding harness is limited to administrators.",
    ),
    ("codeOff", "The coding harness is off on this server."),
    (
        "codeNeedsProject",
        "Open a project chat to run a Cowork task.",
    ),
    (
        "codeNoRepository",
        "No repository is registered on this server.",
    ),
    (
        "unchecked",
        "This tool's permission couldn't be checked, so it's unavailable for now.",
    ),
];

fn reason_text(code: &str) -> Option<&'static str> {
    REASONS.iter().find(|(c, _)| *c == code).map(|(_, t)| *t)
}

/// core `reasonFor(code)`: the text and the code, `unchecked` for one it does not know.
fn reason_for(code: Option<&str>) -> (&'static str, &'static str) {
    match code.and_then(|c| REASONS.iter().find(|(k, _)| *k == c)) {
        Some((k, t)) => (t, k),
        None => (
            "This tool's permission couldn't be checked, so it's unavailable for now.",
            "unchecked",
        ),
    }
}

fn units(s: &str) -> Vec<u16> {
    s.encode_utf16().collect()
}

fn s(v: &str) -> JValue {
    JValue::Str(v.to_owned())
}

fn opt(v: Option<&str>) -> JValue {
    v.map_or(JValue::Null, s)
}

/// What the account's own state said, gathered on the blocking pool.
struct Facts {
    project: Result<Option<Project>, ()>,
    policies: HashMap<String, String>,
    diary: bool,
    /// The connectors this account is connected to (Google Drive is the only one this front can say).
    connected: Vec<String>,
    /// Directory servers each account signs in to (`true`) or brings its own key for (`false`).
    oauth: HashMap<String, bool>,
    ready: crate::mcp_broker::Ready,
}

/// The project asked about (`Ok(None)`: no id was asked about; `Err`: it does not exist).
fn project_of(app: &App, user: &str, project_id: Option<&str>) -> Result<Option<Project>, ()> {
    let Some(pid) = project_id else {
        return Ok(None);
    };
    let data_dir = app.config.data_dir.as_ref().ok_or(())?;
    let dir = server_projects::store::user_dir(data_dir, user).map_err(|_| ())?;
    let projects = server_projects::store::read(&dir).map_err(|_| ())?;
    let found = server_projects::store::find(&projects, pid).ok_or(())?;
    Ok(Some(Project {
        auto: found.get("toolsMode").as_str() == Some("auto"),
        docs_defaulted: found.get("docsToolboxDefaulted").as_bool() == Some(true),
        toolboxes: match found.get("toolboxes") {
            JValue::Arr(items) => Some(
                items
                    .iter()
                    .map(|i| i.as_str().map(units))
                    .collect::<Vec<_>>(),
            ),
            _ => None,
        },
    }))
}

fn gather(app: &Arc<App>, user: &str, admin: bool, project_id: Option<&str>) -> Result<Facts, ()> {
    let writer = app.writes.writer().map_err(|_| ())?;
    let policies: HashMap<String, String> = writer
        .read(|r| {
            r.rows(
                "SELECT tool, mode FROM tool_policies WHERE user_id=?1",
                [user],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
            )
        })
        .map_err(|_| ())?
        .into_iter()
        .collect();
    let diary = writer
        .read(|r| {
            r.row(
                "SELECT diary_enabled FROM user_features WHERE user_id=?1",
                [user],
                |row| row.get::<_, i64>(0),
            )
        })
        .map_err(|_| ())?
        .is_some_and(|d| d != 0);
    let connected: Vec<String> = crate::gdrive::of(app)
        .is_some_and(|drive| {
            drive
                .slot_for(user, admin)
                .map(|slot| drive.state(&slot).state == "connected")
                .unwrap_or(false)
        })
        .then(|| "gdrive".to_owned())
        .into_iter()
        .collect();
    let (oauth, ready) = match &app.mcp {
        Some(mcp) => {
            let oauth = mcp
                .dir_rows(app)
                .map_err(|_| ())?
                .into_iter()
                .filter(|r| r.oauth || r.personal)
                .map(|r| (r.id, r.oauth))
                .collect();
            (oauth, mcp.ready_for(app, user).map_err(|_| ())?)
        }
        None => (HashMap::new(), crate::mcp_broker::Ready::default()),
    };
    Ok(Facts {
        project: project_of(app, user, project_id),
        policies,
        diary,
        connected,
        oauth,
        ready,
    })
}

/// A listed box (what the answer shows), before the crate's verdict is merged in.
struct Listed {
    id: String,
    label: Option<String>,
    description: Option<String>,
    source: String,
    in_app: bool,
    /// The host's own limit on the box: `connect`, `signIn`, `diaryOff` or `notConnected`.
    limit: Option<&'static str>,
    code: bool,
    tools: Vec<ListedTool>,
}

struct ListedTool {
    name: String,
    description: String,
    write: bool,
    /// The most permissive permission the account's own limits allow.
    floor: Permission,
    blocked: bool,
}

fn policy_of(facts: &Facts, tool: &str, write: bool) -> (&'static str, Policy) {
    let stored = facts.policies.get(tool).map(|m| units(m));
    let mode = policy_leaves::mode(stored.as_deref(), write).name();
    (
        mode,
        match mode {
            "block" => Policy::Block,
            "ask" => Policy::Ask,
            _ => Policy::Other,
        },
    )
}

fn permission_name(p: Permission) -> &'static str {
    match p {
        Permission::Allowed => "allowed",
        Permission::NeedsApproval => "needs-approval",
        Permission::Unavailable => "unavailable",
    }
}

/// core `computePermittedTools`: the answer's boxes.
fn compute(cat: &Catalogue, facts: &Facts, admin: bool, cowork: bool) -> Result<Vec<JValue>, ()> {
    let project = facts.project.as_ref().ok().and_then(Option::as_ref);
    let connector: HashSet<&str> = cat.connector_boxes.iter().map(String::as_str).collect();
    let connected: Vec<&str> = facts.connected.iter().map(String::as_str).collect();
    let is_connected = |id: &str| connected.contains(&id);

    let mut listed: Vec<Listed> = Vec::new();
    let mut crate_boxes: Vec<toolboxes_permitted::Box> = Vec::new();
    for b in &cat.boxes {
        let BoxInfo { id, tools, .. } = b;
        let mut limit: Option<&'static str> = None;
        let mut asked: Option<bool> = None;
        if connector.contains(id.as_str()) && !is_connected(id) {
            limit = Some("connect");
        } else if let Some(is_oauth) = facts.oauth.get(id.as_str()) {
            let ok = if *is_oauth {
                facts.ready.signed_in.contains(id)
            } else {
                facts.ready.keys.contains(id)
            };
            asked = Some(ok);
            if !ok {
                limit = Some("signIn");
            }
        }
        if limit.is_none() && id == "diary" && !facts.diary {
            limit = Some("diaryOff");
        }
        let mut rows = Vec::with_capacity(tools.len());
        let mut for_crate = Vec::with_capacity(tools.len());
        for t in tools {
            let (mode, policy) = policy_of(facts, &t.name, t.write);
            for_crate.push(toolboxes_permitted::Tool {
                write: t.write,
                policy,
            });
            rows.push(ListedTool {
                name: t.name.clone(),
                description: t.description.chars().take(240).collect(),
                write: t.write,
                floor: if mode == "block" {
                    Permission::Unavailable
                } else if mode == "ask" || t.write {
                    Permission::NeedsApproval
                } else {
                    Permission::Allowed
                },
                blocked: mode == "block",
            });
        }
        crate_boxes.push(toolboxes_permitted::Box {
            id: units(id),
            ready: asked,
            tools: for_crate,
        });
        listed.push(Listed {
            id: id.clone(),
            label: Some(b.label.clone()),
            description: Some(b.description.clone()),
            source: b.source.clone(),
            in_app: b.in_app,
            limit,
            code: false,
            tools: rows,
        });
    }
    // Configured but not discovered (the server is down, or its credentials are missing): shown so the
    // person sees the boundary, never offered.
    let present: HashSet<&str> = cat.boxes.iter().map(|b| b.id.as_str()).collect();
    for m in &cat.manifest {
        if present.contains(m.id.as_str()) || (m.id == "diary" && !facts.diary) {
            continue;
        }
        listed.push(Listed {
            id: m.id.clone(),
            label: Some(if m.label.is_empty() {
                m.id.clone()
            } else {
                m.label.clone()
            }),
            description: Some(m.description.clone()),
            source: "mcp".into(),
            in_app: m.in_app,
            limit: Some("notConnected"),
            code: false,
            tools: Vec::new(),
        });
    }
    listed.push(Listed {
        id: CODE_BOX.into(),
        label: Some("Coding harness".into()),
        description: Some("Read, edit and run commands in a registered repository. Every edit and command asks first.".into()),
        source: "code".into(),
        in_app: true,
        limit: None,
        code: true,
        tools: [
            ("read_repository", "Read files in the chosen repository.", false),
            ("edit_file", "Change files on a task branch.", true),
            ("execute_command", "Run a command in the sandbox.", true),
        ]
        .into_iter()
        .map(|(name, description, write)| ListedTool {
            name: name.into(),
            description: description.into(),
            write,
            floor: Permission::Allowed,
            blocked: false,
        })
        .collect(),
    });

    let input = Input {
        is_admin: admin,
        project: project.cloned(),
        cowork,
        defaults: cat.defaults.iter().map(|d| Some(units(d))).collect(),
        docs: units(DOCS_BOX),
        connector: cat.connector_boxes.iter().map(|c| units(c)).collect(),
        connected: connected.iter().map(|c| Some(units(c))).collect(),
        oauth: facts.oauth.keys().map(|o| units(o)).collect(),
        diary_enabled: facts.diary,
        harness_enabled: cat.harness_enabled,
        has_repositories: cat.has_repositories,
        boxes: crate_boxes,
        manifest: cat
            .manifest
            .iter()
            .map(|m| Some(Some(units(&m.id))))
            .collect(),
    };
    // Whatever the crate says is checked against the catalogue it was asked about; another shape is
    // no verdict at all.
    let verdict = toolboxes_permitted::permitted(&input, &mut Work::for_request(4 * 1024 * 1024))
        .ok()
        .filter(|v| {
            v.len() == listed.len()
                && v.iter().zip(&listed).all(|(p, l)| {
                    p.id.as_deref() == Some(units(&l.id).as_slice())
                        && p.tools.len() == l.tools.len()
                })
        });
    let header = |l: &Listed| -> Vec<(&'static str, JValue)> {
        let mut pairs = vec![("id", s(&l.id))];
        if let Some(label) = &l.label {
            pairs.push(("label", s(label)));
        }
        if let Some(description) = &l.description {
            pairs.push(("description", s(description)));
        }
        pairs.push(("source", s(&l.source)));
        pairs.push(("inApp", JValue::Bool(l.in_app)));
        pairs
    };
    let Some(verdict) = verdict else {
        // The floor: no verdict, so every box and tool shows unavailable.
        let (reason, code) = reason_for(Some("unchecked"));
        return Ok(listed
            .iter()
            .map(|l| {
                let mut pairs = header(l);
                pairs.push(("state", s("unavailable")));
                pairs.push(("reason", s(reason)));
                pairs.push(("reasonCode", s(code)));
                pairs.push(("active", JValue::Bool(false)));
                pairs.push((
                    "tools",
                    JValue::Arr(
                        l.tools
                            .iter()
                            .map(|t| {
                                JValue::obj([
                                    ("name", s(&t.name)),
                                    ("description", s(&t.description)),
                                    ("write", JValue::Bool(t.write)),
                                    ("permission", s("unavailable")),
                                    ("reason", s(reason)),
                                    ("reasonCode", s(code)),
                                ])
                            })
                            .collect(),
                    ),
                ));
                JValue::obj(pairs)
            })
            .collect());
    };
    let mut out = Vec::with_capacity(listed.len());
    for (l, p) in listed.iter().zip(&verdict) {
        let mut state = if p.available {
            "available"
        } else {
            "unavailable"
        };
        let (mut reason, mut reason_code): (Option<&str>, Option<&str>) = if p.available {
            (None, None)
        } else {
            let (t, c) = reason_for(p.reason_code);
            (Some(t), Some(c))
        };
        if state == "available" {
            if let (Some(limit), false) = (l.limit, l.code) {
                state = "unavailable";
                let (t, c) = reason_for(Some(limit));
                reason = Some(t);
                reason_code = Some(c);
            }
        }
        let active = p.active && state == "available";
        let mut tools = Vec::with_capacity(l.tools.len());
        for (t, q) in l.tools.iter().zip(&p.tools) {
            let (permission, t_reason, t_code): (Permission, Option<&str>, Option<&str>) = if l.code
            {
                let (r, c) = match q.reason_code {
                    Some(code) => {
                        let (t, c) = reason_for(Some(code));
                        (Some(t), Some(c))
                    }
                    None => (None, None),
                };
                (q.permission, r, c)
            } else {
                let mut permission = q.permission;
                let (mut r, mut c): (Option<&str>, Option<&str>) = match q.reason_code {
                    Some("blocked") => (reason_text("blocked"), Some("blocked")),
                    Some(code) => {
                        let (t, k) = reason_for(Some(code));
                        (Some(t), Some(k))
                    }
                    None => (None, None),
                };
                if state == "unavailable" && permission != Permission::Unavailable {
                    permission = Permission::Unavailable;
                    r = reason;
                    c = reason_code;
                }
                let floor = if state == "unavailable" {
                    Permission::Unavailable
                } else {
                    t.floor
                };
                if permission < floor {
                    permission = floor;
                    if floor == Permission::Unavailable && t.blocked {
                        r = reason_text("blocked");
                        c = Some("blocked");
                    } else {
                        r = None;
                        c = None;
                    }
                }
                (permission, r, c)
            };
            tools.push(JValue::obj([
                ("name", s(&t.name)),
                ("description", s(&t.description)),
                ("write", JValue::Bool(t.write)),
                ("permission", s(permission_name(permission))),
                ("reason", opt(t_reason)),
                ("reasonCode", opt(t_code)),
            ]));
        }
        let mut pairs = header(l);
        pairs.push(("state", s(state)));
        pairs.push(("reason", opt(reason)));
        pairs.push(("reasonCode", opt(reason_code)));
        pairs.push(("active", JValue::Bool(active)));
        pairs.push(("tools", JValue::Arr(tools)));
        out.push(JValue::obj(pairs));
    }
    Ok(out)
}

fn answer(status: u16, body: &JValue) -> Response<Body> {
    reply::json_text(
        StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_GATEWAY),
        js_json::stringify(body).unwrap_or_else(|| "{}".into()),
        true,
    )
}

fn error(status: u16, message: &str) -> Response<Body> {
    answer(status, &JValue::obj([("error", s(message))]))
}

pub async fn serve(
    app: &Arc<App>,
    conn: crate::serve::Conn,
    req: Request<Body>,
    path: String,
) -> Response<Body> {
    if path != PERMITTED {
        return app.proxy.forward(conn.peer, req).await;
    }
    let method = req.method().clone();
    let query = req.uri().query().unwrap_or("").to_owned();
    let (parts, _body) = req.into_parts();
    let creds = identity::creds_of(&parts);
    let gate = Arc::clone(&app.identity);
    let now = identity::now_ms();
    let gate_path = path.clone();
    let id = match tokio::task::spawn_blocking(move || gate.gate_blocking(&creds, &gate_path, now))
        .await
    {
        Ok(Ok(id)) => id,
        Ok(Err(rejection)) => return rejection.into_response(),
        Err(_) => return identity::Rejection::Unavailable.into_response(),
    };
    if method != axum::http::Method::GET {
        return error(405, "method not allowed");
    }
    let param = |name: &str| {
        url::form_urlencoded::parse(query.as_bytes())
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.into_owned())
    };
    let project_id = param("projectId").filter(|p| !p.is_empty());
    let mode = param("mode").unwrap_or_else(|| "chat".into());
    let mode = if mode.is_empty() { "chat".into() } else { mode };
    if mode != "chat" && mode != "cowork" {
        return error(400, "mode must be \"chat\" or \"cowork\"");
    }
    let user = id.user.id.clone();
    let admin = id.user.role == server_auth::Role::Admin;
    let (a, u, pid) = (Arc::clone(app), user, project_id.clone());
    let facts =
        match tokio::task::spawn_blocking(move || gather(&a, &u, admin, pid.as_deref())).await {
            Ok(Ok(f)) => f,
            _ => {
                return error(
                    503,
                    "The tool permissions could not be read. Try again in a moment.",
                )
            }
        };
    if facts.project.is_err() {
        return error(404, "project not found");
    }
    let Some(catalogue) = crate::mcp_discovery::read(app).await else {
        let mut res = error(
            503,
            "The tool list is being prepared. Try again in a moment.",
        );
        res.headers_mut()
            .insert("retry-after", axum::http::HeaderValue::from_static("2"));
        return res;
    };
    let cowork = mode == "cowork";
    let boxes =
        match tokio::task::spawn_blocking(move || compute(&catalogue, &facts, admin, cowork)).await
        {
            Ok(Ok(b)) => b,
            _ => return error(500, "The tool list could not be built."),
        };
    answer(
        200,
        &JValue::obj([
            ("mode", s(&mode)),
            ("projectId", project_id.as_deref().map_or(JValue::Null, s)),
            ("boxes", JValue::Arr(boxes)),
            ("computedAt", JValue::Num(identity::now_ms() as f64)),
        ]),
    )
}

/// The old JS rules' frozen answers against `compute` (sbstndalton/noevia#2202 round 2 review, M4).
///
/// Until round 2 the catalogue was core's `computePermittedTools`, checked against 2500 frozen answers of
/// the JS (core tests/fixtures/toolboxes-permitted-js.v1.json, byte-identical, sha256-pinned in core's
/// differential test). The computation is this file's now, and core's copy of that check is gone, so this
/// is the one that keeps the port honest: each frozen input is rebuilt as what the front reads (the
/// catalogue as the publisher projects it, the account's facts as the front gathers them), `compute` runs,
/// and the hash of its key-sorted JSON is the frozen `catalogue.live` one.
///
/// Three presentation differences are the publisher's by design, and are put back on the output before the
/// hash, tool by tool, so that everything else (every state, permission, reason and flag) is compared
/// exactly: a tool's `write` is the flag `!!isWriteTool(name)` (the JS printed whatever the callback
/// returned), a name is a text (a tool named by a number is the number's text here, and printed as the
/// number again), and a description is cut at 240 characters (the same). A case the front cannot be
/// asked is skipped, and counted: an id list holding something that is not text, a box whose id is not
/// text, a manifest entry that is not an object with a text id. The JS faulted on the first two (every box
/// "unchecked"); the publisher cannot write the third.
#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]
mod differential {
    use super::*;
    use crate::mcp_discovery::{ManifestBox, ToolInfo};
    use sha2::{Digest, Sha256};

    const FROZEN: &str = include_str!("../tests/fixtures/toolboxes-permitted-js.v1.json");
    /// core tests/server/toolboxes-permitted-differential.test.cjs `PINS`: the same bytes.
    const PIN: &str = "1aff93ac23e096837f167433a569b6193521656aba35f7702fdddcaeed6f685e";

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }

    /// core's `canon` in the differential test: key-sorted JSON, `undefined` as null.
    fn canon(v: &JValue) -> String {
        match v {
            JValue::Arr(items) => format!(
                "[{}]",
                items.iter().map(canon).collect::<Vec<_>>().join(",")
            ),
            JValue::Obj(pairs) => {
                let mut sorted: Vec<&(String, JValue)> = pairs.iter().collect();
                sorted.sort_by(|a, b| a.0.encode_utf16().cmp(b.0.encode_utf16()));
                format!(
                    "{{{}}}",
                    sorted
                        .iter()
                        .map(|(k, v)| format!(
                            "{}:{}",
                            js_json::stringify(&JValue::Str(k.clone())).unwrap(),
                            canon(v)
                        ))
                        .collect::<Vec<_>>()
                        .join(",")
                )
            }
            JValue::Undefined => "null".into(),
            other => js_json::stringify(other).unwrap(),
        }
    }

    fn hash_of(v: &JValue) -> String {
        hex(&Sha256::digest(canon(v).as_bytes()))[..16].to_owned()
    }

    fn texts(list: &JValue) -> Option<Vec<String>> {
        match list {
            JValue::Arr(items) => items
                .iter()
                .map(|x| x.as_str().map(str::to_owned))
                .collect(),
            _ => None,
        }
    }

    /// `String(v || '')`.
    fn js_text(v: &JValue) -> Option<String> {
        if !v.truthy() {
            return Some(String::new());
        }
        js_json::to_js_string(v).ok()
    }

    fn cut240(text: &str) -> String {
        String::from_utf16_lossy(&text.encode_utf16().take(240).collect::<Vec<_>>())
    }

    /// The hash of what `compute` gives for the case, or `None` when the front cannot be asked it.
    fn ours(case: &JValue) -> Option<String> {
        let i = case.get("in");
        let defaults = texts(i.get("defaultToolboxes"))?;
        let connector = texts(i.get("connectorBoxes"))?;
        let connected = texts(i.get("connected"))?;
        let oauth_ids = texts(i.get("oauthServerIds"))?;
        let policy = i.get("policy");
        let write = i.get("write");

        let JValue::Arr(raw_boxes) = i.get("boxes") else {
            return None;
        };
        let mut boxes = Vec::new();
        // Per box and tool, what the JS printed that the front prints differently.
        let mut raw: Vec<Vec<(JValue, JValue)>> = Vec::new();
        let mut policies: HashMap<String, String> = HashMap::new();
        for b in raw_boxes {
            let id = b.get("id").as_str()?.to_owned();
            let mut tools = Vec::new();
            let mut printed = Vec::new();
            if let JValue::Arr(items) = b.get("tools") {
                for t in items {
                    let name = t.get("function").get("name");
                    if !name.truthy() {
                        continue;
                    }
                    let text = match name {
                        JValue::Str(n) => n.clone(),
                        JValue::Num(_) => js_json::to_js_string(name).ok()?,
                        _ => return None,
                    };
                    let flag = write.get(&text);
                    tools.push(ToolInfo {
                        name: text.clone(),
                        description: cut240(&js_text(t.get("function").get("description"))?),
                        write: flag.truthy(),
                    });
                    // The JS treated a mode other than block or ask as neither; the front's own reading of
                    // a stored mode makes anything else a block, so only these two are stored.
                    if let Some(m) = policy.get(&text).as_str().filter(|m| *m == "block" || *m == "ask")
                    {
                        policies.insert(text, m.to_owned());
                    }
                    printed.push((name.clone(), flag.clone()));
                }
            }
            raw.push(printed);
            boxes.push(BoxInfo {
                id,
                label: b.get("label").as_str()?.to_owned(),
                description: b.get("description").as_str()?.to_owned(),
                source: b.get("source").as_str()?.to_owned(),
                in_app: b.get("source").as_str() == Some("builtin"),
                server: None,
                tools,
            });
        }
        let mut manifest = Vec::new();
        if let JValue::Arr(entries) = i.get("manifest") {
            for e in entries {
                let id = e.get("id").as_str()?.to_owned();
                if !matches!(e, JValue::Obj(_)) {
                    return None;
                }
                let label = if e.get("label").truthy() {
                    e.get("label").as_str()?.to_owned()
                } else {
                    id.clone()
                };
                manifest.push(ManifestBox {
                    id,
                    label,
                    description: js_text(e.get("description"))?,
                    in_app: true,
                });
            }
        }
        let project = {
            let p = i.get("project");
            if p.truthy() {
                Some(Project {
                    auto: p.get("toolsMode").as_str() == Some("auto"),
                    docs_defaulted: p.get("docsToolboxDefaulted").as_bool() == Some(true),
                    toolboxes: match p.get("toolboxes") {
                        JValue::Arr(items) => {
                            Some(items.iter().map(|x| x.as_str().map(units)).collect())
                        }
                        _ => None,
                    },
                })
            } else {
                None
            }
        };
        let cat = Catalogue {
            mcp_enabled: true,
            harness_enabled: i.get("harnessEnabled").truthy(),
            has_repositories: matches!(i.get("repositories"), JValue::Arr(r) if !r.is_empty()),
            nextcloud_configured: false,
            nextcloud_origins: Vec::new(),
            defaults,
            connector_boxes: connector,
            boxes,
            manifest,
            servers: Vec::new(),
        };
        let ready = i.get("ready");
        let facts = Facts {
            project: Ok(project),
            policies,
            diary: i.get("diaryEnabled").truthy(),
            connected,
            oauth: oauth_ids.iter().map(|id| (id.clone(), true)).collect(),
            ready: crate::mcp_broker::Ready {
                signed_in: oauth_ids
                    .iter()
                    .filter(|id| ready.get(id).truthy())
                    .cloned()
                    .collect(),
                keys: Default::default(),
            },
        };
        let admin = i.get("user").get("role").as_str() == Some("admin");
        let cowork = i.get("mode").as_str() == Some("cowork");
        let mut out = compute(&cat, &facts, admin, cowork).ok()?;
        // Put the JS's own printing of a tool's name and write flag back (see the module note).
        for (shown, printed) in out.iter_mut().zip(&raw) {
            let JValue::Obj(pairs) = shown else {
                panic!("a box is an object")
            };
            let tools = pairs.iter_mut().find(|(k, _)| k == "tools").unwrap();
            let JValue::Arr(tools) = &mut tools.1 else {
                panic!("tools is a list")
            };
            assert_eq!(tools.len(), printed.len());
            for (tool, (name, flag)) in tools.iter_mut().zip(printed) {
                let JValue::Obj(fields) = tool else {
                    panic!("a tool is an object")
                };
                for (k, v) in fields.iter_mut() {
                    match k.as_str() {
                        "name" => *v = name.clone(),
                        "write" => *v = flag.clone(),
                        _ => {}
                    }
                }
            }
        }
        Some(hash_of(&JValue::Arr(out)))
    }

    #[test]
    fn the_front_gives_the_catalogue_the_old_js_gave_for_every_frozen_input_it_can_be_asked() {
        assert_eq!(hex(&Sha256::digest(FROZEN.as_bytes())), PIN);
        let frozen = js_json::parse(FROZEN).unwrap();
        let JValue::Arr(cases) = frozen.get("cases") else {
            panic!("cases")
        };
        assert_eq!(cases.len(), 2500);
        let (mut compared, mut skipped) = (0usize, 0usize);
        let mut wrong = Vec::new();
        for case in cases {
            let catalogue = case.get("catalogue");
            match ours(case) {
                None => skipped += 1,
                Some(hash) => {
                    compared += 1;
                    assert_eq!(
                        catalogue.get("faulted").as_bool(),
                        Some(false),
                        "case {:?}: a case the front can be asked was a fault in the JS",
                        case.get("i")
                    );
                    if Some(hash.as_str()) != catalogue.get("live").as_str() {
                        wrong.push(js_json::stringify(case.get("i")).unwrap());
                    }
                }
            }
        }
        assert!(wrong.is_empty(), "differing cases: {}", wrong.join(" "));
        // A comparison that quietly skipped everything would pass: the counts are the measured ones.
        assert_eq!((compared, skipped), (0, 0));
    }
}
