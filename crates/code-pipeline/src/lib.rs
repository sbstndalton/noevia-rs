//! noevia-core's Code pipeline in Rust, its model-free and I/O-free pieces: from
//! `server/code-pipeline.cjs` the constants, the Executor's brief (`renderExecutorBrief`), the
//! plan hash (`planHashOf`), the stage-deadline text, the feedback after failed tests or a
//! `request_changes` verdict, `loop()`, the merge decision, the plan and accept approval cards
//! and the result objects ([`pipeline`], [`brief`]); from `server/planner-plan.cjs` the Planner's
//! instructions and caps, `validatePlanArtifact` and its in-session failures ([`planner`]); from
//! `server/code-review.cjs` the reviewer's ([`review`]); from `server/auditor-report.cjs`
//! `readAudit` and `buildAuditReport` ([`audit`]). The async driver (`createCodePipeline`'s
//! `drive`) is built on these later; nothing calls them yet. No async runtime, file system or
//! network here, and no panics on any input.
//!
//! Values are JS values as [`js_json`] holds them in its WTF-16 form (a lone surrogate kept as its
//! mapped character, object keys in JS order), so every output keeps Node's key order, every
//! length and cut counts what Node counts (UTF-16 units for `.length`/`.slice`, code points where
//! Node uses `Array.from`), and `${}` and `join` convert as `String()` does. A review verdict is
//! the review-verdict crate's [`Verdict`] (`readVerdict`'s output). `pipelineView` is ported in
//! server-projects (`task_journals.rs`); role-context's own `serialize` works on its projection
//! type, so the canonical form of a plan (any JSON value) is [`brief::canonicalize`] here.
//!
//! The behaviour is pinned by `tests/fixtures/code-pipeline.v1.json`, recorded from the Node
//! modules by `tools/gen-fixtures.cjs` (the sha256 of each source file is in the fixture and in
//! the `*_SHA256` constants). Exported functions are called as they are; what code-pipeline.cjs
//! builds inline (and the three byte/token caps the modules do not export) is cut out of the
//! source text between markers and run by the generator, which says so in its header.
//!
//! # Differences from Node
//!
//! None in behaviour. The inputs are typed where Node's are by construction (shas, branch and
//! file names are strings, revisions and loop counts integers, a merge preflight is a closure),
//! and `undefined` passed to [`plan_hash`] is `None` where Node's hash throws. Kept as Node has
//! them: "within 1 seconds" in the timeout reasons, and an own `__proto__` key dropped from the
//! plan hash's canonical form (Node's copy sets a prototype instead).
#![forbid(unsafe_code)]

pub mod audit;
pub mod brief;
mod js;
pub mod pipeline;
pub mod planner;
pub mod review;

pub use audit::{
    audit_schema, build_audit_report, is_full_sha, read_audit, AuditCheck, AuditEvidenceItem,
    AuditInput, AuditReport, AuditReportError, AuditWriteUp, AuditWriteUpError, AUDIT_INSTRUCTIONS,
    AUDIT_SOURCES,
};
pub use brief::{canonicalize, plan_hash, render_executor_brief, serialize_projection};
pub use pipeline::{
    accept_card, accept_result, blocked, blocked_result, capability_text, is_accepted, merge_offer,
    merge_refused, merge_turned_off_result, merged_result, merged_stage_reason, minutes,
    next_round, plan_card, review_feedback, should_merge, stage_deadline_message,
    tests_failed_feedback, Accept, Deadlines, Round, Withheld, CAPABILITY_TEXT, DEADLINES,
    EXECUTOR_INSTRUCTIONS, EXPECTED_ARTIFACTS, MAX_LOOPS, PIPELINE_META, PLAN_ACTION,
};
pub use planner::validate_plan_artifact;
pub use review::{read_verdict, REVIEW_ACTION};
pub use review_verdict::{Finding, Invalid, Verdict};

/// sha256 of the `server/code-pipeline.cjs` this port follows.
pub const CODE_PIPELINE_SHA256: &str =
    "18d7e6ce2f3e779345d0789af10c5aa0bd056501f8ac55659ec6c3c3a0b339b5";
/// sha256 of the `server/planner-plan.cjs` this port follows.
pub const PLANNER_PLAN_SHA256: &str =
    "b37efeaae70136bd50786bfa71bf11d49903e7a1290879f35fb9c9659083ee03";
/// sha256 of the `server/code-review.cjs` this port follows.
pub const CODE_REVIEW_SHA256: &str =
    "4cc944fdffd5d7c374257d8b301252ec947ac1fb729006c53895fe0db293287f";
/// sha256 of the `server/auditor-report.cjs` this port follows.
pub const AUDITOR_REPORT_SHA256: &str =
    "330b43dfeb73491ef4d181a4d49b2b1597bfd561062fc10426cf48b5be250f6e";
/// sha256 of the `server/role-context.cjs` whose `serializeProjection` the plan hash follows.
pub const ROLE_CONTEXT_SHA256: &str =
    "0c817f6de59bb3e623905929183b4f3b0c1a3fe0321523c49096a17fda6c2c22";
/// sha256 of the `server/code-actions.cjs` whose `ACTIONS` name the capabilities.
pub const CODE_ACTIONS_SHA256: &str =
    "a8a4b12d142ac3bd20caf39cabc8eb836ecfe19d6fc690f38e7ba056fa32b4a5";
/// sha256 of the `server/code-review-verdict.cjs` the fixture's verdicts were read by.
pub const CODE_REVIEW_VERDICT_SHA256: &str =
    "fde7e49afb0a60ee035036d83fc984426017d9519c283d55023b2e8773c328e2";

/// Where Node throws a TypeError (a property read of `null`/`undefined`, a missing array method,
/// or `String()` of an object with its own `toString` key).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TypeError;

/// A fixed `{ ok: false, code, reason }` answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Failure {
    pub code: &'static str,
    pub reason: &'static str,
}
