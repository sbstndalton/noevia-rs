//! noevia-core's `server/toolboxes-permitted.cjs` in Rust, exported from `dav-parse.wasm`
//! (TOOLBOXES_PERMITTED_IMPL): which toolbox ids one request carries, and which tools an account
//! could use this turn, with each box's and tool's state and reason code.
//!
//! - [`project_ids`]: `projectToolboxIds`, a project's own list (Automatic: the operator default,
//!   plus the Project documents box it defaulted to; Manual: its list or the default).
//! - [`selected_ids`]: `selectedToolboxIds`, that list without connector ids, plus the account's
//!   connected connectors.
//! - [`permitted`]: `computePermittedTools` over the host's projection of the account, the project,
//!   the discovered boxes (each tool's write flag and policy mode, read once by the JS run), the
//!   configured-but-undiscovered boxes and the coding harness.
//!
//! The host (toolboxes-permitted.cjs under TOOLBOXES_PERMITTED_IMPL=wasm) computes the JS answer
//! first and never uses the port to offer more: a box id is carried only if both carry it, a box is
//! available and active only if both say so, and a tool's permission is the stricter of the two
//! (`unavailable` over `needs-approval` over `allowed`). A fault or a reply of another shape makes
//! the request carry no box and the catalogue show every box and tool unavailable.
//!
//! Nothing here reads text beyond comparing ids, so there is nothing Unicode-dependent. Linear:
//! every loop is charged to a work budget proportional to the request size, no panics.

#![forbid(unsafe_code)]

use prompt_framing::json::{self, Value};
use std::collections::HashSet;

/// The largest request [`call`] accepts (the op byte and the JSON).
pub const MAX_INPUT_BYTES: usize = 4 * 1024 * 1024 + 1;
/// Work units a request may use per byte of its size (plus [`WORK_FLOOR`]).
pub const WORK_PER_BYTE: u64 = 64;
/// Work units every request may use, whatever its size.
pub const WORK_FLOOR: u64 = 1 << 16;

const JSON_CAP: usize = 6;

/// Why the port gives no answer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refusal {
    Input,
    TooLarge,
}

impl Refusal {
    /// The refusal reply.
    pub fn json(self) -> &'static str {
        match self {
            Refusal::Input => r#"{"error":"input"}"#,
            Refusal::TooLarge => r#"{"error":"too_large"}"#,
        }
    }
}

type R<T> = Result<T, Refusal>;

/// Work left for one request.
#[derive(Debug)]
pub struct Work {
    left: u64,
}

impl Work {
    /// A budget of `units`.
    pub fn new(units: u64) -> Self {
        Work { left: units }
    }

    /// The budget for a request of `bytes` bytes.
    pub fn for_request(bytes: usize) -> Self {
        let bytes = u64::try_from(bytes).unwrap_or(u64::MAX);
        Work::new(
            bytes
                .saturating_mul(WORK_PER_BYTE)
                .saturating_add(WORK_FLOOR),
        )
    }

    /// Charge `n` units (and one for the step itself).
    pub fn charge(&mut self, n: usize) -> R<()> {
        let n = u64::try_from(n).unwrap_or(u64::MAX).saturating_add(1);
        if n > self.left {
            self.left = 0;
            return Err(Refusal::TooLarge);
        }
        self.left -= n;
        Ok(())
    }
}

/// An id list element: a string, or `None` for any other value the JS list holds.
pub type Id = Option<Vec<u16>>;

/// A project as the host projects it (`null` for a falsy project).
#[derive(Clone, Debug, Default)]
pub struct Project {
    /// `project.toolsMode === 'auto'`
    pub auto: bool,
    /// `project.docsToolboxDefaulted === true`
    pub docs_defaulted: bool,
    /// `project.toolboxes` when it is an array.
    pub toolboxes: Option<Vec<Id>>,
}

fn has(list: &[Id], id: &[u16]) -> bool {
    list.iter().any(|x| x.as_deref() == Some(id))
}

