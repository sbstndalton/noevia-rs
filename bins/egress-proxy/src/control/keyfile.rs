//! Permission-safe current key observations on a reviewed LOCAL key volume.
//! Configuration is trusted service configuration, never a task/request path.
//! No key, directory descriptor, failure or prior success is cached. This source
//! does not enable a mode, open a production path or qualify container mounts.
use super::engine::{Error, KeyProvider};
use egress::control::FileKey;
use std::{
    ffi::OsString,
    path::{Component, Path},
};
pub struct KeyFile {
    parents: Vec<OsString>,
    name: OsString,
    uid: u32,
    gid: u32,
}
impl std::fmt::Debug for KeyFile {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("KeyFile(<configured>)")
    }
}
impl KeyFile {
    /// Exact owner IDs must come from the independently reviewed service profile.
    /// This does not grant supplementary groups or infer filesystem permissions.
    pub fn new(path: &Path, uid: u32, gid: u32) -> Result<Self, Error> {
        if !cfg!(unix)
            || !path.is_absolute()
            || path.as_os_str().len() > 4096
            || path.as_os_str().as_encoded_bytes().last() == Some(&b'/')
        {
            return Err(Error::Key);
        }
        let mut parts = Vec::new();
        for component in path.components() {
            match component {
                Component::RootDir => {}
                Component::Normal(p) => parts.push(p.to_os_string()),
                _ => return Err(Error::Key),
            }
        }
        if parts.is_empty() || parts.len() > 64 {
            return Err(Error::Key);
        }
        let name = parts.pop().ok_or(Error::Key)?;
        Ok(Self {
            parents: parts,
            name,
            uid,
            gid,
        })
    }
}
#[cfg(unix)]
mod unix {
    use super::*;
    use rustix::fs::{open, openat, statat, AtFlags, FileType, Mode, OFlags};
    use std::{
        fs::{File, Metadata},
        io::Read,
        os::unix::fs::{MetadataExt, PermissionsExt},
    };
    impl KeyFile {
        fn parent(&self) -> Result<File, Error> {
            let flags = OFlags::RDONLY
                | OFlags::DIRECTORY
                | OFlags::NOFOLLOW
                | OFlags::NONBLOCK
                | OFlags::CLOEXEC;
            let mut fd = open("/", flags, Mode::empty()).map_err(|_| Error::Key)?;
            for name in &self.parents {
                fd = openat(&fd, name.as_os_str(), flags, Mode::empty()).map_err(|_| Error::Key)?;
            }
            let parent: File = fd.into();
            self.check_parent(&parent.metadata().map_err(|_| Error::Key)?)?;
            Ok(parent)
        }
        fn check_parent(&self, m: &Metadata) -> Result<(), Error> {
            if !m.is_dir()
                || !(m.uid() == 0 || m.uid() == self.uid)
                || m.permissions().mode() & 0o022 != 0
            {
                return Err(Error::Key);
            }
            Ok(())
        }
        fn entry(&self, parent: &File) -> Result<File, Error> {
            // Do not even open a known device/FIFO/symlink. NONBLOCK/NOFOLLOW also
            // protect the race between this check and descriptor acquisition.
            let s = statat(parent, self.name.as_os_str(), AtFlags::SYMLINK_NOFOLLOW)
                .map_err(|_| Error::Key)?;
            if FileType::from_raw_mode(s.st_mode) != FileType::RegularFile {
                return Err(Error::Key);
            }
            let flags = OFlags::RDONLY
                | OFlags::NOFOLLOW
                | OFlags::NONBLOCK
                | OFlags::CLOEXEC
                | OFlags::NOCTTY;
            let fd = openat(parent, self.name.as_os_str(), flags, Mode::empty())
                .map_err(|_| Error::Key)?;
            Ok(fd.into())
        }
        fn check_file(&self, m: &Metadata) -> Result<(), Error> {
            if !m.is_file()
                || m.nlink() != 1
                || m.len() != 65
                || m.uid() != self.uid
                || m.gid() != self.gid
                || m.permissions().mode() & 0o7777 != 0o440
            {
                return Err(Error::Key);
            }
            Ok(())
        }
        // The hook is private and used only for deterministic owned-fixture races;
        // production calls an empty closure and exposes no clock/file mutation hooks.
        fn read_with(&self, hook: impl FnOnce()) -> Result<FileKey, Error> {
            let parent = self.parent()?;
            let parent_before = parent.metadata().map_err(|_| Error::Key)?;
            let mut file = self.entry(&parent)?;
            let before = file.metadata().map_err(|_| Error::Key)?;
            self.check_file(&before)?;
            let mut bytes = Vec::with_capacity(66);
            (&mut file)
                .take(66)
                .read_to_end(&mut bytes)
                .map_err(|_| Error::Key)?;
            hook();
            let after = file.metadata().map_err(|_| Error::Key)?;
            self.check_file(&after)?;
            if !same_file(&before, &after) || bytes.len() != 65 || bytes.last() != Some(&b'\n') {
                return Err(Error::Key);
            }
            let now_parent = self.parent()?;
            let parent_after = now_parent.metadata().map_err(|_| Error::Key)?;
            if parent_before.dev() != parent_after.dev()
                || parent_before.ino() != parent_after.ino()
            {
                return Err(Error::Key);
            }
            let current = self.entry(&now_parent)?;
            let observed = current.metadata().map_err(|_| Error::Key)?;
            self.check_file(&observed)?;
            if !same_file(&after, &observed) {
                return Err(Error::Key);
            }
            let text = std::str::from_utf8(&bytes).map_err(|_| Error::Key)?;
            FileKey::from_hex(text).map_err(|_| Error::Key)
        }
    }
    fn same_file(a: &Metadata, b: &Metadata) -> bool {
        a.dev() == b.dev()
            && a.ino() == b.ino()
            && a.len() == b.len()
            && a.uid() == b.uid()
            && a.gid() == b.gid()
            && a.mode() == b.mode()
            && a.nlink() == b.nlink()
            && a.mtime() == b.mtime()
            && a.mtime_nsec() == b.mtime_nsec()
            && a.ctime() == b.ctime()
            && a.ctime_nsec() == b.ctime_nsec()
    }
    impl KeyProvider for KeyFile {
        fn current(&self) -> Result<FileKey, Error> {
            self.read_with(|| {})
        }
    }
    #[cfg(test)]
    mod tests {
        use super::*;
        use std::{fs, os::unix::fs::symlink, path::PathBuf};
        struct Tree(PathBuf);
        impl Tree {
            fn new() -> Result<Self, Box<dyn std::error::Error>> {
                let mut nonce = [0u8; 16];
                getrandom::fill(&mut nonce).map_err(|_| "entropy")?;
                let id: String = nonce.iter().map(|b| format!("{b:02x}")).collect();
                let p = std::env::temp_dir().join(format!("noevia-key-{id}"));
                fs::create_dir(&p)?;
                fs::set_permissions(&p, fs::Permissions::from_mode(0o700))?;
                Ok(Self(p.canonicalize()?))
            }
            fn key(&self) -> PathBuf {
                self.0.join("control.key")
            }
            fn write(&self, hex: &str) -> Result<(), Box<dyn std::error::Error>> {
                write(&self.key(), hex)
            }
            fn provider(&self) -> Result<KeyFile, Box<dyn std::error::Error>> {
                let m = fs::metadata(self.key())?;
                Ok(KeyFile::new(&self.key(), m.uid(), m.gid())?)
            }
        }
        impl Drop for Tree {
            fn drop(&mut self) {
                let _ = fs::remove_dir_all(&self.0);
            }
        }
        fn write_raw(p: &Path, bytes: &[u8]) -> Result<(), Box<dyn std::error::Error>> {
            if p.exists() {
                fs::set_permissions(p, fs::Permissions::from_mode(0o600))?;
            }
            fs::write(p, bytes)?;
            fs::set_permissions(p, fs::Permissions::from_mode(0o440))?;
            Ok(())
        }
        fn write(p: &Path, hex: &str) -> Result<(), Box<dyn std::error::Error>> {
            write_raw(p, format!("{hex}\n").as_bytes())
        }
        fn key() -> String {
            "07".repeat(32)
        }
        #[test]
        fn exact_descriptor_snapshot_and_replacement_between_calls_are_current(
        ) -> Result<(), Box<dyn std::error::Error>> {
            let t = Tree::new()?;
            t.write(&key())?;
            let p = t.provider()?;
            assert_eq!(
                p.current()?.fingerprint(),
                FileKey::from_bytes([7; 32]).fingerprint()
            );
            let replacement = t.0.join("new.key");
            write(&replacement, &"08".repeat(32))?;
            fs::rename(&replacement, t.key())?;
            assert_eq!(
                p.current()?.fingerprint(),
                FileKey::from_bytes([8; 32]).fingerprint()
            );
            assert!(!format!("{p:?}").contains(&key()));
            fs::remove_file(t.key())?;
            assert_eq!(p.current().err(), Some(Error::Key));
            Ok(())
        }
        #[test]
        fn wrong_identity_modes_format_and_lengths_refuse_without_cached_fallback(
        ) -> Result<(), Box<dyn std::error::Error>> {
            let t = Tree::new()?;
            t.write(&key())?;
            let p = t.provider()?;
            let m = fs::metadata(t.key())?;
            assert_eq!(
                KeyFile::new(&t.key(), m.uid().wrapping_add(1), m.gid())?
                    .current()
                    .err(),
                Some(Error::Key)
            );
            assert_eq!(
                KeyFile::new(&t.key(), m.uid(), m.gid().wrapping_add(1))?
                    .current()
                    .err(),
                Some(Error::Key)
            );
            for mode in [0o400, 0o640, 0o444, 0o660, 0o2440] {
                fs::set_permissions(t.key(), fs::Permissions::from_mode(mode))?;
                assert_eq!(p.current().err(), Some(Error::Key));
            }
            for text in [
                key(),
                format!("{}\r\n", key()),
                format!("{}\n\n", key()),
                format!("{}\n", "AB".repeat(32)),
                format!("{}\n", "g".repeat(64)),
                "a".repeat(100_000),
            ] {
                write_raw(&t.key(), text.as_bytes())?;
                assert_eq!(p.current().err(), Some(Error::Key));
            }
            t.write(&key())?;
            assert!(p.current().is_ok());
            fs::set_permissions(&t.0, fs::Permissions::from_mode(0o777))?;
            assert_eq!(p.current().err(), Some(Error::Key));
            Ok(())
        }
        #[test]
        fn symlink_ancestors_hardlinks_and_nonregular_entries_refuse(
        ) -> Result<(), Box<dyn std::error::Error>> {
            let t = Tree::new()?;
            t.write(&key())?;
            let p = t.provider()?;
            let link = t.0.join("alias.key");
            fs::hard_link(t.key(), &link)?;
            assert_eq!(p.current().err(), Some(Error::Key));
            fs::remove_file(&link)?;
            fs::rename(t.key(), &link)?;
            symlink(&link, t.key())?;
            assert_eq!(p.current().err(), Some(Error::Key));
            fs::remove_file(t.key())?;
            fs::create_dir(t.key())?;
            assert_eq!(p.current().err(), Some(Error::Key));
            fs::remove_dir(t.key())?;
            assert!(std::process::Command::new("mkfifo")
                .arg(t.key())
                .status()?
                .success());
            assert_eq!(p.current().err(), Some(Error::Key));
            fs::remove_file(t.key())?;
            let target = t.0.join("actual");
            fs::create_dir(&target)?;
            write(&target.join("control.key"), &key())?;
            symlink(&target, t.0.join("dirlink"))?;
            assert_eq!(
                KeyFile::new(&t.0.join("dirlink/control.key"), p.uid, p.gid)?
                    .current()
                    .err(),
                Some(Error::Key)
            );
            assert!(KeyFile::new(Path::new("relative.key"), p.uid, p.gid).is_err());
            assert!(KeyFile::new(&t.0.join("../control.key"), p.uid, p.gid).is_err());
            Ok(())
        }
        #[test]
        fn deterministic_entry_and_permission_replacement_during_read_fail_closed(
        ) -> Result<(), Box<dyn std::error::Error>> {
            let t = Tree::new()?;
            t.write(&key())?;
            let p = t.provider()?;
            let result = p.read_with(|| {
                let path = t.0.join("swap.key");
                assert!(write(&path, &"08".repeat(32)).is_ok());
                assert!(fs::rename(path, t.key()).is_ok());
            });
            assert_eq!(result.err(), Some(Error::Key));
            assert_eq!(
                p.current()?.fingerprint(),
                FileKey::from_bytes([8; 32]).fingerprint()
            );
            let result = p.read_with(|| {
                assert!(fs::set_permissions(t.key(), fs::Permissions::from_mode(0o400)).is_ok());
            });
            assert_eq!(result.err(), Some(Error::Key));
            Ok(())
        }
        #[test]
        fn detached_parent_path_is_rejected_even_when_open_file_remains_valid(
        ) -> Result<(), Box<dyn std::error::Error>> {
            let t = Tree::new()?;
            let parent = t.0.join("keys");
            fs::create_dir(&parent)?;
            let path = parent.join("control.key");
            write(&path, &key())?;
            let m = fs::metadata(&path)?;
            let p = KeyFile::new(&path, m.uid(), m.gid())?;
            let result = p.read_with(|| {
                assert!(fs::rename(&parent, t.0.join("old-keys")).is_ok());
                assert!(fs::create_dir(&parent).is_ok());
                assert!(write(&path, &"08".repeat(32)).is_ok());
            });
            assert_eq!(result.err(), Some(Error::Key));
            assert_eq!(
                p.current()?.fingerprint(),
                FileKey::from_bytes([8; 32]).fingerprint()
            );
            Ok(())
        }
    }
}
#[cfg(not(unix))]
impl KeyProvider for KeyFile {
    fn current(&self) -> Result<FileKey, Error> {
        let _ = (&self.parents, &self.name, self.uid, self.gid);
        Err(Error::Key)
    }
}
