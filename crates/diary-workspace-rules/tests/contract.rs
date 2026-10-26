#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]
use base64::{engine::general_purpose::STANDARD, Engine as _};
use diary_workspace_rules::{
    check_tree_bounds, decode_legacy, decode_strict, encode_writer, folder_version, normalize_uuid,
    rel_path, replaced_name, safe_file_path, sha256, trashable, FileVersion, NewCapsule,
    ProtectionConfig, TrashState,
};
use serde_json::Value;

fn fixtures() -> Value {
    serde_json::from_str(include_str!("fixtures/python.json")).unwrap()
}
fn string(v: &Value) -> &str {
    v.as_str().unwrap()
}

#[test]
fn shipped_python_protected_paths_and_locale_templates() {
    for row in fixtures()["protection"].as_array().unwrap() {
        let cfg = &row["config"];
        let input = ProtectionConfig {
            remote_root: string(&cfg["remote_root"]).into(),
            entries_prefix: string(&cfg["entries_prefix"]).into(),
            monthly_prefix: string(&cfg["monthly_prefix"]).into(),
            month_file_template: string(&cfg["month_file_template"]).into(),
            index_file: string(&cfg["index_file"]).into(),
            month_names: cfg["month_names"]
                .as_array()
                .unwrap()
                .iter()
                .map(|s| string(s).into())
                .collect(),
        };
        assert_eq!(
            input.protected(string(&row["path"])).unwrap(),
            row["expected"].as_bool().unwrap(),
            "{row}"
        );
    }
}

#[test]
fn shipped_python_folder_etags_include_unicode_prefixes_and_explicit_dirs() {
    for row in fixtures()["folder"].as_array().unwrap() {
        let files = row["files"]
            .as_array()
            .unwrap()
            .iter()
            .map(|f| FileVersion {
                path: string(&f["path"]).into(),
                version: string(&f["version"]).into(),
            })
            .collect::<Vec<_>>();
        let dirs = row["dirs"]
            .as_array()
            .unwrap()
            .iter()
            .map(|d| string(d).into())
            .collect::<Vec<_>>();
        assert_eq!(
            folder_version(&files, &dirs, string(&row["full"])),
            string(&row["expected"]),
            "{row}"
        );
    }
}

#[test]
fn shipped_python_path_and_uuid_rules() {
    for row in fixtures()["paths"].as_array().unwrap() {
        let path = string(&row["path"]);
        let actual = match string(&row["mode"]) {
            "safe" => safe_file_path(path).ok().map(|()| path.to_owned()),
            "rel" => rel_path(path, false).ok(),
            "rel_root" => rel_path(path, true).ok(),
            other => panic!("unknown mode {other}"),
        };
        assert_eq!(actual.as_deref(), row["expected"].as_str(), "{row}");
    }
    for row in fixtures()["identity"].as_array().unwrap() {
        assert_eq!(
            normalize_uuid(string(&row["input"])).ok().as_deref(),
            row["expected"].as_str(),
            "{row}"
        );
    }
}

#[test]
fn shipped_python_trash_capsule_exact_bytes_and_hashes() {
    for row in fixtures()["capsules"].as_array().unwrap() {
        let raw = STANDARD.decode(string(&row["raw"])).unwrap();
        let state = match string(&row["state"]) {
            "trashed" => TrashState::Trashed,
            _ => TrashState::Restored,
        };
        let encoded = encode_writer(&NewCapsule {
            id: string(&row["id"]),
            path: string(&row["path"]),
            raw: &raw,
            state,
            trashed_at: f64::from_bits(
                u64::from_str_radix(string(&row["trashedAtBits"]), 16).unwrap(),
            ),
            restored_at: row["restoredAtBits"]
                .as_str()
                .map(|s| f64::from_bits(u64::from_str_radix(s, 16).unwrap())),
        })
        .unwrap();
        assert_eq!(encoded, string(&row["expected"]).as_bytes(), "{row}");
        assert_eq!(sha256(&encoded), string(&row["sha256"]));
        let decoded = decode_strict(&encoded, string(&row["id"])).unwrap();
        assert_eq!(decoded.raw, raw);
    }
}

