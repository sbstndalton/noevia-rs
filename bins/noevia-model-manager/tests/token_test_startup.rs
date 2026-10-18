#![allow(clippy::unwrap_used)]
#[test]
fn native_token_test_requires_native_writer_before_any_startup_side_effect() {
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_noevia-model-manager"))
        .env_clear()
        .env("MODEL_MANAGER_TOKEN_TEST_IMPL", "rust")
        .env("MODEL_MANAGER_SETTINGS_WRITE_IMPL", "python")
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains(
        if cfg!(feature = "native-routing") {
            "requires exact TZ=UTC"
        } else {
            "mandatory native-routing capability"
        }
    ));
}
