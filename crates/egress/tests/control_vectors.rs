use egress::control::*;
use serde_json::Value;
use std::error::Error as StdError;

type TestResult<T = ()> = Result<T, Box<dyn StdError>>;
fn fixture() -> TestResult<Value> {
    Ok(serde_json::from_str(include_str!(
        "../../../contracts/egress-control.v1.vectors.json"
    ))?)
}
fn text<'a>(value: &'a Value, key: &str) -> TestResult<&'a str> {
    value.get(key).and_then(Value::as_str).ok_or("text".into())
}
fn bytes<const N: usize>(value: &str) -> TestResult<[u8; N]> {
    let mut out = [0; N];
    for (i, slot) in out.iter_mut().enumerate() {
        *slot = u8::from_str_radix(value.get(i * 2..i * 2 + 2).ok_or("hex")?, 16)?;
    }
    if value.len() != N * 2 {
        return Err("hex length".into());
    }
    Ok(out)
}
fn keys(f: &Value) -> TestResult<(FileKey, EpochKeys)> {
    let master = FileKey::from_hex(text(f, "masterKey")?)?;
    let file = derive_web_file_key(&master, &bytes(text(f, "webNonce")?)?)?;
    let epoch = derive_epoch_keys(
        &file,
        &bytes(text(f, "bootId")?)?,
        &bytes(text(f, "sessionId")?)?,
    )?;
    Ok((file, epoch))
}
fn meta(c: &Value) -> TestResult<RequestMetadata<'_>> {
    Ok(RequestMetadata {
        request_id: text(c, "requestId")?,
        deadline_ms: c
            .get("deadlineMs")
            .and_then(Value::as_u64)
            .ok_or("deadline")?,
        boot_id: c.get("bootId").and_then(Value::as_str),
        key_fingerprint: text(c, "keyFingerprint")?,
    })
}

#[test]
fn independent_stdlib_derivation_request_and_response_vectors() -> TestResult {
    let f = fixture()?;
    assert_eq!(text(&f, "capability")?, CAPABILITY);
    let (file, epoch) = keys(&f)?;
    assert_eq!(file.fingerprint(), text(&f, "fingerprint")?);
    assert_eq!(
        FileKey::from_hex(text(&f, "fileKey")?)?.fingerprint(),
        file.fingerprint()
    );
    assert_eq!(
        FileKey::from_hex(&format!("{}\n", text(&f, "fileKey")?))?.fingerprint(),
        file.fingerprint()
    );
    let grant = egress::verify(
        &epoch.grant,
        text(&f, "grantToken")?.as_bytes(),
        1_700_000_000_001,
    )
    .map_err(|e| format!("synthetic grant verification: {e:?}"))?;
    assert_eq!(grant.task, "task-1");
    for c in f.get("cases").and_then(Value::as_array).ok_or("cases")? {
        let route = Route::parse(text(c, "route")?)?;
        let body = text(c, "body")?.as_bytes();
        let tag = text(c, "requestMac")?;
        assert_eq!(epoch.control.sign_request(route, meta(c)?, body)?, tag);
        epoch.control.verify_request(route, meta(c)?, body, tag)?;
        let status = u16::try_from(
            c.get("responseStatus")
                .and_then(Value::as_u64)
                .ok_or("status")?,
        )?;
        let reply = text(c, "responseBody")?.as_bytes();
        let reply_tag = text(c, "responseMac")?;
        assert_eq!(epoch.control.sign_response(tag, status, reply)?, reply_tag);
        epoch
            .control
            .verify_response(tag, status, reply, reply_tag)?;
    }
    Ok(())
}

#[test]
fn epoch_changes_refuse_old_grants_without_changing_ngr1_codec() -> TestResult {
    let f = fixture()?;
    let (file, _) = keys(&f)?;
    let master = FileKey::from_hex(text(&f, "masterKey")?)?;
    assert_ne!(
        derive_web_file_key(&master, &[255; 32])?.fingerprint(),
        file.fingerprint()
    );
    assert_ne!(
        derive_web_file_key(
            &FileKey::from_bytes([255; 32]),
            &bytes(text(&f, "webNonce")?)?
        )?
        .fingerprint(),
        file.fingerprint()
    );
    let boot = bytes(text(&f, "bootId")?)?;
    let session = bytes(text(&f, "sessionId")?)?;
    for epoch in [
        derive_epoch_keys(&file, &[255; 16], &session)?,
        derive_epoch_keys(&file, &boot, &[255; 16])?,
        derive_epoch_keys(&FileKey::from_bytes([255; 32]), &boot, &session)?,
    ] {
        assert!(egress::verify(
            &epoch.grant,
            text(&f, "grantToken")?.as_bytes(),
            1_700_000_000_001
        )
        .is_err());
    }
    assert_eq!(format!("{file:?}"), "FileKey(<redacted>)");
    assert_eq!(
        format!("{:?}", derive_control_key(&file)?),
        "ControlKey(<redacted>)"
    );
    Ok(())
}