#[test]
fn shipped_python_legacy_read_is_distinct_from_strict_writer() {
    let rows = fixtures();
    let mut legacy_only = 0;
    for row in rows["legacy"].as_array().unwrap() {
        let bytes = string(&row["encoded"]).as_bytes();
        let operation = string(&row["operation"]);
        let actual = decode_legacy(bytes, operation).is_ok();
        assert_eq!(actual, row["valid"].as_bool().unwrap(), "{row}");
        if actual && decode_strict(bytes, operation).is_err() {
            legacy_only += 1;
        }
    }
    assert!(legacy_only >= 2);
}

#[test]
fn marker_and_size_limits_are_distinct_from_empty_file() {
    assert!(trashable(b"").is_ok());
    assert!(trashable(b"text <!-- xid:abcd-1234 -->").is_err());
    assert!(trashable(&vec![b'x'; 512 * 1024]).is_ok());
    assert!(trashable(&vec![b'x'; 512 * 1024 + 1]).is_err());
}

#[test]
fn shipped_python_replaced_names_and_capture_whitespace() {
    let f = fixtures();
    for row in f["replaced"].as_array().unwrap() {
        let at = f64::from_bits(u64::from_str_radix(string(&row["atBits"]), 16).unwrap());
        assert_eq!(
            replaced_name(string(&row["path"]), at).unwrap(),
            string(&row["expected"]),
            "{row}"
        );
    }
    for row in f["markers"].as_array().unwrap() {
        let raw = STANDARD.decode(string(&row["raw"])).unwrap();
        assert_eq!(
            trashable(&raw).is_ok(),
            row["trashable"].as_bool().unwrap(),
            "{row}"
        );
    }
}

#[test]
fn operation_bounds_match_inclusive_python_caps() {
    assert!(check_tree_bounds(500, 0, 50 * 1024 * 1024).is_ok());
    assert!(check_tree_bounds(499, 1, 0).is_ok());
    assert!(check_tree_bounds(501, 0, 0).is_err());
    assert!(check_tree_bounds(0, 0, 50 * 1024 * 1024 + 1).is_err());
}

#[test]
fn python312_unicode_uuid_prefix_edges() {
    let data: Value = serde_json::from_str(include_str!("fixtures/python312-edges.json")).unwrap();
    for row in data["identity"].as_array().unwrap() {
        assert_eq!(
            normalize_uuid(string(&row["input"])).ok().as_deref(),
            row["expected"].as_str(),
            "{row}"
        );
    }
}

#[test]
fn python312_legacy_base64_edges() {
    let data: Value = serde_json::from_str(include_str!("fixtures/python312-edges.json")).unwrap();
    for row in data["base64"].as_array().unwrap() {
        let encoded = string(&row["encoded"]).as_bytes();
        let actual = decode_legacy(encoded, string(&row["operation"]));
        assert_eq!(actual.is_ok(), row["valid"].as_bool().unwrap(), "{row}");
        if let Ok(actual) = actual {
            assert_eq!(
                actual.raw,
                STANDARD.decode(string(&row["raw"])).unwrap(),
                "{row}"
            );
            assert_eq!(
                actual.encoded, encoded,
                "legacy bytes must not be normalized"
            );
        }
    }
}

#[test]
fn legacy_tail_bits_do_not_relax_strict_writer_bytes() {
    let id = "12345678-1234-1234-1234-123456789abc";
    let canonical = encode_writer(&NewCapsule {
        id,
        path: "notes/a.md",
        raw: &[0],
        state: TrashState::Trashed,
        trashed_at: 1.0,
        restored_at: None,
    })
    .unwrap();
    let altered = String::from_utf8(canonical.clone())
        .unwrap()
        .replace("\"data\": \"AA==\"", "\"data\": \"AB==\"");
    assert_ne!(canonical, altered.as_bytes());
    assert_eq!(decode_legacy(altered.as_bytes(), id).unwrap().raw, vec![0]);
    assert!(decode_strict(&canonical, id).is_ok());
    assert!(decode_strict(altered.as_bytes(), id).is_err());
}

#[test]
fn python312_linux_preserved_timestamp_edges() {
    let data: Value =
        serde_json::from_str(include_str!("fixtures/python312-linux-clock.json")).unwrap();
    for row in data["cases"].as_array().unwrap() {
        let bits = u64::from_str_radix(string(&row["bits"]), 16).unwrap();
        let actual = replaced_name("notes/a.md", f64::from_bits(bits)).ok();
        let expected = row["stamp"]
            .as_str()
            .map(|stamp| format!("notes/a (replaced {stamp}).md"));
        assert_eq!(actual, expected, "{row}");
    }
}
