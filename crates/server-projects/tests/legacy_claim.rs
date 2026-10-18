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
fn hex_bytes(text: &str) -> Vec<u8> {
    assert_eq!(text.len() % 2, 0);
    let (pairs, remainder) = text.as_bytes().as_chunks::<2>();
    assert!(remainder.is_empty());
    pairs
        .iter()
        .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap())
        .collect()
}
fn hex_text(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}
fn oracle_path(rel: &str) -> String {
    let parts = rel.split('/');
    let mut out = Vec::new();
    for part in parts {
        if out.last().is_some_and(|p: &&str| *p == "migration-backups")
            && part.starts_with("legacy-")
        {
            out.push("legacy");
        } else {
            out.push(part);
        }
    }
    out.join("/")
}
fn oracle_snapshot(root: &Path, result: bool) -> serde_json::Value {
    fn visit(root: &Path, rel: &Path, tree: &mut Vec<serde_json::Value>) {
        let path = root.join(rel);
        let meta = fs::symlink_metadata(&path).unwrap();
        let raw = rel.to_string_lossy().replace('\\', "/");
        let canonical = oracle_path(&raw);
        if meta.file_type().is_symlink() {
            tree.push(serde_json::json!({"path":canonical,"link":fs::read_link(path).unwrap().to_string_lossy()}));
        } else if meta.is_dir() {
            tree.push(serde_json::json!({"path":canonical,"type":"dir"}));
            let mut children: Vec<_> = fs::read_dir(path)
                .unwrap()
                .map(|entry| entry.unwrap().file_name())
                .collect();
            children.sort();
            for child in children {
                visit(root, &rel.join(child), tree);
            }
        } else {
            let bytes = fs::read(&path).unwrap();
            let mut record = serde_json::json!({"path":canonical,"type":"file"});
            if rel
                .file_name()
                .is_some_and(|name| name == "legacy-owner.json")
            {
                record["json"] = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
                if record["json"].is_null() {
                    record["json"] = serde_json::Value::Null;
                    record["hex"] = hex_text(&bytes).into();
                    record.as_object_mut().unwrap().remove("json");
                }
            } else if rel.file_name().is_some_and(|name| name == "migration.json")
                && rel.components().any(|part| part.as_os_str() == "users")
            {
                if let Ok(mut marker) = serde_json::from_slice::<serde_json::Value>(&bytes) {
                    if let Some(backup) = marker.get("backup").and_then(|value| value.as_str()) {
                        let relative = Path::new(backup).strip_prefix(root).unwrap();
                        marker["backup"] =
                            oracle_path(&relative.to_string_lossy().replace('\\', "/")).into();
                    }
                    marker.as_object_mut().unwrap().remove("migratedAt");
                    record["json"] = marker;
                } else {
                    record["hex"] = hex_text(&bytes).into();
                }
            } else {
                record["hex"] = hex_text(&bytes).into();
            }
            tree.push(record);
        }
    }
    let mut tree = Vec::new();
    let mut children: Vec<_> = fs::read_dir(root)
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect();
    children.sort();
    for child in children {
        visit(root, Path::new(&child), &mut tree);
    }
    serde_json::json!({"result":result,"tree":tree})
}

fn mode(path: &Path) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    fs::metadata(path).unwrap().permissions().mode() & 0o777
}

#[test]
fn actual_legacy_node_source_differential_matches_full_synthetic_tree() {
    let fixture: serde_json::Value =
        serde_json::from_str(include_str!("fixtures/node-legacy-claim.v1.json")).unwrap();
    assert_eq!(
        fixture["oracle"]["core"],
        "62abda5fbaeaa62d309a3fc8e19f1096afb5591b"
    );
    for case in fixture["cases"].as_array().unwrap() {
        let root = tempfile::tempdir().unwrap();
        for item in case["files"].as_array().unwrap() {
            let rel = item["path"].as_str().unwrap();
            let file = root.path().join(rel);
            fs::create_dir_all(file.parent().unwrap()).unwrap();
            fs::write(file, hex_bytes(item["hex"].as_str().unwrap())).unwrap();
        }
        for id in case["prior"].as_array().unwrap() {
            let user = root.path().join("users").join(id.as_str().unwrap());
            fs::create_dir_all(&user).unwrap();
            fs::write(user.join("migration.json"), r#"{"version":1}"#).unwrap();
        }
        if !case["owner"].is_null() {
            let owner = &case["owner"];
            let bytes = if let Some(text) = owner.as_str() {
                text.as_bytes().to_vec()
            } else {
                serde_json::to_vec(owner).unwrap()
            };
            fs::write(root.path().join("legacy-owner.json"), bytes).unwrap();
        }
        let result = claim_root(root.path(), case["user"].as_str().unwrap()).unwrap();
        let actual = oracle_snapshot(root.path(), result);
        assert_eq!(
            actual,
            case["expected"],
            "Rust claim output diverged from immutable Node source for case {}",
            case["name"].as_str().unwrap()
        );
    }
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
fn previous_owner_selection_is_lexical_not_filesystem_iteration_order() {
    let root = tempfile::tempdir().unwrap();
    fs::write(root.path().join("projects.json"), "{}").unwrap();
    let first = "00000000-0000-4000-8000-000000000000";
    let later = "22222222-2222-4222-8222-222222222222";
    // Create in reverse lexical order; Node sorts readdirSync before choosing.
    for id in [later, first] {
        let user = root.path().join("users").join(id);
        fs::create_dir_all(&user).unwrap();
        fs::write(user.join("migration.json"), r#"{"version":1}"#).unwrap();
    }

    assert!(!claim_locked(root.path(), OWNER).unwrap());
    let owner: serde_json::Value =
        serde_json::from_slice(&fs::read(root.path().join("legacy-owner.json")).unwrap()).unwrap();
    assert_eq!(owner["userId"], first);
    assert!(!root.path().join("users").join(OWNER).exists());
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
