//! Native task control primitives. A cancelled execution keeps its reservation until
//! the execution guard is dropped after sidecar teardown. Approval waits own their registration.
pub mod account;
pub mod acp;
pub mod code;
pub mod context;
pub mod egress;
pub mod facts;
pub mod files;
pub mod git;
pub mod guard;
pub mod launch;
pub mod meta;
pub mod output;
pub mod pin;
pub mod policy;
pub mod resources;
pub mod session;
pub mod start;
pub mod workspace;

use js_json::JValue;
use rand_core::{OsRng, RngCore};
use server_projects::task_journals::Surface;
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use tokio::sync::{oneshot, watch};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Scope {
    pub user: String,
    pub project: String,
    pub surface: Surface,
}
#[derive(Debug, PartialEq, Eq)]
pub enum Error {
    Unavailable,
    Busy,
    Missing,
    UnknownDecision,
    MissingApproval,
    Expired,
    NotWaiting,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Decision {
    Approve,
    ApproveAll,
    Deny,
    Timeout,
    Aborted,
}
impl Decision {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Approve => "approve",
            Self::ApproveAll => "approve_all",
            Self::Deny => "deny",
            Self::Timeout => "timeout",
            Self::Aborted => "aborted",
        }
    }
    pub fn parse(value: &str) -> Result<Self, Error> {
        match value {
            "approve" => Ok(Self::Approve),
            "approve_all" => Ok(Self::ApproveAll),
            "deny" => Ok(Self::Deny),
            _ => Err(Error::UnknownDecision),
        }
    }
}
struct Execution {
    scope: Scope,
    cancel: watch::Sender<bool>,
}
struct Pending {
    order: u64,
    scope: Scope,
    task: String,
    card: JValue,
    deadline: Instant,
    answer: oneshot::Sender<Decision>,
}
#[derive(Default)]
struct State {
    next_approval: u64,
    executions: HashMap<String, Arc<Execution>>,
    approvals: HashMap<String, Pending>,
    expired: HashMap<String, (Scope, String, Instant)>,
}
#[derive(Clone, Default)]
pub struct Control {
    state: Arc<Mutex<State>>,
}
impl Control {
    pub fn begin(&self, scope: Scope, task: String) -> Result<ExecutionGuard, Error> {
        let mut state = self.state.lock().map_err(|_| Error::Unavailable)?;
        if state.executions.contains_key(&task)
            || state.executions.values().any(|e| e.scope == scope)
        {
            return Err(Error::Busy);
        }
        let (cancel, _) = watch::channel(false);
        let execution = Arc::new(Execution { scope, cancel });
        state.executions.insert(task.clone(), execution.clone());
        Ok(ExecutionGuard {
            control: self.clone(),
            task,
            execution,
        })
    }
    pub fn cancel(&self, scope: &Scope, task: &str) -> Result<(), Error> {
        let mut state = self.state.lock().map_err(|_| Error::Unavailable)?;
        let execution = state
            .executions
            .get(task)
            .filter(|e| &e.scope == scope)
            .ok_or(Error::Missing)?;
        execution.cancel.send_replace(true);
        abort_approvals(&mut state, scope, task);
        Ok(())
    }
    pub fn end_approvals(&self, scope: &Scope, task: &str) -> Result<(), Error> {
        let mut state = self.state.lock().map_err(|_| Error::Unavailable)?;
        abort_approvals(&mut state, scope, task);
        Ok(())
    }
    pub fn ask(
        &self,
        scope: &Scope,
        task: &str,
        card: JValue,
        timeout: Duration,
    ) -> Result<ApprovalWait, Error> {
        let id = random_id()?;
        let (answer, receiver) = oneshot::channel();
        let mut state = self.state.lock().map_err(|_| Error::Unavailable)?;
        let execution = state
            .executions
            .get(task)
            .filter(|e| &e.scope == scope)
            .ok_or(Error::Missing)?;
        if *execution.cancel.borrow() {
            return Err(Error::Missing);
        }
        let cancel = execution.cancel.subscribe();
        let deadline = Instant::now()
            .checked_add(timeout)
            .ok_or(Error::Unavailable)?;
        let order = state.next_approval;
        state.next_approval = order.checked_add(1).ok_or(Error::Unavailable)?;
        state.approvals.insert(
            id.clone(),
            Pending {
                order,
                scope: scope.clone(),
                task: task.into(),
                card,
                deadline,
                answer,
            },
        );
        Ok(ApprovalWait {
            control: self.clone(),
            id,
            scope: scope.clone(),
            task: task.into(),
            receiver,
            cancel,
            deadline,
        })
    }
    pub fn decide(&self, scope: &Scope, task: &str, id: &str, value: &str) -> Result<(), Error> {
        let decision = Decision::parse(value)?;
        if id.is_empty() {
            return Err(Error::MissingApproval);
        }
        let mut state = self.state.lock().map_err(|_| Error::Unavailable)?;
        let now = Instant::now();
        state.expired.retain(|_, (_, _, until)| *until > now);
        if !state
            .approvals
            .get(id)
            .is_some_and(|p| &p.scope == scope && p.task == task)
        {
            return Err(
                if state
                    .expired
                    .get(id)
                    .is_some_and(|(s, t, _)| s == scope && t == task)
                {
                    Error::Expired
                } else {
                    Error::NotWaiting
                },
            );
        }
        let pending = state.approvals.remove(id).ok_or(Error::NotWaiting)?;
        if now >= pending.deadline {
            state.expired.insert(
                id.into(),
                (scope.clone(), task.into(), now + Duration::from_secs(60)),
            );
            let _ = pending.answer.send(Decision::Timeout);
            return Err(Error::Expired);
        }
        let decision = if scope.surface == Surface::Browser && decision == Decision::ApproveAll {
            Decision::Approve
        } else {
            decision
        };
        pending.answer.send(decision).map_err(|_| Error::NotWaiting)
    }
    pub fn cards(&self, scope: &Scope) -> Result<Vec<(String, String, JValue)>, Error> {
        let state = self.state.lock().map_err(|_| Error::Unavailable)?;
        let mut cards: Vec<_> = state
            .approvals
            .iter()
            .filter(|(_, p)| &p.scope == scope)
            .collect();
        cards.sort_by_key(|(_, p)| p.order);
        Ok(cards
            .into_iter()
            .map(|(id, p)| (id.clone(), p.task.clone(), p.card.clone()))
            .collect())
    }
}
fn random_id() -> Result<String, Error> {
    let mut bytes = [0u8; 16];
    OsRng
        .try_fill_bytes(&mut bytes)
        .map_err(|_| Error::Unavailable)?;
    // Match randomUUID's public card id shape.
    let mut hex = bytes.iter().map(|b| format!("{b:02x}")).collect::<String>();
    hex.replace_range(12..13, "4");
    let variant = bytes.get(8).copied().ok_or(Error::Unavailable)? & 3 | 8;
    hex.replace_range(16..17, &format!("{variant:x}"));
    for at in [20, 16, 12, 8] {
        hex.insert(at, '-');
    }
    Ok(hex)
}
fn abort_approvals(state: &mut State, scope: &Scope, task: &str) {
    let ids: Vec<_> = state
        .approvals
        .iter()
        .filter(|(_, p)| &p.scope == scope && p.task == task)
        .map(|(id, _)| id.clone())
        .collect();
    for id in ids {
        if let Some(pending) = state.approvals.remove(&id) {
            let _ = pending.answer.send(Decision::Aborted);
        }
    }
}
/// Kept by the actor until teardown finishes, including when a request future is dropped.
pub struct ExecutionGuard {
    control: Control,
    task: String,
    execution: Arc<Execution>,
}
impl ExecutionGuard {
    pub fn cancellation(&self) -> watch::Receiver<bool> {
        self.execution.cancel.subscribe()
    }
}
impl Drop for ExecutionGuard {
    fn drop(&mut self) {
        self.execution.cancel.send_replace(true);
        if let Ok(mut state) = self.control.state.lock() {
            if state
                .executions
                .get(&self.task)
                .is_some_and(|e| Arc::ptr_eq(e, &self.execution))
            {
                state.executions.remove(&self.task);
                abort_approvals(&mut state, &self.execution.scope, &self.task);
            }
        }
    }
}
/// Removing this future at any await removes the matching approval; a late card cannot authorize work.
pub struct ApprovalWait {
    control: Control,
    pub id: String,
    scope: Scope,
    task: String,
    receiver: oneshot::Receiver<Decision>,
    cancel: watch::Receiver<bool>,
    deadline: Instant,
}
impl ApprovalWait {
    pub async fn wait(mut self) -> Decision {
        let (decision, expired_here) = tokio::select! {
            biased;
            result = &mut self.receiver => (result.unwrap_or(Decision::Aborted), false),
            _ = self.cancel.changed() => (Decision::Aborted, false),
            _ = tokio::time::sleep_until(self.deadline.into()) => (Decision::Timeout, true),
        };
        if expired_here {
            if let Ok(mut state) = self.control.state.lock() {
                let now = Instant::now();
                state.expired.retain(|_, (_, _, until)| *until > now);
                state.expired.insert(
                    self.id.clone(),
                    (
                        self.scope.clone(),
                        self.task.clone(),
                        now + Duration::from_secs(60),
                    ),
                );
            }
        }
        decision
    }
}
impl Drop for ApprovalWait {
    fn drop(&mut self) {
        if let Ok(mut state) = self.control.state.lock() {
            state.approvals.remove(&self.id);
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    fn scope(surface: Surface) -> Scope {
        Scope {
            user: "account-a".into(),
            project: "project".into(),
            surface,
        }
    }
    #[tokio::test]
    async fn decisions_and_browser_allow_all_have_existing_semantics() {
        for surface in [Surface::Code, Surface::Browser] {
            let c = Control::default();
            let s = scope(surface);
            let _run = c.begin(s.clone(), "task".into()).unwrap();
            for (input, want) in [
                ("approve", Decision::Approve),
                ("deny", Decision::Deny),
                (
                    "approve_all",
                    if surface == Surface::Code {
                        Decision::ApproveAll
                    } else {
                        Decision::Approve
                    },
                ),
            ] {
                let wait = c
                    .ask(&s, "task", JValue::Null, Duration::from_secs(10))
                    .unwrap();
                c.decide(&s, "task", &wait.id, input).unwrap();
                assert_eq!(wait.wait().await, want);
            }
        }
    }
    #[tokio::test]
    async fn cancellation_keeps_reservation_until_teardown_and_refuses_cards() {
        let c = Control::default();
        let s = scope(Surface::Browser);
        let run = c.begin(s.clone(), "task".into()).unwrap();
        let wait = c
            .ask(&s, "task", JValue::Null, Duration::from_secs(10))
            .unwrap();
        let id = wait.id.clone();
        c.cancel(&s, "task").unwrap();
        assert_eq!(wait.wait().await, Decision::Aborted);
        assert_eq!(c.decide(&s, "task", &id, "approve"), Err(Error::NotWaiting));
        assert!(matches!(
            c.begin(s.clone(), "next".into()),
            Err(Error::Busy)
        ));
        assert!(c
            .ask(&s, "task", JValue::Null, Duration::from_secs(1))
            .is_err());
        drop(run);
        assert!(c.begin(s, "next".into()).is_ok());
    }
    #[tokio::test]
    async fn expired_card_cannot_approve_a_successor_card() {
        let c = Control::default();
        let s = scope(Surface::Code);
        let _run = c.begin(s.clone(), "task".into()).unwrap();
        let first = c.ask(&s, "task", JValue::Null, Duration::ZERO).unwrap();
        let old = first.id.clone();
        assert_eq!(first.wait().await, Decision::Timeout);
        let next = c
            .ask(&s, "task", JValue::Null, Duration::from_secs(10))
            .unwrap();
        assert_eq!(
            c.decide(&s, "task", &old, "approve_all"),
            Err(Error::Expired)
        );
        c.decide(&s, "task", &next.id, "deny").unwrap();
        assert_eq!(next.wait().await, Decision::Deny);
    }
    #[tokio::test]
    async fn decision_deadline_is_enforced_even_before_wait_is_polled() {
        let c = Control::default();
        let s = scope(Surface::Code);
        let _run = c.begin(s.clone(), "task".into()).unwrap();
        let wait = c.ask(&s, "task", JValue::Null, Duration::ZERO).unwrap();
        assert_eq!(
            c.decide(&s, "task", &wait.id, "approve"),
            Err(Error::Expired)
        );
        assert_eq!(wait.wait().await, Decision::Timeout);
    }
    #[tokio::test]
    async fn foreign_scope_and_dropped_waits_cannot_answer_cards() {
        let c = Control::default();
        let s = scope(Surface::Code);
        let _run = c.begin(s.clone(), "task".into()).unwrap();
        let wait = c
            .ask(&s, "task", JValue::Null, Duration::from_secs(10))
            .unwrap();
        let id = wait.id.clone();
        let mut other = s.clone();
        other.user = "account-b".into();
        assert_eq!(c.cancel(&other, "task"), Err(Error::Missing));
        assert_eq!(
            c.decide(&other, "task", &id, "approve"),
            Err(Error::NotWaiting)
        );
        assert!(c.cards(&other).unwrap().is_empty());
        drop(wait);
        assert!(c.cards(&s).unwrap().is_empty());
        assert_eq!(c.decide(&s, "task", &id, "approve"), Err(Error::NotWaiting));
    }
    #[tokio::test]
    async fn aborted_wait_future_removes_approval_registration() {
        let c = Control::default();
        let s = scope(Surface::Code);
        let _run = c.begin(s.clone(), "task".into()).unwrap();
        let wait = c
            .ask(&s, "task", JValue::Null, Duration::from_secs(10))
            .unwrap();
        let job = tokio::spawn(wait.wait());
        tokio::task::yield_now().await;
        job.abort();
        let _ = job.await;
        assert!(c.cards(&s).unwrap().is_empty());
    }
    #[tokio::test]
    async fn a_settled_answer_is_not_rewritten_by_later_cancellation() {
        let c = Control::default();
        let s = scope(Surface::Code);
        let run = c.begin(s.clone(), "task".into()).unwrap();
        let wait = c
            .ask(&s, "task", JValue::Null, Duration::from_secs(10))
            .unwrap();
        c.decide(&s, "task", &wait.id, "approve").unwrap();
        c.cancel(&s, "task").unwrap();
        assert_eq!(wait.wait().await, Decision::Approve);
        assert!(
            *run.cancellation().borrow(),
            "execution must still observe cancellation independently of the settled card"
        );
    }

    #[tokio::test]
    async fn code_tasks_reserve_a_project_and_cancellation_is_per_task() {
        let c = Control::default();
        let s = scope(Surface::Code);
        let first = c.begin(s.clone(), "first".into()).unwrap();
        assert!(matches!(
            c.begin(s.clone(), "second".into()),
            Err(Error::Busy)
        ));
        let mut other = s.clone();
        other.project = "other-project".into();
        let second = c.begin(other, "second".into()).unwrap();
        assert!(matches!(
            c.begin(s.clone(), "first".into()),
            Err(Error::Busy)
        ));
        c.cancel(&s, "first").unwrap();
        assert!(*first.cancellation().borrow());
        assert!(!*second.cancellation().borrow());
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod journal_tests;
