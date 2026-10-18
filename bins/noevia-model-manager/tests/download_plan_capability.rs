#![allow(clippy::unwrap_used)]
// Explicit selection must refuse startup when either compiled planning or native jobs is absent.
#[test]
fn explicit_planning_requires_both_capabilities_before_child_start() {
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_noevia-model-manager"))
        .env_clear()
        .env("MODEL_MANAGER_DOWNLOAD_PLAN_IMPL", "rust")
        .env("MODEL_MANAGER_DOWNLOADS_IMPL", "python")
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains(
        "native download planning requires compiled native-download-plan and native downloads"
    ));
}