/// `projectToolboxIds(project, defaultToolboxes)`.
pub fn project_ids(
    project: Option<&Project>,
    defaults: &[Id],
    docs: &[u16],
    work: &mut Work,
) -> R<Vec<Id>> {
    work.charge(defaults.len())?;
    match project {
        Some(p) if p.auto => {
            if let Some(t) = &p.toolboxes {
                work.charge(t.len())?;
            }
            let keep = p.docs_defaulted
                && p.toolboxes.as_ref().is_some_and(|t| has(t, docs))
                && !has(defaults, docs);
            let mut out = defaults.to_vec();
            if keep {
                out.push(Some(docs.to_vec()));
            }
            Ok(out)
        }
        Some(Project {
            toolboxes: Some(t), ..
        }) => {
            work.charge(t.len())?;
            Ok(t.clone())
        }
        _ => Ok(defaults.to_vec()),
    }
}

/// `selectedToolboxIds({ project, defaultToolboxes, connectorBoxes, connected })`.
pub fn selected_ids(
    project: Option<&Project>,
    defaults: &[Id],
    docs: &[u16],
    connector: &HashSet<Vec<u16>>,
    connected: &[Id],
    work: &mut Work,
) -> R<Vec<Id>> {
    let mut out: Vec<Id> = project_ids(project, defaults, docs, work)?
        .into_iter()
        .filter(|id| id.as_ref().is_none_or(|s| !connector.contains(s)))
        .collect();
    work.charge(connected.len())?;
    out.extend(connected.iter().cloned());
    Ok(out)
}

/// A tool's policy mode as the JS compares it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Policy {
    Block,
    Ask,
    Other,
}

/// One tool of a discovered box: `isWriteTool(name)` (truthiness) and `policyMode(...)`.
#[derive(Clone, Copy, Debug)]
pub struct Tool {
    pub write: bool,
    pub policy: Policy,
}

/// One discovered box: its id, whether the account is signed in to it (only where the JS asked:
/// an OAuth box that is not a disconnected connector), and its named tools.
#[derive(Clone, Debug)]
pub struct Box {
    pub id: Vec<u16>,
    pub ready: Option<bool>,
    pub tools: Vec<Tool>,
}

/// The facts `computePermittedTools` reads.
#[derive(Clone, Debug, Default)]
pub struct Input {
    pub is_admin: bool,
    pub project: Option<Project>,
    pub cowork: bool,
    pub defaults: Vec<Id>,
    pub docs: Vec<u16>,
    pub connector: HashSet<Vec<u16>>,
    pub connected: Vec<Id>,
    pub oauth: HashSet<Vec<u16>>,
    pub diary_enabled: bool,
    pub harness_enabled: bool,
    pub has_repositories: bool,
    pub boxes: Vec<Box>,
    /// Configured boxes: `None` for a falsy entry, else its id (`None` inside for a non-string).
    pub manifest: Vec<Option<Id>>,
}

/// A tool's permission, most permissive first.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Permission {
    Allowed,
    NeedsApproval,
    Unavailable,
}

impl Permission {
    fn as_str(self) -> &'static str {
        match self {
            Permission::Allowed => "allowed",
            Permission::NeedsApproval => "needs-approval",
            Permission::Unavailable => "unavailable",
        }
    }
}

/// One tool's answer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ToolOut {
    pub permission: Permission,
    pub reason_code: Option<&'static str>,
}

/// One box's answer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BoxOut {
    pub id: Id,
    pub available: bool,
    pub reason_code: Option<&'static str>,
    pub active: bool,
    pub tools: Vec<ToolOut>,
}

