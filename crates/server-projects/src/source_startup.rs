//! Startup-only inline source migration. Original bytes are backed up under the shared
//! projects lock; all content files are durable before projects.json points at them.
use crate::{lifecycle, lock, store};
use js_json::{wtf16, JValue};
use server_store::RustProjects;
use sha2::{Digest, Sha256};
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

fn inline(project: &JValue) -> bool {
    matches!(project.get("files"), JValue::Arr(files) if files.iter().any(|f| f.get("content").as_str().is_some()))
}

fn backup(dir: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let folder = dir.join("migration-backups");
    store::create_dirs_below(dir, &folder)?;
    let hash = format!("{:x}", Sha256::digest(bytes));
    let target = folder.join(format!("projects-pre-1806-{}.json", &hash[..16]));
    if target.exists() {
        // A retry must never trust a torn or unrelated existing backup.
        if std::fs::read(&target)? != bytes {
            return Err(std::io::ErrorKind::InvalidData.into());
        }
    } else {
        let temp = folder.join(format!(".source-startup-{}.tmp", crate::js::random36(12)));
        let result = (|| {
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .custom_flags(libc::O_NOFOLLOW)
                .open(&temp)?;
            file.write_all(bytes)?;
            file.sync_all()?;
            std::fs::rename(&temp, &target)
        })();
        if result.is_err() {
            let _ = std::fs::remove_file(&temp);
        }
        result?;
    }
    std::fs::File::open(&folder)?.sync_all()?;
    std::fs::File::open(dir)?.sync_all()
}

