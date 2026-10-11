#[path = "../examples/support/mutant_classifier.rs"]
mod classifier;
fn output() -> String {
    "\nrunning 1 test\ntest selected ... FAILED\n\nfailures:\n\n---- selected stdout ----\n\nthread 'selected' (123) panicked at tests/owned_controls.rs:1:1:\nPOLICY\nnote: run with `RUST_BACKTRACE=1` environment variable to display a backtrace\n\n\nfailures:\n    selected\n\ntest result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; 3 filtered out; finished in 0.01s\n\n".into()
}
#[test]
fn exact_classifier_negative_controls() {
    let policy = classifier::Policy::bind("assert!(false, \"POLICY\");", "selected", "POLICY")
        .unwrap_or_else(|_| unreachable!());
    let good = output();
    assert!(policy.accepts(Some(101), good.as_bytes(), b""));
    for (old, new) in [
        ("POLICY\n", "Setup\n"),
        ("POLICY\n", "Deadline\n"),
        ("POLICY\n", "Output\n"),
        ("POLICY\n", "Protocol\n"),
        ("POLICY\n", "OTHER_POLICY\n"),
        ("test selected ...", "test other ..."),
        ("---- selected stdout ----", "---- other stdout ----"),
        ("rs:1:1:", "rs:2:1:"),
        ("    selected\n", "    other\n"),
        ("running 1 test", "running 2 tests"),
        ("0 passed; 1 failed", "0 passed; 2 failed"),
    ] {
        assert!(!policy.accepts(Some(101), good.replace(old, new).as_bytes(), b""));
    }
    assert!(!policy.accepts(Some(1), good.as_bytes(), b""));
    assert!(!policy.accepts(None, good.as_bytes(), b""));
    assert!(!policy.accepts(
        Some(101),
        good.as_bytes(),
        b"error[E0123]: compiler failure"
    ));
    assert!(!policy.accepts(
        Some(101),
        format!("{good}thread 'other' (4) panicked at other:3:1:\nextra\n").as_bytes(),
        b""
    ));
    assert!(!policy.accepts(
        Some(101),
        good.as_bytes(),
        b"thread 'other' panicked at other:1:1"
    ));
    assert!(classifier::Policy::bind(
        "assert!(false, \"POLICY\");\nassert!(true, \"POLICY\");",
        "selected",
        "POLICY"
    )
    .is_err());
}
