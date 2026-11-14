//! The facts noevia-core still supplies to the Rust front, computed in Rust (track R11). Pure: the
//! caller reads the environment, the account's files and the inference machines registry and
//! passes them in; nothing here touches a file, the network or a clock.
//!
//! - [`provider`]: `providers.cjs` `engineCapabilities`, `parseCapabilities`,
//!   `effectiveCapabilities`, the registry's `getProvider(id)` over a merged view and
//!   `providerHeaders(provider, extra)`, and `provider-egress.cjs` `providerProjection` (the
//!   external verdict itself is the `provider-egress` crate's, as in core).
//! - [`code`]: what core answers on `POST /internal/code/engine` for one Code start
//!   (`rust-code-executor.cjs` `createCodeEngineBridge` as `index.cjs` wires it): `codeTargetFrom`
//!   (`code-machine.cjs`), `codeEngine`, `codeRoleEndpoint` and `roleFacts`, and the answer's
//!   assembly in core's order.
//! - [`shared`]: `shared-context.cjs` `forCode(project, loadChats(project.id))` with `projects.cjs`
//!   `sanitizeChats`.
//!
//! Recorded in `tests/fixtures/node-engine-facts.v1.json`, written by
//! `tests/record_engine_facts.cjs`, which runs those modules and index.cjs's own lines (cut out of
//! the pinned file) over stubs for the environment, the registry, the roles, the last loaded model,
//! the tune state, the feature flags and the project.
//!
//! Named differences:
//! - [`shared`]: a chat whose `updatedAt` is neither a number, `null`, `false` nor `true` (a string,
//!   an object) is sorted as 0 (Node coerces it, and a `NaN` leaves the order to V8's sort), as
//!   `chat-prepare`'s `forChat` does. Chat rows are written by the server with numbers.

pub mod code;
pub mod provider;
pub mod shared;

/// What Node throws where it would throw (the bridge answers 503).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Thrown;

/// JavaScript's `===` for two values that came out of `JSON.parse`: two objects or arrays are never
/// the same (they are different references).
pub fn strict_eq(a: &js_json::JValue, b: &js_json::JValue) -> bool {
    use js_json::JValue as J;
    match (a, b) {
        (J::Undefined, J::Undefined) | (J::Null, J::Null) => true,
        (J::Bool(x), J::Bool(y)) => x == y,
        (J::Num(x), J::Num(y)) => x == y,
        (J::Str(x), J::Str(y)) => x == y,
        _ => false,
    }
}
