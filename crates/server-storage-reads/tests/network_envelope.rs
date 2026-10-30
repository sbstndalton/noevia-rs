use server_storage_reads::{
    admission::Admission,
    isolated::{self, Error, NetworkObservation},
    protocol::{Method, Request},
    response_ipc::Reply,
    LIST_BYTES, TEXT_BYTES,
};
use std::{
    process::Command,
    sync::atomic::{AtomicBool, Ordering},
    time::Duration,
};
fn request(cap: usize) -> Request {
    Request {
        method: Method::Get,
        url: "http://127.0.0.1/synthetic".into(),
        headers: vec![],
        attempt_timeout: Duration::from_secs(15),
        body_cap: cap,
        body: Default::default(),
    }
}
fn command(mode: &str) -> Command {
    let mut c = Command::new(env!("CARGO_BIN_EXE_private-network-fixture"));
    c.arg(mode);
    c
}
#[test]
fn complete_full_caps_and_nonempty_boundaries() {
    for cap in [TEXT_BYTES, LIST_BYTES] {
        for mode in ["full", "lookahead", "boundary"] {
            let admission = Admission::default();
            let permit = admission
                .try_acquire("owner")
                .unwrap_or_else(|_| unreachable!());
            let reply = isolated::attempt_http(
                command(mode),
                request(cap),
                Duration::from_secs(8),
                &AtomicBool::new(false),
                permit,
            )
            .unwrap_or_else(|_| unreachable!());
            assert!(
                matches!(reply.view(),Ok(Reply::Http{status:200,capped,payload})if capped==(mode=="lookahead")&&payload.len()==match mode{"full"=>cap,"lookahead"=>cap+1,_=>4097})
            );
            assert!(admission.try_acquire("owner").is_ok());
        }
    }
}
#[test]
fn tag_metadata_exit_and_legacy_compatibility() {
    for (mode, expected) in [
        ("legacy", Error::Protocol),
        ("bad-tag", Error::Protocol),
        ("bad-phase", Error::Protocol),
        ("nonzero", Error::Exit),
        ("extra", Error::Protocol),
        ("stall", Error::Deadline),
    ] {
        let admission = Admission::default();
        let permit = admission
            .try_acquire("owner")
            .unwrap_or_else(|_| unreachable!());
        let result = isolated::attempt_http(
            command(mode),
            request(TEXT_BYTES),
            Duration::from_secs(2),
            &AtomicBool::new(false),
            permit,
        );
        assert!(matches!(result,Err(error)if error==expected));
        assert!(admission.try_acquire("owner").is_ok());
    }
    let admission = Admission::default();
    let permit = admission
        .try_acquire("owner")
        .unwrap_or_else(|_| unreachable!());
    let input = server_storage_reads::request_ipc::encode(&request(TEXT_BYTES))
        .unwrap_or_else(|_| unreachable!());
    assert!(matches!(
        isolated::attempt(
            command("boundary"),
            input,
            TEXT_BYTES,
            Duration::from_secs(2),
            &AtomicBool::new(false),
            permit
        ),
        Err(Error::Protocol)
    ));
    let permit = admission
        .try_acquire("owner")
        .unwrap_or_else(|_| unreachable!());
    let mut old = Command::new(env!("CARGO_BIN_EXE_private-child-proof"));
    old.arg("fixture-echo");
    assert_eq!(
        isolated::attempt(
            old,
            zeroize::Zeroizing::new(b"old BODY".to_vec()),
            8,
            Duration::from_secs(2),
            &AtomicBool::new(false),
            permit
        ),
        Ok(b"old BODY".to_vec())
    );
}
#[test]
fn cancellation_inside_payload_keeps_admission() {
    let admission = Admission::default();
    let permit = admission
        .try_acquire("owner")
        .unwrap_or_else(|_| unreachable!());
    let cancelled = AtomicBool::new(false);
    let mut payload_seen = false;
    let mut cleanup_seen = false;
    let result = isolated::attempt_http_observed(
        command("full"),
        request(LIST_BYTES),
        Duration::from_secs(8),
        &cancelled,
        permit,
        |stage, _| {
            assert!(admission.try_acquire("owner").is_err());
            match stage {
                NetworkObservation::OutputProgress(bytes) if bytes > 38 => {
                    payload_seen = true;
                    cancelled.store(true, Ordering::Release);
                }
                NetworkObservation::BeforeReap => cleanup_seen = true,
                _ => {}
            }
        },
    );
    assert!(matches!(result, Err(Error::Cancelled)));
    assert!(payload_seen && cleanup_seen);
    assert!(admission.try_acquire("owner").is_ok());
}
