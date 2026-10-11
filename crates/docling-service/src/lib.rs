#![forbid(unsafe_code)]
//! Bounded Docling engine transport. HTTP ownership lives in this crate, not Python.
use serde_json::{json, Value};
use std::{path::PathBuf, process::Stdio, time::Duration};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    process::{Child, ChildStdin, ChildStdout, Command},
    time::{timeout_at, Instant},
};

pub const BODY_CAP: usize = 25 * 1024 * 1024;
pub const CONNECTION_CAP: usize = 8;
// Two million Unicode characters can require 24 MB as escaped surrogate pairs.
// 32 MiB also admits the bounded 300-page envelope; larger engine output is refused.
pub const FRAME_CAP: usize = 32 * 1024 * 1024;
pub const FORMATS: &[&str] = &[
    ".bmp", ".csv", ".docx", ".epub", ".htm", ".html", ".jpeg", ".jpg", ".md", ".odp", ".ods",
    ".odt", ".pdf", ".png", ".pptx", ".tif", ".tiff", ".xlsx",
];

#[derive(Clone, Debug)]
pub struct Config {
    pub python: PathBuf,
    pub directory: PathBuf,
    pub long_timeout: Duration,
    pub short_timeout: Duration,
    pub ready_timeout: Duration,
    pub body_timeout: Duration,
    pub header_timeout: Duration,
}
impl Config {
    pub fn production(directory: PathBuf) -> Self {
        Self {
            python: PathBuf::from("python"),
            directory,
            long_timeout: Duration::from_secs(3600),
            short_timeout: Duration::from_secs(600),
            ready_timeout: Duration::from_secs(30),
            body_timeout: Duration::from_secs(30),
            header_timeout: Duration::from_secs(30),
        }
    }
}
#[derive(Debug, PartialEq, Eq)]
pub enum Failure {
    Unavailable,
    Conversion,
    Timeout,
    Cancelled,
}

struct Process {
    child: Child,
    input: ChildStdin,
    output: ChildStdout,
    group: nix::unistd::Pid,
    killed: bool,
}
impl Process {
    fn signal(&mut self) {
        if !self.killed {
            // Only this owned, unreaped child group is targeted. Signal before reap,
            // so its PID cannot already have been recycled by the operating system.
            let _ = nix::sys::signal::killpg(self.group, nix::sys::signal::Signal::SIGKILL);
            self.killed = true;
        }
    }
    async fn stop(mut self) {
        self.signal();
        let _ = tokio::time::timeout(Duration::from_secs(10), self.child.wait()).await;
    }
}
impl Drop for Process {
    fn drop(&mut self) {
        self.signal();
    }
}

