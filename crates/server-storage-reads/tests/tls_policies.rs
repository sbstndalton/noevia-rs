#[path = "support/tls_lab.rs"]
mod lab;
use server_storage_reads::{
    protocol::TransportFailure,
    response_ipc::{Phase, Reply},
    TEXT_BYTES,
};
use std::time::Duration;

#[test]
#[allow(clippy::expect_used, clippy::panic)] // Deliberate distinct CONTROL failures.
fn original_identity_policy() {
    let fixture = lab::pki::Fixture::generate().expect("CONTROL fixed PKI");
    lab::run(
        &fixture,
        "trust",
        true,
        false,
        lab::response(1),
        TEXT_BYTES,
        Duration::from_secs(10),
        false,
        false,
        |reply| {
            let refused = match reply {
                Reply::Failure {
                    phase: Phase::Tls,
                    failure: TransportFailure::Tls,
                    ..
                } => true,
                Reply::Http {
                    status: 200,
                    payload,
                    ..
                } if payload == b"x" => false,
                _ => panic!("CONTROL unrelated outcome"),
            };
            assert!(refused, "M4_TLS_IDENTITY");
        },
    );
}

#[test]
#[allow(clippy::expect_used, clippy::panic)] // Deliberate distinct CONTROL failures.
fn handshake_phase_policy() {
    let fixture = lab::pki::Fixture::generate().expect("CONTROL fixed PKI");
    lab::run(
        &fixture,
        "trust",
        true,
        false,
        lab::response(1),
        TEXT_BYTES,
        Duration::from_secs(10),
        false,
        false,
        |reply| {
            let correct = match reply {
                Reply::Failure {
                    phase: Phase::Tls,
                    failure: TransportFailure::Tls,
                    ..
                } => true,
                Reply::Failure {
                    phase: Phase::Headers,
                    failure: TransportFailure::Header,
                    ..
                } => false,
                _ => panic!("CONTROL unrelated outcome"),
            };
            assert!(correct, "M4_TLS_PHASE");
        },
    );
}
