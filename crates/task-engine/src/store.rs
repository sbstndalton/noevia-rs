//! The job store: one append-only journal per job, kept in memory. The host owns the clock, the
//! job ids and the file I/O; this module only decides whether a write is allowed.

use std::collections::BTreeMap;

use serde_json::{json, Value};

use code_actions::{approval_for, Action};

use crate::error::EngineError;
use crate::event::{
    check_event, encoded_len, valid_job_id, Event, APPROVAL_ID_KEY, AUTHORITY_TYPES,
    MAX_EVENTS_PER_JOB,
};
use crate::fold::{Fold, JobView, Status};

/// Count and size limits on the store (jobs.cjs `MAX_*` and the Code limits).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    pub max_jobs: usize,
    pub max_capabilities: usize,
    pub max_capability_bytes: usize,
    pub max_card_bytes: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Limits {
            max_jobs: 4096,
            max_capabilities: 32,
            max_capability_bytes: 64,
            max_card_bytes: 64 * 1024,
        }
    }
}

/// The decisions a harness can answer an approval with (code-harness.cjs `askApproval`, plus the
/// automatic `denied` it records itself).
pub const DECISIONS: [&str; 6] = [
    "approve",
    "approve_all",
    "deny",
    "denied",
    "timeout",
    "aborted",
];

/// What a new job is created with (jobs.cjs `create`).
#[derive(Debug, Clone, Copy)]
pub struct NewJob<'a> {
    pub kind: &'a str,
    pub project_id: Option<&'a str>,
    pub parent_id: Option<&'a str>,
    pub capabilities: &'a [String],
}

#[derive(Debug, Clone)]
struct Journal {
    events: Vec<Event>,
    fold: Fold,
}

/// The job store. Journals are keyed by job id, so listing is in id order; [`JobStore::list`] sorts
/// by creation time the way jobs.cjs does.
#[derive(Debug, Clone)]
pub struct JobStore {
    limits: Limits,
    journals: BTreeMap<String, Journal>,
}

impl JobStore {
    pub fn new(limits: Limits) -> Self {
        JobStore {
            limits,
            journals: BTreeMap::new(),
        }
    }

    /// Creates a job under a host-chosen id, which must be valid and unused.
    pub fn create(&mut self, id: &str, at: u64, job: NewJob<'_>) -> Result<&JobView, EngineError> {
        if !valid_job_id(id) {
            return Err(EngineError::BadJobId);
        }
        if self.journals.contains_key(id) {
            return Err(EngineError::BadInput("job id is already in use"));
        }
        if self.journals.len() >= self.limits.max_jobs {
            return Err(EngineError::Limit("the store holds too many jobs"));
        }
        if job.kind.is_empty() {
            return Err(EngineError::BadInput("job kind required"));
        }
        let capabilities = self.checked_capabilities(job.capabilities)?;
        if let Some(parent) = job.parent_id {
            let parent = self.journals.get(parent).ok_or(EngineError::NoSuchParent)?;
            let held = &parent.fold.view().capabilities;
            if capabilities.iter().any(|c| !held.contains(c)) {
                return Err(EngineError::WidenCapabilities);
            }
        }
        let data = json!({
            "kind": job.kind,
            "projectId": job.project_id,
            "parentId": job.parent_id,
            "capabilities": capabilities,
        });
        let event = Event {
            job: id.to_string(),
            seq: 1,
            kind: "job.created".into(),
            at,
            data,
        };
        let fold = Fold::begin(&event)?;
        let journal = Journal {
            events: vec![event],
            fold,
        };
        Ok(self
            .journals
            .entry(id.to_string())
            .or_insert(journal)
            .fold
            .view())
    }

    /// Appends one event, after the same checks the Node store makes.
    pub fn append(
        &mut self,
        id: &str,
        at: u64,
        kind: &str,
        data: Value,
    ) -> Result<&JobView, EngineError> {
        if !valid_job_id(id) {
            return Err(EngineError::BadJobId);
        }
        if kind == "job.created" {
            return Err(EngineError::BadInput("create a job with create()"));
        }
        check_event(kind, &data)?;
        let journal = self.journals.get_mut(id).ok_or(EngineError::NoSuchJob)?;
        let event = Event {
            job: id.to_string(),
            seq: journal.fold.view().event_count + 1,
            kind: kind.to_string(),
            at,
            data,
        };
        let next = journal.fold.next(&event)?;
        journal.events.push(event);
        journal.fold = next;
        Ok(journal.fold.view())
    }

