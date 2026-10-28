//! One bounded root-owned receipt in the already protected socket volume.
//! Records successful test-helper creation, independently of payload progress.
use rustix::fs::{open, Mode, OFlags};
use serde_json::json;
use std::fs::File;
use std::io::{Seek, SeekFrom, Write};
use std::os::unix::fs::MetadataExt;
use std::path::Path;
type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;
pub(super) const NAME: &str = ".noevia-native-admission.json";
pub(super) struct Receipt {
    file: File,
    nonce: String,
    test_helpers: u8,
    complete: bool,
}
impl Receipt {
    pub(super) fn begin(parent: &Path, nonce: &str) -> Result<Self> {
        if !(32..=128).contains(&nonce.len())
            || !nonce
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            return Err("receipt nonce".into());
        }
        let owner = std::fs::metadata(parent)?;
        let file = File::from(open(
            parent.join(NAME),
            OFlags::RDWR | OFlags::CREATE | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
            Mode::from_raw_mode(0o600),
        )?);
        let m = file.metadata()?;
        // Validate before any truncation; do not damage a linked or foreign entry.
        if !m.is_file()
            || m.nlink() != 1
            || m.uid() != owner.uid()
            || m.gid() != owner.gid()
            || m.mode() & 0o7777 != 0o600
            || m.len() > 1024
        {
            return Err("receipt file authority".into());
        }
        let mut result = Self {
            file,
            nonce: nonce.to_owned(),
            test_helpers: 0,
            complete: false,
        };
        result.flush()?;
        Ok(result)
    }
    pub(super) fn test_helper_spawned(&mut self) -> Result<()> {
        if self.complete || self.test_helpers != 0 {
            return Err("unexpected test admission".into());
        }
        // Advance before IO: failure cannot restore a zero count after a real spawn.
        self.test_helpers = 1;
        self.flush()
    }
    pub(super) fn finish(&mut self) -> Result<()> {
        if self.complete {
            return Err("receipt already final".into());
        }
        self.complete = true;
        self.flush()
    }
    fn flush(&mut self) -> Result<()> {
        let bytes = serde_json::to_vec(&json!({
            "noevia":"verify-admission/1", "nonce":self.nonce,
            "testHelpersSpawned":self.test_helpers, "complete":self.complete
        }))?;
        if bytes.len() > 1024 {
            return Err("receipt bound".into());
        }
        self.file.seek(SeekFrom::Start(0))?;
        self.file.write_all(&bytes)?;
        self.file.set_len(u64::try_from(bytes.len())?)?;
        self.file.sync_data()?;
        Ok(())
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::{symlink, PermissionsExt};
    fn read(parent: &Path) -> Result<serde_json::Value> {
        Ok(serde_json::from_slice(&std::fs::read(parent.join(NAME))?)?)
    }
    #[test]
    fn recorded_spawn_and_early_failure_cannot_look_like_no_admission() -> Result<()> {
        let root = tempfile::tempdir()?;
        let nonce = "ab".repeat(16);
        let mut receipt = Receipt::begin(root.path(), &nonce)?;
        assert_eq!(
            read(root.path())?.get("testHelpersSpawned"),
            Some(&json!(0))
        );
        assert_eq!(read(root.path())?.get("complete"), Some(&json!(false)));
        receipt.test_helper_spawned()?;
        // No payload callback/write is involved; even an immediate child failure remains1.
        assert_eq!(
            read(root.path())?.get("testHelpersSpawned"),
            Some(&json!(1))
        );
        receipt.finish()?;
        assert_eq!(read(root.path())?.get("complete"), Some(&json!(true)));
        assert!(receipt.test_helper_spawned().is_err());
        assert!(receipt.finish().is_err());
        Ok(())
    }
    #[test]
    fn expired_before_spawn_zero_is_nonce_bound_and_final_not_missing() -> Result<()> {
        let root = tempfile::tempdir()?;
        let mut receipt = Receipt::begin(root.path(), &"cd".repeat(16))?;
        receipt.finish()?;
        let value = read(root.path())?;
        assert_eq!(value.get("nonce"), Some(&json!("cd".repeat(16))));
        assert_eq!(value.get("testHelpersSpawned"), Some(&json!(0)));
        assert_eq!(value.get("complete"), Some(&json!(true)));
        Ok(())
    }
    #[test]
    fn single_file_is_bounded_and_reset_for_a_new_request() -> Result<()> {
        let root = tempfile::tempdir()?;
        let mut receipt = Receipt::begin(root.path(), &"ab".repeat(64))?;
        receipt.test_helper_spawned()?;
        receipt.finish()?;
        drop(receipt);
        let receipt = Receipt::begin(root.path(), &"cd".repeat(16))?;
        assert_eq!(
            read(root.path())?.get("nonce"),
            Some(&json!("cd".repeat(16)))
        );
        assert_eq!(read(root.path())?.get("complete"), Some(&json!(false)));
        assert_eq!(
            read(root.path())?.get("testHelpersSpawned"),
            Some(&json!(0))
        );
        assert_eq!(std::fs::read_dir(root.path())?.count(), 1);
        assert!(std::fs::metadata(root.path().join(NAME))?.len() <= 1024);
        drop(receipt);
        Ok(())
    }
    #[test]
    fn symlink_hardlink_and_wrong_modes_refuse_without_truncating() -> Result<()> {
        let root = tempfile::tempdir()?;
        let victim = root.path().join("victim");
        std::fs::write(&victim, b"preserve")?;
        std::fs::set_permissions(&victim, std::fs::Permissions::from_mode(0o600))?;
        symlink(&victim, root.path().join(NAME))?;
        assert!(Receipt::begin(root.path(), &"ab".repeat(16)).is_err());
        std::fs::remove_file(root.path().join(NAME))?;
        std::fs::hard_link(&victim, root.path().join(NAME))?;
        assert!(Receipt::begin(root.path(), &"ab".repeat(16)).is_err());
        assert_eq!(std::fs::read(&victim)?, b"preserve");
        std::fs::remove_file(root.path().join(NAME))?;
        std::fs::write(root.path().join(NAME), b"wrong-mode")?;
        std::fs::set_permissions(
            root.path().join(NAME),
            std::fs::Permissions::from_mode(0o644),
        )?;
        assert!(Receipt::begin(root.path(), &"ab".repeat(16)).is_err());
        assert_eq!(std::fs::read(root.path().join(NAME))?, b"wrong-mode");
        Ok(())
    }
}
