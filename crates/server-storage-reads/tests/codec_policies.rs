#[allow(dead_code)]
#[path = "../src/response_codecs.rs"]
mod codecs;
#[path = "support/codec_lab.rs"]
mod lab;
use server_storage_reads::{
    protocol::{Method, TransportFailure},
    response_ipc::Reply,
    TEXT_BYTES,
};
use std::time::{Duration, Instant};
#[test]
fn reverse_stack_policy() {
    let body = lab::encode("deflate", &lab::encode("gzip", b"stack"));
    lab::run(
        lab::response(200, &[b"gzip,deflate"], &body),
        4096,
        Method::Get,
        TEXT_BYTES,
        |reply| {
            let correct = matches!(reply,Reply::Http{payload,capped:false,..}if payload==b"stack");
            assert!(correct, "M4_CODEC_ORDER");
        },
    );
}
#[test]
fn raw_deflate_policy() {
    let body = lab::encode("raw", b"raw deflate");
    lab::run(
        lab::response(200, &[b"deflate"], &body),
        4096,
        Method::Get,
        TEXT_BYTES,
        |reply| {
            let correct =
                matches!(reply,Reply::Http{payload,capped:false,..}if payload==b"raw deflate");
            assert!(correct, "M4_CODEC_DEFLATE");
        },
    );
}
#[test]
fn decoded_capped_policy() {
    let body = lab::encode("gzip", &vec![b'x'; TEXT_BYTES + 1]);
    lab::run(
        lab::response(200, &[b"gzip"], &body),
        4096,
        Method::Get,
        TEXT_BYTES,
        |reply| {
            let correct = matches!(
                reply,
                Reply::Failure {
                    failure: TransportFailure::Body,
                    ..
                }
            );
            assert!(correct, "M4_CODEC_OVERFLOW_REFUSAL");
        },
    );
}
#[test]
fn whole_unknown_policy() {
    let body = lab::encode("gzip", b"whole unknown");
    lab::run(
        lab::response(200, &[b"gzip,unknown"], &body),
        4096,
        Method::Get,
        TEXT_BYTES,
        |reply| {
            let correct = matches!(reply,Reply::Http{payload,capped:false,..}if payload==body);
            assert!(correct, "M4_CODEC_UNKNOWN");
        },
    );
}
#[test]
fn strict_brotli_and_expired_deadline_baseline() {
    let mut headers = hyper::http::HeaderMap::new();
    headers.insert(
        "content-encoding",
        hyper::http::HeaderValue::from_static("br"),
    );
    let plan = codecs::Plan::headers(&headers).unwrap_or_else(|_| unreachable!());
    assert!(matches!(
        codecs::Pipeline::new(plan, Instant::now() - Duration::from_secs(1)),
        Err(codecs::Fault::Timeout)
    ));
    lab::run(
        lab::response(200, &[b"br"], &[17, 30, 0, 0]),
        1,
        Method::Get,
        TEXT_BYTES,
        |reply| {
            assert!(
                matches!(
                    reply,
                    Reply::Failure {
                        failure: TransportFailure::Body,
                        ..
                    }
                ),
                "CONTROL strict marker narrow refusal"
            )
        },
    );
}
