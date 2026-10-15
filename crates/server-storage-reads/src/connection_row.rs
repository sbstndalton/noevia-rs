//! Private owner-filtered storage snapshot. This module accepts only the read-only Store.
use server_store::{rusqlite::types::ValueRef, Reader, Store};
use zeroize::Zeroizing;

pub const OWNER_BYTES: usize = 65_536;
pub const KIND_BYTES: usize = 32;
pub const FIELD_BYTES: usize = 65_536;
pub const SECRET_BYTES: usize = 131_072;
pub const ROW_BYTES: usize = 262_144;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProjectionError {
    Owner,
    Unavailable,
    Schema,
    Row,
    Cap,
    Allocation,
}
impl std::fmt::Display for ProjectionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Owner => "storage owner unavailable",
            Self::Unavailable => "storage unavailable",
            Self::Schema => "storage schema unavailable",
            Self::Row => "storage row unavailable",
            Self::Cap => "storage connection exceeds limits",
            Self::Allocation => "storage connection unavailable",
        })
    }
}
impl std::error::Error for ProjectionError {}

// No Debug, Clone or serialization: all connection configuration is private.
pub(crate) struct Snapshot {
    pub kind: String,
    pub base_url: String,
    pub bucket: String,
    pub region: String,
    pub username: String,
    pub secret: Zeroizing<String>,
    pub corpus_root: String,
}
fn schema(r: &Reader<'_>) -> Result<bool, server_store::StoreError> {
    let table: i64 = r
        .row(
            "SELECT count(*) FROM sqlite_master WHERE name='storage_connections' AND type='table'",
            [],
            |row| row.get(0),
        )?
        .unwrap_or(0);
    if table != 1 {
        return Ok(false);
    }
    let columns: i64 = r.row(
        "SELECT count(*) FROM pragma_table_info('storage_connections') WHERE name IN ('user_id','kind','base_url','bucket','region','username','secret','corpus_root','updated_at')",
        [], |row| row.get(0),
    )?.unwrap_or(0);
    let pk: i64 = r
        .row(
            "SELECT count(*) FROM pragma_table_info('storage_connections') WHERE pk>0",
            [],
            |row| row.get(0),
        )?
        .unwrap_or(0);
    let owner_pk: i64 = r.row(
        "SELECT count(*) FROM pragma_table_info('storage_connections') WHERE name='user_id' AND pk=1",
        [], |row| row.get(0),
    )?.unwrap_or(0);
    Ok(columns == 9 && pk == 1 && owner_pk == 1)
}
fn text<'a>(value: ValueRef<'a>, cap: usize) -> Result<&'a str, ProjectionError> {
    let ValueRef::Text(bytes) = value else {
        return Err(ProjectionError::Row);
    };
    if bytes.len() > cap {
        return Err(ProjectionError::Cap);
    }
    std::str::from_utf8(bytes).map_err(|_| ProjectionError::Row)
}
fn copy(value: &str) -> Result<String, ProjectionError> {
    let mut out = String::new();
    out.try_reserve_exact(value.len())
        .map_err(|_| ProjectionError::Allocation)?;
    out.push_str(value);
    Ok(out)
}
fn map(row: &server_store::rusqlite::Row<'_>, owner: &str) -> Result<Snapshot, ProjectionError> {
    let get = |name| row.get_ref(name).map_err(|_| ProjectionError::Row);
    let user_id = text(get("user_id")?, OWNER_BYTES)?;
    if user_id != owner {
        return Err(ProjectionError::Row);
    }
    let kind = text(get("kind")?, KIND_BYTES)?;
    let base_url = text(get("base_url")?, FIELD_BYTES)?;
    let bucket = text(get("bucket")?, FIELD_BYTES)?;
    let region = text(get("region")?, FIELD_BYTES)?;
    let username = text(get("username")?, FIELD_BYTES)?;
    let secret = text(get("secret")?, SECRET_BYTES)?;
    let corpus_root = text(get("corpus_root")?, FIELD_BYTES)?;
    if !matches!(get("updated_at")?, ValueRef::Integer(_)) {
        return Err(ProjectionError::Row);
    }
    let mut total = 0usize;
    for s in [
        user_id,
        kind,
        base_url,
        bucket,
        region,
        username,
        secret,
        corpus_root,
    ] {
        total = total
            .checked_add(s.len())
            .filter(|&n| n <= ROW_BYTES)
            .ok_or(ProjectionError::Cap)?;
    }
    // Every field and aggregate is checked before any retained field copy.
    Ok(Snapshot {
        kind: copy(kind)?,
        base_url: copy(base_url)?,
        bucket: copy(bucket)?,
        region: copy(region)?,
        username: copy(username)?,
        secret: Zeroizing::new(copy(secret)?),
        corpus_root: copy(corpus_root)?,
    })
}
pub(crate) fn snapshot(store: &Store, owner: &str) -> Result<Option<Snapshot>, ProjectionError> {
    store.read(|r| {
        if !schema(r)? { return Ok(Err(ProjectionError::Schema)); }
        let mut rows = r.rows(
            "SELECT user_id,kind,base_url,bucket,region,username,secret,corpus_root,updated_at FROM storage_connections WHERE user_id=?1 LIMIT 2",
            [owner], |row| Ok(map(row, owner)),
        )?;
        if rows.len() > 1 { return Ok(Err(ProjectionError::Row)); }
        Ok(rows.pop().transpose())
    }).map_err(|_| ProjectionError::Unavailable)?
}
