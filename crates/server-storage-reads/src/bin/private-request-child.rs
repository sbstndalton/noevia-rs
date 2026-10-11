//! Linux-required private literal-IP HTTP/TLS child; no activation or resolver.
use server_storage_reads::{child_http, child_request, isolated};
use std::process::ExitCode;
fn run() -> Result<(), ()> {
    isolated::linux_setup().map_err(|_| ())?;
    let runtime = child_http::runtime().map_err(|_| ())?;
    let policy = child_http::ProductionTls::new().map_err(|_| ())?;
    let request =
        child_request::receive(&mut std::io::stdin().lock(), &mut std::io::stdout().lock())
            .map_err(|_| ())?;
    let cap = request.body_cap;
    let frame = runtime
        .block_on(child_http::execute_tls(request, &policy))
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
