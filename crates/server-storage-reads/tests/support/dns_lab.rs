#[allow(dead_code)]
#[path = "tls_fixture.rs"]
pub mod pki;
use server_storage_reads::{
    admission::Admission,
    isolated::{self, NetworkObservation},
    protocol::{Method, Request},
    response_ipc::Reply,
    TEXT_BYTES,
};
use std::{
    net::{SocketAddr, TcpListener},
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
#[derive(Default)]
pub struct Report {
    pub tcp: bool,
    pub tls: bool,
    pub sni: bool,
    pub input: Vec<u8>,
}
pub fn listener(v6: bool, port: u16) -> TcpListener {
    let address = if v6 {
        SocketAddr::from((std::net::Ipv6Addr::LOCALHOST, port))
    } else {
        SocketAddr::from((std::net::Ipv4Addr::LOCALHOST, port))
    };
    let listener = TcpListener::bind(address)
        .unwrap_or_else(|_| panic!("CONTROL IPv6/IPv4 loopback required"));
    listener
        .set_nonblocking(true)
        .unwrap_or_else(|_| unreachable!());
    listener
}
pub async fn serve(
    listener: TcpListener,
    config: Arc<rustls::ServerConfig>,
    byte: u8,
    body_delay: Duration,
) -> Report {
    let mut report = Report::default();
    let deadline = Instant::now() + Duration::from_secs(2);
    let listener = tokio::net::TcpListener::from_std(listener).unwrap_or_else(|_| unreachable!());
    let socket = match timeout_at(deadline, listener.accept()).await {
        Ok(Ok((socket, _))) => socket,
        _ => return report,
    };
    report.tcp = true;
    let mut stream = match timeout_at(
        deadline,
        tokio_rustls::TlsAcceptor::from(config).accept(socket),
    )
    .await
    {
        Ok(Ok(stream)) => stream,
        _ => return report,
    };
    report.tls = true;
    report.sni = stream.get_ref().1.server_name() == Some("localhost");
    let mut scratch = [0; 4096];
    while !report.input.windows(4).any(|b| b == b"\r\n\r\n") {
        match timeout_at(deadline, stream.read(&mut scratch)).await {
            Ok(Ok(0)) => return report,
            Ok(Ok(n)) => {
                assert!(report.input.len() + n <= 65536, "CONTROL input bound");
                report
                    .input
                    .extend_from_slice(scratch.get(..n).unwrap_or_else(|| unreachable!()));
            }
            _ => return report,
        }
    }
    tokio::time::sleep(body_delay).await;
    let mut message = b"HTTP/1.1 200 Fixed\r\nContent-Length: 1\r\n\r\n".to_vec();
    message.push(byte);
    let _ = timeout_at(deadline, stream.write_all(&message)).await;
    let _ = timeout_at(deadline, stream.shutdown()).await;
    report
}
pub fn request(host: &str, port: u16, timeout: Duration) -> Request {
    Request {
        method: Method::Propfind,
        url: format!("https://{host}:{port}/signed%2Fpath?X=a%2Fb"),
        headers: vec![(
            "host".into(),
            Zeroizing::new("different.synthetic.invalid".into()),
        )],
        body_cap: TEXT_BYTES,
        attempt_timeout: timeout,
    }
}
pub fn command(fixture: &pki::Fixture, mode: &str) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_private-dns-fixture"));
    command.arg(mode).arg(&fixture.directory);
    command
        .env("LOCALDOMAIN", "synthetic.invalid")
        .env("RES_OPTIONS", "attempts:9");
    command
}
#[allow(clippy::too_many_arguments)]
pub fn run(
    fixture: &pki::Fixture,
    mode: &str,
    host: &str,
    listener: TcpListener,
    dns_cert: bool,
    timeout: Duration,
    delay: Duration,
    child_deadline_only: bool,
    check: impl FnOnce(&Reply<'_>),
) -> Report {
    let port = listener
        .local_addr()
        .unwrap_or_else(|_| unreachable!())
        .port();
    let config = fixture.server(dns_cert).unwrap_or_else(|_| unreachable!());
    std::thread::scope(|scope| {
        let server = scope.spawn(move || {
            server_storage_reads::child_http::runtime()
                .unwrap_or_else(|_| unreachable!())
                .block_on(serve(listener, config, b'x', delay))
        });
        let admission = Admission::default();
        let permit = admission
            .try_acquire("dns")
            .unwrap_or_else(|_| unreachable!());
        let cancel = AtomicBool::new(false);
        let observe = |_, _| {
            assert!(
                admission.try_acquire("dns").is_err(),
                "CONTROL lease through reap"
            )
        };
        // Independent child deadline witness intentionally permits a larger owner
        // budget. This legacy call is not an owned DNS budget certificate.
        let result = if child_deadline_only {
            isolated::attempt_http_observed(
                command(fixture, mode),
                request(host, port, timeout),
                Duration::from_secs(2),
                &cancel,
                permit,
                observe,
            )
        } else {
            isolated::attempt_dns_observed(
                command(fixture, mode),
                request(host, port, timeout),
                Duration::from_secs(2),
                &cancel,
                permit,
                observe,
            )
        };
        let reply = result.unwrap_or_else(|e| panic!("CONTROL owner {e:?}"));
        check(&reply.view().unwrap_or_else(|_| unreachable!()));
        drop(reply);
        assert!(
            admission.try_acquire("dns").is_ok(),
            "CONTROL released after reap"
        );
        server.join().unwrap_or_else(|_| unreachable!())
    })
}
pub fn stalled(
    fixture: &pki::Fixture,
    cancel_during: bool,
    outer: Duration,
    request_budget: Duration,
) {
    let admission = Admission::default();
    let permit = admission
        .try_acquire("stall")
        .unwrap_or_else(|_| unreachable!());
    let cancel = AtomicBool::new(false);
    let start = std::time::Instant::now();
    let mut before_reap = false;
    let mut saw_ready = false;
    let mut pid = None;
    std::thread::scope(|scope| {
        let canceller = scope.spawn(|| {
            if cancel_during {
                std::thread::sleep(Duration::from_millis(250));
                cancel.store(true, Ordering::Release);
            }
        });
        let result = isolated::attempt_dns_observed(
            command(fixture, "stall"),
            request("localhost", 9, request_budget),
            outer,
            &cancel,
            permit,
            |stage, process_id| {
                pid = Some(process_id);
                assert!(
                    admission.try_acquire("stall").is_err(),
                    "CONTROL stall lease held"
                );
                match stage {
                    NetworkObservation::BeforeReap => before_reap = true,
                    NetworkObservation::OutputProgress(8) => saw_ready = true,
                    _ => {}
                }
            },
        );
        assert!(
            matches!(result, Err(isolated::Error::Cancelled)) && cancel_during
                || matches!(result, Err(isolated::Error::Deadline)) && !cancel_during,
            "CONTROL stall lifecycle"
        );
        canceller.join().unwrap_or_else(|_| unreachable!());
    });
    assert!(
        before_reap && saw_ready,
        "CONTROL READY and held BeforeReap"
    );
    assert!(
        start.elapsed() < Duration::from_secs(2),
        "CONTROL enclosing clamp"
    );
    assert!(
        admission.try_acquire("stall").is_ok(),
        "CONTROL confirmed reap release"
    );
    let pid = rustix::process::Pid::from_raw(
        i32::try_from(pid.unwrap_or_else(|| unreachable!())).unwrap_or_else(|_| unreachable!()),
    )
    .unwrap_or_else(|| unreachable!());
    assert!(
        matches!(
            rustix::process::waitid(
                rustix::process::WaitId::Pid(pid),
                rustix::process::WaitIdOptions::EXITED | rustix::process::WaitIdOptions::NOHANG
            ),
            Err(rustix::io::Errno::CHILD)
        ),
        "CONTROL already reaped"
    );
}
