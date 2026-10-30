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
    connected: bool,
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
    let connected = crate::gdrive::of(app).is_some_and(|drive| {
        drive
            .slot_for(user, admin)
            .map(|slot| drive.state(&slot).state == "connected")
            .unwrap_or(false)
    });
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
    let connected: Vec<&str> = if facts.connected {
        vec!["gdrive"]
    } else {
        Vec::new()
    };
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
