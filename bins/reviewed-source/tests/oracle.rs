//! Parity with the reference (noevia-core `code-sandbox/verify-native-source.py`, run as the oracle).
//!
//! Every case archive is checked by the native `inspect` and compared with the reference's verdict: the
//! digest it prints, or `refused`. The verdicts are committed in `tests/fixtures/oracle-verdicts.tsv`.
//! With `REVIEWED_SOURCE_ORACLE=<path to verify-native-source.py>` the reference is run live (python3) on
//! every case as well, and `REVIEWED_SOURCE_RECORD=1` rewrites the fixture from it.
//! `STRICTER` lists the cases where the native reader refuses and the reference accepts by silently ending
//! the archive at a malformed header after the first member (see the `tar` module documentation).
#![allow(
    clippy::indexing_slicing,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic
)]

mod common;
use common::{base, gzip, rows, Header, Tar, BLOCK};
use reviewed_source::{inspect, Limits};
use std::collections::BTreeMap;
use std::path::PathBuf;

const STRICTER: &[&str] = &[
    "garbage-after-member",
    "bad-checksum-after-member",
    "bad-pax-after-member",
    "truncated-header-after-member",
];

fn cases() -> Vec<(&'static str, Vec<u8>)> {
    let long = format!("root/{}", "d".repeat(120));
    let long_file = format!("{long}/{}", "f".repeat(130));
    let mut out: Vec<(&'static str, Vec<u8>)> = vec![
        ("rows", rows(false)),
        ("rows-reversed-gzip", gzip(&rows(true), 99)),
        (
            "rows-missing-build",
            Tar::new()
                .dir("root")
                .dir("root/src")
                .file("root/src/main", b"reviewed source", 0o644)
                .end(),
        ),
        ("rows-extra", base().file("root/extra", b"x", 0o644).end()),
        (
            "rows-changed",
            Tar::new()
                .dir("root")
                .dir("root/src")
                .file("root/src/main", b"reviewed source", 0o644)
                .file("root/build", b"changed", 0o755)
                .end(),
        ),
        (
            "rows-mode",
            Tar::new()
                .dir("root")
                .dir("root/src")
                .file("root/src/main", b"reviewed source", 0o644)
                .file("root/build", b"#!/bin/sh\n", 0o644)
                .end(),
        ),
        (
            "link-relative",
            base().symlink("root/link", "src/main").end(),
        ),
        (
            "link-child",
            base()
                .symlink("root/link", "src/main")
                .file("root/link/child", b"outside", 0o644)
                .end(),
        ),
        (
            "dup-file",
            base().file("root/build", b"#!/bin/sh\n", 0o755).end(),
        ),
        ("dup-root", base().dir("root").end()),
        ("other-root", base().file("other/file", b"x", 0o644).end()),
        ("dotdot", base().file("root/../escape", b"x", 0o644).end()),
        (
            "double-slash",
            base().file("root/src//bad", b"x", 0o644).end(),
        ),
        (
            "link-absolute",
            base().symlink("root/absolute", "/tmp/out").end(),
        ),
        (
            "link-escaping",
            base().symlink("root/escaping", "../out").end(),
        ),
        (
            "hardlink",
            base()
                .member(Header::new("root/hard", b'1').link("root/src/main"), b"")
                .end(),
        ),
        (
            "missing-parent",
            Tar::new().file("root/absent/file", b"x", 0o644).end(),
        ),
        (
            "gnu-sparse",
            Tar::new()
                .dir("root")
                .member(Header::new("root/sparse", b'S'), b"x")
                .end(),
        ),
        (
            "pax-sparse",
            Tar::new()
                .dir("root")
                .pax(
                    b'x',
                    &[
                        ("GNU.sparse.map", b"0,1"),
                        ("GNU.sparse.size", b"32768"),
                        ("GNU.sparse.name", b"root/sparse"),
                    ],
                )
                .file("root/sparse", b"x", 0o644)
                .end(),
        ),
        (
            "pax-realsize",
            Tar::new()
                .dir("root")
                .pax(b'x', &[("SCHILY.realsize", b"1")])
                .file("root/r", b"x", 0o644)
                .end(),
        ),
        (
            "global-comment",
            Tar::new()
                .pax(
                    b'g',
                    &[("comment", b"0123456789abcdef0123456789abcdef01234567")],
                )
                .dir("root")
                .file("root/a", b"a", 0o664)
                .file("root/x", b"#!", 0o775)
                .end(),
        ),
        (
            "global-sparse-key",
            Tar::new()
                .pax(b'g', &[("GNU.sparse.major", b"1")])
                .dir("root")
                .file("root/a", b"a", 0o644)
                .end(),
        ),
        (
            "global-path",
            Tar::new()
                .pax(b'g', &[("path", b"root/same")])
                .dir("root")
                .file("root/a", b"a", 0o644)
                .end(),
        ),
        (
            "pax-long-path",
            Tar::new()
                .dir("root")
                .pax(b'x', &[("path", long.as_bytes())])
                .member(Header::new("root/dshort", b'5').mode(0o755), b"")
                .pax(b'x', &[("path", long_file.as_bytes())])
                .file("root/short", b"long", 0o644)
                .end(),
        ),
        (
            "pax-path-trailing-slash-file",
            Tar::new()
                .dir("root")
                .pax(b'x', &[("path", b"root/file//")])
                .file("root/zzz", b"f", 0o644)
                .end(),
        ),
        (
            "pax-size",
            Tar::new()
                .dir("root")
                .pax(b'x', &[("size", b"3")])
                .member(
                    Header::new("root/sized", b'0').raw_size(b"00000000000\0"),
                    b"abc",
                )
                .end(),
        ),
        (
            "pax-size-garbage",
            Tar::new()
                .dir("root")
                .pax(b'x', &[("size", b"zz")])
                .member(
                    Header::new("root/empty", b'0').raw_size(b"00000000000\0"),
                    b"",
                )
                .end(),
        ),
        (
            "pax-linkpath",
            Tar::new()
                .dir("root")
                .file("root/t", b"t", 0o644)
                .pax(b'x', &[("linkpath", b"t")])
                .symlink("root/l", "ignored")
                .end(),
        ),
        (
            "gnu-longname",
            Tar::new()
                .dir("root")
                .gnu_long(b'L', format!("{long}/").as_bytes())
                .member(Header::new("root/dshort/", b'5').mode(0o755), b"")
                .gnu_long(b'L', long_file.as_bytes())
                .file("root/x", b"gnu", 0o755)
                .end(),
        ),
        (
            "gnu-longname-dir",
            Tar::new()
                .dir("root")
                .gnu_long(b'L', b"root/sub/")
                .member(Header::new("root/ignored", b'5').mode(0o755), b"")
                .end(),
        ),
        (
            "gnu-longlink",
            Tar::new()
                .dir("root")
                .file("root/t", b"t", 0o644)
                .gnu_long(b'K', b"t")
                .symlink("root/l", "ignored")
                .end(),
        ),
        (
            "ustar-prefix",
            Tar::new()
                .dir("root")
                .dir("root/p")
                .member(Header::new("name", b'0').prefix("root/p"), b"prefixed")
                .end(),
        ),
        (
            "ustar-prefix-dir",
            Tar::new()
                .dir("root")
                .member(Header::new("q/", b'5').prefix("root"), b"")
                .end(),
        ),
        (
            "old-dir",
            Tar::new()
                .member(Header::new("root/", 0), b"")
                .member(Header::new("root/old/", 0), b"")
                .file("root/old/f", b"o", 0o644)
                .end(),
        ),
        (
            "old-file",
            Tar::new()
                .dir("root")
                .member(Header::new("root/plain", 0), b"plain")
                .end(),
        ),
        (
            "dir-many-slashes",
            Tar::new()
                .member(Header::new("root///", b'5'), b"")
                .member(Header::new("root/d//", b'5'), b"")
                .end(),
        ),
        (
            "file-trailing-slash",
            Tar::new()
                .dir("root")
                .member(Header::new("root/f/", b'0'), b"x")
                .end(),
        ),
        (
            "dot-entry",
            Tar::new()
                .dir("root")
                .member(Header::new("root/.", b'5'), b"")
                .file("root/a", b"a", 0o644)
                .end(),
        ),
        ("dot-inside", base().file("root/./a", b"a", 0o644).end()),
        (
            "exec-bits",
            Tar::new()
                .dir("root")
                .file("root/u", b"u", 0o744)
                .file("root/g", b"g", 0o614)
                .file("root/o", b"o", 0o601)
                .file("root/s", b"s", 0o4755)
                .end(),
        ),
        (
            "mode-base256-negative",
            Tar::new()
                .dir("root")
                .member(
                    Header::new("root/n", b'0')
                        .raw_mode(&[0o377, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff]),
                    b"n",
                )
                .end(),
        ),
        (
            "mode-spaces",
            Tar::new()
                .dir("root")
                .member(Header::new("root/m", b'0').raw_mode(b"  755 \0\0"), b"m")
                .end(),
        ),
        (
            "mode-0o-prefix",
            Tar::new()
                .dir("root")
                .member(Header::new("root/m", b'0').raw_mode(b"0o7_55\0"), b"m")
                .end(),
        ),
        (
            "mode-garbage",
            Tar::new()
                .dir("root")
                .member(Header::new("root/m", b'0').raw_mode(b"75x\0"), b"m")
                .end(),
        ),
        (
            "size-base256",
            Tar::new()
                .dir("root")
                .member(
                    Header::new("root/b", b'0').raw_size(&[0o200, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 4]),
                    b"four",
                )
                .end(),
        ),
        (
            "size-negative",
            Tar::new()
                .dir("root")
                .member(
                    Header::new("root/b", b'0').raw_size(&[
                        0o377, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
                    ]),
                    b"",
                )
                .end(),
        ),
        (
            "contiguous",
            Tar::new()
                .dir("root")
                .member(Header::new("root/c", b'7'), b"c")
                .end(),
        ),
        (
            "fifo",
            Tar::new()
                .dir("root")
                .member(Header::new("root/p", b'6'), b"")
                .end(),
        ),
        (
            "unknown-type",
            Tar::new()
                .dir("root")
                .member(Header::new("root/z", b'Z'), b"zdata")
                .file("root/a", b"a", 0o644)
                .end(),
        ),
        (
            "dir-with-size",
            Tar::new()
                .dir("root")
                .member(
                    Header::new("root/d/", b'5')
                        .mode(0o755)
                        .raw_size(b"00000000005\0"),
                    b"",
                )
                .file("root/d/a", b"a", 0o644)
                .end(),
        ),
        (
            "unicode-names",
            Tar::new()
                .dir("root")
                .file("root/caf\u{e9}", b"e", 0o644)
                .file("root/\u{1f600}", b"s", 0o644)
                .file("root/q\"b\\n", b"q", 0o644)
                .file("root/tab\tnl\nx", b"t", 0o644)
                .symlink("root/ul", "caf\u{e9}")
                .end(),
        ),
        (
            "sorting-bytes",
            Tar::new()
                .dir("root")
                .file("root/\u{e9}", b"1", 0o644)
                .file("root/z", b"2", 0o644)
                .file("root/Z", b"3", 0o644)
                .file("root/a-b", b"4", 0o644)
                .file("root/a.b", b"5", 0o644)
                .end(),
        ),
        (
            "name-not-utf8",
            Tar::new()
                .dir("root")
                .member(Header::new(b"root/\xff".as_slice(), b'0'), b"x")
                .end(),
        ),
        (
            "linkname-not-utf8-on-file",
            Tar::new()
                .dir("root")
                .member(
                    Header::new("root/f", b'0').link(b"\xff\xfe".as_slice()),
                    b"f",
                )
                .end(),
        ),
        (
            "link-dots",
            Tar::new()
                .dir("root")
                .dir("root/a")
                .file("root/b", b"b", 0o644)
                .symlink("root/a/l", "./../b")
                .symlink("root/a/m", "x/../../b")
                .symlink("root/a/o", "..//./b/")
                .end(),
        ),
        (
            "link-dots-escape",
            Tar::new()
                .dir("root")
                .dir("root/a")
                .file("root/b", b"b", 0o644)
                .symlink("root/a/n", "../../b")
                .end(),
        ),
        (
            "link-into-missing",
            Tar::new()
                .dir("root")
                .symlink("root/l", "nowhere/at/all")
                .end(),
        ),
        ("root-not-dir", Tar::new().file("root", b"r", 0o644).end()),
        (
            "no-root-entry",
            Tar::new().file("root/a", b"a", 0o644).end(),
        ),
        ("only-root", Tar::new().dir("root").end()),
        ("no-end-marker", base().bytes),
        ("data-after-end", {
            let mut v = rows(false);
            v.extend_from_slice(b"anything at all after the end blocks");
            v
        }),
        ("garbage-after-member", base().raw(&[0x41u8; BLOCK]).end()),
        ("bad-checksum-after-member", {
            let mut h = Header::new("root/z", b'0');
            h.bad_checksum = true;
            base().member(h, b"z").end()
        }),
        (
            "bad-pax-after-member",
            base()
                .member(Header::new("././@PaxHeader", b'x'), b"9 path\n")
                .file("root/p", b"p", 0o644)
                .end(),
        ),
        (
            "truncated-header-after-member",
            base().raw(&[0x41u8; 100]).bytes,
        ),
        ("bad-checksum-first", {
            let mut h = Header::new("root/", b'5');
            h.bad_checksum = true;
            Tar::new().member(h, b"").end()
        }),
        ("signed-checksum", {
            let mut h = Header::new("root/\u{e9}\u{e9}\u{e9}", b'0');
            h.signed_checksum = true;
            Tar::new().dir("root").member(h, b"s").end()
        }),
        ("truncated-content", {
            let mut v = Tar::new()
                .dir("root")
                .member(
                    Header::new("root/t", b'0').raw_size(b"00000001000\0"),
                    b"short",
                )
                .bytes;
            v.truncate(v.len() - 400);
            v
        }),
        ("empty", Vec::new()),
        ("zero-block", vec![0u8; 2 * BLOCK]),
        ("short-garbage", b"not a tar".to_vec()),
        ("gzip-multi-member", {
            let t = rows(false);
            let (a, b) = t.split_at(3 * BLOCK);
            let mut v = gzip(a, 1);
            v.extend(gzip(b, 2));
            v
        }),
        ("gzip-zero-padding", {
            let mut v = gzip(&rows(false), 5);
            v.extend_from_slice(&[0u8; 37]);
            v
        }),
        ("gzip-trailing-garbage", {
            let mut v = gzip(&rows(false), 5);
            v.extend_from_slice(b"junk");
            v
        }),
        ("gzip-truncated", {
            let v = gzip(&rows(false), 5);
            v[..v.len() - 6].to_vec()
        }),
        ("gzip-bad-crc", {
            let mut v = gzip(&rows(false), 5);
            let n = v.len();
            v[n - 8] ^= 1;
            v
        }),
        ("gzip-not-tar", gzip(b"hello", 1)),
    ];
    out.sort_by_key(|(name, _)| *name);
    out
}

