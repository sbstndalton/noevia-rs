//! The HTTP/1.1 exchange with the proxy: one `POST` per connection, `connection: close`, the
//! response read raw so the issuer, not an HTTP library, decides which headers are acceptable.
use crate::wire::{endpoint_address, MAX_RESPONSE};
use crate::{err, ControlError, Reason};
use std::future::Future;
use std::pin::Pin;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

pub type BoxFuture<T> = Pin<Box<dyn Future<Output = T> + Send>>;

pub struct TransportRequest {
    pub endpoint: String,
    pub route: &'static str,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
    pub timeout_ms: f64,
}

pub struct Reply {
    pub status: u16,
    pub raw_headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

pub trait Transport: Send + Sync + 'static {
    fn post(&self, request: TransportRequest) -> BoxFuture<Result<Reply, ControlError>>;
}

const MAX_HEAD: usize = 16 * 1024 + 256;

#[derive(Debug, Default, Clone, Copy)]
pub struct TcpTransport;

impl Transport for TcpTransport {
    fn post(&self, request: TransportRequest) -> BoxFuture<Result<Reply, ControlError>> {
        Box::pin(async move {
            let (host, port) = endpoint_address(&request.endpoint)?;
            let wait = Duration::from_secs_f64((request.timeout_ms.max(1.0)) / 1000.0);
            match tokio::time::timeout(wait, exchange(&host, port, &request)).await {
                Ok(done) => done,
                Err(_) => err(Reason::Timeout),
            }
        })
    }
}

async fn exchange(
    host: &str,
    port: u16,
    request: &TransportRequest,
) -> Result<Reply, ControlError> {
    let transport = || ControlError(Reason::Transport);
    let mut stream = tokio::net::TcpStream::connect((host, port))
        .await
        .map_err(|_| transport())?;
    let mut head = format!("POST {} HTTP/1.1\r\nhost: {host}:{port}\r\n", request.route);
    for (name, value) in &request.headers {
        if name.bytes().any(|c| c.is_ascii_control() || c == b':')
            || value.bytes().any(|c| c == b'\r' || c == b'\n')
        {
            return err(Reason::Request);
        }
        head.push_str(&format!("{name}: {value}\r\n"));
    }
    head.push_str("\r\n");
    stream
        .write_all(head.as_bytes())
        .await
        .map_err(|_| transport())?;
    stream
        .write_all(&request.body)
        .await
        .map_err(|_| transport())?;
    let mut data: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 8192];
    let boundary = loop {
        if let Some(at) = data.windows(4).position(|w| w == b"\r\n\r\n") {
            break at;
        }
        if data.len() > MAX_HEAD {
            return err(Reason::Response);
        }
        let n = stream.read(&mut chunk).await.map_err(|_| transport())?;
        if n == 0 {
            return err(Reason::Transport);
        }
        data.extend_from_slice(chunk.get(..n).unwrap_or(&[]));
    };
    let head_text =
        std::str::from_utf8(data.get(..boundary).unwrap_or(&[])).map_err(|_| transport())?;
    let mut lines = head_text.split("\r\n");
    let status_line = lines.next().ok_or_else(transport)?;
    let mut parts = status_line.split(' ');
    if !parts.next().is_some_and(|v| v.starts_with("HTTP/1.")) {
        return err(Reason::Transport);
    }
    let status: u16 = parts
        .next()
        .and_then(|s| s.parse().ok())
        .ok_or_else(transport)?;
    let mut raw_headers = Vec::new();
    for line in lines {
        let (name, value) = line.split_once(':').ok_or_else(transport)?;
        raw_headers.push((name.to_string(), value.trim().to_string()));
    }
    let declared = raw_headers
        .iter()
        .find(|(n, _)| n.eq_ignore_ascii_case("content-length"))
        .and_then(|(_, v)| v.parse::<usize>().ok());
    let mut body = data.get(boundary + 4..).unwrap_or(&[]).to_vec();
    loop {
        if body.len() > MAX_RESPONSE {
            return err(Reason::Response);
        }
        if declared.is_some_and(|d| body.len() >= d) {
            break;
        }
        let n = stream.read(&mut chunk).await.map_err(|_| transport())?;
        if n == 0 {
            // Truncated: the declared length was not delivered (Node's `aborted`).
            if declared.is_some_and(|d| body.len() < d) {
                return err(Reason::Transport);
            }
            break;
        }
        body.extend_from_slice(chunk.get(..n).unwrap_or(&[]));
    }
    if let Some(d) = declared {
        body.truncate(d);
    }
    Ok(Reply {
        status,
        raw_headers,
        body,
    })
}
