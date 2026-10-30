use server_storage_reads::{
    admission::Admission,
    isolated::{self, Error, NetworkObservation},
    protocol::{Method, Request},
    TEXT_BYTES,
};
use std::{
    process::Command,
    sync::atomic::{AtomicBool, Ordering},
    time::{Duration, Instant},
};
fn request() -> Request {
    Request {
        method: Method::Get,
        url: "http://127.0.0.1/synthetic".into(),
        headers: vec![],
        attempt_timeout: Duration::from_secs(15),
        body_cap: TEXT_BYTES,
        body: Default::default(),
    }
}
fn command(mode: &str) -> Command {
    let mut c = Command::new(env!("CARGO_BIN_EXE_private-network-fixture"));
    c.arg(mode);
    c
}
fn refuse(mode: &str, expected: Error) -> bool {
    let a = Admission::default();
    let p = a.try_acquire("owner").unwrap_or_else(|_| unreachable!());
    let r = isolated::attempt_http(
        command(mode),
        request(),
        Duration::from_secs(2),
        &AtomicBool::new(false),
        p,
    );
    match r {
        Ok(_) => true,
        Err(e) => {
            assert!(e == expected, "CONTROL unexpected failure {e:?}");
            false
        }
    }
}
#[test]
fn tag_policy() {
    let accepted = refuse("legacy", Error::Protocol);
    assert!(!accepted, "M4_NET_TAG");
}
#[test]
fn metadata_policy() {
    let accepted = refuse("bad-flags", Error::Protocol);
    assert!(!accepted, "M4_NET_METADATA");
}
#[test]
fn exit_policy() {
    let accepted = refuse("nonzero", Error::Exit);
    assert!(!accepted, "M4_NET_EXIT");
}
#[test]
fn eof_policy() {
    let a = Admission::default();
    let p = a.try_acquire("owner").unwrap_or_else(|_| unreachable!());
    let start = Instant::now();
    let mut complete = false;
    let mut early = false;
    let r = isolated::attempt_http_observed(
        command("lookahead-stall"),
        request(),
        Duration::from_secs(2),
        &AtomicBool::new(false),
        p,
        |s, _| match s {
            NetworkObservation::OutputProgress(n) => complete = n == TEXT_BYTES + 39,
            NetworkObservation::BeforeReap => {
                early = complete && start.elapsed() < Duration::from_secs(1)
            }
        },
    );
    assert!(!early, "M4_NET_EOF");
    assert!(matches!(r, Err(Error::Deadline)), "CONTROL EOF failure");
}
#[test]
fn cancel_policy() {
    let a = Admission::default();
    let p = a.try_acquire("owner").unwrap_or_else(|_| unreachable!());
    let cancelled = AtomicBool::new(false);
    let mut seen = false;
    let mut cleanup = false;
    let r = isolated::attempt_http_observed(
        command("full"),
        request(),
        Duration::from_secs(2),
        &cancelled,
        p,
        |s, _| match s {
            NetworkObservation::OutputProgress(n) if n > 38 => {
                seen = true;
                cancelled.store(true, Ordering::Release)
            }
            NetworkObservation::BeforeReap => cleanup = true,
            _ => {}
        },
    );
    assert!(seen && cleanup, "CONTROL progress");
    let ignored = matches!(r, Ok(_) | Err(Error::Deadline));
    assert!(!ignored, "M4_NET_CANCEL");
    assert!(matches!(r, Err(Error::Cancelled)), "CONTROL cancel failure");
}
#[test]
fn admission_policy() {
    let a = Admission::default();
    let p = a.try_acquire("owner").unwrap_or_else(|_| unreachable!());
    let cancelled = AtomicBool::new(false);
    let r = isolated::attempt_http_observed(
        command("boundary"),
        request(),
        Duration::from_secs(2),
        &cancelled,
        p,
        |s, _| {
            let held = a.try_acquire("owner").is_err();
            assert!(held, "M4_NET_ADMISSION");
            if matches!(s,NetworkObservation::OutputProgress(n)if n>38) {
                cancelled.store(true, Ordering::Release)
            }
        },
    );
    assert!(
        matches!(r, Err(Error::Cancelled)),
        "CONTROL admission failure"
    );
    assert!(a.try_acquire("owner").is_ok(), "CONTROL release");
}
