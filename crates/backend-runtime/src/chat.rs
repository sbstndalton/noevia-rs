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
    /// Trusted provider configuration, never a field accepted from model or public chat input.
    /// Unknown engines are external until the owning registry supplies their actual row.
    fn provider(&self) -> provider_egress::Provider {
        provider_egress::Provider {
            external: true,
            ..Default::default()
        }
    }
    fn complete<'a>(
        &'a self,
        messages: &'a [Value],
        tools: &'a [Value],
        control: &'a Control,
    ) -> FutureResult<'a, Value>;
}
pub trait Tool: Send + Sync {
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
        })
    }
}
impl Model for OpenAiModel {
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
        tools: BTreeMap<String, Arc<dyn Tool>>,
    ) -> Result<Self> {
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
    /// `emit` is a bounded output sink supplied by the front; a disconnected sink returns
    /// an error and stops execution. No provider or tool is detached from this future.
    pub async fn run(
        &self,
        binding: Binding,
        id: &str,
        user_message: Value,
        control: &Control,
        mut emit: impl FnMut(Value) -> Result<()>,
    ) -> Result<Vec<Value>> {
        binding.validate()?;
        if user_message.get("role").and_then(Value::as_str) != Some("user")
            || !user_message
                .get("content")
                .is_some_and(|v| v.is_string() || v.is_array())
        {
            return Err(Fault::Invalid);
        }
        let (revision, mut messages) = self.store.begin(&binding, id, &user_message, control)?;
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
                emit(json!({"type":"done"}))?;
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
        emit: &mut impl FnMut(Value) -> Result<()>,
    ) -> Result<()> {
        let provider = self.model.provider();
        let external = provider_egress::external_strict(Some(&provider));
        let permitted = |name: &str, tool: &Arc<dyn Tool>| {
            tool.permitted(binding) && !(external && (tool.private() || name.starts_with("diary_")))
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
        let schemas:Vec<Value>=self.tools.iter().filter(|(name,tool)|permitted(name,tool)).map(|(name,tool)|json!({"type":"function","function":{"name":name,"parameters":tool.schema()}})).collect();
        // Original tool-exchange contract: one cache per turn, canonical Rust keys,
        // serial execution, uncertain writes cached and reads invalidated only on write attempt.
        let mut results = BTreeMap::<Vec<u16>, (String, bool)>::new();
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
            let reply = self.model.complete(messages, &schemas, control).await?;
            if reply.get("role").and_then(Value::as_str) != Some("assistant") {
                return Err(Fault::Protocol);
            }
            let calls = match reply.get("tool_calls") {
                None | Some(Value::Null) => Vec::new(),
                Some(Value::Array(v)) if v.len() <= 32 => v.clone(),
                _ => return Err(Fault::Protocol),
            };
            if let Some(text) = reply.get("content").and_then(Value::as_str) {
                emit(json!({"type":"delta","text":text}))?;
            }
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
                let output = match checked {
                    tool_exchange::Check::Answer(answer) => {
                        String::from_utf16(&answer).map_err(|_| Fault::Protocol)?
                    }
                    tool_exchange::Check::Run(key) => {
                        if let Some((text, _)) = results.get(&key) {
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
                                results.insert(key, (text.clone(), false));
                                messages
                                    .push(json!({"role":"tool","tool_call_id":id,"content":text}));
                                emit(
                                    json!({"type":"tool_result","index":index,"name":name,"text":text.chars().take(300).collect::<String>()}),
                                )?;
                                continue;
                            }
                            let allowed = if tool.mutating() && !self.approvals.granted(binding)? {
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
                            let output = if allowed {
                                if tool.mutating() {
                                    results.retain(|_, (_, write)| *write);
                                }
                                match tool.call(binding, arguments, control).await {
                                    Ok(Value::String(text)) => text,
                                    Ok(value) => serde_json::to_string(&value)?,
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
                                "The user declined this action.".to_owned()
                            };
                            if output.len() > 1024 * 1024 {
                                return Err(Fault::Capacity);
                            }
                            results.insert(key, (output.clone(), tool.mutating()));
                            output
                        }
                    }
                };
                if output.len() > 1024 * 1024 {
                    return Err(Fault::Capacity);
                }
                messages.push(json!({"role":"tool","tool_call_id":id,"content":output}));
                emit(
                    json!({"type":"tool_result","index":index,"name":name,"text":output.chars().take(300).collect::<String>()}),
                )?;
            }
        }
        Err(Fault::Capacity)
    }
}
