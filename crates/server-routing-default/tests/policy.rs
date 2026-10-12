#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]
//! Routing policy over parsed preferences, and the bounded tenant reader's fail-closed cases.
use proptest::prelude::*;
use server_routing_default::{routing_of, Fault, Root, Routing, BODY_LIMIT, FILE};
use server_routing_mode::input::Json;
use std::{
    os::unix::fs::symlink,
    path::Path,
    sync::atomic::{AtomicBool, Ordering},
};

const A: &str = "aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa";
const B: &str = "bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb";

fn routing_text(text: &str) -> Routing {
    routing_of(Json::parse(text.as_bytes()).ok().as_ref())
}

fn tenant(root: &Path, id: &str) -> std::path::PathBuf {
    let d = root.join("users").join(id);
    std::fs::create_dir_all(&d).unwrap();
    d
}

#[test]
fn policy_selects_manual_only_for_the_exact_string() {
    assert_eq!(routing_of(None), Routing::Auto);
    assert_eq!(
        routing_text(r#"{"defaultRouting":"manual"}"#),
        Routing::Manual
    );
    for text in [
        r#"{"defaultRouting":"auto"}"#,
        r#"{"defaultRouting":"MANUAL"}"#,
        r#"{"defaultRouting":" manual"}"#,
        r#"{"defaultRouting":"manual\u0000"}"#,
        r#"{"defaultRouting":true}"#,
        r#"{"defaultRouting":{"manual":true}}"#,
        r#"{"defaultRouting":["manual"]}"#,
        r#"{"defaultroutingmode":"manual"}"#,
        r#"[{"defaultRouting":"manual"}]"#,
        r#""manual""#,
        "null",
    ] {
        assert_eq!(routing_text(text), Routing::Auto, "{text}");
    }
    assert_eq!(Routing::Manual.as_str(), "manual");
    assert_eq!(Routing::Auto.as_str(), "auto");
}

proptest! {
    /// Last duplicate wins, exactly as JSON.parse keeps the last value.
    #[test]
    fn last_duplicate_decides(values in proptest::collection::vec(prop_oneof![
        Just("manual"), Just("auto"), Just("other")
    ], 1..6)) {
        let body: Vec<String> = values.iter().map(|v| format!(r#""defaultRouting":"{v}""#)).collect();
        let text = format!("{{{}}}", body.join(","));
        let expected = if values.last() == Some(&"manual") { Routing::Manual } else { Routing::Auto };
        prop_assert_eq!(routing_text(&text), expected);
    }
}

#[test]
fn tenant_reads_only_their_own_file() {
    let d = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(d.path().join("users").join(A)).unwrap();
    std::fs::write(
        d.path().join("users").join(A).join(FILE),
        r#"{"defaultRouting":"manual"}"#,
    )
    .unwrap();
    let c = AtomicBool::new(false);
    let root = Root::open(d.path()).unwrap();
    assert_eq!(root.read(A, &c).unwrap(), Routing::Manual);
    assert_eq!(root.read(B, &c).unwrap(), Routing::Auto);
    // Upper-case hex is the same tenant on Node (/^[0-9a-f-]{36}$/i), and ids naming
    // anything else never reach the filesystem.
    assert_eq!(
        root.read(&A.to_ascii_uppercase(), &c).unwrap(),
        Routing::Manual
    );
    for id in [
        "../aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa",
        "",
        "aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaa/",
        "users",
        "a/../../x",
    ] {
        assert_eq!(root.read(id, &c), Err(Fault::Unavailable), "{id:?}");
    }
}

#[test]
fn a_get_creates_nothing() {
    let d = tempfile::tempdir().unwrap();
    let c = AtomicBool::new(false);
    assert_eq!(
        Root::open(d.path()).unwrap().read(A, &c).unwrap(),
        Routing::Auto
    );
    assert!(!d.path().join("users").exists());
}

#[test]
fn missing_and_malformed_files_are_auto() {
    let d = tempfile::tempdir().unwrap();
    let c = AtomicBool::new(false);
    let root = Root::open(d.path()).unwrap();
    tenant(d.path(), A);
    assert_eq!(root.read(A, &c).unwrap(), Routing::Auto);
    std::fs::write(
        d.path().join("users").join(A).join(FILE),
        b"{\"defaultRouting\":",
    )
    .unwrap();
    assert_eq!(root.read(A, &c).unwrap(), Routing::Auto);
    std::fs::write(d.path().join("users").join(A).join(FILE), b"\xff\xfe{").unwrap();
    assert_eq!(root.read(A, &c).unwrap(), Routing::Auto);
}

#[test]
fn hostile_files_fail_closed() {
    let c = AtomicBool::new(false);
    let manual = br#"{"defaultRouting":"manual"}"#;

    // Symlinked tenant tree and file: never followed.
    let d = tempfile::tempdir().unwrap();
    let external = tempfile::tempdir().unwrap();
    std::fs::write(external.path().join(FILE), manual).unwrap();
    symlink(external.path(), d.path().join("users")).unwrap();
    assert_eq!(
        Root::open(d.path()).unwrap().read(A, &c),
        Err(Fault::Unavailable)
    );
    std::fs::remove_file(d.path().join("users")).unwrap();
    let t = tenant(d.path(), A);
    symlink(external.path().join(FILE), t.join(FILE)).unwrap();
    assert_eq!(
        Root::open(d.path()).unwrap().read(A, &c),
        Err(Fault::Unavailable)
    );
    std::fs::remove_file(t.join(FILE)).unwrap();

    // A second hard link to the file: refused, as a shared inode may change under the reader.
    std::fs::write(t.join(FILE), manual).unwrap();
    std::fs::hard_link(t.join(FILE), external.path().join("link.json")).unwrap();
    assert_eq!(
        Root::open(d.path()).unwrap().read(A, &c),
        Err(Fault::Unavailable)
    );
    std::fs::remove_file(external.path().join("link.json")).unwrap();

    // A directory where the file should be.
    std::fs::remove_file(t.join(FILE)).unwrap();
    std::fs::create_dir(t.join(FILE)).unwrap();
    assert_eq!(
        Root::open(d.path()).unwrap().read(A, &c),
        Err(Fault::Unavailable)
    );
    std::fs::remove_dir(t.join(FILE)).unwrap();

    // Over the byte cap: refused, never truncated into a different answer.
    let mut big = manual.to_vec();
    big.extend(std::iter::repeat_n(b' ', BODY_LIMIT + 1 - manual.len()));
    std::fs::write(t.join(FILE), &big).unwrap();
    assert_eq!(
        Root::open(d.path()).unwrap().read(A, &c),
        Err(Fault::Unavailable)
    );

    // Exactly at the cap still answers.
    let pad = BODY_LIMIT - manual.len();
    let mut at_cap = manual.to_vec();
    at_cap.splice(
        manual.len() - 1..manual.len() - 1,
        std::iter::repeat_n(b' ', pad),
    );
    assert_eq!(at_cap.len(), BODY_LIMIT);
    std::fs::write(t.join(FILE), &at_cap).unwrap();
    assert_eq!(
        Root::open(d.path()).unwrap().read(A, &c).unwrap(),
        Routing::Manual
    );

    // Nesting Node parses but serde refuses: fail closed, not auto.
    let deep = format!(
        r#"{{"defaultRouting":"manual","x":{}{}}}"#,
        "[".repeat(200),
        "]".repeat(200)
    );
    std::fs::write(t.join(FILE), deep).unwrap();
    assert_eq!(
        Root::open(d.path()).unwrap().read(A, &c),
        Err(Fault::Unavailable)
    );

    // A FIFO must not block the request.
    std::fs::remove_file(t.join(FILE)).unwrap();
    let fifo = t.join(FILE);
    let status = std::process::Command::new("mkfifo")
        .arg(&fifo)
        .status()
        .unwrap();
    assert!(status.success());
    assert_eq!(
        Root::open(d.path()).unwrap().read(A, &c),
        Err(Fault::Unavailable)
    );
}

#[test]
fn cancellation_before_or_after_the_read_is_reported() {
    let d = tempfile::tempdir().unwrap();
    let t = tenant(d.path(), A);
    std::fs::write(t.join(FILE), r#"{"defaultRouting":"manual"}"#).unwrap();
    let root = Root::open(d.path()).unwrap();
    let cancel = AtomicBool::new(true);
    assert_eq!(root.read(A, &cancel), Err(Fault::Cancelled));
    cancel.store(false, Ordering::Release);
    assert_eq!(root.read(A, &cancel).unwrap(), Routing::Manual);
}
