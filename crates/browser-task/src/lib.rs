//! Browser mode as a project feature (core server/browser-service.cjs, browser-executor.cjs and the
//! answers of routes/browser.cjs), in Rust for the front (Rust port track R5, shift 2).
//!
//! One task is one durable job (kind `browser`, server-projects' task store, the same journals the
//! front's task reads already serve) that holds one browser session open for its life. Actions
//! arrive one at a time ([`service::Service::act`]) and run in order; every consequential one stops
//! at noevia's approval card, answered through [`service::Service::decide`], exactly as Node's do.
//! Approvals live in memory on purpose: a restart re-asks.
//!
//! What runs the browser is [`engine`]: a trait shaped like the Playwright calls core's executor
//! makes (launch, an isolated context with the task's egress proxy, request and WebSocket routing,
//! the page and element operations). The policy (crate browser-policy, as core's browser-policy.cjs
//! applies it), the facts read off the page, the approval binding, the size and time limits and what
//! is written to the journal are this crate's; the engine only carries actions out.
//!
//! Recorded parity: tests/fixtures/service-node.v1.json is core's own routes/browser.cjs over
//! browser-service.cjs, browser-executor.cjs, browser-policy.cjs (dav-parse.wasm) and jobs.cjs with
//! a scripted fake Playwright (tools/gen-service-fixtures.cjs); tests/service.rs replays it here with
//! the same fake behind [`engine`].

pub mod engine;
pub mod executor;
pub mod policy;
pub mod service;
pub mod signal;

use js_json::JValue;

/// An error with core's shape: `status` (none: a 500 without detail), the message, the public
/// message the route shows, and a result to record with a failed job.
#[derive(Debug, Clone, PartialEq)]
pub struct Fail {
    pub status: Option<u16>,
    pub message: String,
    pub public: Option<String>,
    pub result: Option<JValue>,
}

impl Fail {
    /// core's `fail(status, message)`: the message is public too.
    pub fn public(status: u16, message: &str) -> Fail {
        Fail {
            status: Some(status),
            message: message.to_string(),
            public: Some(message.to_string()),
            result: None,
        }
    }

    /// A plain `Error(message)`, optionally with a status.
    pub fn plain(status: Option<u16>, message: &str) -> Fail {
        Fail {
            status,
            message: message.to_string(),
            public: None,
            result: None,
        }
    }

    /// routes/browser.cjs's answer for it: `error.status || 500` and
    /// `error.publicMessage || (error.status ? error.message : 'Browser request failed')`.
    pub fn answer(&self) -> (u16, String) {
        let status = self.status.filter(|s| *s != 0).unwrap_or(500);
        let text = match &self.public {
            Some(p) if !p.is_empty() => p.clone(),
            _ if self.status.is_some_and(|s| s != 0) => self.message.clone(),
            _ => "Browser request failed".to_string(),
        };
        (status, text)
    }
}
