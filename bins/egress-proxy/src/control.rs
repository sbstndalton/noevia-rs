//! Controlled-mode implementation under construction.
//! Buffered Ready/renew dispatch now integrates authentication, replay, native
//! session and operation drain. No live listener/forwarding mode is enabled;
//! scoped revoke, retained activity/forget and transport qualification remain held.
pub mod wire;

// Session core is not wired to routing or forwarding yet.
pub mod session;

pub mod replay;

pub mod http;

pub mod engine;
