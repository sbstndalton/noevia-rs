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

/// The stored data of the archive's first entry (manifest.json): a 30-byte local header with the
/// size at 18 and the name length at 26, no extra field.
fn first_entry(zip: &[u8]) -> (&str, &[u8]) {
    let size = u32::from_le_bytes(zip[18..22].try_into().unwrap()) as usize;
    let name = u16::from_le_bytes(zip[26..28].try_into().unwrap()) as usize;
    assert_eq!(&zip[28..30], &[0, 0]);
    (
        std::str::from_utf8(&zip[30..30 + name]).unwrap(),
        &zip[30 + name..30 + name + size],
    )
}

#[test]
fn the_manifest_is_the_python_exporters_bytes() {
    // noevia#2652. Both texts are Python's own (python3, stdlib json): the header's settings as
    // workspace_export_native.py writes them, json.dumps(settings, ensure_ascii=False,
    // allow_nan=False, separators=(',', ':')), and the manifest workspace_export.py writes for the
    // same snapshot, json.dumps(manifest, ensure_ascii=False, sort_keys=True, indent=2). The
    // settings were {"zeta": 1, "alpha": {"b": [1.5, 1e-07, 1e16, -0.0, 12345678901234567890123],
    // "a": "月\u0001\"x\\\u007f "}, "Mid": True, "n": None, "e": [], "o": {}}.
    let end = "\u{7f}\u{2028}";
    let settings = r#"{"zeta":1,"alpha":{"b":[1.5,1e-07,1e+16,-0.0,12345678901234567890123],"a":"月\u0001\"x\\<END>"},"Mid":true,"n":null,"e":[],"o":{}}"#.replace("<END>", end);
    let header = format!(
        r#"{{"settings":{settings},"directories":["empty/nested"],"files":[{{"path":"a.txt","bytes":0}},{{"path":"日記/note.md","bytes":5}}]}}"#
    );
    let mut input = MAGIC.to_vec();
    input.extend((header.len() as u32).to_be_bytes());
    input.extend(header.as_bytes());
    input.extend(b"hello");
    let zip = export(input.as_slice()).unwrap();
    workspace_restore_verify::verify(&zip).unwrap();
    let want = r#"{
  "bytes": 5,
  "directories": [
    "empty",
    "empty/nested",
    "日記"
  ],
  "fileCount": 2,
  "files": [
    {
      "bytes": 0,
      "path": "a.txt",
      "sha256": "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
    },
    {
      "bytes": 5,
      "path": "日記/note.md",
      "sha256": "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824"
    }
  ],
  "format": "noevia-workspace-v1",
  "settings": {
    "Mid": true,
    "alpha": {
      "a": "月\u0001\"x\\<END>",
      "b": [
        1.5,
        1e-07,
        1e+16,
        -0.0,
        12345678901234567890123
      ]
    },
    "e": [],
    "n": null,
    "o": {},
    "zeta": 1
  }
}"#
    .replace("<END>", end);
    let (name, manifest) = first_entry(&zip);
    assert_eq!(name, "manifest.json");
    assert_eq!(std::str::from_utf8(manifest).unwrap(), want);
}
