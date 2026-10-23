//! `workspace-restore-verify verify`: one workspace ZIP on stdin, the decision on stdout
//! (noevia-services' diary/agent/workspace_restore.py, WORKSPACE_RESTORE_VERIFY_IMPL=rust).
//!
//! Output: `accept <64 hex>` (the verified listing's SHA-256, see
//! `Verified::listing_sha256`) and exit 0, or `reject: <code>` and exit 1. Bad usage: nothing on
//! stdout, exit 2. I/O failure: exit 3. The verify command never prints a path, name or byte of the archive.
//! Input beyond 272 MiB is not read further: one byte past the cap is `reject: archive_too_large`.
//!
//! `workspace-restore-verify decode-v1`: fully verified metadata and raw bytes using the
//! versioned protocol in crates/workspace-restore-verify/DECODE-V1.md. A partial write is exit3.
//!
//! `workspace-restore-verify self-test`: a built-in synthetic export is accepted and a tampered
//! copy refused; prints `ok`. For image smoke tests.

#![forbid(unsafe_code)]

use std::io::{Read, Write};
use std::process::ExitCode;

use workspace_restore_verify::{verify, verify_archive, Reject, MAX_ARCHIVE};

const USAGE: &str =
    "usage: workspace-restore-verify verify|decode-v1 < archive.zip | workspace-restore-verify self-test";

fn fail(msg: &str, code: u8) -> ExitCode {
    let _ = writeln!(std::io::stderr().lock(), "workspace-restore-verify: {msg}");
    ExitCode::from(code)
}

fn say(line: &str, code: u8) -> ExitCode {
    let mut out = std::io::stdout().lock();
    if writeln!(out, "{line}").and_then(|()| out.flush()).is_err() {
        return fail("could not write to stdout", 3);
    }
    ExitCode::from(code)
}

fn decide(body: &[u8]) -> Result<String, Reject> {
    verify(body).map(|v| v.listing_sha256())
}

/// A stored, CRC-correct export with one file, one empty directory and the manifest, built
/// here so the self-test needs no fixture file in the image.
fn synthetic_export(content: &[u8]) -> Vec<u8> {
    use workspace_restore_verify::{crc32, sha256_hex};
    let manifest = format!(
        "{{\"bytes\": {n}, \"directories\": [\"empty\"], \"fileCount\": 1, \"files\": [{{\"bytes\": {n}, \"path\": \"note.md\", \"sha256\": \"{h}\"}}], \"format\": \"noevia-workspace-v1\", \"settings\": {{}}}}",
        n = content.len(),
        h = sha256_hex(content)
    );
    let members: [(&str, &[u8], u32); 4] = [
        ("manifest.json", manifest.as_bytes(), 0o100_600),
        ("workspace/", b"", 0o040_700),
        ("workspace/empty/", b"", 0o040_700),
        ("workspace/note.md", content, 0o100_600),
    ];
    let mut out = Vec::new();
    let mut central = Vec::new();
    for (name, data, mode) in members {
        let offset = out.len() as u32;
        let (crc, len, nlen) = (crc32(data), data.len() as u32, name.len() as u16);
        out.extend_from_slice(b"PK\x03\x04\x14\x00\x00\x00\x00\x00\x00\x00\x21\x00");
        out.extend_from_slice(&crc.to_le_bytes());
        out.extend_from_slice(&len.to_le_bytes());
        out.extend_from_slice(&len.to_le_bytes());
        out.extend_from_slice(&nlen.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(name.as_bytes());
        out.extend_from_slice(data);
        central.extend_from_slice(b"PK\x01\x02\x14\x03\x14\x00\x00\x00\x00\x00\x00\x00\x21\x00");
        central.extend_from_slice(&crc.to_le_bytes());
        central.extend_from_slice(&len.to_le_bytes());
        central.extend_from_slice(&len.to_le_bytes());
        central.extend_from_slice(&nlen.to_le_bytes());
        central.extend_from_slice(&[0u8; 8]);
        central.extend_from_slice(&(mode << 16).to_le_bytes());
        central.extend_from_slice(&offset.to_le_bytes());
        central.extend_from_slice(name.as_bytes());
    }
    let cd_offset = out.len() as u32;
    let cd_len = central.len() as u32;
    out.extend_from_slice(&central);
    out.extend_from_slice(b"PK\x05\x06\x00\x00\x00\x00\x04\x00\x04\x00");
    out.extend_from_slice(&cd_len.to_le_bytes());
    out.extend_from_slice(&cd_offset.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out
}

fn self_test() -> ExitCode {
    let good = synthetic_export(b"# Synthetic self-test note\n");
    let mut tampered = good.clone();
    // Flip one content byte: the CRC (and the manifest checksum) no longer match.
    if let Some(at) = tampered.windows(9).position(|w| w == b"Synthetic") {
        if let Some(b) = tampered.get_mut(at) {
            *b ^= 0x20;
        }
    }
    let accepted = verify(&good).is_ok_and(|v| v.files.len() == 1 && v.directories == ["empty"]);
    if accepted && verify(&tampered).is_err() && good != tampered {
        say("ok", 0)
    } else {
        fail("self-test failed", 1)
    }
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("verify" | "decode-v1") if args.len() == 1 => {}
        Some("self-test") if args.len() == 1 => return self_test(),
        _ => return fail(USAGE, 2),
    }
    let mut body = Vec::new();
    if std::io::stdin()
        .lock()
        .take(MAX_ARCHIVE as u64 + 1)
        .read_to_end(&mut body)
        .is_err()
    {
        return fail("could not read stdin", 3);
    }
    if args.first().map(String::as_str) == Some("decode-v1") {
        return match verify_archive(&body) {
            Ok(archive) => match archive.write_decode_v1(std::io::stdout().lock()) {
                Ok(()) => ExitCode::SUCCESS,
                Err(_) => fail("could not write decoded archive", 3),
            },
            // Refusals retain the stable text/exit1 contract; they never emit a binary prefix.
            Err(r) => say(&format!("reject: {}", r.code()), 1),
        };
    }
    match decide(&body) {
        Ok(digest) => say(&format!("accept {digest}"), 0),
        Err(r) => say(&format!("reject: {}", r.code()), 1),
    }
}
