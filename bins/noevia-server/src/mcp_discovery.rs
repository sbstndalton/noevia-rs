//! What only Node's running MCP client knows, published for the front (sbstndalton/noevia#2202
//! round 2): the toolboxes this deployment offers right now with their tools, the MCP servers' last
//! discovery (a tool count, the first tools, the error), the configured boxes and two facts of the
//! chat process (the coding harness is on; a repository is registered).
//!
//! Node's MCP client stays in Node (a chat's live sessions are chat-bound), so the tool lists per
//! server come from `mcp-discovery.json`, written atomically by core `mcp-discovery-snapshot.cjs`
//! while someone asks (the demand marker, snapshot.rs). A snapshot that is not provably current
//! (missing, corrupt, older than two intervals) is **not ready**: the caller answers "being prepared"
//! (503), never an old catalogue. Everything per account (permissions, sign-ins, Drive, the project's
//! selection) is read by the front itself at the time of the request, so none of it waits for Node.
use crate::{snapshot, App};
use serde_json::Value;
use std::sync::Arc;

pub const SNAPSHOT_FILE: &str = "mcp-discovery.json";
pub const SNAPSHOT_KIND: &str = "mcp-discovery";

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
    Some(Catalogue {
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

/// The current catalogue, asking Node to publish if it has stopped (the demand marker is touched; a
/// request that finds Node idle waits a moment for the first file). `None`: not ready.
pub async fn read(app: &Arc<App>) -> Option<Arc<Catalogue>> {
    let dir = app.config.data_dir.clone()?;
    let file = dir.join(SNAPSHOT_FILE);
    app.demand
        .read(dir, move |now| {
            let snap = snapshot::read(&file, SNAPSHOT_KIND, now).ok()?;
            parse(&snap.body).map(Arc::new)
        })
        .await
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    fn body() -> Value {
        serde_json::json!({
            "mcpEnabled": true, "harnessEnabled": false, "hasRepositories": true,
            "nextcloudConfigured": true, "nextcloudOrigins": ["https://cloud.example.test"],
            "defaults": ["core"], "connectorBoxes": ["gdrive"],
            "boxes": [{"id": "core", "label": "Core", "description": "d", "source": "builtin", "inApp": true,
                "tools": [{"name": "get_current_time", "description": "t", "write": false}]},
                {"id": "dir-x", "label": "X", "description": "", "source": "mcp", "inApp": false, "server": "dir-x",
                "tools": []}],
            "manifest": [{"id": "files", "label": "Files", "description": "", "inApp": true}],
            "servers": [{"id": "dir-x", "auth": "oauth", "directory": true, "error": "down", "toolCount": 0,
                "tools": [{"name": "a", "description": "b"}], "toolsTruncated": false}],
        })
    }

    #[test]
    fn the_published_shape_is_read() {
        let c = parse(&body()).unwrap();
        assert_eq!(c.boxes.len(), 2);
        assert_eq!(c.boxes[0].tools[0].name, "get_current_time");
        assert_eq!(c.boxes[1].server.as_deref(), Some("dir-x"));
        assert_eq!(c.server("dir-x").unwrap().error.as_deref(), Some("down"));
        assert_eq!(c.server("dir-x").unwrap().tools, [("a".into(), "b".into())]);
        assert!(c.has_repositories && !c.harness_enabled);
    }

    #[test]
    fn anything_of_another_shape_is_not_a_catalogue() {
        for drop in [
            "boxes",
            "manifest",
            "servers",
            "defaults",
            "connectorBoxes",
            "mcpEnabled",
            "harnessEnabled",
            "hasRepositories",
            "nextcloudConfigured",
            "nextcloudOrigins",
        ] {
            let mut b = body();
            b.as_object_mut().unwrap().remove(drop);
            assert!(parse(&b).is_none(), "{drop}");
        }
        let mut b = body();
        b["boxes"][0]["tools"][0]["write"] = serde_json::json!("yes");
        assert!(parse(&b).is_none(), "a write flag that is not a boolean");
        let mut b = body();
        b["boxes"][0]["id"] = serde_json::json!(5);
        assert!(parse(&b).is_none());
    }
}
