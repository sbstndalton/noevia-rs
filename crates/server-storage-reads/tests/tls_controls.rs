#[path = "support/tls_lab.rs"]
mod lab;
use server_storage_reads::{
    protocol::TransportFailure,
    response_ipc::{Phase, Reply},
    LIST_BYTES, TEXT_BYTES,
};
use std::time::Duration;
#[test]
fn versions_original_identity_and_capped_positive() {
    let f = lab::pki::Fixture::generate().unwrap_or_else(|_| unreachable!());
    for (mode, dns) in [("tls12", false), ("tls13", false), ("dns", true)] {
        let r = lab::run(
            &f,
            mode,
            dns,
            dns,
            lab::response(4097),
            TEXT_BYTES,
            Duration::from_secs(15),
            false,
            false,
            |r| {
                assert!(
                    matches!(r,Reply::Http{status:200,capped:false,payload}if payload.len()==4097)
                )
            },
        );
        assert!(r.accepted && r.identity);
        let input = std::str::from_utf8(&r.input).unwrap_or_else(|_| unreachable!());
        assert!(input.starts_with(if dns {
            "PROPFIND /signed%2Fpath?X=a%2Fb HTTP/1.1"
        } else {
            "GET /signed%2Fpath?X=a%2Fb HTTP/1.1"
        }));
        assert!(input.contains("host: different.synthetic.invalid"));
    }
    for cap in [TEXT_BYTES, LIST_BYTES] {
        for size in [cap, cap + 1] {
            let r = lab::run(
                &f,
                "trust",
                false,
                false,
                lab::response(size),
                cap,
                Duration::from_secs(15),
                false,
                false,
                |r| {
                    assert!(
                        matches!(r,Reply::Http{capped,payload,..}if *capped==(size>cap)&&payload.len()==size)
                    )
                },
            );
            assert!(r.accepted && r.identity);
        }
    }
    println!("synthetic TLS1.2/1.3/IP noSNI/original localhostSNI/fullcap controls passed");
}
#[test]
fn trust_san_expiry_handshake_and_body_refusals() {
    let f = lab::pki::Fixture::generate().unwrap_or_else(|_| unreachable!());
    for (mode, dns_cert, dns_url) in [
        ("public", false, false),
        ("expired", false, false),
        ("trust", true, false),
        ("dns", false, true),
    ] {
        let r = lab::run(
            &f,
            mode,
            dns_cert,
            dns_url,
            lab::response(0),
            TEXT_BYTES,
            Duration::from_secs(15),
            false,
            false,
            |r| {
                assert!(matches!(
                    r,
                    Reply::Failure {
                        phase: Phase::Tls,
                        failure: TransportFailure::Tls
                    }
                ))
            },
        );
        assert!(r.input.is_empty());
    }
    let r = lab::run(
        &f,
        "trust",
        false,
        false,
        vec![],
        TEXT_BYTES,
        Duration::from_millis(250),
        false,
        true,
        |r| {
            assert!(matches!(
                r,
                Reply::Failure {
                    phase: Phase::Tls,
                    failure: TransportFailure::Timeout
                }
            ))
        },
    );
    assert!(r.input.is_empty());
    lab::run(
        &f,
        "trust",
        false,
        false,
        b"HTTP/1.1 200 Fixed\r\nContent-Length: 5\r\n\r\na".to_vec(),
        TEXT_BYTES,
        Duration::from_secs(15),
        false,
        false,
        |r| {
            assert!(matches!(
                r,
                Reply::Failure {
                    phase: Phase::Body,
                    failure: TransportFailure::Body
                }
            ))
        },
    );
    println!("publicroots/untrusted/mismatchedSAN/fixtureclockexpiry/handshake timeout/bodytruncation controls passed");
}
#[test]
fn tls_body_cancel_retains_reap_admission() {
    let f = lab::pki::Fixture::generate().unwrap_or_else(|_| unreachable!());
    let r = lab::run(
        &f,
        "trust",
        false,
        false,
        lab::response(LIST_BYTES),
        LIST_BYTES,
        Duration::from_secs(15),
        true,
        false,
        |_| {},
    );
    assert!(r.accepted && r.identity);
    println!("TLS body cancel held lease through cleanup");
}