/// No directory creation for missing accounts; no rewrite when nothing is inline.
/// The caller handles each account independently, as workspace.migrateSourceText did.
pub fn migrate(dir: &Path, switch: RustProjects) -> Result<bool, store::StoreError> {
    if !dir.join(store::PROJECTS_FILE).exists() {
        return Ok(false);
    }
    store::existing(dir)?;
    let _held = lock::acquire(&dir.join(store::PROJECTS_FILE))?;
    let bytes = std::fs::read(dir.join(store::PROJECTS_FILE))?;
    let mut doc =
        wtf16::parse(&String::from_utf8_lossy(&bytes)).map_err(|_| store::StoreError::Corrupt)?;
    let JValue::Obj(fields) = &mut doc else {
        return Ok(false);
    };
    let Some((_, JValue::Arr(projects))) = fields.iter_mut().find(|(k, _)| k == "projects") else {
        return Ok(false);
    };
    if !projects.iter().any(inline) {
        return Ok(false);
    }
    backup(dir, &bytes)?;
    for project in projects.iter_mut() {
        if project.get("id").as_str().is_none() {
            continue;
        }
        // Existing native externalization has the same durable file writer. Normalize
        // lone UTF-16 surrogates as Buffer.from(text, 'utf8') does, keeping JS length.
        let mut lengths = Vec::new();
        if let JValue::Obj(p) = project {
            if let Some((_, JValue::Arr(files))) = p.iter_mut().find(|(k, _)| k == "files") {
                for (index, file) in files.iter_mut().enumerate() {
                    if let Some(text) = file.get("content").as_str() {
                        let len = wtf16::len(text);
                        let normalized: String = text
                            .chars()
                            .map(|c| {
                                if wtf16::lone_unit(c).is_some() {
                                    '\u{fffd}'
                                } else {
                                    c
                                }
                            })
                            .collect();
                        if let JValue::Obj(items) = file {
                            items.retain(|(k, _)| k != "textRef" && k != "chars");
                        }
                        store::set(file, "content", JValue::Str(normalized));
                        lengths.push((index, len));
                    }
                }
            }
        }
        lifecycle::externalize(dir, project)?;
        if let JValue::Obj(p) = project {
            if let Some((_, JValue::Arr(files))) = p.iter_mut().find(|(k, _)| k == "files") {
                for (index, len) in lengths {
                    let file = files.get_mut(index).ok_or(store::StoreError::Corrupt)?;
                    store::set(file, "chars", JValue::Num(len as f64));
                }
            }
        }
    }
    let text = wtf16::stringify_pretty(&doc).ok_or(store::StoreError::Corrupt)?;
    server_store::json::write_shared_projects(dir, store::PROJECTS_FILE, &text, switch)
        .map_err(store::StoreError::Write)?;
    std::fs::File::open(dir)?.sync_all()?;
    Ok(true)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
mod tests {
    use super::*;
    #[test]
    fn backup_text_and_metadata_are_compatible_and_retry_is_a_noop() {
        let dir = tempfile::tempdir().unwrap();
        let original = br#"{"extra":true,"projects":[{"id":"p/1","files":[{"name":"x","content":"A\ud800\ud83d\ude00","textRef":"stale","chars":99}]}]}"#;
        std::fs::write(dir.path().join("projects.json"), original).unwrap();
        assert!(migrate(dir.path(), RustProjects::owned()).unwrap());
        let doc = store::read_json(&dir.path().join("projects.json"))
            .unwrap()
            .unwrap();
        assert_eq!(doc.get("extra"), &JValue::Bool(true));
        let JValue::Arr(projects) = doc.get("projects") else {
            panic!()
        };
        let JValue::Arr(files) = projects[0].get("files") else {
            panic!()
        };
        assert_eq!(files[0].get("chars"), &JValue::Num(4.0));
        assert!(files[0].get("content").as_str().is_none());
        let reference = files[0].get("textRef").as_str().unwrap();
        assert_eq!(
            std::fs::read_to_string(
                dir.path()
                    .join("project-sources/p1")
                    .join(format!("{reference}.txt"))
            )
            .unwrap(),
            "A�😀"
        );
        let backups: Vec<_> = std::fs::read_dir(dir.path().join("migration-backups"))
            .unwrap()
            .collect();
        assert_eq!(backups.len(), 1);
        assert_eq!(
            std::fs::read(backups[0].as_ref().unwrap().path()).unwrap(),
            original
        );
        let bytes = std::fs::read(dir.path().join("projects.json")).unwrap();
        assert!(!migrate(dir.path(), RustProjects::owned()).unwrap());
        assert_eq!(
            std::fs::read(dir.path().join("projects.json")).unwrap(),
            bytes
        );
    }
    #[test]
    fn backup_failure_leaves_inline_json_and_no_text_files() {
        let dir = tempfile::tempdir().unwrap();
        let original = br#"{"projects":[{"id":"p","files":[{"content":"hello"}]}]}"#;
        std::fs::write(dir.path().join("projects.json"), original).unwrap();
        std::fs::write(dir.path().join("migration-backups"), b"blocked").unwrap();
        assert!(migrate(dir.path(), RustProjects::owned()).is_err());
        assert_eq!(
            std::fs::read(dir.path().join("projects.json")).unwrap(),
            original
        );
        assert!(!dir.path().join("project-sources").exists());
    }
    #[test]
    fn missing_or_corrupt_workspace_is_never_recreated_or_replaced() {
        let dir = tempfile::tempdir().unwrap();
        assert!(!migrate(&dir.path().join("absent"), RustProjects::owned()).unwrap());
        std::fs::write(dir.path().join("projects.json"), b"{bad").unwrap();
        assert!(migrate(dir.path(), RustProjects::owned()).is_err());
        assert_eq!(
            std::fs::read(dir.path().join("projects.json")).unwrap(),
            b"{bad"
        );
    }
    #[test]
    fn a_failed_text_write_keeps_inline_json_and_retry_repairs_it() {
        let dir = tempfile::tempdir().unwrap();
        let original = br#"{"projects":[{"id":"p","files":[{"content":"hello"}]}]}"#;
        std::fs::write(dir.path().join("projects.json"), original).unwrap();
        std::fs::write(dir.path().join("project-sources"), b"blocked").unwrap();
        assert!(migrate(dir.path(), RustProjects::owned()).is_err());
        assert_eq!(
            std::fs::read(dir.path().join("projects.json")).unwrap(),
            original
        );
        assert_eq!(
            std::fs::read_dir(dir.path().join("migration-backups"))
                .unwrap()
                .count(),
            1
        );
        std::fs::remove_file(dir.path().join("project-sources")).unwrap();
        assert!(migrate(dir.path(), RustProjects::owned()).unwrap());
        assert_eq!(
            std::fs::read_dir(dir.path().join("migration-backups"))
                .unwrap()
                .count(),
            1
        );
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(dir.path().join("migration-backups"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        let backup = std::fs::read_dir(dir.path().join("migration-backups"))
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        assert_eq!(
            std::fs::metadata(backup).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
}
