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