/// `computePermittedTools(input)`: every discovered box, then every configured box that was not
/// discovered, then the coding harness.
pub fn permitted(input: &Input, work: &mut Work) -> R<Vec<BoxOut>> {
    let selected: HashSet<Vec<u16>> = selected_ids(
        input.project.as_ref(),
        &input.defaults,
        &input.docs,
        &input.connector,
        &input.connected,
        work,
    )?
    .into_iter()
    .flatten()
    .collect();
    let mut out = Vec::with_capacity(input.boxes.len() + input.manifest.len() + 1);
    for b in &input.boxes {
        work.charge(b.tools.len().saturating_add(b.id.len()))?;
        let reason: Option<&'static str> =
            if input.connector.contains(&b.id) && !has(&input.connected, &b.id) {
                Some("connect")
            } else if input.oauth.contains(&b.id) && !b.ready.ok_or(Refusal::Input)? {
                Some("signIn")
            } else if b.id.iter().copied().eq("diary".encode_utf16()) && !input.diary_enabled {
                Some("diaryOff")
            } else {
                None
            };
        let tools = b
            .tools
            .iter()
            .map(|t| {
                let permission = if reason.is_some() || t.policy == Policy::Block {
                    Permission::Unavailable
                } else if t.policy == Policy::Ask || t.write {
                    Permission::NeedsApproval
                } else {
                    Permission::Allowed
                };
                let reason_code = reason.or((t.policy == Policy::Block).then_some("blocked"));
                ToolOut {
                    permission,
                    reason_code,
                }
            })
            .collect();
        out.push(BoxOut {
            id: Some(b.id.clone()),
            available: reason.is_none(),
            reason_code: reason,
            active: reason.is_none() && selected.contains(&b.id),
            tools,
        });
    }
    let present: HashSet<&[u16]> = input.boxes.iter().map(|b| b.id.as_slice()).collect();
    for entry in &input.manifest {
        work.charge(0)?;
        let Some(id) = entry else { continue };
        if id.as_deref().is_some_and(|i| present.contains(i)) {
            continue;
        }
        if id
            .as_deref()
            .is_some_and(|i| i.iter().copied().eq("diary".encode_utf16()))
            && !input.diary_enabled
        {
            continue;
        }
        out.push(BoxOut {
            id: id.clone(),
            available: false,
            reason_code: Some("notConnected"),
            active: false,
            tools: Vec::new(),
        });
    }
    let code = if !input.cowork {
        Some("codeNeedsCowork")
    } else if !input.is_admin {
        Some("codeAdminOnly")
    } else if !input.harness_enabled {
        Some("codeOff")
    } else if input.project.is_none() {
        Some("codeNeedsProject")
    } else if !input.has_repositories {
        Some("codeNoRepository")
    } else {
        None
    };
    let tool = |write: bool| ToolOut {
        permission: if code.is_some() {
            Permission::Unavailable
        } else if write {
            Permission::NeedsApproval
        } else {
            Permission::Allowed
        },
        reason_code: code,
    };
    out.push(BoxOut {
        id: Some("code".encode_utf16().collect()),
        available: code.is_none(),
        reason_code: code,
        active: code.is_none(),
        tools: vec![tool(false), tool(true), tool(true)],
    });
    Ok(out)
}

// ── Wire ────────────────────────────────────────────────────────────────────

fn string(v: &Value) -> R<Vec<u16>> {
    v.as_str().map(<[u16]>::to_vec).ok_or(Refusal::Input)
}

fn ids(v: &Value, work: &mut Work) -> R<Vec<Id>> {
    let Value::Arr(items) = v else {
        return Err(Refusal::Input);
    };
    work.charge(items.len())?;
    items
        .iter()
        .map(|i| match i {
            Value::Str(s) => Ok(Some(s.clone())),
            Value::Null => Ok(None),
            _ => Err(Refusal::Input),
        })
        .collect()
}

fn set(v: &Value, work: &mut Work) -> R<HashSet<Vec<u16>>> {
    let Value::Arr(items) = v else {
        return Err(Refusal::Input);
    };
    work.charge(items.len())?;
    items.iter().map(string).collect()
}

fn boolean(v: Option<&Value>) -> R<bool> {
    match v {
        Some(Value::Bool(b)) => Ok(*b),
        _ => Err(Refusal::Input),
    }
}

fn get<'a>(v: &'a Value, k: &str) -> R<&'a Value> {
    v.get(k).ok_or(Refusal::Input)
}

fn project_of(v: &Value, work: &mut Work) -> R<Option<Project>> {
    match v {
        Value::Null => Ok(None),
        Value::Obj(_) => Ok(Some(Project {
            auto: boolean(v.get("auto"))?,
            docs_defaulted: boolean(v.get("docsDefaulted"))?,
            toolboxes: match get(v, "toolboxes")? {
                Value::Null => None,
                t => Some(ids(t, work)?),
            },
        })),
        _ => Err(Refusal::Input),
    }
}

