//! Operation bodies alone never prove authentication or live session authority.
use egress::control::{RequestMetadata, Route, MAX_REQUEST_BYTES, MAX_SAFE_INTEGER};
use egress_proxy::control::wire::{Error, ReadyMode, Request};
use serde_json::{json, Value};

const ID: &str = "00112233445566778899aabbccddeeff";
const FP: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
const NONCE: &str = "AAECAwQFBgcICQoLDA0ODw";
// Deliberately syntactically valid but unauthenticated: execution MUST verify claims/MAC.
const TOKEN: &str = "ngr1.e30.AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";
fn meta(boot: Option<&str>) -> RequestMetadata<'_> {
    RequestMetadata {
        request_id: ID,
        deadline_ms: 1000,
        boot_id: boot,
        key_fingerprint: FP,
    }
}
fn body(route: Route) -> Value {
    match route {
        Route::Ready => {
            json!({"version":1,"mode":"initial","challenge":ID,"expectedKeyFingerprint":FP,"priorSession":null})
        }
        Route::Renew => json!({"version":1,"sessionId":ID,"sequence":1}),
        Route::Activity => {
            json!({"version":1,"sessionId":ID,"taskId":"task.A:1_-","leaseNonce":NONCE,"signedGrant":TOKEN})
        }
        Route::Revoke => {
            json!({"version":1,"sessionId":ID,"taskId":"task.A:1_-","leaseNonce":NONCE,"grantIssuedAt":1,"signedGrant":TOKEN,"signedRevoke":TOKEN})
        }
        Route::Forget => {
            json!({"version":1,"sessionId":ID,"taskId":"task.A:1_-","leaseNonce":NONCE,"snapshotId":ID})
        }
    }
}
fn decode(route: Route, value: &Value) -> Result<Request, Error> {
    Request::decode(
        route,
        meta((route != Route::Ready).then_some(ID)),
        value.to_string().as_bytes(),
    )
}
fn set(v: &mut Value, key: &str, value: Value) -> Result<(), Box<dyn std::error::Error>> {
    v.as_object_mut()
        .ok_or("expected object")?
        .insert(key.into(), value);
    Ok(())
}
#[test]
fn admits_closed_operations_without_implying_authentication(
) -> Result<(), Box<dyn std::error::Error>> {
    for route in [
        Route::Ready,
        Route::Renew,
        Route::Activity,
        Route::Revoke,
        Route::Forget,
    ] {
        let r = decode(route, &body(route))?;
        assert_eq!(r.route(), route);
        if route == Route::Ready {
            let v = r.ready().ok_or("ready missing")?;
            assert_eq!(v.mode, ReadyMode::Initial);
            assert_eq!(v.challenge, ID);
            assert_eq!(v.expected_key_fingerprint, FP);
            assert_eq!(v.prior_session, None);
            assert!(r.lease().is_none());
        } else if route == Route::Renew {
            assert_eq!(r.renewal(), Some((ID, 1)));
        } else {
            let lease = r.lease().ok_or("lease missing")?;
            assert_eq!(
                (lease.session_id, lease.task_id, lease.nonce),
                (ID, "task.A:1_-", NONCE)
            );
        }
        assert!(!format!("{r:?}").contains(TOKEN));
        assert!(!format!("{r:?}").contains(ID));
    }
    assert_eq!(
        decode(Route::Activity, &body(Route::Activity))?.signed_grant(),
        Some(TOKEN)
    );
    assert_eq!(
        decode(Route::Revoke, &body(Route::Revoke))?.revocation(),
        Some((1, TOKEN))
    );
    assert_eq!(
        decode(Route::Forget, &body(Route::Forget))?.snapshot_id(),
        Some(ID)
    );
    Ok(())
}
#[test]
fn ready_requires_explicit_preconditions_and_matching_fingerprint(
) -> Result<(), Box<dyn std::error::Error>> {
    let mut v = body(Route::Ready);
    v.as_object_mut().ok_or("object")?.remove("priorSession");
    assert!(decode(Route::Ready, &v).is_err());
    set(&mut v, "priorSession", Value::Null)?;
    assert!(decode(Route::Ready, &v).is_ok());
    set(&mut v, "priorSession", json!(ID))?;
    assert!(decode(Route::Ready, &v).is_err());
    set(&mut v, "mode", json!("replace"))?;
    assert!(decode(Route::Ready, &v).is_err());
    assert!(Request::decode(Route::Ready, meta(Some(ID)), v.to_string().as_bytes()).is_ok());
    set(&mut v, "priorSession", Value::Null)?;
    assert!(Request::decode(Route::Ready, meta(Some(ID)), v.to_string().as_bytes()).is_ok());
    set(&mut v, "mode", json!("initial"))?;
    assert!(Request::decode(Route::Ready, meta(Some(ID)), v.to_string().as_bytes()).is_err());
    set(&mut v, "expectedKeyFingerprint", json!("0".repeat(64)))?;
    assert!(decode(Route::Ready, &v).is_err());
    Ok(())
}
#[test]
fn rejects_duplicate_missing_unknown_fields_and_noninteger_values(
) -> Result<(), Box<dyn std::error::Error>> {
    for route in [
        Route::Ready,
        Route::Renew,
        Route::Activity,
        Route::Revoke,
        Route::Forget,
    ] {
        let v = body(route);
        let fields: Vec<_> = v.as_object().ok_or("object")?.keys().cloned().collect();
        for key in fields {
            let mut missing = v.clone();
            missing.as_object_mut().ok_or("object")?.remove(&key);
            assert!(decode(route, &missing).is_err(), "missing {route:?}/{key}");
            let duplicate = format!(
                "{{{key_json}:{value},{}",
                v.to_string().trim_start_matches('{'),
                key_json = json!(key),
                value = v.get(&key).ok_or("field")?
            );
            assert!(
                Request::decode(
                    route,
                    meta((route != Route::Ready).then_some(ID)),
                    duplicate.as_bytes()
                )
                .is_err(),
                "duplicate {route:?}/{key}"
            );
        }
        let mut unknown = v.clone();
        set(&mut unknown, "extra", json!(true))?;
        assert!(decode(route, &unknown).is_err());
        let trailing = format!("{v} {{}}");
        assert!(Request::decode(
            route,
            meta((route != Route::Ready).then_some(ID)),
            trailing.as_bytes()
        )
        .is_err());
        for bad in [
            json!(null),
            json!(true),
            json!("1"),
            json!(1.0),
            json!(-1),
            json!(2),
        ] {
            let mut wrong = v.clone();
            set(&mut wrong, "version", bad)?;
            assert!(decode(route, &wrong).is_err());
        }
        if route != Route::Ready {
            assert!(Request::decode(route, meta(None), v.to_string().as_bytes()).is_err());
        }
    }
    for field in ["sequence", "grantIssuedAt"] {
        let route = if field == "sequence" {
            Route::Renew
        } else {
            Route::Revoke
        };
        for bad in [
            json!(0),
            json!(MAX_SAFE_INTEGER + 1),
            json!(1.0),
            json!("1"),
            json!(-1),
        ] {
            let mut v = body(route);
            set(&mut v, field, bad)?;
            assert!(decode(route, &v).is_err());
        }
        let mut v = body(route);
        set(&mut v, field, json!(MAX_SAFE_INTEGER))?;
        assert!(decode(route, &v).is_ok());
    }
    Ok(())
}
#[test]
fn lease_and_token_boundaries_fail_closed() -> Result<(), Box<dyn std::error::Error>> {
    for route in [Route::Activity, Route::Revoke, Route::Forget] {
        for bad in [
            "".into(),
            "é".into(),
            "has space".into(),
            "bad\0task".into(),
            "x".repeat(129),
        ] {
            let mut v = body(route);
            set(&mut v, "taskId", json!(bad))?;
            assert!(decode(route, &v).is_err());
        }
        let mut v = body(route);
        set(&mut v, "taskId", json!("x".repeat(128)))?;
        assert!(decode(route, &v).is_ok());
        for bad in [
            "AAECAwQFBgcICQoLDA0ODx",
            "AAECAwQFBgcICQoLDA0OD=",
            "",
            "AAECAwQFBgcICQoLDA0ODwA",
        ] {
            let mut v = body(route);
            set(&mut v, "leaseNonce", json!(bad))?;
            assert!(decode(route, &v).is_err());
        }
        for bad in [ID.to_uppercase(), "0".repeat(31), "g".repeat(32)] {
            let mut v = body(route);
            set(&mut v, "sessionId", json!(bad))?;
            assert!(decode(route, &v).is_err());
        }
    }
    for field in ["signedGrant", "signedRevoke"] {
        let route = if field == "signedGrant" {
            Route::Activity
        } else {
            Route::Revoke
        };
        for bad in [
            "".into(),
            TOKEN.replace("ngr1", "ngr2"),
            format!("{TOKEN}.extra"),
            TOKEN.replace("e30", "e3="),
            format!("ngr1.e30.{}B", "A".repeat(42)),
            "s".repeat(8193),
        ] {
            let mut v = body(route);
            set(&mut v, field, json!(bad))?;
            assert!(decode(route, &v).is_err());
        }
    }
    let mut v = body(Route::Forget);
    set(&mut v, "snapshotId", json!("A".repeat(32)))?;
    assert!(decode(Route::Forget, &v).is_err());
    assert_eq!(
        Request::decode(Route::Ready, meta(None), &vec![b' '; MAX_REQUEST_BYTES + 1]).err(),
        Some(Error::Capacity)
    );
    let mut maximum_token = body(Route::Activity);
    set(
        &mut maximum_token,
        "signedGrant",
        json!(format!("ngr1.{}.{}", "A".repeat(8143), "A".repeat(43))),
    )?;
    assert!(decode(Route::Activity, &maximum_token).is_ok());
    let mut padded = body(Route::Ready).to_string().into_bytes();
    padded.resize(MAX_REQUEST_BYTES, b' ');
    assert!(Request::decode(Route::Ready, meta(None), &padded).is_ok());
    for bad in [0, MAX_SAFE_INTEGER + 1] {
        let mut m = meta(None);
        m.deadline_ms = bad;
        assert!(
            Request::decode(Route::Ready, m, body(Route::Ready).to_string().as_bytes()).is_err()
        );
    }
    Ok(())
}
