//! Native usage reporting (M6) and, since track R12, the recording (`write`, used by the front only
//! with NOEVIA_RUST_USAGE_WRITES=1; otherwise Node remains the recording owner).
//! Inputs are synthetic in tests; production paths come only from validated identities.
pub const FILE_CAP: u64 = 2 * 1024 * 1024;
pub const ACCOUNT_CAP: usize = 1000;
pub const TOTAL_CAP: u64 = 32 * 1024 * 1024;
pub const KEY_CAP: usize = 65536;
pub const SAFE_INTEGER: f64 = 9_007_199_254_740_991.0;

pub mod summary;
#[derive(Debug, Clone)]
pub enum Fault {
    Limit,
    Clock,
    Io,
    InvalidUser,
    Database,
}
pub mod read;
pub mod write;

pub mod input;
