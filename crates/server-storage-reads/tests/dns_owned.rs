#[allow(clippy::panic, clippy::expect_used)]
#[path = "support/dns_lab.rs"]
mod lab;
// Source inclusion accesses only the crate-private closed testing seam.
use server_storage_reads::{isolated, protocol, response_ipc, LIST_BYTES, TEXT_BYTES};
#[allow(dead_code)]
#[path = "../src/child_http.rs"]
mod actual_transport;
use std::{
    net::{Ipv6Addr, SocketAddr},
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};
fn trust() -> Arc<rustls::ClientConfig> {
    Arc::new(
        rustls::ClientConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .unwrap_or_else(|_| unreachable!())
        .with_root_certificates(rustls::RootCertStore::empty())
        .with_no_client_auth(),
    )
}
fn request(host: &str) -> protocol::Request {
    let mut request = lab::request(host, 9, Duration::from_secs(1));
    request.url = request.url.replacen("https://", "http://", 1);
    request
}
#[test]
#[allow(clippy::expect_used)]
fn dns_host_caps_aliases_guards_and_literal_no_resolver() {
    let calls = AtomicUsize::new(0);
    let maximum = format!(
        "{}.{}.{}.{}",
        "a".repeat(63),
        "b".repeat(63),
        "c".repeat(63),
        "d".repeat(61)
    );
    assert_eq!(maximum.len(), 253);
    let over = format!("{maximum}e");
    for (host, expected) in [
        (maximum.as_str(), true),
        (over.as_str(), false),
        ("127.1", false),
        ("0x7f000001", false),
        ("local%68ost", false),
        ("localhost.", true),
        ("LOCALHOST", true),
        ("127.0.0.1", false),
        ("[::1]", false),
    ] {
        let before = calls.load(Ordering::Acquire);
        let attempt = actual_transport::prepare_dns_inner(request(host), trust(), |name, port| {
            assert!(tokio::runtime::Handle::try_current().is_err());
            assert!(name.len() <= 253 && name.is_ascii());
            assert_eq!(port, 9);
            calls.fetch_add(1, Ordering::Release);
            Ok(std::iter::empty::<SocketAddr>())
        })
        .expect("CONTROL prepare");
        if host == "127.0.0.1" || host == "[::1]" {
            assert_eq!(
                calls.load(Ordering::Acquire),
                before,
                "CONTROL literal bypasses resolver"
            );
            drop(attempt); // Pure preparation; actual dialing uses owned fixture listeners below.
            continue;
        }
        let bytes = actual_transport::runtime()
            .expect("CONTROL runtime")
            .block_on(actual_transport::execute_owned_dns(attempt))
            .expect("CONTROL frame");
        assert_eq!(
            calls.load(Ordering::Acquire) - before,
            usize::from(expected),
            "CONTROL resolver guard {host}"
        );
        if host == over || host == "127.1" || host == "0x7f000001" {
            assert!(
                matches!(
                    response_ipc::decode(&bytes, TEXT_BYTES).expect("CONTROL NET"),
                    response_ipc::Reply::Failure {
                        phase: response_ipc::Phase::Dns,
                        failure: protocol::TransportFailure::Dns
                    }
                ),
                "CONTROL Dns restriction {host}"
            );
        }
        if host == "local%68ost" {
            assert!(
                matches!(
                    response_ipc::decode(&bytes, TEXT_BYTES).expect("CONTROL NET"),
                    response_ipc::Reply::Failure {
                        phase: response_ipc::Phase::Headers,
                        failure: protocol::TransportFailure::Header
                    }
                ),
                "CONTROL malformed authority"
            );
        }
    }
    for name in ["bad name".to_owned(), "a".repeat(129)] {
        let mut invalid = request("localhost");
        invalid
            .headers
            .push((name, zeroize::Zeroizing::new("x".into())));
        let before = calls.load(Ordering::Acquire);
        let _ = actual_transport::prepare_dns_inner(invalid, trust(), |_, _| {
            calls.fetch_add(1, Ordering::Release);
            Ok(std::iter::empty::<SocketAddr>())
        })
        .expect("CONTROL header refusal");
        assert_eq!(calls.load(Ordering::Acquire), before);
    }
    let mut invalid = request("localhost");
    invalid.url = "x".repeat(65537);
    assert!(matches!(
        actual_transport::prepare_dns_inner(invalid, trust(), |_, _| Ok(std::iter::empty::<
            SocketAddr,
        >())),
        Err(isolated::Error::Input)
    ));
}
#[test]
#[allow(clippy::expect_used)]
fn dns_success_limits_failures_and_ipv6() {
    let fixture = lab::pki::Fixture::generate().expect("CONTROL fixed PKI");
    for (mode, v6) in [
        ("v4", false),
        ("v6", true),
        ("32", false),
        ("literal", false),
        ("literal", true),
        ("v4-v6", true),
        ("v6-v4", false),
    ] {
        let report = lab::run(
            &fixture,
            mode,
            if mode == "literal" {
                if v6 {
                    "[::1]"
                } else {
                    "127.0.0.1"
                }
            } else {
                "localhost"
            },
            lab::listener(v6, 0),
            mode != "literal",
            Duration::from_secs(2),
            Duration::ZERO,
            false,
            |reply| {
                assert!(
                    matches!(reply,response_ipc::Reply::Http{status:200,payload,..} if *payload==b"x")
                )
            },
        );
        assert!(report.tcp && report.tls);
        if mode != "literal" {
            assert!(report.sni);
        }
        assert!(report
            .input
            .starts_with(b"PROPFIND /signed%2Fpath?X=a%2Fb HTTP/1.1\r\n"));
        assert!(report
            .input
            .windows(b"host: different.synthetic.invalid".len())
            .any(|b| b == b"host: different.synthetic.invalid"));
    }
    for mode in ["empty", "error", "public"] {
        let report = lab::run(
            &fixture,
            mode,
            "localhost",
            lab::listener(false, 0),
            true,
            Duration::from_secs(2),
            Duration::ZERO,
            false,
            |reply| {
                assert!(
                    matches!(reply,response_ipc::Reply::Failure{phase:response_ipc::Phase::Dns,failure:protocol::TransportFailure::Dns} if mode!="public")
                        || matches!(reply,response_ipc::Reply::Failure{phase:response_ipc::Phase::Tls,failure:protocol::TransportFailure::Tls} if mode=="public")
                )
            },
        );
        assert!(report.input.is_empty());
        assert_eq!(report.tcp, mode == "public");
    }
}
#[test]
#[allow(clippy::expect_used)]
fn dns_owned_stall_cancel_and_both_budget_clamps() {
    let fixture = lab::pki::Fixture::generate().expect("CONTROL fixed PKI");
    lab::stalled(
        &fixture,
        true,
        Duration::from_secs(3),
        Duration::from_secs(2),
    );
    lab::stalled(
        &fixture,
        false,
        Duration::from_secs(3),
        Duration::from_millis(350),
    );
    lab::stalled(
        &fixture,
        false,
        Duration::from_millis(350),
        Duration::from_secs(3),
    );
    let admission = server_storage_reads::admission::Admission::default();
    let permit = admission.try_acquire("zero").expect("CONTROL lease");
    let mut command = std::process::Command::new("/not-a-program");
    command.arg("not spawned");
    assert!(matches!(
        isolated::attempt_dns(
            command,
            request("localhost"),
            Duration::ZERO,
            &AtomicBool::new(false),
            permit
        ),
        Err(isolated::Error::Deadline)
    ));
    assert!(admission.try_acquire("zero").is_ok());
}
#[test]
#[allow(clippy::expect_used)]
fn dns_delay_exhaustion_and_thread_identity() {
    let original_thread = std::thread::current().id();
    let mut req = request("localhost");
    req.attempt_timeout = Duration::from_millis(50);
    let start = Instant::now();
    let called = AtomicBool::new(false);
    let attempt = actual_transport::prepare_dns_inner(req, trust(), |_, _| {
        assert_eq!(std::thread::current().id(), original_thread);
        assert!(tokio::runtime::Handle::try_current().is_err());
        called.store(true, Ordering::Release);
        std::thread::sleep(Duration::from_millis(75));
        Ok(std::iter::once(SocketAddr::from((Ipv6Addr::LOCALHOST, 9))))
    })
    .expect("CONTROL prepare");
    let bytes = actual_transport::runtime()
        .expect("CONTROL runtime")
        .block_on(actual_transport::execute_owned_dns(attempt))
        .expect("CONTROL frame");
    assert!(called.load(Ordering::Acquire) && start.elapsed() >= Duration::from_millis(75));
    assert!(matches!(
        response_ipc::decode(&bytes, TEXT_BYTES).expect("CONTROL NET"),
        response_ipc::Reply::Failure {
            phase: response_ipc::Phase::Dns,
            failure: protocol::TransportFailure::Timeout
        }
    ));
}

