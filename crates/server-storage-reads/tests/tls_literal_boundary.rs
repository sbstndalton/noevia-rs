use server_storage_reads::{
    child_http,
    protocol::{Method, Request, TransportFailure},
    response_ipc::{self, Phase, Reply},
    TEXT_BYTES,
};
use std::time::Duration;
#[test]
fn production_hostname_refuses_before_dns_or_socket() {
    let policy = child_http::ProductionTls::new().unwrap_or_else(|_| unreachable!());
    let bytes = child_http::runtime()
        .unwrap_or_else(|_| unreachable!())
        .block_on(child_http::execute_tls(
            Request {
                method: Method::Get,
                url: "https://localhost/".into(),
                headers: vec![],
                body_cap: TEXT_BYTES,
                body: Default::default(),
                attempt_timeout: Duration::from_secs(1),
            },
            &policy,
        ))
        .unwrap_or_else(|_| unreachable!());
    assert!(matches!(
        response_ipc::decode(&bytes, TEXT_BYTES).unwrap_or_else(|_| unreachable!()),
        Reply::Failure {
            phase: Phase::Dns,
            failure: TransportFailure::Dns
        }
    ));
}
