//! Explicit compiled capability and selector omission, without launching any child or job.
#![allow(clippy::unwrap_used)]
#[test]
fn feature_advertisement_and_deliberate_omission() {
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_noevia-model-manager"))
        .arg("--features")
        .output()
        .unwrap();
    assert_eq!(output.status.success(), cfg!(feature = "native-logs"));
    let features = String::from_utf8(output.stdout).unwrap();
    assert_eq!(
        features.lines().any(|line| line == "native-bench-observe"),
        cfg!(feature = "native-bench-observe")
    );
    let rust = vec![("MODEL_MANAGER_BENCHMARK_OBSERVE_IMPL".into(), "rust".into())];
    assert_eq!(
        noevia_model_manager::bench_observe_cmd::select(&rust, true).is_ok(),
        cfg!(feature = "native-bench-observe")
    );
    assert!(noevia_model_manager::bench_observe_cmd::select(&rust, false).is_err());
}
