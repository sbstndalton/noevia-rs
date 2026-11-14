//! noevia-core's embedding model switch in Rust (track R11): the decisions behind
//! `GET`/`POST /api/embedding`, over a [`Host`] the front implements.
//!
//! - [`target`]: `embedding-config.cjs` `normalizeTarget`, `seedFromEnv`, identity and
//!   `sameTarget`; `embedding-profile.cjs` names; `embedding-served.cjs`'s verdict on a
//!   `/v1/models` list.
//! - [`config`]: the active / next / previous targets, `embedding-config.json` version 1 (read,
//!   the damaged-file stand-in of `loadEmbeddingConfig`, atomic write), the transitions.
//! - [`migration`]: `embedding-migration.cjs` in the native-worker mode production runs (the
//!   re-index pass itself is `crates/embedding-worker`): set-next, clear-next, switch (served
//!   check, endpoint parity, coverage), rollback, finish, which vectors are dropped, `status()`.
//! - [`routes`]: `routes/embedding.cjs`: the administrator check, `apply()` (request reading is
//!   `embedding-admin`'s), the projects and Diary halves, the audit record, the service sync,
//!   `servedSet()` and the `service-switch` / `service-off` actions.
//!
//! Everything outside the process (the config lock, the native worker, the index files, the
//! embeddings servers, the Diary, the embedding service, the audit log) is a [`Host`] call, made in
//! the Node code's order; the caller runs one request at a time (Node's `serialized` turn and the
//! single-threaded event loop). Nothing here logs. Recorded in
//! `tests/fixtures/node-embedding-migration.v1.json` by `tests/record.cjs`.

#![forbid(unsafe_code)]

pub mod config;
pub mod migration;
pub mod routes;
pub mod target;

pub use config::{Config, Source, State};
pub use migration::{Coverage, Migration, Pacing};
pub use routes::{Answer, Embedding, HttpReply, ServeError, User};
pub use target::Target;

use js_json::{wtf16, JValue};

/// An error the projects side throws (`Object.assign(Error(message), { status, code, ... })`).
#[derive(Debug, Clone, PartialEq)]
pub struct Failure {
    pub message: String,
    /// `None` is an error without a status (a lock or write failure): the answer says 500.
    pub status: Option<u16>,
    pub code: Option<&'static str>,
    /// The `coverage`, `parity` or `served` field a refusal carries (at most one).
    pub detail: Option<Box<Detail>>,
}

impl Failure {
    pub(crate) fn new(status: u16, code: &'static str, message: impl Into<String>) -> Self {
        Failure {
            status: Some(status),
            code: Some(code),
            ..Failure::plain(message)
        }
    }

    pub(crate) fn plain(message: impl Into<String>) -> Self {
        Failure {
            message: message.into(),
            status: None,
            code: None,
            detail: None,
        }
    }

    /// `projects()`'s error object: `{ error (300 UTF-16 units), status, code?, coverage?,
    /// parity?, served? }`.
    pub fn to_json(&self) -> JValue {
        let mut out = vec![
            (
                "error".to_string(),
                JValue::Str(wtf16::slice(&self.message, 300)),
            ),
            (
                "status".to_string(),
                JValue::Num(f64::from(self.status.unwrap_or(500))),
            ),
        ];
        if let Some(code) = self.code {
            out.push(("code".into(), code.into()));
        }
        match self.detail.as_deref() {
            Some(Detail::Coverage(v)) => out.push(("coverage".into(), v.clone())),
            Some(Detail::Parity(v)) => out.push(("parity".into(), v.clone())),
            Some(Detail::Served(s)) => out.push(("served".into(), served_json(s))),
            None => {}
        }
        JValue::Obj(out)
    }
}

impl Failure {
    pub(crate) fn with(mut self, detail: Detail) -> Self {
        self.detail = Some(Box::new(detail));
        self
    }

