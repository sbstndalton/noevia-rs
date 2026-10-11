#[allow(dead_code)]
#[path = "../src/response_codecs.rs"]
mod codecs;
#[path = "support/codec_lab.rs"]
mod lab;
use server_storage_reads::{
    protocol::{Method, TransportFailure},
    response_ipc::Reply,
    LIST_BYTES, TEXT_BYTES,
};
use std::time::{Duration, Instant};
fn pure(
    header: &[u8],
    body: &[u8],
    pieces: &[usize],
    cap: usize,
) -> Result<Vec<u8>, codecs::Fault> {
    let mut headers = hyper::http::HeaderMap::new();
    headers.insert(
        "content-encoding",
        hyper::http::HeaderValue::from_bytes(header).unwrap_or_else(|_| unreachable!()),
    );
    let plan = codecs::Plan::headers(&headers)?;
    let mut p = codecs::Pipeline::new(plan, Instant::now() + Duration::from_secs(5))?;
    let mut out = Vec::new();
    out.try_reserve_exact(cap + 1)
        .unwrap_or_else(|_| unreachable!());
    let mut offset = 0;
    for size in pieces.iter().copied().chain(std::iter::repeat(4096)) {
        if offset == body.len() {
            break;
        }
        let end = (offset + size.clamp(1, 4096)).min(body.len());
        if let codecs::Progress::Capped = p.feed(
            body.get(offset..end).unwrap_or_else(|| unreachable!()),
            &mut out,
            cap,
        )? {
            return Ok(out);
        }
        offset = end;
    }
    if !matches!(p.finish(&mut out, cap)?, codecs::Progress::Complete) {
        return Err(codecs::Fault::Body);
    }
    Ok(out)
}
#[test]
fn new_ordinary_headers_methods_status_and_fixed_bytes() {
    // Independent fixed RFC1950/RFC1951/GZIP fixtures for literal hello.
    let gzip = [
        31, 139, 8, 0, 0, 0, 0, 0, 0, 3, 203, 72, 205, 201, 201, 7, 0, 134, 166, 16, 54, 5, 0, 0, 0,
    ];
    let zlib = [120, 156, 203, 72, 205, 201, 201, 7, 0, 6, 44, 2, 21];
    let raw = [203, 72, 205, 201, 201, 7, 0];
    for (header, body) in [
        (b"gzip".as_slice(), gzip.as_slice()),
        (b"x-gzip", gzip.as_slice()),
        (b"deflate", zlib.as_slice()),
        (b"deflate", raw.as_slice()),
        (b" \xa0GZIP\xa0 ", gzip.as_slice()),
    ] {
        lab::plaintext(header, body, b"hello");
    }
    for body in [b"".as_slice(), b"<x/>", b"\0binary\xff"] {
        for kind in ["gzip", "deflate", "raw"] {
            let encoded = lab::encode(kind, body);
            let header = if kind == "raw" {
                b"deflate".as_slice()
            } else {
                kind.as_bytes()
            };
            for method in [Method::Get, Method::Propfind] {
                for status in [200, 207] {
                    lab::run(
                        lab::response(status, &[header], &encoded),
                        1,
                        method,
                        TEXT_BYTES,
                        |r| {
                            assert!(
                                matches!(r,Reply::Http{payload,capped:false,..}if payload==body)
                            )
                        },
                    );
                }
            }
        }
    }
    for status in [204, 205, 302, 500] {
        lab::run(
            lab::response(status, &[b"bad,bad,bad,bad,bad,bad"], b""),
            1,
            Method::Get,
            TEXT_BYTES,
            |r| assert!(matches!(r,Reply::Http{payload,capped:false,..}if payload.is_empty())),
        );
    }
    for headers in [
        vec![],
        vec![b"".as_slice()],
        vec![b"identity".as_slice()],
        vec![b" ".as_slice()],
        vec![b"identity".as_slice(), b"identity".as_slice()],
    ] {
        lab::run(
            lab::response(200, &headers, b"raw"),
            1,
            Method::Get,
            TEXT_BYTES,
            |r| assert!(matches!(r,Reply::Http{payload,..}if payload==b"raw")),
        );
    }
}
#[test]
fn new_stack_order_unknown_and_six_tokens() {
    for count in [2, 3, 5] {
        let mut bytes = b"stack identity".to_vec();
        let mut tokens = Vec::new();
        for i in 0..count {
            let kind = if i % 2 == 0 { "gzip" } else { "deflate" };
            bytes = lab::encode(kind, &bytes);
            tokens.push(kind);
        }
        let header = tokens.join(", ");
        lab::plaintext(header.as_bytes(), &bytes, b"stack identity");
        let headers: Vec<&[u8]> = tokens.iter().map(|s| s.as_bytes()).collect();
        lab::run(
            lab::response(200, &headers, &bytes),
            1,
            Method::Get,
            TEXT_BYTES,
            |r| assert!(matches!(r,Reply::Http{payload,..}if payload==b"stack identity")),
        );
    }
    let gzip = lab::encode("gzip", b"ordinary");
    for header in [
        b"gzip,unknown".as_slice(),
        b"gzip,identity",
        b"gzip,",
        b",gzip",
    ] {
        lab::plaintext(header, &gzip, &gzip);
    }
    for header in [
        b"gzip,gzip,gzip,gzip,gzip,gzip".as_slice(),
        b",,,,,",
        b"unknown,unknown,unknown,unknown,unknown,unknown",
    ] {
        lab::run(
            lab::response(200, &[header], &gzip),
            4096,
            Method::Get,
            TEXT_BYTES,
            |r| {
                assert!(matches!(
                    r,
                    Reply::Failure {
                        failure: TransportFailure::Body,
                        ..
                    }
                ))
            },
        );
    }
}
#[test]
fn new_fragmentation_properties_and_strict_framing() {
    let body = b"finite property identity\0\xff";
    let mut seed = 0x5917u32;
    let mut total = 0;
    for kind in ["gzip", "deflate", "raw"] {
        let encoded = lab::encode(kind, body);
        let header = if kind == "raw" {
            b"deflate".as_slice()
        } else {
            kind.as_bytes()
        };
        for split in 1..encoded.len() {
            assert_eq!(
                pure(header, &encoded, &[split], TEXT_BYTES),
                Ok(body.to_vec())
            );
            total += encoded.len();
        }
        assert_eq!(
            pure(header, &encoded, &vec![1; encoded.len()], TEXT_BYTES),
            Ok(body.to_vec())
        );
        for _ in 0..64 {
            seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
            let size = (seed as usize % 31) + 1;
            assert_eq!(
                pure(header, &encoded, &vec![size; encoded.len()], TEXT_BYTES),
                Ok(body.to_vec())
            );
            total += encoded.len();
        }
        for trunc in 0..encoded.len() {
            assert!(
                pure(
                    header,
                    encoded.get(..trunc).unwrap_or_else(|| unreachable!()),
                    &[1],
                    TEXT_BYTES
                )
                .is_err(),
                "CONTROL strict EOF"
            );
        }
        let mut bad = encoded.clone();
        if kind != "raw" {
            let last = bad.last_mut().unwrap_or_else(|| unreachable!());
            *last ^= 1;
            assert!(pure(header, &bad, &[1], TEXT_BYTES).is_err());
        }
        let mut tail = encoded.clone();
        tail.push(1);
        assert!(pure(header, &tail, &[1], TEXT_BYTES).is_err());
    }
    let mut members = lab::encode("gzip", b"one");
    members.extend(lab::encode("gzip", b"two"));
    assert_eq!(
        pure(b"gzip", &members, &vec![1; members.len()], TEXT_BYTES),
        Ok(b"onetwo".to_vec())
    );
    let mut gzip = lab::encode("gzip", body);
    *gzip.get_mut(3).unwrap_or_else(|| unreachable!()) = 0x04 | 0x08 | 0x10;
    let mut fields = vec![2, 0, 7, 9];
    fields.extend_from_slice(b"name\0comment\0");
    gzip.splice(10..10, fields);
    for split in 1..gzip.len() {
        assert_eq!(
            pure(b"gzip", &gzip, &[split], TEXT_BYTES),
            Ok(body.to_vec())
        );
    }
    for field in [0x08, 0x10] {
        for length in [65535, 65536] {
            let mut g = lab::encode("gzip", body);
            *g.get_mut(3).unwrap_or_else(|| unreachable!()) = field;
            let mut f = vec![b'a'; length];
            f.push(0);
            g.splice(10..10, f);
            assert_eq!(
                pure(b"gzip", &g, &vec![1; g.len()], TEXT_BYTES).is_ok(),
                length == 65535
            );
        }
    }
    for index in [0, 2, 3] {
        let mut g = lab::encode("gzip", body);
        *g.get_mut(index).unwrap_or_else(|| unreachable!()) = 255;
        assert!(pure(b"gzip", &g, &[1], TEXT_BYTES).is_err());
    }
    println!("property seed=0x5917 schedules64 per codec finite tested_bytes={total}");
}
#[test]
fn new_decoded_boundaries_and_deadline() {
    for cap in [TEXT_BYTES, LIST_BYTES] {
        for length in [cap - 1, cap, cap + 1] {
            for kind in ["gzip", "deflate", "raw"] {
                let body = vec![b'x'; length];
                let encoded = lab::encode(kind, &body);
                let header = if kind == "raw" {
                    b"deflate".as_slice()
                } else {
                    kind.as_bytes()
                };
                lab::run(
                    lab::response(200, &[header], &encoded),
                    4096,
                    Method::Get,
                    cap,
                    |r| {
                        if length > cap {
                            assert!(matches!(
                                r,
                                Reply::Failure {
                                    failure: TransportFailure::Body,
                                    ..
                                }
                            ));
                        } else {
                            assert!(
                                matches!(r,Reply::Http{payload,capped:false,..}if payload==body)
                            );
                        }
                    },
                );
            }
        }
    }
    let headers = hyper::http::HeaderMap::new();
    let plan = codecs::Plan::headers(&headers).unwrap_or_else(|_| unreachable!());
    assert!(matches!(
        codecs::Pipeline::new(plan, Instant::now() - Duration::from_secs(1)),
        Err(codecs::Fault::Timeout)
    ));
}

