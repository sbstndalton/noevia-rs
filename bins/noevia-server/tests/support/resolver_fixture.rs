//! Closed Rust-only helper: host names select fixed fixture behaviour, never DNS.
use std::io::{Read, Write};
fn main() -> std::io::Result<()> {
    let mut input = Vec::new();
    std::io::stdin().take(8193).read_to_end(&mut input)?;
    if input.len() < 14 || input.len() > 8192 {
        std::process::exit(2)
    }
    let host_len = usize::from(input.get(13).copied().unwrap_or(0));
    let host = String::from_utf8_lossy(input.get(14..14 + host_len).unwrap_or(&[]));
    if host == "blocked" {
        std::thread::sleep(std::time::Duration::from_secs(20));
        return Ok(());
    }
    if host == "stderr" {
        std::io::stderr().write_all(&[b'x'; 4097])?;
        return Ok(());
    }
    if host == "flood" {
        std::io::stdout().write_all(&[b'x'; 32769])?;
        return Ok(());
    }
    if host == "exit" {
        std::process::exit(2)
    }
    let mut out = b"NA01".to_vec();
    out.extend_from_slice(input.get(4..12).unwrap_or(&[]));
    if host == "stale" {
        if let Some(b) = out.last_mut() {
            *b = b.wrapping_add(1);
        }
    }
    let count = input.get(12).copied().unwrap_or(0);
    out.push(count);
    for ordinal in 0..count {
        if host == "nx" || (host == "partial" && ordinal == 1) {
            out.extend_from_slice(&[ordinal, 1, 0, 0]);
        } else if host == "records" {
            out.extend_from_slice(&[ordinal, 0, 1, 1]);
        } else {
            out.extend_from_slice(&[ordinal, 0, 0, 1]);
            out.push(9);
            out.extend_from_slice(b"127.0.0.2");
        }
    }
    if host == "suffix" {
        out.push(0);
    }
    if host == "truncated" {
        out.pop();
    }
    std::io::stdout().write_all(&out)
}