pub struct Worker {
    config: Config,
    process: Option<Process>,
}
impl Worker {
    pub fn new(config: Config) -> Self {
        Self {
            config,
            process: None,
        }
    }
    pub async fn stop(&mut self) {
        if let Some(process) = self.process.take() {
            process.stop().await;
        }
    }
    async fn start(&mut self, deadline: Instant) -> Result<(), Failure> {
        let mut child = Command::new(&self.config.python)
            .arg(self.config.directory.join("isolated_worker.py"))
            .args(["--stdout-replies", "extract:extract"])
            .current_dir(&self.config.directory)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .process_group(0)
            .kill_on_drop(true)
            .spawn()
            .map_err(|_| Failure::Unavailable)?;
        let pid = child
            .id()
            .and_then(|n| i32::try_from(n).ok())
            .ok_or(Failure::Unavailable)?;
        let input = child.stdin.take().ok_or(Failure::Unavailable)?;
        let output = child.stdout.take().ok_or(Failure::Unavailable)?;
        let process = Process {
            child,
            input,
            output,
            group: nix::unistd::Pid::from_raw(pid),
            killed: false,
        };
        self.process = Some(process);
        let process = self.process.as_mut().ok_or(Failure::Unavailable)?;
        let ready_deadline = Instant::now()
            .checked_add(self.config.ready_timeout)
            .unwrap_or(deadline);
        let ready = frame(&mut process.output, deadline.min(ready_deadline), 64).await;
        if ready != Ok(json!({"ready":true})) {
            self.stop().await;
            return Err(
                if ready == Err(Failure::Timeout) && deadline <= ready_deadline {
                    Failure::Timeout
                } else {
                    Failure::Unavailable
                },
            );
        }
        Ok(())
    }
    pub async fn convert(
        &mut self,
        path: &std::path::Path,
        name: &str,
        deadline: Instant,
    ) -> Result<Value, Failure> {
        // Probe idle stdout without waiting or reaping. EOF means the warm worker
        // died; unsolicited bytes violate framing. Cleanup precedes a fresh start.
        // Never try_wait/reap before group cleanup: descendants may still exist.
        if let Some(process) = self.process.as_mut() {
            use std::{pin::Pin, task::Poll};
            use tokio::io::{AsyncRead, ReadBuf};
            let stale = std::future::poll_fn(|cx| {
                let mut bytes = [0_u8; 1];
                let mut buffer = ReadBuf::new(&mut bytes);
                Poll::Ready(
                    Pin::new(&mut process.output)
                        .poll_read(cx, &mut buffer)
                        .is_ready(),
                )
            })
            .await;
            if stale {
                self.stop().await;
            }
        }
        if self.process.is_none() {
            self.start(deadline).await?;
        }
        let request = serde_json::to_vec(&json!({"path":path,"name":name}))
            .map_err(|_| Failure::Conversion)?;
        if request.len() > 4096 {
            return Err(Failure::Conversion);
        }
        let process = self.process.as_mut().ok_or(Failure::Unavailable)?;
        let result = async {
            process
                .input
                .write_all(&request)
                .await
                .map_err(|_| Failure::Unavailable)?;
            process
                .input
                .write_all(b"\n")
                .await
                .map_err(|_| Failure::Unavailable)?;
            process
                .input
                .flush()
                .await
                .map_err(|_| Failure::Unavailable)?;
            frame(&mut process.output, deadline, FRAME_CAP).await
        };
        let reply = timeout_at(deadline, result)
            .await
            .map_err(|_| Failure::Timeout)
            .and_then(|v| v);
        match reply {
            Ok(Value::Object(mut object)) if object.len() == 1 && object.contains_key("ok") => {
                object.remove("ok").ok_or(Failure::Conversion)
            }
            Ok(value) if value == json!({"error":true}) => Err(Failure::Conversion),
            Ok(_) => {
                self.stop().await;
                Err(Failure::Conversion)
            }
            Err(error) => {
                self.stop().await;
                Err(error)
            }
        }
    }
}

async fn frame(
    output: &mut (impl tokio::io::AsyncRead + Unpin),
    deadline: Instant,
    cap: usize,
) -> Result<Value, Failure> {
    let mut header = [0_u8; 4];
    timeout_at(deadline, output.read_exact(&mut header))
        .await
        .map_err(|_| Failure::Timeout)?
        .map_err(|_| Failure::Conversion)?;
    let count = usize::try_from(u32::from_be_bytes(header)).map_err(|_| Failure::Conversion)?;
    if count == 0 || count > cap {
        return Err(Failure::Conversion);
    }
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(count)
        .map_err(|_| Failure::Conversion)?;
    bytes.resize(count, 0);
    timeout_at(deadline, output.read_exact(&mut bytes))
        .await
        .map_err(|_| Failure::Timeout)?
        .map_err(|_| Failure::Conversion)?;
    bounded_json(&bytes)
}

fn bounded_json(bytes: &[u8]) -> Result<Value, Failure> {
    // Bound parsed-tree overhead before serde allocates it. Count structural
    // punctuation outside strings; the fixed engine envelope uses < 4,000.
    let mut string = false;
    let mut escaped = false;
    let mut depth = 0usize;
    let mut punctuation = 0usize;
    for byte in bytes {
        if string {
            if escaped {
                escaped = false;
            } else if *byte == b'\\' {
                escaped = true;
            } else if *byte == b'"' {
                string = false;
            }
        } else {
            match byte {
                b'"' => string = true,
                b'{' | b'[' => {
                    depth = depth.checked_add(1).ok_or(Failure::Conversion)?;
                    punctuation += 1;
                }
                b'}' | b']' => {
                    depth = depth.checked_sub(1).ok_or(Failure::Conversion)?;
                    punctuation += 1;
                }
                b',' | b':' => punctuation += 1,
                _ => (),
            }
            if depth > 32 || punctuation > 20_000 {
                return Err(Failure::Conversion);
            }
        }
    }
    serde_json::from_slice(bytes).map_err(|_| Failure::Conversion)
}

