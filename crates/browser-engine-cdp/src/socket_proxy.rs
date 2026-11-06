//! Session-local WebSocket gate. Denials happen before opening any upstream socket.
//! HTTP(S) shares the gate and forwards through the configured egress proxy.
use super::*;
use base64::Engine;
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    task::JoinSet,
};
trait Stream: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> Stream for T {}
type Socket = Box<dyn Stream>;
#[derive(Debug)]
struct TransportFailure(&'static str);
impl std::fmt::Display for TransportFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.0)
    }
}
impl std::error::Error for TransportFailure {}
struct FailureRecord {
    generation: u64,
    authority: String,
    code: &'static str,
}
type Failures = Arc<tokio::sync::Mutex<Vec<FailureRecord>>>;
pub(super) struct SocketProxy {
    pub address: String,
    failures: Failures,
    generation: Arc<std::sync::atomic::AtomicU64>,
    pub credentials: ProxyCredentials,
    task: JoinHandle<()>,
    pub blocked: Arc<tokio::sync::Mutex<Vec<Value>>>,
}
impl SocketProxy {
    pub fn next_navigation(&self) -> u64 {
        self.generation
            .fetch_add(1, Ordering::SeqCst)
            .wrapping_add(1)
    }
    pub async fn failure(&self, generation: u64, url: &str) -> Option<&'static str> {
        let authority = origin(url);
        self.failures
            .lock()
            .await
            .iter()
            .rev()
            .find(|v| v.generation == generation && v.authority == authority)
            .map(|v| v.code)
    }
    pub fn stop(&self) {
        self.task.abort();
    }
    pub fn abort_handle(&self) -> tokio::task::AbortHandle {
        self.task.abort_handle()
    }
}
impl Drop for SocketProxy {
    fn drop(&mut self) {
        self.task.abort();
    }
}
impl SocketProxy {
    pub async fn start(config: &Launch) -> Result<Self> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let address = format!("http://{}", listener.local_addr()?);
        let mut entropy = [0u8; 32];
        std::io::Read::read_exact(&mut std::fs::File::open("/dev/urandom")?, &mut entropy)?;
        let password = base64::engine::general_purpose::STANDARD.encode(entropy);
        let credentials = ProxyCredentials {
            username: "session".into(),
            password: password.clone(),
        };
        let authorization = format!(
            "Basic {}",
            base64::engine::general_purpose::STANDARD.encode(format!("session:{password}"))
        );
        let blocked = Arc::new(tokio::sync::Mutex::new(Vec::new()));
        let refused = blocked.clone();
        let failures = Arc::new(tokio::sync::Mutex::new(Vec::new()));
        let failures_task = failures.clone();
        let generation = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let generation_task = generation.clone();
        let config = config.clone();
        let task = tokio::spawn(async move {
            let mut connections = JoinSet::new();
            loop {
                tokio::select! {
                    incoming=listener.accept()=>{let Ok((socket,_))=incoming else {break};let config=config.clone();let authorization=authorization.clone();let refused=refused.clone();let failures=failures_task.clone();let generation=generation_task.clone();connections.spawn(async move {let _=serve(socket,&config,&authorization,&refused,&failures,&generation).await;});},
                    _=connections.join_next(), if !connections.is_empty()=>{}
                }
            }
        });
        Ok(Self {
            address,
            failures,
            generation,
            credentials,
            blocked,
            task,
        })
    }
}
async fn headers(socket: &mut (impl AsyncRead + Unpin)) -> Result<String> {
    let mut bytes = Vec::new();
    loop {
        let byte = socket.read_u8().await?;
        bytes.push(byte);
        if bytes.ends_with(b"\r\n\r\n") {
            break;
        }
        if bytes.len() > 32768 {
            bail!("Proxy headers too large")
        }
    }
    Ok(String::from_utf8(bytes)?)
}
async fn upstream(config: &Launch, host: &str, port: u16) -> Result<(Socket, bool)> {
    let Some(server) = &config.proxy else {
        // The resolver switch is allowed only for the disposable synthetic fixture.
        let fixture = config.synthetic_direct
            && host.ends_with(".test")
            && config
                .args
                .iter()
                .any(|s| s == "--host-resolver-rules=MAP *.test 127.0.0.1");
        return Ok((
            Box::new(TcpStream::connect((if fixture { "127.0.0.1" } else { host }, port)).await?),
            false,
        ));
    };
    let proxy = url::Url::parse(server)?;
    let hostname = proxy
        .host_str()
        .ok_or_else(|| anyhow!("Proxy host missing"))?;
    let tcp = TcpStream::connect((hostname, proxy.port_or_known_default().unwrap_or(1080))).await?;
    if proxy.scheme() == "socks5" {
        let mut tcp = tcp;
        tcp.write_all(&[5, 1, 0]).await?;
        let mut reply = [0; 2];
        tcp.read_exact(&mut reply).await?;
        if reply != [5, 0] {
            return Err(anyhow!(TransportFailure(
                "net::ERR_SOCKS_CONNECTION_FAILED"
            )));
        }
        let length = u8::try_from(host.len())?;
        let mut request = vec![5, 1, 0, 3, length];
        request.extend(host.as_bytes());
        request.extend(port.to_be_bytes());
        tcp.write_all(&request).await?;
        let mut reply = [0; 4];
        tcp.read_exact(&mut reply).await?;
        if reply.get(1) != Some(&0) {
            return Err(anyhow!(TransportFailure(
                "net::ERR_SOCKS_CONNECTION_FAILED"
            )));
        }
        let length = match reply.get(3) {
            Some(1) => 4,
            Some(4) => 16,
            Some(3) => usize::from(tcp.read_u8().await?),
            _ => bail!("Invalid proxy response"),
        };
        let mut address = vec![0; length + 2];
        tcp.read_exact(&mut address).await?;
        return Ok((Box::new(tcp), false));
    }
    let socket: Socket = if proxy.scheme() == "https" {
        let mut roots = tokio_rustls::rustls::RootCertStore::from_iter(
            webpki_roots::TLS_SERVER_ROOTS.iter().cloned(),
        );
        if let Some(path) = &config.synthetic_proxy_ca {
            let certificate = tokio::fs::read(path)
                .await
                .context("Fixture certificate unavailable")?;
            if certificate.len() > 65536 {
                bail!("Fixture certificate too large")
            }
            roots
                .add(tokio_rustls::rustls::pki_types::CertificateDer::from(
                    certificate,
                ))
                .map_err(|_| anyhow!("Fixture certificate invalid"))?;
        }
        let tls = tokio_rustls::rustls::ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth();
        let name = tokio_rustls::rustls::pki_types::ServerName::try_from(hostname.to_owned())?;
        Box::new(
            tokio_rustls::TlsConnector::from(Arc::new(tls))
                .connect(name, tcp)
                .await?,
        )
    } else {
        Box::new(tcp)
    };
    Ok((socket, true))
}
async fn serve(
    mut client: TcpStream,
    config: &Launch,
    authorization: &str,
    refused: &tokio::sync::Mutex<Vec<Value>>,
    failures: &Failures,
    generation: &std::sync::atomic::AtomicU64,
) -> Result<()> {
    let request = timeout(LIMIT, headers(&mut client)).await??;
    if !request.lines().skip(1).any(|line| {
        line.split_once(':').is_some_and(|(name, value)| {
            name.eq_ignore_ascii_case("proxy-authorization") && value.trim() == authorization
        })
    }) {
        client.write_all(b"HTTP/1.1 407 Proxy Authentication Required\r\nProxy-Authenticate: Basic realm=\"session\"\r\nContent-Length: 0\r\n\r\n").await?;
        return Ok(());
    }
    let line = request
        .lines()
        .next()
        .ok_or_else(|| anyhow!("Missing request"))?;
    let mut parts = line.split_whitespace();
    let method = parts.next().unwrap_or("");
    let target = parts.next().unwrap_or("");
    let tunnel = method == "CONNECT";
    let url = url::Url::parse(&if tunnel {
        format!("https://{target}/")
    } else {
        target.to_owned()
    })?;
    if !matches!(url.scheme(), "http" | "https" | "ws" | "wss")
        || !navigation(
            &format!(
                "{}://{}",
                if tunnel { "https" } else { "http" },
                url.as_str()
                    .split_once("://")
                    .map(|(_, tail)| tail)
                    .unwrap_or("")
            ),
            &config.allowed_domains,
        )
    {
        let mut blocked = refused.lock().await;
        if blocked.len() < 100 {
            blocked.push(json!({"url":mask(url.as_str(),&config.secrets).chars().take(300).collect::<String>(),"reason":"Not on this task’s allowed list.","source":"proxy"}));
        }
        drop(blocked);
        client
            .write_all(b"HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
            .await?;
        return Ok(());
    }
    let host = url
        .host_str()
        .ok_or_else(|| anyhow!("Missing destination"))?;
    let port = url
        .port_or_known_default()
        .ok_or_else(|| anyhow!("Missing port"))?;
    let current = generation.load(Ordering::SeqCst);
    let (mut remote, proxy) = match timeout(LIMIT, upstream(config, host, port)).await {
        Ok(Ok(pair)) => pair,
        failure => {
            let code = match failure {
                Ok(Err(error)) => error
                    .downcast_ref::<TransportFailure>()
                    .map(|v| v.0)
                    .unwrap_or_else(|| {
                        let certificate = error
                            .downcast_ref::<std::io::Error>()
                            .and_then(|v| v.get_ref())
                            .and_then(|v| v.downcast_ref::<tokio_rustls::rustls::Error>())
                            .is_some_and(|v| {
                                matches!(v, tokio_rustls::rustls::Error::InvalidCertificate(_))
                            });
                        if certificate {
                            "net::ERR_PROXY_CERTIFICATE_INVALID"
                        } else if config.proxy.is_some() {
                            "net::ERR_PROXY_CONNECTION_FAILED"
                        } else {
                            "net::ERR_CONNECTION_REFUSED"
                        }
                    }),
                _ => "net::ERR_CONNECTION_TIMED_OUT",
            };
            record_failure(failures, current, &url, code).await;
            return Ok(());
        }
    };
    let auth = if proxy {
        config
            .proxy_credentials
            .as_ref()
            .map(|c| {
                format!(
                    "Proxy-Authorization: Basic {}\r\n",
                    base64::engine::general_purpose::STANDARD
                        .encode(format!("{}:{}", c.username, c.password))
                )
            })
            .unwrap_or_default()
    } else {
        String::new()
    };
    if tunnel {
        if proxy {
            remote
                .write_all(
                    format!("CONNECT {target} HTTP/1.1\r\nHost: {target}\r\n{auth}\r\n").as_bytes(),
                )
                .await?;
            let response = timeout(LIMIT, headers(&mut remote)).await??;
            if response.split_whitespace().nth(1) != Some("200") {
                if response.split_whitespace().nth(1) == Some("407") {
                    record_failure(
                        failures,
                        current,
                        &url,
                        "net::ERR_HTTP_RESPONSE_CODE_FAILURE",
                    )
                    .await;
                }
                client
                    .write_all(b"HTTP/1.1 502 Bad Gateway\r\nContent-Length: 0\r\n\r\n")
                    .await?;
                return Ok(());
            }
        }
        client
            .write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
            .await?;
    } else {
        let target = if proxy {
            target.to_owned()
        } else {
            format!(
                "{}{}",
                url.path(),
                url.query().map(|q| format!("?{q}")).unwrap_or_default()
            )
        };
        let upgrade = request.lines().any(|line| {
            line.split_once(':').is_some_and(|(name, value)| {
                name.eq_ignore_ascii_case("upgrade")
                    && value.trim().eq_ignore_ascii_case("websocket")
            })
        });
        let mut forwarded = format!("{method} {target} HTTP/1.1\r\n");
        for header in request.lines().skip(1).filter(|s| !s.is_empty()) {
            let name = header.split(':').next().unwrap_or("");
            if !name.eq_ignore_ascii_case("proxy-authorization")
                && !name.eq_ignore_ascii_case("proxy-connection")
                && (upgrade || !name.eq_ignore_ascii_case("connection"))
            {
                forwarded.push_str(header);
                forwarded.push_str("\r\n");
            }
        }
        if !upgrade {
            forwarded.push_str("Connection: close\r\n");
        }
        forwarded.push_str(&auth);
        forwarded.push_str("\r\n");
        remote.write_all(forwarded.as_bytes()).await?;
    }
    tokio::io::copy_bidirectional(&mut client, &mut remote).await?;
    Ok(())
}

async fn record_failure(failures: &Failures, generation: u64, url: &url::Url, code: &'static str) {
    let mut records = failures.lock().await;
    if records.len() >= 100 {
        records.remove(0);
    }
    records.push(FailureRecord {
        generation,
        authority: url.origin().ascii_serialization(),
        code,
    });
}
