//! Rust-owned test oracle. Python is only the fixed loader for actual source.
use base64::{engine::general_purpose::STANDARD, Engine};
use js_json::JValue;
use sha2::{Digest, Sha256};
use std::{
    io::{Read, Write},
    net::{TcpStream, UdpSocket},
    path::Path,
    process::{Child, Command, Stdio},
    time::Duration,
};
pub type Error = Box<dyn std::error::Error>;
pub const REF: &str = "b45a122c04173a14e884fa1aa93a6a101281431d";
pub const HASHES: &[(&str, &str)] = &[
    (
        "server.py",
        "cc70c4d6aa015083e78bf09c67aad230ee7e09bb0759bc5c2993d8571f131880",
    ),
    (
        "isolation.py",
        "b67b303c18fa5331fe9fb3094370fed572fd1a070a0adf920e912757efd4e7d0",
    ),
    (
        "isolated_worker.py",
        "b41e2cb8f0a5748038ef35d87d34dd111f57e71b96cf50a576a334844e5e4013",
    ),
    (
        "extract.py",
        "ffa7f5e383834acdb55a8b387e14929c8a8accf6a5b0ae651f87611006b42e69",
    ),
];
fn string(s: impl Into<String>) -> JValue {
    JValue::Str(s.into())
}
fn optional(s: Option<&str>) -> JValue {
    s.map(string).unwrap_or(JValue::Null)
}
pub fn prepare(source: &Path, target: &Path) -> Result<Vec<String>, Error> {
    for (name, expected) in HASHES {
        let bytes = std::fs::read(source.join(name))?;
        if format!("{:x}", Sha256::digest(&bytes)) != *expected {
            return Err(format!("source hash mismatch: {name}").into());
        }
    }
    let source_text = std::fs::read_to_string(source.join("extract.py"))?;
    let literal = source_text
        .split_once("SUPPORTED = {")
        .and_then(|(_, s)| s.split_once('}').map(|(s, _)| s))
        .ok_or("pinned SUPPORTED literal missing")?;
    let mut formats = Vec::new();
    for token in literal.split(',').map(str::trim).filter(|s| !s.is_empty()) {
        let value = token
            .strip_prefix('"')
            .and_then(|s| s.strip_suffix('"'))
            .ok_or("unsupported pinned literal")?;
        if !value.starts_with('.') || !value.chars().skip(1).all(|c| c.is_ascii_alphanumeric()) {
            return Err("unsupported pinned suffix".into());
        }
        formats.push(value.to_owned());
    }
    formats.sort();
    if formats.len() != 17 {
        return Err("pinned format count changed".into());
    }
    for name in [
        "server.py",
        "isolation.py",
        "isolated_worker.py",
        "fake_converter.py",
    ] {
        std::fs::copy(source.join(name), target.join(name))?;
    }
    let list = serde_json::to_string(&formats)?;
    std::fs::write(target.join("extract.py"),format!("from fake_converter import extract as fake\nSUPPORTED=set({list})\ndef extract(path,name):\n result=fake(path,name)\n result.pop('pid',None)\n return result\n"))?;
    Ok(formats)
}
// Fixed loader only: imports the pinned HTTP owner, binds before reporting ready,
// and keeps its lifetime tied to the Rust-owned stdin pipe. No cases/HTTP policy.
const LOADER: &str = "import sys,threading,socket\nsys.path.insert(0,sys.argv[1])\nimport server\nh=server.ThreadingHTTPServer(('127.0.0.1',0),server.Handler)\nthreading.Thread(target=h.serve_forever,daemon=True).start()\nsocket.socket(socket.AF_INET,socket.SOCK_DGRAM).sendto((str(h.server_address[1])+':'+str(sys.version_info.major)+'.'+str(sys.version_info.minor)).encode(),('127.0.0.1',int(sys.argv[2])))\ntry:sys.stdin.buffer.read()\nfinally:h.shutdown();h.server_close();server.runner.close()\n";
pub struct Server {
    child: Child,
    pub port: u16,
    pub production: bool,
}
impl Server {
    pub fn start(directory: &Path) -> Result<Self, Error> {
        let ready_socket = UdpSocket::bind("127.0.0.1:0")?;
        ready_socket.set_read_timeout(Some(Duration::from_secs(10)))?;
        let mut child = Command::new("python3")
            .args(["-c", LOADER])
            .arg(directory)
            .arg(ready_socket.local_addr()?.port().to_string())
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()?;
        let result = (|| {
            let mut bytes = [0u8; 64];
            let size = ready_socket.recv(&mut bytes)?;
            let ready = std::str::from_utf8(bytes.get(..size).ok_or("ready count")?)?;
            let (port, version) = ready
                .trim()
                .split_once(':')
                .ok_or("loader readiness missing")?;
            Ok::<_, Error>((port.parse()?, version == "3.12"))
        })();
        match result {
            Ok((port, production)) => Ok(Self {
                child,
                port,
                production,
            }),
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                Err(error)
            }
        }
    }
}
impl Drop for Server {
    fn drop(&mut self) {
        drop(self.child.stdin.take());
        let _ = self.child.wait();
    }
}
pub fn exchange(
    port: u16,
    method: &str,
    path: &str,
    length: Option<&str>,
    name: Option<&str>,
    data: &[u8],
) -> Result<JValue, Error> {
    let mut socket = TcpStream::connect(("127.0.0.1", port))?;
    socket.set_read_timeout(Some(Duration::from_secs(10)))?;
    socket.set_write_timeout(Some(Duration::from_secs(10)))?;
    let mut header = format!("{method} {path} HTTP/1.0\r\nHost: localhost\r\n");
    if let Some(n) = length {
        header.push_str(&format!("Content-Length: {n}\r\n"));
    }
    if let Some(n) = name {
        header.push_str(&format!("X-Document-Name: {n}\r\n"));
    }
    header.push_str("\r\n");
    socket.write_all(header.as_bytes())?;
    socket.write_all(data)?;
    let mut reply = Vec::new();
    let mut chunk = [0u8; 65536];
    loop {
        match socket.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => {
                if reply
                    .len()
                    .checked_add(n)
                    .is_none_or(|n| n > 4 * 1024 * 1024)
                {
                    return Err("oracle reply cap".into());
                }
                reply.extend_from_slice(chunk.get(..n).ok_or("read count")?);
            }
            Err(e) if e.kind() == std::io::ErrorKind::ConnectionReset => break,
            Err(e) => return Err(e.into()),
        }
    }
    let split = reply
        .windows(4)
        .position(|b| b == b"\r\n\r\n")
        .ok_or("HTTP delimiter missing")?;
    let headers = std::str::from_utf8(reply.get(..split).ok_or("headers missing")?)?;
    let body = reply
        .get(split.checked_add(4).ok_or("offset")?..)
        .ok_or("body missing")?;
    let status = headers
        .split_whitespace()
        .nth(1)
        .ok_or("status missing")?
        .parse::<u16>()?;
    let field = |key: &str| {
        headers
            .lines()
            .find_map(|l| {
                l.split_once(':')
                    .filter(|(k, _)| k.eq_ignore_ascii_case(key))
                    .map(|(_, v)| v.trim())
            })
            .ok_or("header missing")
    };
    let size = field("content-length")?.parse::<usize>()?;
    if body.len() != if method == "HEAD" { 0 } else { size } {
        return Err("incomplete HTTP response".into());
    }
    let kind = field("content-type")?;
    let body = if kind == "application/json" {
        js_json::parse(std::str::from_utf8(body)?).map_err(|_| "oracle JSON invalid")?
    } else {
        string(std::str::from_utf8(body)?)
    };
    Ok(JValue::obj([
        ("status", JValue::Num(f64::from(status))),
        ("type", string(kind)),
        ("body", body),
    ]))
}
pub fn generate(source: &Path, output: &Path) -> Result<(), Error> {
    let directory = tempfile::tempdir()?;
    let formats = prepare(source, directory.path())?;
    let server = Server::start(directory.path())?;
    if std::env::var("CI").is_ok() && !server.production {
        return Err("mandatory oracle interpreter must be Python3.12".into());
    }
    type Case = (
        &'static str,
        &'static str,
        Option<&'static str>,
        Option<String>,
        Vec<u8>,
    );
    let mut cases: Vec<Case> = vec![];
    macro_rules! case {
        ($m:expr,$p:expr,$l:expr,$n:expr,$d:expr) => {
            cases.push(($m, $p, $l, $n.map(str::to_owned), $d.to_vec()));
        };
    }
    case!("GET", "/health", None, None::<&str>, b"");
    case!("GET", "//health", None, None::<&str>, b"");
    case!("GET", "/health?x=1", None, None::<&str>, b"");
    case!("POST", "/unknown", None, None::<&str>, b"");
    case!("POST", "/extract", None, Some("x.md"), b"");
    case!("POST", "/extract", Some("26214401"), Some("x.md"), b"");
    case!("POST", "/extract", Some("1"), None::<&str>, b"");
    case!("POST", "/extract", Some("1"), Some("x/y.md"), b"");
    case!("POST", "/extract", Some("1"), Some("x\\y.md"), b"");
    cases.push(("POST", "/extract", Some("1"), Some("x".repeat(201)), vec![]));
    cases.push((
        "POST",
        "/extract",
        Some("1"),
        Some("x".repeat(200)),
        b"x".to_vec(),
    ));
    case!("POST", "/extract", Some("1"), Some("x.zip"), b"x");
    case!("POST", "/extract", Some("1"), Some("x.md "), b"x");
    case!("POST", "/extract", Some("+1"), Some("x.DOCX"), b"x");
    case!(
        "POST",
        "/extract",
        Some("1_0"),
        Some("x.docx"),
        b"abcdefghij"
    );
    case!("POST", "/extract", Some("4"), Some("x.docx"), b"BOOM");
    // #1711: Markdown is read as text by core and is not a Docling input.
    case!("POST", "/extract", Some("1"), Some("x.md"), b"x");
    for suffix in formats {
        cases.push((
            "POST",
            "/extract",
            Some("1"),
            Some(format!("x{suffix}")),
            b"x".to_vec(),
        ));
    }
    let mut rows = vec![];
    for (method, path, length, name, data) in cases {
        let expected = exchange(server.port, method, path, length, name.as_deref(), &data)?;
        rows.push(JValue::obj([
            ("method", string(method)),
            ("path", string(path)),
            ("length", optional(length)),
            ("name", optional(name.as_deref())),
            ("data", string(STANDARD.encode(data))),
            ("expected", expected),
        ]));
    }
    if rows.len() != 34 {
        return Err("oracle case count changed".into());
    }
    let fixture = JValue::obj([
        ("services", string(REF)),
        (
            "sources",
            JValue::obj(HASHES.iter().map(|(n, h)| (*n, string(*h)))),
        ),
        ("cases", JValue::Arr(rows)),
    ]);
    let text = js_json::stringify_pretty(&fixture).ok_or("oracle serialization failed")?;
    std::fs::write(output, format!("{text}\n"))?;
    println!("actual Python Docling HTTP goldens: 34");
    Ok(())
}
