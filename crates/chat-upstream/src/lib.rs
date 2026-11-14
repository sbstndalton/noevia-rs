//! The provider call path of a native chat turn: what noevia-core `chat.cjs` does between the
//! settled route and the first byte sent to the provider (lines ~1009-1440 and the call sites at
//! ~1911/2116/2277), in Rust, for the turns `rust-chat-native.cjs` still refuses as `provider` and
//! `provider-key`.
//!
//! - [`target`]: the upstream address (`upstreamUrl`), the member-own-provider rule and
//!   `ssrf.cjs` `endpointApproved`, which fetch carries the call (direct, an Inference machine, the
//!   sign-in broker), the bearer rule of `providerHeaders`, and the texts Node answers with.
//! - [`binding`]: the turn-bound call binding (track R8 decision 19's design): what the cut pins
//!   (user, provider, address, row fingerprint, model, chat key, expiry), a MAC'd token for it,
//!   and the per-request re-check against the row as it is now (fail closed on a changed row).
//! - [`envelope`]: the response headers that may cross a relay (an allowlist) and the upstream
//!   status envelope, so a relay's own refusal is never read as the provider's.
//! - [`plan`]: one request as the host sends it (method, headers rebuilt from the key as it is now,
//!   redirects refused, the call site's deadline).
//! - [`relay`]: one relayed request as a pure decision (shape, token, expiry, re-check, pinned
//!   model, request), with typed refusals and the transport-error answer.
//! - [`refusal`]: the `provider`/`provider-key` checks of `refusal()` and what a front that runs
//!   this path (a future `providers` caps token) may lift: external rows only, never machines,
//!   the sign-in provider or a route target in this slice.
//!
//! - [`host`]: the native turn's host for a provider-row turn: wraps the front's host, sends each request to
//!   the bound address with the row's key as it is now, re-checks the binding first, never sends the local
//!   lease, refuses redirects, and skips the local model manager as chat.cjs does for a non-default row.
//! - [`cut`]: what a front calls once at the cut (resolve, then bind) and before each request (the re-check's
//!   facts by the same rules).
//! - [`limit`]: the runtime limit of a provider-row turn (`hostedLimit`: the row's stated context, else the
//!   hosted default), which a provider-row frame must carry.
//!
//! Nothing calls this crate yet: production behaviour is unchanged until a front wires it behind
//! a caps token core also understands.

pub mod binding;
pub mod cut;
pub mod envelope;
pub mod host;
pub mod limit;
pub mod plan;
pub mod refusal;
pub mod relay;
pub mod target;
