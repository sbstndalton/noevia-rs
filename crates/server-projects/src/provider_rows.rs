//! The provider rows (sbstndalton/noevia#2190 round 6): `users/<id>/providers.json` (an account's
//! private rows) and `shared-providers.json` (the administrators' shared ones), both
//! `{ "providers": [...] }` as core workspace.cjs `atomicJson` wrote them. The Rust front is the
//! only writer of both: its provider routes, the ChatGPT broker's row, and Node's one remaining
//! save (the seal-on-load migration of a plaintext key) through the project store
//! ([`crate::store_api::StoreFile::Providers`]). Every change is one read-modify-write under the
//! file's lock, so a request never writes from a stale copy and two writers cannot lose each
//! other's rows.
//!
//! The rows are handled as parsed JSON (key order and every field kept), and a key's ciphertext is
//! never opened here: a row nobody edits is written back byte for byte as it was read, which also
//! keeps an unreadable key's original ciphertext (core #782).

use crate::lock;
use crate::store::{self, StoreError};
use js_json::JValue;
use std::path::Path;

pub const FILE: &str = "providers.json";

/// The rows of a parsed file: `readJson(file, { providers: [] }).providers || []`.
fn rows_of(doc: Option<JValue>) -> Vec<JValue> {
    match doc {
        Some(d) => match d.get("providers") {
            JValue::Arr(rows) => rows.clone(),
            _ => Vec::new(),
        },
        None => Vec::new(),
    }
}

/// The bytes `atomicJson(file, { providers })` writes.
pub fn serialise(rows: Vec<JValue>) -> String {
    js_json::wtf16::stringify_pretty(&JValue::obj([("providers", JValue::Arr(rows))]))
        .unwrap_or_default()
}

/// The rows as a reader sees them now: a missing or unparseable file is no rows (as Node's
/// `readJson` fallback); a value this port cannot hold is [`StoreError::Corrupt`].
pub fn read_file(file: &Path) -> Result<Vec<JValue>, StoreError> {
    Ok(rows_of(store::read_json(file)?))
}

/// The account's private rows.
pub fn read_user(dir: &Path) -> Result<Vec<JValue>, StoreError> {
    read_file(&dir.join(FILE))
}

/// The shared rows.
pub fn read_shared(data_dir: &Path) -> Result<Vec<JValue>, StoreError> {
    read_file(&data_dir.join(server_store::ROOT_PROVIDER_FILE))
}

