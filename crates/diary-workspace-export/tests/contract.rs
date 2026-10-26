#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
use diary_workspace_export::{export, Reject, MAGIC, MAX_ENTRIES, MAX_FILE, MAX_HEADER};
use serde_json::{json, Value};

fn frame(header: Value, bodies: &[u8]) -> Vec<u8> {
    let header = serde_json::to_vec(&header).unwrap();
    let mut body = MAGIC.to_vec();
    body.extend((header.len() as u32).to_be_bytes());
    body.extend(header);
    body.extend(bodies);
    body
}
fn header() -> Value {
    json!({"settings": {}, "directories": [], "files": []})
}
#[test]
fn empty_and_binary_unicode_exports_verify() {
    let empty = export(frame(header(), &[]).as_slice()).unwrap();
    workspace_restore_verify::verify(&empty).unwrap();
    let input = frame(
        json!({"settings": {"layout": "月", "number": 12345678901234567890u64},
        "directories": ["empty/nested", "empty/nested"], "files": [
            {"path": "日記/note.md", "bytes": 5}, {"path": "manifest.json", "bytes": 3}]}),
        b"a\r\n\0\xffsrc",
    );
    let body = export(input.as_slice()).unwrap();
    workspace_restore_verify::verify(&body).unwrap();
}
#[test]
fn refuse_invalid_frames_without_output() {
    let mut valid = frame(header(), &[]);
    valid.push(0);
    assert_eq!(export(valid.as_slice()), Err(Reject::Frame));
    let valid = frame(header(), &[]);
    assert_eq!(
        export(valid.get(..valid.len() - 1).unwrap()),
        Err(Reject::Frame)
    );
    let mut large = MAGIC.to_vec();
    large.extend(((MAX_HEADER + 1) as u32).to_be_bytes());
    assert_eq!(export(large.as_slice()), Err(Reject::Frame));
    let mut unknown = header();
    unknown["extra"] = json!(true);
    assert_eq!(export(frame(unknown, &[]).as_slice()), Err(Reject::Frame));
}
#[test]
fn declarations_refuse_before_reading_bodies() {
    let cases = [
        (
            json!({"settings":{}, "directories":[], "files":[{"path":"a","bytes": MAX_FILE+1}]}),
            Reject::SourceLimits,
        ),
        (
            json!({"settings":{}, "directories":[], "files":[{"path":"a","bytes":1},{"path":"a","bytes":1}]}),
            Reject::Overlap,
        ),
        (
            json!({"settings":{}, "directories":[], "files":[{"path":"a","bytes":1},{"path":"a/b","bytes":1}]}),
            Reject::Overlap,
        ),
        (
            json!({"settings":{}, "directories":["a/b"], "files":[{"path":"a","bytes":1}]}),
            Reject::Overlap,
        ),
        (
            json!({"settings":{}, "directories":["../bad"], "files":[]}),
            Reject::Path,
        ),
        (
            json!({"settings":{}, "directories":[], "files":[{"path":"a","bytes":1}]}),
            Reject::Frame,
        ),
    ];
    for (header, error) in cases {
        assert_eq!(export(frame(header, &[]).as_slice()), Err(error));
    }
}
#[test]
fn counts_and_inferred_parent_limits() {
    let mut input = header();
    input["directories"] = json!((0..MAX_ENTRIES + 1)
        .map(|i| i.to_string())
        .collect::<Vec<_>>());
    assert_eq!(
        export(frame(input, &[]).as_slice()),
        Err(Reject::Directories)
    );
    let mut input = header();
    input["files"] = json!((0..MAX_ENTRIES + 1)
        .map(|i| json!({"path":i.to_string(),"bytes":0}))
        .collect::<Vec<_>>());
    assert_eq!(export(frame(input, &[]).as_slice()), Err(Reject::Counts));
    let mut input = header();
    input["files"] = json!((0..MAX_ENTRIES)
        .map(|i| json!({"path":format!("a{i}/b/c"),"bytes":0}))
        .collect::<Vec<_>>());
    assert_eq!(
        export(frame(input, &[]).as_slice()),
        Err(Reject::Directories)
    );
}
#[test]
fn cli_refusal_and_success_are_unambiguous() {
    use std::io::Write;
    use std::process::{Command, Stdio};
    for (request, expected) in [(frame(header(), &[]), 0), (b"bad".to_vec(), 2)] {
        let mut child = Command::new(env!("CARGO_BIN_EXE_diary-workspace-export"))
            .arg("export")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child.stdin.take().unwrap().write_all(&request).unwrap();
        let output = child.wait_with_output().unwrap();
        assert_eq!(output.status.code(), Some(expected));
        assert!(output.stderr.is_empty());
        if expected == 0 {
            let start = diary_workspace_export::OUTPUT_MAGIC.len();
            assert!(output
                .stdout
                .starts_with(diary_workspace_export::OUTPUT_MAGIC));
            let len = u64::from_be_bytes(
                output
                    .stdout
                    .get(start..start + 8)
                    .unwrap()
                    .try_into()
                    .unwrap(),
            ) as usize;
            let zip = output.stdout.get(start + 40..start + 40 + len).unwrap();
            use sha2::{Digest, Sha256};
            assert_eq!(
                output.stdout.get(start + 8..start + 40).unwrap(),
                Sha256::digest(zip).as_slice()
            );
            assert_eq!(
                output.stdout.get(start + 40 + len..).unwrap(),
                diary_workspace_export::OUTPUT_END
            );
            workspace_restore_verify::verify(zip).unwrap();
        } else {
            assert_eq!(output.stdout, b"reject: frame\n");
        }
    }
}

#[test]
fn manifest_and_total_limits() {
    let input = json!({"settings": "x".repeat(diary_workspace_export::MAX_MANIFEST),
        "directories": [], "files": []});
    assert_eq!(export(frame(input, &[]).as_slice()), Err(Reject::Manifest));
    let input = json!({"settings": {}, "directories": [], "files":
        (0..5).map(|i| json!({"path": i.to_string(), "bytes": MAX_FILE})).collect::<Vec<_>>()});
    assert_eq!(
        export(frame(input, &[]).as_slice()),
        Err(Reject::SourceLimits)
    );
}
#[test]
fn exact_file_count_boundary_and_path_cases() {
    let input = json!({"settings": {}, "directories": [], "files":
        (0..MAX_ENTRIES).map(|i| json!({"path":i.to_string(),"bytes":0})).collect::<Vec<_>>()});
    let result = export(frame(input, &[]).as_slice()).unwrap();
    workspace_restore_verify::verify(&result).unwrap();
    for path in ["", "/a", "a//b", ".", "a/..", "a/./b", "a\\b", "a\0b"] {
        let input = json!({"settings": {}, "directories": [path], "files": []});
        assert_eq!(export(frame(input, &[]).as_slice()), Err(Reject::Path));
    }
}
