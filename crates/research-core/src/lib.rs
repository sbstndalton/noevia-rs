//! Deep research's own rules (docs/spec-deep-research.md in noevia), moved from core research-sources.cjs,
//! research-plan.cjs, research-runner.cjs and research-service.cjs (sbstndalton/noevia#2218). No I/O: the model,
//! the search, the page reader and the journal are the caller's. `tests/differential.rs` holds them to the
//! answers Node gave (`tests/fixtures/record.cjs`).
pub mod plan;
pub mod report;
pub mod runner;
pub mod sources;
