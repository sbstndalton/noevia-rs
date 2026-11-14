//! Ports of the reference's own unit tests (code-sandbox/tests/verify-native-source.test.py), plus the CLI.
#![allow(
    clippy::indexing_slicing,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic
)]

mod common;
use common::{base, gzip, rows, Header, Tar};
use reviewed_source::{extract, inspect, Error, Limits};
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

fn digest(data: &[u8]) -> String {
    inspect(data, "root", &Limits::DEFAULT).unwrap().digest
}

fn mode(path: &Path) -> u32 {
    std::fs::symlink_metadata(path)
        .unwrap()
        .permissions()
        .mode()
        & 0o7777
}

fn reason(result: Result<String, Error>) -> String {
    match result {
        Err(Error::Refused(refused)) => refused.0,
        Err(Error::Io(error)) => format!("io: {:?}", error.kind()),
        Ok(digest) => panic!("expected a refusal, got {digest}"),
    }
}

#[test]
fn compression_owner_timestamp_do_not_change_content_pin() {
    let first = rows(false);
    let pin = digest(&first);
    let second = gzip(&rows(true), 99);
    assert_eq!(digest(&second), pin);
    let parent = tempfile::tempdir().unwrap();
    let dest = parent.path().join("verified");
    assert_eq!(
        extract(&second, "root", &pin, &dest, &Limits::DEFAULT).unwrap(),
        pin
    );
    assert_eq!(
        std::fs::read(dest.join("src/main")).unwrap(),
        b"reviewed source"
    );
    assert_eq!(mode(&dest.join("build")), 0o755);
    assert_eq!(mode(&dest.join("src/main")), 0o644);
    assert_eq!(
        reason(extract(&first, "root", &pin, &dest, &Limits::DEFAULT)),
        "io: AlreadyExists"
    );
}

#[test]
fn contents_path_set_and_exec_mode_are_bound_before_any_write() {
    let pin = digest(&rows(false));
    let variants = [
        Tar::new()
            .dir("root")
            .dir("root/src")
            .file("root/src/main", b"reviewed source", 0o644)
            .end(),
        base().file("root/extra", b"x", 0o644).end(),
        Tar::new()
            .dir("root")
            .dir("root/src")
            .file("root/src/main", b"reviewed source", 0o644)
            .file("root/build", b"changed", 0o755)
            .end(),
        Tar::new()
            .dir("root")
            .dir("root/src")
            .file("root/src/main", b"reviewed source", 0o644)
            .file("root/build", b"#!/bin/sh\n", 0o644)
            .end(),
    ];
    for data in variants {
        let parent = tempfile::tempdir().unwrap();
        let dest = parent.path().join("untouched");
        assert_eq!(
            reason(extract(&data, "root", &pin, &dest, &Limits::DEFAULT)),
            "source differs from reviewed Git export"
        );
        assert!(!dest.exists());
    }
}

#[test]
fn relative_links_are_preserved_but_never_traversed() {
    let data = base().symlink("root/link", "src/main").end();
    let pin = digest(&data);
    let parent = tempfile::tempdir().unwrap();
    let dest = parent.path().join("verified");
    extract(&data, "root", &pin, &dest, &Limits::DEFAULT).unwrap();
    assert!(std::fs::symlink_metadata(dest.join("link"))
        .unwrap()
        .file_type()
        .is_symlink());
    assert_eq!(
        std::fs::read(dest.join("link")).unwrap(),
        b"reviewed source"
    );
    let bad = base()
        .symlink("root/link", "src/main")
        .file("root/link/child", b"outside", 0o644)
        .end();
    assert_eq!(
        inspect(&bad, "root", &Limits::DEFAULT).unwrap_err().0,
        "missing or non-directory source parent"
    );
}