    /// The model ids a `modelNotServed` refusal echoes.
    pub fn served(&self) -> Option<&[String]> {
        match self.detail.as_deref() {
            Some(Detail::Served(served)) => Some(served),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Detail {
    /// `coverageOf(next)` of a `notReady` switch.
    Coverage(JValue),
    /// `endpointsAgree()` of a `parityFailed` switch.
    Parity(JValue),
    /// The listed ids of a `modelNotServed` refusal.
    Served(Vec<String>),
}

pub(crate) fn served_json(served: &[String]) -> JValue {
    JValue::Arr(served.iter().map(|s| JValue::Str(s.clone())).collect())
}

/// What the Node code reaches outside itself, one method per stub of the recording. The caller
/// implements it with its own I/O (and `block_on` where that is async).
pub trait Host {
    /// One project index (rag.cjs `listIndexes` entry).
    type Index: Clone;

    /// `embedding-config-lock.cjs` `withFlock(<config file>.lock)`: the OS lock the native
    /// worker publishes under. An `Err` is the reason it could not be taken (nothing ran).
    fn lock(&mut self) -> Result<(), String>;
    /// Releases what [`Host::lock`] took (also after a refusal inside the hold).
    fn unlock(&mut self);
    /// The native worker's re-index request (`nativeOwner.kick()`).
    fn kick(&mut self);
    /// The native worker's status file when current (`nativeOwner.snapshot()`), else `None`.
    fn snapshot(&mut self) -> Option<JValue>;
    fn indexes(&mut self) -> Vec<Self::Index>;
    /// `rag.indexCoverage(entry, target)`: `None` when the index cannot be read.
    fn coverage(&mut self, index: &Self::Index, target: &Target) -> Option<Coverage>;
    /// `rag.dropIndexSlot(entry, target)`; an error is ignored (Node logs it).
    fn drop_slot(&mut self, index: &Self::Index, target: &Target) -> Result<(), String>;
    /// `rag.servedModels({ baseUrl, model })`: the ids `/v1/models` lists, `None` when it threw.
    fn served_models(&mut self, base_url: Option<&str>, model: &str) -> Option<Vec<String>>;
    /// `rag.embed(texts, { kind: 'document', target })`; `Err` is the thrown message.
    fn embed(&mut self, texts: &[&str], target: &Target) -> Result<Vec<Vec<f64>>, String>;
    /// `embedding-address.cjs` `checkEmbeddingAddress(url)`: `Err(reason)` when refused.
    fn check_address(&mut self, url: &str) -> Result<(), String>;
    /// `fetchJson(url, { method, headers: diaryServiceHeaders(), body? }, 15000)` to the Diary;
    /// `Err` is the thrown message.
    fn diary(&mut self, url: &str, method: &str, body: Option<&str>) -> Result<HttpReply, String>;
    /// `authService.audit(action, user, null, detail)`.
    fn audit(&mut self, action: &str, user: &str, detail: &JValue);
    /// `embedding-service.cjs` (#1668), as `service.refresh()`, `managed()`, `enabled()` (true,
    /// false or unknown), `status()`, `baseUrl`, `isAt(address)`, `fileOf(model)`.
    fn service_refresh(&mut self);
    fn service_managed(&mut self) -> bool;
    fn service_enabled(&mut self) -> Option<bool>;
    fn service_status(&mut self) -> JValue;
    fn service_base_url(&self) -> Option<String>;
    fn service_is_at(&mut self, address: &JValue) -> bool;
    fn service_file_of(&mut self, model: &JValue) -> Option<String>;
    /// `service.serve(models)`, each `{ model, file }`.
    fn service_serve(&mut self, models: &[JValue]) -> Result<(), ServeError>;
    /// `service.waitLoaded(models, { timeoutMs })`.
    fn service_wait_loaded(&mut self, models: &[String], timeout_ms: u64) -> bool;
    /// `resolveModelFile({ model })`; `None` also when it threw.
    fn resolve_file(&mut self, model: &str) -> Option<String>;
}
