//! Linux-required Request boundary only. No HTTP/socket/resolver or activation.
use server_storage_reads::{
    child_request, isolated,
    protocol::TransportFailure,
    response_ipc::{self, Phase, Reply},
};
use std::process::ExitCode;
fn run() -> Result<(), ()> {
    isolated::linux_setup().map_err(|_| ())?;
    let _runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .max_blocking_threads(1)
        .thread_stack_size(2 * 1024 * 1024)
        .build()
        .map_err(|_| ())?;
    let roots = rustls::RootCertStore::from_iter(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    let mut _config = rustls::ClientConfig::builder_with_provider(std::sync::Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .map_err(|_| ())?
    .with_root_certificates(roots)
    .with_no_client_auth();
    _config.resumption = rustls::client::Resumption::disabled();
    _config.enable_early_data = false;
    let request =
        child_request::receive(&mut std::io::stdin().lock(), &mut std::io::stdout().lock())
            .map_err(|_| ())?;
    // No network phase implemented: fixed nonretryable refusal, no fallback.
    let frame = response_ipc::encode(
        Reply::Failure {
            phase: Phase::Headers,
            failure: TransportFailure::Other,
        },
        request.body_cap,
    )
    .map_err(|_| ())?;
    child_request::network_reply(&mut std::io::stdout().lock(), &frame, request.body_cap)
        .map_err(|_| ())
}
fn main() -> ExitCode {
    if run().is_ok() {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}
