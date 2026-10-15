//! New pure controls: compile only in Stage A. No oracle, key IO or transport.
#[path = "../examples/support/storage_factory_capture.rs"]
pub mod process;
#[path = "../examples/support/storage_factory_topology.rs"]
pub mod topology;
use serde_json::json;
use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
};

#[test]
fn owner_plaintext_and_module_containment_controls() -> Result<(), String> {
    let expected = json!({"owner":"a","secret":"synthetic-secret","kind":"webdav","baseUrl":"https://storage.invalid/dav","bucket":"synthetic","region":"us-east-1","username":"synthetic","corpusRoot":"synthetic","secretConfigured":true,"secretNeedsReauth":false,"effectiveRole":"member"});
    topology::compare_private_projection(&expected, &expected)?;
    let mut wrong_owner = expected.clone();
    *wrong_owner.get_mut("owner").ok_or("owner")? = json!("b");
    assert!(topology::compare_private_projection(&expected, &wrong_owner).is_err());
    let mut wrong_plaintext = expected.clone();
    *wrong_plaintext.get_mut("secret").ok_or("secret")? = json!("different-synthetic");
    assert!(topology::compare_private_projection(&expected, &wrong_plaintext).is_err());
    let mut missing = expected.clone();
    missing.as_object_mut().ok_or("object")?.remove("secret");
    assert!(topology::compare_private_projection(&expected, &missing).is_err());
    let root = Path::new("/private/tmp/fixed-oracle");
    let files = BTreeSet::from([PathBuf::from("server/auth.cjs")]);
    assert!(topology::contained_module(
        root,
        "/private/tmp/fixed-oracle/server/auth.cjs",
        &files
    ));
    for bad in [
        "/private/tmp/fixed-oracle-other/server/auth.cjs",
        "/private/tmp/fixed-oracle/server/../auth.cjs",
        "/private/tmp/fixed-oracle/server/uninventoried.cjs",
        "server/auth.cjs",
        "/tmp/fixed-oracle/server/auth.cjs",
    ] {
        assert!(!topology::contained_module(root, bad, &files));
    }
    let cases = topology::cases()?;
    assert_eq!(cases.len(), 126);
    assert_eq!(cases.chunks(14).count(), 9);
    let raw_missing = json!({"kind":"local","baseUrl":"","bucket":"","region":"us-east-1","username":"","corpusRoot":""});
    topology::assert_missing_factory_shape(&raw_missing)?;
    for field in ["secret", "secretConfigured", "secretNeedsReauth"] {
        let mut unexpected = raw_missing.clone();
        unexpected
            .as_object_mut()
            .ok_or("raw object")?
            .insert(field.into(), json!(false));
        assert!(topology::assert_missing_factory_shape(&unexpected).is_err());
    }
    Ok(())
}

#[test]
fn fixed_view_absent_property_contract_controls() -> Result<(), String> {
    let case = topology::cases()?
        .into_iter()
        .find(|c| c.get("id") == Some(&json!("S-view")))
        .ok_or("fixed view case")?;
    let contract = topology::storage_schema_evidence_contract(&case)?;
    assert_eq!(
        contract.get("sql"),
        Some(&json!("SELECT * FROM storage_connections WHERE user_id=?"))
    );
    let mut state = vec![json!({"columns":[],"rows":[]}); topology::STATE_SQL.len()];
    *state.last_mut().ok_or("storage state")? =
        json!({"columns":["user_id"],"rows":[[{"t":"text","v":"a"}]]});
    let valid = json!({"id":"S-view","owner":"a","storedEvidence":{"presence":"property-missing","row":{"user_id":{"t":"text","v":"a"}}},"storedPlain":null,"currentOpen":null,"foreignOpen":null,"storage":{"bucket":"","region":"us-east-1","secret":"","secretConfigured":false,"secretNeedsReauth":false},"before":state,"after":state});
    topology::assert_view_factory_evidence(&case, &valid)?;
    let mut present_null = valid.clone();
    present_null
        .get_mut("storedEvidence")
        .and_then(|v| v.get_mut("row"))
        .and_then(serde_json::Value::as_object_mut)
        .ok_or("view row")?
        .insert("secret".into(), json!({"t":"null","v":null}));
    assert!(topology::assert_view_factory_evidence(&case, &present_null).is_err());
    let mut adapter_exception = valid.clone();
    adapter_exception
        .as_object_mut()
        .ok_or("view object")?
        .insert("exceptionPhase".into(), json!("evidence"));
    assert!(topology::assert_view_factory_evidence(&case, &adapter_exception).is_err());
    let mut fabricated_stored = valid.clone();
    fabricated_stored
        .as_object_mut()
        .ok_or("view object")?
        .insert("stored".into(), json!(null));
    assert!(topology::assert_view_factory_evidence(&case, &fabricated_stored).is_err());
    let mut sql_change = valid.clone();
    *sql_change
        .get_mut("after")
        .and_then(serde_json::Value::as_array_mut)
        .and_then(|v| v.last_mut())
        .ok_or("after state")? = json!({"columns":["user_id"],"rows":[]});
    assert!(topology::assert_view_factory_evidence(&case, &sql_change).is_err());
    let mut normalized = valid.clone();
    normalized
        .get_mut("storage")
        .and_then(serde_json::Value::as_object_mut)
        .ok_or("view factory")?
        .insert("kind".into(), json!("local"));
    assert!(topology::assert_view_factory_evidence(&case, &normalized).is_err());
    Ok(())
}