#[test]
fn byte_identity_route_epoch_and_response_roles_cannot_be_swapped() -> TestResult {
    let f = fixture()?;
    let (_, epoch) = keys(&f)?;
    let cases = f.get("cases").and_then(Value::as_array).ok_or("cases")?;
    let c = cases.get(2).ok_or("activity")?;
    let m = meta(c)?;
    let body = text(c, "body")?.as_bytes();
    let tag = text(c, "requestMac")?;
    assert_eq!(
        derive_control_key(&FileKey::from_bytes([255; 32]))?.verify_request(
            Route::Activity,
            m,
            body,
            tag
        ),
        Err(Error::Authentication)
    );
    let mut altered = body.to_vec();
    altered.push(b' ');
    assert_eq!(
        epoch
            .control
            .verify_request(Route::Activity, m, &altered, tag),
        Err(Error::Authentication)
    );
    assert_eq!(
        epoch.control.verify_request(Route::Revoke, m, body, tag),
        Err(Error::Authentication)
    );
    for changed in [
        RequestMetadata {
            request_id: "ffffffffffffffffffffffffffffffff",
            ..m
        },
        RequestMetadata {
            deadline_ms: m.deadline_ms + 1,
            ..m
        },
        RequestMetadata {
            boot_id: Some("ffffffffffffffffffffffffffffffff"),
            ..m
        },
        RequestMetadata {
            key_fingerprint: "ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff",
            ..m
        },
    ] {
        assert_eq!(
            epoch
                .control
                .verify_request(Route::Activity, changed, body, tag),
            Err(Error::Authentication)
        );
    }
    let reply = text(c, "responseBody")?.as_bytes();
    let reply_tag = text(c, "responseMac")?;
    assert_eq!(
        epoch.control.verify_response(tag, 200, reply, tag),
        Err(Error::Authentication)
    );
    assert_eq!(
        epoch.control.verify_response(tag, 409, reply, reply_tag),
        Err(Error::Authentication)
    );
    assert_eq!(
        epoch
            .control
            .verify_response(reply_tag, 200, reply, reply_tag),
        Err(Error::Authentication)
    );
    assert_eq!(
        epoch
            .control
            .verify_request(Route::Activity, m, body, reply_tag),
        Err(Error::Authentication)
    );
    let mut changed_reply = reply.to_vec();
    changed_reply.push(b' ');
    assert_eq!(
        epoch
            .control
            .verify_response(tag, 200, &changed_reply, reply_tag),
        Err(Error::Authentication)
    );
    Ok(())
}

#[test]
fn malformed_metadata_numeric_spellings_and_byte_limits_refuse() -> TestResult {
    let f = fixture()?;
    let (_, epoch) = keys(&f)?;
    let c = f
        .get("cases")
        .and_then(Value::as_array)
        .and_then(|v| v.first())
        .ok_or("ready")?;
    let m = meta(c)?;
    for bad in [
        "",
        "0",
        "01",
        "+1",
        "-1",
        "1.0",
        "1e3",
        " 1",
        "1\n",
        "１",
        "1\0",
        "9007199254740992",
    ] {
        assert_eq!(parse_deadline_ms(bad), Err(Error::Malformed));
    }
    assert_eq!(parse_deadline_ms("9007199254740991")?, MAX_SAFE_INTEGER);
    for bad in [
        "/v1/control/ready?x=1",
        "http://example.com/v1/control/ready",
        "/v1/control/READY",
        "/v1/revoke",
    ] {
        assert_eq!(Route::parse(bad), Err(Error::Malformed));
    }
    for changed in [
        RequestMetadata {
            request_id: "0000000000000000000000000000000\0",
            ..m
        },
        RequestMetadata {
            request_id: "0000000000000000000000000000000\n",
            ..m
        },
        RequestMetadata {
            deadline_ms: 0,
            ..m
        },
        RequestMetadata {
            deadline_ms: MAX_SAFE_INTEGER + 1,
            ..m
        },
        RequestMetadata {
            boot_id: Some("00"),
            ..m
        },
    ] {
        assert_eq!(
            epoch.control.sign_request(Route::Ready, changed, b"{}"),
            Err(Error::Malformed)
        );
    }
    assert_eq!(
        epoch.control.sign_request(Route::Renew, m, b"{}"),
        Err(Error::Malformed)
    );
    assert!(epoch
        .control
        .sign_request(Route::Ready, m, &vec![0; MAX_REQUEST_BYTES])
        .is_ok());
    assert_eq!(
        epoch
            .control
            .sign_request(Route::Ready, m, &vec![0; MAX_REQUEST_BYTES + 1]),
        Err(Error::Capacity)
    );
    let tag = text(c, "requestMac")?;
    assert!(epoch
        .control
        .sign_response(tag, 200, &vec![0; MAX_RESPONSE_BYTES])
        .is_ok());
    assert_eq!(
        epoch
            .control
            .sign_response(tag, 200, &vec![0; MAX_RESPONSE_BYTES + 1]),
        Err(Error::Capacity)
    );
    assert_eq!(
        epoch.control.sign_response(tag, 99, b"{}"),
        Err(Error::Malformed)
    );
    assert_eq!(
        epoch.control.sign_response(tag, 600, b"{}"),
        Err(Error::Malformed)
    );
    for bad in [
        "0".repeat(63),
        "0".repeat(65),
        format!("{}\0", "0".repeat(63)),
        "F".repeat(64),
    ] {
        assert_eq!(
            epoch.control.verify_request(Route::Ready, m, b"{}", &bad),
            Err(Error::Malformed)
        );
        assert!(FileKey::from_hex(&bad).is_err());
    }
    Ok(())
}
