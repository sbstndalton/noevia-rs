//! Explicit compiled capability advertisement, without launching any child or job.
#![allow(clippy::unwrap_used)]
#[test]
fn feature_advertisement_matches_the_build() {
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
}
