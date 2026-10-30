//! A file read from storage in bounded chunks, staged on disk (sbstndalton/noevia#2191 round 8).
//!
//! The streamed read ([`crate::source_storage::SourceStorage::read_binary`]) hands the front at most 8 MiB
//! at a time. They are written to a temp file under the user's directory (`source-staging/`, 0700, files
//! 0600) while their SHA-256 is taken, the file is fsynced and renamed to its content hash, and only then
//! read back for the extraction. A read that fails part-way (the child crashed, a range lied, the cap was
//! passed) leaves nothing: the temp file is removed on every path out, including a panic, and anything
//! left by a process that died is removed by [`sweep`] the next time a read starts.
use crate::source_storage::{BinaryRead, SourceStorage, StorageError};
use sha2::{Digest, Sha256};
use std::io::{Read, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

pub const DIR: &str = "source-staging";
/// How long a staged file may sit before [`sweep`] takes it (a read never takes this long).
const STALE: Duration = Duration::from_secs(2 * 60 * 60);

/// A staged file; removed when dropped.
pub struct Staged {
    path: PathBuf,
    /// SHA-256 of the bytes.
    pub sha256: String,
    pub len: u64,
}

impl Staged {
    pub fn path(&self) -> &Path {
        &self.path
    }
    /// The whole file in memory (the extraction takes bytes). At most the read's cap.
    pub fn bytes(&self) -> std::io::Result<Vec<u8>> {
        let mut f = std::fs::File::open(&self.path)?;
        let mut out = Vec::new();
        out.try_reserve_exact(usize::try_from(self.len).unwrap_or(0))
            .map_err(|_| std::io::Error::other("out of memory"))?;
        f.read_to_end(&mut out)?;
        Ok(out)
    }
}

impl Drop for Staged {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

fn io(e: &std::io::Error) -> StorageError {
    if e.kind() == std::io::ErrorKind::NotFound {
        return StorageError::gone();
    }
    eprintln!("noevia-server: staging a storage read failed: {}", e.kind());
    StorageError::new(500, "Internal error")
}

/// Removes staged files older than [`STALE`] (what a process that died mid-read left).
pub fn sweep(user_dir: &Path) {
    let Ok(read) = std::fs::read_dir(user_dir.join(DIR)) else {
        return;
    };
    for entry in read.flatten() {
        let old = entry
            .metadata()
            .ok()
            .and_then(|m| m.modified().ok())
            .and_then(|t| SystemTime::now().duration_since(t).ok())
            .is_some_and(|age| age > STALE);
        if old && entry.file_type().is_ok_and(|t| t.is_file()) {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

/// Reads `path` from storage into a staged file. On any failure the temp file is gone.
pub fn stage(
    user_dir: &Path,
    storage: &dyn SourceStorage,
    path: &str,
    read: BinaryRead<'_>,
) -> Result<Staged, StorageError> {
    let folder = user_dir.join(DIR);
    // Only `source-staging/` is made, below the user's directory: an account removed meanwhile is
    // gone (410), not recreated (#1425).
    crate::store::create_dirs_below(user_dir, &folder).map_err(|e| io(&e))?;
    let temp = folder.join(format!("{}.part", crate::js::random36(24)));
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&temp)
        .map_err(|e| io(&e))?;
    // From here the temp file is ours to remove, whatever happens.
    struct Cleanup(Option<PathBuf>);
    impl Drop for Cleanup {
        fn drop(&mut self) {
            if let Some(p) = self.0.take() {
                let _ = std::fs::remove_file(p);
            }
        }
    }
    let mut cleanup = Cleanup(Some(temp.clone()));
    let mut hash = Sha256::new();
    let mut len = 0u64;
    let mut sink = |chunk: &[u8]| -> Result<(), StorageError> {
        file.write_all(chunk).map_err(|e| io(&e))?;
        hash.update(chunk);
        len += chunk.len() as u64;
        Ok(())
    };
    storage.read_binary(path, read, &mut sink)?;
    file.sync_all().map_err(|e| io(&e))?;
    drop(file);
    let sha256: String = hash.finalize().iter().map(|b| format!("{b:02x}")).collect();
    let done = folder.join(format!("{sha256}.{}.bin", crate::js::random36(8)));
    std::fs::rename(&temp, &done).map_err(|e| io(&e))?;
    cleanup.0 = None;
    if let Ok(d) = std::fs::File::open(&folder) {
        let _ = d.sync_all();
    }
    Ok(Staged {
        path: done,
        sha256,
        len,
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing, clippy::panic)]
mod tests {
    use super::*;
    use crate::source_storage::fake::Fake;

    fn left(dir: &Path) -> Vec<String> {
        std::fs::read_dir(dir.join(DIR))
            .map(|r| {
                r.flatten()
                    .map(|e| e.file_name().to_string_lossy().into_owned())
                    .collect()
            })
            .unwrap_or_default()
    }

    fn hook(_: Option<u64>) -> Result<(), StorageError> {
        Ok(())
    }

    #[test]
    fn a_staged_read_is_hashed_fsynced_renamed_and_removed_with_its_handle() {
        let root = tempfile::tempdir().unwrap();
        let s = Fake::default();
        let body: Vec<u8> = (0..10_000_000u32).map(|i| (i % 253) as u8).collect();
        s.put("dir/a.pdf", &body);
        let mut h = hook;
        let staged = stage(
            root.path(),
            &s,
            "dir/a.pdf",
            BinaryRead {
                cap: 96 * 1024 * 1024,
                timeout: Duration::from_secs(30),
                before_body: &mut h,
            },
        )
        .unwrap();
        assert_eq!(staged.len, body.len() as u64);
        assert_eq!(staged.sha256, crate::uploads_ingest::sha_hex(&body));
        assert_eq!(staged.bytes().unwrap(), body);
        assert!(staged.path().to_string_lossy().ends_with(".bin"));
        assert_eq!(left(root.path()).len(), 1);
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(staged.path())
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600);
        drop(staged);
        assert!(
            left(root.path()).is_empty(),
            "the handle took its file with it"
        );
    }

    #[test]
    fn a_read_that_fails_part_way_leaves_no_temp_file() {
        let root = tempfile::tempdir().unwrap();
        // A storage that delivers two chunks, then loses its child.
        struct Crashes;
        impl SourceStorage for Crashes {
            fn s3(&self) -> bool {
                false
            }
            fn fingerprint(&self) -> String {
                String::new()
            }
            fn list(&self, _: &str) -> Result<Vec<crate::source_storage::Entry>, StorageError> {
                Ok(vec![])
            }
            fn read_text(&self, _: &str) -> Result<crate::source_storage::TextRead, StorageError> {
                Err(StorageError::new(500, "x"))
            }
            fn read_binary(
                &self,
                _: &str,
                _: BinaryRead<'_>,
                sink: &mut dyn FnMut(&[u8]) -> Result<(), StorageError>,
            ) -> Result<u64, StorageError> {
                sink(&vec![1u8; 8 * 1024 * 1024])?;
                sink(&vec![2u8; 8 * 1024 * 1024])?;
                Err(
                    StorageError::new(502, "the storage server could not be reached")
                        .with_code("unreachable"),
                )
            }
            fn create_folder(&self, _: &str) -> Result<bool, StorageError> {
                Ok(false)
            }
            fn write(
                &self,
                _: &str,
                _: &[u8],
                _: &crate::source_storage::Condition,
            ) -> Result<crate::source_storage::Written, StorageError> {
                Err(StorageError::new(500, "x"))
            }
            fn discard(&self, _: &str, _: &str) -> (bool, &'static str) {
                (false, "")
            }
        }
        let mut h = hook;
        let e = stage(
            root.path(),
            &Crashes,
            "dir/a.pdf",
            BinaryRead {
                cap: 96 * 1024 * 1024,
                timeout: Duration::from_secs(30),
                before_body: &mut h,
            },
        )
        .err()
        .unwrap();
        assert_eq!(e.failure().map(|f| f.kind), Some("unreachable"));
        assert!(
            left(root.path()).is_empty(),
            "no temp file: {:?}",
            left(root.path())
        );
        // A file over the cap is refused with nothing left behind either.
        let s = Fake::default();
        s.put("big.pdf", &vec![0u8; 2 * 1024 * 1024]);
        let mut h = hook;
        let e = stage(
            root.path(),
            &s,
            "big.pdf",
            BinaryRead {
                cap: 1024 * 1024,
                timeout: Duration::from_secs(30),
                before_body: &mut h,
            },
        )
        .err()
        .unwrap();
        assert_eq!(e.status, 413);
        assert!(left(root.path()).is_empty());
    }

    #[test]
    fn a_sweep_takes_only_old_files() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join(DIR)).unwrap();
        let old = root.path().join(DIR).join("old.part");
        let young = root.path().join(DIR).join("young.part");
        std::fs::write(&old, b"x").unwrap();
        std::fs::write(&young, b"x").unwrap();
        let three_hours_ago = SystemTime::now() - Duration::from_secs(3 * 60 * 60);
        std::fs::File::options()
            .write(true)
            .open(&old)
            .unwrap()
            .set_modified(three_hours_ago)
            .unwrap();
        sweep(root.path());
        assert!(!old.exists());
        assert!(young.exists());
    }
}
