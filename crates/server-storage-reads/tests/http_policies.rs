#[path = "http_plain.rs"]
mod lab;
use server_storage_reads::{
    protocol::{Method, TransportFailure},
    response_ipc::Reply,
    TEXT_BYTES,
};
#[test]
fn wire_policy() {
    let mut info = b"HTTP/1.1 103 Hint\r\nX: ".to_vec();
    info.extend_from_slice(&vec![b'a'; 4000]);
    info.extend_from_slice(b"\r\n\r\n");
    lab::run(
        info,
        17000,
        lab::response(200, "", b""),
        Method::Get,
        TEXT_BYTES,
        |r| {
            let refused = matches!(
                r,
                Reply::Failure {
                    failure: TransportFailure::Header,
                    ..
                }
            );
            assert!(refused, "M4_HTTP_WIRE");
        },
    );
}
#[test]
fn redirect_policy() {
    lab::run(
        lab::response(302, "Location: /fixed\r\n", b""),
        1,
        vec![],
        Method::Get,
        TEXT_BYTES,
        |r| {
            let refused = matches!(
                r,
                Reply::Failure {
                    failure: TransportFailure::Redirect,
                    ..
                }
            );
            assert!(refused, "M4_HTTP_REDIRECT");
        },
    );
}
#[test]
fn lookahead_policy() {
    lab::run(
        lab::response(200, "", &vec![b'x'; TEXT_BYTES + 1]),
        1,
        vec![],
        Method::Get,
        TEXT_BYTES,
        |r| {
            let correct =
                matches!(r,Reply::Http{capped:true,payload,..}if payload.len()==TEXT_BYTES+1);
            assert!(correct, "M4_HTTP_LOOKAHEAD");
        },
    );
}
