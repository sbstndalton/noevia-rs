//! Controlled-mode implementation under construction.
//! Only bounded operation-body admission exists here so far. No control route,
//! readiness, live-session, forwarding, lifecycle or production-mode change is enabled.
pub mod wire;

// Session core is not wired to routing or forwarding yet.
pub mod session;
