//! Backup orchestration. The caller retains the existing gated remote backend and flock.
use base64::{engine::general_purpose::STANDARD, Engine};
use serde_json::{json, Value};
use std::io::{BufRead, Read, Write};
type R<T> = Result<T, Box<dyn std::error::Error>>;
fn string<'a>(v: &'a Value, k: &str) -> R<&'a str> {
    v.get(k)
        .and_then(Value::as_str)
        .ok_or_else(|| "field".into())
}
fn read(input: &mut impl BufRead) -> R<Value> {
    let mut line = String::new();
    // Only bounded control replies enter this process; file payloads travel outward.
    input.take(4 * 1024 * 1024 + 1).read_line(&mut line)?;
    if line.len() > 4 * 1024 * 1024 || !line.ends_with('\n') {
        return Err("frame".into());
    }
    Ok(serde_json::from_str(&line)?)
}
fn emit(v: Value) -> R<()> {
    let mut out = std::io::stdout().lock();
    serde_json::to_writer(&mut out, &v)?;
    out.write_all(b"\n")?;
    out.flush()?;
    Ok(())
}
fn verify(input: &mut impl BufRead, path: &str, data: &[u8]) -> R<bool> {
    emit(json!({"type":"verify","path":path,"data":STANDARD.encode(data)}))?;
    let reply = read(input)?;
    match string(&reply, "result")? {
        "ok" => Ok(true),
        "loginRefused" => Err("loginRefused".into()),
        _ => Err("remote".into()),
    }
}
fn run() -> R<()> {
    if std::env::args().nth(1).as_deref() != Some("run-v1") {
        return Err("protocol".into());
    }
    let mut input = std::io::stdin().lock();
    let v = read(&mut input)?;
    // pass_fds preserves the broker's *same open file description*, hence its flock.
    // This inherited raw descriptor is deliberately never closed/wrapped by Rust:
    // the kernel retains it until process exit, including all final DB writes and
    // error paths, even if the broker is killed. No unsafe fd ownership conversion.
    let lock_fd = v
        .get("lockFd")
        .and_then(Value::as_i64)
        .filter(|fd| *fd >= 3)
        .ok_or("lock fd")?;
    let inherited = std::fs::read_link(format!("/proc/self/fd/{lock_fd}"))?;
    let expected = std::path::Path::new(string(&v, "db")?)
        .parent()
        .ok_or("db parent")?
        .join("managed-backup.lock");
    if inherited.canonicalize()? != expected.canonicalize()? {
        return Err("lock identity".into());
    }
    let result = diary_backup_run::backup(&v, |path, data| verify(&mut input, path, data))?;
    emit(json!({"type":"done","outcome":result}))
}
fn main() {
    if run().is_err() {
        eprintln!("unavailable");
        std::process::exit(2);
    }
}
