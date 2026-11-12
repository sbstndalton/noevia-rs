//! The toolbox catalogue (sbstndalton/noevia row T): the toolboxes this deployment offers right now with
//! their tools, the MCP servers' last discovery (a tool count, the first tools, the error), the
//! configured boxes and two facts of the chat process (the coding harness is on; a repository is
//! registered).
//!
//! **MCP discovery is this process's** (`mcp_discovery_actor.rs`): the tool lists, the bound curated
//! boxes and each server's last outcome are read from the actor, never from a file Node wrote. What only
//! the chat process knows - the built-in boxes with their tool definitions, the connector boxes, the
//! coding-harness facts and the prefill measurements - is the file `toolbox-chat-facts.json`, written
//! atomically by core `toolbox-chat-facts.cjs` while someone asks (the demand marker, snapshot.rs). A
//! facts file that is not provably current (missing, corrupt, older than two intervals), or a discovery
//! that has not produced its first answer, is **not ready**: the caller answers "being prepared"
//! (503), never an old catalogue. Everything per account (permissions, sign-ins, Drive, the project's
//! selection) is read by the front itself at the time of the request.
//!
//! [`compose`] is core `mcp-discovery-snapshot.cjs` `buildCatalogue` over those two sources, and
//! [`picker_facts`] is the facts document the toolbox picker was built from when Node answered
//! `/internal/toolbox-picker/facts` (that bridge is gone).
use crate::mcp_discovery_actor::Snapshot;
use crate::mcp_discovery_backend::Handle;
use crate::{snapshot, App};
use js_json::{wtf16, JValue};
use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

pub const FACTS_FILE: &str = "toolbox-chat-facts.json";
pub const FACTS_KIND: &str = "toolbox-chat-facts";
/// How long a reader waits for a discovery in progress before it uses what is known (core
/// `DISCOVERY_WAIT_MS`).
const DISCOVERY_WAIT: Duration = Duration::from_millis(1500);
/// Tools carried per box (core `MAX_BOX_TOOLS`).
const MAX_BOX_TOOLS: usize = 500;

/// One tool of an offered box.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolInfo {
    pub name: String,
    /// Cut at 240 characters by the publisher (core `computePermittedTools`).
    pub description: String,
    /// core `isWriteTool(name)`.
    pub write: bool,
}

/// One offered toolbox (core `allToolboxes()`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BoxInfo {
    pub id: String,
    pub label: String,
    pub description: String,
    pub source: String,
    pub in_app: bool,
    /// The MCP server the box belongs to, if it is an MCP box.
    pub server: Option<String>,
    pub tools: Vec<ToolInfo>,
}

/// A configured MCP box (the curated manifest), shown even when its server is down.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManifestBox {
    pub id: String,
    pub label: String,
    pub description: String,
    pub in_app: bool,
}

/// One MCP server's last discovery.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerInfo {
    pub id: String,
    pub auth: String,
    pub directory: bool,
    pub error: Option<String>,
    pub tool_count: Option<i64>,
    /// `(name, description)`, at most 40, cut as the admin page shows them.
    pub tools: Vec<(String, String)>,
    pub tools_truncated: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Catalogue {
    pub mcp_enabled: bool,
    pub harness_enabled: bool,
    pub has_repositories: bool,
    /// Whether any configured MCP server authenticates as `nextcloud`.
    pub nextcloud_configured: bool,
    /// `MCP_NEXTCLOUD_ORIGINS`, as the operator declared them (a credential goes only to one of them).
    pub nextcloud_origins: Vec<String>,
    pub defaults: Vec<String>,
    pub connector_boxes: Vec<String>,
    pub boxes: Vec<BoxInfo>,
    pub manifest: Vec<ManifestBox>,
    pub servers: Vec<ServerInfo>,
    /// All native offers, ordered and deduplicated per tool, independent of presentation limits.
    pub offers: std::collections::HashMap<String, Vec<String>>,
}

impl Catalogue {
    pub fn server(&self, id: &str) -> Option<&ServerInfo> {
        self.servers.iter().find(|s| s.id == id)
    }
}

