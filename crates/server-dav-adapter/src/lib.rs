//! Source-only inbound Diary DAV adapter preparation.
//!
//! This crate has no listener, binary, router registration or production Backend
//! implementation. Mutations are unavailable before any transport send. The
//! account revocation fence, last-used writer, guarded Diary operations and
//! read-only method capability gate remain release prerequisites.
#![forbid(unsafe_code)]

pub mod auth;
pub mod diary_io;
pub mod storage_ops;

/// A DAV principal created only after the app-password verification path.
/// The URL username and caller headers never construct this value.
#[derive(Clone)]
pub struct BoundIdentity {
    user_id: String,
    credential_id: String,
    scope: server_dav::Scope,
}

impl BoundIdentity {
    pub(crate) fn new(user_id: String, credential_id: String, scope: server_dav::Scope) -> Self {
        Self {
            user_id,
            credential_id,
            scope,
        }
    }

    pub fn user_id(&self) -> &str {
        &self.user_id
    }
    pub fn credential_id(&self) -> &str {
        &self.credential_id
    }
    pub fn scope(&self) -> server_dav::Scope {
        self.scope
    }
}