    /// `job.started`: a queued job begins. The task lifecycle moves to `implementing`.
    pub fn start(&mut self, id: &str, at: u64) -> Result<&JobView, EngineError> {
        self.append(id, at, "job.started", json!({}))
    }

    /// `approval.requested`: a running job waits for an answer. `approval_id` is the host's id; the
    /// answer must repeat it. It is stored in the event under [`APPROVAL_ID_KEY`].
    pub fn request_approval(
        &mut self,
        id: &str,
        at: u64,
        approval_id: &str,
        card: Value,
    ) -> Result<&JobView, EngineError> {
        if approval_id.is_empty() || approval_id.len() > 64 {
            return Err(EngineError::BadInput("approval id is empty or too long"));
        }
        let mut card = card;
        let object = card
            .as_object_mut()
            .ok_or(EngineError::BadInput("an approval card is an object"))?;
        object.insert(APPROVAL_ID_KEY.into(), Value::String(approval_id.into()));
        if encoded_len(&card)? > self.limits.max_card_bytes {
            return Err(EngineError::TooLarge("approval card is too large"));
        }
        self.append(id, at, "approval.requested", card)
    }

    /// `approval.requested` for a classified action (code-actions). An action that never needs an
    /// approval is refused, and the card's `action` is the classifier's name, whatever the caller put there.
    pub fn request_action_approval(
        &mut self,
        id: &str,
        at: u64,
        approval_id: &str,
        action: Action,
        card: Value,
    ) -> Result<&JobView, EngineError> {
        if approval_for(action) == "never" {
            return Err(EngineError::BadInput("this action never needs an approval"));
        }
        let mut card = card;
        let object = card
            .as_object_mut()
            .ok_or(EngineError::BadInput("an approval card is an object"))?;
        object.insert("action".into(), Value::String(action.name().into()));
        self.request_approval(id, at, approval_id, card)
    }

    /// `approval.decided`: answers the pending approval `approval_id`. The job runs again.
    pub fn decide_approval(
        &mut self,
        id: &str,
        at: u64,
        approval_id: &str,
        decision: &str,
    ) -> Result<&JobView, EngineError> {
        let view = self.get(id).ok_or(EngineError::NoSuchJob)?;
        if view.pending_approval.is_none() {
            return Err(EngineError::IllegalTransition("no approval is pending"));
        }
        if view.pending_approval_id.as_deref() != Some(approval_id) {
            return Err(EngineError::BadInput(
                "approvalId does not match the pending approval",
            ));
        }
        if !DECISIONS.contains(&decision) {
            return Err(EngineError::BadInput("unknown approval decision"));
        }
        self.append(id, at, "approval.decided", json!({ "decision": decision }))
    }

    /// `job.completed` with the harness's result.
    pub fn complete(&mut self, id: &str, at: u64, result: Value) -> Result<&JobView, EngineError> {
        self.append(id, at, "job.completed", json!({ "result": result }))
    }

    /// `job.failed`. The message is clipped to 500 UTF-16 code units, as jobs.cjs clips it
    /// (`.slice(0, 500)`). Named difference: a cut inside a surrogate pair drops the whole
    /// character, where JS keeps a lone surrogate (one unit shorter here).
    pub fn fail(&mut self, id: &str, at: u64, error: &str) -> Result<&JobView, EngineError> {
        let mut units = 0;
        let error: String = error
            .chars()
            .take_while(|c| {
                units += c.len_utf16();
                units <= 500
            })
            .collect();
        self.append(id, at, "job.failed", json!({ "error": error }))
    }

    /// Cancels a job that has not finished. A finished job is returned as it is, with no event.
    pub fn cancel(&mut self, id: &str, at: u64) -> Result<&JobView, EngineError> {
        let status = self.get(id).ok_or(EngineError::NoSuchJob)?.status;
        if status.is_terminal() {
            return self.get(id).ok_or(EngineError::NoSuchJob);
        }
        self.append(id, at, "job.cancelled", json!({}))
    }

    /// `job.interrupted` for a job the host no longer runs.
    pub fn interrupt(&mut self, id: &str, at: u64, reason: &str) -> Result<&JobView, EngineError> {
        self.append(id, at, "job.interrupted", json!({ "reason": reason }))
    }