#[test]
fn refuses_ambiguous_escaping_duplicate_and_unsupported_entries() {
    let cases: [(Vec<u8>, &str); 9] = [
        (
            base().file("root/build", b"#!/bin/sh\n", 0o755).end(),
            "duplicate source entry",
        ),
        (base().dir("root").end(), "duplicate source root"),
        (
            base().file("other/file", b"x", 0o644).end(),
            "unexpected source root",
        ),
        (
            base().file("root/../escape", b"x", 0o644).end(),
            "noncanonical source path",
        ),
        (
            base().file("root/src//bad", b"x", 0o644).end(),
            "noncanonical source path",
        ),
        (
            base().symlink("root/absolute", "/tmp/out").end(),
            "unsafe source link",
        ),
        (
            base().symlink("root/escaping", "../out").end(),
            "escaping source link",
        ),
        (
            base()
                .member(Header::new("root/hard", b'1').link("root/src/main"), b"")
                .end(),
            "unsupported source type",
        ),
        (
            Tar::new().file("root/absent/file", b"x", 0o644).end(),
            "missing or non-directory source parent",
        ),
    ];
    for (data, expected) in cases {
        assert_eq!(
            inspect(&data, "root", &Limits::DEFAULT).unwrap_err().0,
            expected
        );
    }
}

#[test]
fn gnu_and_pax_sparse_entries_refuse_before_any_write() {
    let gnu = Tar::new()
        .dir("root")
        .member(Header::new("root/sparse", b'S'), b"x")
        .end();
    let pax = Tar::new()
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
        .end();
    for data in [gnu, pax] {
        let parent = tempfile::tempdir().unwrap();
        let dest = parent.path().join("must-not-exist");
        assert_eq!(
            reason(extract(
                &data,
                "root",
                &"0".repeat(64),
                &dest,
                &Limits::DEFAULT
            )),
            "sparse source entry refused"
        );
        assert!(!dest.exists());
    }
}

#[test]
fn aggregate_budget_rejects_the_overbudget_file() {
    let data = Tar::new()
        .dir("root")
        .file("root/one", &[b'a'; 16], 0o644)
        .file("root/two", &[b'b'; 16], 0o644)
        .end();
    let limits = Limits {
        max_content: 24,
        ..Limits::DEFAULT
    };
    let parent = tempfile::tempdir().unwrap();
    let dest = parent.path().join("must-not-exist");
    assert_eq!(
        reason(extract(&data, "root", &"0".repeat(64), &dest, &limits)),
        "aggregate source content limit"
    );
    assert!(!dest.exists());
}

#[test]
fn compressed_bomb_input_size_and_entry_count_are_bounded() {
    let small = Limits {
        max_archive: 2048,
        max_expanded: 4096,
        ..Limits::DEFAULT
    };
    assert_eq!(
        inspect(&[b'x'; 4097], "root", &small).unwrap_err().0,
        "source size limit"
    );
    assert_eq!(
        inspect(&gzip(&[0u8; 4097], 0), "root", &small)
            .unwrap_err()
            .0,
        "source size limit"
    );
    let few = Limits {
        max_entries: 2,
        ..Limits::DEFAULT
    };
    assert_eq!(
        inspect(&rows(false), "root", &few).unwrap_err().0,
        "source entry limit"
    );
    // Exactly at the bound passes: root + 3 entries = 4 members.
    let four = Limits {
        max_entries: 4,
        ..Limits::DEFAULT
    };
    assert!(inspect(&rows(false), "root", &four).is_ok());
    let file = Limits {
        max_expanded: 12,
        ..Limits::DEFAULT
    };
    let data = Tar::new()
        .dir("root")
        .file("root/big", &[b'b'; 13], 0o644)
        .end();
    assert!(data.len() > 12);
    let gz = gzip(&data, 0);
    assert_eq!(
        inspect(
            &gz,
            "root",
            &Limits {
                max_archive: 1 << 20,
                ..file
            }
        )
        .unwrap_err()
        .0,
        "source size limit"
    );
}

#[test]
fn invalid_arguments_refuse_before_reading() {
    let parent = tempfile::tempdir().unwrap();
    let dest = parent.path().join("d");
    assert_eq!(
        reason(extract(
            &rows(false),
            "root",
            "ABC",
            &dest,
            &Limits::DEFAULT
        )),
        "invalid reviewed digest"
    );
    assert_eq!(
        inspect(&rows(false), "a/b", &Limits::DEFAULT)
            .unwrap_err()
            .0,
        "invalid source root"
    );
    assert_eq!(
        inspect(&rows(false), "", &Limits::DEFAULT).unwrap_err().0,
        "noncanonical source path"
    );
    assert_eq!(
        reason(extract(
            &rows(false),
            "root",
            &digest(&rows(false)),
            &parent.path().join("no/parent"),
            &Limits::DEFAULT
        )),
        "io: NotFound"
    );
}