fn input_of(v: &Value, work: &mut Work) -> R<Input> {
    let Value::Arr(raw_boxes) = get(v, "boxes")? else {
        return Err(Refusal::Input);
    };
    let mut boxes = Vec::with_capacity(raw_boxes.len());
    for b in raw_boxes {
        let Value::Arr(raw_tools) = get(b, "tools")? else {
            return Err(Refusal::Input);
        };
        work.charge(raw_tools.len())?;
        let mut tools = Vec::with_capacity(raw_tools.len());
        for t in raw_tools {
            let policy = match get(t, "policy")?.as_str() {
                Some(p) if p.iter().copied().eq("block".encode_utf16()) => Policy::Block,
                Some(p) if p.iter().copied().eq("ask".encode_utf16()) => Policy::Ask,
                Some(p) if p.iter().copied().eq("other".encode_utf16()) => Policy::Other,
                _ => return Err(Refusal::Input),
            };
            tools.push(Tool {
                write: boolean(t.get("write"))?,
                policy,
            });
        }
        boxes.push(Box {
            id: string(get(b, "id")?)?,
            ready: match get(b, "ready")? {
                Value::Null => None,
                Value::Bool(r) => Some(*r),
                _ => return Err(Refusal::Input),
            },
            tools,
        });
    }
    let Value::Arr(raw_manifest) = get(v, "manifest")? else {
        return Err(Refusal::Input);
    };
    work.charge(raw_manifest.len())?;
    let mut manifest = Vec::with_capacity(raw_manifest.len());
    for e in raw_manifest {
        manifest.push(match e {
            Value::Null => None,
            Value::Obj(_) => Some(match get(e, "id")? {
                Value::Str(s) => Some(s.clone()),
                Value::Null => None,
                _ => return Err(Refusal::Input),
            }),
            _ => return Err(Refusal::Input),
        });
    }
    Ok(Input {
        is_admin: boolean(v.get("isAdmin"))?,
        project: project_of(get(v, "project")?, work)?,
        cowork: boolean(v.get("cowork"))?,
        defaults: ids(get(v, "defaults")?, work)?,
        docs: string(get(v, "docsBox")?)?,
        connector: set(get(v, "connectorBoxes")?, work)?,
        connected: ids(get(v, "connected")?, work)?,
        oauth: set(get(v, "oauthServerIds")?, work)?,
        diary_enabled: boolean(v.get("diaryEnabled"))?,
        harness_enabled: boolean(v.get("harnessEnabled"))?,
        has_repositories: boolean(v.get("hasRepositories"))?,
        boxes,
        manifest,
    })
}

fn push_id(out: &mut Vec<u8>, id: &Id) {
    match id {
        Some(s) => json::push_str(out, s),
        None => out.extend_from_slice(b"null"),
    }
}

fn push_ids(out: &mut Vec<u8>, list: &[Id]) {
    out.extend_from_slice(b"{\"ids\":[");
    for (k, id) in list.iter().enumerate() {
        if k > 0 {
            out.push(b',');
        }
        push_id(out, id);
    }
    out.extend_from_slice(b"]}");
}

fn push_code(out: &mut Vec<u8>, code: Option<&str>) {
    match code {
        Some(c) => json::push_ascii(out, c),
        None => out.extend_from_slice(b"null"),
    }
}

fn run(op: u8, args: &[Value], work: &mut Work) -> R<Vec<u8>> {
    let mut out = Vec::new();
    match (op, args) {
        (1, [p, d, docs]) => {
            let project = project_of(p, work)?;
            let list = project_ids(project.as_ref(), &ids(d, work)?, &string(docs)?, work)?;
            push_ids(&mut out, &list);
        }
        (2, [p, d, docs, c, conn]) => {
            let project = project_of(p, work)?;
            let list = selected_ids(
                project.as_ref(),
                &ids(d, work)?,
                &string(docs)?,
                &set(c, work)?,
                &ids(conn, work)?,
                work,
            )?;
            push_ids(&mut out, &list);
        }
        (3, [i]) => {
            let boxes = permitted(&input_of(i, work)?, work)?;
            out.extend_from_slice(b"{\"boxes\":[");
            for (k, b) in boxes.iter().enumerate() {
                work.charge(b.tools.len())?;
                if k > 0 {
                    out.push(b',');
                }
                out.extend_from_slice(b"{\"id\":");
                push_id(&mut out, &b.id);
                out.extend_from_slice(if b.available {
                    b",\"state\":\"available\",\"reasonCode\":"
                } else {
                    b",\"state\":\"unavailable\",\"reasonCode\":"
                });
                push_code(&mut out, b.reason_code);
                out.extend_from_slice(if b.active {
                    b",\"active\":true,\"tools\":["
                } else {
                    b",\"active\":false,\"tools\":["
                });
                for (n, t) in b.tools.iter().enumerate() {
                    if n > 0 {
                        out.push(b',');
                    }
                    out.extend_from_slice(b"{\"permission\":");
                    json::push_ascii(&mut out, t.permission.as_str());
                    out.extend_from_slice(b",\"reasonCode\":");
                    push_code(&mut out, t.reason_code);
                    out.push(b'}');
                }
                out.extend_from_slice(b"]}");
            }
            out.extend_from_slice(b"]}");
        }
        _ => return Err(Refusal::Input),
    }
    Ok(out)
}

