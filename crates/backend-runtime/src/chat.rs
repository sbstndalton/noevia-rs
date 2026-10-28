//! Native bounded model/tool conversation loop. Tool execution uses scoped approvals and
//! a fresh persisted chat-context comparison before every side effect.
use crate::{
    approvals::{Approvals, Binding},
    state::Conversations,
    transport::{Endpoint, Transport},
    Control, Fault, Result,
};
use serde_json::{json, Value};
use std::{collections::BTreeMap, future::Future, pin::Pin, sync::Arc};
pub type FutureResult<'a, T> = Pin<Box<dyn Future<Output = Result<T>> + Send + 'a>>;
pub trait Model: Send + Sync {
    fn complete_sampled<'a>(
        &'a self,
        messages: &'a [Value],
        tools: &'a [Value],
        sampling: &'a Value,
        control: &'a Control,
        emit: &'a mut (dyn FnMut(Value) -> Result<()> + Send),
    ) -> FutureResult<'a, Value> {
        Box::pin(async move {
            if !sampling.as_object().is_some_and(serde_json::Map::is_empty) {
                return Err(Fault::Unavailable);
            }
            self.complete_stream(messages, tools, control, emit).await
        })
    }

    fn selection(&self) -> Option<(&str, &str)> {
        None
    }
    /// Trusted provider configuration, never a field accepted from model or public chat input.
    /// Unknown engines are external until the owning registry supplies their actual row.
    fn provider(&self) -> provider_egress::Provider {
        provider_egress::Provider {
            external: true,
            ..Default::default()
        }
    }
    fn complete_stream<'a>(
        &'a self,
        messages: &'a [Value],
        tools: &'a [Value],
        control: &'a Control,
        emit: &'a mut (dyn FnMut(Value) -> Result<()> + Send),
    ) -> FutureResult<'a, Value> {
        Box::pin(async move {
            let reply = self.complete(messages, tools, control).await?;
            if let Some(text) = reply.get("content").and_then(Value::as_str) {
                emit(json!({"type":"delta","text":text}))?;
            }
            Ok(reply)
        })
    }
    fn complete<'a>(
        &'a self,
        messages: &'a [Value],
        tools: &'a [Value],
        control: &'a Control,
    ) -> FutureResult<'a, Value>;
}
pub trait Tool: Send + Sync {
    /// Catalogue ownership is trusted registration metadata, never a model argument.
    fn toolbox(&self) -> Option<&str> {
        None
    }

