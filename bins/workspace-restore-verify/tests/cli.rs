//! The CLI contract the Diary relies on: exit codes, the exact output shapes, the stdin cap.

// Test code: a panic is the failure report; fixture offsets come from archives built here.
#![allow(clippy::panic, clippy::indexing_slicing, clippy::unwrap_used)]

use std::io::Write;
use std::process::{Command, Stdio};

const BIN: &str = env!("CARGO_BIN_EXE_workspace-restore-verify");

fn run_bytes(args: &[&str], stdin: &[u8]) -> (Option<i32>, Vec<u8>) {
    let mut child = Command::new(BIN)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap_or_else(|e| panic!("spawn: {e}"));
    if let Some(mut input) = child.stdin.take() {
        let _ = input.write_all(stdin);
    }
    let out = child
        .wait_with_output()
        .unwrap_or_else(|e| panic!("wait: {e}"));
    (out.status.code(), out.stdout)
}

fn run(args: &[&str], stdin: &[u8]) -> (Option<i32>, String) {
    let (code, bytes) = run_bytes(args, stdin);
    (code, String::from_utf8_lossy(&bytes).into_owned())
}

#[test]
fn self_test_prints_ok() {
    assert_eq!(run(&["self-test"], b""), (Some(0), "ok\n".into()));
}

#[test]
fn garbage_is_a_refusal_with_exit_1() {
    assert_eq!(
        run(&["verify"], b"not a zip"),
        (Some(1), "reject: invalid\n".into())
    );
}

#[test]
fn bad_usage_prints_nothing_and_exits_2() {
    for args in [
        &[][..],
        &["check"][..],
        &["verify", "extra"][..],
        &["decode-v1", "extra"][..],
    ] {
        assert_eq!(run(args, b""), (Some(2), String::new()), "{args:?}");
    }
}

#[test]
fn input_past_the_cap_is_archive_too_large() {
    let body = vec![0u8; 272 * 1024 * 1024 + 1];
    for command in ["verify", "decode-v1"] {
        assert_eq!(
            run(&[command], &body),
            (Some(1), "reject: archive_too_large\n".into())
        );
    }
}

#[test]
fn an_accepted_archive_prints_only_the_listing_digest() {
    let text = include_str!(
        "../../../crates/workspace-restore-verify/tests/fixtures/workspace-restore.v1.json"
    );
    let doc: serde_json::Value = serde_json::from_str(text).unwrap_or_default();
    let case = doc
        .get("cases")
        .and_then(serde_json::Value::as_array)
        .and_then(|c| {
            c.iter().find(|c| {
                c.get("name").and_then(serde_json::Value::as_str) == Some("accept/export")
            })
        })
        .cloned()
        .unwrap_or_default();
    use base64::Engine;
    let body = base64::engine::general_purpose::STANDARD
        .decode(
            case.get("archive_b64")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default(),
        )
        .unwrap_or_default();
    let (code, out) = run(&["verify"], &body);
    assert_eq!(code, Some(0));
    assert_eq!(
        out,
        format!(
            "{}\n",
            case.get("rust")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("<missing>")
        )
    );
    assert!(out.starts_with("accept ") && out.len() == "accept ".len() + 64 + 1);
}

