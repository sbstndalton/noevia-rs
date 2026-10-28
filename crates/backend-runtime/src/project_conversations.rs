//! Linux adapter to the existing native project authority. Histories stay in the
//! original files and use the original CAS, sanitization, journal and tombstone rules.
//! This adapter is not qualified on non-Linux hosts and has no fallback implementation.
use crate::{
    approvals::{Binding, Holder},
    journal::Journal,
    state::Conversations,
    Control, Fault, Result,
};
use serde_json::{json, Value};
use server_projects::{chat_move, transcript};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::Mutex,
};
pub struct ProjectConversations {
    data: PathBuf,
    start: String,
    journal: Journal,
    running: Mutex<HashMap<(String, String), Running>>,
}
#[derive(Clone)]
struct Running {
    id: String,
    base: Vec<Value>,
    user: Value,
    browser: bool,
    prefix: usize,
}
// Only ordinary text turns are admitted here. Applied/declined write records and
// failed read recovery require the original modelHistory projection, not a lossy cast.
fn text_projection(history: &[Value]) -> Result<Vec<Value>> {
    history
        .iter()
        .map(|m| {
            if !matches!(
                m.get("role").and_then(Value::as_str),
                Some("user" | "assistant")
            ) || !m.get("content").is_some_and(Value::is_string)
                || ["tool_calls", "error", "paused"]
                    .iter()
                    .any(|key| m.get(key).is_some())
            {
                return Err(Fault::Unavailable);
            }
            if let Some(calls) = m.get("toolCalls") {
                let calls = calls.as_array().ok_or(Fault::Unavailable)?;
                // Successful read chips add no modelHistory entries when an answer
                // exists. Applied writes, declines and interrupted reads need notes.
                if m.get("content")
                    .and_then(Value::as_str)
                    .is_none_or(|s| s.trim().is_empty())
                    || calls.iter().any(|c| {
                        c.get("applied") == Some(&Value::Bool(true))
                            || c.get("status").and_then(Value::as_str) == Some("denied")
                    })
                {
                    return Err(Fault::Unavailable);
                }
            }
            Ok(json!({"role":m.get("role"),"content":m.get("content")}))
        })
        .collect()
}
impl ProjectConversations {
    pub fn open(data: &Path, start: &str, journal_path: &Path) -> Result<Self> {
        if !data.is_absolute() || !server_projects::partial::valid_start_id(start) {
            return Err(Fault::Invalid);
        }
        Ok(Self {
            data: data.to_owned(),
            start: start.to_owned(),
            journal: Journal::open(journal_path)?,
            running: Mutex::new(HashMap::new()),
        })
    }
    fn begin_inner(
        &self,
        binding: &Binding,
        id: &str,
        request: &Value,
        control: &Control,
        browser: Option<&[Value]>,
    ) -> Result<(String, Vec<Value>)> {
        control.check()?;
        if self.context(&binding.user, &binding.canonical_chat_id)? != *binding {
            return Err(Fault::Conflict);
        }
        let mut running = self.running.lock().map_err(|_| Fault::Unavailable)?;
        let key = (binding.user.clone(), binding.canonical_chat_id.clone());
        if running.contains_key(&key) || running.len() >= 1024 {
            return Err(Fault::Conflict);
        }
        let current = transcript::read(&self.data, &binding.user, &binding.canonical_chat_id)
            .map_err(|_| Fault::Unavailable)?;
        let encoded = js_json::wtf16::stringify(&current.history).ok_or(Fault::Invalid)?;
        let mut history: Vec<Value> = serde_json::from_str(&encoded)?;
        let base = history.clone();
        if let Some(expected) = browser {
            let projection = text_projection(&history)?;
            let expected = text_projection(expected)?;
            if projection == expected {
                // The browser save has not arrived yet.
            } else if projection.len() == expected.len() + 1
                && projection.get(..expected.len()) == Some(expected.as_slice())
                && projection.last() == Some(request)
            {
                history.pop();
            } else {
                return Err(Fault::Conflict);
            }
        }
        // The model runtime cannot safely reinterpret non-array or lone-surrogate histories.
        // Refuse those inputs; original history routes still serve their original contract.
        self.journal.enqueue_id(
            id,
            "native_chat_turn",
            &json!({"binding":binding,"baseRevision":current.revision,"request":request}),
            &chrono::Local::now().format("%Y-%m-%dT%H:%M:%S").to_string(),
        )?;
        control.check()?;
        running.insert(
            key,
            Running {
                id: id.to_owned(),
                base,
                user: request.clone(),
                browser: browser.is_some(),
                prefix: history.len(),
            },
        );
        Ok((current.revision, history))
    }
    fn native(binding: &Binding) -> chat_move::GrantContext {
        chat_move::GrantContext {
            canonical: binding.canonical_chat_id.clone(),
            holder: match &binding.holder {
                Holder::Free => chat_move::Holder::Free,
                Holder::Project { id } => chat_move::Holder::Project(id.clone()),
                Holder::Absent => chat_move::Holder::Absent,
            },
            generation: binding.generation.clone(),
            start: binding.start.clone(),
            tombstoned: binding.tombstoned,
        }
    }
    fn release(&self, user: &str, id: &str) -> Result<()> {
        self.running
            .lock()
            .map_err(|_| Fault::Unavailable)?
            .retain(|(owner, _), turn| owner != user || turn.id != id);
        Ok(())
    }
}
impl Conversations for ProjectConversations {
    fn supports_project_files(&self) -> bool {
        true
    }
    fn sources(&self, binding: &Binding, control: &Control) -> Result<Vec<Value>> {
        let Some(project) = self.project(binding)? else {
            return Ok(Vec::new());
        };
        if !matches!(binding.holder, Holder::Project { .. }) {
            return Ok(Vec::new());
        }
        Ok(crate::project_sources::fallback(&self.data, &binding.user, &project, control)?.map(|text| json!({"role":"system","content":format!("Relevant knowledge-file excerpts for this message:\n{text}")})).into_iter().collect())
    }
    fn read_project_file(
        &self,
        binding: &Binding,
        args: &Value,
        control: &Control,
    ) -> Result<Value> {
        if !matches!(binding.holder, Holder::Project { .. }) {
            return Err(Fault::Unavailable);
        }
        let project = self.project(binding)?.ok_or(Fault::Missing)?;
        crate::project_sources::read(&self.data, &binding.user, &project, args, control)
    }

