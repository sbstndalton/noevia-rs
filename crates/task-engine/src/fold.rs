//! The job status machine and the fold over a journal (jobs.cjs `foldEvent` and `derive`), with the
//! task lifecycle folded alongside it through `task_lifecycle::transition`.
//!
//! Stricter than Node in two places, both covered by tests: `job.started` is only legal from
//! `queued`, and `approval.requested` / `approval.decided` only from `running` / `waiting_approval`.
//! Node relies on its controller map for the first and does not check the second at all.

use serde_json::Value;
use task_lifecycle::State;

use crate::error::EngineError;
use crate::event::{Event, MAX_EVENTS_PER_JOB};

/// A job's status (jobs.cjs `job.status`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Queued,
    Running,
    WaitingApproval,
    Completed,
    Failed,
    Cancelled,
    Interrupted,
}

impl Status {
    /// The jobs.cjs spelling.
    pub fn name(self) -> &'static str {
        match self {
            Status::Queued => "queued",
            Status::Running => "running",
            Status::WaitingApproval => "waiting_approval",
            Status::Completed => "completed",
            Status::Failed => "failed",
            Status::Cancelled => "cancelled",
            Status::Interrupted => "interrupted",
        }
    }

    /// `completed`, `failed`, `cancelled` or `interrupted`: no event may follow.
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            Status::Completed | Status::Failed | Status::Cancelled | Status::Interrupted
        )
    }
}

/// What `get()` returns for a job: jobs.cjs's derived job object, without the review and plan
/// fields this slice does not carry.
#[derive(Debug, Clone, PartialEq)]
pub struct JobView {
    pub id: String,
    pub kind: String,
    pub project_id: Option<String>,
    pub parent_id: Option<String>,
    pub capabilities: Vec<String>,
    pub status: Status,
    pub stage: Option<String>,
    pub pending_approval: Option<Value>,
    pub result: Value,
    pub error: Option<String>,
    pub created_at: u64,
    pub updated_at: u64,
    /// The task lifecycle state name, or `None` when the journal made an illegal lifecycle move
    /// (jobs.cjs `lifecycleOk = false`, which derives `null`).
    pub lifecycle: Option<&'static str>,
    pub event_count: u64,
}

/// The fold of a journal so far. Each event is applied to a copy, so a refused event leaves the
/// fold exactly as it was.
#[derive(Debug, Clone)]
pub(crate) struct Fold {
    view: JobView,
    lifecycle: Option<State>,
}

impl Fold {
    /// The fold of a journal's first event, which must be `job.created` at seq 1.
    pub(crate) fn begin(first: &Event) -> Result<Fold, EngineError> {
        if first.kind != "job.created" || first.seq != 1 {
            return Err(EngineError::BadJournal("a journal begins with job.created"));
        }
        let d = &first.data;
        let kind = d
            .get("kind")
            .and_then(Value::as_str)
            .filter(|k| !k.is_empty())
            .ok_or(EngineError::BadInput("job kind required"))?;
        let view = JobView {
            id: first.job.clone(),
            kind: kind.to_string(),
            project_id: optional_string(d.get("projectId")),
            parent_id: optional_string(d.get("parentId")),
            capabilities: string_list(d.get("capabilities")),
            status: Status::Queued,
            stage: None,
            pending_approval: None,
            result: Value::Null,
            error: None,
            created_at: first.at,
            updated_at: first.at,
            lifecycle: Some(task_lifecycle::INITIAL_STATE.name()),
            event_count: 1,
        };
        Ok(Fold {
            view,
            lifecycle: Some(task_lifecycle::INITIAL_STATE),
        })
    }

    pub(crate) fn view(&self) -> &JobView {
        &self.view
    }

    /// The fold after `event`, or the refusal. `event.seq` must be the next seq.
    pub(crate) fn next(&self, event: &Event) -> Result<Fold, EngineError> {
        if event.job != self.view.id {
            return Err(EngineError::BadJournal("an event belongs to another job"));
        }
        if event.seq != self.view.event_count + 1 {
            return Err(EngineError::BadJournal("event sequence has a gap"));
        }
        if event.kind == "job.created" {
            return Err(EngineError::BadJournal("job.created appears twice"));
        }
        if MAX_EVENTS_PER_JOB as u64 <= self.view.event_count {
            return Err(EngineError::Limit("job has too many events"));
        }
        let status = next_status(self.view.status, &event.kind)?;
        let mut out = self.clone();
        out.lifecycle = lifecycle_step(self.lifecycle, &event.kind, &event.data);
        out.view.lifecycle = out.lifecycle.map(State::name);
        out.view.status = status;
        out.view.updated_at = event.at;
        out.view.event_count += 1;
        apply_data(&mut out.view, event);
        Ok(out)
    }
}

