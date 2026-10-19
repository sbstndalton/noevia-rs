#![allow(clippy::unwrap_used)]
#[test]
fn retired_planning_selectors_cannot_restore_child_ownership() {
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_noevia-model-manager"))
        .env_clear()
        .env("MODEL_MANAGER_DOWNLOAD_PLAN_IMPL", "rust")
        .env("MODEL_MANAGER_DOWNLOADS_IMPL", "python")
        .output()
        .unwrap();
    assert!(!output.status.success());
    let expected = if cfg!(feature = "native-routing") {
        "mandatory native service requires exact TZ=UTC"
    } else {
        "mandatory native-routing capability is not compiled"
    };
    assert!(String::from_utf8_lossy(&output.stderr).contains(expected));
}