    fn project(&self, binding: &Binding) -> Result<Option<Value>> {
        if self.context(&binding.user, &binding.canonical_chat_id)? != *binding {
            return Err(Fault::Conflict);
        }
        let id = match &binding.holder {
            Holder::Project { id } => id.clone(),
            Holder::Free => format!("cowork-chat-context-{}", binding.canonical_chat_id),
            Holder::Absent => return Err(Fault::Missing),
        };
        let dir = server_projects::store::user_dir(&self.data, &binding.user)
            .map_err(|_| Fault::Unavailable)?;
        let projects = server_projects::store::read(&dir).map_err(|_| Fault::Unavailable)?;
        let Some(project) = server_projects::store::find(&projects, &id) else {
            return if matches!(binding.holder, Holder::Free) {
                Ok(None)
            } else {
                Err(Fault::Missing)
            };
        };
        let encoded = js_json::wtf16::stringify(project).ok_or(Fault::Invalid)?;
        Ok(Some(serde_json::from_str(&encoded)?))
    }

    fn context(&self, user: &str, chat: &str) -> Result<Binding> {
        let context = chat_move::grant_context(&self.data, user, chat, &self.start)
            .map_err(|_| Fault::Unavailable)?;
        let holder = match context.holder {
            chat_move::Holder::Free => Holder::Free,
            chat_move::Holder::Project(id) => Holder::Project { id },
            chat_move::Holder::Absent => return Err(Fault::Missing),
        };
        let scope = match &holder {
            Holder::Free => "free".to_owned(),
            Holder::Project { id } => format!("project:{id}"),
            Holder::Absent => return Err(Fault::Missing),
        };
        let binding = Binding {
            user: user.to_owned(),
            canonical_chat_id: context.canonical,
            scope,
            holder,
            generation: context.generation,
            start: context.start,
            tombstoned: context.tombstoned,
        };
        binding.validate()?;
        Ok(binding)
    }
    fn begin(
        &self,
        binding: &Binding,
        id: &str,
        request: &Value,
        control: &Control,
    ) -> Result<(String, Vec<Value>)> {
        self.begin_inner(binding, id, request, control, None)
    }
    fn begin_browser(
        &self,
        binding: &Binding,
        id: &str,
        request: &Value,
        history: &[Value],
        control: &Control,
    ) -> Result<(String, Vec<Value>)> {
        self.begin_inner(binding, id, request, control, Some(history))
    }
    fn finish(
        &self,
        binding: &Binding,
        id: &str,
        revision: &str,
        messages: &[Value],
        control: &Control,
    ) -> Result<()> {
        control.check()?;
        if binding.start != self.start {
            return Err(Fault::Conflict);
        }
        let turn = self
            .running
            .lock()
            .map_err(|_| Fault::Unavailable)?
            .get(&(binding.user.clone(), binding.canonical_chat_id.clone()))
            .filter(|turn| turn.id == id)
            .cloned()
            .ok_or(Fault::Conflict)?;
        let mut output = messages.to_vec();
        let mut base_revision = revision.to_owned();
        if turn.browser {
            let current = transcript::read(&self.data, &binding.user, &binding.canonical_chat_id)
                .map_err(|_| Fault::Unavailable)?;
            let encoded = js_json::wtf16::stringify(&current.history).ok_or(Fault::Invalid)?;
            let stored: Vec<Value> = serde_json::from_str(&encoded)?;
            if current.revision != revision {
                // Accept only the one queued browser user save. All preceding raw
                // metadata must still match. The final write CAS closes the read race.
                if stored.len() != turn.base.len() + 1
                    || stored.get(..turn.base.len()) != Some(turn.base.as_slice())
                    || text_projection(stored.get(turn.base.len()..).ok_or(Fault::Conflict)?)?
                        != vec![turn.user.clone()]
                {
                    return Err(Fault::Conflict);
                }
            }
            if stored.len() > output.len()
                || text_projection(&stored)?
                    != text_projection(output.get(..stored.len()).ok_or(Fault::Conflict)?)?
            {
                return Err(Fault::Conflict);
            }
            output
                .get_mut(..stored.len())
                .ok_or(Fault::Conflict)?
                .clone_from_slice(&stored);
            base_revision = current.revision;
        }
        let encoded = serde_json::to_string(&output)?;
        let value = js_json::wtf16::parse(&encoded).map_err(|_| Fault::Invalid)?;
        let js_json::JValue::Arr(history) = value else {
            return Err(Fault::Invalid);
        };
        let saved = chat_move::write_bound_transcript(
            &self.data,
            &binding.user,
            &Self::native(binding),
            &history,
            &transcript::Base::Revision(base_revision),
            || control.check().is_ok(),
        )
        .map_err(|_| Fault::Conflict)?;
        match saved {
            transcript::Saved::Saved { .. } => {
                self.journal.mark_applied(id)?;
                self.release(&binding.user, id)
            }
            transcript::Saved::Deleted | transcript::Saved::Conflict { .. } => Err(Fault::Conflict),
        }
    }
    fn finish_tools(
        &self,
        binding: &Binding,
        id: &str,
        revision: &str,
        messages: &[Value],
        tools: &[Value],
        control: &Control,
    ) -> Result<()> {
        let prefix = self
            .running
            .lock()
            .map_err(|_| Fault::Unavailable)?
            .get(&(binding.user.clone(), binding.canonical_chat_id.clone()))
            .filter(|turn| turn.id == id)
            .map(|turn| turn.prefix)
            .ok_or(Fault::Conflict)?;
        let mut history = messages
            .get(..prefix.checked_add(1).ok_or(Fault::Capacity)?)
            .ok_or(Fault::Protocol)?
            .to_vec();
        let final_message = messages.last().ok_or(Fault::Protocol)?;
        if final_message.get("role").and_then(Value::as_str) != Some("assistant") {
            return Err(Fault::Protocol);
        }
        let mut reply = json!({"role":"assistant","content":final_message.get("content").and_then(Value::as_str).unwrap_or("")});
        if !tools.is_empty() {
            reply
                .as_object_mut()
                .ok_or(Fault::Protocol)?
                .insert("toolCalls".into(), Value::Array(tools.to_vec()));
        }
        history.push(reply);
        self.finish(binding, id, revision, &history, control)
    }
    fn fail(&self, user: &str, id: &str, fault: Fault) -> Result<()> {
        self.release(user, id)?;
        // A crash or lost post-commit acknowledgment is inspectable, never auto-replayed.
        self.journal
            .quarantine(id, &format!("quarantined: native turn {fault:?}"))
    }
}
