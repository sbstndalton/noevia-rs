//! code-verify.cjs `connectTo` and `askVerifier`: one request to the verifier, one answer line.

use crate::text::Utf8Stream;
use code_acp::Cancel;
use js_json::{wtf16, JValue};
use std::io::{self, Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::os::unix::net::UnixStream;
use std::sync::mpsc;
use std::time::{Duration, Instant};

/// The most the verifier's answer may be, in bytes.
pub const MAX_ANSWER_BYTES: usize = 2 * 1024 * 1024;
/// How often a blocked read, write or connect looks at the cancel and the deadline.
const SLICE: Duration = Duration::from_millis(10);
/// A close with no answer sooner than this after connecting is "not listening".
const QUICK_CLOSE: Duration = Duration::from_millis(1000);

/// Why one ask failed: Node's `error.code`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AskCode {
    Cancelled,
    Timeout,
    BadAnswer,
    NotListening,
    Unreachable,
}

impl AskCode {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            AskCode::Cancelled => "cancelled",
            AskCode::Timeout => "timeout",
            AskCode::BadAnswer => "bad_answer",
            AskCode::NotListening => "not_listening",
            AskCode::Unreachable => "unreachable",
        }
    }
}

#[derive(Debug)]
pub(crate) struct AskError {
    pub(crate) code: AskCode,
    pub(crate) message: String,
}

fn fail(code: AskCode, message: impl Into<String>) -> AskError {
    AskError {
        code,
        message: message.into(),
    }
}

#[derive(Debug)]
pub(crate) enum Target {
    Unix(String),
    Tcp(String, u16),
}

/// The Linux errno names Node reports (`error.code`); `None` for any other.
fn errno_name(code: i32) -> Option<&'static str> {
    Some(match code {
        1 => "EPERM",
        2 => "ENOENT",
        4 => "EINTR",
        5 => "EIO",
        11 => "EAGAIN",
        12 => "ENOMEM",
        13 => "EACCES",
        20 => "ENOTDIR",
        21 => "EISDIR",
        22 => "EINVAL",
        24 => "EMFILE",
        32 => "EPIPE",
        36 => "ENAMETOOLONG",
        40 => "ELOOP",
        97 => "EAFNOSUPPORT",
        98 => "EADDRINUSE",
        99 => "EADDRNOTAVAIL",
        100 => "ENETDOWN",
        101 => "ENETUNREACH",
        103 => "ECONNABORTED",
        104 => "ECONNRESET",
        105 => "ENOBUFS",
        110 => "ETIMEDOUT",
        111 => "ECONNREFUSED",
        112 => "EHOSTDOWN",
        113 => "EHOSTUNREACH",
        _ => return None,
    })
}

const RETRYABLE: [&str; 4] = ["ENOENT", "ECONNREFUSED", "EAGAIN", "ECONNRESET"];

/// What Node's `error.code` was for this failure, when it is one this port can name.
fn error_name(e: &io::Error) -> Option<&'static str> {
    e.raw_os_error().and_then(errno_name)
}

/// The `error` event: `The verifier is unreachable (<syscall> <CODE> [<address>]).`, retryable
/// (`not_listening`) only before any byte of an answer and for the four codes Node retries.
fn socket_error(e: &io::Error, syscall: &str, address: Option<&str>, bytes: usize) -> AskError {
    let name = error_name(e);
    let text = match (name, address) {
        (Some(n), Some(a)) => format!("{syscall} {n} {a}"),
        (Some(n), None) => format!("{syscall} {n}"),
        (None, _) => e.to_string(),
    };
    let retry = bytes == 0 && name.is_some_and(|n| RETRYABLE.contains(&n));
    fail(
        if retry {
            AskCode::NotListening
        } else {
            AskCode::Unreachable
        },
        format!("The verifier is unreachable ({text})."),
    )
}

/// `Number(digits)` for a string of ASCII digits (large values lose precision as in JS).
fn js_number(digits: &str) -> f64 {
    digits.parse::<f64>().unwrap_or(f64::INFINITY)
}

