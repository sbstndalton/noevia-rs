//! Separate portable artifact: fixed loopback-only request capability, no Linux bypass.
use server_storage_reads::{child_http, child_request};
fn run() -> Result<(), ()> {
    let runtime = child_http::runtime().map_err(|_| ())?;
    let request =
        child_request::receive(&mut std::io::stdin().lock(), &mut std::io::stdout().lock())
            .map_err(|_| ())?;
    let url = url::Url::parse(&request.url).map_err(|_| ())?;
    let host = url
        .host_str()
        .ok_or(())?
        .trim_start_matches('[')
        .trim_end_matches(']');
    if url.scheme() != "http"
        || !host
            .parse::<std::net::IpAddr>()
            .map_err(|_| ())?
            .is_loopback()
    {
        return Err(());
    }
    let cap = request.body_cap;
    let frame = runtime
        .block_on(child_http::execute(request))
        .map_err(|_| ())?;
    child_request::network_reply(&mut std::io::stdout().lock(), &frame, cap).map_err(|_| ())
}
fn main() -> std::process::ExitCode {
    if run().is_ok() {
        std::process::ExitCode::SUCCESS
    } else {
        std::process::ExitCode::FAILURE
    }
}
