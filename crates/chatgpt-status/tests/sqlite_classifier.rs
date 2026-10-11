#[path = "../examples/support/sqlite_assertions.rs"]
mod assertions;
use assertions::{Anchor, Kind};
const STATEMENT: &str = "assert_eq!(1, 2, \"ROUTING_ASSERT/sqlite/control\");";
const SOURCE: &str =
    "#[test]\nfn selected() {\n    assert_eq!(1, 2, \"ROUTING_ASSERT/sqlite/control\");\n}\n";
fn binding() -> Result<assertions::Binding, String> {
    assertions::bind(
        SOURCE,
        "tests/sqlite.rs",
        "selected",
        &[Anchor {
            statement: STATEMENT,
            payload: "ROUTING_ASSERT/sqlite/control",
            kind: Kind::Equal,
        }],
    )
}
fn captures() -> (String, String) {
    ("running 1 test\ntest selected ... FAILED\n\nfailures:\n    selected\n\ntest result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s\n".into(),"thread 'selected' (42) panicked at tests/sqlite.rs:3:5:\nassertion `left == right` failed: ROUTING_ASSERT/sqlite/control\n  left: 1\n right: 2\nnote: run with `RUST_BACKTRACE=1` environment variable to display a backtrace\nerror: test failed, to rerun pass --test sqlite\n".into())
}
#[test]
fn exact_source_location_payload_and_complete_inventory() -> Result<(), String> {
    let b = binding()?;
    let (out, err) = captures();
    let w = assertions::certify(Some(101), out.as_bytes(), err.as_bytes(), &b, SOURCE)?;
    assert_eq!((w.line, w.column, w.exit), (3, 5, 101));
    let forged=[err.replace("tests/sqlite.rs:3:5","src/sqlite.rs:3:5"),err.replace(":3:5:",":4:5:"),err.replace(":3:5:",":3:6:"),err.replace("thread 'selected'","thread 'other'"),err.replace("ROUTING_ASSERT/sqlite/control","ROUTING_ASSERT/sqlite/wrong"),format!("{err}thread 'extra' (43) panicked at tests/sqlite.rs:3:5:\nROUTING_ASSERT/sqlite/control\n"),err.replace("assertion `left == right` failed: ROUTING_ASSERT/sqlite/control","called `Result::unwrap()` on an `Err` value: ROUTING_ASSERT/sqlite/control"),err.replace("  left: 1","setup failed before selected assertion"),format!("error[E0308]: compile failure\n{err}"),err.replace("assertion `left == right` failed:","assertion `left != right` failed:"),err.replace(" right: 2\n",""),err.replace(" (42)"," (notpid)")];
    for e in forged {
        assert!(assertions::certify(Some(101), out.as_bytes(), e.as_bytes(), &b, SOURCE).is_err());
    }
    for o in [out.replace("running 1 test","running 2 tests"),out.replace("test selected ... FAILED","test other ... FAILED"),out.replace("    selected","    other"),out.replace("    selected","    selected\n    extra"),out.replace("0 passed; 1 failed","1 passed; 1 failed"),out.replace("0 filtered out","9 filtered out"),format!("{out}test result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s\n"),format!("{out}ROUTING_ASSERT/sqlite/control\n")] {assert!(assertions::certify(Some(101),o.as_bytes(),err.as_bytes(),&b,SOURCE).is_err());}
    for exit in [None, Some(0), Some(1), Some(137)] {
        assert!(assertions::certify(exit, out.as_bytes(), err.as_bytes(), &b, SOURCE).is_err());
    }
    assert!(assertions::certify(
        Some(101),
        out.as_bytes(),
        err.as_bytes(),
        &b,
        &SOURCE.replace("1, 2", "1, 3")
    )
    .is_err());
    assert!(assertions::certify(
        Some(101),
        &vec![b'x'; 8 * 1024 * 1024 + 1],
        err.as_bytes(),
        &b,
        SOURCE
    )
    .is_err());
    assert!(assertions::certify(
        Some(101),
        out.as_bytes(),
        &vec![b'x'; 64 * 1024 + 1],
        &b,
        SOURCE
    )
    .is_err());
    assert!(assertions::certify(Some(101), &[255], err.as_bytes(), &b, SOURCE).is_err());
    Ok(())
}
#[test]
fn missing_ambiguous_wrong_owner_and_policy_bindings_refuse() -> Result<(), String> {
    let a = Anchor {
        statement: STATEMENT,
        payload: "ROUTING_ASSERT/sqlite/control",
        kind: Kind::Equal,
    };
    for source in [
        "",
        "#[test]\nfn selected(){}",
        &format!("{SOURCE}{SOURCE}"),
        &SOURCE.replace("fn selected()", "fn other()"),
        &SOURCE.replace("#[test]", ""),
    ] {
        assert!(assertions::bind(source, "tests/sqlite.rs", "selected", &[a]).is_err());
    }
    assert!(assertions::bind(SOURCE, "other.rs", "selected", &[a]).is_err());
    assert!(assertions::bind(SOURCE, "tests/sqlite.rs", "selected", &[]).is_err());
    assert!(assertions::bind(
        SOURCE,
        "tests/sqlite.rs",
        "selected",
        &[Anchor {
            payload: "ROUTING_ASSERT/sqlite/wrong",
            ..a
        }]
    )
    .is_err());
    assert!(assertions::bind(
        SOURCE,
        "tests/sqlite.rs",
        "selected",
        &[Anchor {
            kind: Kind::Boolean,
            ..a
        }]
    )
    .is_err());
    assert!(assertions::bind(SOURCE, "tests/sqlite.rs", "selected", &[a, a]).is_err());
    Ok(())
}
#[test]
fn repeated_payload_requires_explicit_concrete_statement_set() -> Result<(), String> {
    const EQ: &str = "assert_eq!(1, 2, \"ROUTING_ASSERT/sqlite/vm_budget\");";
    const BOOL: &str = "assert!(false, \"ROUTING_ASSERT/sqlite/vm_budget\");";
    let source = format!("#[test]\nfn selected() {{\n    {EQ}\n    {BOOL}\n}}\n");
    let anchors = [
        Anchor {
            statement: EQ,
            payload: "ROUTING_ASSERT/sqlite/vm_budget",
            kind: Kind::Equal,
        },
        Anchor {
            statement: BOOL,
            payload: "ROUTING_ASSERT/sqlite/vm_budget",
            kind: Kind::Boolean,
        },
    ];
    let b = assertions::bind(&source, "tests/sqlite.rs", "selected", &anchors)?;
    let (out, err) = captures();
    let e = err.replace(
        "ROUTING_ASSERT/sqlite/control",
        "ROUTING_ASSERT/sqlite/vm_budget",
    );
    assert!(assertions::certify(Some(101), out.as_bytes(), e.as_bytes(), &b, &source).is_ok());
    let boolean="thread 'selected' (42) panicked at tests/sqlite.rs:4:5:\nROUTING_ASSERT/sqlite/vm_budget\nerror: test failed, to rerun pass --test sqlite\n";
    assert!(
        assertions::certify(Some(101), out.as_bytes(), boolean.as_bytes(), &b, &source).is_ok()
    );
    assert!(assertions::certify(
        Some(101),
        out.as_bytes(),
        boolean.replace(":4:5:", ":3:5:").as_bytes(),
        &b,
        &source
    )
    .is_err());
    let one = assertions::bind(&source, "tests/sqlite.rs", "selected", &anchors[..1])?;
    assert!(
        assertions::certify(Some(101), out.as_bytes(), boolean.as_bytes(), &one, &source).is_err()
    );
    Ok(())
}
#[test]
fn process_deadline_cap_and_extra_setup_cannot_certify() -> Result<(), String> {
    let b = binding()?;
    let (out, err) = captures();
    for prefix in [
        "child deadline exceeded\n",
        "child output limit exceeded\n",
        "setup failure\n",
        "could not compile private source\n",
    ] {
        assert!(assertions::certify(
            Some(101),
            out.as_bytes(),
            format!("{prefix}{err}").as_bytes(),
            &b,
            SOURCE
        )
        .is_err());
    }
    assert!(assertions::certify(
        Some(101),
        format!("unexpected extra test output\n{out}").as_bytes(),
        err.as_bytes(),
        &b,
        SOURCE
    )
    .is_err());
    assert!(assertions::certify(
        Some(101),
        out.as_bytes(),
        err.replace("--test sqlite", "--test unrelated").as_bytes(),
        &b,
        SOURCE
    )
    .is_err());
    Ok(())
}
#[test]
fn rust_nocapture_empty_section_requires_single_final_failure_inventory() -> Result<(), String> {
    let b = binding()?;
    let (out, err) = captures();
    let actual = out.replacen("failures:\n", "failures:\n\nfailures:\n", 1);
    assert!(assertions::certify(Some(101), actual.as_bytes(), err.as_bytes(), &b, SOURCE).is_ok());
    for forged in [
        actual.replacen(
            "failures:\n\nfailures:",
            "failures:\n    extra\nfailures:",
            1,
        ),
        actual.replacen(
            "failures:\n\nfailures:",
            "failures:\n\nfailures:\n\nfailures:",
            1,
        ),
    ] {
        assert!(
            assertions::certify(Some(101), forged.as_bytes(), err.as_bytes(), &b, SOURCE).is_err()
        );
    }
    Ok(())
}

#[test]
fn cargo_footer_follows_assertion_and_terminates_stderr() -> Result<(), String> {
    let b = binding()?;
    let (out, err) = captures();
    let footer = "error: test failed, to rerun pass --test sqlite\n";
    let body = err.strip_suffix(footer).ok_or("control footer")?;
    assert!(assertions::certify(
        Some(101),
        out.as_bytes(),
        format!("{err}\n\n").as_bytes(),
        &b,
        SOURCE
    )
    .is_ok());
    for forged in [
        format!("{err}unexpected trailing record\n"),
        format!("{err}   Compiling trailing setup\n"),
        format!("{err}  left: trailing comparison\n"),
        format!("{err}note: run with `RUST_BACKTRACE=1` trailing note\n"),
        format!("{footer}{body}"),
        body.replacen("  left: 1\n", &format!("{footer}  left: 1\n"), 1),
        body.replacen(
            "assertion `left == right` failed:",
            &format!("{footer}assertion `left == right` failed:"),
            1,
        ),
    ] {
        assert!(
            assertions::certify(Some(101), out.as_bytes(), forged.as_bytes(), &b, SOURCE).is_err()
        );
    }
    Ok(())
}
