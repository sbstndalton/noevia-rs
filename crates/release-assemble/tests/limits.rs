//! noevia#2726 items 1 and 3: the reader's caps refuse a broken or hostile archive before it allocates,
//! and extraction refuses symlinks that are empty, absolute or climb out of the tree.
#![cfg(unix)]
#![allow(clippy::indexing_slicing, clippy::unwrap_used)]

use release_assemble::tar::{self, Kind, Limits, Writer};
use std::io::Write;

const BLOCK: usize = 512;

fn octal(buf: &mut [u8], v: u64) {
    let width = buf.len() - 1;
    let s = format!("{v:0width$o}");
    buf[..width].copy_from_slice(&s.as_bytes()[s.len() - width..]);
    buf[width] = 0;
}

/// One raw ustar header; `size` may be anything a 12-byte octal field (or base-256) can say.
fn header(name: &str, typeflag: u8, size_field: &[u8; 12]) -> [u8; BLOCK] {
    let mut h = [0u8; BLOCK];
    h[..name.len()].copy_from_slice(name.as_bytes());
    octal(&mut h[100..108], 0o644);
    octal(&mut h[108..116], 0);
    octal(&mut h[116..124], 0);
    h[124..136].copy_from_slice(size_field);
    octal(&mut h[136..148], 0);
    h[156] = typeflag;
    h[257..263].copy_from_slice(b"ustar\0");
    h[263..265].copy_from_slice(b"00");
    h[148..156].copy_from_slice(b"        ");
    let sum: u64 = h.iter().map(|c| u64::from(*c)).sum();
    let mut cs = [0u8; 8];
    octal(&mut cs[..7], sum);
    cs[7] = b' ';
    h[148..156].copy_from_slice(&cs);
    h
}

fn size_octal(v: u64) -> [u8; 12] {
    let mut f = [0u8; 12];
    octal(&mut f, v);
    f
}

/// GNU base-256: high bit set, big-endian value in the rest.
fn size_base256(v: u64) -> [u8; 12] {
    let mut f = [0u8; 12];
    f[0] = 0x80;
    f[4..].copy_from_slice(&v.to_be_bytes());
    f
}

fn small() -> Limits {
    Limits {
        member: 4096,
        total: 8192,
        meta: 1024,
        members: 64,
    }
}

fn err_text(r: std::io::Result<Vec<tar::Entry>>) -> String {
    match r {
        Ok(e) => format!("accepted {} entries", e.len()),
        Err(e) => e.to_string(),
    }
}

#[test]
fn huge_size_fields_are_refused_before_any_allocation() {
    // 2^63 in base-256 and the largest 11-digit octal: refused as over the cap (default limits too),
    // never a `vec![0; size]` abort. The stream itself is only one header long.
    for field in [size_base256(1 << 63), size_octal(0o77777777777)] {
        let raw = header("big.bin", b'0', &field);
        let got = err_text(tar::read_all(&mut &raw[..]));
        assert!(got.contains("over the"), "{got}");
    }
}

#[test]
fn a_size_larger_than_the_stream_is_a_truncation() {
    let mut raw = header("short.bin", b'0', &size_octal(4000)).to_vec();
    raw.extend_from_slice(&[7u8; 100]);
    let got = err_text(tar::read_all_limited(&mut &raw[..], small()));
    assert!(got.contains("truncated"), "{got}");
}

#[test]
fn member_total_meta_and_count_caps() {
    // One member over `member`.
    let mut w = Writer::new(Vec::new(), 0);
    assert!(w.file(b"a", false, &[1u8; 5000]).is_ok());
    let one = w.finish().unwrap_or_default();
    assert!(err_text(tar::read_all_limited(&mut &one[..], small())).contains("4096-byte cap"));
    // Three members under `member` but over `total` together.
    let mut w = Writer::new(Vec::new(), 0);
    for n in [b"a", b"b", b"c"] {
        assert!(w.file(n, false, &[1u8; 3000]).is_ok());
    }
    let three = w.finish().unwrap_or_default();
    assert!(err_text(tar::read_all_limited(&mut &three[..], small())).contains("archive's data"));
    // The same archive passes the default limits.
    assert_eq!(
        tar::read_all(&mut &three[..]).map(|e| e.len()).ok(),
        Some(3)
    );
    // A GNU long name over `meta`.
    let mut raw = header("././@LongLink", b'L', &size_octal(2000)).to_vec();
    raw.extend_from_slice(&[b'n'; 2048]);
    assert!(err_text(tar::read_all_limited(&mut &raw[..], small())).contains("long name"));
    // More headers than `members` (empty files: no data at all).
    let mut w = Writer::new(Vec::new(), 0);
    for i in 0..65 {
        assert!(w.file(format!("f{i}").as_bytes(), false, b"").is_ok());
    }
    let many = w.finish().unwrap_or_default();
    assert!(err_text(tar::read_all_limited(&mut &many[..], small())).contains("more than 64"));
}

