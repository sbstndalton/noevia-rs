//! Synthetic GGUF byte builders, the same ones test_gguf_hostile_input.py uses.
#![allow(dead_code)]

use std::path::PathBuf;

pub const U32: u32 = 4;
pub const STRING: u32 = 8;
pub const ARRAY: u32 = 9;
pub const U64: u32 = 10;
pub const MB2: usize = 2 * 1024 * 1024;

pub fn s(x: &str) -> Vec<u8> {
    let mut out = (x.len() as u64).to_le_bytes().to_vec();
    out.extend_from_slice(x.as_bytes());
    out
}

pub fn header(kv_count: u64, tensors: u64) -> Vec<u8> {
    let mut out = b"GGUF".to_vec();
    out.extend_from_slice(&3u32.to_le_bytes());
    out.extend_from_slice(&tensors.to_le_bytes());
    out.extend_from_slice(&kv_count.to_le_bytes());
    out
}

pub fn kv_u32(key: &str, v: u32) -> Vec<u8> {
    let mut out = s(key);
    out.extend_from_slice(&U32.to_le_bytes());
    out.extend_from_slice(&v.to_le_bytes());
    out
}

pub fn u32le(v: u32) -> Vec<u8> {
    v.to_le_bytes().to_vec()
}

pub fn u64le(v: u64) -> Vec<u8> {
    v.to_le_bytes().to_vec()
}

/// `struct.pack("<IQ", a, b)`
pub fn iq(a: u32, b: u64) -> Vec<u8> {
    let mut out = u32le(a);
    out.extend_from_slice(&b.to_le_bytes());
    out
}

pub fn cat(parts: &[&[u8]]) -> Vec<u8> {
    parts.concat()
}

pub fn fixtures_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
}

/// Every `NAME.gguf` fixture with its bytes, sorted by name.
pub fn fixtures() -> Vec<(String, PathBuf, Vec<u8>)> {
    let mut out = Vec::new();
    for entry in std::fs::read_dir(fixtures_dir()).unwrap() {
        let path = entry.unwrap().path();
        if path.extension().and_then(|e| e.to_str()) == Some("gguf") {
            let name = path.file_stem().unwrap().to_string_lossy().into_owned();
            let bytes = std::fs::read(&path).unwrap();
            out.push((name, path, bytes));
        }
    }
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}