/// A write refuses a file that exists but cannot be read as rows: its content is never replaced
/// from here (it may hold keys a repair can still recover).
fn rows_for_write(file: &Path) -> Result<Vec<JValue>, StoreError> {
    match std::fs::metadata(file) {
        Ok(_) => match store::read_json(file)? {
            Some(doc) => match doc.get("providers") {
                JValue::Arr(rows) => Ok(rows.clone()),
                _ => Err(StoreError::Corrupt),
            },
            None => Err(StoreError::Corrupt),
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(e) => Err(StoreError::Io(e)),
    }
}

/// One read-modify-write of `dir/providers.json` under its lock: `change` gets the rows as they
/// are on disk now and returns its result and whether it changed them; only then is the file
/// written. The user's directory must exist ([`StoreError::Gone`]: the account was removed).
pub fn update_user<T>(
    dir: &Path,
    switch: server_store::RustProjects,
    change: impl FnOnce(&mut Vec<JValue>) -> (T, bool),
) -> Result<T, StoreError> {
    store::existing(dir)?;
    let file = dir.join(FILE);
    let _held = lock::acquire(&file).map_err(|e| store::gone_if_missing(StoreError::from(e)))?;
    let mut rows = rows_for_write(&file)?;
    let (out, changed) = change(&mut rows);
    if changed {
        server_store::json::write_shared_projects(dir, FILE, &serialise(rows), switch)
            .map_err(|e| store::gone_if_missing(StoreError::Write(e)))?;
    }
    Ok(out)
}

/// [`update_user`] for `shared-providers.json` in the data directory.
pub fn update_shared<T>(
    data_dir: &Path,
    switch: server_store::RustProjects,
    change: impl FnOnce(&mut Vec<JValue>) -> (T, bool),
) -> Result<T, StoreError> {
    let file = data_dir.join(server_store::ROOT_PROVIDER_FILE);
    let _held = lock::acquire(&file).map_err(StoreError::from)?;
    let mut rows = rows_for_write(&file)?;
    let (out, changed) = change(&mut rows);
    if changed {
        server_store::json::write_root_shared_providers(data_dir, &serialise(rows), switch)
            .map_err(StoreError::Write)?;
    }
    Ok(out)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    const USER: &str = "11111111-2222-4333-8444-555555555555";

    fn on() -> server_store::RustProjects {
        server_store::RustProjects::owned()
    }

    fn ws() -> tempfile::TempDir {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("users").join(USER)).unwrap();
        root
    }

    fn row(id: &str, key: &str) -> JValue {
        js_json::parse(&format!(
            r#"{{"id":"{id}","label":"L","baseUrl":"http://x.test","apiKey":"{key}","shared":false}}"#
        ))
        .unwrap()
    }

    #[test]
    fn a_change_is_written_in_nodes_bytes_and_untouched_rows_keep_their_ciphertext() {
        let root = ws();
        let dir = root.path().join("users").join(USER);
        update_user(&dir, on(), |rows| {
            rows.push(row("a", "enc:v1:AAAA"));
            ((), true)
        })
        .unwrap();
        let first = std::fs::read_to_string(dir.join(FILE)).unwrap();
        assert_eq!(
            first,
            "{\n  \"providers\": [\n    {\n      \"id\": \"a\",\n      \"label\": \"L\",\n      \"baseUrl\": \"http://x.test\",\n      \"apiKey\": \"enc:v1:AAAA\",\n      \"shared\": false\n    }\n  ]\n}"
        );
        update_user(&dir, on(), |rows| {
            rows.push(row("b", ""));
            ((), true)
        })
        .unwrap();
        let rows = read_user(&dir).unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].get("apiKey").as_str(), Some("enc:v1:AAAA"));
    }

    #[test]
    fn an_unchanged_pass_writes_nothing_and_a_removed_account_is_gone() {
        let root = ws();
        let dir = root.path().join("users").join(USER);
        update_user(&dir, on(), |_| ((), false)).unwrap();
        assert!(!dir.join(FILE).exists());
        let gone = root
            .path()
            .join("users/99999999-2222-4333-8444-555555555555");
        assert!(matches!(
            update_user(&gone, on(), |_| ((), true)),
            Err(StoreError::Gone)
        ));
        assert!(!gone.exists());
    }

    #[test]
    fn a_damaged_file_is_read_as_no_rows_but_never_overwritten() {
        let root = ws();
        let dir = root.path().join("users").join(USER);
        std::fs::write(dir.join(FILE), "{oops").unwrap();
        assert!(read_user(&dir).unwrap().is_empty());
        assert!(matches!(
            update_user(&dir, on(), |rows| {
                rows.push(row("a", ""));
                ((), true)
            }),
            Err(StoreError::Corrupt)
        ));
        assert_eq!(std::fs::read_to_string(dir.join(FILE)).unwrap(), "{oops");
        std::fs::write(dir.join(FILE), r#"{"providers":{}}"#).unwrap();
        assert!(read_user(&dir).unwrap().is_empty());
        assert!(matches!(
            update_user(&dir, on(), |_| ((), true)),
            Err(StoreError::Corrupt)
        ));
    }

    #[test]
    fn the_shared_file_is_changed_the_same_way() {
        let root = tempfile::tempdir().unwrap();
        update_shared(root.path(), on(), |rows| {
            rows.push(row("s", ""));
            ((), true)
        })
        .unwrap();
        assert_eq!(read_shared(root.path()).unwrap().len(), 1);
        assert!(root.path().join("shared-providers.json.lock").exists());
    }
}