#[test]
fn native_js_foreign_authentication_contract_controls() -> Result<(), String> {
    let auth = json!({"error":{"name":"Error","code":null,"reason":null,"message":"Unsupported state or unable to authenticate data"}});
    topology::assert_js_gcm_auth_failure(&auth)?;
    for invalid in [
        json!(null),
        json!(false),
        json!("error"),
        json!([]),
        json!({"error":null}),
        json!({"error":"Unsupported state or unable to authenticate data"}),
        json!({"error":{}}),
        json!({"plain":"synthetic-secret","keyUsed":"current"}),
        json!({"error":{"name":"Error","code":null,"reason":null,"message":"unknown operational crypto fault"}}),
        json!({"error":{"name":7,"code":null,"reason":null,"message":"Unsupported state or unable to authenticate data"}}),
        json!({"error":{"name":"Error","code":"ERR_OSSL_EVP_BAD_DECRYPT","reason":null,"message":"Unsupported state or unable to authenticate data"}}),
        json!({"error":{"name":"Error","code":null,"reason":"secret_envelope_failed","message":"Unsupported state or unable to authenticate data"}}),
        json!({"error":{"name":"Error","code":null,"reason":null,"message":"Unsupported state or unable to authenticate data"},"plain":"synthetic-secret"}),
    ] {
        assert!(topology::assert_js_gcm_auth_failure(&invalid).is_err());
    }
    let mut wrong_message_type = auth;
    *wrong_message_type
        .get_mut("error")
        .and_then(|v| v.get_mut("message"))
        .ok_or("error message")? = json!(true);
    assert!(topology::assert_js_gcm_auth_failure(&wrong_message_type).is_err());
    Ok(())
}

#[test]
fn fixed_locked_package_root_contract_controls() -> Result<(), String> {
    let mut closure = (0..63)
        .map(|n| format!("synthetic-package-{n}"))
        .collect::<BTreeSet<_>>();
    closure.extend(["tslib".into(), "tsyringe".into()]);
    let roots = topology::expected_package_roots(&closure)?;
    assert_eq!(roots.len(), 66);
    let mut manifests = BTreeSet::new();
    for name in &closure {
        let version = if name == "tslib" { "2.8.1" } else { "1.0.0" };
        topology::validate_package_root(
            &format!("node_modules/{name}"),
            &json!({"name":name,"version":version}),
            &json!({"version":version}),
            &closure,
            &roots,
            &mut manifests,
        )?;
    }
    let nested = topology::NESTED_TSLIB_ROOT;
    let package = json!({"name":"tslib","version":"1.14.1"});
    let pinned = json!({"version":"1.14.1"});
    topology::validate_package_root(nested, &package, &pinned, &closure, &roots, &mut manifests)?;
    topology::validate_package_root_closure(&manifests, &roots)?;
    assert!(topology::validate_package_root(
        nested,
        &package,
        &pinned,
        &closure,
        &roots,
        &mut manifests
    )
    .is_err());
    for missing in ["node_modules/tslib", nested] {
        let mut incomplete = manifests.clone();
        incomplete.remove(missing);
        assert!(topology::validate_package_root_closure(&incomplete, &roots).is_err());
    }
    let mut extra = manifests.clone();
    extra.insert("node_modules/unaccepted".into());
    assert!(topology::validate_package_root_closure(&extra, &roots).is_err());
    for (parent, actual, lock) in [
        (
            nested,
            json!({"name":"wrong","version":"1.14.1"}),
            pinned.clone(),
        ),
        (
            nested,
            json!({"name":"tslib","version":"2.8.1"}),
            json!({"version":"2.8.1"}),
        ),
        (nested, package.clone(), json!({"version":"1.14.2"})),
        ("node_modules/tslib", package.clone(), pinned.clone()),
        (
            "node_modules/other/node_modules/tslib",
            package.clone(),
            pinned.clone(),
        ),
        (
            "node_modules/unaccepted",
            json!({"name":"unaccepted","version":"1.0.0"}),
            json!({"version":"1.0.0"}),
        ),
    ] {
        assert!(topology::validate_package_root(
            parent,
            &actual,
            &lock,
            &closure,
            &roots,
            &mut BTreeSet::new()
        )
        .is_err());
    }
    Ok(())
}