fn cli(args: &[&str]) -> (i32, String, String) {
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_reviewed-source"))
        .args(args)
        .output()
        .unwrap();
    (
        output.status.code().unwrap_or(-1),
        String::from_utf8(output.stdout).unwrap(),
        String::from_utf8(output.stderr).unwrap(),
    )
}

#[test]
fn cli_digest_extract_and_refusal_line() {
    const LINE: &str = "native verifier source does not match the reviewed export\n";
    let dir = tempfile::tempdir().unwrap();
    let archive = dir.path().join("a.tar.gz");
    std::fs::write(&archive, gzip(&rows(false), 7)).unwrap();
    let archive = archive.to_str().unwrap();
    let pin = digest(&rows(false));
    assert_eq!(
        cli(&["digest", archive, "root"]),
        (0, format!("{pin}\n"), String::new())
    );
    let dest = dir.path().join("out");
    let dest = dest.to_str().unwrap();
    assert_eq!(
        cli(&["extract", archive, "root", &pin, dest]),
        (0, format!("{pin}\n"), String::new())
    );
    assert_eq!(
        std::fs::read(Path::new(dest).join("build")).unwrap(),
        b"#!/bin/sh\n"
    );
    assert_eq!(
        cli(&["extract", archive, "root", &pin, dest]),
        (1, String::new(), LINE.into())
    );
    assert_eq!(
        cli(&[
            "extract",
            archive,
            "root",
            &"0".repeat(64),
            dir.path().join("other").to_str().unwrap()
        ]),
        (1, String::new(), LINE.into())
    );
    assert!(!dir.path().join("other").exists());
    assert_eq!(cli(&["digest", archive]), (1, String::new(), LINE.into()));
    assert_eq!(cli(&[]), (1, String::new(), LINE.into()));
    assert_eq!(
        cli(&["fetch", &"a".repeat(40), &pin, dest]),
        (1, String::new(), LINE.into())
    );
    assert_eq!(
        cli(&[
            "digest",
            dir.path().join("missing").to_str().unwrap(),
            "root"
        ]),
        (1, String::new(), LINE.into())
    );
}

#[test]
fn cli_extract_ref_derives_the_export_root() {
    let reference = "0123456789abcdef0123456789abcdef01234567";
    let root = format!("noevia-rs-{reference}");
    let data = Tar::new()
        .dir(&root)
        .file(&format!("{root}/Cargo.toml"), b"[workspace]\n", 0o664)
        .end();
    let pin = inspect(&data, &root, &Limits::DEFAULT).unwrap().digest;
    let dir = tempfile::tempdir().unwrap();
    let archive = dir.path().join("rs.tgz");
    std::fs::write(&archive, gzip(&data, 0)).unwrap();
    let archive = archive.to_str().unwrap();
    let dest = dir.path().join("src");
    assert_eq!(
        cli(&[
            "extract-ref",
            archive,
            reference,
            &pin,
            dest.to_str().unwrap()
        ])
        .0,
        0
    );
    assert_eq!(
        std::fs::read(dest.join("Cargo.toml")).unwrap(),
        b"[workspace]\n"
    );
    assert_eq!(mode(&dest.join("Cargo.toml")), 0o644);
    assert_eq!(mode(&dest), 0o700 & !umask_bits(&dir));
    assert_eq!(
        cli(&[
            "extract-ref",
            archive,
            &reference.to_uppercase(),
            &pin,
            dir.path().join("x").to_str().unwrap()
        ])
        .0,
        1
    );
    assert_eq!(
        cli(&[
            "extract-ref",
            archive,
            &reference[..39],
            &pin,
            dir.path().join("y").to_str().unwrap()
        ])
        .0,
        1
    );
}

/// The process umask, read back from a fresh 0777 directory.
fn umask_bits(dir: &tempfile::TempDir) -> u32 {
    use std::os::unix::fs::DirBuilderExt;
    let probe = dir.path().join("umask-probe");
    std::fs::DirBuilder::new()
        .mode(0o777)
        .create(&probe)
        .unwrap();
    0o777 & !mode(&probe)
}
