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
