//! The release qualification is required, not an optional debug-test substitute.
#![allow(clippy::panic)]

fn semantic_suite_position(workflow: &str) -> Option<usize> {
    let name = "      - name: cargo test\n";
    let position = workflow.find(name)?;
    let block = workflow
        .get(position + name.len()..)?
        .split("\n      - ")
        .next()?;
    if block.lines().any(|line| {
        let line = line.trim();
        line.starts_with("continue-on-error:")
            || (line.starts_with("if:") && line != "if: steps.pkgs.outputs.any == 'true'")
    }) {
        return None;
    }
    let direct = "        run: cargo test --locked --workspace";
    let captured = "        run: |\n          set -euo pipefail\n          cargo test --locked --workspace --message-format=json | tee \"$RUNNER_TEMP/noevia-workspace-test-events.jsonl\"";
    (block.trim_end().ends_with(direct) || block.trim_end().ends_with(captured)).then_some(position)
}

#[test]
fn optimized_performance_phase_is_unconditional_and_covers_both_targets() {
    let workflow = include_str!("../../../.github/workflows/ci.yml");
    let rust = workflow
        .split_once("  rust:\n")
        .and_then(|(_, rest)| rest.split_once("\n  docling-native:\n").map(|(job, _)| job))
        .unwrap_or_else(|| panic!("required Rust workspace job missing"));
    let required = "      - name: Optimized code-actions performance qualification (#1328)\n        run: cargo test --locked --release -p code-actions --test deep_find --test properties -- --test-threads=1\n";
    let debug_position =
        semantic_suite_position(rust).unwrap_or_else(|| panic!("debug semantic suite missing"));
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

#[test]
fn captured_semantic_output_cannot_mask_or_narrow_the_required_suite() {
    let workflow = include_str!("../../../.github/workflows/ci.yml");
    assert!(semantic_suite_position(workflow).is_some());
    let captured = "        run: |\n          set -euo pipefail\n          cargo test --locked --workspace --message-format=json | tee \"$RUNNER_TEMP/noevia-workspace-test-events.jsonl\"";
    assert!(semantic_suite_position(
        &workflow.replace(captured, "        run: cargo test --locked --workspace")
    )
    .is_some());
    for changed in [
        workflow.replace(
            "set -euo pipefail\n          cargo test --locked --workspace",
            "set -eu\n          cargo test --locked --workspace",
        ),
        workflow.replace(
            "id: workspace_tests\n",
            "id: workspace_tests\n        continue-on-error: true\n",
        ),
        workflow.replace(
            "cargo test --locked --workspace --message-format=json",
            "cargo test --locked -p code-actions --message-format=json",
        ),
        workflow.replace(
            "id: workspace_tests\n        if: steps.pkgs.outputs.any == 'true'",
            "id: workspace_tests\n        if: false",
        ),
        workflow.replace(
            "tee \"$RUNNER_TEMP/noevia-workspace-test-events.jsonl\"",
            "tee \"$RUNNER_TEMP/noevia-workspace-test-events.jsonl\" || true",
        ),
    ] {
        assert_ne!(
            changed, workflow,
            "omission control must change the workflow"
        );
        assert!(semantic_suite_position(&changed).is_none());
    }
}
