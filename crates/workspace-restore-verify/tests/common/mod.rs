//! A tiny synthetic ZIP writer for the tests (stored or DEFLATE, explicit modes and sizes).
#![allow(dead_code)]

use workspace_restore_verify::{crc32, sha256_hex};

pub const FILE: u32 = 0o100_600;
pub const DIR: u32 = 0o040_700;

#[derive(Clone)]
pub struct Entry {
    pub name: Vec<u8>,
    pub data: Vec<u8>,
    pub mode: u32,
    pub deflate: bool,
    pub flags: u16,
    /// Override the declared uncompressed size (both headers).
    pub usize: Option<u32>,
    /// Override the CRC (both headers).
    pub crc: Option<u32>,
}

pub fn entry(name: &str, data: &[u8]) -> Entry {
    Entry {
        name: name.as_bytes().to_vec(),
        data: data.to_vec(),
        mode: if name.ends_with('/') { DIR } else { FILE },
        deflate: false,
        flags: 0,
        usize: None,
        crc: None,
    }
}

pub fn zip(entries: &[Entry]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut central = Vec::new();
    for e in entries {
        let offset = out.len() as u32;
        let raw = if e.deflate {
            miniz_oxide::deflate::compress_to_vec(&e.data, 6)
        } else {
            e.data.clone()
        };
        let crc = e.crc.unwrap_or_else(|| crc32(&e.data));
        let usize_ = e.usize.unwrap_or(e.data.len() as u32);
        let method: u16 = if e.deflate { 8 } else { 0 };
        let mut common = Vec::new();
        common.extend_from_slice(&e.flags.to_le_bytes());
        common.extend_from_slice(&method.to_le_bytes());
        common.extend_from_slice(&[0, 0, 0x21, 0]);
        common.extend_from_slice(&crc.to_le_bytes());
        common.extend_from_slice(&(raw.len() as u32).to_le_bytes());
        common.extend_from_slice(&usize_.to_le_bytes());
        common.extend_from_slice(&(e.name.len() as u16).to_le_bytes());
        common.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(b"PK\x03\x04\x14\x00");
        out.extend_from_slice(&common);
        out.extend_from_slice(&e.name);
        out.extend_from_slice(&raw);
        central.extend_from_slice(b"PK\x01\x02\x14\x03\x14\x00");
        central.extend_from_slice(&common);
        central.extend_from_slice(&[0u8; 6]);
        central.extend_from_slice(&(e.mode << 16).to_le_bytes());
        central.extend_from_slice(&offset.to_le_bytes());
        central.extend_from_slice(&e.name);
    }
    let cd_offset = out.len() as u32;
    let n = entries.len() as u16;
    out.extend_from_slice(&central);
    out.extend_from_slice(b"PK\x05\x06\x00\x00\x00\x00");
    out.extend_from_slice(&n.to_le_bytes());
    out.extend_from_slice(&n.to_le_bytes());
    out.extend_from_slice(&(central.len() as u32).to_le_bytes());
    out.extend_from_slice(&cd_offset.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out
}

pub fn manifest(files: &[(&str, &[u8])], dirs: &[&str]) -> Vec<u8> {
    let rows: Vec<String> = files
        .iter()
        .map(|(p, d)| {
            format!(
                "{{\"path\": {}, \"bytes\": {}, \"sha256\": \"{}\"}}",
                serde_json::to_string(p).unwrap_or_default(),
                d.len(),
                sha256_hex(d)
            )
        })
        .collect();
    let dirs: Vec<String> = dirs
        .iter()
        .map(|d| serde_json::to_string(d).unwrap_or_default())
        .collect();
    let total: usize = files.iter().map(|(_, d)| d.len()).sum();
    format!(
        "{{\"format\": \"noevia-workspace-v1\", \"settings\": {{}}, \"directories\": [{}], \"fileCount\": {}, \"bytes\": {}, \"files\": [{}]}}",
        dirs.join(", "),
        files.len(),
        total,
        rows.join(", ")
    )
    .into_bytes()
}

/// The entries of a well-formed export of `files` and `dirs`, in exporter order.
pub fn export_entries(files: &[(&str, &[u8])], dirs: &[&str]) -> Vec<Entry> {
    let mut v = vec![
        entry("manifest.json", &manifest(files, dirs)),
        entry("workspace/", b""),
    ];
    v.extend(dirs.iter().map(|d| entry(&format!("workspace/{d}/"), b"")));
    v.extend(
        files
            .iter()
            .map(|(p, d)| entry(&format!("workspace/{p}"), d)),
    );
    v
}

pub fn export(files: &[(&str, &[u8])], dirs: &[&str]) -> Vec<u8> {
    zip(&export_entries(files, dirs))
}
