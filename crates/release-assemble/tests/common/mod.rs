//! Shared by the oracle tests: a comparison that cannot run (tool or variable missing) is skipped with one
//! line, except when `RELEASE_ASSEMBLE_ORACLES_REQUIRED=1` (CI sets it), where it fails (noevia#2726 item 2).

/// Call where an oracle comparison is about to be skipped; fails the test in required mode.
pub fn skip(what: &str) {
    let required = std::env::var("RELEASE_ASSEMBLE_ORACLES_REQUIRED").as_deref() == Ok("1");
    assert!(
        !required,
        "{what}: required by RELEASE_ASSEMBLE_ORACLES_REQUIRED=1 but cannot run"
    );
    eprintln!("{what}: skipped");
}
