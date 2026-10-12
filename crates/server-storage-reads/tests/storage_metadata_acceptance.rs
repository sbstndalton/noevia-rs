//! Pure source controls. Uncompiled/unexecuted until a separate grant.
#![allow(clippy::unwrap_used, clippy::indexing_slicing)]
#[path = "../examples/support/storage_metadata_acceptance.rs"]
mod acceptance;
#[path = "../examples/support/storage_metadata_capture.rs"]
mod capture;
use serde_json::json;

#[test]
fn closed_canonical_receipts_refuse_duplicate_and_extra_keys() {
    assert!(acceptance::canonical(b"{\"a\":1,\"a\":1}").is_err());
    assert!(acceptance::canonical(b" {\"a\":1}").is_err());
    let value = acceptance::canonical(b"{\"a\":1}").unwrap();
    assert!(acceptance::keys(&value, &["a"]).is_ok());
    assert!(acceptance::keys(&value, &["b"]).is_err());
    assert!(acceptance::keys(&json!({"a":1,"b":2}), &["a"]).is_err());
}
#[test]
fn unfinished_families_cannot_claim_success() {
    for stage in ["public", "entry", "mutants", "factory", "", "unknown"] {
        assert!(acceptance::supported(stage).is_err());
    }
    assert!(acceptance::supported("validate-inputs").is_ok());
    assert!(acceptance::supported("capabilities").is_ok());
}
#[test]
fn caps_preserve_partial_streams_before_refusal() {
    let mut bytes = vec![1, 2];
    assert!(capture::retain(&mut bytes, &[3, 4, 5], 4).is_err());
    assert_eq!(bytes, vec![1, 2, 3, 4]);
    assert!(capture::retain(&mut bytes, &[], 4).is_ok());
    assert!(capture::retain(&mut bytes, &[6], 4).is_err());
    assert_eq!(bytes.len(), 4);
}
#[test]
fn eof_status_and_cleanup_are_all_required() {
    let mut evidence = capture::Evidence {
        spawned: true,
        stdout_eof: true,
        stderr_eof: true,
        raw_status: Some(0),
        code: Some(0),
        group_signal: Some(Ok(())),
        ..Default::default()
    };
    assert!(evidence.completed());
    evidence.raw_status = None;
    assert!(!evidence.completed());
    evidence.raw_status = Some(0);
    evidence.stdout_eof = false;
    assert!(!evidence.completed());
    evidence.stdout_eof = true;
    evidence.cleanup.push("signal errno".into());
    assert!(!evidence.completed());
    evidence.cleanup.clear();
    evidence.failure = Some("deadline".into());
    assert!(!evidence.completed());
}
#[test]
fn probes_are_children_but_never_serving_children() {
    assert!(acceptance::accounting(2, 0, &["probe", "probe"]).is_ok());
    assert!(acceptance::accounting(0, 0, &["probe"]).is_err());
    assert!(acceptance::accounting(2, 2, &["probe", "probe", "serving", "serving"]).is_ok());
    assert!(acceptance::accounting(1, 0, &["unexpected"]).is_err());
}
#[test]
fn wrong_job_and_source_refuse_before_any_file_or_child() {
    assert!(acceptance::run(Vec::new()).is_err());
    for job in ["wrong", "storage-metadata-admission"] {
        assert!(acceptance::admit(&json!({"schema":1,"job":job,"rs":"wrong","core":acceptance::CORE,"supervisor":acceptance::SUPERVISOR,"sources":[],"artifacts":{}})).is_err());
    }
    for path in ["relative", "/tmp/../outside", "/tmp/control\n"] {
        assert!(acceptance::absolute(path).is_err());
    }
}
#[test]
fn selected_profile_and_features_are_exact() {
    let baseline = json!({"reason":"compiler-artifact","target":{"name":"noevia-server","kind":["bin"]},"profile":{"opt_level":"0","debuginfo":2,"debug_assertions":true,"overflow_checks":true,"test":false},"fresh":false,"features":["default","rust-storage-reads"]});
    assert!(acceptance::ordinary_profile(&baseline, true).is_ok());
    assert!(acceptance::ordinary_profile(&baseline, false).is_err());
    let mut wrong = baseline.clone();
    wrong["profile"]["test"] = json!(true);
    assert!(acceptance::ordinary_profile(&wrong, true).is_err());
    wrong = baseline.clone();
    wrong["features"] = json!(["default", "rust-storage-reads", "rust-storage-reads"]);
    assert!(acceptance::ordinary_profile(&wrong, true).is_err());
    wrong = baseline.clone();
    wrong["fresh"] = json!(null);
    assert!(acceptance::ordinary_profile(&wrong, true).is_err());
    wrong = baseline.clone();
    wrong["target"]["name"] = json!("fixture");
    assert!(acceptance::ordinary_profile(&wrong, true).is_err());
    for (key, value) in [
        ("opt_level", json!("3")),
        ("debuginfo", json!(0)),
        ("debug_assertions", json!(false)),
        ("overflow_checks", json!(false)),
    ] {
        wrong = baseline.clone();
        wrong["profile"][key] = value;
        assert!(acceptance::ordinary_profile(&wrong, true).is_err());
    }
    wrong = baseline;
    wrong["profile"]["extra"] = json!(true);
    assert!(acceptance::ordinary_profile(&wrong, true).is_err());
}
#[test]
fn cargo_identity_is_bound_to_the_approved_workspace() {
    let paths = std::collections::BTreeSet::from([
        "/approved/rs/bins/noevia-server/src/main.rs".to_owned(),
        "/approved/rs/bins/noevia-server/Cargo.toml".to_owned(),
    ]);
    let baseline = json!({"target":{"src_path":"/approved/rs/bins/noevia-server/src/main.rs"},"manifest_path":"/approved/rs/bins/noevia-server/Cargo.toml","package_id":"path+file:///approved/rs/bins/noevia-server#0.1.0"});
    assert!(acceptance::workspace_binding(&baseline, &paths).is_ok());
    for package in [
        "path+file:///other/rs/bins/noevia-server#0.1.0",
        "path+file:///approved/rs/bins/noevia-server#noevia-server@0.1.0",
        "registry+https://example.invalid#noevia-server@0.1.0",
    ] {
        let mut wrong = baseline.clone();
        wrong["package_id"] = json!(package);
        assert!(acceptance::workspace_binding(&wrong, &paths).is_err());
    }
    let mut wrong = baseline.clone();
    wrong["manifest_path"] = json!("/other/rs/bins/noevia-server/Cargo.toml");
    assert!(acceptance::workspace_binding(&wrong, &paths).is_err());
    wrong = baseline.clone();
    wrong["target"]["src_path"] = json!("/other/rs/bins/noevia-server/src/main.rs");
    assert!(acceptance::workspace_binding(&wrong, &paths).is_err());
    assert!(acceptance::workspace_binding(&baseline, &std::collections::BTreeSet::new()).is_err());
}
