#![allow(clippy::unwrap_used, clippy::indexing_slicing)]
use chatgpt_status::{feature_env, status, Error, Root, Service, StatusKeys};
use serde_json::Value;
use tempfile::TempDir;
fn root(current: u8, previous: Option<u8>) -> (TempDir, Root) {
    let d = tempfile::tempdir().unwrap();
    std::fs::write(d.path().join("secrets.key"), [current; 32]).unwrap();
    if let Some(p) = previous {
        std::fs::write(d.path().join("secrets.key.previous"), [p; 32]).unwrap()
    }
    let root = Root::open(&d.path().canonicalize().unwrap()).unwrap();
    (d, root)
}
fn cases() -> Value {
    serde_json::from_str(include_str!("node-status.v1.json")).unwrap()
}
fn differential(c: &Value) {
    let (_d, root) = root(
        c["current"].as_u64().unwrap_or(7) as u8,
        c["previous"].as_u64().map(|n| n as u8),
    );
    let service = Service::new(&root, None).unwrap();
    let feature = c["feature"] != false;
    let user = c["user"].as_str().unwrap();
    let mut reads = 0;
    let answer = service.read(feature, user, || {
        reads += 1;
        if c["sqlError"] == true {
            return Err(Error::Unavailable);
        }
        Ok(c["ciphertext"].as_str().map(|s| {
            (
                zeroize::Zeroizing::new(s.to_owned()),
                c["state"].as_str().unwrap().to_owned(),
            )
        }))
    });
    let (code, body) =
        answer.unwrap_or_else(|_| (502, "{\"error\":\"ChatGPT sign-in failed.\"}".into()));
    assert_eq!(
        (code, body.as_str()),
        (
            c["code"].as_u64().unwrap() as u16,
            c["body"].as_str().unwrap()
        ),
        "ROUTING_ASSERT/status/actual_node/{}",
        c["id"]
    );
    assert_eq!(
        reads,
        usize::from(feature),
        "ROUTING_ASSERT/status/feature_reads"
    );
    let trace = c["trace"].as_array().unwrap();
    assert_eq!(trace[0], "constructor");
    assert_eq!(
        trace.len(),
        if !feature {
            1
        } else if c["absent"] == true || c["sqlError"] == true {
            2
        } else {
            3
        }
    );
}
#[test]
fn actual_node_cases() {
    let f = cases();
    assert_eq!(f["core"], "010735c7d86d30269ab3f4a351b2135faea1c8e7");
    for c in f["cases"].as_array().unwrap() {
        differential(c)
    }
}
fn named(id: &str) {
    let f = cases();
    differential(
        f["cases"]
            .as_array()
            .unwrap()
            .iter()
            .find(|c| c["id"] == id)
            .unwrap(),
    );
}
#[test]
fn mask_last_at() {
    named("last-at")
}
#[test]
fn mask_first_utf16() {
    named("astral-first")
}
#[test]
fn plan_arbitrary() {
    named("array-plan")
}
#[test]
fn duplicate_last() {
    named("duplicates")
}
#[test]
fn feature_off_no_reads() {
    named("feature-off")
}
#[test]
fn wrong_user_reconnect() {
    named("wrong-user")
}
#[test]
fn previous_key() {
    named("previous")
}
#[test]
fn snapshot_lifetime() {
    let (d, root) = root(7, None);
    let keys = StatusKeys::load(&root, None).unwrap();
    let enc = secret_envelope::seal(&[7; 32], &[1; 12], b"{}", Some(b"u1")).unwrap();
    std::fs::write(d.path().join("secrets.key"), [8; 32]).unwrap();
    assert!(status(&keys, "u1", Some(("connected", &enc)))
        .unwrap()
        .contains("connected"));
    let restarted = StatusKeys::load(&root, None).unwrap();
    assert!(status(&restarted, "u1", Some(("connected", &enc)))
        .unwrap()
        .contains("reconnect"));
}
#[test]
fn named_credentials_not_projected() {
    named("normal");
    let f = cases();
    let c = f["cases"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["id"] == "normal")
        .unwrap();
    let body = c["body"].as_str().unwrap();
    for forbidden in [
        "synthetic-access",
        "synthetic-refresh",
        "synthetic-account",
        "accessToken",
        "refreshToken",
        "accountId",
        "data_enc",
    ] {
        assert!(!body.contains(forbidden));
    }
}
#[test]
fn bounds_refuse_without_panics() {
    let (_d, root) = root(7, None);
    let k = StatusKeys::load(&root, None).unwrap();
    for payload in [
        " ".repeat(chatgpt_status::ROW_BYTES + 1),
        format!("{{\"plan\":\"{}\"}}", "a".repeat(65537)),
        format!("{}0{}", "[".repeat(66), "]".repeat(66)),
        format!("{{\"plan\":[{}]}}", vec!["null"; 8193].join(",")),
    ] {
        assert_eq!(
            status(&k, "u1", Some(("connected", &payload))),
            Err(Error::Limit)
        );
    }
    assert_eq!(status(&k, &"x".repeat(4097), None), Err(Error::Limit));
}
#[test]
fn feature_env_contract() {
    for s in ["1", "TRUE", "On", "yes", "\u{feff}true\u{a0}"] {
        assert_eq!(feature_env(Some(s)).unwrap(), Some(true))
    }
    for s in ["0", "FALSE", "Off", "no"] {
        assert_eq!(feature_env(Some(s)).unwrap(), Some(false))
    }
    assert_eq!(feature_env(None).unwrap(), None);
    assert_eq!(feature_env(Some(" ")).unwrap(), None);
    assert!(feature_env(Some("enabled")).is_err());
}
proptest::proptest! {
 #[test]fn v2_never_opens_for_other_user(suffix in "[a-z0-9]{1,20}",plan in -1000000i64..1000000){
  let(_d,root)=root(7,None);let k=StatusKeys::load(&root,None).unwrap();let user=format!("tenant-{suffix}");let plain=format!("{{\"plan\":{plan}}}");let enc=secret_envelope::seal(&[7;32],&[1;12],plain.as_bytes(),Some(user.as_bytes())).unwrap();
  proptest::prop_assert!(status(&k,&user,Some(("connected",&enc))).unwrap().contains("\"state\":\"connected\""));
  proptest::prop_assert!(status(&k,"wrong",Some(("connected",&enc))).unwrap().contains("\"state\":\"reconnect\""));
 }
}

#[test]
fn cancellation_and_deadline() {
    let (_d, root) = root(7, None);
    let k = StatusKeys::load(&root, None).unwrap();
    let c = chatgpt_status::Control::default();
    c.cancel();
    assert_eq!(
        chatgpt_status::status_with(&c, &k, "u1", None),
        Err(Error::Cancelled)
    );
    let c = chatgpt_status::Control::with_deadline(std::time::Instant::now());
    assert_eq!(
        chatgpt_status::status_with(&c, &k, "u1", None),
        Err(Error::Cancelled)
    );
}