/// A port past 65535 in an otherwise valid `host:port`: Node's splitEndpoint accepts it and
/// `net.connect` then throws a RangeError.
fn out_of_range_port(text: &str) -> Option<String> {
    let (host, port) = text.rsplit_once(':')?;
    if port.is_empty() || !port.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let host_ok = if let Some(inner) = host.strip_prefix('[') {
        inner
            .strip_suffix(']')
            .is_some_and(|h| !h.is_empty() && !h.contains(']'))
    } else {
        !host.is_empty() && !host.contains(':')
    };
    let n = js_number(port);
    if host_ok && n >= 65536.0 {
        Some(format!(
            "Port should be >= 0 and < 65536. Received type number ({}).",
            js_json::number_to_string(n)
        ))
    } else {
        None
    }
}

/// `connectTo`: `unix:/absolute/path` or `host:port`; the error is Node's message (code `config`).
pub(crate) fn parse_target(endpoint: &str) -> Result<Target, String> {
    if let Some(path) = endpoint.strip_prefix("unix:") {
        if !path.starts_with('/') {
            return Err(format!(
                "CODE_VERIFY_ENDPOINT should be unix:/absolute/path or host:port, not \"{endpoint}\""
            ));
        }
        return Ok(Target::Unix(path.to_string()));
    }
    match code_acp::split_endpoint(endpoint) {
        Ok((host, port)) => Ok(Target::Tcp(host, port)),
        Err(e) => Err(out_of_range_port(endpoint).unwrap_or(e.message)),
    }
}

enum Conn {
    Tcp(TcpStream),
    Unix(UnixStream),
}

impl Conn {
    fn set_timeouts(&self, d: Duration) -> io::Result<()> {
        match self {
            Conn::Tcp(s) => {
                s.set_read_timeout(Some(d))?;
                s.set_write_timeout(Some(d))
            }
            Conn::Unix(s) => {
                s.set_read_timeout(Some(d))?;
                s.set_write_timeout(Some(d))
            }
        }
    }
}

impl Read for Conn {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self {
            Conn::Tcp(s) => s.read(buf),
            Conn::Unix(s) => s.read(buf),
        }
    }
}

impl Write for Conn {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self {
            Conn::Tcp(s) => s.write(buf),
            Conn::Unix(s) => s.write(buf),
        }
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// The attempt's clock: its deadline (`None` when it is too far away to represent) and the task's
/// cancel.
struct Gate<'a> {
    deadline: Option<Instant>,
    cancel: Option<&'a Cancel>,
}

impl Gate<'_> {
    fn check(&self) -> Result<(), AskError> {
        if self.cancel.is_some_and(Cancel::is_cancelled) {
            return Err(fail(AskCode::Cancelled, "Verification was cancelled."));
        }
        if self.deadline.is_some_and(|d| Instant::now() >= d) {
            return Err(fail(
                AskCode::Timeout,
                "The verifier did not answer in time.",
            ));
        }
        Ok(())
    }
    fn slice(&self) -> Duration {
        match self.deadline {
            Some(d) => d
                .saturating_duration_since(Instant::now())
                .clamp(Duration::from_millis(1), SLICE),
            None => SLICE,
        }
    }
}

type Reached = Result<Conn, AskError>;

/// Connects on a helper thread so a blocked connect never holds up the deadline or a cancel (a
/// connect still blocked then is abandoned, and its socket closed when it finishes).
fn reach(target: Target, gate: &Gate<'_>) -> Reached {
    let budget = gate
        .deadline
        .map_or(Duration::from_secs(3600), |d| {
            d.saturating_duration_since(Instant::now())
        })
        .max(Duration::from_millis(1));
    let (tx, rx) = mpsc::channel::<Reached>();
    let spawned = std::thread::Builder::new().spawn(move || {
        let _ = tx.send(connect_blocking(&target, budget));
    });
    if let Err(e) = spawned {
        return Err(fail(
            AskCode::Unreachable,
            format!("The verifier is unreachable ({e})."),
        ));
    }
    loop {
        gate.check()?;
        match rx.recv_timeout(gate.slice()) {
            Ok(r) => return r,
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                return Err(fail(
                    AskCode::Unreachable,
                    "The verifier is unreachable (connect failed).",
                ))
            }
        }
    }
}

