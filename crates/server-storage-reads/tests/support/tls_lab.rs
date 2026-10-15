#[allow(dead_code)]
#[path = "tls_fixture.rs"]
pub mod pki;
use server_storage_reads::{
    admission::Admission,
    isolated::{self, NetworkObservation},
    protocol::{Method, Request},
    response_ipc::Reply,
};
use std::{
    net::TcpListener,
    process::Command,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    time::{timeout_at, Instant},
};
use zeroize::Zeroizing;
pub struct Report {
    pub input: Vec<u8>,
    pub accepted: bool,
    pub identity: bool,
}
pub(crate) async fn serve(
    listener: TcpListener,
    config: Arc<rustls::ServerConfig>,
    dns: bool,
    message: Vec<u8>,
    stall: bool,
) -> Report {
    let mut report = Report {
        input: Vec::new(),
        accepted: false,
        identity: false,
    };
    let deadline = Instant::now() + Duration::from_secs(18);
    let listener = tokio::net::TcpListener::from_std(listener).unwrap_or_else(|_| unreachable!());
    let socket = match timeout_at(deadline, listener.accept()).await {
        Ok(Ok((s, _))) => s,
        _ => return report,
    };
    if stall {
        tokio::time::sleep(Duration::from_secs(2)).await;
        return report;
    }
    let mut stream = match timeout_at(
        deadline,
        tokio_rustls::TlsAcceptor::from(config).accept(socket),
    )
    .await
    {
        Ok(Ok(s)) => s,
        _ => return report,
    };
    report.accepted = true;
    report.identity = if dns {
        stream.get_ref().1.server_name() == Some("localhost")
    } else {
        stream.get_ref().1.server_name().is_none()
    };
    report
        .input
        .try_reserve_exact(65536)
        .unwrap_or_else(|_| unreachable!());
    let mut scratch = [0; 4096];
    while !report.input.windows(4).any(|b| b == b"\r\n\r\n") {
        match timeout_at(deadline, stream.read(&mut scratch)).await {
            Ok(Ok(0)) => return report,
            Ok(Ok(n)) => {
                assert!(report.input.len() + n <= 65536, "CONTROL fixture input");
                report
                    .input
                    .extend_from_slice(scratch.get(..n).unwrap_or_else(|| unreachable!()));
            }
            _ => return report,
        }
    }
    for part in message.chunks(4096) {
        if !matches!(
            timeout_at(deadline, stream.write_all(part)).await,
            Ok(Ok(()))
        ) {
            return report;
        }
    }
    let _ = timeout_at(deadline, stream.shutdown()).await;
    report
}
pub fn response(size: usize) -> Vec<u8> {
    let mut bytes = format!("HTTP/1.1 200 Fixed\r\nContent-Length: {size}\r\n\r\n").into_bytes();
    bytes.resize(bytes.len() + size, b'x');
    bytes
}
#[allow(clippy::too_many_arguments)] // Fixed, synthetic-only fixture choices.
pub fn run(
    fixture: &pki::Fixture,
    mode: &str,
    dns_cert: bool,
    dns_url: bool,
    body: Vec<u8>,
    cap: usize,
    timeout: Duration,
    cancel: bool,
    stall: bool,
    check: impl FnOnce(&Reply<'_>),
) -> Report {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap_or_else(|_| unreachable!());
    listener
        .set_nonblocking(true)
        .unwrap_or_else(|_| unreachable!());
    let address = listener.local_addr().unwrap_or_else(|_| unreachable!());
    let config = fixture.server(dns_cert).unwrap_or_else(|_| unreachable!());
    std::thread::scope(|scope| {
        let server = scope.spawn(move || {
            server_storage_reads::child_http::runtime()
                .unwrap_or_else(|_| unreachable!())
                .block_on(serve(listener, config, dns_url, body, stall))
        });
        let a = Admission::default();
        let p = a.try_acquire("fixture").unwrap_or_else(|_| unreachable!());
        let cancelled = AtomicBool::new(false);
        let mut seen = false;
        let mut cleanup = false;
        let mut command = Command::new(env!("CARGO_BIN_EXE_private-tls-fixture"));
        command.arg(mode).arg(&fixture.directory);
        let request = Request {
            method: if dns_url {
                Method::Propfind
            } else {
                Method::Get
            },
            url: format!(
                "https://{}:{}/signed%2Fpath?X=a%2Fb",
                if dns_url { "localhost" } else { "127.0.0.1" },
                address.port()
            ),
            headers: vec![(
                "host".into(),
                Zeroizing::new("different.synthetic.invalid".into()),
            )],
            body_cap: cap,
            attempt_timeout: timeout,
        };
        let result = isolated::attempt_http_observed(
            command,
            request,
            Duration::from_secs(16),
            &cancelled,
            p,
            |s, _| {
                assert!(a.try_acquire("fixture").is_err(), "CONTROL lease held");
                match s {
                    NetworkObservation::OutputProgress(n) if cancel && n > 38 => {
                        seen = true;
                        cancelled.store(true, Ordering::Release)
                    }
                    NetworkObservation::BeforeReap => cleanup = true,
                    _ => {}
                }
            },
        );
        if cancel {
            assert!(matches!(result, Err(isolated::Error::Cancelled)));
            assert!(seen && cleanup);
        } else {
            let reply = result.unwrap_or_else(|_| unreachable!());
            check(&reply.view().unwrap_or_else(|_| unreachable!()));
        }
        assert!(a.try_acquire("fixture").is_ok());
        server.join().unwrap_or_else(|_| unreachable!())
    })
}