fn cut(value: &JValue, max: usize) -> String {
    let text = match value {
        JValue::Str(s) => s.clone(),
        JValue::Undefined | JValue::Null => String::new(),
        other => js_json::to_js_string(other).unwrap_or_default(),
    };
    js_json::js_slice(&text, max)
}

fn strings(v: &JValue) -> Vec<String> {
    match v {
        JValue::Arr(items) => items
            .iter()
            .filter_map(|x| x.as_str().map(str::to_owned))
            .collect(),
        _ => Vec::new(),
    }
}

/// A box of either source, as the composition reads it.
struct View<'a> {
    id: String,
    label: &'a JValue,
    description: &'a JValue,
    source: String,
    in_app: &'a JValue,
    server: Option<String>,
    reads: Vec<String>,
    tools: &'a [JValue],
}

fn view(b: &JValue) -> Option<View<'_>> {
    let JValue::Obj(_) = b else {
        return None;
    };
    Some(View {
        id: b.get("id").as_str()?.to_owned(),
        label: b.get("label"),
        description: b.get("description"),
        source: b.get("source").as_str().unwrap_or_default().to_owned(),
        in_app: b.get("inApp"),
        server: b
            .get("server")
            .as_str()
            .filter(|s| !s.is_empty())
            .map(str::to_owned),
        reads: strings(b.get("reads")),
        tools: match b.get("tools") {
            JValue::Arr(t) => t,
            _ => &[],
        },
    })
}

fn tool_name(t: &JValue) -> Option<&str> {
    t.get("function")
        .get("name")
        .as_str()
        .filter(|n| !n.is_empty())
}

/// core `allToolboxes()`: the chat process's built-in boxes, then the discovered MCP boxes this
/// deployment offers.
fn all_boxes<'a>(
    facts: &'a JValue,
    snap: &'a Snapshot,
    offered: &dyn Fn(&str) -> bool,
) -> Option<Vec<View<'a>>> {
    let JValue::Arr(builtin) = facts.get("builtin") else {
        return None;
    };
    let mut out: Vec<View<'a>> = builtin.iter().map(view).collect::<Option<_>>()?;
    out.extend(
        snap.boxes
            .iter()
            .filter_map(view)
            .filter(|b| offered(&b.id)),
    );
    Some(out)
}

/// core `isWriteTool`: a tool is a write unless some box declares it a read and no box lists it
/// without declaring it one; a read the MCP server says writes is a write.
fn write_names<'a>(boxes: &'a [View<'a>], snap: &'a Snapshot) -> impl Fn(&str) -> bool + 'a {
    let mut writes: HashSet<&str> = HashSet::new();
    for b in boxes {
        let reads: HashSet<&str> = b.reads.iter().map(String::as_str).collect();
        for t in b.tools {
            if let Some(name) = tool_name(t) {
                if !reads.contains(name) {
                    writes.insert(name);
                }
            }
        }
    }
    let mut read_only: HashSet<&str> = HashSet::new();
    for b in boxes {
        for n in &b.reads {
            if !writes.contains(n.as_str()) {
                read_only.insert(n.as_str());
            }
        }
    }
    let known: std::collections::HashMap<&str, Option<bool>> = snap
        .tools()
        .into_iter()
        .map(|e| (e.name.as_str(), e.read_only))
        .collect();
    move |name| {
        if !read_only.contains(name) {
            return true;
        }
        // The server's positive "this is not read-only" is believed over our manifest.
        known.get(name).copied().flatten() == Some(false)
    }
}