fn native(data: &[u8]) -> String {
    match inspect(data, "root", &Limits::DEFAULT) {
        Ok(inspected) => inspected.digest,
        Err(_) => "refused".into(),
    }
}

fn fixture_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/oracle-verdicts.tsv")
}

fn read_fixture() -> BTreeMap<String, String> {
    let text = std::fs::read_to_string(fixture_path()).expect("oracle fixture");
    text.lines()
        .filter(|line| !line.starts_with('#') && !line.is_empty())
        .map(|line| {
            let (name, verdict) = line.split_once('\t').expect("tab");
            (name.to_owned(), verdict.to_owned())
        })
        .collect()
}

fn oracle(script: &str, data: &[u8], dir: &std::path::Path, name: &str) -> String {
    let path = dir.join(name);
    std::fs::write(&path, data).unwrap();
    let output = std::process::Command::new("python3")
        .arg("-I")
        .arg(script)
        .arg("digest")
        .arg(&path)
        .arg("root")
        .output()
        .expect("python3");
    if output.status.success() {
        String::from_utf8(output.stdout).unwrap().trim().to_owned()
    } else {
        "refused".into()
    }
}

fn agrees(name: &str, reference: &str, ours: &str) -> bool {
    if STRICTER.contains(&name) {
        ours == "refused" && reference != "refused"
    } else {
        ours == reference
    }
}

