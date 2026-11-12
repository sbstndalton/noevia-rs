// Exact catalogue types and parser from L Rust 58c7d626 (no runtime reader).
#![allow(dead_code)]
use serde_json::Value;
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
    /// Every discovered tool name with the servers that offer it, in declaration order (core's
    /// `mcpState.offers`). A publisher that does not write it publishes no offers.
    pub offers: std::collections::HashMap<String, Vec<String>>,
}

impl Catalogue {
    pub fn server(&self, id: &str) -> Option<&ServerInfo> {
        self.servers.iter().find(|s| s.id == id)
    }
}

fn text(v: &Value, key: &str) -> Option<String> {
    v.get(key).and_then(Value::as_str).map(str::to_owned)
}

fn flag(v: &Value, key: &str) -> Option<bool> {
    v.get(key).and_then(Value::as_bool)
}

fn list<'a>(v: &'a Value, key: &str) -> Option<&'a Vec<Value>> {
    v.get(key).and_then(Value::as_array)
}

/// The snapshot's body as a catalogue, or `None` when it is not of the shape the publisher writes.
pub fn parse(body: &Value) -> Option<Catalogue> {
    let boxes = list(body, "boxes")?
        .iter()
        .map(|b| {
            let tools = list(b, "tools")?
                .iter()
                .map(|t| {
                    Some(ToolInfo {
                        name: text(t, "name")?,
                        description: text(t, "description").unwrap_or_default(),
                        write: flag(t, "write")?,
                    })
                })
                .collect::<Option<Vec<_>>>()?;
            Some(BoxInfo {
                id: text(b, "id")?,
                label: text(b, "label").unwrap_or_default(),
                description: text(b, "description").unwrap_or_default(),
                source: text(b, "source").unwrap_or_default(),
                in_app: flag(b, "inApp").unwrap_or(false),
                server: text(b, "server"),
                tools,
            })
        })
        .collect::<Option<Vec<_>>>()?;
    let manifest = list(body, "manifest")?
        .iter()
        .map(|m| {
            Some(ManifestBox {
                id: text(m, "id")?,
                label: text(m, "label").unwrap_or_default(),
                description: text(m, "description").unwrap_or_default(),
                in_app: flag(m, "inApp").unwrap_or(false),
            })
        })
        .collect::<Option<Vec<_>>>()?;
    let servers = list(body, "servers")?
        .iter()
        .map(|s| {
            Some(ServerInfo {
                id: text(s, "id")?,
                auth: text(s, "auth").unwrap_or_default(),
                directory: flag(s, "directory").unwrap_or(false),
                error: text(s, "error"),
                tool_count: s.get("toolCount").and_then(Value::as_i64),
                tools: list(s, "tools")
                    .map(|tools| {
                        tools
                            .iter()
                            .filter_map(|t| Some((text(t, "name")?, text(t, "description")?)))
                            .collect()
                    })
                    .unwrap_or_default(),
                tools_truncated: flag(s, "toolsTruncated").unwrap_or(false),
            })
        })
        .collect::<Option<Vec<_>>>()?;
    let strings = |key: &str| -> Option<Vec<String>> {
        list(body, key)?
            .iter()
            .map(|v| v.as_str().map(str::to_owned))
            .collect()
    };
    let offers = match body.get("offers") {
        Some(Value::Object(map)) => map
            .iter()
            .map(|(name, servers)| {
                let ids = servers
                    .as_array()?
                    .iter()
                    .map(|v| v.as_str().map(str::to_owned))
                    .collect::<Option<Vec<_>>>()?;
                Some((name.clone(), ids))
            })
            .collect::<Option<_>>()?,
        Some(Value::Null) | None => Default::default(),
        _ => return None,
    };
    Some(Catalogue {
        offers,
        mcp_enabled: flag(body, "mcpEnabled")?,
        harness_enabled: flag(body, "harnessEnabled")?,
        has_repositories: flag(body, "hasRepositories")?,
        nextcloud_configured: flag(body, "nextcloudConfigured")?,
        nextcloud_origins: strings("nextcloudOrigins")?,
        defaults: strings("defaults")?,
        connector_boxes: strings("connectorBoxes")?,
        boxes,
        manifest,
        servers,
    })
}
