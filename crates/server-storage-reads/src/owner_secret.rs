//! Bind an existing gate result once. Identity's public fields are forgeable: this is not an auth certificate.
//! Existing-key reads are bounded; no credential writes, lazy upgrades or alternate-owner/AAD API.
use crate::connection_row::{self, ProjectionError, OWNER_BYTES};
use crate::protocol::{Connection, Kind};
use server_auth::{secrets::SecretKeys, Identity, Role};
use server_store::Store;
use zeroize::Zeroizing;

pub const DECRYPTED_BYTES: usize = 65_536;

struct OwnerBoundSecretKeys<'a> {
    owner: String,
    role: Role,
    keys: Option<&'a SecretKeys>,
}
impl<'a> OwnerBoundSecretKeys<'a> {
    fn bind(identity: &Identity, keys: Option<&'a SecretKeys>) -> Result<Self, ProjectionError> {
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
        // A stored credential needs the keys; without them only rows are refused, never owners
        // with no connection (who get the normal no-connection answer, as in Node).
        let keys = self.keys.ok_or(ProjectionError::Unavailable)?;
        match keys.open(value, Some(&self.owner)) {
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
    metadata_kind: String,
    row_present: bool,
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
    project_connection_with(store, identity, Some(keys))
}

/// [`project_connection`] when the credential keys may be missing or invalid (noevia#1827): an
/// owner with no connection row still gets the no-connection answer; an owner with a row is
/// refused (`Unavailable`) because its stored credential cannot be opened.
pub fn project_connection_with(
    store: &Store,
    identity: &Identity,
    keys: Option<&SecretKeys>,
) -> Result<ProjectedConnection, ProjectionError> {
    let bound = OwnerBoundSecretKeys::bind(identity, keys)?;
    let row = connection_row::snapshot(store, &bound.owner)?;
    let row_present = row.is_some();
    let metadata_kind = row
        .as_ref()
        .map_or_else(|| "local".to_owned(), |r| r.kind.clone());
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
        metadata_kind,
        row_present,
        effective_role: bound.role,
        secret_needs_reauth: needs_reauth,
    })
}

/// Node metadata property order; absent rows deliberately omit the two credential flags.
/// Credential plaintext is never serialized.
pub fn metadata_json(projected: &ProjectedConnection) -> String {
    let c = projected.connection();
    let mut out = String::from("{");
    for (index, (key, value)) in [
        ("kind", projected.metadata_kind.as_str()),
        ("baseUrl", c.base_url.as_str()),
        ("bucket", c.bucket.as_str()),
        ("region", c.region.as_str()),
        ("username", c.username.as_str()),
        ("corpusRoot", c.corpus_root.as_str()),
    ]
    .into_iter()
    .enumerate()
    {
        if index != 0 {
            out.push(',');
        }
        crate::json::quote_units(&mut out, &key.encode_utf16().collect::<Vec<_>>());
        out.push(':');
        crate::json::quote_units(&mut out, &value.encode_utf16().collect::<Vec<_>>());
    }
    if projected.row_present {
        out.push_str(if projected.secret_configured {
            ",\"secretConfigured\":true"
        } else {
            ",\"secretConfigured\":false"
        });
        out.push_str(if projected.secret_needs_reauth {
            ",\"secretNeedsReauth\":true"
        } else {
            ",\"secretNeedsReauth\":false"
        });
    }
    out.push('}');
    out
}

/// Logical cap on the optional previous-key environment text, before UTF-16/decode allocation.
/// Previous files take precedence. Over-cap environment inputs refuse rather than truncate.
pub const PREVIOUS_ENV_BYTES: usize = 4096;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyReadError {
    Unavailable,
    Invalid,
}

