//! Owned loopback process fixtures only; no production key/mount/network probes.
#![cfg(unix)]
use egress::control::{derive_control_key, FileKey, RequestMetadata, Route};
use serde_json::{json, Value};
use std::{
    io::{BufRead, BufReader, Read, Write},
    net::{SocketAddr, TcpStream},
    os::unix::fs::{MetadataExt, PermissionsExt},
    path::PathBuf,
    process::{Child, Command, Stdio},
    sync::mpsc,
    thread,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
type TestError = Box<dyn std::error::Error>;
const VARIABLES: &[&str] = &[
    "CODE_EGRESS_GRANTS_FILE",
    "CODE_EGRESS_GRANT_KEY_FILE",
    "CODE_EGRESS_GRANT_KEY",
    "CODE_EGRESS_CONTROL_KEY_FILE",
    "CODE_EGRESS_CONTROL_KEY_UID",
    "CODE_EGRESS_CONTROL_KEY_GID",
    "CODE_EGRESS_BIND",
    "CODE_EGRESS_PORT",
    "CODE_EGRESS_MAX_CONNECTIONS",
    "CODE_EGRESS_MAX_CONNECTIONS_PER_TASK",
    "CODE_EGRESS_TUNNEL_IDLE_MS",
];
fn command() -> Command {
    let mut c = Command::new(env!("CARGO_BIN_EXE_egress-proxy"));
    for v in VARIABLES {
        c.env_remove(v);
    }
    c
}
fn output(command: &mut Command) -> Result<std::process::Output, TestError> {
    let mut child = command
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let started = std::time::Instant::now();
    loop {
        if child.try_wait()?.is_some() {
            return Ok(child.wait_with_output()?);
        }
        if started.elapsed() > Duration::from_secs(2) {
            let _ = child.kill();
            let _ = child.wait();
            return Err("probe did not exit within its budget".into());
        }
        thread::sleep(Duration::from_millis(10));
    }
}
struct Temp {
    dir: PathBuf,
    key: PathBuf,
    uid: u32,
    gid: u32,
}
impl Temp {
    fn new() -> Result<Self, TestError> {
        let mut r = [0u8; 16];
        getrandom::fill(&mut r).map_err(|_| "fixture entropy")?;
        let name: String = r.iter().map(|b| format!("{b:02x}")).collect();
        let dir = std::env::temp_dir()
            .canonicalize()?
            .join(format!("noevia-native-startup-{name}"));
        std::fs::create_dir(&dir)?;
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))?;
        let m = std::fs::metadata(&dir)?;
        let gid = if m.uid() == 0 && m.gid() == 0 {
            1
        } else {
            m.gid()
        };
        Ok(Self {
            key: dir.join("control.key"),
            dir,
            uid: m.uid(),
            gid,
        })
    }
    fn publish(&self) -> Result<(), TestError> {
        std::fs::write(&self.key, format!("{}\n", "31".repeat(32)))?;
        if std::fs::metadata(&self.key)?.gid() != self.gid {
            std::os::unix::fs::chown(&self.key, None, Some(self.gid))?;
        }
        std::fs::set_permissions(&self.key, std::fs::Permissions::from_mode(0o440))?;
        Ok(())
    }
}
impl Drop for Temp {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}
struct Process {
    child: Child,
    reader: Option<thread::JoinHandle<()>>,
}
impl Drop for Process {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        if let Some(r) = self.reader.take() {
            let _ = r.join();
        }
    }
}
fn start(t: &Temp) -> Result<(Process, SocketAddr), TestError> {
    let mut child = command()
        .args(["--native-control-v1", "--control-key-file"])
        .arg(&t.key)
        .args([
            "--control-key-uid",
            &t.uid.to_string(),
            "--control-key-gid",
            &t.gid.to_string(),
            "--listen",
            "127.0.0.1:0",
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()?;
    let stderr = child.stderr.take().ok_or("stderr")?;
    let (tx, rx) = mpsc::channel();
    let reader = thread::spawn(move || {
        for line in BufReader::new(stderr).lines().map_while(Result::ok) {
            if tx.send(line).is_err() {
                break;
            }
        }
    });
    let process = Process {
        child,
        reader: Some(reader),
    };
    loop {
        let line = rx.recv_timeout(Duration::from_secs(3))?;
        if let Ok(v) = serde_json::from_str::<Value>(&line) {
            if v.get("event") == Some(&json!("egress.listening")) {
                assert_eq!(
                    v.get("authority"),
                    Some(&json!("requires-authenticated-ready"))
                );
                let addr = v
                    .get("bind")
                    .and_then(Value::as_str)
                    .ok_or("bind")?
                    .parse()?;
                return Ok((process, addr));
            }
        }
    }
}
fn ready_request() -> Result<Vec<u8>, TestError> {
    let file = FileKey::from_bytes([0x31; 32]);
    let fp = file.fingerprint();
    let id = "00112233445566778899aabbccddeeff";
    let deadline =
        u64::try_from(SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis())? + 30_000;
    let body=json!({"version":1,"mode":"initial","challenge":id,"expectedKeyFingerprint":fp,"priorSession":null}).to_string();
    let mac = derive_control_key(&file)?.sign_request(
        Route::Ready,
        RequestMetadata {
            request_id: id,
            deadline_ms: deadline,
            boot_id: None,
            key_fingerprint: &fp,
        },
        body.as_bytes(),
    )?;
    Ok(format!("POST {} HTTP/1.1\r\nHost: egress:3001\r\nContent-Type: application/json\r\nContent-Length: {}\r\nx-noevia-request-id: {id}\r\nx-noevia-deadline-ms: {deadline}\r\nx-noevia-key-fingerprint: {fp}\r\nx-noevia-control-mac: {mac}\r\nConnection: close\r\n\r\n{body}",Route::Ready.path(),body.len()).into_bytes())
}
fn exchange(addr: SocketAddr, raw: &[u8]) -> Result<Vec<u8>, TestError> {
    let mut s = TcpStream::connect(addr)?;
    s.set_read_timeout(Some(Duration::from_secs(2)))?;
    s.set_write_timeout(Some(Duration::from_secs(2)))?;
    s.write_all(raw)?;
    let mut out = Vec::new();
    s.read_to_end(&mut out)?;
    Ok(out)
}
#[test]
fn real_native_binary_is_unready_until_current_key_is_published() -> Result<(), TestError> {
    let t = Temp::new()?;
    let (_process, addr) = start(&t)?;
    let request = ready_request()?;
    let out = exchange(addr, &request)?;
    assert!(out.starts_with(b"HTTP/1.1 503"));
    assert!(!String::from_utf8_lossy(&out).contains("x-noevia-control-mac"));
    t.publish()?;
    let out = exchange(addr, &request)?;
    assert!(out.starts_with(b"HTTP/1.1 200"));
    assert!(String::from_utf8_lossy(&out).contains("x-noevia-control-mac"));
    assert!(!String::from_utf8_lossy(&out).contains(&t.key.to_string_lossy().to_string()));
    Ok(())
}
#[test]
fn real_probe_needs_no_authority_and_native_configuration_never_falls_back() -> Result<(), TestError>
{
    let out = output(
        command()
            .arg("--capabilities")
            .env("CODE_EGRESS_GRANT_KEY", "synthetic-private-input"),
    )?;
    assert!(out.status.success());
    let v: Value = serde_json::from_slice(&out.stdout)?;
    assert_eq!(
        v.get("readiness"),
        Some(&json!("requires-authenticated-ready"))
    );
    let out = output(
        command()
            .args([
                "--native-control-v1",
                "--control-key-file",
                "/tmp/synthetic-private-input",
                "--control-key-uid",
                "0",
                "--control-key-gid",
                "1",
                "--listen",
                "127.0.0.1:0",
            ])
            .env("CODE_EGRESS_GRANT_KEY", "synthetic-private-input"),
    )?;
    assert!(!out.status.success());
    assert!(!String::from_utf8_lossy(&out.stderr).contains("synthetic-private-input"));
    let out = output(command().arg("synthetic-private-input"))?;
    assert!(!out.status.success());
    assert!(!String::from_utf8_lossy(&out.stderr).contains("synthetic-private-input"));
    use std::os::unix::ffi::OsStringExt;
    let out = output(command().arg(std::ffi::OsString::from_vec(vec![0xff, 0xfe])))?;
    assert_eq!(out.status.code(), Some(2));
    assert!(std::str::from_utf8(&out.stderr).is_ok());
    assert!(out.stderr.len() < 256);

    Ok(())
}
