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

mod actor;
pub mod deps;
mod engine;
pub mod js;
pub mod kv;
mod long;
pub mod phases;
mod planned;
pub mod quality;
pub mod runner;
mod stages;

pub use deps::{Abort, Call, Context, Deps, Fail, Hooks, Quirks, Release, Spawn};
pub use runner::{Answer, Config, Runner, StageHooks, StartBody};

use js_json::{wtf16, JValue};
use sha2::{Digest, Sha256};

/// `hash(value)`: sha256 hex of `JSON.stringify(value)`.
pub fn hash(value: &JValue) -> String {
    let text = wtf16::stringify(value).unwrap_or_default();
    let digest = Sha256::digest(text.as_bytes());
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

/// `hash({ ...identity, profile: undefined })`: a model's identity without its profile part, so
/// a tune's own writes do not change it.
pub fn stable_identity(identity: &JValue) -> String {
    let mut copy = js::spread(&[identity]);
    js::set(&mut copy, "profile", JValue::Undefined);
    hash(&copy)
}

/// model-system.cjs `modelPathFromArgs(args)`: the argument after `--model`/`-m`.
pub fn model_path_from_args(args: &JValue) -> Option<String> {
    let JValue::Arr(args) = args else {
        return None;
    };
    let i = args
        .iter()
        .position(|a| matches!(a.as_str(), Some("--model" | "-m")))?;
    args.get(i + 1)
        .and_then(JValue::as_str)
        .filter(|p| !p.is_empty())
        .map(str::to_string)
}
