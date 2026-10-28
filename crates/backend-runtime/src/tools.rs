//! Concrete native tool adapters. Registration is an owner decision; tool annotations
//! from an MCP server cannot silently turn a mutating tool into a read-only one.
use crate::{
    approvals::Binding,
    chat::{FutureResult, Tool},
    diary::Corpus,
    mcp::Session,
    Control, Fault,
};
use serde_json::{json, Value};
use std::sync::Arc;
use tokio::sync::Mutex;
pub struct McpTool {
    pub user: String,
    pub scope: String,
    pub session: Arc<Mutex<Session>>,
    pub name: String,
    pub parameters: Value,
    pub writes: bool,
}
impl Tool for McpTool {
    fn permitted(&self, binding: &Binding) -> bool {
        binding.user == self.user && binding.scope == self.scope
    }
    // Remote tool data is private until its trusted catalogue classifies it otherwise.
    fn private(&self) -> bool {
        true
    }
    fn schema(&self) -> Value {
        self.parameters.clone()
    }
    fn mutating(&self) -> bool {
        self.writes
    }
    fn call<'a>(
        &'a self,
        binding: &'a Binding,
        arguments: Value,
        control: &'a Control,
    ) -> FutureResult<'a, Value> {
        Box::pin(async move {
            if binding.user != self.user || binding.scope != self.scope {
                return Err(Fault::Forbidden);
            }
            let permit = tokio::time::timeout(control.remaining()?, self.session.lock())
                .await
                .map_err(|_| Fault::Deadline)?;
            let mut session = permit;
            control.check()?;
            session.initialize(control).await?;
            session.call(&self.name, arguments, control).await
        })
    }
}
/// Bound at registration to one authenticated tenant. The public/model arguments contain
/// only a workspace-relative path; they cannot choose the tenant or database location.
pub struct DiaryRead {
    pub corpus: Arc<Corpus>,
    pub user: String,
}
impl Tool for DiaryRead {
    fn permitted(&self, binding: &Binding) -> bool {
        binding.user == self.user && self.user == self.corpus.tenant_id()
    }
    fn private(&self) -> bool {
        true
    }
    fn schema(&self) -> Value {
        json!({"type":"object","properties":{"path":{"type":"string"}},"required":["path"],"additionalProperties":false})
    }
    fn mutating(&self) -> bool {
        false
    }
    fn call<'a>(
        &'a self,
        binding: &'a Binding,
        arguments: Value,
        control: &'a Control,
    ) -> FutureResult<'a, Value> {
        Box::pin(async move {
            if binding.user != self.user {
                return Err(Fault::Forbidden);
            }
            let path = arguments
                .get("path")
                .and_then(Value::as_str)
                .ok_or(Fault::Invalid)?;
            let value = self.corpus.read(path, control)?;
            match value {
                Some(file) => Ok(json!({"content":file.content,"version":file.version})),
                None => Err(Fault::Missing),
            }
        })
    }
}