/// core `buildCatalogue`: the catalogue from the chat process's facts and the front's discovery.
pub fn compose(
    facts: &JValue,
    snap: &Snapshot,
    offered: &dyn Fn(&str) -> bool,
    nextcloud_origins: &[String],
) -> Option<Catalogue> {
    let boxes_in = all_boxes(facts, snap, offered)?;
    let is_write = write_names(&boxes_in, snap);
    let boxes = boxes_in
        .iter()
        .map(|b| BoxInfo {
            id: b.id.clone(),
            label: cut(b.label, 200),
            description: cut(b.description, 1000),
            source: b.source.clone(),
            in_app: b.source == "builtin" || b.in_app.as_bool() == Some(true),
            server: b.server.clone(),
            tools: b
                .tools
                .iter()
                .filter(|t| tool_name(t).is_some())
                .take(MAX_BOX_TOOLS)
                .map(|t| ToolInfo {
                    name: cut(t.get("function").get("name"), 128),
                    description: cut(t.get("function").get("description"), 240),
                    write: is_write(tool_name(t).unwrap_or_default()),
                })
                .collect(),
        })
        .collect();
    let manifest = if snap.enabled {
        snap.manifest
            .iter()
            .filter(|e| e.get("id").as_str().is_some_and(offered))
            .map(|e| ManifestBox {
                id: e.get("id").as_str().unwrap_or_default().to_owned(),
                label: if e.get("label").truthy() {
                    cut(e.get("label"), 200)
                } else {
                    cut(e.get("id"), 200)
                },
                description: cut(e.get("description"), 1000),
                in_app: true,
            })
            .collect()
    } else {
        Vec::new()
    };
    let servers = snap
        .servers
        .iter()
        .map(|f| {
            let box_ = f
                .spec
                .directory
                .then(|| {
                    snap.boxes.iter().find(|b| {
                        b.get("directory").as_bool() == Some(true)
                            && b.get("server").as_str() == Some(f.spec.id.as_str())
                            && b.get("id").as_str() == Some(f.spec.id.as_str())
                    })
                })
                .flatten();
            let all: &[JValue] = match box_.map(|b| b.get("tools")) {
                Some(JValue::Arr(t)) => t,
                _ => &[],
            };
            let tools: Vec<(String, String)> = all
                .iter()
                .take(40)
                .map(|t| {
                    (
                        cut(t.get("function").get("name"), 120),
                        cut(t.get("function").get("description"), 240),
                    )
                })
                .collect();
            ServerInfo {
                id: f.spec.id.clone(),
                auth: f.spec.auth.clone(),
                directory: f.spec.directory,
                error: f
                    .error
                    .as_deref()
                    .filter(|e| !e.is_empty())
                    .map(|e| js_json::js_slice(e, 1000)),
                tool_count: Some(f.tool_count as i64),
                tools_truncated: all.len() > tools.len(),
                tools,
            }
        })
        .collect();
    let mut offers: std::collections::HashMap<String, Vec<String>> = Default::default();
    for entry in &snap.offers {
        let ids = offers.entry(entry.name.clone()).or_default();
        if !ids.contains(&entry.server) {
            ids.push(entry.server.clone());
        }
    }
    Some(Catalogue {
        offers,
        mcp_enabled: snap.enabled,
        harness_enabled: facts.get("harnessEnabled").as_bool()?,
        has_repositories: facts.get("hasRepositories").as_bool()?,
        nextcloud_configured: snap.servers.iter().any(|f| f.spec.auth == "nextcloud"),
        nextcloud_origins: nextcloud_origins.to_vec(),
        defaults: strings(facts.get("defaults")),
        connector_boxes: strings(facts.get("connectorBoxes")),
        boxes,
        manifest,
        servers,
    })
}