#[test]
fn new_virtual_encoded_counter_exact_plus_one_and_pending_clock() {
    let headers = hyper::http::HeaderMap::new();
    let mut p = codecs::Pipeline::new(
        codecs::Plan::headers(&headers).unwrap_or_else(|_| unreachable!()),
        Instant::now() + Duration::from_secs(5),
    )
    .unwrap_or_else(|_| unreachable!());
    let mut out = Vec::with_capacity(TEXT_BYTES + 1);
    for _ in 0..codecs::ENTITY / codecs::STEP {
        assert!(p.feed(&[b'x'; 4096], &mut out, TEXT_BYTES).is_ok());
        out.clear();
    }
    assert!(matches!(
        p.feed(b"x", &mut out, TEXT_BYTES),
        Err(codecs::Fault::Body)
    ));
    assert_eq!(
        codecs::entity_total(codecs::ENTITY - 4096, 4096),
        Ok(codecs::ENTITY)
    );
    assert_eq!(
        codecs::entity_total(codecs::ENTITY, 1),
        Err(codecs::Fault::Body)
    );
    assert_eq!(
        codecs::entity_total(usize::MAX, 1),
        Err(codecs::Fault::Body)
    );
    // Header pumping and refills share the captured clock, with no reset.
    let mut headers = hyper::http::HeaderMap::new();
    headers.insert(
        "content-encoding",
        hyper::http::HeaderValue::from_static("gzip,gzip"),
    );
    let mut p = codecs::Pipeline::new(
        codecs::Plan::headers(&headers).unwrap_or_else(|_| unreachable!()),
        Instant::now() + Duration::from_millis(10),
    )
    .unwrap_or_else(|_| unreachable!());
    assert!(p.feed(&[31], &mut out, TEXT_BYTES).is_ok());
    std::thread::sleep(Duration::from_millis(15));
    assert!(matches!(
        p.feed(&[139], &mut out, TEXT_BYTES),
        Err(codecs::Fault::Timeout)
    ));
}