/// Reference framing consumer used ONLY by tests (not a production adapter). The future Diary adapter must also require
/// clean EOF and exit0 before publishing staged bytes. This does not install an adapter.
fn read_decode(code: Option<i32>, bytes: &[u8]) -> Result<serde_json::Value, ()> {
    use workspace_restore_verify::{
        sha256_hex, DECODE_V1_END, DECODE_V1_MAGIC, MAX_DECODE_METADATA, MAX_FILE, MAX_RECORDS,
        MAX_TOTAL,
    };
    if code != Some(0) {
        return Err(());
    }
    let mut cursor = std::io::Cursor::new(bytes);
    fn take<const N: usize>(c: &mut std::io::Cursor<&[u8]>) -> Result<[u8; N], ()> {
        use std::io::Read;
        let mut b = [0; N];
        c.read_exact(&mut b).map_err(|_| ())?;
        Ok(b)
    }
    use std::io::Read;
    let mut magic = vec![0; DECODE_V1_MAGIC.len()];
    cursor.read_exact(&mut magic).map_err(|_| ())?;
    if magic != DECODE_V1_MAGIC {
        return Err(());
    }
    let length = u32::from_le_bytes(take(&mut cursor)?) as usize;
    if length > MAX_DECODE_METADATA {
        return Err(());
    }
    let mut meta = vec![0; length];
    cursor.read_exact(&mut meta).map_err(|_| ())?;
    let meta: serde_json::Value = serde_json::from_slice(&meta).map_err(|_| ())?;
    if meta["version"] != 1 {
        return Err(());
    }
    let hex = |v: &serde_json::Value| {
        v.as_str().is_some_and(|s| {
            s.len() == 64
                && s.bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        })
    };
    if !hex(&meta["fingerprint"]) || !hex(&meta["listingSha256"]) || meta.get("settings").is_none()
    {
        return Err(());
    }
    let files = meta["files"].as_array().ok_or(())?;
    let dirs = meta["directories"].as_array().ok_or(())?;
    if files.len() > MAX_RECORDS || dirs.len() > MAX_RECORDS {
        return Err(());
    }
    let mut total = 0u64;
    let mut records = Vec::new();
    let mut previous: Option<&str> = None;
    for f in files {
        let path = f["path"].as_str().ok_or(())?;
        if !workspace_restore_verify::safe_key(path) || previous.is_some_and(|p| p >= path) {
            return Err(());
        }
        previous = Some(path);
        let n = u64::from_le_bytes(take(&mut cursor)?);
        if !hex(&f["sha256"]) || f["bytes"].as_u64() != Some(n) || n > MAX_FILE {
            return Err(());
        }
        total = total.checked_add(n).filter(|v| *v <= MAX_TOTAL).ok_or(())?;
        let mut data = vec![0; n as usize];
        cursor.read_exact(&mut data).map_err(|_| ())?;
        if f["sha256"].as_str() != Some(sha256_hex(&data).as_str()) {
            return Err(());
        }
        records.push(workspace_restore_verify::VerifiedFile {
            path: path.into(),
            bytes: n,
            sha256: sha256_hex(&data),
        });
    }
    if meta["bytes"].as_u64() != Some(total) {
        return Err(());
    }
    let directories = dirs
        .iter()
        .map(|d| d.as_str().map(str::to_owned).ok_or(()))
        .collect::<Result<Vec<_>, _>>()?;
    if directories
        .iter()
        .any(|d| !workspace_restore_verify::safe_key(d))
        || directories.windows(2).any(|p| p[0] >= p[1])
    {
        return Err(());
    }
    let file_paths: std::collections::HashSet<_> =
        records.iter().map(|f| f.path.as_str()).collect();
    for path in file_paths
        .iter()
        .copied()
        .chain(directories.iter().map(String::as_str))
    {
        if directories.iter().any(|d| d == path) && file_paths.contains(path) {
            return Err(());
        }
        let mut parent = path;
        while let Some(cut) = parent.rfind('/') {
            parent = &parent[..cut];
            if file_paths.contains(parent) {
                return Err(());
            }
        }
    }
    let listing = workspace_restore_verify::Verified {
        files: records,
        directories,
        bytes: total,
    };
    if meta["listingSha256"].as_str() != Some(listing.listing_sha256().as_str()) {
        return Err(());
    }
    let mut end = vec![0; DECODE_V1_END.len()];
    cursor.read_exact(&mut end).map_err(|_| ())?;
    if end != DECODE_V1_END || cursor.position() as usize != bytes.len() {
        return Err(());
    }
    Ok(meta)
}

fn valid_fixture() -> Vec<u8> {
    use base64::Engine;
    let doc: serde_json::Value = serde_json::from_str(include_str!(
        "../../../crates/workspace-restore-verify/tests/fixtures/workspace-restore.v1.json"
    ))
    .unwrap();
    let case = doc["cases"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["name"] == "accept/export")
        .unwrap();
    base64::engine::general_purpose::STANDARD
        .decode(case["archive_b64"].as_str().unwrap())
        .unwrap()
}

