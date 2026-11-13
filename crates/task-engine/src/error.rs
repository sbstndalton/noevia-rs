//! Refusals, with the HTTP status and stable code the Node store answers with.

use std::fmt;

/// Why a store operation was refused. `status` and `code` match what `jobs.cjs` answers, so a host
/// can map them straight to a response.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EngineError {
    /// The job id is not 36 characters of lowercase hex and dashes (jobs.cjs `file()`).
    BadJobId,
    /// A field or event has the wrong shape, or a value is out of its allowed set.
    BadInput(&'static str),
    /// The event type is not in the journal vocabulary.
    UnknownType,
    /// A lifecycle authority event (`task.stage`, `task.revision`) on the public path.
    ServerOnly,
    /// No job with that id.
    NoSuchJob,
    /// The parent job of a child does not exist.
    NoSuchParent,
    /// A child job asks for a capability its parent does not hold.
    WidenCapabilities,
    /// The job already reached a terminal status.
    Finished,
    /// The event is not legal in the job's current status.
    IllegalTransition(&'static str),
    /// A journal that cannot be folded: bad sequence, mixed job ids, a hash chain, or no `job.created`.
    BadJournal(&'static str),
    /// An event or request is over its size limit.
    TooLarge(&'static str),
    /// A count limit (events per job, jobs per store, capabilities) was reached.
    Limit(&'static str),
}

impl EngineError {
    /// The HTTP status jobs.cjs uses for this refusal.
    pub fn status(self) -> u16 {
        match self {
            EngineError::BadJobId | EngineError::BadInput(_) | EngineError::UnknownType => 400,
            EngineError::ServerOnly | EngineError::WidenCapabilities => 403,
            EngineError::NoSuchJob | EngineError::NoSuchParent => 404,
            EngineError::TooLarge(_) => 413,
            EngineError::Finished
            | EngineError::IllegalTransition(_)
            | EngineError::BadJournal(_)
            | EngineError::Limit(_) => 409,
        }
    }

    /// A stable, machine-readable code.
    pub fn code(self) -> &'static str {
        match self {
            EngineError::BadJobId => "bad_job_id",
            EngineError::BadInput(_) => "bad_input",
            EngineError::UnknownType => "unknown_type",
            EngineError::ServerOnly => "server_only",
            EngineError::NoSuchJob => "no_such_job",
            EngineError::NoSuchParent => "no_such_parent",
            EngineError::WidenCapabilities => "widen_capabilities",
            EngineError::Finished => "finished",
            EngineError::IllegalTransition(_) => "illegal_transition",
            EngineError::BadJournal(_) => "bad_journal",
            EngineError::TooLarge(_) => "too_large",
            EngineError::Limit(_) => "limit",
        }
    }
}

impl fmt::Display for EngineError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            EngineError::BadInput(why)
            | EngineError::IllegalTransition(why)
            | EngineError::BadJournal(why)
            | EngineError::TooLarge(why)
            | EngineError::Limit(why) => write!(f, "{}: {why}", self.code()),
            _ => f.write_str(self.code()),
        }
    }
}

impl std::error::Error for EngineError {}
