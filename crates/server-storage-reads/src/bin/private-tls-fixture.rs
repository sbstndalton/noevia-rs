//! Separate fixed loopback synthetic artifact, never a production trust switch.
use server_storage_reads::{
    child_request, isolated, protocol, response_ipc, CHUNK_FRAME, CHUNK_PREFIX, ETAG_BYTES,
    LIST_BYTES, TEXT_BYTES,
};
#[allow(dead_code)]
#[path = "../child_http.rs"]
mod actual_transport;
#[allow(dead_code)]
#[path = "support/tls_file.rs"]
mod fixture_files;
use std::{path::PathBuf, sync::Arc};
#[derive(Debug)]
struct FutureTime;
impl rustls::time_provider::TimeProvider for FutureTime {
    fn current_time(&self) -> Option<rustls::pki_types::UnixTime> {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .ok()?;
        Some(rustls::pki_types::UnixTime::since_unix_epoch(
            now.checked_add(std::time::Duration::from_secs(2 * 86400))?,
        ))
    }
}
fn run() -> Result<(), ()> {
    let mut args = std::env::args().skip(1);
    let mode = args.next().ok_or(())?;
    if !matches!(
        mode.as_str(),
        "trust" | "public" | "expired" | "dns" | "tls12" | "tls13"
    ) {
        return Err(());
    }
    let directory = PathBuf::from(args.next().ok_or(())?);
    if args.next().is_some()
        || !directory.is_absolute()
        || !directory
            .file_name()
            .and_then(|s| s.to_str())
            .is_some_and(|s| {
                s.starts_with("m4-synthetic-tls-")
                    && s.trim_start_matches("m4-synthetic-tls-")
                        .bytes()
                        .all(|b| b.is_ascii_digit() || b == b'-')
            })
    {
        return Err(());
    }
    let runtime = actual_transport::runtime().map_err(|_| ())?;
    let public = actual_transport::ProductionTls::new().map_err(|_| ())?;
    let mut roots = rustls::RootCertStore::empty();
    roots
        .add(rustls::pki_types::CertificateDer::from(
            fixture_files::read(&directory.join("ca.der")).map_err(|_| ())?,
        ))
        .map_err(|_| ())?;
    let versions = match mode.as_str() {
        "tls12" => &[&rustls::version::TLS12][..],
        "tls13" => &[&rustls::version::TLS13][..],
        _ => rustls::ALL_VERSIONS,
    };
    let mut config = rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_protocol_versions(versions)
    .map_err(|_| ())?
    .with_root_certificates(roots)
    .with_no_client_auth();
    config.resumption = rustls::client::Resumption::disabled();
    config.enable_early_data = false;
    config.enable_sni = true;
    config.alpn_protocols = vec![b"http/1.1".to_vec()];
    if mode == "expired" {
        config.time_provider = Arc::new(FutureTime);
    }
    let config = Arc::new(config);
    let request =
        child_request::receive(&mut std::io::stdin().lock(), &mut std::io::stdout().lock())
            .map_err(|_| ())?;
    let url = url::Url::parse(&request.url).map_err(|_| ())?;
    if url.scheme() != "https" {
        return Err(());
    }
    let host = url
        .host_str()
        .ok_or(())?
        .trim_start_matches('[')
        .trim_end_matches(']');
    let target = if mode == "dns" && host == "localhost" {
        Some(std::net::SocketAddr::from((
            [127, 0, 0, 1],
            url.port_or_known_default().ok_or(())?,
        )))
    } else {
        if !host
            .parse::<std::net::IpAddr>()
            .map_err(|_| ())?
            .is_loopback()
        {
            return Err(());
        }
        None
    };
    let cap = request.body_cap;
    let frame = if mode == "public" {
        runtime.block_on(actual_transport::execute_tls(request, &public))
    } else {
        runtime.block_on(actual_transport::execute_inner(
            request,
            Some(config),
            target,
        ))
    }
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