    fn description(&self) -> &str {
        ""
    }
    fn permitted(&self, _binding: &Binding) -> bool {
        true
    }
    fn private(&self) -> bool {
        false
    }
    fn schema(&self) -> Value;
    /// Classification is configured by the trusted tool owner, never by model arguments.
    fn mutating(&self) -> bool;
    fn call<'a>(
        &'a self,
        binding: &'a Binding,
        arguments: Value,
        control: &'a Control,
    ) -> FutureResult<'a, Value>;
}
pub struct OpenAiModel {
    transport: Transport,
    endpoint: Endpoint,
    model: String,
    provider_id: String,
    provider: provider_egress::Provider,
}
impl OpenAiModel {
    pub fn new(endpoint: Endpoint, model: String) -> Result<Self> {
        if model.is_empty() || model.len() > 256 {
            return Err(Fault::Invalid);
        }
        Ok(Self {
            transport: Transport::new()?,
            endpoint,
            model,
            provider_id: "default".into(),
            provider: provider_egress::Provider {
                external: true,
                ..Default::default()
            },
        })
    }
    /// The default provider is trusted deployment configuration, as in the original
    /// workspace registry. Its actual endpoint still passes the native egress classifier.
    pub fn configured_default(
        endpoint: Endpoint,
        model: String,
        id: String,
        label: String,
    ) -> Result<Self> {
        if id.is_empty()
            || id.len() > 80
            || !id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
        {
            return Err(Fault::Invalid);
        }
        let provider = provider_egress::Provider {
            base_url: endpoint.url().as_str().encode_utf16().collect(),
            label: label.encode_utf16().collect(),
            ..Default::default()
        };
        let mut result = Self::new(endpoint, model)?;
        result.provider_id = id;
        result.provider = provider;
        Ok(result)
    }
}
impl Model for OpenAiModel {
    fn provider(&self) -> provider_egress::Provider {
        self.provider.clone()
    }
    fn selection(&self) -> Option<(&str, &str)> {
        Some((&self.provider_id, &self.model))
    }
    fn complete_stream<'a>(
        &'a self,
        messages: &'a [Value],
        tools: &'a [Value],
        control: &'a Control,
        emit: &'a mut (dyn FnMut(Value) -> Result<()> + Send),
    ) -> FutureResult<'a, Value> {
        Box::pin(async move {
            self.complete_sampled(messages, tools, &json!({}), control, emit)
                .await
        })
    }
    fn complete_sampled<'a>(
        &'a self,
        messages: &'a [Value],
        tools: &'a [Value],
        sampling: &'a Value,
        control: &'a Control,
        emit: &'a mut (dyn FnMut(Value) -> Result<()> + Send),
    ) -> FutureResult<'a, Value> {
        Box::pin(async move {
            let mut body = json!({"model":self.model,"messages":messages,"stream":true});
            let selected = crate::sampling::select(None, "", Some(sampling), false)?;
            body.as_object_mut()
                .ok_or(Fault::Protocol)?
                .extend(selected.as_object().ok_or(Fault::Protocol)?.clone());
            if !tools.is_empty() {
                body.as_object_mut()
                    .ok_or(Fault::Protocol)?
                    .insert("tools".into(), json!(tools));
            }
            let mut decoder = crate::model_stream::Decoder::default();
            self.transport
                .post_stream(
                    &self.endpoint,
                    serde_json::to_vec(&body)?,
                    control,
                    |chunk| {
                        decoder.feed(chunk, emit)?;
                        Ok(decoder.is_done())
                    },
                )
                .await?;
            decoder.finish()
        })
    }

    fn complete<'a>(
        &'a self,
        messages: &'a [Value],
        tools: &'a [Value],
        control: &'a Control,
    ) -> FutureResult<'a, Value> {
        Box::pin(async move {
            let mut body = json!({"model":self.model,"messages":messages,"stream":false});
            if !tools.is_empty() {
                body.as_object_mut()
                    .ok_or(Fault::Protocol)?
                    .insert("tools".to_owned(), Value::Array(tools.to_vec()));
            }
            let response = self
                .transport
                .request(
                    &self.endpoint,
                    reqwest::Method::POST,
                    reqwest::header::HeaderMap::new(),
                    Some(serde_json::to_vec(&body)?),
                    8 * 1024 * 1024,
                    control,
                )
                .await?;
            if !(200..300).contains(&response.status) {
                return Err(Fault::Transport);
            }
            let value: Value = serde_json::from_slice(&response.body)?;
            value
                .get("choices")
                .and_then(Value::as_array)
                .and_then(|v| v.first())
                .and_then(|v| v.get("message"))
                .cloned()
                .ok_or(Fault::Protocol)
        })
    }
}
struct TurnGuard {
    store: Arc<dyn Conversations>,
    user: String,
    id: String,
    finished: bool,
}
impl Drop for TurnGuard {
    fn drop(&mut self) {
        if !self.finished {
            let _ = self.store.fail(&self.user, &self.id, Fault::Cancelled);
        }
    }
}
pub struct Runtime {
    pub approvals: Approvals,
    pub store: Arc<dyn Conversations>,
    model: Arc<dyn Model>,
    tools: BTreeMap<String, Arc<dyn Tool>>,
}
impl Runtime {
    pub fn new<S: Conversations + 'static>(
        store: Arc<S>,
        model: Arc<dyn Model>,
        mut tools: BTreeMap<String, Arc<dyn Tool>>,
    ) -> Result<Self> {
        if store.supports_project_files() {
            tools.insert(
                "read_project_file".into(),
                Arc::new(crate::tools::ProjectFile {
                    store: store.clone(),
                }),
            );
        }
        if tools.len() > 128
            || tools.keys().any(|k| {
                k.is_empty()
                    || k.len() > 128
                    || !k
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
            })
        {
            return Err(Fault::Invalid);
        }
        Ok(Self {
            approvals: Approvals::default(),
            store,
            model,
            tools,
        })
    }
    /// Respect existing project choices; an unsupported pipeline never silently uses the global engine.
    pub fn validate_context(&self, binding: &Binding) -> Result<()> {
        let Some(project) = self.store.project(binding)? else {
            return Ok(());
        };
        if project.get("id").and_then(Value::as_str) == Some("cowork-diary-extras") {
            return Err(Fault::Forbidden);
        }
        if project.get("archived") == Some(&Value::Bool(true))
            || project
                .get("routing")
                .and_then(Value::as_str)
                .is_some_and(|r| r != "manual")
        {
            return Err(Fault::Unavailable);
        }
        if project
            .get("files")
            .is_some_and(|v| !v.is_null() && !v.as_array().is_some_and(Vec::is_empty))
        {
            if !self.store.supports_project_files()
                || !matches!(binding.holder, crate::approvals::Holder::Project { .. })
                || provider_egress::external_strict(Some(&self.model.provider()))
            {
                return Err(Fault::Unavailable);
            }
            #[cfg(target_os = "linux")]
            if !crate::project_sources::supported(&project) {
                return Err(Fault::Unavailable);
            }
            #[cfg(not(target_os = "linux"))]
            return Err(Fault::Unavailable);
        }
        for key in ["assets", "sourceFolders"] {
            if project
                .get(key)
                .is_some_and(|v| !v.is_null() && !v.as_array().is_some_and(Vec::is_empty))
            {
                return Err(Fault::Unavailable);
            }
        }
        for key in ["reasoningEffort", "routingMode", "contextProfile"] {
            if project.get(key).is_some_and(|v| !v.is_null()) {
                return Err(Fault::Unavailable);
            }
        }
        if project.pointer("/sharedContext/chat") == Some(&Value::Bool(true)) {
            return Err(Fault::Unavailable);
        }
        if project
            .get("toolboxes")
            .and_then(Value::as_array)
            .is_some_and(|v| v.iter().any(|x| x.as_str() != Some("core")))
        {
            return Err(Fault::Unavailable);
        }
        for (key, selected) in [
            ("provider", self.model.selection().map(|s| s.0)),
            ("model", self.model.selection().map(|s| s.1)),
        ] {
            if let Some(value) = project
                .get(key)
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
            {
                let value = if key == "provider" && value == "lemonade" {
                    selected.ok_or(Fault::Unavailable)?
                } else {
                    value
                };
                if Some(value) != selected {
                    return Err(Fault::Unavailable);
                }
            }
        }
        Ok(())
    }
    pub fn with_conversations(&self, store: Arc<dyn Conversations>) -> Self {
        let mut tools = self.tools.clone();
        if store.supports_project_files() {
            tools.insert(
                "read_project_file".into(),
                Arc::new(crate::tools::ProjectFile {
                    store: store.clone(),
                }),
            );
        } else {
            tools.remove("read_project_file");
        }
        Self {
            approvals: self.approvals.clone(),
            store,
            model: Arc::clone(&self.model),
            tools,
        }
    }
    /// `emit` is a bounded output sink supplied by the front; a disconnected sink returns
    /// an error and stops execution. No provider or tool is detached from this future.
    pub async fn run(
        &self,
        binding: Binding,
        id: &str,
        user_message: Value,
        control: &Control,
        emit: impl FnMut(Value) -> Result<()> + Send,
    ) -> Result<Vec<Value>> {
        self.run_browser(binding, id, user_message, None, control, emit)
            .await
    }
    pub async fn run_browser(
        &self,
        binding: Binding,
        id: &str,
        user_message: Value,
        browser_history: Option<&[Value]>,
        control: &Control,
        mut emit: impl FnMut(Value) -> Result<()> + Send,
    ) -> Result<Vec<Value>> {
        binding.validate()?;
        if user_message.get("role").and_then(Value::as_str) != Some("user")
            || !user_message
                .get("content")
                .is_some_and(|v| v.is_string() || v.is_array())
        {
            return Err(Fault::Invalid);
        }
        self.validate_context(&binding)?;
        let (revision, mut messages) = match browser_history {
            Some(history) => {
                self.store
                    .begin_browser(&binding, id, &user_message, history, control)?
            }
            None => self.store.begin(&binding, id, &user_message, control)?,
        };
        let mut guard = TurnGuard {
            store: Arc::clone(&self.store),
            user: binding.user.clone(),
            id: id.to_owned(),
            finished: false,
        };
        messages.push(user_message);
        let result = self
            .exchange(&binding, &mut messages, control, &mut emit)
            .await;
        match result {
            Ok(()) => {
                if let Err(e) = self
                    .store
                    .finish(&binding, id, &revision, &messages, control)
                {
                    let _ = self.store.fail(&binding.user, id, e);
                    return Err(e);
                }
                guard.finished = true;
                emit(match self.model.selection() {
                    Some((_, model)) => json!({"type":"done","model":model}),
                    None => json!({"type":"done"}),
                })?;
                Ok(messages)
            }
            Err(e) => {
                let _ = self.store.fail(&binding.user, id, e);
                Err(e)
            }
        }
    }
    async fn exchange(
        &self,
        binding: &Binding,
        messages: &mut Vec<Value>,
        control: &Control,
        emit: &mut (impl FnMut(Value) -> Result<()> + Send),
    ) -> Result<()> {
        let message = messages
            .iter()
            .rev()
            .find(|v| v.get("role").and_then(Value::as_str) == Some("user"))
            .and_then(|v| v.get("content"))
            .and_then(Value::as_str)
            .unwrap_or("");
        let project = self.store.project(binding)?;
        let sampling = crate::sampling::select(
            None,
            message,
            project.as_ref().and_then(|p| p.get("sampling")),
            self.store.sampling_enabled(binding)?,
        )?;
        let provider = self.model.provider();
        let external = provider_egress::external_strict(Some(&provider));
        let permitted = |name: &str, tool: &Arc<dyn Tool>| {
            tool.permitted(binding)
                && self
                    .store
                    .tool_mode(binding, name, tool.mutating())
                    .is_ok_and(|m| m != policy_leaves::Mode::Block)
                && !(external && (tool.private() || name.starts_with("diary_")))
                && match tool.toolbox() {
                    None => true,
                    Some(id) => self.store.project(binding).is_ok_and(|project| {
                        project.as_ref().is_none_or(|project| {
                            if project.get("toolsMode").and_then(Value::as_str) == Some("auto") {
                                return id == "core";
                            }
                            project
                                .get("toolboxes")
                                .and_then(Value::as_array)
                                .map_or(id == "core", |boxes| {
                                    boxes.iter().any(|v| v.as_str() == Some(id))
                                })
                        })
                    }),
                }
        };
        // Previously stored private results may not be replayed into a newly external engine.
        if external
            && messages
                .iter()
                .filter_map(|m| m.get("tool_calls").and_then(Value::as_array))
                .flatten()
                .any(|call| {
                    call.pointer("/function/name")
                        .and_then(Value::as_str)
                        .is_some_and(|name| {
                            name.starts_with("diary_")
                                || self.tools.get(name).is_none_or(|t| t.private())
                        })
                })
        {
            return Err(Fault::Forbidden);
        }
        let schemas:Vec<Value>=self.tools.iter().filter(|(name,tool)|permitted(name,tool)).map(|(name,tool)|json!({"type":"function","function":{"name":name,"description":tool.description(),"parameters":tool.schema()}})).collect();
        // Original tool-exchange contract: one cache per turn, canonical Rust keys,
        // serial execution, uncertain writes cached and reads invalidated only on write attempt.
        let mut results = BTreeMap::<Vec<u16>, (String, bool, bool, bool)>::new();
        let mut event_index = 0usize;
        for _round in 0..32 {
            control.check()?;
            if self
                .store
                .context(&binding.user, &binding.canonical_chat_id)?
                != *binding
            {
                return Err(Fault::Conflict);
            }
            if serde_json::to_vec(messages)?.len() > 8 * 1024 * 1024 {
                return Err(Fault::Capacity);
            }
            self.validate_context(binding)?;
            let mut framed = self.store.system(binding)?;
            framed.extend(self.store.sources(binding, control)?);
            framed.extend(messages.iter().cloned());
            if serde_json::to_vec(&framed)?.len() > 8 * 1024 * 1024 {
                return Err(Fault::Capacity);
            }
            let reply = self
                .model
                .complete_sampled(&framed, &schemas, &sampling, control, emit)
                .await?;
            if reply.get("role").and_then(Value::as_str) != Some("assistant") {
                return Err(Fault::Protocol);
            }
            let calls = match reply.get("tool_calls") {
                None | Some(Value::Null) => Vec::new(),
                Some(Value::Array(v)) if v.len() <= 32 => v.clone(),
                _ => return Err(Fault::Protocol),
            };
            messages.push(reply);
            if calls.is_empty() {
                return Ok(());
            }
            let mut ids = std::collections::HashSet::new();
            for call in calls {
                let index = event_index;
                event_index += 1;
                control.check()?;
                let id = call
                    .get("id")
                    .and_then(Value::as_str)
                    .filter(|id| !id.is_empty() && id.len() <= 256)
                    .ok_or(Fault::Protocol)?;
                if !ids.insert(id.to_owned())
                    || call.get("type").and_then(Value::as_str) != Some("function")
                {
                    return Err(Fault::Protocol);
                }
                let function = call.get("function").ok_or(Fault::Protocol)?;
                let name = function
                    .get("name")
                    .and_then(Value::as_str)
                    .ok_or(Fault::Protocol)?;
                let raw = function
                    .get("arguments")
                    .and_then(Value::as_str)
                    .ok_or(Fault::Protocol)?;
                if raw.len() > 1024 * 1024 {
                    return Err(Fault::Capacity);
                }
                let name_units: Vec<u16> = name.encode_utf16().collect();
                let raw_units: Vec<u16> = raw.encode_utf16().collect();
                let checked = tool_exchange::check(
                    false,
                    self.tools
                        .get(name)
                        .is_some_and(|tool| permitted(name, tool)),
                    &name_units,
                    if raw.is_empty() {
                        None
                    } else {
                        Some(&raw_units)
                    },
                )
                .map_err(|_| Fault::Protocol)?;
                emit(json!({"type":"tool","index":index,"name":name,"args":raw}))?;
                let mut applied = false;
                let mut declined = false;
                let mut not_run = false;
                let output = match checked {
                    tool_exchange::Check::Answer(answer) => {
                        not_run = true;
                        String::from_utf16(&answer).map_err(|_| Fault::Protocol)?
                    }
                    tool_exchange::Check::Run(key) => {
                        if let Some((text, _, saved_applied, saved_declined)) = results.get(&key) {
                            applied = *saved_applied;
                            declined = *saved_declined;
                            text.clone()
                        } else {
                            let arguments: Value = if raw.is_empty() {
                                json!({})
                            } else {
                                serde_json::from_str(raw)?
                            };
                            let tool = self.tools.get(name).ok_or(Fault::Forbidden)?;
                            if let Some(refusal) = provider_egress::tool_refusal(
                                Some(&provider),
                                &name_units,
                                &prompt_framing::json::Value::Str(raw_units.clone()),
                                None,
                            ) {
                                let text =
                                    String::from_utf16(&refusal).map_err(|_| Fault::Protocol)?;
                                results.insert(key, (text.clone(), false, false, false));
                                messages
                                    .push(json!({"role":"tool","tool_call_id":id,"content":text}));
                                emit(
                                    json!({"type":"tool_result","index":index,"name":name,"text":text.chars().take(300).collect::<String>(),"notRun":true}),
                                )?;
                                continue;
                            }
                            let permission =
                                self.store.tool_mode(binding, name, tool.mutating())?;
                            if permission == policy_leaves::Mode::Block {
                                return Err(Fault::Forbidden);
                            }
                            let allowed = if permission == policy_leaves::Mode::Ask
                                && !self.approvals.granted(binding)?
                            {
                                let ticket =
                                    self.approvals.ask(binding.clone(), control.remaining()?)?;
                                emit(
                                    json!({"type":"tool_pending","id":ticket.id,"index":index,"name":name,"args":raw}),
                                )?;
                                ticket.wait(control).await?
                            } else {
                                true
                            };
                            if self
                                .store
                                .context(&binding.user, &binding.canonical_chat_id)?
                                != *binding
                            {
                                return Err(Fault::Conflict);
                            }
                            control.check()?;
                            let latest = self.store.tool_mode(binding, name, tool.mutating())?;
                            if latest == policy_leaves::Mode::Block
                                || (permission != policy_leaves::Mode::Ask
                                    && latest == policy_leaves::Mode::Ask)
                            {
                                return Err(Fault::Forbidden);
                            }
                            let output = if allowed {
                                if tool.mutating() {
                                    results.retain(|_, (_, write, _, _)| *write);
                                }
                                match tool.call(binding, arguments, control).await {
                                    Ok(value) => {
                                        let text = match &value {
                                            Value::String(text) => text.clone(),
                                            _ => serde_json::to_string(&value)?,
                                        };
                                        // Original applied-write contract: confirmed call success,
                                        // never an uncertain transport failure or an ERROR result.
                                        let error_prefix =
                                            text.strip_prefix("ERROR").is_some_and(|rest| {
                                                rest.chars().next().is_none_or(|c| {
                                                    !c.is_ascii_alphanumeric() && c != '_'
                                                })
                                            });
                                        applied = tool.mutating()
                                            && !error_prefix
                                            && value.get("isError") != Some(&Value::Bool(true));
                                        text
                                    }
                                    Err(Fault::Cancelled) => return Err(Fault::Cancelled),
                                    Err(Fault::Deadline) => return Err(Fault::Deadline),
                                    Err(error) => {
                                        let message: Vec<u16> =
                                            error.to_string().encode_utf16().collect();
                                        String::from_utf16(
                                            &tool_exchange::call_error(&name_units, &message)
                                                .map_err(|_| Fault::Protocol)?,
                                        )
                                        .map_err(|_| Fault::Protocol)?
                                    }
                                }
                            } else {
                                declined = true;
                                "The user declined this action.".to_owned()
                            };
                            if output.len() > 1024 * 1024 {
                                return Err(Fault::Capacity);
                            }
                            results
                                .insert(key, (output.clone(), tool.mutating(), applied, declined));
                            output
                        }
                    }
                };
                if output.len() > 1024 * 1024 {
                    return Err(Fault::Capacity);
                }
                // Tool output is third-party data, even when the tool succeeded.
                // Refusals produced by our own gate remain trusted server messages.
                let for_model = if not_run || declined {
                    output.clone()
                } else {
                    String::from_utf16(&prompt_framing::framing::frame_untrusted(
                        &"tool result".encode_utf16().collect::<Vec<_>>(),
                        &name_units,
                        &output.encode_utf16().collect::<Vec<_>>(),
                    ))
                    .map_err(|_| Fault::Protocol)?
                };
                messages.push(json!({"role":"tool","tool_call_id":id,"content":for_model}));
                emit(
                    json!({"type":"tool_result","index":index,"name":name,"text":output.chars().take(300).collect::<String>(),"applied":applied,"declined":declined,"notRun":not_run}),
                )?;
            }
        }
        Err(Fault::Capacity)
    }
}

#[cfg(test)]
mod provider_tests {
    use super::*;
    #[test]
    fn only_configured_defaults_receive_original_provider_classification() -> Result<()> {
        for (url, external) in [
            ("http://127.0.0.1:1234/v1/chat/completions", false),
            ("https://integrate.api.nvidia.com/v1/chat/completions", true),
        ] {
            let origin = url::Url::parse(url)
                .map_err(|_| Fault::Invalid)?
                .origin()
                .ascii_serialization();
            let endpoint = Endpoint::new(url, &[origin], None)?;
            let unknown = OpenAiModel::new(endpoint.clone(), "synthetic".into())?;
            assert!(provider_egress::external_strict(Some(&unknown.provider())));
            let model = OpenAiModel::configured_default(
                endpoint,
                "synthetic".into(),
                "configured".into(),
                "Engine".into(),
            )?;
            assert_eq!(model.selection(), Some(("configured", "synthetic")));
            assert_eq!(
                provider_egress::external_strict(Some(&model.provider())),
                external
            );
        }
        Ok(())
    }
}
