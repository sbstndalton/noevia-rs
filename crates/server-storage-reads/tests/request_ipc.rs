use server_storage_reads::{
    protocol::{Method, Request},
    request_ipc::{decode, encode},
    LIST_BYTES,
};
use std::time::Duration;
use zeroize::Zeroizing;
#[test]
fn owned_request_roundtrip_and_negative_lengths() {
    let request = Request {
        method: Method::Propfind,
        url: "https://synthetic.invalid/dav/a%20b".into(),
        headers: vec![(
            "Authorization".into(),
            Zeroizing::new("Basic c3ludGhldGlj".into()),
        )],
        body_cap: LIST_BYTES,
        body: Default::default(),
        attempt_timeout: Duration::from_nanos(15_000_000_001),
    };
    let bytes = encode(&request).unwrap_or_else(|_| unreachable!());
    let decoded = decode(&bytes).unwrap_or_else(|_| unreachable!());
    assert_eq!(decoded.method, request.method);
    assert_eq!(decoded.url, request.url);
    assert_eq!(decoded.headers, request.headers);
    assert_eq!(decoded.body_cap, request.body_cap);
    assert_eq!(decoded.attempt_timeout, request.attempt_timeout);
    for end in 0..bytes.len() {
        assert!(decode(bytes.get(..end).unwrap_or_default()).is_err());
    }
    let mut trailing = bytes.to_vec();
    trailing.push(0);
    assert!(decode(&trailing).is_err());
    let mut bad = request;
    bad.headers
        .push(("x".into(), Zeroizing::new("a\r\nb".into())));
    assert!(encode(&bad).is_err());
}
