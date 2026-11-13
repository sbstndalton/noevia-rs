//! The native model auto-tune runner (sbstndalton/noevia R6, #2516): noevia-core
//! `server/llamacpp-full-autotune.cjs` (`createFullAutotuner`) in Rust.
//!
//! The runner is the one writer of `native-autotune-<hash>.json` (its job, history and per-model
//! settings). It keeps the job as JS-ordered JSON ([`js_json::JValue`]) so the file's bytes are the
//! ones Node's `JSON.stringify` writes: Node's actor and this one read each other's file, and a
//! rollback of the implementation switch leaves a job either can show and resume.
//!
//! Oracle: traces of the Node runner recorded at its dependency boundary
//! (`tests/fixtures/record-node.cjs`, format in sbstndalton/noevia's R6 handoff
//! `R6-trace-format.md`): every router call, profile write, hold, calibrator call and state-file
//! save in order. The pure parts here are also checked against Node's exported functions
//! (`tests/pure`).
//!
//! The decisions the Node runner already took from Rust (the planner `autotune-plan`, the
//! foreign-client decision `tune-contention`, the load verdict, the long profile, the serving
//! verdict) stay in those crates; this crate only orders the work around them.

pub mod js;
pub mod kv;
pub mod phases;
pub mod quality;
