//! The Planner review of a direct Code task (`features.plannerReview`, core #519): code-harness.cjs
//! `start`'s `reviewed` / `reviewGate`, and the reviewer core index.cjs builds for it
//! (code-review.cjs `createPlannerReview` with the Code role engine, so a review alone pins the
//! task's model for that one task: `reviewInSession` without a session).
//!
//! The flag is read once per task, at its start, by the host ([`decide`]: a reader that panics is
//! off), and the reviewer travels with the start ([`crate::executor::StartRequest::review`]). Off,
//! a task runs exactly as it did without the gate. On, after the run has finished and its workspace
//! has been released (and its grant revoked), the gate reads the change, records
//! `review.requested`, asks the reviewer with the bounded state, records `review.completed` or
//! `review.failed`, and puts the `review_change` card in front of the person; only `approve` or
//! `approve_all` accepts, and a `timeout` or `aborted` card is "not accepted". The task's result
//! carries `review: { reviewed, verdict, accepted, decision, headSha }`. A run cancelled before or
//! during the review returns its result without one; a run that failed is never reviewed.
//!
//! Pinned by `tests/fixtures/review-gate-node.v1.json`, recorded from core's own
//! `createCodeHarness` and `createPlannerReview` by `tools/gen-review-gate-fixtures.cjs` (the
//! sha256 of each source is in the fixture and in [`CODE_HARNESS_SHA256`] /
//! [`CODE_REVIEW_SHA256`]).
//!
//! Named differences from Node:
//! - The reviewer's deadline covers the pin and the call as Node's does, but a pin still under way
//!   when it passes is waited for (Node's pin takes no signal either; its answer is then refused
//!   as `timeout`, as Node refuses it).
//! - A host with the flag on but no Code role engine for this start (a core that gives no `role`
//!   fact) reviews with [`Unconfigured`]: Node's own answer for a reviewer with neither a provider
//!   nor a role engine ("No reviewer model is configured on this server."), so the card says why.

use crate::executor::{Approvals, Fail, Jobs};
use crate::session::Ended;
use code_acp::Cancel;
use code_pipeline::driver::{Deadline, Session, Signal};
use code_pipeline::review::{review_pin_code, REVIEW_ACTION, REVIEW_CANCELLED_BEFORE};
use code_pipeline::review::{review_timeout_reason, DEFAULT_DEADLINE_MS};
use code_pipeline::sessions::{review_in_session, verdict_value};
use code_pipeline::{RoleFail, Verdict};
use js_json::{to_js_string, wtf16, JValue};
use std::sync::Arc;

/// sha256 of the `server/code-harness.cjs` (its `reviewGate`) the fixture was recorded from.
pub const CODE_HARNESS_SHA256: &str =
    "ec017f978fbe11b4905e7964f5959a251db362de6f41b29c8a13d66cb03f6fe6";
/// sha256 of the `server/code-review.cjs` (`createPlannerReview`) the fixture was recorded from.
pub const CODE_REVIEW_SHA256: &str =
    "4cc944fdffd5d7c374257d8b301252ec947ac1fb729006c53895fe0db293287f";

/// What `review()` answers: the verdict and whether it was `corrected`, or `{ code, reason }`.
pub type Outcome = Result<(Verdict, bool), RoleFail>;

/// code-review.cjs's `review({ state, signal })`: never panics into the task, never accepts.
pub trait Reviewer: Send + Sync {
    fn review(&self, state: &JValue, signal: &Signal) -> Outcome;
}

/// role-engine.cjs `pinModel({ taskId })` for one review: the pinned session, or the pin's
/// `{ code, reason }`.
pub trait ReviewRoles: Send + Sync {
    fn pin(&self, task_id: &str) -> Result<Arc<dyn Session>, RoleFail>;
}

fn s(text: &str) -> JValue {
    JValue::Str(text.to_string())
}

/// `createPlannerReview({ roleEngine, log, deadlineMs })`'s `review` without a session.
pub struct PlannerReview {
    pub roles: Arc<dyn ReviewRoles>,
    pub deadline_ms: u64,
    pub log: Arc<dyn Fn(JValue) + Send + Sync>,
}

impl PlannerReview {
    /// With Node's deadline (`DEFAULT_DEADLINE_MS`).
    pub fn new(roles: Arc<dyn ReviewRoles>, log: Arc<dyn Fn(JValue) + Send + Sync>) -> Self {
        PlannerReview {
            roles,
            deadline_ms: DEFAULT_DEADLINE_MS,
            log,
        }
    }
}

