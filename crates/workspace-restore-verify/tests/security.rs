//! The security checks, one by one, on archives built here (synthetic data only).

// Test code: a panic is the failure report; fixture offsets come from archives built here.
#![allow(clippy::panic, clippy::indexing_slicing)]

mod common;

use common::{entry, export, export_entries, zip, Entry, DIR, FILE};
use workspace_restore_verify::{verify, Reject, MAX_ARCHIVE};

const NOTE: &[u8] = b"# Synthetic note\n";

fn base() -> Vec<Entry> {
    export_entries(
        &[("raw/note.md", NOTE), ("a.bin", b"\x00\xff")],
        &["raw", "empty"],
    )
}

fn with(mut entries: Vec<Entry>, name: &str, f: impl Fn(&mut Entry)) -> Vec<u8> {
    for e in &mut entries {
        if e.name == name.as_bytes() {
            f(e);
        }
    }
    zip(&entries)
}

#[test]
fn a_well_formed_export_is_accepted_with_its_listing() {
    let v = verify(&zip(&base())).map_err(Reject::code);
    let v = v.unwrap_or_else(|c| panic!("refused: {c}"));
    let paths: Vec<&str> = v.files.iter().map(|f| f.path.as_str()).collect();
    assert_eq!(paths, ["a.bin", "raw/note.md"]);
    assert_eq!(v.directories, ["empty", "raw"]);
    assert_eq!(v.bytes, (NOTE.len() + 2) as u64);
    assert_eq!(v.listing_sha256().len(), 64);
}

// Corrupt archives.

#[test]
fn every_truncation_and_garbage_is_invalid_never_a_panic() {
    let good = zip(&base());
    for cut in 0..good.len() {
        assert!(
            verify(good.get(..cut).unwrap_or_default()).is_err(),
            "cut {cut}"
        );
    }
    assert_eq!(verify(b"not a zip"), Err(Reject::Invalid));
    assert_eq!(verify(b""), Err(Reject::Invalid));
}

#[test]
fn deflated_members_including_empty_ones_are_accepted() {
    let mut entries = export_entries(&[("empty.md", b""), ("n.md", NOTE)], &[]);
    for e in &mut entries {
        e.deflate = !e.name.ends_with(b"/");
    }
    assert!(verify(&zip(&entries)).is_ok());
}

#[test]
fn corrupt_deflate_and_crc_mismatch_are_invalid() {
    let mut entries = base();
    for e in &mut entries {
        e.deflate = !e.name.ends_with(b"/");
    }
    let good = zip(&entries);
    assert!(verify(&good).is_ok());
    // Overwrite the first two compressed bytes of raw/note.md.
    let mut bad = good.clone();
    let name = b"workspace/raw/note.md";
    let local = bad.windows(name.len()).position(|w| w == name).unwrap_or(0) + name.len();
    if let Some(b) = bad.get_mut(local) {
        *b = 0xff;
    }
    if let Some(b) = bad.get_mut(local + 1) {
        *b = 0xff;
    }
    assert_eq!(verify(&bad), Err(Reject::Invalid));
    let crc = with(base(), "workspace/raw/note.md", |e| e.crc = Some(0));
    assert_eq!(verify(&crc), Err(Reject::Invalid));
}

#[test]
fn local_header_disagreeing_with_central_directory_is_invalid() {
    let good = zip(&base());
    // The local name of the last member differs from its central name (same length).
    let name = b"workspace/raw/note.md";
    let mut bad = good.clone();
    let at = bad.windows(name.len()).position(|w| w == name).unwrap_or(0);
    if let Some(b) = bad.get_mut(at + name.len() - 1) {
        *b = b'x';
    }
    assert_eq!(verify(&bad), Err(Reject::Invalid));
}