/// Folds a whole journal, as jobs.cjs `derive()` does, after the same checks [`crate::JobStore::load`]
/// makes. Events must be in seq order with one job id.
pub fn fold(events: &[Event]) -> Result<JobView, EngineError> {
    let mut iter = events.iter();
    let first = iter.next().ok_or(EngineError::NoSuchJob)?;
    let mut fold = Fold::begin(first)?;
    for event in iter {
        fold = fold.next(event)?;
    }
    Ok(fold.view)
}

/// The status a `kind` event moves a job to, or the refusal. Terminal jobs take no event.
fn next_status(current: Status, kind: &str) -> Result<Status, EngineError> {
    if current.is_terminal() {
        return Err(EngineError::Finished);
    }
    Ok(match kind {
        "job.started" if current == Status::Queued => Status::Running,
        "job.started" => {
            return Err(EngineError::IllegalTransition(
                "job.started needs a queued job",
            ))
        }
        "approval.requested" if current == Status::Running => Status::WaitingApproval,
        "approval.requested" => {
            return Err(EngineError::IllegalTransition(
                "approval.requested needs a running job",
            ))
        }
        "approval.decided" if current == Status::WaitingApproval => Status::Running,
        "approval.decided" => {
            return Err(EngineError::IllegalTransition(
                "approval.decided needs a pending approval",
            ))
        }
        "job.completed" => Status::Completed,
        "job.failed" => Status::Failed,
        "job.cancelled" => Status::Cancelled,
        "job.interrupted" => Status::Interrupted,
        // Everything else (progress, steps, tools, artifacts, plans, reviews, output) keeps the status.
        _ => current,
    })
}

/// The task lifecycle move for one event (jobs.cjs `step`, non-authoritative rules). A refused
/// move ends the lifecycle for good (`None`), as `lifecycleOk = false` does.
fn lifecycle_step(state: Option<State>, kind: &str, data: &Value) -> Option<State> {
    let state = state?;
    match kind {
        "job.started" => advance(state, State::Implementing),
        "progress" => match data.get("stage").and_then(Value::as_str) {
            Some("implementing") => advance(state, State::Implementing),
            Some("verifying") => advance(state, State::Verifying),
            _ => Some(state),
        },
        "job.completed" => advance(state, State::Verifying),
        "job.failed" | "job.cancelled" | "job.interrupted" => advance(state, State::Blocked),
        _ => Some(state),
    }
}

fn advance(from: State, to: State) -> Option<State> {
    task_lifecycle::transition(from, to).ok()
}

/// The view fields an event sets (jobs.cjs `foldEvent`'s switch).
fn apply_data(view: &mut JobView, event: &Event) {
    let d = &event.data;
    match event.kind.as_str() {
        "progress" => {
            if let Some(stage) = d.get("stage").and_then(Value::as_str) {
                view.stage = Some(stage.to_string());
            }
        }
        "approval.requested" => view.pending_approval = Some(d.clone()),
        "approval.decided" => view.pending_approval = None,
        "job.completed" | "job.cancelled" => {
            view.pending_approval = None;
            view.result = d.get("result").cloned().unwrap_or(Value::Null);
        }
        "job.failed" => {
            view.pending_approval = None;
            view.result = d.get("result").cloned().unwrap_or(Value::Null);
            view.error = Some(
                d.get("error")
                    .and_then(Value::as_str)
                    .unwrap_or("failed")
                    .to_string(),
            );
        }
        "job.interrupted" => {
            view.pending_approval = None;
            view.error = Some(
                d.get("reason")
                    .and_then(Value::as_str)
                    .unwrap_or("interrupted")
                    .to_string(),
            );
        }
        _ => {}
    }
}

fn optional_string(v: Option<&Value>) -> Option<String> {
    v.and_then(Value::as_str).map(str::to_string)
}

fn string_list(v: Option<&Value>) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    if let Some(Value::Array(items)) = v {
        for item in items {
            if let Some(s) = item.as_str() {
                if !out.iter().any(|x| x == s) {
                    out.push(s.to_string());
                }
            }
        }
    }
    out
}
