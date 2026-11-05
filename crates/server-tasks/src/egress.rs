//! Task-facing egress boundary. The integration owner supplies the native issuer adapter.
//! Grant/revoke/report shapes stay outside the lifecycle actor's transport assumptions.
use js_json::JValue;
use std::{future::Future, pin::Pin};
use zeroize::Zeroizing;

pub type Reply<'a, T> = Pin<Box<dyn Future<Output = Result<T, Unavailable>> + Send + 'a>>;
#[derive(Debug, Default)]
pub struct Unavailable {
    /// Only an explicitly public issuer message; never transport diagnostics or credentials.
    pub public_message: Option<String>,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Network {
    Unconfigured,
    Unavailable,
    Ready,
}
impl Network {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Unconfigured => "unconfigured",
            Self::Unavailable => "unavailable",
            Self::Ready => "ready",
        }
    }
}
/// No Debug/serialization: the lease token is only for the sandbox/browser engine channel.
pub struct Grant {
    pub task_id: String,
    pub handle: String,
    token: Zeroizing<String>,
}
impl Grant {
    pub fn new(task_id: String, handle: String, token: String) -> Self {
        Self {
            task_id,
            handle,
            token: Zeroizing::new(token),
        }
    }
    pub fn token(&self) -> &str {
        self.token.as_str()
    }
}
pub struct Cleanup {
    pub confirmed: bool,
    /// Full native network report; retained on the journal before forget is called.
    pub report: JValue,
}
pub trait Egress: Send + Sync {
    /// Authority failure is distinct from an authoritative unavailable result.
    fn network(&self) -> Result<Network, Unavailable>;
    fn endpoint(&self) -> &str;
    /// Implementations must own cleanup from issuance onward, even if this future is dropped
    /// before its response is observed. The actor must never retry an uncertain grant silently.
    fn grant<'a>(&'a self, task: &'a str, domains: &'a [String]) -> Reply<'a, Grant>;
    /// Idempotent, bound to the exact opaque grant handle; never revoke a successor task lease.
    fn revoke<'a>(&'a self, grant: &'a Grant) -> Reply<'a, Cleanup>;
    /// Called only after a final confirmed report with this snapshot id was durably retained.
    fn forget<'a>(&'a self, grant: &'a Grant, snapshot: &'a str) -> Reply<'a, bool>;
}