    /// After a restart: every unfinished job that `is_live` does not name is interrupted. Returns
    /// the ids it interrupted, in id order. Approvals are never resumed.
    pub fn recover(&mut self, at: u64, is_live: &dyn Fn(&str) -> bool) -> Vec<String> {
        let todo: Vec<(String, Status)> = self
            .journals
            .iter()
            .filter(|(id, j)| !j.fold.view().status.is_terminal() && !is_live(id))
            .map(|(id, j)| (id.clone(), j.fold.view().status))
            .collect();
        let mut done = Vec::new();
        for (id, status) in todo {
            let reason = if status == Status::WaitingApproval {
                "The server restarted while waiting for approval; start again to be asked again."
            } else {
                "The server restarted before this finished."
            };
            if self.interrupt(&id, at, reason).is_ok() {
                done.push(id);
            }
        }
        done
    }

    /// Removes finished jobs older than `retain_ms`, and all but the newest `max_jobs` finished ones.
    /// Returns the removed ids.
    pub fn prune(&mut self, now: u64, retain_ms: u64, max_jobs: usize) -> Vec<String> {
        let mut done: Vec<(u64, String)> = self
            .journals
            .iter()
            .filter(|(_, j)| j.fold.view().status.is_terminal())
            .map(|(id, j)| (j.fold.view().updated_at, id.clone()))
            .collect();
        done.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
        let mut removed = Vec::new();
        for (i, (updated, id)) in done.into_iter().enumerate() {
            if now.saturating_sub(updated) > retain_ms || i >= max_jobs {
                self.journals.remove(&id);
                removed.push(id);
            }
        }
        removed
    }

    pub fn get(&self, id: &str) -> Option<&JobView> {
        self.journals.get(id).map(|j| j.fold.view())
    }

    pub fn events(&self, id: &str) -> Option<&[Event]> {
        self.journals.get(id).map(|j| j.events.as_slice())
    }

    /// Every job, newest first.
    pub fn list(&self) -> Vec<&JobView> {
        let mut out: Vec<&JobView> = self.journals.values().map(|j| j.fold.view()).collect();
        out.sort_by(|a, b| {
            b.created_at
                .cmp(&a.created_at)
                .then_with(|| a.id.cmp(&b.id))
        });
        out
    }

    /// Loads one job from its journal rows (for example, read from disk by the host). The rows must
    /// be one job, in seq order from 1, and pass the same checks as writes. Lifecycle authority
    /// events are refused: this store has no pipeline that could have written them.
    pub fn load(&mut self, events: Vec<Event>) -> Result<&JobView, EngineError> {
        let first = events
            .first()
            .ok_or(EngineError::BadJournal("an empty journal"))?;
        let id = first.job.clone();
        if !valid_job_id(&id) {
            return Err(EngineError::BadJobId);
        }
        if self.journals.contains_key(&id) {
            return Err(EngineError::BadInput("job id is already in use"));
        }
        if events.len() > MAX_EVENTS_PER_JOB {
            return Err(EngineError::Limit("job has too many events"));
        }
        let mut fold = Fold::begin(first)?;
        for event in events.iter().skip(1) {
            if AUTHORITY_TYPES.contains(&event.kind.as_str()) {
                return Err(EngineError::BadJournal(
                    "lifecycle authority events are not supported yet",
                ));
            }
            check_event(&event.kind, &event.data)?;
            fold = fold.next(event)?;
        }
        self.journals.insert(id.clone(), Journal { events, fold });
        self.get(&id).ok_or(EngineError::NoSuchJob)
    }

    /// The capability list as a child or a new job stores it: unique, bounded, in first-seen order.
    fn checked_capabilities(&self, raw: &[String]) -> Result<Vec<String>, EngineError> {
        let mut out: Vec<String> = Vec::new();
        for cap in raw {
            if cap.is_empty() || cap.len() > self.limits.max_capability_bytes {
                return Err(EngineError::BadInput(
                    "capability name is empty or too long",
                ));
            }
            if !out.contains(cap) {
                out.push(cap.clone());
            }
        }
        if out.len() > self.limits.max_capabilities {
            return Err(EngineError::Limit("too many capabilities"));
        }
        Ok(out)
    }
}
