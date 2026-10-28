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
    running: Mutex<HashMap<(String, String), String>>,
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
            .retain(|(owner, _), turn| owner != user || turn != id);
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
        let history: Vec<Value> = serde_json::from_str(&encoded)?;
        // The model runtime cannot safely reinterpret non-array or lone-surrogate histories.
        // Refuse those inputs; original history routes still serve their original contract.
        self.journal.enqueue_id(
            id,
            "native_chat_turn",
            &json!({"binding":binding,"baseRevision":current.revision,"request":request}),
            &chrono::Local::now().format("%Y-%m-%dT%H:%M:%S").to_string(),
        )?;
        control.check()?;
        running.insert(key, id.to_owned());
        Ok((current.revision, history))
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
        if self
            .running
            .lock()
            .map_err(|_| Fault::Unavailable)?
            .get(&(binding.user.clone(), binding.canonical_chat_id.clone()))
            .map(String::as_str)
            != Some(id)
        {
            return Err(Fault::Conflict);
        }
        let encoded = serde_json::to_string(messages)?;
        let value = js_json::wtf16::parse(&encoded).map_err(|_| Fault::Invalid)?;
        let js_json::JValue::Arr(history) = value else {
            return Err(Fault::Invalid);
        };
        let saved = chat_move::write_bound_transcript(
            &self.data,
            &binding.user,
            &Self::native(binding),
            &history,
            &transcript::Base::Revision(revision.to_owned()),
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
    fn fail(&self, user: &str, id: &str, fault: Fault) -> Result<()> {
        self.release(user, id)?;
        // A crash or lost post-commit acknowledgment is inspectable, never auto-replayed.
        self.journal
            .quarantine(id, &format!("quarantined: native turn {fault:?}"))
    }
}
