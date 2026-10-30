//! Selected synthetic measurement, not retroactive baseline diagnosis.
use server_storage_reads::{
    admission::Admission,
    isolated::{self, Error, NetworkObservation},
    protocol::{Method, Request},
    TEXT_BYTES,
};
use std::{
    process::Command,
    sync::{atomic::AtomicBool, Barrier},
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
fn command(binary: &str, mode: &str) -> Command {
    let mut c = Command::new(binary);
    c.arg(mode);
    c
}
fn measured(a: &Admission, mode: &str, old: bool) -> Result<(), Error> {
    let p = a.try_acquire("measured").unwrap_or_else(|_| unreachable!());
    let start = Instant::now();
    let mut events = 0u32;
    let mut bytes = 0usize;
    let mut prereap = None;
    let binary = if old {
        env!("CARGO_BIN_EXE_private-child-proof")
    } else {
        env!("CARGO_BIN_EXE_private-network-fixture")
    };
    let r = isolated::attempt_http_observed(
        command(binary, mode),
        request(),
        Duration::from_secs(2),
        &AtomicBool::new(false),
        p,
        |s, _| match s {
            NetworkObservation::OutputProgress(n) => {
                events = events.saturating_add(1);
                bytes = bytes.max(n)
            }
            NetworkObservation::BeforeReap => prereap = Some(start.elapsed().as_millis()),
        },
    );
    let result = r.map(|_| ());
    println!("fixed_mode={mode} result={result:?} output_events={events} output_bytes={bytes} prereap_ms={prereap:?}");
    assert!(prereap.is_some(), "CONTROL cleanup observed");
    assert!(
        a.try_acquire("measured").is_ok(),
        "CONTROL admission released"
    );
    result
}
#[test]
fn metadata_with_owned_competitor_and_ready_paths() {
    let a = Admission::default();
    let p = a
        .try_acquire("competitor")
        .unwrap_or_else(|_| unreachable!());
    let barrier = Barrier::new(2);
    std::thread::scope(|scope| {
        let worker = scope.spawn(|| {
            barrier.wait();
            let r = isolated::attempt_http(
                command(
                    env!("CARGO_BIN_EXE_private-network-fixture"),
                    "lookahead-stall",
                ),
                request(),
                Duration::from_secs(2),
                &AtomicBool::new(false),
                p,
            );
            assert!(
                matches!(r, Err(Error::Deadline)),
                "CONTROL competitor deadline"
            );
        });
        barrier.wait();
        assert_eq!(measured(&a, "bad-flags", false), Err(Error::Protocol));
        worker.join().unwrap_or_else(|_| unreachable!());
    });
    assert_eq!(measured(&a, "fixture-no-ready", true), Err(Error::Deadline));
    assert_eq!(
        measured(&a, "fixture-bad-ready", true),
        Err(Error::Protocol)
    );
}