#[test]
#[allow(clippy::expect_used)]
fn dns_env_clear_and_no_fallback_after_first_tcp_tls_failure() {
    let fixture = lab::pki::Fixture::generate().expect("CONTROL fixed PKI");
    let v4 = lab::listener(false, 0);
    let port = v4.local_addr().expect("CONTROL port").port();
    let v6 = lab::listener(true, port);
    let c4 = fixture
        .server(false)
        .expect("CONTROL wrong-name certificate");
    let c6 = fixture.server(true).expect("CONTROL DNS certificate");
    std::thread::scope(|scope| {
        let s4 = scope.spawn(move || {
            actual_transport::runtime()
                .expect("CONTROL runtime")
                .block_on(lab::serve(v4, c4, b'4', Duration::ZERO))
        });
        let s6 = scope.spawn(move || {
            actual_transport::runtime()
                .expect("CONTROL runtime")
                .block_on(lab::serve(v6, c6, b'6', Duration::ZERO))
        });
        let admission = server_storage_reads::admission::Admission::default();
        let permit = admission.try_acquire("one").expect("CONTROL lease");
        let reply = isolated::attempt_dns(
            lab::command(&fixture, "v4-v6"),
            lab::request("localhost", port, Duration::from_secs(2)),
            Duration::from_secs(3),
            &AtomicBool::new(false),
            permit,
        )
        .expect("CONTROL owner");
        assert!(matches!(
            reply.view().expect("CONTROL NET"),
            response_ipc::Reply::Failure {
                phase: response_ipc::Phase::Tls,
                failure: protocol::TransportFailure::Tls
            }
        ));
        let r4 = s4.join().expect("CONTROL server4");
        let r6 = s6.join().expect("CONTROL server6");
        assert!(r4.tcp && !r4.tls && r4.input.is_empty());
        assert!(!r6.tcp && r6.input.is_empty());
    });
}