#[test]
fn new_extended_header_checksum_brotli_and_output_boundaries() {
    let mut gzip = lab::encode("gzip", b"header");
    *gzip.get_mut(3).unwrap_or_else(|| unreachable!()) = 2;
    let mut crc = flate2::Crc::new();
    crc.update(gzip.get(..10).unwrap_or_else(|| unreachable!()));
    let checksum = (crc.sum() as u16).to_le_bytes();
    gzip.splice(10..10, checksum);
    for split in 1..gzip.len() {
        assert_eq!(
            pure(b"gzip", &gzip, &[split], TEXT_BYTES),
            Ok(b"header".to_vec())
        );
    }
    *gzip.get_mut(10).unwrap_or_else(|| unreachable!()) ^= 1;
    assert!(pure(b"gzip", &gzip, &[1], TEXT_BYTES).is_err());
    for field in [0x04, 0x08, 0x10] {
        let mut g = lab::encode("gzip", b"fields");
        *g.get_mut(3).unwrap_or_else(|| unreachable!()) = field;
        let f = if field == 4 {
            let mut f = vec![255, 255];
            f.extend(vec![7; 65535]);
            f
        } else {
            let mut f = vec![b'a'; 65535];
            f.push(0);
            f
        };
        g.splice(10..10, f);
        assert_eq!(
            pure(b"gzip", &g, &vec![4096; g.len() / 4096 + 1], TEXT_BYTES),
            Ok(b"fields".to_vec())
        );
    }
    // Exact bytes from the installed fixed Node Brotli compression capture.
    let br = [
        11, 7, 128, 111, 114, 100, 105, 110, 97, 114, 121, 32, 98, 114, 111, 116, 108, 105, 3,
    ];
    for split in 1..br.len() {
        assert_eq!(
            pure(b"br", &br, &[split], TEXT_BYTES),
            Ok(b"ordinary brotli".to_vec())
        );
    }
    for trunc in 0..br.len() {
        assert!(pure(
            b"br",
            br.get(..trunc).unwrap_or_else(|| unreachable!()),
            &[1],
            TEXT_BYTES
        )
        .is_err());
    }
    assert_eq!(pure(b"br", &[59], &[1], TEXT_BYTES), Ok(Vec::new()));
    for tail in [0, 1, 59] {
        let mut b = br.to_vec();
        b.push(tail);
        assert!(pure(b"br", &b, &[1], TEXT_BYTES).is_err());
    }
    for length in [4095, 4096, 4097, 8192] {
        let plain: Vec<u8> = (0..length).map(|i| (i % 251) as u8).collect();
        for kind in ["gzip", "deflate", "raw"] {
            let encoded = lab::encode(kind, &plain);
            assert_eq!(
                pure(
                    if kind == "raw" {
                        b"deflate"
                    } else {
                        kind.as_bytes()
                    },
                    &encoded,
                    &vec![1; encoded.len()],
                    TEXT_BYTES
                ),
                Ok(plain.clone())
            );
        }
    }
    let mut dictionary = vec![120, 32, 0, 0, 0, 1];
    dictionary.extend(lab::encode("raw", b"dictionary"));
    assert!(pure(b"deflate", &dictionary, &[1], TEXT_BYTES).is_err());
}

