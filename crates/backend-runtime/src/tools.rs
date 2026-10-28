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

/// Original core current-time tool: timezone-aware dates and computed ISO calendar facts.
pub struct CurrentTime;
impl CurrentTime {
    pub fn at(now: chrono::DateTime<chrono::Utc>, arguments: &Value) -> Value {
        let zone = arguments
            .get("timezone")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty());
        let (formatted, day) = if let Some(zone) = zone {
            let Ok(tz) = zone.parse::<chrono_tz::Tz>() else {
                return json!(format!(
                    "ERROR: unknown IANA timezone \"{}\"",
                    zone.chars().take(64).collect::<String>()
                ));
            };
            let local = now.with_timezone(&tz);
            (
                format!("{} ({zone})", local.format("%A, %-d %B %Y at %H:%M:%S %Z")),
                local.date_naive(),
            )
        } else {
            let local = now.with_timezone(&chrono::Local);
            (
                local.format("%a %b %d %Y %H:%M:%S GMT%z (%Z)").to_string(),
                local.date_naive(),
            )
        };
        let system = crate::calendar::system_part(day);
        let facts = system.lines().nth(1).unwrap_or("");
        json!(format!(
            "Current time: {formatted} | ISO: {} | {facts}",
            now.to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
        ))
    }
}
impl Tool for CurrentTime {
    fn toolbox(&self) -> Option<&str> {
        Some("core")
    }
    fn description(&self) -> &str {
        "Get the current date and time on the server, optionally in a specific IANA timezone. Use whenever freshness, today, or a timezone matters."
    }
    fn schema(&self) -> Value {
        json!({"type":"object","properties":{"timezone":{"type":"string","description":"Optional IANA timezone name"}},"required":[]})
    }
    fn mutating(&self) -> bool {
        false
    }
    fn call<'a>(
        &'a self,
        _: &'a Binding,
        arguments: Value,
        control: &'a Control,
    ) -> FutureResult<'a, Value> {
        Box::pin(async move {
            control.check()?;
            Ok(Self::at(chrono::Utc::now(), &arguments))
        })
    }
}
pub fn core_tools() -> std::collections::BTreeMap<String, Arc<dyn Tool>> {
    std::collections::BTreeMap::from([(
        "get_current_time".to_owned(),
        Arc::new(CurrentTime) as Arc<dyn Tool>,
    )])
}
/// Existing project source reads stay bound to the authenticated conversation adapter.
pub struct ProjectFile {
    pub store: Arc<dyn crate::state::Conversations>,
}
impl Tool for ProjectFile {
    fn toolbox(&self) -> Option<&str> {
        Some("core")
    }
    fn description(&self) -> &str {
        "Read an attached project text file by its stored name or an unambiguous trailing path."
    }
    fn permitted(&self, binding: &Binding) -> bool {
        matches!(binding.holder, crate::approvals::Holder::Project { .. })
    }
    fn private(&self) -> bool {
        true
    }
    fn schema(&self) -> Value {
        json!({"type":"object","properties":{"name":{"type":"string"}},"required":["name"]})
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
        Box::pin(async move { self.store.read_project_file(binding, &arguments, control) })
    }
}

#[cfg(test)]
mod time_tests {
    #[test]
    fn original_tool_dates_follow_requested_zone_and_reject_unknown(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let now = "2027-01-01T00:30:00Z".parse::<chrono::DateTime<chrono::Utc>>()?;
        let west =
            super::CurrentTime::at(now, &serde_json::json!({"timezone":"America/Los_Angeles"}));
        let text = west.as_str().ok_or("text")?;
        assert!(text.contains("31 December 2026"));
        assert!(text.contains("day 365 of 365 in 2026, 0 days left"));
        let east = super::CurrentTime::at(now, &serde_json::json!({"timezone":"Asia/Tokyo"}));
        assert!(east
            .as_str()
            .ok_or("text")?
            .contains("ISO week 53 of 2026 (the ISO year is not the calendar year 2027)"));
        assert!(
            super::CurrentTime::at(now, &serde_json::json!({"timezone":"unknown/zone"}))
                .as_str()
                .ok_or("text")?
                .starts_with("ERROR: unknown IANA timezone")
        );
        Ok(())
    }
}
