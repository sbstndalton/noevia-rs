//! The Diary's chunk index (ported from `diary/agent/retrieval.py` and `vector_slots.py`) and the
//! embedding profile, configuration and address guard (`diary/agent/embedding.py`, see [`embedding`]).
//!
//! One SQLite file per tenant holds the chunks of every month file and, per embedding model, a
//! vector slot (`vec_meta 'slot:<id>'`, a `vec0` table and a content-hash map). Search and
//! indexing use only the active model's slot; another model's slot can be filled beside it and a
//! rollback still finds the old one. The layout is byte-compatible with the Python service, which
//! keeps running on the same files while this crate is behind its switch.
//!
//! Locking stays with the caller: the Python service holds a per-instance lock and the tenant's
//! index-publication guard around writes. Here every write runs inside [`Publication::run`] and
//! the embedding calls can be made outside any lock with [`index::Retriever::plan`] /
//! [`index::PendingPlan`] and [`index::Retriever::search_slot`].
//!
//! Nothing calls this crate in production yet.

use sha2::{Digest, Sha256};

pub mod embedding;
pub mod index;
pub mod pyjson;
pub mod slots;

/// An embedding model identity. Equality covers the address too (Python dataclass equality), but
/// the stored vector identity is only profile and model ([`Target::slot_id`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    pub base_url: Option<String>,
    pub model: String,
    pub profile: String,
}

pub const DEFAULT_PROFILE: &str = "nomic-v1";

impl Target {
    pub fn new(base_url: Option<&str>, model: &str, profile: &str) -> Self {
        Self {
            base_url: base_url.map(str::to_owned),
            model: model.to_owned(),
            profile: profile.to_owned(),
        }
    }

    /// `profile \0 model`.
    pub fn identity(&self) -> String {
        format!("{}\0{}", self.profile, self.model)
    }

    /// First 16 hex digits of sha256(identity).
    pub fn slot_id(&self) -> String {
        let digest = Sha256::digest(self.identity().as_bytes());
        let mut out = String::with_capacity(16);
        for byte in digest.iter().take(8) {
            out.push_str(&format!("{byte:02x}"));
        }
        out
    }
}

/// Fixed messages only: no entry text, path or provider body ever reaches one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    /// A SQLite call failed (the Python service raises `sqlite3.Error`).
    Sqlite(String),
    /// `ensure_slot` was asked for a width outside 1..=65536 (Python `ValueError`).
    BadWidth(usize),
    /// The tenant's publication guard refused the write (Python `WriterBusy`).
    Busy(String),
    /// Stored data the Python service also fails on with an uncaught error.
    Value(&'static str),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Sqlite(code) => write!(f, "diary index sqlite error ({code})"),
            Self::BadWidth(dim) => write!(f, "bad vector width {dim}"),
            Self::Busy(reason) => write!(f, "diary index writer busy ({reason})"),
            Self::Value(what) => write!(f, "diary index data error ({what})"),
        }
    }
}

impl std::error::Error for Error {}

impl From<rusqlite::Error> for Error {
    fn from(err: rusqlite::Error) -> Self {
        // The sqlite error code only: driver messages can quote SQL or values.
        let code = match &err {
            rusqlite::Error::SqliteFailure(e, _) => format!("{:?}", e.code),
            _ => "client".to_owned(),
        };
        Self::Sqlite(code)
    }
}

pub type Result<T> = std::result::Result<T, Error>;

/// The guard around every index write (Python `tenant_writer.index_publication`). A managed
/// tenant's implementation takes the backup barrier on its primary database; [`NoGuard`] is for a
/// legacy or non-managed index file, which the Python service also writes unguarded.
pub trait Publication {
    fn run(&self, write: &mut dyn FnMut() -> Result<()>) -> Result<()>;
}

pub struct NoGuard;

impl Publication for NoGuard {
    fn run(&self, write: &mut dyn FnMut() -> Result<()>) -> Result<()> {
        write()
    }
}
