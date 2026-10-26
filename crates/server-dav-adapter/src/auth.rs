//! Read-only DAV credential and eligibility gate.
//!
//! This is a preparation for an inbound adapter, not a production authorization
//! fence. It does not update `last_used_at`; Node still owns that DAV write. A
//! future mutation path must recheck authorization in the same transaction as
//! its guarded operation. No caller-supplied tenant ID or headers enter this API.

use std::sync::Arc;

use server_dav::Scope;
use server_store::rusqlite::params;
use server_store::Store;

use crate::BoundIdentity;

/// Configuration checked before any credential or Diary operation is possible.
/// The sidecar and current Node client accept a trimmed UTF-8 tenant key, without
/// a required encoding. This source-only gate deliberately refuses blank keys,
/// controls, and keys over 4096 bytes rather than permitting unsigned requests.
pub struct AuthConfig {
    scope: Scope,
    tenant_key: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConfigError {
    InvalidScope,
    InvalidTenantKey,
}

impl AuthConfig {
    pub fn new(scope: &str, tenant_key: &str) -> Result<Self, ConfigError> {
        let scope = match scope {
            "lan" => Scope::Lan,
            "public" => Scope::Public,
            _ => return Err(ConfigError::InvalidScope),
        };
        let key = tenant_key.trim();
        if key.is_empty() || key.len() > 4096 || key.chars().any(char::is_control) {
            return Err(ConfigError::InvalidTenantKey);
        }
        Ok(Self {
            scope,
            tenant_key: key.to_owned(),
        })
    }

    pub fn scope(&self) -> Scope {
        self.scope
    }

    /// For a separately reviewed, identity-bound tenant assertion sender.
    /// Never place this value in errors, logs, or a client response.
    pub fn tenant_key(&self) -> &str {
        &self.tenant_key
    }
}

/// Database and credential failures are intentionally indistinguishable to a
/// future HTTP boundary; this type never holds the credential or tenant key.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AuthError {
    Unavailable,
}

/// Read-only account authority for the configured DAV endpoint scope.
pub struct AuthGate {
    store: Arc<Store>,
    config: AuthConfig,
}

impl AuthGate {
    pub fn new(store: Arc<Store>, config: AuthConfig) -> Self {
        Self { store, config }
    }

    pub fn scope(&self) -> Scope {
        self.config.scope()
    }

    /// Verify an app password using the existing Rust port of Node verifyDav.
    /// The only tenant in the result is the user ID found with that credential.
    /// The caller must also check `allowed` before any read; this method does
    /// not claim current sharing eligibility or update `last_used_at`.
    pub fn authenticate(
        &self,
        username: &str,
        password: &str,
    ) -> Result<Option<BoundIdentity>, AuthError> {
        let identity = server_auth::app_passwords::verify_dav(
            &self.store,
            username,
            password,
            self.config.scope.as_str(),
        )
        .map_err(|_| AuthError::Unavailable)?;
        Ok(identity.and_then(|identity| {
            (identity.scope == self.config.scope.as_str()).then(|| {
                BoundIdentity::new(identity.user_id, identity.credential_id, self.config.scope)
            })
        }))
    }

    /// One read transaction sees credential ownership, account status, sharing,
    /// Diary enablement, and local storage together. A missing storage row means
    /// the same server-held local default as Node `auth.getStorage(userId)`.
    /// This is an eligibility snapshot; it cannot fence a later operation.
    pub fn allowed(&self, identity: &BoundIdentity) -> Result<bool, AuthError> {
        if identity.scope() != self.config.scope {
            return Ok(false);
        }
        self.store
            .read(|reader| {
                reader.row(
                    "SELECT 1 FROM app_passwords AS a \
                     JOIN users AS u ON u.id = a.user_id \
                     JOIN dav_settings AS d ON d.user_id = u.id \
                     JOIN user_features AS f ON f.user_id = u.id \
                     LEFT JOIN storage_connections AS s ON s.user_id = u.id \
                     WHERE a.id = ?1 AND a.user_id = ?2 AND a.scope = ?3 \
                       AND u.disabled_at IS NULL AND d.scope = ?3 \
                       AND f.diary_enabled = 1 \
                       AND (s.kind IS NULL OR s.kind = 'local') \
                     LIMIT 1",
                    params![
                        identity.credential_id(),
                        identity.user_id(),
                        self.config.scope.as_str()
                    ],
                    |row| row.get::<_, i64>(0),
                )
            })
            .map(|row| row.is_some())
            .map_err(|_| AuthError::Unavailable)
    }
}
