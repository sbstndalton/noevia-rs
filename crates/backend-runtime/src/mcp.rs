//! Native MCP HTTP session, initialization, tools/list and tools/call.
//! Neither transport nor server tool annotations grant execution/write authority.
use crate::{
    transport::{Endpoint, Transport},
    Control, Fault, Result,
};
use reqwest::{
    header::{HeaderMap, HeaderValue},
    Method,
};
use serde_json::{json, Value};
use std::sync::atomic::{AtomicU64, Ordering};
pub struct Session {
    endpoint: Endpoint,
    transport: Transport,
    session: Option<HeaderValue>,
    next: AtomicU64,
    initialized: bool,
}
impl Session {
    pub fn new(endpoint: Endpoint) -> Result<Self> {
        Ok(Self {
            endpoint,
            transport: Transport::new()?,
            session: None,
            next: AtomicU64::new(1),
            initialized: false,
        })
    }
    fn headers(&self) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(
            "accept",
            HeaderValue::from_static("application/json, text/event-stream"),
        );
        headers.insert(
            "mcp-protocol-version",
            HeaderValue::from_static("2025-03-26"),
        );
        if let Some(value) = &self.session {
            headers.insert("mcp-session-id", value.clone());
        }
        headers
    }
    async fn rpc(&mut self, method: &str, params: Value, control: &Control) -> Result<Value> {
        let id = self
            .next
            .try_update(Ordering::AcqRel, Ordering::Acquire, |n| n.checked_add(1))
            .map_err(|_| Fault::Capacity)?;
        let id = id.to_string();
        let body =
            serde_json::to_vec(&json!({"jsonrpc":"2.0","id":id,"method":method,"params":params}))?;
        let response = self
            .transport
            .request(
                &self.endpoint,
                Method::POST,
                self.headers(),
                Some(body),
                8 * 1024 * 1024,
                control,
            )
            .await?;
        if !(200..300).contains(&response.status) {
            return Err(Fault::Transport);
        }
        if let Some(session) = response.headers.get("mcp-session-id") {
            if session.as_bytes().len() > 1024
                || self.session.as_ref().is_some_and(|old| old != session)
            {
                return Err(Fault::Protocol);
            }
            self.session = Some(session.clone());
        }
        let text = std::str::from_utf8(&response.body).map_err(|_| Fault::Protocol)?;
        let units: Vec<u16> = text.encode_utf16().collect();
        let event_stream = response
            .headers
            .get("content-type")
            .and_then(|h| h.to_str().ok())
            .is_some_and(|h| h.split(';').next() == Some("text/event-stream"));
        let outcome = mcp_frame::parse_rpc_body(
            event_stream,
            &units,
            &mcp_frame::Expected::String(id.encode_utf16().collect()),
        )
        .map_err(|_| Fault::Capacity)?;
        let mcp_frame::Outcome::Reply(span) = outcome else {
            return Err(Fault::Protocol);
        };
        let selected = String::from_utf16(units.get(span).ok_or(Fault::Protocol)?)
            .map_err(|_| Fault::Protocol)?;
        let value: Value = serde_json::from_str(&selected)?;
        if value.get("jsonrpc").and_then(Value::as_str) != Some("2.0")
            || value.get("error").is_some()
        {
            return Err(Fault::Protocol);
        }
        value.get("result").cloned().ok_or(Fault::Protocol)
    }
    pub async fn initialize(&mut self, control: &Control) -> Result<()> {
        if self.initialized {
            return Ok(());
        }
        let reply=self.rpc("initialize",json!({"protocolVersion":"2025-03-26","capabilities":{},"clientInfo":{"name":"noevia","version":"0.1.0"}}),control).await?;
        if reply.get("protocolVersion").and_then(Value::as_str) != Some("2025-03-26")
            || !reply.get("capabilities").is_some_and(Value::is_object)
        {
            return Err(Fault::Protocol);
        }
        let response = self
            .transport
            .request(
                &self.endpoint,
                Method::POST,
                self.headers(),
                Some(serde_json::to_vec(
                    &json!({"jsonrpc":"2.0","method":"notifications/initialized"}),
                )?),
                1024,
                control,
            )
            .await?;
        if !(200..300).contains(&response.status) {
            return Err(Fault::Transport);
        }
        self.initialized = true;
        Ok(())
    }
    pub async fn list_tools(&mut self, control: &Control) -> Result<Vec<Value>> {
        self.initialize(control).await?;
        let mut tools = Vec::new();
        let mut cursor = None::<String>;
        let mut seen = std::collections::HashSet::new();
        for _ in 0..32 {
            let params = cursor
                .as_ref()
                .map_or_else(|| json!({}), |c| json!({"cursor":c}));
            let reply = self.rpc("tools/list", params, control).await?;
            let page = reply
                .get("tools")
                .and_then(Value::as_array)
                .ok_or(Fault::Protocol)?;
            if tools.len().saturating_add(page.len()) > 1024 {
                return Err(Fault::Capacity);
            }
            for tool in page {
                let name = tool
                    .get("name")
                    .and_then(Value::as_str)
                    .ok_or(Fault::Protocol)?;
                if name.is_empty()
                    || name.len() > 256
                    || !tool.get("inputSchema").is_some_and(Value::is_object)
                    || !seen.insert(name.to_owned())
                {
                    return Err(Fault::Protocol);
                }
                tools.push(tool.clone());
            }
            match reply.get("nextCursor") {
                None | Some(Value::Null) => return Ok(tools),
                Some(Value::String(next))
                    if !next.is_empty() && next.len() <= 4096 && cursor.as_ref() != Some(next) =>
                {
                    cursor = Some(next.clone())
                }
                _ => return Err(Fault::Protocol),
            }
        }
        Err(Fault::Capacity)
    }
    /// The caller must already have authorized this exact tool and arguments.
    pub async fn call(&mut self, name: &str, arguments: Value, control: &Control) -> Result<Value> {
        if !self.initialized || name.is_empty() || !arguments.is_object() {
            return Err(Fault::Invalid);
        }
        self.rpc(
            "tools/call",
            json!({"name":name,"arguments":arguments}),
            control,
        )
        .await
    }
    pub async fn close(&mut self, control: &Control) -> Result<()> {
        if self.session.is_none() {
            self.initialized = false;
            return Ok(());
        }
        let result = self
            .transport
            .request(
                &self.endpoint,
                Method::DELETE,
                self.headers(),
                None,
                1024,
                control,
            )
            .await;
        self.session = None;
        self.initialized = false;
        match result {
            Ok(r) if (200..300).contains(&r.status) || r.status == 404 || r.status == 405 => Ok(()),
            Ok(_) => Err(Fault::Transport),
            Err(e) => Err(e),
        }
    }
}