#[test]
fn every_case_matches_the_reference_verdict() {
    let cases = cases();
    let mut seen = std::collections::BTreeSet::new();
    for (name, _) in &cases {
        assert!(seen.insert(*name), "duplicate case {name}");
    }
    let live = std::env::var("REVIEWED_SOURCE_ORACLE")
        .ok()
        .filter(|s| !s.is_empty());
    if let Some(script) = live {
        let dir = tempfile::tempdir().unwrap();
        let mut lines = vec![
            "# Reference verdicts (digest or refused) of verify-native-source.py digest <case> root.".to_owned(),
            "# Recorded by REVIEWED_SOURCE_RECORD=1 REVIEWED_SOURCE_ORACLE=<script> cargo test -p reviewed-source --test oracle".to_owned(),
        ];
        let mut failures = Vec::new();
        for (name, data) in &cases {
            let reference = oracle(&script, data, dir.path(), name);
            let ours = native(data);
            if !agrees(name, &reference, &ours) {
                failures.push(format!("{name}: reference {reference}, native {ours}"));
            }
            lines.push(format!("{name}\t{reference}"));
        }
        if std::env::var("REVIEWED_SOURCE_RECORD").as_deref() == Ok("1") {
            std::fs::write(fixture_path(), lines.join("\n") + "\n").unwrap();
        }
        assert!(
            failures.is_empty(),
            "live oracle disagreements:\n{}",
            failures.join("\n")
        );
        println!("live oracle: {} cases agree", cases.len());
    }
    let fixture = read_fixture();
    assert_eq!(
        fixture.len(),
        cases.len(),
        "fixture and case list differ in size"
    );
    let mut failures = Vec::new();
    for (name, data) in &cases {
        let reference = fixture
            .get(*name)
            .unwrap_or_else(|| panic!("case {name} missing from the fixture"));
        let ours = native(data);
        if !agrees(name, reference, &ours) {
            failures.push(format!("{name}: reference {reference}, native {ours}"));
        }
    }
    assert!(
        failures.is_empty(),
        "fixture disagreements:\n{}",
        failures.join("\n")
    );
    let accepted = fixture.values().filter(|v| v.as_str() != "refused").count();
    assert!(
        accepted >= 25 && fixture.len() - accepted >= 25,
        "case mix drifted: {accepted} accepted of {}",
        fixture.len()
    );
}

