//! Node-differential tests for `server_export::route` (#1315): every case of
//! tests/fixtures/node-export-route.v1.json (made by tests/route-oracle.cjs from core's own
//! workspace store, readHistory, export route and http.cjs over synthetic tenants on disk) is laid
//! out the same way and exported natively. The answer must be Node's: the same ZIP bytes (and
//! filename day), the same 413 message, the same 500, except for the cases marked `refusal`
//! (route.rs's narrower admission), which must be the front's refusal. Another tenant's data is on
//! disk in every case and must never appear.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]

use base64::Engine as _;
use serde_json::Value;
use server_export::route::{export, Outcome};
use sha2::{Digest, Sha256};
use std::path::Path;

fn fixture() -> Value {
    let path = std::env::var("NOEVIA_EXPORT_ROUTE_FIXTURE").unwrap_or_else(|_| {
        concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/node-export-route.v1.json"
        )
        .to_string()
    });
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

fn bytes_of(spec: &Value) -> Option<Vec<u8>> {
    if let Some(s) = spec.as_str() {
        return Some(s.as_bytes().to_vec());
    }
    if let Some(h) = spec.get("hex").and_then(Value::as_str) {
        return Some(hex(h));
    }
    if let Some(unit) = spec.get("repeat").and_then(Value::as_str) {
        let n = spec["count"].as_u64().unwrap() as usize;
        let mut out = spec
            .get("prefix")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        out.push_str(&unit.repeat(n));
        out.push_str(spec.get("suffix").and_then(Value::as_str).unwrap_or(""));
        return Some(out.into_bytes());
    }
    None
}

fn hex(s: &str) -> Vec<u8> {
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
        .collect()
}

fn layout(root: &Path, user: &str, files: &Value) {
    let dir = root.join("users").join(user);
    std::fs::create_dir_all(&dir).unwrap();
    for (name, spec) in files.as_object().unwrap() {
        let target = dir.join(name);
        match bytes_of(spec) {
            Some(b) => std::fs::write(target, b).unwrap(),
            None => std::fs::create_dir_all(target).unwrap(),
        }
    }
}

/// Stored entries, so the payloads are plain in the ZIP: nothing of the other tenant, of the
/// Diary-internal projects, or of reasoning ever appears.
fn assert_no_secrets(zip: &[u8], case: &str, other: &str) {
    let text = String::from_utf8_lossy(zip);
    for secret in [other, "HIDDEN", "SECRET_REASONING"] {
        assert!(!text.contains(secret), "{case}: {secret} leaked");
    }
}

#[test]
fn every_node_case_is_answered_as_node_answers_it() {
    let f = fixture();
    let (a, b) = (
        f["users"]["a"].as_str().unwrap(),
        f["users"]["b"].as_str().unwrap(),
    );
    let mut checked = 0;
    for c in f["cases"].as_array().unwrap() {
        let id = c["id"].as_str().unwrap();
        let root = tempfile::tempdir().unwrap();
        layout(root.path(), b, &f["other"]);
        if c.get("missingDir").is_none() {
            layout(root.path(), a, &c["files"]);
        }
        let user = c["user"].as_str().unwrap();
        let now = c["now"].as_i64().unwrap();
        let got = export(root.path(), user, now);
        let node = &c["node"];
        if c.get("refusal").is_some() {
            assert_eq!(got, Outcome::Internal, "{id}: the front's refusal");
            checked += 1;
            continue;
        }
        match node["status"].as_u64().unwrap() {
            200 => {
                let Outcome::Zip { bytes, day } = got else {
                    panic!("{id}: Node answered 200, the front {got:?}");
                };
                assert_eq!(
                    hex::encode(Sha256::digest(&bytes)),
                    node["zipSha256"].as_str().unwrap(),
                    "{id}: ZIP bytes"
                );
                assert_eq!(bytes.len() as u64, node["zipLength"].as_u64().unwrap());
                if let Some(z) = node.get("zip").and_then(Value::as_str) {
                    let want = base64::engine::general_purpose::STANDARD.decode(z).unwrap();
                    assert_eq!(bytes, want, "{id}: ZIP bytes");
                }
                assert_eq!(
                    format!("attachment; filename=\"noevia-conversations-{day}.zip\""),
                    node["headers"]["content-disposition"].as_str().unwrap(),
                    "{id}"
                );
                assert_no_secrets(&bytes, id, if user == a { "B SECRET" } else { "A SECRET" });
            }
            413 => {
                let Outcome::TooLarge(message) = got else {
                    panic!("{id}: Node answered 413, the front {got:?}");
                };
                let body: Value = serde_json::from_str(node["body"].as_str().unwrap()).unwrap();
                assert_eq!(body["error"].as_str().unwrap(), message, "{id}");
            }
            500 => {
                assert_eq!(got, Outcome::Internal, "{id}");
                assert_eq!(node["body"], "{\"error\":\"Internal error\"}");
            }
            other => panic!("{id}: unexpected Node status {other}"),
        }
        checked += 1;
    }
    assert!(checked >= 25, "{checked}");
}

#[test]
fn the_fixture_covers_what_it_claims() {
    let f = fixture();
    let cases = f["cases"].as_array().unwrap();
    let status = |s: u64| cases.iter().filter(|c| c["node"]["status"] == s).count();
    assert!(status(200) >= 10 && status(413) == 2 && status(500) >= 6);
    // Node audits exactly the downloads, with the byte count.
    for c in cases {
        let audits = c["node"]["audits"].as_array().unwrap();
        if c["node"]["status"] == 200 {
            let user = c["user"].as_str().unwrap();
            assert_eq!(audits.len(), 1);
            assert_eq!(audits[0][0], "export.conversations");
            assert_eq!(audits[0][1], user);
            assert_eq!(audits[0][2], user);
            assert_eq!(
                audits[0][3],
                format!("{{\"bytes\":{}}}", c["node"]["zipLength"])
            );
        } else {
            assert!(audits.is_empty());
        }
    }
}

#[test]
fn oversized_history_files_are_refused_before_they_are_read_whole() {
    let root = tempfile::tempdir().unwrap();
    let user = "aaaaaaaa-1111-4111-8111-aaaaaaaaaaaa";
    let dir = root.path().join("users").join(user);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("free-chats.json"), r#"[{"id":"big"}]"#).unwrap();
    let f = std::fs::File::create(dir.join("history-big.json")).unwrap();
    f.set_len(server_export::route::HISTORY_READ_CAP + 1)
        .unwrap();
    assert!(matches!(
        export(root.path(), user, 1_800_000_000_000),
        Outcome::TooLarge(_)
    ));
    // A user id that is not workspace.cjs's form never names a directory.
    for bad in ["../users/x", "", "aaaaaaaa-1111-4111-8111-aaaaaaaaaaa/"] {
        assert_eq!(export(root.path(), bad, 0), Outcome::Internal);
    }
}
