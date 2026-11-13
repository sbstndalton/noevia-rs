//! code-pipeline-verify.cjs: the pipeline's view of server-measured verification (#705 from
//! #703). One small adapter, so the pipeline never depends on the verifier's internals.
//!
//! What the adapter adds is distrust. Only `passed` or `failed` with a report the verifier itself
//! vouches for ([`Verifier::is_measured`] for exactly this task, commit and revision) is ever a
//! test result. Everything else is `unavailable` or `error`, and the pipeline goes `blocked` on
//! both: a missing, unreachable or unbound verification is never a pass.
//!
//! A verifier that panics is Node's verifier that throws: `verifier_threw`.

use crate::{Event, Outcome, Report, Request, Status, VerifyError};
use js_json::wtf16;
use std::panic::{catch_unwind, AssertUnwindSafe};

/// The contract the adapter relies on (code-verify.cjs, #703): [`crate::Verify`] is the real one.
pub trait Verifier: Send + Sync {
    fn available(&self) -> bool;
    fn run(&self, request: &Request<'_>, emit: &mut dyn FnMut(&Event<'_>)) -> Outcome;
    fn is_measured(&self, report: &Report, task_id: &str, head_sha: &str, revision: f64) -> bool;
}

fn unavailable(message: &str) -> Outcome {
    Outcome {
        status: Status::Unavailable,
        report: None,
        error: Some(VerifyError {
            code: "no_verifier".into(),
            message: message.into(),
        }),
    }
}

fn error(code: &str, message: &str) -> Outcome {
    Outcome {
        status: Status::Error,
        report: None,
        error: Some(VerifyError {
            code: code.into(),
            message: message.into(),
        }),
    }
}

/// `createVerifyAdapter(verify)`.
pub struct Adapter {
    verify: Option<Box<dyn Verifier>>,
}

impl Adapter {
    /// `None` when this server has no verifier (then every run is `unavailable`).
    pub fn new(verify: Option<Box<dyn Verifier>>) -> Adapter {
        Adapter { verify }
    }

    pub fn available(&self) -> bool {
        self.verify
            .as_ref()
            .is_some_and(|v| catch_unwind(AssertUnwindSafe(|| v.available())).unwrap_or(false))
    }

    pub fn run(&self, request: &Request<'_>, emit: &mut dyn FnMut(&Event<'_>)) -> Outcome {
        let verify = match self.verify.as_ref() {
            Some(v) if self.available() => v,
            _ => {
                return unavailable(
                    "Verification is not set up on this server (the verifier container, CODE_VERIFY_ENDPOINT).",
                )
            }
        };
        let Ok(outcome) = catch_unwind(AssertUnwindSafe(|| verify.run(request, emit))) else {
            return error("verifier_threw", "The verifier failed unexpectedly.");
        };
        let reason = outcome
            .error
            .as_ref()
            .map(|e| wtf16::slice(&e.message, 300))
            .filter(|r| !r.is_empty());
        match outcome.status {
            Status::Unavailable => {
                unavailable(reason.as_deref().unwrap_or("Verification is unavailable."))
            }
            Status::Error => {
                let code = outcome
                    .error
                    .as_ref()
                    .map(|e| e.code.as_str())
                    .filter(|c| !c.is_empty())
                    .unwrap_or("error");
                error(
                    &wtf16::slice(code, 40),
                    reason.as_deref().unwrap_or("Verification could not run."),
                )
            }
            Status::Passed | Status::Failed => {
                let measured = outcome.report.as_ref().is_some_and(|r| {
                    // The verifier's own say, and this crate's binding to the request.
                    crate::is_measured(r, request.task_id, request.head_sha, request.revision)
                        && catch_unwind(AssertUnwindSafe(|| {
                            verify.is_measured(
                                r,
                                request.task_id,
                                request.head_sha,
                                request.revision,
                            )
                        }))
                        .unwrap_or(false)
                });
                match outcome.report {
                    Some(report) if measured => Outcome {
                        // Re-derived here rather than trusted from `status`: a "passed" over a
                        // failing report is not one.
                        status: if report.passed() {
                            Status::Passed
                        } else {
                            Status::Failed
                        },
                        report: Some(report),
                        error: None,
                    },
                    _ => error(
                        "unmeasured",
                        "The test report was not measured by the verifier for this task, commit and revision.",
                    ),
                }
            }
        }
    }
}