fn case(id: &str) -> Result<serde_json::Value, String> {
    topology::cases()?
        .into_iter()
        .find(|c| c.get("id") == Some(&json!(id)))
        .ok_or_else(|| format!("fixed case {id}"))
}

/// noevia#1390 F1: Node's csrfValid is `''` for an absent header; it must compare as `false`.
#[test]
fn raw_node_csrf_is_normalised_exactly() -> Result<(), String> {
    for (raw, expected) in [
        (json!({"type":"string","value":""}), json!(false)),
        (json!({"type":"boolean","value":false}), json!(false)),
        (json!({"type":"boolean","value":true}), json!(true)),
        (json!({"type":"object","value":null}), json!(null)),
    ] {
        assert_eq!(topology::observed_csrf(&raw)?, expected, "{raw}");
    }
    for raw in [
        json!({"type":"string","value":"synthetic-csrf-a"}),
        json!({"type":"string","value":"false"}),
        json!({"type":"boolean","value":""}),
        json!({"type":"boolean","value":null}),
        json!({"type":"object","value":{}}),
        json!({"type":"undefined","value":null}),
        json!({"type":"number","value":0}),
        json!({"value":""}),
        json!({"type":"string"}),
    ] {
        assert!(topology::observed_csrf(&raw).is_err(), "{raw}");
    }
    Ok(())
}

/// noevia#1390 F2: a strict Rust refusal accepts only the exact typed Node exception it names.
#[test]
fn strict_refusal_accepts_only_the_named_typed_exception() -> Result<(), String> {
    let dir = "/synthetic/node-00-00";
    let key = case("K-current31")?;
    let key_error = json!({"name":"Error","code":null,"reason":null,"message":format!("invalid credential-encryption key at {dir}/secrets.key")});
    let accepted = json!({"id":"K-current31","exceptionPhase":"keys","error":key_error});
    assert!(topology::qualify_refusal_exception(
        &key,
        "key refused",
        &accepted,
        dir
    )?);
    let mut refused = Vec::new();
    // An unrelated factory/setup failure must not stand in for the key refusal.
    refused.push(json!({"id":"K-current31","exceptionPhase":"factory","error":{"name":"TypeError","code":null,"reason":null,"message":"routes.handle is not a function"}}));
    refused.push(json!({"id":"K-current31","exceptionPhase":"keys","error":{"name":"TypeError","code":null,"reason":null,"message":"routes.handle is not a function"}}));
    // Another directory's key, a non-null reason/code, a missing or null record, no exception.
    refused.push(json!({"id":"K-current31","exceptionPhase":"keys","error":{"name":"Error","code":null,"reason":null,"message":"invalid credential-encryption key at /elsewhere/secrets.key"}}));
    let mut with_reason = accepted.clone();
    *with_reason
        .get_mut("error")
        .and_then(|e| e.get_mut("reason"))
        .ok_or("reason")? = json!("secret_envelope_failed");
    refused.push(with_reason);
    let mut with_code = accepted.clone();
    *with_code
        .get_mut("error")
        .and_then(|e| e.get_mut("code"))
        .ok_or("code")? = json!("EACCES");
    refused.push(with_code);
    refused.push(json!({"id":"K-current31","exceptionPhase":"keys"}));
    refused.push(json!({"id":"K-current31","exceptionPhase":"keys","error":null}));
    refused.push(json!({"id":"K-current31"}));
    refused.push(json!({"id":"K-current31","exceptionPhase":7,"error":key_error}));
    for node in &refused {
        assert!(
            topology::qualify_refusal_exception(&key, "key refused", node, dir).is_err(),
            "{node}"
        );
    }

    let missing = case("S-missing")?;
    let state = json!([{"columns":[],"rows":[]}]);
    let table = json!({"name":"SqliteError","code":"SQLITE_ERROR","reason":null,"message":"no such table: storage_connections"});
    let semantic = json!({"id":"S-missing","exceptionPhase":"projection","error":table,"before":state,"after":state});
    assert!(topology::qualify_refusal_exception(
        &missing,
        "projection:Schema",
        &semantic,
        dir
    )?);
    let mut delta = semantic.clone();
    *delta.get_mut("after").ok_or("after")? = json!([]);
    let mut wrong_phase = semantic.clone();
    *wrong_phase.get_mut("exceptionPhase").ok_or("phase")? = json!("factory");
    let unrelated = json!({"id":"S-missing","exceptionPhase":"projection","error":{"name":"TypeError","code":null,"reason":null,"message":"Cannot read properties of undefined"},"before":state,"after":state});
    for node in [delta, wrong_phase, unrelated] {
        assert!(
            topology::qualify_refusal_exception(&missing, "projection:Schema", &node, dir).is_err(),
            "{node}"
        );
    }
    // The same SQLite error is not acceptable for a shape that does not drop the table.
    let view = case("S-view")?;
    assert!(
        topology::qualify_refusal_exception(&view, "projection:Schema", &semantic, dir).is_err()
    );
    // No Node exception: the caller must go on to compare SQL effects, not accept.
    assert!(!topology::qualify_refusal_exception(
        &view,
        "projection:Schema",
        &json!({"id":"S-view"}),
        dir
    )?);
    Ok(())
}

