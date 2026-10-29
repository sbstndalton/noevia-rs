//! Explicit new installation of the original cowork.db schema. Existing data is
//! never migrated or overwritten by this initializer. Production selection is separate.
use crate::{database, Fault, Result};
use sha2::{Digest, Sha256};
use std::{
    fs::{File, OpenOptions},
    io::Write,
    path::Path,
};
pub fn create(data: &Path, setup_code: &str) -> Result<()> {
    if !data.is_absolute()
        || setup_code.len() < 22
        || setup_code.len() > 256
        || !setup_code
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_-".contains(&b))
    {
        return Err(Fault::Invalid);
    }
    let parent = data
        .parent()
        .ok_or(Fault::Invalid)?
        .canonicalize()
        .map_err(|_| Fault::Unavailable)?;
    let data = parent.join(data.file_name().ok_or(Fault::Invalid)?);
    let mut builder = std::fs::DirBuilder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(&data).map_err(|_| Fault::Conflict)?;
    // The new directory and database are exclusively ours. Failed initialization
    // leaves them inspectable and will not be mistaken for a fresh retry target.
    let mut db = database::open(&server_store::db_path(&data), database::Open::New)?;
    db.pragma_update(None, "journal_mode", "WAL")?;
    db.pragma_update(None, "foreign_keys", true)?;
    let tx = db.transaction()?;
    tx.execute_batch(include_str!("original_schema.sql"))?;
    tx.execute(
        "INSERT INTO settings(key,value) VALUES('setup_code_hash',?)",
        [format!("{:x}", Sha256::digest(setup_code.as_bytes()))],
    )?;
    tx.commit()?;
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options
        .open(data.join("first-run-setup-code"))
        .map_err(|_| Fault::Database)?;
    file.write_all(format!("{setup_code}\n").as_bytes())
        .map_err(|_| Fault::Database)?;
    file.sync_all().map_err(|_| Fault::Database)?;
    db.execute_batch("PRAGMA wal_checkpoint(TRUNCATE)")?;
    drop(db);
    // Existing native reader and writer schema/topology guards judge our result.
    server_store::Store::open(&data).map_err(|_| Fault::Unavailable)?;
    server_store::Writer::open(&data, server_store::RustAuth::owned())
        .map_err(|_| Fault::Unavailable)?;
    File::open(&data)
        .and_then(|f| f.sync_all())
        .map_err(|_| Fault::Database)?;
    File::open(parent)
        .and_then(|f| f.sync_all())
        .map_err(|_| Fault::Database)?;
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn native_initialization_passes_original_guards_and_never_reinitializes() -> Result<()> {
        let parent = tempfile::tempdir().map_err(|_| Fault::Database)?;
        let data = parent.path().join("fresh");
        let code = "synthetic-setup-code-20261028";
        create(&data, code)?;
        let db = rusqlite::Connection::open(server_store::db_path(&data))?;
        assert_eq!(
            db.query_row("SELECT max(version) FROM schema_migrations", [], |r| r
                .get::<_, i64>(0))?,
            5
        );
        assert_eq!(
            db.query_row("SELECT count(*) FROM users", [], |r| r.get::<_, i64>(0))?,
            0
        );
        assert_eq!(
            db.query_row(
                "SELECT value FROM settings WHERE key='setup_code_hash'",
                [],
                |r| r.get::<_, String>(0)
            )?,
            format!("{:x}", Sha256::digest(code.as_bytes()))
        );
        assert_eq!(
            create(&data, "other-synthetic-setup-code"),
            Err(Fault::Conflict)
        );
        assert_eq!(
            std::fs::read_to_string(data.join("first-run-setup-code"))
                .map_err(|_| Fault::Database)?,
            format!("{code}\n")
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&data)
                    .map_err(|_| Fault::Database)?
                    .permissions()
                    .mode()
                    & 0o777,
                0o700
            );
            assert_eq!(
                std::fs::metadata(data.join("first-run-setup-code"))
                    .map_err(|_| Fault::Database)?
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }
        Ok(())
    }
}