#[test]
fn decode_v1_complete_output_preserves_metadata_and_requires_eof_and_exit_zero() {
    let input = valid_fixture();
    let (code, output) = run_bytes(&["decode-v1"], &input);
    let metadata = read_decode(code, &output).unwrap();
    assert_eq!(
        metadata["fingerprint"],
        workspace_restore_verify::sha256_hex(&input)
    );
    let checked = workspace_restore_verify::verify_archive(&input).unwrap();
    assert_eq!(metadata["settings"], *checked.settings());
    assert_eq!(
        metadata["files"].as_array().unwrap().len(),
        checked.files().len()
    );
    for cut in 0..output.len() {
        assert!(
            read_decode(Some(0), &output[..cut]).is_err(),
            "truncation at {cut}"
        );
    }
    for code in [None, Some(1), Some(3)] {
        assert!(read_decode(code, &output).is_err());
    }
    let mut trailing = output.clone();
    trailing.push(0);
    assert!(read_decode(Some(0), &trailing).is_err());
    let mut bad = output.clone();
    bad[0] ^= 1;
    assert!(read_decode(Some(0), &bad).is_err());
    let mut huge = output.clone();
    let at = workspace_restore_verify::DECODE_V1_MAGIC.len();
    huge[at..at + 4].copy_from_slice(&u32::MAX.to_le_bytes());
    assert!(read_decode(Some(0), &huge).is_err());
}

#[test]
fn a_late_archive_failure_emits_only_refusal_not_a_partial_decoded_body() {
    // This fixture passes ZIP/manifest/file validation but has a wrong final total.
    use base64::Engine;
    let doc: serde_json::Value = serde_json::from_str(include_str!(
        "../../../crates/workspace-restore-verify/tests/fixtures/workspace-restore.v1.json"
    ))
    .unwrap();
    let case = doc["cases"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["rust"] == "reject: totals_mismatch")
        .unwrap();
    let body = base64::engine::general_purpose::STANDARD
        .decode(case["archive_b64"].as_str().unwrap())
        .unwrap();
    assert_eq!(
        run(&["decode-v1"], &body),
        (Some(1), "reject: totals_mismatch\n".into())
    );
    assert_eq!(
        run(&["decode-v1"], b"not zip"),
        (Some(1), "reject: invalid\n".into())
    );
}

#[test]
fn a_closed_output_pipe_is_exit_three_never_success() {
    let mut child = Command::new(BIN)
        .arg("decode-v1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    drop(child.stdout.take()); // no reader: first write must fail, without a successful receipt
    child
        .stdin
        .take()
        .unwrap()
        .write_all(&valid_fixture())
        .unwrap();
    assert_eq!(child.wait().unwrap().code(), Some(3));
}

#[test]
fn malformed_decoded_records_are_not_a_successful_receipt() {
    let (code, output) = run_bytes(&["decode-v1"], &valid_fixture());
    assert_eq!(code, Some(0));
    use workspace_restore_verify::DECODE_V1_MAGIC;
    let at = DECODE_V1_MAGIC.len();
    let original_len = u32::from_le_bytes(output[at..at + 4].try_into().unwrap()) as usize;
    let meta_start = at + 4;
    let body_start = meta_start + original_len;
    let original: serde_json::Value =
        serde_json::from_slice(&output[meta_start..body_start]).unwrap();
    for mutation in 0..5 {
        let mut meta = original.clone();
        match mutation {
            0 => meta["version"] = serde_json::json!(2),
            1 => meta["files"][0]["bytes"] = serde_json::json!(u64::MAX),
            2 => meta["files"][0]["sha256"] = serde_json::json!("0".repeat(64)),
            3 => meta["files"][0]["path"] = serde_json::json!("../outside"),
            _ => meta["listingSha256"] = serde_json::json!("0".repeat(64)),
        }
        let encoded = serde_json::to_vec(&meta).unwrap();
        let mut malformed = DECODE_V1_MAGIC.to_vec();
        malformed.extend_from_slice(&(encoded.len() as u32).to_le_bytes());
        malformed.extend_from_slice(&encoded);
        malformed.extend_from_slice(&output[body_start..]);
        assert!(
            read_decode(Some(0), &malformed).is_err(),
            "mutation {mutation}"
        );
    }
    // An advertised payload length cannot override the independently checked metadata.
    let mut bad_length = output.clone();
    bad_length[body_start..body_start + 8].copy_from_slice(&u64::MAX.to_le_bytes());
    assert!(read_decode(Some(0), &bad_length).is_err());
}
