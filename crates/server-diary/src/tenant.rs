//! core `diary.cjs` `diaryHeaders()`: the headers one Diary request carries for one tenant.
//!
//! Storage header policy (core #292): a blocked or local descriptor never carries a secret.
//! Without `DIARY_TENANT_KEY` the remote credential rides on every call. With it, a remote
//! descriptor carries only `secretRef` (a name for the credential, not the credential), and the
//! secret itself is attached only on the retry after the sidecar answered 428.

use crate::config::DiaryConfig;
use base64::Engine as _;
use js_json::JValue;
use rand_core::{OsRng, RngCore};
use server_auth::{Credential, Identity, PublicUser, Role};
use server_storage_reads::owner_secret::{self, ProjectedConnection};
use server_storage_reads::protocol::{self, Kind};
use server_store::Store;
use sha2::{Digest, Sha256};
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};
use url::Url;
use zeroize::Zeroizing;

/// Header names, as core sends them.
pub const USER_ID: &str = "X-Cowork-User-ID";
pub const LEGACY_OWNER: &str = "X-Cowork-Legacy-Owner";
pub const STORAGE: &str = "X-Cowork-Storage";
pub const STORAGE_BLOCKED: &str = "X-Cowork-Storage-Blocked";
pub const ASSERTION: &str = "X-Cowork-Tenant-Assertion";

/// The tenant's headers could not be made. Every route turns this into the answer core's thrown
/// error gave; none of them carries a detail.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TenantError {
    /// The storage row could not be read or opened, or its secret could not be named.
    Storage,
    /// The system could not give a nonce or a clock.
    Entropy,
}

/// What `diaryHeaders()` reads: the person, their saved storage connection (the secret already
/// opened) and whether their workspace holds `migration.json`.
pub struct Tenant {
    user_id: String,
    admin: bool,
    legacy_owner: bool,
    storage: ProjectedConnection,
}

/// `workspaceStore.get`: `users/<id>` under the data directory, for ids core's `userDir` accepts
/// (`/^[0-9a-f-]{36}$/i`). Anything else has no workspace here and so no marker.
pub(crate) fn workspace_dir(data_dir: &Path, user_id: &str) -> Option<std::path::PathBuf> {
    let ok = user_id.len() == 36
        && user_id
            .bytes()
            .all(|b| b.is_ascii_digit() || matches!(b, b'a'..=b'f' | b'A'..=b'F' | b'-'));
    ok.then(|| data_dir.join("users").join(user_id))
}

impl Tenant {
    /// Blocking (SQLite and files). `identity` MUST come from the request gate.
    pub fn load(
        identity: &Identity,
        store: &Store,
        data_dir: &Path,
        config: &DiaryConfig,
    ) -> Result<Self, TenantError> {
        // A missing or unreadable secrets.key only matters to an owner who has a stored credential
        // (project_connection_with refuses that owner), as in storage_browse_routes.
        let keys = owner_secret::load_existing_keys(data_dir, config.previous_key_env()).ok();
        let storage = owner_secret::project_connection_with(store, identity, keys.as_ref())
            .map_err(|_| TenantError::Storage)?;
        let legacy_owner = workspace_dir(data_dir, &identity.user.id)
            .is_some_and(|dir| dir.join("migration.json").exists());
        Ok(Self {
            user_id: identity.user.id.clone(),
            admin: identity.user.role == Role::Admin,
            legacy_owner,
            storage,
        })
    }

    pub fn user_id(&self) -> &str {
        &self.user_id
    }

    /// [`Tenant::load`] for an account that is not signed in: a Diary connector speaks for its
    /// owner with no session (core `callDiaryFile` runs as `{ user, legacy: false }`). Only the id
    /// and the role are read from an identity, so only they are given; `role` is the account's
    /// own, which the member-origin policy looks at.
    pub fn load_user(
        user_id: &str,
        role: Role,
        store: &Store,
        data_dir: &Path,
        config: &DiaryConfig,
    ) -> Result<Self, TenantError> {
        let identity = Identity {
            user: PublicUser {
                id: user_id.to_owned(),
                username: String::new(),
                display_name: String::new(),
                role,
                disabled: false,
                diary_enabled: true,
                onboarded: true,
            },
            account_role: role,
            credential: Credential::Legacy,
        };
        Self::load(&identity, store, data_dir, config)
    }

    /// The saved connection as the storage-login latch names it: kind, address, bucket, user and
    /// the secret (core `fingerprintOf`). The secret is lent, never copied out.
    pub fn storage_identity(&self) -> StorageIdentity<'_> {
        let c = self.storage.connection();
        StorageIdentity {
            kind: kind_name(c.kind),
            base_url: &c.base_url,
            bucket: &c.bucket,
            username: &c.username,
            secret: Zeroizing::new(String::from_utf8_lossy(&c.secret).into_owned()),
        }
    }
}

/// What core's `fingerprintOf(connection)` hashes. No `Debug`: it holds the secret.
pub struct StorageIdentity<'a> {
    pub kind: &'static str,
    pub base_url: &'a str,
    pub bucket: &'a str,
    pub username: &'a str,
    pub secret: Zeroizing<String>,
}

/// How a request's body is bound into the assertion (diary-tenant-assertion.cjs `bodyIsHashed`):
/// a JSON (or empty) body is hashed; a ZIP upload signs `stream`.
#[derive(Clone, Copy)]
pub enum Content<'a> {
    Json(Option<&'a [u8]>),
    Zip,
}

