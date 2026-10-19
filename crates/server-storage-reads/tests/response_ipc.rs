use server_storage_reads::{
    admission::Admission,
    isolated,
    protocol::{Method, Request, TransportFailure},
    request_ipc,
    response_ipc::{self, Frame, Phase, Reply},
    TEXT_BYTES,
};
use std::{process::Command, sync::atomic::AtomicBool, time::Duration};
#[test]
fn bounded_response_fragmentation_and_rejections() {
    for status in [200, 204, 207, 301, 500, 502] {
        let bytes = response_ipc::encode(
            Reply::Http {
                status,
                capped: false,
                payload: b"",
            },
            32,
        )
        .unwrap_or_else(|_| unreachable!());
        for cut in 0..=bytes.len() {
            let mut frame = Frame::new(32).unwrap_or_else(|_| unreachable!());
            assert!(frame.feed(bytes.get(..cut).unwrap_or_default()).is_ok());
            assert!(frame.feed(bytes.get(cut..).unwrap_or_default()).is_ok());
            assert!(matches!(frame.finish(),Ok(Reply::Http{status:s,..})if s==status));
        }
        for length in 0..bytes.len() {
            assert!(response_ipc::decode(bytes.get(..length).unwrap_or_default(), 32).is_err());
        }
        let mut extra = bytes.clone();
        extra.push(0);
        assert!(response_ipc::decode(&extra, 32).is_err());
    }
    for (phase, failure) in [
        (Phase::Dns, TransportFailure::Dns),
        (Phase::Tls, TransportFailure::Tls),
        (Phase::Headers, TransportFailure::PreHeaderReset),
        (Phase::Body, TransportFailure::Body),
        (Phase::Body, TransportFailure::Timeout),
        (Phase::Tcp, TransportFailure::Timeout),
    ] {
        let bytes = response_ipc::encode(Reply::Failure { phase, failure }, 32)
            .unwrap_or_else(|_| unreachable!());
        assert!(
            matches!(response_ipc::decode(&bytes,32),Ok(Reply::Failure{phase:p,failure:f})if p==phase&&f==failure)
        );
    }
    assert!(response_ipc::encode(
        Reply::Failure {
            phase: Phase::Body,
            failure: TransportFailure::PreHeaderReset
        },
        32
    )
    .is_err());
    // The parent-only projection must not add a child wire code (#2085).
    assert!(response_ipc::encode(
        Reply::Failure {
            phase: Phase::Body,
            failure: TransportFailure::BodyTimeout,
        },
        32,
    )
    .is_err());
    let capped = response_ipc::encode(
        Reply::Http {
            status: 200,
            capped: true,
            payload: &[0; 33],
        },
        32,
    )
    .unwrap_or_else(|_| unreachable!());
    assert!(response_ipc::decode(&capped, 31).is_err());
    for (offset, value) in [(8, 2), (9, 9), (12, 1), (13, 2)] {
        let mut bytes = capped.clone();
        if let Some(byte) = bytes.get_mut(offset) {
            *byte = value;
        }
        assert!(response_ipc::decode(&bytes, 32).is_err());
    }
}
#[test]
fn actual_request_consumption_separate_portable_artifact() {
    for method in [Method::Get, Method::Propfind] {
        let request = Request {
            method,
            url: "http://127.0.0.1/synthetic".into(),
            headers: vec![],
            attempt_timeout: Duration::from_secs(2),
            body_cap: TEXT_BYTES,
        };
        let input = request_ipc::encode(&request).unwrap_or_else(|_| unreachable!());
        let admission = Admission::default();
        let permit = admission
            .try_acquire("synthetic")
            .unwrap_or_else(|_| unreachable!());
        let bytes = isolated::attempt(
            Command::new(env!("CARGO_BIN_EXE_private-request-fixture")),
            input,
            TEXT_BYTES + 19,
            Duration::from_secs(2),
            &AtomicBool::new(false),
            permit,
        )
        .unwrap_or_else(|_| unreachable!());
        assert!(
            matches!(response_ipc::decode(&bytes,TEXT_BYTES),Ok(Reply::Http{status:207,payload,..})if payload==if method==Method::Get{b"synthetic GET".as_slice()}else{b"synthetic PROPFIND".as_slice()})
        );
        assert!(admission.try_acquire("synthetic").is_ok());
    }
}

#[test]
fn response_is_not_committed_before_eof_exit_and_cleanup() {
    for (mode, expected) in [
        ("nonzero", isolated::Error::Exit),
        ("extra", isolated::Error::Protocol),
        ("stall", isolated::Error::Deadline),
    ] {
        let request = Request {
            method: Method::Get,
            url: "http://127.0.0.1/synthetic".into(),
            headers: vec![],
            attempt_timeout: Duration::from_secs(2),
            body_cap: TEXT_BYTES,
        };
        let input = request_ipc::encode(&request).unwrap_or_else(|_| unreachable!());
        let admission = Admission::default();
        let permit = admission
            .try_acquire("synthetic")
            .unwrap_or_else(|_| unreachable!());
        let mut command = Command::new(env!("CARGO_BIN_EXE_private-request-fixture"));
        command.arg(mode);
        assert_eq!(
            isolated::attempt(
                command,
                input,
                TEXT_BYTES + 19,
                Duration::from_millis(200),
                &AtomicBool::new(false),
                permit
            ),
            Err(expected)
        );
        assert!(admission.try_acquire("synthetic").is_ok());
    }
}
