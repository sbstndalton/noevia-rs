//! noevia-core's Code harness in Rust, its pieces so far: `server/code-meta.cjs` ([`meta`]),
//! `server/code-tool-schemas.cjs`, the Executor guard ([`tool_schemas`]), and code-harness.cjs's
//! `defaultFiles`, the agent's file API ([`files`]). The session itself (`createSession`) joins
//! them later. Nothing calls it yet.
//!
//! Values are JS values as [`js_json`] holds them in its WTF-16 form (a lone surrogate kept as its
//! mapped character, object keys in JS order), so every output keeps Node's key order and every
//! length, cut and comparison counts UTF-16 code units. The schema check runs the stream-guard
//! crate and a command is read by the code-actions crate, each through the same wire call the
//! dav-parse wasm module runs for Node, so those answers match by construction.
//!
//! The behaviour is pinned by `tests/fixtures/code-harness.v1.json`, recorded from the Node
//! modules themselves by `tools/gen-fixtures.cjs` (the sha256 of each source file is in the
//! fixture and in [`CODE_META_SHA256`] / [`CODE_TOOL_SCHEMAS_SHA256`]).
//!
//! # Differences from Node
//!
//! Each is tested against Node's own answer on the input that defines it (the fixture's `port`).
//!
//! - **D1, numbers that are not numbers.** Node's readers (`readUsage`, `readContext`,
//!   `readExitCode`, `readAgent`'s `protocolVersion`) run every value through `Number()`, so
//!   `null`, `''`, a whitespace-only string and `[]` read as 0, `[7]` and `['7']` as 7 (a
//!   harness's `exit_code: null` is recorded as success, a `null` token count as zero), and an
//!   object with its own `toString` key throws a TypeError out of the reader. That contradicts the
//!   module's own rule ("anything unreadable treated as absent", "rather than defaulting it to
//!   zero"; sbstndalton/noevia#2541). The port reads numbers and booleans as Node does and strings
//!   by `Number()` (hex, `Infinity` and all); every other value, and a blank string, is absent,
//!   exactly as if the key were missing.
//! - **D2, a primitive conversion that throws.** Where the guard converts an agent's value to a
//!   primitive, Node's `String(toolCall.kind)` (an unknown kind) and a write's
//!   `params.line < 1` / `params.limit < 1` (which the write schema does not constrain) throw a
//!   TypeError for a JSON object with its own `toString` key, or an array holding one, so the
//!   malformed call crashes the guard method instead of being refused. The port refuses a write
//!   whose `line` or `limit` would throw, as one below 1 (never letting through what Node's throw
//!   stopped), and reads an unknown kind as if that key were absent (`"[object Object]"`), which
//!   is refused like any other unknown kind.
//! - **D3, names on `Object.prototype`.** Node looks kinds and methods up on plain objects, so
//!   `toolCallSchema('constructor')` (`toString`, `__proto__`, ...) gets a function or
//!   `Object.prototype` as its `rawInput` schema, and `checkFsCall('constructor', ...)` validates
//!   against a function and answers "Arguments could not be checked." with a function as its hint.
//!   The port treats such a name as unknown: `other`'s `rawInput`, and "Unknown file method".
//!
//! Not differences, but worth knowing (kept as Node has them): a write's unread params (an ACP
//! `_meta`, say) are validated, so an over-deep or over-2 MiB extra key is refused while the same
//! key on a read is projected away; `think`/`switch_mode` validate their whole `rawInput`; and
//! `codingIdentity` throws, as Node does, for a `null` argument or capabilities it cannot spread
//! or sort.
#![forbid(unsafe_code)]

pub mod executor;
pub mod files;
mod js;
pub mod meta;
pub mod pipeline_host;
pub mod review_gate;
pub mod service;
pub mod session;
pub mod tool_schemas;

pub use meta::{
    coding_identity, identity_hash, read_agent, read_context, read_exit_code, read_usage, summarize,
};
pub use tool_schemas::{
    check_fs_call, check_tool_call, correction_for, fs_schema, fs_schemas, raw_input_schema,
    raw_input_schemas, reject_outcome, tool_call_schema, violation_error, violation_text,
    BlockedError, ExecutorGuard, ExecutorGuardFlag, FlagCheck, GuardHooks, GuardOptions, NoHooks,
    RpcError, ACP_KINDS, MAX_VIOLATIONS, PATH_KEYS,
};

/// sha256 of the `server/code-meta.cjs` this port follows.
pub const CODE_META_SHA256: &str =
    "1fb1a895f1f2db33d9e075848935167dce774b9a6ea4f0d1c5ca10621831ba8b";
/// sha256 of the `server/code-tool-schemas.cjs` this port follows.
pub const CODE_TOOL_SCHEMAS_SHA256: &str =
    "a6712df74379166d019cec892574c7cc77ce86d801e95a16e3f8059a86561ade";

/// What a JS call threw: an `Error` (its `name` and `message`) or any other value.
#[derive(Debug, Clone, PartialEq)]
pub enum Thrown {
    Error { name: String, message: String },
    Value(js_json::JValue),
}

impl Thrown {
    /// `new Error(message)`.
    pub fn error(message: impl Into<String>) -> Self {
        Thrown::Error {
            name: "Error".into(),
            message: message.into(),
        }
    }

    /// A `TypeError` with V8's message.
    pub fn type_error(message: impl Into<String>) -> Self {
        Thrown::Error {
            name: "TypeError".into(),
            message: message.into(),
        }
    }
}
