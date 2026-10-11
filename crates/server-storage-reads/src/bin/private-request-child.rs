//! Linux-required private one-attempt literal-IP plain HTTP child; no activation.
use server_storage_reads::{child_http, child_request, isolated};
use std::process::ExitCode;
fn run() -> Result<(), ()> {
    isolated::linux_setup().map_err(|_| ())?;
    let runtime = child_http::runtime().map_err(|_| ())?;
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
    let cap = request.body_cap;
    let frame = runtime
        .block_on(child_http::execute(request))
        .map_err(|_| ())?;
    child_request::network_reply(&mut std::io::stdout().lock(), &frame, cap).map_err(|_| ())
}
fn main() -> ExitCode {
    if run().is_ok() {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}
