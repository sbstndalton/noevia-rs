//! Owned loopback fixtures only; no external endpoint, resolver or stored credentials.
use server_storage_reads::{
    admission::Admission,
    isolated,
    protocol::{Method, Request, TransportFailure},
    response_ipc::Reply,
    LIST_BYTES, TEXT_BYTES,
};
use std::{
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    process::Command,
    sync::atomic::AtomicBool,
    time::{Duration, Instant},
};
use zeroize::Zeroizing;
fn request(url: String, method: Method, cap: usize) -> Request {
    Request {
        method,
        url,
        headers: vec![
            ("host".into(), Zeroizing::new("signed.synthetic:987".into())),
            (
                "authorization".into(),
                Zeroizing::new("Synthetic only".into()),
            ),
            ("x-fixed".into(), Zeroizing::new("first".into())),
            ("x-fixed".into(), Zeroizing::new("second".into())),
        ],
        attempt_timeout: Duration::from_secs(15),
        body_cap: cap,
    }
}
fn transient(e: &std::io::Error) -> bool {
    matches!(
        e.kind(),
        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted
    )
}
fn write(socket: &mut TcpStream, bytes: &[u8], deadline: Instant) -> bool {
    let mut offset = 0;
    while offset < bytes.len() && Instant::now() < deadline {
        let end = (offset + 4096).min(bytes.len());
        match socket.write(bytes.get(offset..end).unwrap_or_else(|| unreachable!())) {
            Ok(0) => return false,
            Ok(n) => offset += n,
            Err(e) if transient(&e) => std::thread::sleep(Duration::from_millis(1)),
            Err(_) => return false,
        }
    }
    offset == bytes.len()
}
pub fn serve(listener: TcpListener, head: Vec<u8>, repeat: usize, tail: Vec<u8>) -> Vec<u8> {
    let deadline = Instant::now() + Duration::from_secs(18);
    let mut socket = loop {
        match listener.accept() {
            Ok((s, _)) => break s,
            Err(e) if transient(&e) && Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(1))
            }
            _ => return Vec::new(),
        }
    };
    socket
        .set_nonblocking(true)
        .unwrap_or_else(|_| unreachable!());
    let mut input = Vec::new();
    input
        .try_reserve_exact(65536)
        .unwrap_or_else(|_| unreachable!());
    let mut scratch = [0; 4096];
    while Instant::now() < deadline && !input.windows(4).any(|b| b == b"\r\n\r\n") {
        match socket.read(&mut scratch) {
            Ok(0) => return input,
            Ok(n) => {
                assert!(input.len() + n <= 65536, "CONTROL fixture input cap");
                input.extend_from_slice(scratch.get(..n).unwrap_or_else(|| unreachable!()));
            }
            Err(e) if transient(&e) => std::thread::sleep(Duration::from_millis(1)),
            Err(_) => return input,
        }
    }
    for _ in 0..repeat {
        if !write(&mut socket, &head, deadline) {
            return input;
        }
    }
    write(&mut socket, &tail, deadline);
    input
}
pub fn run(
    head: Vec<u8>,
    repeat: usize,
    tail: Vec<u8>,
    method: Method,
    cap: usize,
    check: impl FnOnce(&Reply<'_>),
) -> Vec<u8> {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap_or_else(|_| unreachable!());
    listener
        .set_nonblocking(true)
        .unwrap_or_else(|_| unreachable!());
    let address = listener.local_addr().unwrap_or_else(|_| unreachable!());
    std::thread::scope(|scope| {
        let server = scope.spawn(move || serve(listener, head, repeat, tail));
        let a = Admission::default();
        let p = a
            .try_acquire("synthetic-owner")
            .unwrap_or_else(|_| unreachable!());
        let r = isolated::attempt_http(
            Command::new(env!("CARGO_BIN_EXE_private-http-fixture")),
            request(format!("http://{address}/a%2Fb?X=a%2Fb&X=2"), method, cap),
            Duration::from_secs(16),
            &AtomicBool::new(false),
            p,
        )
        .unwrap_or_else(|_| unreachable!());
        let view = r.view().unwrap_or_else(|_| unreachable!());
        check(&view);
        assert!(a.try_acquire("synthetic-owner").is_ok());
        server.join().unwrap_or_else(|_| unreachable!())
    })
}
pub fn response(status: u16, headers: &str, body: &[u8]) -> Vec<u8> {
    let mut out = format!(
        "HTTP/1.1 {status} Synthetic\r\nContent-Length: {}\r\n{headers}\r\n",
        body.len()
    )
    .into_bytes();
    out.extend_from_slice(body);
    out
}
#[test]
fn methods_signed_bytes_status_redirect_and_information() {
    for method in [Method::Get, Method::Propfind] {
        let input = run(
            response(207, "", b"ordinary"),
            1,
            vec![],
            method,
            TEXT_BYTES,
            |r| {
                assert!(
                    matches!(r,Reply::Http{status:207,capped:false,payload}if *payload==b"ordinary")
                )
            },
        );
        let text = std::str::from_utf8(&input).unwrap_or_else(|_| unreachable!());
        assert!(text.starts_with(match method {
            Method::Get => "GET /a%2Fb?X=a%2Fb&X=2 HTTP/1.1\r\n",
            Method::Propfind => "PROPFIND /a%2Fb?X=a%2Fb&X=2 HTTP/1.1\r\n",
        }));
        let text = text.to_ascii_lowercase();
        assert!(
            text.contains("host: signed.synthetic:987\r\n")
                && text.contains("authorization: synthetic only\r\n")
                && text.contains("x-fixed: first\r\n")
                && text.contains("x-fixed: second\r\n")
        );
    }
    for status in [204, 500, 502, 302] {
        run(
            response(status, "", if status == 204 { b"" } else { b"ignored" }),
            1,
            vec![],
            Method::Get,
            TEXT_BYTES,
            |r| {
                assert!(
                    matches!(r,Reply::Http{status:s,capped:false,payload}if *s==status&&payload.is_empty())
                )
            },
        );
    }
    run(
        response(302, "Location: /another\r\n", b""),
        1,
        vec![],
        Method::Get,
        TEXT_BYTES,
        |r| {
            assert!(matches!(
                r,
                Reply::Failure {
                    failure: TransportFailure::Redirect,
                    ..
                }
            ))
        },
    );
    run(
        b"HTTP/1.1 103 Early Hints\r\nLink: </fixed>\r\n\r\n".to_vec(),
        1,
        response(200, "", b"final"),
        Method::Get,
        TEXT_BYTES,
        |r| assert!(matches!(r,Reply::Http{status:200,payload,..}if *payload==b"final")),
    );
}
#[test]
fn decoded_cap_lookahead_and_trailer_limits() {
    for cap in [TEXT_BYTES, LIST_BYTES] {
        for size in [cap, cap + 1, cap + 4096] {
            run(
                response(200, "", &vec![b'x'; size]),
                1,
                vec![],
                Method::Get,
                cap,
                |r| {
                    assert!(
                        matches!(r,Reply::Http{status:200,capped,payload}if *capped==(size>cap)&&payload.len()==size.min(cap+1))
                    )
                },
            );
        }
    }
    run(b"HTTP/1.1 200 Fine\r\nTransfer-Encoding: chunked\r\n\r\n3\r\nabc\r\n0\r\nX-Fixed: trailer\r\n\r\n".to_vec(),1,vec![],Method::Get,TEXT_BYTES,|r|assert!(matches!(r,Reply::Http{payload,..}if *payload==b"abc")));
    let mut trailer = b"HTTP/1.1 200 Fine\r\nTransfer-Encoding: chunked\r\n\r\n0\r\nX: ".to_vec();
    trailer.extend_from_slice(&vec![b'a'; 16384]);
    trailer.extend_from_slice(b"\r\n\r\n");
    run(trailer, 1, vec![], Method::Get, TEXT_BYTES, |r| {
        assert!(matches!(
            r,
            Reply::Failure {
                failure: TransportFailure::Body,
                ..
            }
        ))
    });
}
#[test]
fn framing_header_wire_and_encoding_refusals() {
    for message in [
        b"HTTP/1.1 200 Fine\r\nContent-Length: 5\r\n\r\na".to_vec(),
        b"HTTP/1.1 200 Fine\r\nTransfer-Encoding: chunked\r\n\r\ninvalid\r\n".to_vec(),
        response(200, "Content-Encoding: gzip\r\n", b"not decoded"),
    ] {
        run(message, 1, vec![], Method::Get, TEXT_BYTES, |r| {
            assert!(matches!(
                r,
                Reply::Failure {
                    failure: TransportFailure::Body,
                    ..
                }
            ))
        });
    }
    let mut header = b"HTTP/1.1 200 Fine\r\n".to_vec();
    for _ in 0..100 {
        header.extend_from_slice(b"X: a\r\n");
    }
    header.extend_from_slice(b"Content-Length: 0\r\n\r\n");
    run(header, 1, vec![], Method::Get, TEXT_BYTES, |r| {
        assert!(matches!(
            r,
            Reply::Failure {
                failure: TransportFailure::Header,
                ..
            }
        ))
    });
    let mut info = b"HTTP/1.1 103 Hint\r\nX: ".to_vec();
    info.extend_from_slice(&vec![b'a'; 4000]);
    info.extend_from_slice(b"\r\n\r\n");
    run(
        info,
        17000,
        response(200, "", b""),
        Method::Get,
        TEXT_BYTES,
        |r| {
            assert!(matches!(
                r,
                Reply::Failure {
                    failure: TransportFailure::Header,
                    ..
                }
            ))
        },
    );
}