#[test]
fn a_gzip_bomb_ends_at_the_total_cap() {
    // 64 MiB of one member inflated from about 64 KiB: refused at an 8 KiB total, not inflated whole.
    let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::best());
    let h = header("bomb.bin", b'0', &size_octal(64 << 20));
    assert!(gz.write_all(&h).is_ok());
    let chunk = vec![0x41u8; 1 << 20];
    for _ in 0..64 {
        assert!(gz.write_all(&chunk).is_ok());
    }
    let bytes = gz.finish().unwrap_or_default();
    assert!(bytes.len() < 1 << 20);
    let got = match tar::read_gz_limited(&mut &bytes[..], small()) {
        Ok(e) => format!("accepted {}", e.len()),
        Err(e) => e.to_string(),
    };
    assert!(got.contains("4096-byte cap"), "{got}");
}

#[test]
fn the_total_cap_holds_through_gzip() {
    // Members each under `member`, together over `total`: refused while inflating, not after.
    let mut w = Writer::new(Vec::new(), 0);
    for i in 0..40 {
        assert!(w
            .file(format!("m{i}").as_bytes(), false, &[0x41u8; 3000])
            .is_ok());
    }
    let raw = w.finish().unwrap_or_default();
    let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::best());
    assert!(gz.write_all(&raw).is_ok());
    let bytes = gz.finish().unwrap_or_default();
    let got = match tar::read_gz_limited(&mut &bytes[..], small()) {
        Ok(e) => format!("accepted {}", e.len()),
        Err(e) => e.to_string(),
    };
    assert!(got.contains("archive's data"), "{got}");
    assert_eq!(
        tar::read_gz(&mut &bytes[..]).map(|e| e.len()).ok(),
        Some(40)
    );
}

/// The two-link escape a purely lexical check misses: `d/s -> ../x`, then `l -> d/s/../..`.
#[test]
fn a_climb_through_an_earlier_link_is_refused() {
    let mut w = Writer::new(Vec::new(), 0);
    assert!(w.dir(b"d").is_ok());
    assert!(w.dir(b"x").is_ok());
    assert!(w.symlink(b"d/s", b"../x").is_ok());
    assert!(w.symlink(b"l", b"d/s/../..").is_ok());
    let raw = w.finish().unwrap_or_default();
    let entries = tar::read_all(&mut &raw[..]).unwrap_or_default();
    assert_eq!(entries.len(), 4);
    let Some(d) = tempfile::tempdir().ok() else {
        return;
    };
    assert!(tar::extract(&entries, 0, d.path()).is_err());
}

fn link_archive(name: &[u8], target: &[u8]) -> Vec<tar::Entry> {
    let mut w = Writer::new(Vec::new(), 0);
    assert!(w.dir(b"top").is_ok());
    assert!(w.dir(b"top/a").is_ok());
    assert!(w.symlink(name, target).is_ok());
    let raw = w.finish().unwrap_or_default();
    tar::read_all(&mut &raw[..]).unwrap_or_default()
}

#[test]
fn symlinks_that_leave_the_tree_are_refused() {
    let refused: &[(&str, &str, usize)] = &[
        ("top/a/l", "/etc/passwd", 0),
        ("top/a/l", "", 0),
        ("top/a/l", "../../..", 0),
        ("top/a/l", "x/../../../../y", 0),
        // after --strip-components=1 the link sits one level higher: the same target now escapes
        ("top/a/l", "../../x", 1),
        ("top/l", "..", 1),
        // `..` after a named component (that component may be a link: noevia#2726 review)
        ("top/a/l", "./b/../c", 1),
        ("top/a/l", "s/../..", 0),
    ];
    for (name, target, strip) in refused {
        let d = tempfile::tempdir().ok();
        let Some(d) = d else { return };
        let entries = link_archive(name.as_bytes(), target.as_bytes());
        assert!(matches!(
            entries.last().map(|e| &e.kind),
            Some(Kind::Symlink { .. })
        ));
        let got = tar::extract(&entries, *strip, d.path());
        assert!(
            got.is_err(),
            "{name:?} -> {target:?} strip {strip} was written"
        );
    }
    let kept: &[(&str, &str, usize)] = &[
        ("top/a/l", "../../x", 0),
        ("top/a/l", "..", 1),
        ("top/a/l", "./../b/./c", 0),
        ("top/a/l", "sibling", 2),
    ];
    for (name, target, strip) in kept {
        let d = tempfile::tempdir().ok();
        let Some(d) = d else { return };
        let entries = link_archive(name.as_bytes(), target.as_bytes());
        let got = tar::extract(&entries, *strip, d.path());
        assert!(got.is_ok(), "{name:?} -> {target:?} strip {strip}: {got:?}");
    }
}

/// noevia#2738 item 3: a refused symlink leaves the file already at its path untouched.
#[test]
fn a_refused_symlink_keeps_the_existing_file() {
    let mut w = Writer::new(Vec::new(), 0);
    assert!(w.dir(b"top").is_ok());
    assert!(w.file(b"top/f", false, b"kept\n").is_ok());
    assert!(w.symlink(b"top/f", b"/etc/passwd").is_ok());
    let raw = w.finish().unwrap_or_default();
    let entries = tar::read_all(&mut &raw[..]).unwrap_or_default();
    assert_eq!(entries.len(), 3);
    let d = tempfile::tempdir().unwrap();
    assert!(tar::extract(&entries, 0, d.path()).is_err());
    assert_eq!(std::fs::read(d.path().join("top/f")).unwrap(), b"kept\n");
}
