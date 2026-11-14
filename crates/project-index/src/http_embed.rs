//! The `/v1/embeddings` request of rag.cjs `embedOnce` for index chunks, over the native embedding
//! worker's guarded HTTP boundary (`embedding_worker::http::Http`: no proxy, no redirect, private,
//! loopback or carrier-grade addresses only, checked at connect time, capped bodies).
//!
//! What it keeps from Node:
//! - the address: the target's own base URL, else the inference engine's; trailing slashes and one
//!   `/v1` are cut, then `/v1/embeddings` is added;
//! - the headers: an explicit address never receives the inference credentials (Node sends only
//!   `Content-Type` there); the inference engine gets its bearer token;
//! - the body: `{"model": <target model>, "input": [...]}`;
//! - the answer: `data` when it is an array, else no items (the batch rules then apply);
//! - a 16 MiB cap on the answer and no redirect (Node: `redirect: 'error'`).
//!
//! Named differences: a refused answer reads `embeddings <status>` without Node's first 120 characters
//! of the body (the worker's boundary keeps response text out of diagnostics), and an error body that
//! is not JSON is reported as the boundary's fixed label; a request is cut after 60 s (Node waits for
//! its abort signal); the embedding worker's foreground counter is not entered here (the caller's
//! `Host::foreground_busy` already decides).

use crate::slots::Target;
use embedding_worker::http::Http;
use serde_json::{json, Value};
use std::time::Duration;

/// Node's 16 MB answer cap (#920).
pub const ANSWER_CAP: usize = 16 * 1024 * 1024;
/// The request deadline (named difference).
pub const DEADLINE: Duration = Duration::from_secs(60);

/// Where one target's embeddings go, as the server resolves it.
#[derive(Clone)]
pub enum Endpoint {
    /// The target's own `baseUrl`: no credentials are sent.
    Explicit(String),
    /// The inference engine's address and its bearer token (if any).
    Inference { base: String, token: Option<String> },
}

impl std::fmt::Debug for Endpoint {
    // Never print a token.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Explicit(base) => f.debug_tuple("Explicit").field(base).finish(),
            Self::Inference { base, token } => f
                .debug_struct("Inference")
                .field("base", base)
                .field("token", &token.as_ref().map(|_| "<set>"))
                .finish(),
        }
    }
}

/// `${base.replace(/\/+$/, '').replace(/\/v1$/, '')}/v1/embeddings`.
pub fn embeddings_url(base: &str) -> String {
    let trimmed = base.trim_end_matches('/');
    let trimmed = trimmed.strip_suffix("/v1").unwrap_or(trimmed);
    format!("{trimmed}/v1/embeddings")
}

/// One `/v1/embeddings` call per batch, made from a blocking thread on `handle`'s runtime.
pub struct HttpEmbed<R>
where
    R: Fn(&Target) -> Result<Endpoint, String>,
{
    http: Http,
    handle: tokio::runtime::Handle,
    resolve: R,
}

impl<R> HttpEmbed<R>
where
    R: Fn(&Target) -> Result<Endpoint, String>,
{
    pub fn new(http: Http, handle: tokio::runtime::Handle, resolve: R) -> Self {
        Self {
            http,
            handle,
            resolve,
        }
    }

    /// `embedOnce(target, input)`: the answer's `data` items. Must not be called on a runtime worker
    /// thread (it blocks on `handle`); the indexer runs on blocking threads.
    pub fn call(&self, target: &Target, input: &[String]) -> Result<Vec<Value>, String> {
        let (base, token) = match (self.resolve)(target)? {
            Endpoint::Explicit(base) => (base, None),
            Endpoint::Inference { base, token } => (base, token),
        };
        let body = json!({ "model": target.model, "input": input }).to_string();
        let url = embeddings_url(&base);
        let (status, answer) = self.handle.block_on(self.http.post_json(
            &url,
            token.as_deref(),
            body,
            DEADLINE,
            ANSWER_CAP,
        ))?;
        if !(200..300).contains(&status) {
            return Err(format!("embeddings {status}"));
        }
        Ok(match answer.get("data") {
            Some(Value::Array(items)) => items.clone(),
            _ => Vec::new(),
        })
    }
}