pub async fn serve(listener: tokio::net::TcpListener, config: Config) -> std::io::Result<()> {
    serve_with_shutdown(listener, config, std::future::pending()).await
}
pub async fn serve_with_shutdown(
    listener: tokio::net::TcpListener,
    config: Config,
    shutdown: impl std::future::Future<Output = ()>,
) -> std::io::Result<()> {
    use std::sync::Arc;
    let connections = Arc::new(tokio::sync::Semaphore::new(CONNECTION_CAP));
    let header_timeout = config.header_timeout;
    let worker = Arc::new(tokio::sync::Mutex::new(Worker::new(config)));
    let mut tasks = tokio::task::JoinSet::new();
    tokio::pin!(shutdown);
    loop {
        let accepted = tokio::select! {
            _=&mut shutdown => break,
            done=tasks.join_next(), if !tasks.is_empty() => {let _=done;continue;},
            accepted=listener.accept()=>accepted,
        };
        let (socket, _) = accepted?;
        let Ok(permit) = connections.clone().try_acquire_owned() else {
            drop(socket);
            continue;
        };
        let worker = worker.clone();
        tasks.spawn(async move {
            let _permit = permit;
            let _ = connection(socket, worker, header_timeout).await;
        });
    }
    tasks.abort_all();
    while tasks.join_next().await.is_some() {}
    worker.lock().await.stop().await;
    Ok(())
}

const INVALID: &str = "Invalid document size or name.";
const BUSY: &str = "Document extraction is busy; refresh to retry.";
const UNAVAILABLE: &str = "Document extraction is unavailable; refresh to retry.";
const TIMEOUT: &str =
    "This document took longer than its processing time limit to read; the original is retained.";
const CONVERSION: &str =
    "This document could not be read within its processing limits; the original is retained.";

