//! New finite numeric-loopback codec controls; no resolver or stored data.
#![allow(dead_code)]
use server_storage_reads::{
    child_http,
    protocol::{Method, Request},
    response_ipc::{self, Reply},
    TEXT_BYTES,
};
use std::{
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    time::{Duration, Instant},
};
pub fn encode(kind: &str, bytes: &[u8]) -> Vec<u8> {
    use flate2::{
        write::{DeflateEncoder, GzEncoder, ZlibEncoder},
        Compression,
    };
    match kind {
        "gzip" => {
            let mut e = GzEncoder::new(Vec::new(), Compression::default());
            e.write_all(bytes).unwrap_or_else(|_| unreachable!());
            e.finish().unwrap_or_else(|_| unreachable!())
        }
        "deflate" => {
            let mut e = ZlibEncoder::new(Vec::new(), Compression::default());
            e.write_all(bytes).unwrap_or_else(|_| unreachable!());
            e.finish().unwrap_or_else(|_| unreachable!())
        }
        "raw" => {
            let mut e = DeflateEncoder::new(Vec::new(), Compression::default());
            e.write_all(bytes).unwrap_or_else(|_| unreachable!());
            e.finish().unwrap_or_else(|_| unreachable!())
        }
        _ => unreachable!(),
    }
}
pub fn response(status: u16, headers: &[&[u8]], body: &[u8]) -> Vec<u8> {
    let mut out = format!(
        "HTTP/1.1 {status} Synthetic\r\nContent-Length: {}\r\n",
        body.len()
    )
    .into_bytes();
    for header in headers {
        out.extend_from_slice(b"Content-Encoding: ");
        out.extend_from_slice(header);
        out.extend_from_slice(b"\r\n");
    }
    out.extend_from_slice(b"\r\n");
    out.extend_from_slice(body);
    out
}
fn transient(e: &std::io::Error) -> bool {
    matches!(
        e.kind(),
        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted
    )
}
pub fn listener() -> TcpListener {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap_or_else(|_| unreachable!());
    listener
        .set_nonblocking(true)
        .unwrap_or_else(|_| unreachable!());
    listener
}
pub fn serve(listener: TcpListener, message: &[u8], split: usize) -> Vec<u8> {
    serve_control(listener, message, split, Duration::ZERO, None)
}
pub fn serve_control(
    listener: TcpListener,
    message: &[u8],
    split: usize,
    hold: Duration,
    delivered: Option<&std::sync::atomic::AtomicBool>,
) -> Vec<u8> {
    assert!(hold <= Duration::from_millis(500));
    assert!(message.len() <= 8 * 1024 * 1024);
    let deadline = Instant::now() + Duration::from_secs(8);
    let mut socket: TcpStream = loop {
        match listener.accept() {
            Ok((socket, _)) => break socket,
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
    let mut scratch = [0; 4096];
    while !input.windows(4).any(|s| s == b"\r\n\r\n") && Instant::now() < deadline {
        match socket.read(&mut scratch) {
            Ok(0) => return input,
            Ok(n) => {
                assert!(input.len() + n <= 65536);
                input.extend_from_slice(scratch.get(..n).unwrap_or_else(|| unreachable!()));
            }
            Err(e) if transient(&e) => std::thread::sleep(Duration::from_millis(1)),
            _ => return input,
        }
    }
    let mut offset = 0;
    while offset < message.len() && Instant::now() < deadline {
        let end = (offset + split.clamp(1, 4096)).min(message.len());
        match socket.write(message.get(offset..end).unwrap_or_else(|| unreachable!())) {
            Ok(0) => break,
            Ok(n) => offset += n,
            Err(e) if transient(&e) => std::thread::sleep(Duration::from_millis(1)),
            _ => break,
        }
    }
    if offset == message.len() {
        if let Some(delivered) = delivered {
            delivered.store(true, std::sync::atomic::Ordering::Release);
        }
        std::thread::sleep(hold);
    }
    input
}
pub fn run(
    message: Vec<u8>,
    split: usize,
    method: Method,
    cap: usize,
    check: impl FnOnce(Reply<'_>),
) {
    let listener = listener();
    let address = listener.local_addr().unwrap_or_else(|_| unreachable!());
    std::thread::scope(|scope| {
        let server = scope.spawn(move || serve(listener, &message, split));
        let request = Request {
            method,
            url: format!("http://{address}/synthetic"),
            headers: vec![],
            body_cap: cap,
            attempt_timeout: Duration::from_secs(5),
        };
        let frame = child_http::runtime()
            .unwrap_or_else(|_| unreachable!())
            .block_on(child_http::execute(request))
            .unwrap_or_else(|_| unreachable!());
        check(response_ipc::decode(&frame, cap).unwrap_or_else(|_| unreachable!()));
        let input = server.join().unwrap_or_else(|_| unreachable!());
        assert!(!input.is_empty(), "CONTROL owned request observed");
    });
}
pub fn plaintext(header: &[u8], body: &[u8], expected: &[u8]) {
    run(
        response(200, &[header], body),
        4096,
        Method::Get,
        TEXT_BYTES,
        |r| {
            assert!(
                matches!(r, Reply::Http{payload,capped:false,..} if payload==expected),
                "CONTROL decoded bytes"
            )
        },
    );
}
