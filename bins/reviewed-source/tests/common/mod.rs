//! A small tar writer for building exact archive bytes, including malformed ones.
#![allow(
    dead_code,
    clippy::indexing_slicing,
    clippy::unwrap_used,
    clippy::expect_used
)]

use std::io::Write;

pub const BLOCK: usize = 512;

#[derive(Clone)]
pub struct Header {
    pub name: Vec<u8>,
    pub mode: Vec<u8>,
    pub size: Option<Vec<u8>>,
    pub kind: u8,
    pub link: Vec<u8>,
    pub prefix: Vec<u8>,
    pub signed_checksum: bool,
    pub bad_checksum: bool,
}

impl Header {
    pub fn new(name: impl AsRef<[u8]>, kind: u8) -> Self {
        Header {
            name: name.as_ref().to_vec(),
            mode: b"0000644\0".to_vec(),
            size: None,
            kind,
            link: Vec::new(),
            prefix: Vec::new(),
            signed_checksum: false,
            bad_checksum: false,
        }
    }

    pub fn mode(mut self, mode: u32) -> Self {
        self.mode = format!("{mode:07o}\0").into_bytes();
        self
    }

    pub fn raw_mode(mut self, field: &[u8]) -> Self {
        self.mode = field.to_vec();
        self
    }

    pub fn link(mut self, link: impl AsRef<[u8]>) -> Self {
        self.link = link.as_ref().to_vec();
        self
    }

    pub fn prefix(mut self, prefix: impl AsRef<[u8]>) -> Self {
        self.prefix = prefix.as_ref().to_vec();
        self
    }

    pub fn raw_size(mut self, field: &[u8]) -> Self {
        self.size = Some(field.to_vec());
        self
    }

    pub fn encode(&self, size: usize) -> [u8; BLOCK] {
        let mut block = [0u8; BLOCK];
        put(&mut block, 0, 100, &self.name);
        put(&mut block, 100, 8, &self.mode);
        put(&mut block, 108, 8, b"0001750\0");
        put(&mut block, 116, 8, b"0001750\0");
        match &self.size {
            Some(field) => put(&mut block, 124, 12, field),
            None => put(&mut block, 124, 12, format!("{size:011o}\0").as_bytes()),
        }
        put(&mut block, 136, 12, b"00000000001\0");
        block[156] = self.kind;
        put(&mut block, 157, 100, &self.link);
        put(&mut block, 257, 8, b"ustar\x0000");
        put(&mut block, 265, 32, b"qa");
        put(&mut block, 297, 32, b"qa");
        put(&mut block, 345, 155, &self.prefix);
        block[148..156].copy_from_slice(b"        ");
        let sum: i64 = if self.signed_checksum {
            block.iter().map(|b| i64::from(*b as i8)).sum()
        } else {
            block.iter().map(|b| i64::from(*b)).sum()
        };
        let sum = if self.bad_checksum { sum + 1 } else { sum };
        let text = if sum < 0 {
            format!("{sum:06}\0 ")
        } else {
            format!("{sum:06o}\0 ")
        };
        put(&mut block, 148, 8, text.as_bytes());
        block
    }
}

fn put(block: &mut [u8; BLOCK], at: usize, width: usize, value: &[u8]) {
    let n = value.len().min(width);
    block[at..at + n].copy_from_slice(&value[..n]);
}

/// Archive builder.
#[derive(Default, Clone)]
pub struct Tar {
    pub bytes: Vec<u8>,
}

impl Tar {
    pub fn new() -> Self {
        Tar::default()
    }

    pub fn raw(mut self, bytes: &[u8]) -> Self {
        self.bytes.extend_from_slice(bytes);
        self
    }

    pub fn member(mut self, header: Header, data: &[u8]) -> Self {
        self.bytes.extend_from_slice(&header.encode(data.len()));
        self.bytes.extend_from_slice(data);
        let pad = (BLOCK - data.len() % BLOCK) % BLOCK;
        self.bytes.extend(std::iter::repeat_n(0u8, pad));
        self
    }

    pub fn dir(self, name: &str) -> Self {
        self.member(Header::new(format!("{name}/"), b'5').mode(0o755), b"")
    }

    pub fn file(self, name: &str, data: &[u8], mode: u32) -> Self {
        self.member(Header::new(name, b'0').mode(mode), data)
    }

    pub fn symlink(self, name: &str, target: &str) -> Self {
        self.member(Header::new(name, b'2').mode(0o777).link(target), b"")
    }

    /// A pax header (`x` extended or `g` global) carrying `records`.
    pub fn pax(self, kind: u8, records: &[(&str, &[u8])]) -> Self {
        let mut body = Vec::new();
        for (key, value) in records {
            body.extend(pax_record(key.as_bytes(), value));
        }
        let name = if kind == b'g' {
            "pax_global_header"
        } else {
            "././@PaxHeader"
        };
        self.member(Header::new(name, kind).mode(0o644), &body)
    }

    pub fn gnu_long(self, kind: u8, value: &[u8]) -> Self {
        let mut data = value.to_vec();
        data.push(0);
        self.member(Header::new("././@LongLink", kind), &data)
    }

    pub fn end(mut self) -> Vec<u8> {
        self.bytes.extend_from_slice(&[0u8; 2 * BLOCK]);
        self.bytes
    }
}

pub fn pax_record(key: &[u8], value: &[u8]) -> Vec<u8> {
    let tail = key.len() + value.len() + 3;
    let mut length = tail + 1;
    while length.to_string().len() + tail != length {
        length = length.to_string().len() + tail;
    }
    let mut out = format!("{length} ").into_bytes();
    out.extend_from_slice(key);
    out.push(b'=');
    out.extend_from_slice(value);
    out.push(b'\n');
    out
}

pub fn gzip(data: &[u8], mtime: u32) -> Vec<u8> {
    let mut encoder = flate2::GzBuilder::new()
        .mtime(mtime)
        .write(Vec::new(), flate2::Compression::default());
    encoder.write_all(data).unwrap();
    encoder.finish().unwrap()
}

/// The reference's own test rows: a root, a subdirectory, a plain file and an executable file.
pub fn rows(stamp_reverse: bool) -> Vec<u8> {
    let mut members: Vec<Box<dyn Fn(Tar) -> Tar>> = vec![
        Box::new(|t: Tar| t.dir("root")),
        Box::new(|t: Tar| t.dir("root/src")),
        Box::new(|t: Tar| t.file("root/src/main", b"reviewed source", 0o644)),
        Box::new(|t: Tar| t.file("root/build", b"#!/bin/sh\n", 0o755)),
    ];
    if stamp_reverse {
        members.reverse();
    }
    members.iter().fold(Tar::new(), |tar, add| add(tar)).end()
}

pub fn base() -> Tar {
    Tar::new()
        .dir("root")
        .dir("root/src")
        .file("root/src/main", b"reviewed source", 0o644)
        .file("root/build", b"#!/bin/sh\n", 0o755)
}
