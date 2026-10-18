//! Default-off and compiled-feature omission contracts for database bootstrap.
#![cfg(feature = "native-prompts")]
#![allow(clippy::unwrap_used)]
use noevia_model_manager::prompts::Prompts;
use std::os::unix::fs::PermissionsExt;
#[tokio::test]
async fn default_prompt_startup_does_not_take_full_schema_ownership() {
    let root = tempfile::tempdir_in("/tmp").unwrap();
    std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let prompts = Prompts::open(root.path()).await.unwrap();
    let db = rusqlite::Connection::open(root.path().join("model_loader.db")).unwrap();
    let names: Vec<String> = db.prepare("SELECT name FROM sqlite_schema WHERE type='table' AND name NOT LIKE 'sqlite_%' ORDER BY name")
        .unwrap().query_map([], |r| r.get(0)).unwrap().collect::<rusqlite::Result<_>>().unwrap();
    assert_eq!(names, ["kv", "prompts"]);
    drop(prompts);
}
#[cfg(not(feature = "native-db-init"))]
#[tokio::test]
async fn omitted_bootstrap_feature_refuses_without_opening_or_seeding_a_database() {
    let root = tempfile::tempdir_in("/tmp").unwrap();
    assert!(Prompts::open_with_schema(root.path(), true).await.is_err());
    assert!(!root.path().join("model_loader.db").exists());
}

#[test]
fn the_real_binary_advertises_the_feature_and_refuses_invalid_ownership_before_io() {
    let binary = env!("CARGO_BIN_EXE_noevia-model-manager");
    let shown = std::process::Command::new(binary)
        .env_clear()
        .arg("--features")
        .output()
        .unwrap();
    assert!(shown.status.success());
    let features = std::str::from_utf8(&shown.stdout).unwrap();
    assert_eq!(
        features.lines().any(|line| line == "native-db-init"),
        cfg!(feature = "native-db-init")
    );
    for mode in ["on", "rust"] {
        let root = tempfile::tempdir_in("/tmp").unwrap();
        let output = std::process::Command::new(binary)
            .env_clear()
            .env("MODEL_MANAGER_IMPL", "rust")
            .env("MODEL_MANAGER_PROMPTS_IMPL", "python")
            .env("MODEL_MANAGER_DB_INIT_IMPL", mode)
            .env("DATA_DIR", root.path())
            .output()
            .unwrap();
        assert!(!output.status.success());
        assert!(!root.path().join("model_loader.db").exists());
        let error = std::str::from_utf8(&output.stderr).unwrap();
        assert!(
            error.contains("MODEL_MANAGER_DB_INIT_IMPL")
                || error.contains("native DB initialization")
        );
    }
}
