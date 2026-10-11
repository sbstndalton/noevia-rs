//! The release qualification is required, not an optional debug-test substitute.
#![allow(clippy::panic)]
#[test]
fn optimized_performance_phase_is_unconditional_and_covers_both_targets() {
    let workflow = include_str!("../../../.github/workflows/ci.yml");
    let rust = workflow
        .split_once("  rust:\n")
        .and_then(|(_, rest)| rest.split_once("\n  docling-native:\n").map(|(job, _)| job))
        .unwrap_or_else(|| panic!("required Rust workspace job missing"));
    let required = "      - name: Optimized code-actions performance qualification (#1328)\n        run: cargo test --locked --release -p code-actions --test deep_find --test properties -- --test-threads=1\n";
    let debug_position = rust
        .find("run: cargo test --locked --workspace")
        .unwrap_or_else(|| panic!("debug semantic suite missing"));
    let release_position = rust
        .find(required)
        .unwrap_or_else(|| panic!("mandatory optimized phase missing or conditional"));
    assert!(
        release_position > debug_position,
        "qualification must follow semantic suite"
    );
    let prefix = rust.split("    steps:\n").next().unwrap_or("");
    assert!(
        !prefix
            .lines()
            .any(|line| line.trim_start().starts_with("if:")),
        "Rust job cannot condition away qualification"
    );
}
