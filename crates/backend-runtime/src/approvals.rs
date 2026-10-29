//! In-process write approval authority. Grants never survive restart or a context change.
use crate::{Control, Fault, Result};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use tokio::sync::oneshot;
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase", deny_unknown_fields)]
pub enum Holder {
    Free,
    Project { id: String },
    Absent,
}
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Binding {
    pub user: String,
    pub canonical_chat_id: String,
    pub scope: String,
    pub holder: Holder,
    pub generation: String,
    pub start: String,
    pub tombstoned: bool,
}
impl Binding {
    pub fn validate(&self) -> Result<()> {
        if self.user.is_empty()
            || self.scope.is_empty()
            || self.canonical_chat_id.is_empty()
            || self.canonical_chat_id.len() > 256
            || !self
                .canonical_chat_id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
            || self.generation.parse::<u64>().is_err()
            || self.generation.starts_with('0') && self.generation != "0"
            || self.start.len() != 32
            || !self
                .start
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
            || self.tombstoned
        {
            return Err(Fault::Forbidden);
        }
        if matches!(&self.holder, Holder::Project{id} if id.is_empty()) {
            return Err(Fault::Forbidden);
        }
        Ok(())
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    Approve,
    Deny,
    ApproveAll,
}
impl Decision {
    pub fn parse(value: &str) -> Result<Self> {
        match value {
            "approve" => Ok(Self::Approve),
            "deny" => Ok(Self::Deny),
            "approve_all" => Ok(Self::ApproveAll),
            _ => Err(Fault::Invalid),
        }
    }
}
struct Pending {
    binding: Binding,
    expires: Instant,
    sender: oneshot::Sender<bool>,
}
#[derive(Default)]
struct State {
    pending: HashMap<String, Pending>,
    grants: HashMap<(String, String), (Binding, Instant)>,
}
#[derive(Clone, Default)]
pub struct Approvals {
    state: Arc<Mutex<State>>,
}
pub struct Ticket {
    pub id: String,
    state: Arc<Mutex<State>>,
    receiver: Option<oneshot::Receiver<bool>>,
    expires: Instant,
}
impl Drop for Ticket {
    fn drop(&mut self) {
        if let Ok(mut state) = self.state.lock() {
            state.pending.remove(&self.id);
        }
    }
}
impl Ticket {
    pub async fn wait(mut self, control: &Control) -> Result<bool> {
        let mut receiver = self.receiver.take().ok_or(Fault::Conflict)?;
        loop {
            control.check()?;
            if Instant::now() >= self.expires {
                return Ok(false);
            }
            tokio::select! {
                result = &mut receiver => { control.check()?; return result.map_err(|_| Fault::Unavailable); },
                _ = tokio::time::sleep(control.remaining()?.min(Duration::from_millis(50))) => {}
            }
        }
    }
}
impl Approvals {
    pub fn granted(&self, binding: &Binding) -> Result<bool> {
        binding.validate()?;
        let mut state = self.state.lock().map_err(|_| Fault::Unavailable)?;
        let key = (binding.user.clone(), binding.canonical_chat_id.clone());
        match state.grants.get(&key) {
            Some((old, expiry)) if Instant::now() < *expiry && old == binding => Ok(true),
            Some(_) => {
                state.grants.remove(&key);
                Ok(false)
            }
            None => Ok(false),
        }
    }
    pub fn ask(&self, binding: Binding, timeout: Duration) -> Result<Ticket> {
        binding.validate()?;
        let expires = Instant::now()
            .checked_add(timeout.min(Duration::from_secs(300)))
            .ok_or(Fault::Invalid)?;
        let mut state = self.state.lock().map_err(|_| Fault::Unavailable)?;
        let now = Instant::now();
        state
            .pending
            .retain(|_, p| p.expires > now && !p.sender.is_closed());
        state.grants.retain(|_, (_, expiry)| *expiry > now);
        if state.pending.len() >= 1024 {
            return Err(Fault::Capacity);
        }
        let id = uuid::Uuid::new_v4().to_string();
        let (sender, receiver) = oneshot::channel();
        state.pending.insert(
            id.clone(),
            Pending {
                binding,
                expires,
                sender,
            },
        );
        Ok(Ticket {
            id,
            state: Arc::clone(&self.state),
            receiver: Some(receiver),
            expires,
        })
    }
    pub fn decide(&self, user: &str, id: &str, decision: Decision) -> Result<()> {
        let mut state = self.state.lock().map_err(|_| Fault::Unavailable)?;
        let now = Instant::now();
        let pending = state.pending.get(id).ok_or(Fault::Missing)?;
        if pending.binding.user != user || pending.expires <= now || pending.sender.is_closed() {
            return Err(Fault::Missing);
        }
        let pending = state.pending.remove(id).ok_or(Fault::Missing)?;
        if decision == Decision::ApproveAll && pending.binding.holder != Holder::Absent {
            if state.grants.len() >= 1024 {
                return Err(Fault::Capacity);
            }
            let key = (
                pending.binding.user.clone(),
                pending.binding.canonical_chat_id.clone(),
            );
            state
                .grants
                .insert(key, (pending.binding, now + Duration::from_secs(3600)));
        }
        pending
            .sender
            .send(decision != Decision::Deny)
            .map_err(|_| Fault::Missing)
    }
    pub fn revoke(&self, user: &str, chat: &str) -> Result<()> {
        let mut state = self.state.lock().map_err(|_| Fault::Unavailable)?;
        state.grants.remove(&(user.to_owned(), chat.to_owned()));
        state
            .pending
            .retain(|_, p| !(p.binding.user == user && p.binding.canonical_chat_id == chat));
        Ok(())
    }
}