impl Content<'_> {
    fn content_type(self) -> &'static str {
        match self {
            Content::Json(_) => "application/json",
            Content::Zip => "application/zip",
        }
    }
    fn body_hash(self) -> String {
        match self {
            Content::Json(body) => hex(&Sha256::digest(body.unwrap_or_default())),
            Content::Zip => tenant_assertion::STREAM.to_owned(),
        }
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn kind_name(kind: Kind) -> &'static str {
    match kind {
        Kind::WebDav => "webdav",
        Kind::Nextcloud => "nextcloud",
        Kind::S3 => "s3",
        Kind::Local => "local",
    }
}

fn s(v: &str) -> JValue {
    JValue::Str(v.to_owned())
}

impl Tenant {
    /// `storageDescriptor(userId, storage, includeSecret)` as JSON text, key order as core's.
    fn descriptor(
        &self,
        config: &DiaryConfig,
        include_secret: bool,
    ) -> Result<Zeroizing<String>, TenantError> {
        let c = self.storage.connection();
        let mut fields = vec![
            ("kind".to_owned(), s(kind_name(c.kind))),
            ("baseUrl".to_owned(), s(&c.base_url)),
            ("bucket".to_owned(), s(&c.bucket)),
            ("region".to_owned(), s(&c.region)),
            ("username".to_owned(), s(&c.username)),
            ("corpusRoot".to_owned(), s(&c.corpus_root)),
        ];
        let flags = |fields: &mut Vec<(String, JValue)>| {
            // getStorage answers an absent row without the two credential flags.
            if self.storage.row_present() {
                fields.push((
                    "secretConfigured".into(),
                    JValue::Bool(self.storage.secret_configured()),
                ));
                fields.push((
                    "secretNeedsReauth".into(),
                    JValue::Bool(self.storage.secret_needs_reauth()),
                ));
            }
        };
        let secret = Zeroizing::new(String::from_utf8_lossy(&c.secret).into_owned());
        let key = config.tenant_key();
        if c.kind == Kind::Local {
            // `rest`: the connection without its secret.
            flags(&mut fields);
        } else if key.is_empty() {
            // Before M2 the remote credential rides on every call: the whole connection.
            fields.push(("secret".into(), JValue::Str(secret.to_string())));
            flags(&mut fields);
        } else {
            flags(&mut fields);
            let reference = if secret.is_empty() {
                String::new()
            } else {
                tenant_assertion::storage_secret_ref(key, &self.user_id, &secret)
                    .ok_or(TenantError::Storage)?
            };
            fields.push(("secretRef".into(), JValue::Str(reference)));
            if include_secret && !secret.is_empty() {
                fields.push(("secret".into(), JValue::Str(secret.to_string())));
            }
        }
        js_json::stringify(&JValue::Obj(fields))
            .map(Zeroizing::new)
            .ok_or(TenantError::Storage)
    }

    /// The headers of one request to `url`. `secret`: attach the remote storage secret (the 428
    /// retry; without a tenant key it is attached always, so the caller passes true then).
    pub(crate) fn headers(
        &self,
        config: &DiaryConfig,
        method: &str,
        url: &Url,
        secret: bool,
        content: Content<'_>,
    ) -> Result<Vec<(&'static str, String)>, TenantError> {
        let mut h: Vec<(&'static str, String)> =
            vec![("Content-Type", content.content_type().to_owned())];
        if !config.token().is_empty() {
            h.push(("Authorization", format!("Bearer {}", config.token())));
        }
        h.push((USER_ID, self.user_id.clone()));
        if self.legacy_owner {
            h.push((LEGACY_OWNER, "1".to_owned()));
        }
        let blocked = self.storage.connection().kind != Kind::Local
            && !protocol::endpoint_approved(
                &self.storage.connection().base_url,
                self.admin,
                &config.member_origins(),
            );
        // The sidecar may serve an already-active app diary, but must never resolve a legacy
        // remote or send credentials to an endpoint the policy refused.
        let descriptor = if blocked {
            h.push((STORAGE_BLOCKED, "1".to_owned()));
            Zeroizing::new(r#"{"kind":"blocked"}"#.to_owned())
        } else {
            self.descriptor(config, secret)?
        };
        let storage =
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(descriptor.as_bytes());
        h.push((STORAGE, storage.clone()));
        if !config.tenant_key().is_empty() && !self.user_id.is_empty() {
            h.push((
                ASSERTION,
                self.assertion(config, method, url, &storage, blocked, content)?,
            ));
        }
        Ok(h)
    }

    /// `signTenantAssertion`: `v2.<ts>.<nonce>.<sig>` bound to the tenant, the method, the encoded
    /// path, the query, the body (or `stream`) and the exact storage/legacy/blocked headers.
    fn assertion(
        &self,
        config: &DiaryConfig,
        method: &str,
        url: &Url,
        storage: &str,
        blocked: bool,
        content: Content<'_>,
    ) -> Result<String, TenantError> {
        let ts = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|_| TenantError::Entropy)?
            .as_secs();
        let mut nonce_bytes = [0u8; 16];
        OsRng
            .try_fill_bytes(&mut nonce_bytes)
            .map_err(|_| TenantError::Entropy)?;
        let nonce = hex(&nonce_bytes);
        let body_hash = content.body_hash();
        let request = tenant_assertion::Request {
            key: config.tenant_key(),
            user_id: &self.user_id,
            assertion: "",
            method,
            path: url.path(),
            query: url.query().unwrap_or("").as_bytes(),
            body_hash: &body_hash,
            storage,
            legacy_owner: if self.legacy_owner { "1" } else { "" },
            blocked: if blocked { "1" } else { "" },
            now: 0.0,
        };
        tenant_assertion::sign(&request, ts, &nonce).ok_or(TenantError::Entropy)
    }
}
