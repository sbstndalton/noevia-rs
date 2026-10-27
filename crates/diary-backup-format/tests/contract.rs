#![allow(clippy::unwrap_used, clippy::indexing_slicing, clippy::panic)]
use diary_backup_format::*;
use serde_json::Value;

const TENANT: &str = "12345678-1234-1234-1234-123456789abc";
fn limits() -> Limits {
    Limits {
        manifest_bytes: 1_000_000,
        files: 100,
        directories: 100,
        json_depth: 32,
        json_nodes: 10000,
        string_bytes: 10000,
        payload_bytes: 10000,
    }
}
fn fixtures() -> Value {
    serde_json::from_str(include_str!("fixtures/python.json")).unwrap()
}
fn input() -> PlanInput {
    PlanInput {
        corpus_root: "Corpus/é".into(),
        tenant: TENANT.into(),
        generation: "1".into(),
        saved_at: 1.0,
        settings: PythonJson::Null,
        directories: vec!["empty".into(), "empty/é".into()],
        files: vec![
            SourceFile {
                path: "assets/bin.dat".into(),
                sha256: sha256(b"\0\xff"),
            },
            SourceFile {
                path: "Notes/é😀.md".into(),
                sha256: sha256(b"synthetic\r\n"),
            },
        ],
    }
}

