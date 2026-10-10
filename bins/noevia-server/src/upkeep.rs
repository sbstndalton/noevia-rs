//! M3 (`NOEVIA_RUST_AUTH=1`): the session and device-grant writes Node's request gate made on
//! every request (server-auth `upkeep`), now Rust's because Rust owns those tables and Node
//! refuses to write them. Run before every request the front proxies to Node or answers on an
//! /api/ route of its own; the bundle and /api/ready, which Node no longer sees since M1, are
//! left alone as they are today.
//!
//! A failure never blocks the request: Node and the Rust gate still refuse what they would
//! (a session the upkeep failed to delete is refused again), only `last_seen_at` stops moving
//! until the store is back. It is reported, at most once a minute, without any request detail.

use server_auth::{Authenticator, Creds};
use server_store::{RustAuth, StoreError, Writer};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const WARN_EVERY: Duration = Duration::from_secs(60);

pub struct WriterLayer {
    data_dir: Option<PathBuf>,
    switch: Option<RustAuth>,
    /// Opened on first use and kept once it opens; a failed open is retried next time.
    writer: Mutex<Option<Arc<Writer>>>,
    warned: Mutex<Option<Instant>>,
}

impl WriterLayer {
    pub fn new(config: &crate::config::Config) -> Self {
        WriterLayer {
            data_dir: config.data_dir.clone(),
            switch: config.rust_auth,
            writer: Mutex::new(None),
            warned: Mutex::new(None),
        }
    }

    /// The switch, when it is on.
    pub fn switch(&self) -> Option<RustAuth> {
        self.switch
    }

    /// The writer (blocking: opens the database the first time). Refused without the switch.
    pub fn writer(&self) -> Result<Arc<Writer>, StoreError> {
        let switch = self
            .switch
            .ok_or_else(|| StoreError::NotOwned("cowork.db (NOEVIA_RUST_AUTH is off)".into()))?;
        let mut slot = self.writer.lock().map_err(|_| StoreError::Poisoned)?;
        if let Some(w) = slot.as_ref() {
            return Ok(Arc::clone(w));
        }
        let dir = self
            .data_dir
            .as_ref()
            .ok_or_else(|| StoreError::NotReady("UI_DATA_DIR is not set".into()))?;
        let w = Arc::new(Writer::open(dir, switch)?);
        *slot = Some(Arc::clone(&w));
        Ok(w)
    }

    /// Node's per-request writes for `creds` (blocking).
    pub fn upkeep_blocking(&self, auth: Option<&Authenticator>, creds: &Creds, now_ms: i64) {
        let Some(auth) = auth else { return };
        let result = self.writer().and_then(|w| auth.upkeep(&w, creds, now_ms));
        if let Err(e) = result {
            self.warn(&e);
        }
    }

    fn warn(&self, e: &StoreError) {
        let Ok(mut last) = self.warned.lock() else {
            return;
        };
        if last.is_some_and(|t| t.elapsed() < WARN_EVERY) {
            return;
        }
        *last = Some(Instant::now());
        eprintln!("noevia-server: session upkeep failed: {e}");
    }
}
