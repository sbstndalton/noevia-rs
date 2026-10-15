#[allow(dead_code, clippy::panic, clippy::expect_used)]
#[path = "support/dns_lab.rs"]
mod lab;
use server_storage_reads::{
    protocol::TransportFailure,
    response_ipc::{Phase, Reply},
};
use std::time::Duration;
#[test]
#[allow(clippy::expect_used, clippy::panic)]
fn original_dns_identity_policy() {
    let fixture = lab::pki::Fixture::generate().expect("CONTROL fixed PKI");
    let report = lab::run(
        &fixture,
        "v4",
        "localhost",
        lab::listener(false, 0),
        false,
        Duration::from_secs(2),
        Duration::ZERO,
        false,
        |reply| {
            let correct = match reply {
                Reply::Failure {
                    phase: Phase::Tls,
                    failure: TransportFailure::Tls,
                } => true,
                Reply::Http {
                    status: 200,
                    payload,
                    ..
                } if payload == b"x" => false,
                _ => panic!("CONTROL unrelated identity outcome"),
            };
            assert!(correct, "M4_DNS_IDENTITY");
        },
    );
    assert!(report.tcp && !report.tls && report.input.is_empty());
}
#[test]
#[allow(clippy::expect_used, clippy::panic)]
fn result33_policy() {
    let fixture = lab::pki::Fixture::generate().expect("CONTROL fixed PKI");
    let report = lab::run(
        &fixture,
        "33",
        "localhost",
        lab::listener(false, 0),
        true,
        Duration::from_secs(2),
        Duration::ZERO,
        false,
        |reply| {
            let correct = match reply {
                Reply::Failure {
                    phase: Phase::Dns,
                    failure: TransportFailure::Dns,
                } => true,
                Reply::Http {
                    status: 200,
                    payload,
                    ..
                } if payload == b"x" => false,
                _ => panic!("CONTROL unrelated result33 outcome"),
            };
            assert!(correct, "M4_DNS_RESULT33");
        },
    );
    assert!(!report.tcp && report.input.is_empty());
}
#[allow(clippy::expect_used, clippy::panic)]
fn ordered(v6_first: bool) -> bool {
    let fixture = lab::pki::Fixture::generate().expect("CONTROL fixed PKI");
    let v4 = lab::listener(false, 0);
    let port = v4.local_addr().expect("CONTROL port").port();
    let v6 = lab::listener(true, port);
    let config = fixture.server(true).expect("CONTROL certificate");
    std::thread::scope(|scope| {
        let c4 = std::sync::Arc::clone(&config);
        let s4 = scope.spawn(move || {
            server_storage_reads::child_http::runtime()
                .expect("CONTROL runtime")
                .block_on(lab::serve(v4, c4, b'4', Duration::ZERO))
        });
        let s6 = scope.spawn(move || {
            server_storage_reads::child_http::runtime()
                .expect("CONTROL runtime")
                .block_on(lab::serve(v6, config, b'6', Duration::ZERO))
        });
        let admission = server_storage_reads::admission::Admission::default();
        let permit = admission.try_acquire("order").expect("CONTROL lease");
        let result = server_storage_reads::isolated::attempt_dns(
            lab::command(&fixture, if v6_first { "v6-v4" } else { "v4-v6" }),
            lab::request("localhost", port, Duration::from_secs(2)),
            Duration::from_secs(3),
            &std::sync::atomic::AtomicBool::new(false),
            permit,
        )
        .expect("CONTROL owner");
        let selected = match result.view().expect("CONTROL NET") {
            Reply::Http {
                status: 200,
                payload,
                ..
            } if payload == b"4" => false,
            Reply::Http {
                status: 200,
                payload,
                ..
            } if payload == b"6" => true,
            _ => panic!("CONTROL unrelated order outcome"),
        };
        let r4 = s4.join().expect("CONTROL server4");
        let r6 = s6.join().expect("CONTROL server6");
        assert!(r4.tcp ^ r6.tcp, "CONTROL one TCP success only");
        selected == v6_first
    })
}
#[test]
fn os_order_policy() {
    let correct = ordered(false);
    assert!(correct, "M4_DNS_OS_ORDER");
}
#[test]
fn ipv6_first_policy() {
    assert!(ordered(true), "CONTROL IPv6 first executed");
}
#[test]
#[allow(clippy::expect_used, clippy::panic)]
fn remaining_deadline_policy() {
    let fixture = lab::pki::Fixture::generate().expect("CONTROL fixed PKI");
    let report = lab::run(
        &fixture,
        "delay",
        "localhost",
        lab::listener(false, 0),
        true,
        Duration::from_millis(500),
        Duration::from_millis(400),
        true,
        |reply| {
            let correct = match reply {
                Reply::Failure {
                    phase: Phase::Headers,
                    failure: TransportFailure::Timeout,
                } => true,
                Reply::Http {
                    status: 200,
                    payload,
                    ..
                } if payload == b"x" => false,
                _ => panic!("CONTROL unrelated remaining deadline outcome"),
            };
            assert!(correct, "M4_DNS_DEADLINE");
        },
    );
    assert!(report.tcp && report.tls && report.sni && !report.input.is_empty());
}
