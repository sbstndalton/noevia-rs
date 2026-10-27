//! Controlled-mode implementation under construction.
//! Buffered dispatch integrates native sessions, scoped revoke/drain, retained
//! ownership/reports and acknowledged privacy cleanup. No live listener/forwarding
//! mode or production transport qualification is enabled here.
pub mod wire;

// Session core is not wired to routing or forwarding yet.
pub mod session;

pub mod replay;

pub mod http;

pub mod engine;

mod leases;

pub mod keyfile;

pub mod transport;