impl Reviewer for PlannerReview {
    fn review(&self, state: &JValue, signal: &Signal) -> Outcome {
        if signal.is_aborted() {
            return Err(REVIEW_CANCELLED_BEFORE.into());
        }
        // One deadline for the pin and the call, as Node's one timer.
        let deadline = Deadline::start(signal, self.deadline_ms);
        let task_id = state.get("taskId").as_str().unwrap_or("");
        let out = match self.roles.pin(task_id) {
            Err(f) => {
                (self.log)(JValue::obj([
                    ("event", s("code.review_failed")),
                    ("code", f.code.clone()),
                ]));
                let code = match f.code.as_str() {
                    Some(c) => s(review_pin_code(c)),
                    None => f.code,
                };
                return Err(RoleFail {
                    code,
                    reason: f.reason,
                });
            }
            Ok(session) => review_in_session(
                &*session,
                state,
                &deadline.signal,
                self.deadline_ms,
                &*self.log,
            ),
        };
        if deadline.finish() {
            return Err(RoleFail {
                code: s("timeout"),
                reason: s(&review_timeout_reason(self.deadline_ms)),
            });
        }
        out
    }
}

/// A reviewer with neither a provider nor a role engine (code-review.cjs's `unavailable`).
pub struct Unconfigured;

impl Reviewer for Unconfigured {
    fn review(&self, _state: &JValue, _signal: &Signal) -> Outcome {
        Err(RoleFail {
            code: s("unavailable"),
            reason: s("No reviewer model is configured on this server."),
        })
    }
}

/// The review decided once for a start: `enabled` (features.plannerReview; a panic is off), then
/// `make` for the reviewer.
pub fn decide(
    enabled: &dyn Fn() -> bool,
    make: &dyn Fn() -> Arc<dyn Reviewer>,
) -> Option<Arc<dyn Reviewer>> {
    let on = std::panic::catch_unwind(std::panic::AssertUnwindSafe(enabled)).unwrap_or(false);
    if !on {
        return None;
    }
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(make))
        .ok()
        .or_else(|| Some(Arc::new(Unconfigured)))
}

/// A log entry's own keys (the executor puts `at` and `taskId` first).
pub(crate) type Entry<'k> = Vec<(&'k str, JValue)>;

/// What the gate is given about the task.
pub(crate) struct Gate<'a> {
    pub task_id: &'a str,
    pub jobs: &'a dyn Jobs,
    pub workspaces: &'a code_workspace::Workspaces,
    pub approvals: &'a dyn Approvals,
    pub log: &'a dyn for<'k> Fn(Entry<'k>),
    pub reviewer: &'a dyn Reviewer,
    pub prompt: &'a str,
    pub tenant_id: Option<&'a str>,
    pub capabilities: &'a [String],
    pub branch: JValue,
    pub base_sha: JValue,
    /// `handle.released?.headSha ?? null`.
    pub head_sha: JValue,
    pub engine_key: Option<String>,
    pub grant_token: JValue,
    pub cancel: &'a Arc<Cancel>,
}

/// `runSummary(result)`.
fn run_summary(result: &JValue) -> String {
    let n = |k: &str| match result.get(k) {
        JValue::Num(v) if v.is_finite() && v.fract() == 0.0 => {
            to_js_string(&JValue::Num(*v)).unwrap_or_else(|_| "0".into())
        }
        _ => "0".into(),
    };
    format!(
        "{} tool calls; {} allowed, {} declined by the person, {} refused by noevia.",
        n("tools"),
        n("allowed"),
        n("refused"),
        n("denied")
    )
}

fn paths(change: &JValue) -> Vec<JValue> {
    match change.get("files") {
        JValue::Arr(files) => files.iter().map(|f| f.get("path").clone()).collect(),
        _ => Vec::new(),
    }
}

