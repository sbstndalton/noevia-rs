#[path = "http_plain.rs"]
mod lab;
use server_storage_reads::{
    admission::Admission,
    child_http,
    isolated::{self, Error, NetworkObservation},
    protocol::{Method, Request},
    LIST_BYTES, TEXT_BYTES,
};
use std::{
    net::TcpListener,
    process::Command,
    sync::atomic::{AtomicBool, Ordering},
    time::Duration,
};
fn request(url: String, cap: usize) -> Request {
    Request {
        method: Method::Get,
        url,
        headers: vec![],
        attempt_timeout: Duration::from_secs(15),
        body_cap: cap,
        body: Default::default(),
    }
}
#[test]
fn invalid_cap_refuses_before_transport() {
    let runtime = child_http::runtime().unwrap_or_else(|_| unreachable!());
    for cap in [0, TEXT_BYTES - 1, LIST_BYTES + 1] {
        assert!(matches!(
            runtime.block_on(child_http::execute(request(
                "http://127.0.0.1:1/".into(),
                cap
            ))),
            Err(Error::Input)
        ));
    }
}
#[test]
fn actual_http_cancel_retains_owner_through_reap() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap_or_else(|_| unreachable!());
    listener
        .set_nonblocking(true)
        .unwrap_or_else(|_| unreachable!());
    let address = listener.local_addr().unwrap_or_else(|_| unreachable!());
    std::thread::scope(|scope| {
        let server = scope.spawn(move || {
            lab::serve(
                listener,
                lab::response(200, "", &vec![b'x'; LIST_BYTES]),
                1,
                vec![],
            )
        });
        let a = Admission::default();
        let p = a.try_acquire("fixed").unwrap_or_else(|_| unreachable!());
        let cancel = AtomicBool::new(false);
        let mut seen = false;
        let mut cleanup = false;
        let result = isolated::attempt_http_observed(
            Command::new(env!("CARGO_BIN_EXE_private-http-fixture")),
            request(format!("http://{address}/fixed"), LIST_BYTES),
            Duration::from_secs(16),
            &cancel,
            p,
            |stage, _| {
                assert!(a.try_acquire("fixed").is_err(), "CONTROL lease held");
                match stage {
                    NetworkObservation::OutputProgress(n) if n > 38 => {
                        seen = true;
                        cancel.store(true, Ordering::Release);
                    }
                    NetworkObservation::BeforeReap => cleanup = true,
                    _ => {}
                }
            },
        );
        assert!(matches!(result, Err(Error::Cancelled)));
        assert!(seen && cleanup && a.try_acquire("fixed").is_ok());
        server.join().unwrap_or_else(|_| unreachable!());
    });
}
#[test]
fn extension_dependency_boundary() {
    for size in [16383, 16384] {
        let mut message = b"HTTP/1.1 200 Fine\r\nTransfer-Encoding: chunked\r\n\r\n3;".to_vec();
        message.extend_from_slice(&vec![b'a'; size]);
        message.extend_from_slice(b"\r\nabc\r\n0\r\n\r\n");
        lab::run(message, 1, vec![], Method::Get, TEXT_BYTES, |reply| {
            if size == 16383 {
                assert!(
                    matches!(reply,server_storage_reads::response_ipc::Reply::Http{payload,..}if *payload==b"abc")
                );
            } else {
                assert!(matches!(
                    reply,
                    server_storage_reads::response_ipc::Reply::Failure {
                        failure: server_storage_reads::protocol::TransportFailure::Body,
                        ..
                    }
                ));
            }
        });
    }
}
