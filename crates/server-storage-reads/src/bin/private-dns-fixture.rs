//! Separate closed loopback fixture. Production has no resolver or trust switch.
use server_storage_reads::{
    child_request, isolated, protocol, response_ipc, CHUNK_FRAME, CHUNK_PREFIX, ETAG_BYTES,
    LIST_BYTES, TEXT_BYTES,
};
#[allow(dead_code)]
#[path = "../child_http.rs"]
mod actual_transport;
#[path = "support/tls_file.rs"]
mod fixture_files;
use std::{
    net::{Ipv4Addr, Ipv6Addr, SocketAddr},
    path::PathBuf,
    sync::Arc,
    time::Duration,
};
fn run() -> Result<(), ()> {
    let mut args = std::env::args().skip(1);
    let mode = args.next().ok_or(())?;
    if !matches!(
        mode.as_str(),
        "v4" | "v6"
            | "v4-v6"
            | "v6-v4"
            | "32"
            | "33"
            | "empty"
            | "error"
            | "delay"
            | "stall"
            | "public"
            | "literal"
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
    let mut roots = rustls::RootCertStore::empty();
    roots
        .add(rustls::pki_types::CertificateDer::from(
            fixture_files::read(&directory.join("ca.der")).map_err(|_| ())?,
        ))
        .map_err(|_| ())?;
    if mode == "public" {
        roots = rustls::RootCertStore::from_iter(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    }
    let mut config = rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .map_err(|_| ())?
    .with_root_certificates(roots)
    .with_no_client_auth();
    config.resumption = rustls::client::Resumption::disabled();
    config.enable_early_data = false;
    config.enable_sni = true;
    config.alpn_protocols = vec![b"http/1.1".to_vec()];
    let request =
        child_request::receive(&mut std::io::stdin().lock(), &mut std::io::stdout().lock())
            .map_err(|_| ())?;
    let url = url::Url::parse(&request.url).map_err(|_| ())?;
    if !matches!(url.scheme(), "https" | "http") {
        return Err(());
    }
    match url.host() {
        Some(url::Host::Domain("localhost")) if mode != "literal" => {}
        Some(url::Host::Ipv4(ip)) if ip.is_loopback() && mode == "literal" => {}
        Some(url::Host::Ipv6(ip)) if ip.is_loopback() && mode == "literal" => {}
        _ => return Err(()),
    }
    let cap = request.body_cap;
    // This callback executes on this child's main thread before block_on.
    let attempt = actual_transport::prepare_dns_inner(request, Arc::new(config), |host, port| {
        if host != "localhost"
            || tokio::runtime::Handle::try_current().is_ok()
            || std::env::var_os("LOCALDOMAIN").is_some()
            || std::env::var_os("RES_OPTIONS").is_some()
        {
            return Err(std::io::Error::other("fixture boundary"));
        }
        if mode == "stall" {
            use std::io::Write;
            std::io::stdout().lock().flush()?;
            // No stage protocol: fixed main-thread stall, witnessed by held owner.
            std::thread::sleep(Duration::from_secs(5));
        }
        if mode == "delay" {
            std::thread::sleep(Duration::from_millis(200));
        }
        if mode == "error" || mode == "literal" {
            return Err(std::io::Error::other("fixture resolver refused"));
        }
        let v4 = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
        let v6 = SocketAddr::from((Ipv6Addr::LOCALHOST, port));
        let mut results = [None; 33];
        match mode.as_str() {
            "empty" => {}
            "32" | "33" => {
                for slot in results.iter_mut().take(if mode == "32" { 32 } else { 33 }) {
                    *slot = Some(v4);
                }
            }
            "v4-v6" | "v6-v4" => {
                *results
                    .get_mut(0)
                    .ok_or_else(|| std::io::Error::other("slot"))? =
                    Some(if mode == "v4-v6" { v4 } else { v6 });
                *results
                    .get_mut(1)
                    .ok_or_else(|| std::io::Error::other("slot"))? =
                    Some(if mode == "v4-v6" { v6 } else { v4 });
            }
            _ => {
                *results
                    .get_mut(0)
                    .ok_or_else(|| std::io::Error::other("slot"))? =
                    Some(if mode == "v6" { v6 } else { v4 });
            }
        }
        Ok(results.into_iter().flatten())
    })
    .map_err(|_| ())?;
    let frame = runtime
        .block_on(actual_transport::execute_owned_dns(attempt))
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