/// noevia#1390 F4: an operational failure keeps the capped partial streams and reaped status.
#[test]
fn operational_failures_retain_first_attempt_evidence() -> Result<(), String> {
    use std::{process::Command, time::Duration};
    let probe = env!("CARGO_BIN_EXE_private-oracle-probe");
    let dir = tempfile::tempdir().map_err(|e| e.to_string())?;

    let mut stalled = Command::new(probe);
    stalled.arg("partial");
    let output = process::bounded(stalled, Vec::new(), Duration::from_millis(500));
    assert!(matches!(output.failure, Some(process::Failure::Deadline)));
    assert_eq!(output.stdout, b"partial-stdout");
    assert_eq!(output.stderr, b"partial-stderr");
    let status = output.status.ok_or("reaped status retained")?;
    assert!(!status.success());
    topology::retain_capture(dir.path(), "stalled", &output)?;
    assert_eq!(
        std::fs::read(dir.path().join("stalled.stdout")).map_err(|e| e.to_string())?,
        b"partial-stdout"
    );
    assert_eq!(
        std::fs::read(dir.path().join("stalled.stderr")).map_err(|e| e.to_string())?,
        b"partial-stderr"
    );
    let recorded: serde_json::Value = serde_json::from_slice(
        &std::fs::read(dir.path().join("stalled.status.json")).map_err(|e| e.to_string())?,
    )
    .map_err(|e| e.to_string())?;
    assert_eq!(recorded.get("failure"), Some(&json!("Deadline")));
    assert_eq!(recorded.get("success"), Some(&json!(false)));
    assert!(recorded
        .get("status_debug")
        .and_then(serde_json::Value::as_str)
        .is_some_and(|s| !s.is_empty()));
    // First-attempt evidence is never overwritten by a retry.
    assert!(topology::retain_capture(dir.path(), "stalled", &output).is_err());

    let mut noisy = Command::new(probe);
    noisy.arg("stdout-noise");
    let output = process::bounded(noisy, Vec::new(), Duration::from_secs(5));
    assert!(matches!(output.failure, Some(process::Failure::OutputCap)));
    assert_eq!(output.stdout.len(), 8 * 1024 * 1024);
    assert!(output.status.is_some());
    topology::retain_capture(dir.path(), "noisy", &output)?;
    Ok(())
}

/// noevia#1390 F5: materialization, preflight, capture and comparison are separate invocations.
#[test]
fn stages_are_selected_one_at_a_time() {
    let args = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
    for (flag, stage) in [
        ("--fixed-materialize-only", topology::Stage::Materialize),
        ("--fixed-preflight-only", topology::Stage::Preflight),
        ("--fixed-capture-only", topology::Stage::Capture),
        ("--fixed-compare-only", topology::Stage::Compare),
    ] {
        assert_eq!(topology::stage(&args(&[flag])), Some(stage));
    }
    for refused in [
        &[][..],
        &["--fixed-capture-only", "--fixed-compare-only"][..],
        &["--fixed-preflight-only", "--fixed-preflight-only"][..],
        &["--fixed-all"][..],
        &["--fixed-capture-only "][..],
        &["fixed-capture-only"][..],
    ] {
        assert_eq!(topology::stage(&args(refused)), None, "{refused:?}");
    }
}