fn read_existing_key(path: &std::path::Path) -> Result<Zeroizing<Vec<u8>>, std::io::Error> {
    use std::io::Read;
    use std::os::unix::fs::MetadataExt;
    let fd = rustix::fs::open(
        path,
        rustix::fs::OFlags::RDONLY
            | rustix::fs::OFlags::NOFOLLOW
            | rustix::fs::OFlags::NONBLOCK
            | rustix::fs::OFlags::CLOEXEC,
        rustix::fs::Mode::empty(),
    )?;
    let mut file = std::fs::File::from(fd);
    let before = file.metadata()?;
    let invalid = || std::io::Error::from(std::io::ErrorKind::InvalidData);
    if !before.is_file() || before.nlink() != 1 || before.len() != 32 {
        return Err(invalid());
    }
    let mut bytes = Zeroizing::new(Vec::with_capacity(33));
    (&mut file).take(33).read_to_end(&mut bytes)?;
    let after = file.metadata()?;
    if bytes.len() != 32
        || before.dev() != after.dev()
        || before.ino() != after.ino()
        || after.nlink() != 1
        || after.len() != 32
        || before.mtime() != after.mtime()
        || before.mtime_nsec() != after.mtime_nsec()
        || before.ctime() != after.ctime()
        || before.ctime_nsec() != after.ctime_nsec()
    {
        return Err(invalid());
    }
    Ok(bytes)
}

/// Open each final component once, without following it. This does not attest ancestor paths
/// or provide an atomic filesystem snapshot. No permissions or file contents are changed.
pub fn load_existing_keys(
    data_dir: &std::path::Path,
    previous_env: Option<&str>,
) -> Result<SecretKeys, KeyReadError> {
    let current =
        read_existing_key(&data_dir.join("secrets.key")).map_err(|_| KeyReadError::Unavailable)?;
    let previous = match read_existing_key(&data_dir.join("secrets.key.previous")) {
        Ok(bytes) => Some(bytes),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            match previous_env.filter(|v| !v.is_empty()) {
                None => None,
                Some(text) => {
                    if text.len() > PREVIOUS_ENV_BYTES {
                        return Err(KeyReadError::Invalid);
                    }
                    let units = Zeroizing::new(text.encode_utf16().collect::<Vec<_>>());
                    let text =
                        Zeroizing::new(String::from_utf16_lossy(policy_leaves::js_trim(&units)));
                    let bytes = if text.len() == 64 && text.bytes().all(|b| b.is_ascii_hexdigit()) {
                        text.as_bytes()
                            .chunks(2)
                            .filter_map(|p| {
                                std::str::from_utf8(p)
                                    .ok()
                                    .and_then(|s| u8::from_str_radix(s, 16).ok())
                            })
                            .collect()
                    } else {
                        let units = Zeroizing::new(text.encode_utf16().collect::<Vec<_>>());
                        secret_envelope::decode_base64_node(&units)
                    };
                    Some(Zeroizing::new(bytes))
                }
            }
        }
        Err(_) => return Err(KeyReadError::Invalid),
    };
    SecretKeys::from_existing_bytes(current, previous).map_err(|_| KeyReadError::Invalid)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod metadata_key_tests {
    use super::*;
    #[test]
    fn keys_refuse_missing_short_long_symlink_and_multiple_links() {
        let d = tempfile::tempdir().unwrap();
        assert!(load_existing_keys(d.path(), None).is_err());
        let key = d.path().join("secrets.key");
        for len in [31, 33] {
            std::fs::write(&key, vec![7; len]).unwrap();
            assert!(load_existing_keys(d.path(), None).is_err());
        }
        std::fs::remove_file(&key).unwrap();
        let target = d.path().join("target");
        std::fs::write(&target, vec![7; 32]).unwrap();
        std::os::unix::fs::symlink(&target, &key).unwrap();
        assert!(load_existing_keys(d.path(), None).is_err());
        std::fs::remove_file(&key).unwrap();
        std::fs::hard_link(&target, &key).unwrap();
        assert!(load_existing_keys(d.path(), None).is_err());
    }
    #[test]
    fn previous_file_precedes_bounded_lenient_environment() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("secrets.key"), vec![7; 32]).unwrap();
        assert!(load_existing_keys(d.path(), Some(&"a".repeat(PREVIOUS_ENV_BYTES + 1))).is_err());
        assert!(load_existing_keys(d.path(), Some("not a key")).is_err());
        std::fs::write(d.path().join("secrets.key.previous"), vec![8; 32]).unwrap();
        assert!(
            load_existing_keys(d.path(), Some(&"a".repeat(PREVIOUS_ENV_BYTES + 1)))
                .is_ok_and(|k| k.has_previous())
        );
        std::fs::write(d.path().join("secrets.key.previous"), vec![7; 32]).unwrap();
        assert!(load_existing_keys(d.path(), Some("invalid")).is_ok_and(|k| !k.has_previous()));
    }
}
