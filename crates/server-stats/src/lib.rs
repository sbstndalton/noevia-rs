//! noevia-core's `GET /api/stats` in Rust (full-Rust migration M6, sbstndalton/noevia#1650):
//! `server/routes/models.cjs` (the handler and its 2 s shared read), `model-manager.cjs` and
//! `llamacpp-manager.cjs` (the reads), `llamacpp-metrics.cjs`, `mtp.cjs` `acceptance`,
//! `engine-stats.cjs` `reportedTokenRate`, `short-read.cjs` and http.cjs `fetchJson`.
//!
//! - [`engine`]: the engine from the environment Node reads, and one shared answer's reads.
//! - [`compose`]: the reply bytes Node sends for those reads.
//! - [`shared`]: the shared answer (concurrent polls share one read; kept 2 s after it lands).
//!
//! Where the port and Node differ (all documented at the item):
//!
//! - llama.cpp: the reads Node makes but never uses for that kind (`health()`, `listModels()`) are
//!   not made; the reply is the same.
//! - Lemonade: no "last response" MTP fallback (Node keeps those samples in its own memory). The
//!   front refuses the switch with that kind instead of answering differently.
//! - A JSON value nested 64 deep or more that the JS would look into is answered with 503
//!   ([`compose::Reply::unavailable`]) instead of a guess.
//! - Plain `http` engines only, no proxy, no compressed replies ([`engine`]).

#![forbid(unsafe_code)]

pub mod compose;
pub mod engine;
pub mod js;
pub mod metrics;
pub mod mtp;
pub mod shared;

pub use compose::Reply;
pub use engine::{Engine, Kind, Timeouts};
pub use shared::{SharedRead, STATS_TTL};

/// routes/models.cjs: a write (any method but GET and HEAD) under these paths drops the shared
/// answer (`/^\/api\/(models|model-manager|auto-roles)(\/|$)/`, on the WHATWG pathname).
pub fn write_invalidates(method: &str, pathname: &str) -> bool {
    if method == "GET" || method == "HEAD" {
        return false;
    }
    ["/api/models", "/api/model-manager", "/api/auto-roles"]
        .iter()
        .any(|p| {
            pathname
                .strip_prefix(p)
                .is_some_and(|rest| rest.is_empty() || rest.starts_with('/'))
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn which_writes_drop_the_shared_answer() {
        for (m, p) in [
            ("POST", "/api/models"),
            ("POST", "/api/models/load"),
            ("DELETE", "/api/models/x/y"),
            ("PUT", "/api/auto-roles"),
            ("OPTIONS", "/api/model-manager/sections/a"),
            ("PATCH", "/api/models/"),
        ] {
            assert!(write_invalidates(m, p), "{m} {p}");
        }
        for (m, p) in [
            ("GET", "/api/models/load"),
            ("HEAD", "/api/auto-roles"),
            ("POST", "/api/modelsx"),
            ("POST", "/api/model"),
            ("POST", "/api/stats"),
            ("PUT", "/api/auto-roles-x"),
            ("POST", "/x/api/models"),
        ] {
            assert!(!write_invalidates(m, p), "{m} {p}");
        }
    }
}