fn connect_blocking(target: &Target, budget: Duration) -> Reached {
    match target {
        Target::Unix(path) => match UnixStream::connect(path) {
            Ok(s) => Ok(Conn::Unix(s)),
            // std refuses a path longer than sun_path before connecting; libuv cuts it short and
            // Node's connect then fails ENOENT (retried).
            Err(e) if e.raw_os_error().is_none() && e.kind() == io::ErrorKind::InvalidInput => {
                Err(socket_error(
                    &io::Error::from_raw_os_error(2),
                    "connect",
                    Some(path.as_str()),
                    0,
                ))
            }
            Err(e) => Err(socket_error(&e, "connect", Some(path.as_str()), 0)),
        },
        Target::Tcp(host, port) => {
            let addrs = match (host.as_str(), *port).to_socket_addrs() {
                Ok(a) => a,
                Err(_) => {
                    return Err(fail(
                        AskCode::Unreachable,
                        format!("The verifier is unreachable (getaddrinfo ENOTFOUND {host})."),
                    ))
                }
            };
            let mut last: Option<AskError> = None;
            for addr in addrs {
                match TcpStream::connect_timeout(&addr, budget) {
                    Ok(s) => return Ok(Conn::Tcp(s)),
                    Err(e) if e.raw_os_error().is_none() && e.kind() == io::ErrorKind::TimedOut => {
                        return Err(fail(
                            AskCode::Timeout,
                            "The verifier did not answer in time.",
                        ))
                    }
                    Err(e) => {
                        last = Some(socket_error(
                            &e,
                            "connect",
                            Some(&format!("{}:{}", addr.ip(), addr.port())),
                            0,
                        ))
                    }
                }
            }
            Err(last.unwrap_or_else(|| {
                fail(
                    AskCode::Unreachable,
                    format!("The verifier is unreachable (getaddrinfo ENOTFOUND {host})."),
                )
            }))
        }
    }
}

/// The deadline `timeout` from now, or `None` when that is not representable.
pub(crate) fn after(timeout: Duration) -> Option<Instant> {
    Instant::now().checked_add(timeout)
}

/// Sleeps `d`, waking early when `cancel` fires.
pub(crate) fn pause(d: Duration, cancel: Option<&Cancel>) {
    let end = after(d);
    loop {
        if cancel.is_some_and(Cancel::is_cancelled) {
            return;
        }
        let left = match end {
            Some(e) => e.saturating_duration_since(Instant::now()),
            None => SLICE,
        };
        if left.is_zero() {
            return;
        }
        std::thread::sleep(left.min(SLICE));
    }
}