#[test]
fn reference_test_relations_hold() {
    let base = native(&rows(false));
    assert_ne!(base, "refused");
    assert_eq!(
        native(&gzip(&rows(true), 99)),
        base,
        "compression and member order do not change the pin"
    );
    for variant in [
        "rows-missing-build",
        "rows-extra",
        "rows-changed",
        "rows-mode",
    ] {
        let data = &cases().into_iter().find(|(n, _)| *n == variant).unwrap().1;
        let digest = native(data);
        assert_ne!(digest, "refused", "{variant}");
        assert_ne!(digest, base, "{variant} must change the digest");
    }
}

/// A real codeload export: with `REVIEWED_SOURCE_EXPORT=<tarball>`, `REVIEWED_SOURCE_EXPORT_ROOT=<root>` and
/// `REVIEWED_SOURCE_EXPORT_DIGEST=<reviewed digest>` (e.g. the code-sandbox image's verifier pin), the native
/// digest must equal the reviewed one. Skipped (and says so) without them.
#[test]
fn real_export_matches_its_reviewed_pin() {
    let (Ok(path), Ok(root), Ok(expected)) = (
        std::env::var("REVIEWED_SOURCE_EXPORT"),
        std::env::var("REVIEWED_SOURCE_EXPORT_ROOT"),
        std::env::var("REVIEWED_SOURCE_EXPORT_DIGEST"),
    ) else {
        println!("skipped: REVIEWED_SOURCE_EXPORT* not set");
        return;
    };
    let data = std::fs::read(path).unwrap();
    let inspected = inspect(&data, &root, &Limits::DEFAULT).unwrap();
    println!("real export: {} entries", inspected.entries.len());
    assert_eq!(inspected.digest, expected);
}