#[test]
fn overlapping_members_are_invalid() {
    // Two central entries pointing at the same local header (a "quine"-style overlap).
    let good = zip(&[entry("workspace/", b"")]);
    let eocd = good.len() - 22;
    let cd = u32::from_le_bytes(good[eocd + 16..eocd + 20].try_into().unwrap_or([0; 4])) as usize;
    let central = good[cd..eocd].to_vec();
    let mut bad = good[..cd].to_vec();
    bad.extend_from_slice(&central);
    bad.extend_from_slice(&central);
    bad.extend_from_slice(b"PK\x05\x06\x00\x00\x00\x00\x02\x00\x02\x00");
    bad.extend_from_slice(&((central.len() * 2) as u32).to_le_bytes());
    bad.extend_from_slice(&(cd as u32).to_le_bytes());
    bad.extend_from_slice(&0u16.to_le_bytes());
    // Duplicate names are refused first; give the copy another name of the same length.
    let second = bad.len() - 22 - central.len() + 46;
    if let Some(b) = bad.get_mut(second) {
        *b = b'W';
    }
    assert_eq!(verify(&bad), Err(Reject::Invalid));
}

#[test]
fn comment_prepended_bytes_and_nul_names_are_invalid() {
    let good = zip(&base());
    let mut commented = good.clone();
    let len = commented.len();
    commented[len - 2..].copy_from_slice(&3u16.to_le_bytes());
    commented.extend_from_slice(b"abc");
    assert_eq!(verify(&commented), Err(Reject::Invalid));
    let mut prefixed = b"MZ".to_vec();
    prefixed.extend_from_slice(&good);
    assert_eq!(verify(&prefixed), Err(Reject::Invalid));
    let nul = with(base(), "workspace/a.bin", |e| {
        e.name = b"workspace/a.bin\0junk".to_vec()
    });
    assert_eq!(verify(&nul), Err(Reject::Invalid));
}

// Path traversal.

#[test]
fn traversal_and_unsafe_names_are_refused() {
    for name in [
        "../escape",
        "/absolute",
        "workspace/../../escape",
        "workspace/a\\b",
        "workspace/./x",
        "workspace//x",
        "workspace/..",
        "/",
    ] {
        let mut entries = base();
        entries.push(entry(name, b"bad"));
        assert_eq!(verify(&zip(&entries)), Err(Reject::Path), "{name}");
    }
    let manifest = common::manifest(&[("../escape", NOTE)], &[]);
    let entries = vec![entry("manifest.json", &manifest), entry("workspace/", b"")];
    assert_eq!(verify(&zip(&entries)), Err(Reject::Path));
    let manifest = common::manifest(&[], &["a/../../b"]);
    let entries = vec![entry("manifest.json", &manifest), entry("workspace/", b"")];
    assert_eq!(verify(&zip(&entries)), Err(Reject::Path));
    // An unlisted but safe entry is still refused.
    let mut entries = base();
    entries.push(entry("workspace/extra", b"bad"));
    assert_eq!(verify(&zip(&entries)), Err(Reject::EntriesMismatch));
}

// Symlinks and special files.

#[test]
fn links_special_files_and_encryption_are_refused() {
    for fmt in [0o120_000u32, 0o010_000, 0o020_000, 0o060_000, 0o140_000] {
        let bad = with(base(), "workspace/raw/note.md", |e| e.mode = fmt | 0o777);
        assert_eq!(verify(&bad), Err(Reject::EntryType), "{fmt:o}");
        let bad = with(base(), "workspace/empty/", |e| e.mode = fmt | 0o777);
        assert_eq!(verify(&bad), Err(Reject::EntryType), "dir {fmt:o}");
    }
    let enc = with(base(), "workspace/raw/note.md", |e| e.flags = 1);
    assert_eq!(verify(&enc), Err(Reject::EntryType));
    let dir_on_file = with(base(), "workspace/raw/note.md", |e| e.mode = DIR);
    assert_eq!(verify(&dir_on_file), Err(Reject::TypeMismatch));
    let file_on_dir = with(base(), "workspace/empty/", |e| e.mode = FILE);
    assert_eq!(verify(&file_on_dir), Err(Reject::TypeMismatch));
    let zero = with(base(), "workspace/raw/note.md", |e| e.mode = 0);
    assert!(verify(&zero).is_ok());
}

// Size bombs.