/// `askVerifier`: connects, writes the request line, reads one answer line (at most
/// [`MAX_ANSWER_BYTES`]) and hangs up (dropping the connection is also the cancel). Resolves with
/// the parsed answer.
pub(crate) fn ask(
    target: Target,
    request_line: &str,
    timeout: Duration,
    cancel: Option<&Cancel>,
) -> Result<JValue, AskError> {
    let gate = Gate {
        deadline: after(timeout),
        cancel,
    };
    // Node connects before it looks at the signal; here a task cancelled before this never
    // connects at all.
    gate.check()?;
    let mut conn = reach(target, &gate)?;
    let connected = Instant::now();
    conn.set_timeouts(SLICE)
        .map_err(|e| socket_error(&e, "connect", None, 0))?;

    // The request. A write that fails is an `error` event as in Node: `write EPIPE` (the peer
    // closed before the request went out) is a real failure, `write ECONNRESET` is retryable.
    let data = request_line.as_bytes();
    let mut sent = 0usize;
    while sent < data.len() {
        gate.check()?;
        match conn.write(data.get(sent..).unwrap_or_default()) {
            Ok(0) => break,
            Ok(n) => sent += n,
            Err(e)
                if matches!(
                    e.kind(),
                    io::ErrorKind::WouldBlock
                        | io::ErrorKind::TimedOut
                        | io::ErrorKind::Interrupted
                ) => {}
            Err(e) => return Err(socket_error(&e, "write", None, 0)),
        }
    }

    let mut decoder = Utf8Stream::default();
    let mut received = String::new();
    let mut bytes = 0usize;
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        gate.check()?;
        let (text, eof) = match conn.read(&mut buf) {
            Ok(0) => (decoder.finish(), true),
            Ok(n) => (decoder.push(buf.get(..n).unwrap_or_default()), false),
            Err(e)
                if matches!(
                    e.kind(),
                    io::ErrorKind::WouldBlock
                        | io::ErrorKind::TimedOut
                        | io::ErrorKind::Interrupted
                ) =>
            {
                continue
            }
            Err(e) => return Err(socket_error(&e, "read", None, bytes)),
        };
        if !text.is_empty() {
            bytes += text.len();
            if bytes > MAX_ANSWER_BYTES {
                return Err(fail(
                    AskCode::BadAnswer,
                    "The verifier\u{2019}s answer was too large.",
                ));
            }
            let before = received.len();
            received.push_str(&text);
            if let Some(i) = text.find('\n') {
                let line = received.get(..before + i).unwrap_or_default();
                return wtf16::parse(line).map_err(|_| {
                    fail(
                        AskCode::BadAnswer,
                        "The verifier\u{2019}s answer was not readable.",
                    )
                });
            }
        }
        if eof {
            let quick = connected.elapsed() < QUICK_CLOSE;
            return Err(fail(
                if bytes == 0 && quick {
                    AskCode::NotListening
                } else {
                    AskCode::Unreachable
                },
                "The verifier closed the connection without an answer.",
            ));
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing, clippy::panic)]
mod tests {
    use super::*;

    fn err(e: &str) -> String {
        parse_target(e).unwrap_err()
    }

    #[test]
    fn endpoints_parse_as_connect_to_does() {
        assert!(
            matches!(parse_target("unix:/run/v.sock"), Ok(Target::Unix(p)) if p == "/run/v.sock")
        );
        assert!(
            matches!(parse_target("verifier:9000"), Ok(Target::Tcp(h, 9000)) if h == "verifier")
        );
        assert!(matches!(parse_target("[::1]:80"), Ok(Target::Tcp(h, 80)) if h == "::1"));
        assert_eq!(
            err("unix:relative"),
            "CODE_VERIFY_ENDPOINT should be unix:/absolute/path or host:port, not \"unix:relative\""
        );
        assert_eq!(
            err("nocolon"),
            "CODE_HARNESS_ENDPOINT should be host:port, not \"nocolon\""
        );
        assert_eq!(
            err("h:70000"),
            "Port should be >= 0 and < 65536. Received type number (70000)."
        );
        assert_eq!(
            err("h:99999999999999999999999"),
            "Port should be >= 0 and < 65536. Received type number (1e+23)."
        );
        assert!(err("a:b:80").starts_with("CODE_HARNESS_ENDPOINT"));
        assert!(err("h:80a").starts_with("CODE_HARNESS_ENDPOINT"));
    }

    #[test]
    fn only_four_connect_errors_are_not_listening() {
        let code = |errno: i32, bytes: usize| {
            socket_error(
                &io::Error::from_raw_os_error(errno),
                "connect",
                Some("/x"),
                bytes,
            )
            .code
        };
        for errno in [2, 111, 11, 104] {
            assert_eq!(code(errno, 0), AskCode::NotListening);
            assert_eq!(code(errno, 1), AskCode::Unreachable);
        }
        assert_eq!(code(13, 0), AskCode::Unreachable);
        assert_eq!(code(32, 0), AskCode::Unreachable);
        let e = socket_error(&io::Error::from_raw_os_error(2), "connect", Some("/x"), 0);
        assert_eq!(
            e.message,
            "The verifier is unreachable (connect ENOENT /x)."
        );
    }
}
