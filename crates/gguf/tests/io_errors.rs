//! A failing reader is an I/O error, never mistaken for an array that runs out of data.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

mod common;

use std::io::{Read, Seek, SeekFrom};

use common::*;
use gguf::{read_raw_stream, Value};

/// Serves `data` up to `fail_at`, then fails every read; claims `len` bytes in total.
struct Failing {
    data: Vec<u8>,
    fail_at: u64,
    len: u64,
    pos: u64,
}

impl Read for Failing {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if self.pos >= self.fail_at {
            return Err(std::io::Error::other("synthetic disk failure"));
        }
        let n = (self.fail_at - self.pos).min(buf.len() as u64) as usize;
        for (i, b) in buf[..n].iter_mut().enumerate() {
            *b = self.data.get(self.pos as usize + i).copied().unwrap_or(0);
        }
        self.pos += n as u64;
        Ok(n)
    }
}

impl Seek for Failing {
    fn seek(&mut self, pos: SeekFrom) -> std::io::Result<u64> {
        self.pos = match pos {
            SeekFrom::Start(p) => p,
            SeekFrom::End(d) => self.len.saturating_add_signed(d),
            SeekFrom::Current(d) => self.pos.saturating_add_signed(d),
        };
        Ok(self.pos)
    }
}

#[test]
fn read_failure_while_skipping_an_array_is_reported_as_io() {
    let strings: Vec<u8> = (0..20).flat_map(|i| s(&format!("t{i}"))).collect();
    let data = cat(&[
        &header(1, 0),
        &s("tokenizer.ggml.tokens"),
        &u32le(ARRAY),
        &iq(STRING, 100),
        &strings,
    ]);
    let fail_at = data.len() as u64;
    let r = Failing {
        data,
        fail_at,
        len: fail_at + 4096,
        pos: 0,
    };
    let raw = read_raw_stream(r).unwrap();
    assert_eq!(
        raw.get("_error"),
        Some(&Value::Str(
            "stopped at KV read: synthetic disk failure".into()
        ))
    );
}