#[test]
fn cp_python_float_bit_fixtures() {
    for row in fixtures()["floats"].as_array().unwrap() {
        let bits = u64::from_str_radix(row["bits"].as_str().unwrap(), 16).unwrap();
        let actual = python_float(f64::from_bits(bits));
        assert_eq!(
            actual,
            row["python"].as_str().unwrap(),
            "IEEE bits {bits:016x}"
        );
    }
}
#[test]
fn cp_python_exact_manifest_bytes_hash_and_immutable_keys() {
    for row in fixtures()["manifests"].as_array().unwrap() {
        let mut source = input();
        source.generation = row["generation"].as_str().unwrap().into();
        source.saved_at =
            f64::from_bits(u64::from_str_radix(row["bits"].as_str().unwrap(), 16).unwrap());
        source.settings =
            PythonJson::parse(row["settings"].as_str().unwrap().as_bytes(), limits()).unwrap();
        let planned = plan(source, limits()).unwrap();
        assert_eq!(planned.bytes, row["expected"].as_str().unwrap().as_bytes());
        assert_eq!(planned.sha256, row["sha256"].as_str().unwrap());
        assert_eq!(planned.manifest_key, row["manifestKey"].as_str().unwrap());
        let parsed = read(
            &planned.bytes,
            &planned.sha256,
            ReadPolicy::LegacyRead,
            limits(),
        )
        .unwrap();
        assert_eq!(parsed.original_sha256, planned.sha256);
        assert_eq!(parsed.files, planned.objects);
        let Some(PythonJson::Float(saved)) = parsed.saved_at else {
            panic!("savedAt float missing")
        };
        if planned.manifest.saved_at.is_nan() {
            assert!(saved.is_nan());
        } else {
            assert_eq!(saved.to_bits(), planned.manifest.saved_at.to_bits());
        }

        assert!(read(
            &planned.bytes,
            &planned.sha256,
            ReadPolicy::StrictWriterLayout {
                expected_tenant: TENANT
            },
            limits()
        )
        .is_ok());
    }
}
#[test]
fn immutable_digest_is_of_original_not_reencoded_bytes() {
    let planned = plan(input(), limits()).unwrap();
    let mut noncanonical = b" \n".to_vec();
    noncanonical.extend(&planned.bytes);
    noncanonical.extend(b"\n");
    let digest = sha256(&noncanonical);
    assert!(read(&noncanonical, &digest, ReadPolicy::LegacyRead, limits()).is_ok());
    assert_eq!(
        read(
            &noncanonical,
            &planned.sha256,
            ReadPolicy::LegacyRead,
            limits()
        )
        .unwrap_err(),
        Error::Checksum
    );
}
fn altered_object(key: &str) -> Vec<u8> {
    let planned = plan(input(), limits()).unwrap();
    let mut value: Value = serde_json::from_slice(&planned.bytes).unwrap();
    value["files"][0]["object"] = Value::String(key.into());
    serde_json::to_vec(&value).unwrap()
}
#[test]
fn legacy_namespace_matches_component_sequence_without_claiming_writer_layout() {
    for key in [
        format!("x/noevia-backups/{TENANT}/objects/anything/nested/old-name"),
        format!("noevia-backups/{TENANT}/objects/a"),
        format!("noevia-backups/{TENANT}/objects/other-tenant/objects/anything"),
    ] {
        let bytes = altered_object(&key);
        let hash = sha256(&bytes);
        assert!(
            read(&bytes, &hash, ReadPolicy::LegacyRead, limits()).is_ok(),
            "{key}"
        );
        assert_eq!(
            read(
                &bytes,
                &hash,
                ReadPolicy::StrictWriterLayout {
                    expected_tenant: TENANT
                },
                limits()
            )
            .unwrap_err(),
            Error::Namespace
        );
    }
    for key in [
        format!("xnoevia-backups/{TENANT}/objects/a"),
        format!("noevia-backups/{TENANT}/objects"),
        format!("noevia-backups/{TENANT}x/objects/a"),
        format!("noevia-backups/{TENANT}/objectsx/a"),
        format!("../noevia-backups/{TENANT}/objects/a"),
        format!("/noevia-backups/{TENANT}/objects/a"),
    ] {
        let bytes = altered_object(&key);
        assert!(
            read(&bytes, &sha256(&bytes), ReadPolicy::LegacyRead, limits()).is_err(),
            "{key}"
        );
    }
}
#[test]
fn strict_tenant_mismatch_even_for_empty_manifest() {
    let mut source = input();
    source.files.clear();
    let planned = plan(source, limits()).unwrap();
    assert_eq!(
        read(
            &planned.bytes,
            &planned.sha256,
            ReadPolicy::StrictWriterLayout {
                expected_tenant: "00000000-0000-0000-0000-000000000000"
            },
            limits()
        )
        .unwrap_err(),
        Error::Tenant
    );
    assert!(read(
        &planned.bytes,
        &planned.sha256,
        ReadPolicy::LegacyRead,
        limits()
    )
    .is_ok());
}
#[test]
fn every_budget_refuses_overlimit_without_truncation() {
    let planned = plan(input(), limits()).unwrap();
    let mut cap = limits();
    cap.manifest_bytes = planned.bytes.len() - 1;
    assert_eq!(
        read(&planned.bytes, &planned.sha256, ReadPolicy::LegacyRead, cap).unwrap_err(),
        Error::Limit
    );
    assert_eq!(plan(input(), cap).unwrap_err(), Error::Limit);
    let mut cap = limits();
    cap.files = 1;
    assert_eq!(
        read(&planned.bytes, &planned.sha256, ReadPolicy::LegacyRead, cap).unwrap_err(),
        Error::Limit
    );
    assert_eq!(plan(input(), cap).unwrap_err(), Error::Limit);
    let mut cap = limits();
    cap.directories = 1;
    assert_eq!(
        read(&planned.bytes, &planned.sha256, ReadPolicy::LegacyRead, cap).unwrap_err(),
        Error::Limit
    );
    assert_eq!(plan(input(), cap).unwrap_err(), Error::Limit);
    for (text, depth, nodes, string_bytes) in [
        ("[[[]]]", 1, 100, 100),
        ("[0,0]", 10, 2, 100),
        ("\"abc\"", 10, 100, 2),
    ] {
        let mut cap = limits();
        cap.json_depth = depth;
        cap.json_nodes = nodes;
        cap.string_bytes = string_bytes;
        assert_eq!(
            PythonJson::parse(text.as_bytes(), cap).unwrap_err(),
            Error::Limit
        );
        let value = PythonJson::parse(text.as_bytes(), limits()).unwrap();
        assert_eq!(value.canonical_bytes(cap), Err(Error::Limit));
    }
    let mut source = input();
    source.corpus_root = "oversize".repeat(1000);
    let mut cap = limits();
    cap.string_bytes = 100;
    assert_eq!(plan(source, cap).unwrap_err(), Error::Limit);
}
#[test]
fn pure_payload_verification_and_aggregate_budget() {
    let planned = plan(input(), limits()).unwrap();
    let parsed = read(
        &planned.bytes,
        &planned.sha256,
        ReadPolicy::LegacyRead,
        limits(),
    )
    .unwrap();
    let bodies: &[&[u8]] = &[b"synthetic\r\n", b"\0\xff"];
    assert_eq!(verify_payloads(&parsed, bodies, limits()).unwrap(), 13);
    assert_eq!(
        verify_payloads(&parsed, &[b"bad", b"\0\xff"], limits()),
        Err(Error::Checksum)
    );
    let mut cap = limits();
    cap.payload_bytes = 12;
    assert_eq!(verify_payloads(&parsed, bodies, cap), Err(Error::Limit));
}
#[test]
fn parser_preserves_python_numbers_duplicate_keys_and_optional_legacy_fields() {
    let value = PythonJson::parse(b"{\"n\":1,\"n\":100000000000000000000000000000000000000000000000000000001,\"z\":-0,\"f\":1e9999}", limits()).unwrap();
    assert_eq!(value.canonical_bytes(limits()).unwrap(), b"{\"f\": Infinity, \"n\": 100000000000000000000000000000000000000000000000000000001, \"z\": 0}");
    let bytes =
        format!("{{\"format\":\"{FORMAT}\",\"tenant\":\"{TENANT}\",\"files\":[]}}").into_bytes();
    let parsed = read(&bytes, &sha256(&bytes), ReadPolicy::LegacyRead, limits()).unwrap();
    assert!(parsed.generation.is_none() && parsed.saved_at.is_none());
    assert!(parsed.directories.is_empty());
    for invalid in [
        "+1.0",
        ".5",
        "01.0",
        "1.",
        "[0,]",
        "{\"x\":1,}",
        "1e",
        "-NaN",
    ] {
        assert!(
            PythonJson::parse(invalid.as_bytes(), limits()).is_err(),
            "{invalid}"
        );
    }
}
#[test]
fn tenant_uuid_matches_python_text_normalization() {
    for raw in [
        TENANT.to_uppercase(),
        "12345678123412341234123456789abc".into(),
        format!("urn:uuid:{{{TENANT}}}"),
        "{1-2345678123412341234123456789abc}".into(),
    ] {
        assert_eq!(tenant_uuid(&raw).unwrap(), TENANT);
    }
}
#[test]
fn planner_rejects_unsafe_and_overlapping_source_paths() {
    let mut source = input();
    source.files[0].path = "../escape".into();
    assert!(plan(source, limits()).is_err());
    let mut source = input();
    source.directories.push("Notes/é😀.md".into());
    assert!(plan(source, limits()).is_err());
    let mut source = input();
    source.files.push(SourceFile {
        path: "Notes/é😀.md/child".into(),
        sha256: sha256(b"x"),
    });
    assert!(plan(source, limits()).is_err());
    let mut source = input();
    source.corpus_root = "../escape".into();
    assert!(plan(source, limits()).is_err());
}