#[test]
fn new_partial_encoded_feed_parent_cancel_and_deadline_ownership() {
    use server_storage_reads::{
        admission::Admission,
        isolated::{self, Error, NetworkObservation},
        protocol::Request,
    };
    use std::{
        process::Command,
        sync::atomic::{AtomicBool, Ordering},
    };
    for cancel_mode in [true, false] {
        let listener = lab::listener();
        let address = listener.local_addr().unwrap_or_else(|_| unreachable!());
        let mut message =
            b"HTTP/1.1 200 Synthetic\r\nContent-Length: 20\r\nContent-Encoding: gzip\r\n\r\n"
                .to_vec();
        message.push(31);
        let delivered = AtomicBool::new(false);
        let cancel = AtomicBool::new(false);
        let admission = Admission::default();
        let lease = admission
            .try_acquire("codec-owner")
            .unwrap_or_else(|_| unreachable!());
        let mut before_reap = false;
        std::thread::scope(|scope| {
            let server = scope.spawn(|| {
                lab::serve_control(
                    listener,
                    &message,
                    1,
                    Duration::from_millis(250),
                    Some(&delivered),
                )
            });
            let trigger = scope.spawn(|| {
                if cancel_mode {
                    let deadline = Instant::now() + Duration::from_secs(1);
                    while !delivered.load(Ordering::Acquire) && Instant::now() < deadline {
                        std::thread::sleep(Duration::from_millis(1));
                    }
                    if delivered.load(Ordering::Acquire) {
                        std::thread::sleep(Duration::from_millis(10));
                        cancel.store(true, Ordering::Release);
                    }
                }
            });
            let result = isolated::attempt_http_observed(
                Command::new(env!("CARGO_BIN_EXE_private-http-fixture")),
                Request {
                    method: Method::Get,
                    url: format!("http://{address}/synthetic"),
                    headers: vec![],
                    body_cap: TEXT_BYTES,
                    attempt_timeout: Duration::from_millis(100),
                },
                if cancel_mode {
                    Duration::from_secs(1)
                } else {
                    Duration::from_millis(100)
                },
                &cancel,
                lease,
                |observation, _| {
                    if matches!(observation, NetworkObservation::BeforeReap) {
                        before_reap = true;
                        assert!(admission.try_acquire("codec-owner").is_err());
                    }
                },
            );
            if cancel_mode {
                assert!(matches!(result, Err(Error::Cancelled)));
            } else {
                assert!(matches!(result, Err(Error::Deadline)));
            }
            trigger.join().unwrap_or_else(|_| unreachable!());
            let input = server.join().unwrap_or_else(|_| unreachable!());
            assert!(!input.is_empty());
        });
        assert!(delivered.load(Ordering::Acquire) && before_reap);
        assert!(admission.try_acquire("codec-owner").is_ok());
    }
}
