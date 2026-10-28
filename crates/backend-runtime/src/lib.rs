//! Experimental native execution implementation. Not wired into the production front.
//! Existing migration and activation gates still apply; this crate does not claim parity.
#![forbid(unsafe_code)]
pub mod api;
pub mod approvals;
pub mod chat;
pub mod dav;
pub mod diary;
pub mod diary_http;
#[cfg(target_os = "linux")]
pub mod frontend;
pub mod indexing;
pub mod journal;
pub mod mcp;
#[cfg(target_os = "linux")]
pub mod project_conversations;
pub mod replay;
pub mod retrieval;
pub mod session_conversations;
pub mod snapshot;
pub mod state;
pub mod tools;
pub mod transport;
use std::fmt;
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fault {
    Invalid,
    Unauthorized,
    Forbidden,
    Missing,
    Conflict,
    Capacity,
    Cancelled,
    Deadline,
    Database,
    Transport,
    Protocol,
    Unavailable,
}
impl fmt::Display for Fault {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "native backend operation refused: {self:?}")
    }
}
impl std::error::Error for Fault {}
impl From<rusqlite::Error> for Fault {
    fn from(_: rusqlite::Error) -> Self {
        Self::Database
    }
}
impl From<serde_json::Error> for Fault {
    fn from(_: serde_json::Error) -> Self {
        Self::Invalid
    }
}
pub type Result<T> = std::result::Result<T, Fault>;
/// A monotonic operation deadline and explicit cancellation shared by all stages.
#[derive(Clone)]
pub struct Control {
    deadline: std::time::Instant,
    cancelled: std::sync::Arc<std::sync::atomic::AtomicBool>,
}
impl Control {
    pub fn new(duration: std::time::Duration) -> Result<Self> {
        if duration.is_zero() {
            return Err(Fault::Invalid);
        }
        Ok(Self {
            deadline: std::time::Instant::now()
                .checked_add(duration)
                .ok_or(Fault::Invalid)?,
            cancelled: Default::default(),
        })
    }
    pub fn cancel(&self) {
        self.cancelled
            .store(true, std::sync::atomic::Ordering::Release);
    }
    pub fn check(&self) -> Result<()> {
        if self.cancelled.load(std::sync::atomic::Ordering::Acquire) {
            return Err(Fault::Cancelled);
        }
        if std::time::Instant::now() >= self.deadline {
            return Err(Fault::Deadline);
        }
        Ok(())
    }
    pub fn remaining(&self) -> Result<std::time::Duration> {
        self.check()?;
        Ok(self
            .deadline
            .saturating_duration_since(std::time::Instant::now()))
    }
}
