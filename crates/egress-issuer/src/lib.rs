//! The native issuer of the code-sandbox egress proxy's control key and signed task grants, ported
//! from noevia-core `code-egress-control.cjs` (kept as the oracle in `tests/reference/`). The Rust
//! front is the only issuer: it generates, persists, publishes and rotates the control key, keeps
//! the authenticated session with the proxy (Ready/Renew/recovery), signs task grants and revokes
//! them. Node keeps no key material; it asks the front for a grant and hands the token to the task.
//!
//! - [`files`]: the published shared proxy key (mode 0440, unchanged contract) and the private
//!   issuer state (mode 0600), the key keeper that restores or replaces only its own key.
//! - [`wire`]: request/response framing, strict header and report validation.
//! - [`transport`]: the HTTP/1.1 exchange with the proxy.
//! - [`client`]: the session state machine (one Ready flight, renewal, grants, revoke, forget).
//!
//! No key material, token or path is ever put in an error, log line or `Debug` output.
#![forbid(unsafe_code)]

pub mod client;
pub mod files;
pub mod transport;
pub mod wire;

pub use client::{Client, ClientConfig, Clock, Handle, Revoked, SystemClock};
pub use files::{IssuerFiles, KeyKeeper, KeyState};
pub use transport::{Reply, TcpTransport, Transport, TransportRequest};

/// Why the issuer refused or could not complete an operation. The answer is always "native egress
/// is unavailable" (503); the reason is for diagnostics and tests, never a secret.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reason {
    Key,
    Identity,
    Integer,
    Request,
    Response,
    Headers,
    Shape,
    Scope,
    Snapshot,
    Summary,
    Endpoint,
    Transport,
    Timeout,
    Stale,
    Authentication,
    Uncertain,
    Proof,
    Epoch,
    Conflict,
    Ready,
    Renew,
    Clock,
    Closed,
    Unavailable,
    Task,
    Hosts,
    Handle,
    Forget,
    Publication,
    Report,
}

impl Reason {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Key => "key",
            Self::Identity => "identity",
            Self::Integer => "integer",
            Self::Request => "request",
            Self::Response => "response",
            Self::Headers => "headers",
            Self::Shape => "shape",
            Self::Scope => "scope",
            Self::Snapshot => "snapshot",
            Self::Summary => "summary",
            Self::Endpoint => "endpoint",
            Self::Transport => "transport",
            Self::Timeout => "timeout",
            Self::Stale => "stale",
            Self::Authentication => "authentication",
            Self::Uncertain => "uncertain",
            Self::Proof => "proof",
            Self::Epoch => "epoch",
            Self::Conflict => "conflict",
            Self::Ready => "ready",
            Self::Renew => "renew",
            Self::Clock => "clock",
            Self::Closed => "closed",
            Self::Unavailable => "unavailable",
            Self::Task => "task",
            Self::Hosts => "hosts",
            Self::Handle => "handle",
            Self::Forget => "forget",
            Self::Publication => "publication",
            Self::Report => "report",
        }
    }
}

/// `EgressControlError`: native egress is unavailable (HTTP 503, code `EGRESS_UNAVAILABLE`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ControlError(pub Reason);

impl std::fmt::Display for ControlError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("native egress is unavailable")
    }
}
impl std::error::Error for ControlError {}

pub(crate) fn err<T>(reason: Reason) -> Result<T, ControlError> {
    Err(ControlError(reason))
}

/// The capability string both ends carry (`egress::control::CAPABILITY`).
pub const CAPABILITY: &str = egress::control::CAPABILITY;
