//! Bind an existing gate result once. Identity's public fields are forgeable: this is not an auth certificate.
//! No key-file IO, credential writes, lazy upgrades or alternate-owner/AAD API.
use crate::connection_row::{self, ProjectionError, OWNER_BYTES};
use crate::protocol::{Connection, Kind};
use server_auth::{secrets::SecretKeys, Identity, Role};
use server_store::Store;
use zeroize::Zeroizing;

pub const DECRYPTED_BYTES: usize = 65_536;

struct OwnerBoundSecretKeys<'a> {
    owner: String,
    role: Role,
    keys: &'a SecretKeys,
}
impl<'a> OwnerBoundSecretKeys<'a> {
    fn bind(identity: &Identity, keys: &'a SecretKeys) -> Result<Self, ProjectionError> {
        let owner = identity.user.id.as_str();
        if owner.is_empty() || owner.len() > OWNER_BYTES {
            return Err(ProjectionError::Owner);
        }
        let mut bound = String::new();
        bound
            .try_reserve_exact(owner.len())
            .map_err(|_| ProjectionError::Allocation)?;
        bound.push_str(owner);
        Ok(Self {
            owner: bound,
            role: identity.user.role,
            keys,
        })
    }
    fn open(&self, value: &str) -> Result<(Zeroizing<String>, bool), ProjectionError> {
        match self.keys.open(value, Some(&self.owner)) {
            Ok((plain, _used)) => {
                if plain.len() > DECRYPTED_BYTES {
                    return Err(ProjectionError::Cap);
                }
                Ok((plain, false))
            }
            Err(_) => Ok((Zeroizing::new(String::new()), !value.is_empty())),
        }
    }
}

// No Debug, Clone or serialization of credential-bearing configuration.
pub struct ProjectedConnection {
    connection: Connection,
    effective_role: Role,
    secret_configured: bool,
    secret_needs_reauth: bool,
}
impl ProjectedConnection {
    pub fn connection(&self) -> &Connection {
        &self.connection
    }
    pub fn effective_role(&self) -> Role {
        self.effective_role
    }
    pub fn secret_configured(&self) -> bool {
        self.secret_configured
    }
    pub fn secret_needs_reauth(&self) -> bool {
        self.secret_needs_reauth
    }
}

/// `identity` MUST come from the actual request gate. No caller-selectable row owner or AAD.
/// Snapshot transaction ends before opening the credential; the returned connection owns its data.
pub fn project_connection(
    store: &Store,
    identity: &Identity,
    keys: &SecretKeys,
) -> Result<ProjectedConnection, ProjectionError> {
    let bound = OwnerBoundSecretKeys::bind(identity, keys)?;
    let row = connection_row::snapshot(store, &bound.owner)?;
    let connection;
    let needs_reauth;
    if let Some(row) = row {
        let (plain, needs) = bound.open(&row.secret)?;
        needs_reauth = needs;
        let mut secret = Zeroizing::new(Vec::new());
        secret
            .try_reserve_exact(plain.len())
            .map_err(|_| ProjectionError::Allocation)?;
        secret.extend_from_slice(plain.as_bytes());
        let kind = match row.kind.as_str() {
            "webdav" => Kind::WebDav,
            "nextcloud" => Kind::Nextcloud,
            "s3" => Kind::S3,
            _ => Kind::Local,
        };
        connection = Connection {
            kind,
            base_url: row.base_url,
            username: row.username,
            secret,
            corpus_root: row.corpus_root,
            bucket: row.bucket,
            region: s3_sign::normalize_region(&row.region),
        };
    } else {
        needs_reauth = false;
        connection = Connection {
            kind: Kind::Local,
            base_url: String::new(),
            username: String::new(),
            secret: Zeroizing::new(Vec::new()),
            corpus_root: String::new(),
            bucket: String::new(),
            region: "us-east-1".into(),
        };
    }
    Ok(ProjectedConnection {
        secret_configured: !connection.secret.is_empty(),
        connection,
        effective_role: bound.role,
        secret_needs_reauth: needs_reauth,
    })
}