#[test]
fn declared_sizes_over_the_caps_are_refused_before_inflating() {
    let big = with(base(), "workspace/raw/note.md", |e| {
        e.usize = Some(64 * 1024 * 1024 + 1)
    });
    assert_eq!(verify(&big), Err(Reject::EntryLimits));
    let mut entries = base();
    for i in 0..5 {
        let mut e = entry(&format!("workspace/bomb{i}"), b"x");
        e.usize = Some(60 * 1024 * 1024);
        entries.push(e);
    }
    assert_eq!(verify(&zip(&entries)), Err(Reject::Expanded));
    let manifest = with(base(), "manifest.json", |e| {
        e.usize = Some(8 * 1024 * 1024 + 1)
    });
    assert_eq!(verify(&manifest), Err(Reject::ManifestSize));
    let payload = with(base(), "workspace/empty/", |e| e.data = b"payload".to_vec());
    assert_eq!(verify(&payload), Err(Reject::DirectoryNotEmpty));
}

#[test]
fn deflate_output_beyond_the_declared_size_is_invalid() {
    // 1 MiB of zeros compresses to about 1 KiB; the headers claim 12 bytes.
    let zeros = vec![0u8; 1024 * 1024];
    let mut e = entry("workspace/bomb.md", &zeros);
    e.deflate = true;
    e.usize = Some(12);
    e.crc = Some(workspace_restore_verify::crc32(&zeros[..12]));
    let manifest = common::manifest(&[("bomb.md", &zeros[..12])], &[]);
    let entries = vec![
        entry("manifest.json", &manifest),
        entry("workspace/", b""),
        e,
    ];
    assert_eq!(verify(&zip(&entries)), Err(Reject::Invalid));
}

#[test]
fn too_many_entries_and_duplicates_are_refused() {
    let mut entries = vec![entry("workspace/", b"")];
    entries.extend((0..10_002).map(|i| entry(&format!("workspace/f{i}"), b"")));
    assert_eq!(verify(&zip(&entries)), Err(Reject::Entries));
    let dup = zip(&[entry("manifest.json", b"{}"), entry("manifest.json", b"{}")]);
    assert_eq!(verify(&dup), Err(Reject::Entries));
}

#[test]
fn an_archive_over_272_mib_is_refused_without_parsing() {
    let body = vec![0u8; MAX_ARCHIVE + 1];
    assert_eq!(verify(&body), Err(Reject::ArchiveTooLarge));
}

// Checksum mismatch and manifest consistency.

#[test]
fn checksum_size_and_totals_mismatches_are_refused() {
    let mut entries = base();
    for e in &mut entries {
        if e.name == b"workspace/raw/note.md" {
            e.data = b"# Changed note!!\n".to_vec();
        }
    }
    assert_eq!(verify(&zip(&entries)), Err(Reject::Checksum));
    let manifest = String::from_utf8(common::manifest(&[("n.md", NOTE)], &[])).unwrap_or_default();
    let upper = {
        let sha = workspace_restore_verify::sha256_hex(NOTE);
        manifest.replace(&sha, &sha.to_uppercase())
    };
    let entries = vec![
        entry("manifest.json", upper.as_bytes()),
        entry("workspace/", b""),
        entry("workspace/n.md", NOTE),
    ];
    assert_eq!(verify(&zip(&entries)), Err(Reject::Checksum));
    let wrong_total = manifest.replace("\"fileCount\": 1", "\"fileCount\": 2");
    let entries = vec![
        entry("manifest.json", wrong_total.as_bytes()),
        entry("workspace/", b""),
        entry("workspace/n.md", NOTE),
    ];
    assert_eq!(verify(&zip(&entries)), Err(Reject::TotalsMismatch));
    let bool_total = manifest.replace("\"fileCount\": 1", "\"fileCount\": true");
    let entries = vec![
        entry("manifest.json", bool_total.as_bytes()),
        entry("workspace/", b""),
        entry("workspace/n.md", NOTE),
    ];
    assert_eq!(verify(&zip(&entries)), Err(Reject::TotalsInvalid));
    assert_eq!(
        verify(&export(&[("a", b"1"), ("a/b", b"2")], &[])),
        Err(Reject::ParentOverlap)
    );
}