/// One request: `u8(op)` and UTF-8 JSON (an array of arguments). A project is `null` or
/// `{"auto","docsDefaulted","toolboxes":[id|null,…]|null}`; an id list holds strings and `null`
/// (any other value).
///
/// - op 1 `[project, defaults, docsBox]` → `{"ids":[…]}` (projectToolboxIds);
/// - op 2 `[project, defaults, docsBox, connectorBoxes, connected]` → `{"ids":[…]}`
///   (selectedToolboxIds);
/// - op 3 `[{"isAdmin","project","cowork","defaults","docsBox","connectorBoxes","connected",
///   "oauthServerIds","diaryEnabled","harnessEnabled","hasRepositories","boxes":[{"id","ready",
///   "tools":[{"write","policy":"block"|"ask"|"other"}]}],"manifest":[null|{"id"}]}]` →
///   `{"boxes":[{"id","state","reasonCode","active","tools":[{"permission","reasonCode"}]}]}`.
///
/// Status 0 and the reply, or status 1 and `{"error":"input"|"too_large"}`.
pub fn call(input: &[u8]) -> (u32, Vec<u8>) {
    let refuse = |r: Refusal| (1, r.json().as_bytes().to_vec());
    if input.len() > MAX_INPUT_BYTES {
        return refuse(Refusal::TooLarge);
    }
    let Some((&op, body)) = input.split_first() else {
        return refuse(Refusal::Input);
    };
    let mut work = Work::for_request(input.len());
    let Some(Value::Arr(args)) = json::parse_utf8(body, JSON_CAP) else {
        return refuse(Refusal::Input);
    };
    match run(op, &args, &mut work) {
        Ok(out) => (0, out),
        Err(e) => refuse(e),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
mod tests {
    use super::*;

    fn reply(op: u8, json: &str) -> String {
        let mut input = vec![op];
        input.extend(json.as_bytes());
        let (status, out) = call(&input);
        assert_eq!(status, 0, "{}", String::from_utf8_lossy(&out));
        String::from_utf8(out).unwrap()
    }

    #[test]
    fn project_lists() {
        assert_eq!(
            reply(1, r#"[null,["core","web"],"project-docs"]"#),
            r#"{"ids":["core","web"]}"#
        );
        assert_eq!(
            reply(
                1,
                r#"[{"auto":true,"docsDefaulted":true,"toolboxes":["project-docs","x"]},["core"],"project-docs"]"#
            ),
            r#"{"ids":["core","project-docs"]}"#
        );
        assert_eq!(
            reply(
                1,
                r#"[{"auto":false,"docsDefaulted":false,"toolboxes":["x",null]},["core"],"project-docs"]"#
            ),
            r#"{"ids":["x",null]}"#
        );
        assert_eq!(
            reply(
                2,
                r#"[null,["core","gmail"],"project-docs",["gmail"],["gmail"]]"#
            ),
            r#"{"ids":["core","gmail"]}"#
        );
    }

    #[test]
    fn ready_must_be_known() {
        let mut input = vec![3u8];
        input.extend(br#"[{"isAdmin":false,"project":null,"cowork":false,"defaults":[],"docsBox":"d","connectorBoxes":[],"connected":[],"oauthServerIds":["s"],"diaryEnabled":true,"harnessEnabled":false,"hasRepositories":false,"boxes":[{"id":"s","ready":null,"tools":[]}],"manifest":[]}]"#);
        assert_eq!(call(&input).0, 1);
    }
}
