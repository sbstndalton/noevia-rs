#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]
use server_projects::{
    legacy_claim::{claim_root, ClaimError, MAX_BYTES, MAX_DEPTH, MAX_ENTRIES},
    lock,
};
use std::fs;
use std::path::Path;
const OWNER: &str = "11111111-2222-4333-8444-555555555555";
const OTHER: &str = "99999999-2222-4333-8444-555555555555";
fn claim_locked(root: &Path, id: &str) -> Result<bool, ClaimError> {
    let _lock = lock::acquire(&root.join("legacy-owner.json")).unwrap();
    claim_root(root, id)
}
fn mode(path: &Path) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    fs::metadata(path).unwrap().permissions().mode() & 0o777
}

#[test]
fn copies_the_node_claim_set_to_owner_and_backup_and_retains_source() {
    let root = tempfile::tempdir().unwrap();
    fs::write(
        root.path().join("projects.json"),
        r#"{"projects":[{"id":"old"}]}"#,
    )
    .unwrap();
    fs::write(
        root.path().join("providers.json"),
        r#"{"providers":[{"apiKey":"secret"}]}"#,
    )
    .unwrap();
    fs::write(root.path().join("free-chats.json"), "[]").unwrap();
    fs::write(root.path().join("auto-roles.json"), "{}").unwrap();
    fs::write(root.path().join("history-chat.json"), "{}").unwrap();
    fs::create_dir(root.path().join("rag")).unwrap();
    fs::write(root.path().join("rag/blob"), [0u8, 1, 2, 255]).unwrap();

    assert!(claim_locked(root.path(), OWNER).unwrap());
    let target = root.path().join("users").join(OWNER);
    let marker: serde_json::Value =
        serde_json::from_slice(&fs::read(target.join("migration.json")).unwrap()).unwrap();
    assert_eq!(marker["version"], 1);
    assert_eq!(marker["source"], "legacy-root");
    let backup = Path::new(marker["backup"].as_str().unwrap());
    assert_eq!(
        fs::read(backup.join("providers.json")).unwrap(),
        fs::read(root.path().join("providers.json")).unwrap()
    );
    assert_eq!(fs::read(target.join("rag/blob")).unwrap(), [0u8, 1, 2, 255]);
    assert_eq!(
        fs::read(root.path().join("projects.json")).unwrap(),
        br#"{"projects":[{"id":"old"}]}"#
    );
    assert_eq!(mode(&target), 0o700);
    assert_eq!(mode(backup), 0o700);
    assert_eq!(mode(&target.join("providers.json")), 0o600);
    assert_eq!(mode(&target.join("migration.json")), 0o600);
    assert_eq!(mode(&root.path().join("legacy-owner.json")), 0o600);
}
#[test]
fn trigger_owner_recovery_and_parallel_admins_match_the_node_contract() {
    let root = tempfile::tempdir().unwrap();
    fs::write(root.path().join("free-chats.json"), "[]").unwrap();
    assert!(!claim_locked(root.path(), OWNER).unwrap());
    fs::write(root.path().join("projects.json"), "{}").unwrap();
    let root = std::sync::Arc::new(root);
    let ids = [OWNER.to_string(), OTHER.to_string()];
    let joins: Vec<_> = ids
        .into_iter()
        .map(|id| {
            let root = root.clone();
            std::thread::spawn(move || claim_locked(root.path(), &id).unwrap())
        })
        .collect();
    for join in joins {
        let _ = join.join().unwrap();
    }
    let owner: serde_json::Value =
        serde_json::from_slice(&fs::read(root.path().join("legacy-owner.json")).unwrap()).unwrap();
    let owner = owner["userId"].as_str().unwrap();
    assert!(root
        .path()
        .join("users")
        .join(owner)
        .join("migration.json")
        .is_file());
    let other = if owner == OWNER { OTHER } else { OWNER };
    assert!(!root.path().join("users").join(other).exists());
}
#[test]
fn rejects_links_oversize_depth_and_entry_count_before_creating_output() {
    let linked = tempfile::tempdir().unwrap();
    fs::write(linked.path().join("projects.json"), "{}").unwrap();
    fs::write(linked.path().join("outside"), "private").unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink(
        linked.path().join("outside"),
        linked.path().join("history-link.json"),
    )
    .unwrap();
    assert!(matches!(
        claim_locked(linked.path(), OWNER),
        Err(ClaimError::Unsafe)
    ));
    assert!(!linked.path().join("users").exists());
    assert!(!linked.path().join("migration-backups").exists());

    let large = tempfile::tempdir().unwrap();
    fs::write(large.path().join("projects.json"), "{}").unwrap();
    fs::write(
        large.path().join("providers.json"),
        vec![0u8; (MAX_BYTES + 1) as usize],
    )
    .unwrap();
    assert!(matches!(
        claim_locked(large.path(), OWNER),
        Err(ClaimError::Limit)
    ));
    assert!(!large.path().join("users").exists());
    assert!(!large.path().join("migration-backups").exists());
    assert!(!large.path().join("legacy-owner.json").exists());

    let deep = tempfile::tempdir().unwrap();
    fs::write(deep.path().join("projects.json"), "{}").unwrap();
    let mut path = deep.path().join("rag");
    fs::create_dir(&path).unwrap();
    for _ in 0..=MAX_DEPTH {
        path.push("d");
        fs::create_dir(&path).unwrap();
    }
    assert!(matches!(
        claim_locked(deep.path(), OWNER),
        Err(ClaimError::Limit)
    ));
    assert!(!deep.path().join("users").exists());

    let many = tempfile::tempdir().unwrap();
    fs::write(many.path().join("projects.json"), "{}").unwrap();
    fs::create_dir(many.path().join("rag")).unwrap();
    for i in 0..MAX_ENTRIES {
        fs::write(many.path().join("rag").join(i.to_string()), "").unwrap();
    }
    assert!(matches!(
        claim_locked(many.path(), OWNER),
        Err(ClaimError::Limit)
    ));
    assert!(!many.path().join("users").exists());
}
#[test]
fn malformed_owner_and_non_owner_do_not_receive_legacy_data() {
    let malformed = tempfile::tempdir().unwrap();
    fs::write(malformed.path().join("projects.json"), "{}").unwrap();
    fs::write(malformed.path().join("legacy-owner.json"), "{").unwrap();
    assert!(!claim_locked(malformed.path(), OWNER).unwrap());
    assert!(!malformed.path().join("users").exists());

    let root = tempfile::tempdir().unwrap();
    fs::write(root.path().join("projects.json"), "{}").unwrap();
    fs::write(
        root.path().join("legacy-owner.json"),
        format!(r#"{{"userId":"{OWNER}"}}"#),
    )
    .unwrap();
    assert!(!claim_locked(root.path(), OTHER).unwrap());
    assert!(!root.path().join("users").join(OTHER).exists());
}
