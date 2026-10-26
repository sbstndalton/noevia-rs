//! Synthetic retained-content/IPC checks. No filesystem extraction or services.
#![allow(clippy::panic, clippy::indexing_slicing, clippy::unwrap_used)]
mod common;
use common::*;
use serde_json::{json, Value};
use std::io::{self, Write};
use workspace_restore_verify::{
    sha256_hex, verify, verify_archive, DECODE_V1_END, DECODE_V1_MAGIC,
};

fn bundle(settings: Option<Value>) -> Vec<u8> {
    let mut entries = export_entries(
        &[("z.bin", b"\0\xff\r\n"), ("é.md", b"# note\r\n")],
        &["empty"],
    );
    let mut manifest: Value = serde_json::from_slice(&entries[0].data).unwrap();
    if let Some(settings) = settings {
        manifest["settings"] = settings;
    } else {
        manifest.as_object_mut().unwrap().remove("settings");
    }
    entries[0].data = serde_json::to_vec(&manifest).unwrap();
    for entry in &mut entries {
        entry.flags |= 0x800;
    }
    // Exercise the same DEFLATE path as verify, not a new extraction library.
    entries[2].deflate = true;
    entries[3].deflate = true;
    zip(&entries)
}

#[test]
fn checked_bytes_settings_fingerprint_and_sorted_listing_stay_bound() {
    for settings in [
        None,
        Some(Value::Null),
        Some(json!(["layout", 9, false])),
        Some(json!({"unicode":"日本語", "other":{"x":true}})),
    ] {
        let body = bundle(settings.clone());
        let archive = verify_archive(&body).unwrap();
        assert_eq!(archive.fingerprint(), sha256_hex(&body));
        assert_eq!(archive.settings(), &settings.unwrap_or_else(|| json!({})));
        assert_eq!(archive.listing(), &verify(&body).unwrap());
        let files: Vec<_> = archive.files().collect();
        assert_eq!(files.len(), 2);
        assert_eq!(files[0].0.path, "z.bin");
        assert_eq!(files[0].1, b"\0\xff\r\n");
        assert_eq!(files[1].0.path, "é.md");
        assert_eq!(files[1].1, b"# note\r\n");
        for (record, bytes) in files {
            assert_eq!(record.bytes, bytes.len() as u64);
            assert_eq!(record.sha256, sha256_hex(bytes));
        }
        let mut out = Vec::new();
        archive.write_decode_v1(&mut out).unwrap();
        assert!(out.starts_with(DECODE_V1_MAGIC));
        assert!(out.ends_with(DECODE_V1_END));
        // Payloads are raw, once: no base64 or second compressed representation.
        assert_eq!(out.windows(4).filter(|p| *p == b"\0\xff\r\n").count(), 1);
    }
}

#[test]
fn retained_api_replays_every_frozen_decision_and_listing() {
    use base64::Engine;
    let corpus: Value =
        serde_json::from_str(include_str!("fixtures/workspace-restore.v1.json")).unwrap();
    let cases = corpus["cases"].as_array().unwrap();
    assert!(cases.len() > 250);
    for case in cases {
        let body = base64::engine::general_purpose::STANDARD
            .decode(case["archive_b64"].as_str().unwrap())
            .unwrap();
        let result = verify_archive(&body);
        let actual = match &result {
            Ok(a) => format!("accept {}", a.listing().listing_sha256()),
            Err(e) => format!("reject: {}", e.code()),
        };
        assert_eq!(actual, case["rust"].as_str().unwrap(), "{}", case["name"]);
        if let Ok(a) = result {
            assert_eq!(a.listing(), &verify(&body).unwrap());
            for (record, bytes) in a.files() {
                assert_eq!(sha256_hex(bytes), record.sha256);
            }
        }
    }
}

struct BrokenOutput {
    remaining: usize,
    flush_fails: bool,
}
impl Write for BrokenOutput {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if self.remaining == 0 {
            return Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "synthetic writer failure",
            ));
        }
        let count = self.remaining.min(bytes.len());
        self.remaining -= count;
        Ok(count)
    }
    fn flush(&mut self) -> io::Result<()> {
        if self.flush_fails {
            Err(io::Error::other("synthetic flush failure"))
        } else {
            Ok(())
        }
    }
}

#[test]
fn every_partial_write_and_final_flush_failure_propagates() {
    let archive = verify_archive(&bundle(None)).unwrap();
    let mut full = Vec::new();
    archive.write_decode_v1(&mut full).unwrap();
    for cut in 0..full.len() {
        assert!(
            archive
                .write_decode_v1(BrokenOutput {
                    remaining: cut,
                    flush_fails: false
                })
                .is_err(),
            "silently succeeded after {cut} bytes"
        );
    }
    assert!(archive
        .write_decode_v1(BrokenOutput {
            remaining: full.len(),
            flush_fails: true
        })
        .is_err());
}