#[test]
fn handcrafted_reader_only_surrogates_are_typed_unsupported_not_replaced() {
    for raw in [br#""\ud800""#.as_slice(), br#"{"settings":"\udfff"}"#] {
        assert_eq!(
            PythonJson::parse(raw, limits()),
            Err(Error::UnsupportedInput)
        );
    }
    let pair = PythonJson::parse(br#""\ud83d\ude00""#, limits()).unwrap();
    assert_eq!(pair.canonical_bytes(limits()).unwrap(), "\"😀\"".as_bytes());
    let escaped_literal = PythonJson::parse(br#""\\ud800""#, limits()).unwrap();
    assert_eq!(
        escaped_literal.canonical_bytes(limits()).unwrap(),
        br#""\\ud800""#
    );
    // Python int(hex, 16) accepts an underscore here, but the writer always emits
    // normalized UUID text. Supporting this handcrafted reader spelling is deferred.
    assert_eq!(
        tenant_uuid("1234567_123412341234123456789abc"),
        Err(Error::Tenant)
    );
}

#[test]
fn manifest_writer_checks_json_structure_and_exact_byte_boundary() {
    let planned = plan(input(), limits()).unwrap();
    let mut cap = limits();
    cap.manifest_bytes = planned.bytes.len();
    assert_eq!(plan(input(), cap).unwrap().bytes, planned.bytes);
    for (depth, nodes) in [(2, 10000), (32, 1)] {
        let mut cap = limits();
        cap.json_depth = depth;
        cap.json_nodes = nodes;
        assert_eq!(plan(input(), cap).unwrap_err(), Error::Limit);
    }
    let mut source = input();
    source.corpus_root = "//".into();
    assert!(plan(source, limits())
        .unwrap()
        .manifest_key
        .starts_with("noevia-backups/"));
    let mut source = input();
    source.corpus_root = "/Corpus/é/".into();
    assert_eq!(plan(source, limits()).unwrap().bytes, planned.bytes);
}

#[test]
fn python_legacy_namespace_and_documented_reader_only_fixtures() {
    for row in fixtures()["namespace"].as_array().unwrap() {
        let bytes = altered_object(row["object"].as_str().unwrap());
        assert_eq!(
            read(&bytes, &sha256(&bytes), ReadPolicy::LegacyRead, limits()).is_ok(),
            row["pythonAccepts"].as_bool().unwrap()
        );
    }
    for row in fixtures()["readerOnlySurrogates"].as_array().unwrap() {
        assert_eq!(row["writerCanEmit"], false);
        assert_eq!(
            PythonJson::parse(row["raw"].as_str().unwrap().as_bytes(), limits()),
            Err(Error::UnsupportedInput)
        );
    }
    let row = &fixtures()["readerOnlyUuid"];
    assert_eq!(row["python"], "01234567-1234-1234-1234-123456789abc");
    assert_eq!(
        tenant_uuid(row["input"].as_str().unwrap()),
        Err(Error::Tenant)
    );
}

#[test]
fn identical_content_and_basename_share_immutable_object_without_losing_manifest_rows() {
    let mut source = input();
    source.files = vec![
        SourceFile {
            path: "one/x.md".into(),
            sha256: sha256(b"same"),
        },
        SourceFile {
            path: "two/x.md".into(),
            sha256: sha256(b"same"),
        },
    ];
    let planned = plan(source, limits()).unwrap();
    assert_eq!(planned.objects.len(), 2);
    assert_eq!(planned.objects[0].object, planned.objects[1].object);
    assert_ne!(planned.objects[0].path, planned.objects[1].path);
    let parsed = read(
        &planned.bytes,
        &planned.sha256,
        ReadPolicy::StrictWriterLayout {
            expected_tenant: TENANT,
        },
        limits(),
    )
    .unwrap();
    assert_eq!(
        verify_payloads(&parsed, &[b"same", b"same"], limits()).unwrap(),
        8
    );
}