struct Headers {
    method: String,
    path: String,
    length: Option<String>,
    name: Option<String>,
}
async fn headers(socket: &mut tokio::net::TcpStream) -> Result<Headers, ()> {
    // Read only the header, never speculative body bytes before admission. Bounded
    // one-byte reads also keep EOF monitoring independent of a buffering HTTP layer.
    let mut bytes = Vec::new();
    bytes.try_reserve_exact(16 * 1024).map_err(|_| ())?;
    while !bytes.ends_with(b"\r\n\r\n") {
        if bytes.len() == 16 * 1024 {
            return Err(());
        }
        bytes.push(socket.read_u8().await.map_err(|_| ())?);
    }
    let mut text = String::new();
    text.try_reserve_exact(bytes.len().checked_mul(2).ok_or(())?)
        .map_err(|_| ())?;
    for byte in bytes {
        text.push(char::from(byte));
    }
    let mut lines = text.split("\r\n");
    let mut request = lines.next().ok_or(())?.split_whitespace();
    let method = request.next().ok_or(())?.to_owned();
    let path = request.next().ok_or(())?.to_owned();
    let path = if path.starts_with("//") {
        format!("/{}", path.trim_start_matches('/'))
    } else {
        path
    };
    if !matches!(request.next(), Some("HTTP/1.0" | "HTTP/1.1")) || request.next().is_some() {
        return Err(());
    }
    let mut result = Headers {
        method,
        path,
        length: None,
        name: None,
    };
    for (count, line) in lines.filter(|line| !line.is_empty()).enumerate() {
        if count >= 99 {
            return Err(());
        }
        let (key, value) = line.split_once(':').ok_or(())?;
        if key.eq_ignore_ascii_case("content-length") && result.length.is_none() {
            result.length = Some(value.trim_start_matches([' ', '\t']).to_owned());
        }
        if key.eq_ignore_ascii_case("x-document-name") && result.name.is_none() {
            result.name = Some(value.trim_start_matches([' ', '\t']).to_owned());
        }
        if key.eq_ignore_ascii_case("transfer-encoding") {
            return Err(());
        }
    }
    Ok(result)
}
fn selection(headers: &Headers) -> Option<(usize, &str, String)> {
    let length = headers.length.as_deref().unwrap_or("0").trim();
    let length = length.strip_prefix('+').unwrap_or(length);
    let mut size = 0usize;
    let mut previous_digit = false;
    for byte in length.bytes() {
        if byte == b'_' && previous_digit {
            previous_digit = false;
            continue;
        }
        if !byte.is_ascii_digit() {
            return None;
        }
        size = size
            .checked_mul(10)?
            .checked_add(usize::from(byte - b'0'))?;
        if size > BODY_CAP {
            return None;
        }
        previous_digit = true;
    }
    if !previous_digit {
        return None;
    }
    let name = headers.name.as_deref()?;
    if size == 0
        || size > BODY_CAP
        || name.is_empty()
        || name.chars().count() > 200
        || name.contains(['/', '\\'])
    {
        return None;
    }
    let suffix = name
        .rsplit_once('.')
        .map(|(_, suffix)| format!(".{}", suffix.to_lowercase()))
        .unwrap_or_default();
    Some((size, name, suffix))
}
async fn reply(
    socket: &mut tokio::net::TcpStream,
    status: u16,
    body: Value,
) -> std::io::Result<()> {
    let bytes = serde_json::to_vec(&body)?;
    reply_bytes(socket, status, "application/json", &bytes, false).await
}
async fn reply_bytes(
    socket: &mut tokio::net::TcpStream,
    status: u16,
    content_type: &str,
    bytes: &[u8],
    omit: bool,
) -> std::io::Result<()> {
    let reason = match status {
        200 => "OK",
        400 => "Bad Request",
        404 => "Not Found",
        415 => "Unsupported Media Type",
        422 => "Unprocessable Entity",
        503 => "Service Unavailable",
        501 => "Not Implemented",
        _ => "Error",
    };
    let header = format!("HTTP/1.0 {status} {reason}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", bytes.len());
    let mut response = Vec::new();
    response
        .try_reserve_exact(
            header
                .len()
                .checked_add(bytes.len())
                .ok_or_else(|| std::io::Error::other("response limit"))?,
        )
        .map_err(|_| std::io::Error::other("response allocation"))?;
    response.extend_from_slice(header.as_bytes());
    if !omit {
        response.extend_from_slice(bytes);
    }
    tokio::time::timeout(Duration::from_secs(30), socket.write_all(&response))
        .await
        .map_err(|_| std::io::Error::other("response deadline"))?
}
async fn connection(
    mut socket: tokio::net::TcpStream,
    worker: std::sync::Arc<tokio::sync::Mutex<Worker>>,
    header_timeout: Duration,
) -> std::io::Result<()> {
    let parsed = tokio::time::timeout(header_timeout, headers(&mut socket)).await;
    let Ok(Ok(headers)) = parsed else {
        return reply(&mut socket, 400, json!({"error":INVALID})).await;
    };
    if headers.method == "GET" {
        return reply(
            &mut socket,
            if headers.path == "/health" { 200 } else { 404 },
            if headers.path == "/health" {
                json!({"service":"docling","formats":FORMATS})
            } else {
                json!({"service":"docling"})
            },
        )
        .await;
    }
    if headers.method != "POST" {
        let method = headers
            .method
            .replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;");
        let body=format!("<!DOCTYPE HTML>\n<html lang=\"en\">\n    <head>\n        <meta charset=\"utf-8\">\n        <title>Error response</title>\n    </head>\n    <body>\n        <h1>Error response</h1>\n        <p>Error code: 501</p>\n        <p>Message: Unsupported method ('{method}').</p>\n        <p>Error code explanation: 501 - Server does not support this operation.</p>\n    </body>\n</html>\n");
        return reply_bytes(
            &mut socket,
            501,
            "text/html;charset=utf-8",
            body.as_bytes(),
            headers.method == "HEAD",
        )
        .await;
    }
    if headers.path != "/extract" {
        return reply(&mut socket, 404, json!({"error":"not found"})).await;
    }
    let Some((size, name, suffix)) = selection(&headers) else {
        return reply(&mut socket, 400, json!({"error":INVALID})).await;
    };
    let Ok(mut worker) = worker.try_lock() else {
        return reply(&mut socket, 503, json!({"error":BUSY})).await;
    };
    let Some(body_deadline) = Instant::now().checked_add(worker.config.body_timeout) else {
        drop(worker);
        return reply(&mut socket, 503, json!({"error":UNAVAILABLE})).await;
    };
    let mut data = Vec::new();
    if data.try_reserve_exact(size).is_err() {
        drop(worker);
        return reply(&mut socket, 503, json!({"error":UNAVAILABLE})).await;
    }
    data.resize(size, 0);
    if !matches!(
        timeout_at(body_deadline, socket.read_exact(&mut data)).await,
        Ok(Ok(_))
    ) {
        drop(worker);
        return reply(
            &mut socket,
            400,
            json!({"error":"Incomplete document body."}),
        )
        .await;
    }
    if !FORMATS.contains(&suffix.as_str()) {
        drop(worker);
        return reply(&mut socket, 415, json!({"error":format!("This worker cannot read {}.", if suffix.is_empty() {"files without an extension"} else {&suffix})})).await;
    }
    let directory = tempfile::Builder::new()
        .prefix("noevia-docling-")
        .tempdir()?;
    let path = directory.path().join(format!("input{suffix}"));
    if !matches!(
        timeout_at(body_deadline, tokio::fs::write(&path, &data)).await,
        Ok(Ok(()))
    ) {
        drop(worker);
        return reply(&mut socket, 503, json!({"error":UNAVAILABLE})).await;
    }
    drop(data);
    let duration = if matches!(suffix.as_str(), ".pdf" | ".tif" | ".tiff") {
        worker.config.long_timeout
    } else {
        worker.config.short_timeout
    };
    let Some(deadline) = Instant::now().checked_add(duration) else {
        drop(worker);
        return reply(&mut socket, 503, json!({"error":UNAVAILABLE})).await;
    };
    // A direct socket read observes full close AND shutdown(SHUT_WR), even while
    // conversion has no HTTP body left to read. Extra pipelined bytes are refused.
    let mut probe = [0_u8; 1];
    let outcome = tokio::select! {
        biased;
        _ = socket.read(&mut probe) => Err(Failure::Cancelled),
        result = worker.convert(&path, name, deadline) => result,
    };
    if outcome == Err(Failure::Cancelled) {
        worker.stop().await;
    }
    drop(directory);
    drop(worker); // Release the single conversion slot BEFORE writing any answer.
    match outcome {
        Ok(body) => reply(&mut socket, 200, body).await,
        Err(Failure::Cancelled) => Ok(()),
        Err(error) => reply(&mut socket, match error {Failure::Unavailable => 503, _ => 422}, json!({"error":match error {Failure::Unavailable => UNAVAILABLE, Failure::Timeout => TIMEOUT, _ => CONVERSION}})).await,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn fragmented_truncated_and_oversized_frames() {
        let payload = br#"{"ready":true}"#;
        let (mut writer, mut reader) = tokio::io::duplex(2);
        let job = tokio::spawn(async move {
            for byte in (payload.len() as u32)
                .to_be_bytes()
                .into_iter()
                .chain(payload.iter().copied())
            {
                writer
                    .write_all(&[byte])
                    .await
                    .unwrap_or_else(|e| unreachable!("{e}"));
            }
        });
        assert_eq!(
            frame(&mut reader, Instant::now() + Duration::from_secs(1), 64).await,
            Ok(json!({"ready":true}))
        );
        job.await.unwrap_or_else(|e| unreachable!("{e}"));
        for bytes in [
            vec![0, 0],
            vec![0, 0, 0, 10, b'{'],
            vec![0, 0, 1, 0],
            vec![0, 0, 0, 0],
            vec![0, 0, 0, 1, b'x'],
        ] {
            assert_eq!(
                frame(
                    &mut bytes.as_slice(),
                    Instant::now() + Duration::from_secs(1),
                    64
                )
                .await,
                Err(Failure::Conversion)
            );
        }
    }
    proptest::proptest! {
        #[test]
        fn arbitrary_names_never_escape_input_bounds(name in ".{0,512}", length in 0usize..BODY_CAP+2) {
            let headers=Headers{method:"POST".into(),path:"/extract".into(),length:Some(length.to_string()),name:Some(name)};
            if let Some((size,name,suffix))=selection(&headers) {
                proptest::prop_assert!((1..=BODY_CAP).contains(&size));
                proptest::prop_assert!(!name.is_empty() && name.chars().count()<=200 && !name.contains(['/', '\\']));
                proptest::prop_assert!(suffix.is_empty() || suffix.starts_with('.'));
            }
        }
    }
}