/// The facts the picker is composed from (`server_projects::toolbox_picker::compose`): every offered box
/// with its tool definitions, the connector boxes, the prefill measurements and the MCP status.
pub fn picker_facts(
    facts: &JValue,
    snap: &Snapshot,
    offered: &dyn Fn(&str) -> bool,
) -> Option<JValue> {
    let boxes_in = all_boxes(facts, snap, offered)?;
    let boxes: Vec<JValue> = boxes_in
        .iter()
        .map(|b| {
            let mut row = vec![
                ("id".to_owned(), JValue::Str(b.id.clone())),
                ("label".to_owned(), b.label.clone()),
                ("description".to_owned(), b.description.clone()),
                ("source".to_owned(), JValue::Str(b.source.clone())),
            ];
            if !matches!(b.in_app, JValue::Undefined) {
                row.push(("inApp".to_owned(), b.in_app.clone()));
            }
            row.push(("tools".to_owned(), JValue::Arr(b.tools.to_vec())));
            JValue::Obj(row)
        })
        .collect();
    let str_or_null = |v: &Option<String>| v.clone().map_or(JValue::Null, JValue::Str);
    let mcp = JValue::obj([
        ("enabled", JValue::Bool(snap.enabled)),
        ("error", str_or_null(&snap.error)),
        (
            "tools",
            JValue::Arr(
                snap.tools()
                    .into_iter()
                    .map(|e| {
                        JValue::obj([
                            ("name", JValue::Str(e.name.clone())),
                            ("serverId", JValue::Str(e.server.clone())),
                        ])
                    })
                    .collect(),
            ),
        ),
        (
            "states",
            JValue::Arr(
                snap.servers
                    .iter()
                    .map(|f| {
                        JValue::obj([
                            ("id", JValue::Str(f.spec.id.clone())),
                            ("error", str_or_null(&f.error)),
                            ("toolCount", JValue::Num(f.tool_count as f64)),
                            ("discoveredAt", JValue::Num(f.discovered_at as f64)),
                        ])
                    })
                    .collect(),
            ),
        ),
        (
            "servers",
            JValue::Arr(
                snap.servers
                    .iter()
                    .map(|f| {
                        let mut row = vec![
                            ("id", JValue::Str(f.spec.id.clone())),
                            ("auth", JValue::Str(f.spec.auth.clone())),
                            ("directory", JValue::Bool(f.spec.directory)),
                        ];
                        if let Some(t) = &f.spec.title {
                            row.push(("title", JValue::Str(t.clone())));
                        }
                        JValue::obj(row)
                    })
                    .collect(),
            ),
        ),
        (
            "manifest",
            JValue::Arr(
                snap.manifest
                    .iter()
                    .map(|b| {
                        JValue::obj([
                            ("server", b.get("server").clone()),
                            ("tools", b.get("tools").clone()),
                        ])
                    })
                    .collect(),
            ),
        ),
    ]);
    Some(JValue::obj([
        ("boxes", JValue::Arr(boxes)),
        ("connectorBoxes", facts.get("connectorBoxes").clone()),
        ("prefill", facts.get("prefill").clone()),
        ("mcp", mcp),
    ]))
}

/// The chat process's published facts, proven current (touching the demand marker, and waiting a
/// moment for the first file after an idle spell). `None`: not ready.
pub async fn chat_facts(app: &Arc<App>) -> Option<Arc<JValue>> {
    let dir = app.config.data_dir.clone()?;
    let file = dir.join(FACTS_FILE);
    app.demand
        .read(dir, move |now| {
            let snap = snapshot::read(&file, FACTS_KIND, now).ok()?;
            let text = std::str::from_utf8(&snap.raw).ok()?;
            wtf16::parse(text).ok().map(Arc::new)
        })
        .await
}

/// What discovery knows, asking it for a refresh if it is due and waiting no longer than
/// [`DISCOVERY_WAIT`] for one in progress. `None`: discovery has not produced its first answer, or
/// cannot answer (a failed run).
pub async fn discovered(handle: &Handle) -> Option<Arc<Snapshot>> {
    let snap = match tokio::time::timeout(DISCOVERY_WAIT, handle.source.discover(false)).await {
        Ok(Ok(snap)) => snap,
        Ok(Err(_)) => return None,
        // A slow server: the catalogue as it stands, the next one carries the result.
        Err(_) => handle.source.current(),
    };
    (snap.generation > 0).then_some(snap)
}

/// The current catalogue. `None`: not ready.
pub async fn read(app: &Arc<App>) -> Option<Arc<Catalogue>> {
    let facts = chat_facts(app).await?;
    let handle = crate::mcp_discovery_backend::handle(app)?;
    let snap = discovered(handle).await?;
    compose(
        &facts,
        &snap,
        &|id| handle.offered(id),
        &handle.nextcloud_origins,
    )
    .map(Arc::new)
}

/// The picker's facts document. `None`: not ready.
pub async fn picker(app: &Arc<App>) -> Option<JValue> {
    let facts = chat_facts(app).await?;
    let handle = crate::mcp_discovery_backend::handle(app)?;
    let snap = discovered(handle).await?;
    picker_facts(&facts, &snap, &|id| handle.offered(id))
}