impl Gate<'_> {
    fn event(&self, kind: &str, data: JValue) -> Result<(), Fail> {
        self.jobs.append(self.task_id, kind, data)
    }

    /// `reviewGate(ctx, result)`: the result with its `review`, or as it was when the task was
    /// cancelled meanwhile.
    pub(crate) fn run(&self, result: JValue) -> Result<JValue, Fail> {
        let shas = || {
            vec![
                ("baseSha".to_string(), self.base_sha.clone()),
                ("headSha".to_string(), self.head_sha.clone()),
            ]
        };
        let (change, mut outcome) = match self.workspaces.change(self.task_id, 200) {
            Ok(c) => (Some(c), None),
            Err(e) => {
                let code = e.code.filter(|c| !c.is_empty()).unwrap_or("no_change");
                let reason = if e.message.is_empty() {
                    "The change could not be read."
                } else {
                    &e.message
                };
                let fail = RoleFail {
                    code: s(code),
                    reason: s(&wtf16::slice(reason, 300)),
                };
                (None, Some(Err(fail)))
            }
        };
        let files = change.as_ref().map(paths);
        let mut requested = shas();
        let count = files
            .as_ref()
            .map_or(JValue::Null, |f| JValue::Num(f.len() as f64));
        requested.push(("files".into(), count));
        self.event("review.requested", JValue::Obj(requested))?;
        // The task's signal, for the reviewer and the card.
        let signal = Signal::new();
        let ended = Arc::new(Ended::default());
        {
            let (signal, ended) = (signal.clone(), ended.clone());
            self.cancel.on_cancel(Box::new(move || {
                signal.abort();
                ended.end();
            }));
        }
        if let (None, Some(change)) = (&outcome, &change) {
            let job = self.jobs.get(self.task_id).unwrap_or(JValue::Null);
            let plan = match job.get("plan").get("subQuestions") {
                JValue::Arr(steps) if !steps.is_empty() => JValue::obj([(
                    "steps",
                    JValue::Arr(
                        steps
                            .iter()
                            .map(|e| JValue::obj([("do", e.clone())]))
                            .collect(),
                    ),
                )]),
                _ => JValue::Undefined,
            };
            let changed = JValue::Arr(files.clone().unwrap_or_default());
            let state = JValue::obj([
                ("taskId", s(self.task_id)),
                ("tenantId", self.tenant_id.map_or(JValue::Null, s)),
                ("request", s(self.prompt)),
                (
                    "capabilities",
                    JValue::Arr(self.capabilities.iter().map(|c| s(c)).collect()),
                ),
                ("plan", plan),
                (
                    "execution",
                    JValue::obj([
                        ("headSha", self.head_sha.clone()),
                        ("changedFiles", changed),
                        ("summary", s(&run_summary(&result))),
                    ]),
                ),
                ("change", change.clone()),
                (
                    "credentials",
                    self.engine_key
                        .as_deref()
                        .filter(|k| !k.is_empty())
                        .map_or(JValue::Undefined, |k| JValue::obj([("engine", s(k))])),
                ),
                (
                    "tokens",
                    if self.grant_token.truthy() {
                        JValue::Arr(vec![self.grant_token.clone()])
                    } else {
                        JValue::Undefined
                    },
                ),
            ]);
            let reviewer = self.reviewer;
            let answered = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                reviewer.review(&state, &signal)
            }));
            outcome = Some(answered.unwrap_or_else(|_| {
                Err(RoleFail {
                    code: s("error"),
                    reason: s("The reviewer could not be reached."),
                })
            }));
        }
        if self.cancel.is_cancelled() {
            return Ok(result);
        }
        let outcome = outcome.unwrap_or_else(|| {
            Err(RoleFail {
                code: s("error"),
                reason: s("The reviewer could not be reached."),
            })
        });
        let reviewed = outcome.is_ok();
        let reason = match &outcome {
            Ok((verdict, corrected)) => {
                let mut data = shas();
                if let JValue::Obj(items) = verdict_value(verdict) {
                    for (k, v) in items {
                        wtf16::set(&mut data, &k, v);
                    }
                }
                wtf16::set(&mut data, "corrected", JValue::Bool(*corrected));
                self.event("review.completed", JValue::Obj(data))?;
                "The Planner\u{2019}s verdict is advice. Accepting records this reviewed head as accepted; nothing is merged automatically.".to_string()
            }
            Err(f) => {
                let mut data = shas();
                data.push(("code".into(), f.code.clone()));
                data.push(("reason".into(), f.reason.clone()));
                self.event("review.failed", JValue::Obj(data))?;
                format!(
                    "Not reviewed: {} Review the change on its branch yourself before accepting it.",
                    to_js_string(&f.reason).unwrap_or_default()
                )
            }
        };
        let recorded = self
            .jobs
            .get(self.task_id)
            .map(|j| j.get("review").clone())
            .filter(JValue::truthy)
            .unwrap_or(JValue::Null);
        let files = JValue::Arr(files.unwrap_or_default());
        let first: Vec<JValue> = match &files {
            JValue::Arr(f) => f.iter().take(50).cloned().collect(),
            _ => Vec::new(),
        };
        let request = JValue::obj([
            ("taskId", s(self.task_id)),
            ("action", s(REVIEW_ACTION)),
            ("title", s("Accept this change")),
            ("kind", s("review")),
            ("command", s("")),
            ("paths", JValue::Arr(first)),
            ("reason", s(&reason)),
            (
                "arguments",
                JValue::obj([
                    ("branch", self.branch.clone()),
                    ("baseSha", self.base_sha.clone()),
                    ("headSha", self.head_sha.clone()),
                    ("files", files),
                ]),
            ),
            ("diff", JValue::Null),
            ("review", recorded),
        ]);
        self.event("approval.requested", request.clone())?;
        let answer = self.approvals.ask(&request, &ended);
        self.event(
            "approval.decided",
            JValue::obj([("decision", s(&answer)), ("action", s(REVIEW_ACTION))]),
        )?;
        (self.log)(vec![
            ("event", s("code.review_decided")),
            ("decision", s(&answer)),
            ("reviewed", JValue::Bool(reviewed)),
        ]);
        // Only the person's own yes accepts.
        let accepted = answer == "approve" || answer == "approve_all";
        let verdict = match &outcome {
            Ok((v, _)) => s(v.verdict),
            Err(_) => JValue::Null,
        };
        let mut out = match result {
            JValue::Obj(items) => items,
            _ => Vec::new(),
        };
        wtf16::set(
            &mut out,
            "review",
            JValue::obj([
                ("reviewed", JValue::Bool(reviewed)),
                ("verdict", verdict),
                ("accepted", JValue::Bool(accepted)),
                ("decision", s(&answer)),
                ("headSha", self.head_sha.clone()),
            ]),
        );
        Ok(JValue::Obj(out))
    }
}
