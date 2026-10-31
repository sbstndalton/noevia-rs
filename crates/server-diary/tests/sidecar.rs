//! The Diary client against a synthetic sidecar on loopback: what each call carries (bearer, tenant,
//! storage descriptor, legacy marker, signed assertion, verified with `tenant-assertion`), the 428
//! retry, the endpoint policy, and every route's answers, caps and deadlines. Synthetic data only.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]
mod support;

use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use js_json::JValue;
use server_diary::{Diary, Limits, Reply, ReplyBody, Request, Route};
use sha2::{Digest, Sha256};
use std::time::Duration;
use support::*;

const NOW: fn() -> f64 = || {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs_f64()
};

fn sha(data: &[u8]) -> String {
    Sha256::digest(data)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

enum Got {
    Json(u16, String),
    /// status, the bytes received, whether the stream ended cleanly
    Zip(u16, Vec<u8>, bool),
}

impl Got {
    fn status(&self) -> u16 {
        match self {
            Got::Json(s, _) | Got::Zip(s, _, _) => *s,
        }
    }
    fn text(&self) -> &str {
        match self {
            Got::Json(_, t) => t,
            Got::Zip(..) => panic!("a stream"),
        }
    }
}

async fn run(
    d: &Diary,
    w: &World,
    id: &str,
    route: Route,
    method: &str,
    target: &str,
    body: &[u8],
) -> Got {
    run_as(d, w.ctx(id, true), route, method, target, body, None).await
}

async fn run_as(
    d: &Diary,
    ctx: server_diary::Ctx,
    route: Route,
    method: &str,
    target: &str,
    body: &[u8],
    content_length: Option<&str>,
) -> Got {
    let reply: Reply = d
        .serve(
            route,
            ctx,
            Request {
                method: method.into(),
                target: target.into(),
                content_length: content_length.map(str::to_owned),
            },
            Full::new(Bytes::copy_from_slice(body)),
        )
        .await;
    match reply.body {
        ReplyBody::Json(t) => Got::Json(reply.status, t),
        ReplyBody::Zip(mut b) => {
            let mut bytes = Vec::new();
            let mut clean = true;
            while let Some(frame) = b.frame().await {
                match frame {
                    Ok(f) => bytes.extend_from_slice(&f.into_data().unwrap()),
                    Err(_) => {
                        clean = false;
                        break;
                    }
                }
            }
            Got::Zip(reply.status, bytes, clean)
        }
    }
}

fn verify(seen: &Seen, key: &str, user: &str, body_hash: &str) {
    let assertion = seen.header("x-cowork-tenant-assertion").expect("assertion");
    let r = tenant_assertion::Request {
        key,
        user_id: user,
        assertion,
        method: &seen.method,
        path: seen.path(),
        query: seen.query().as_bytes(),
        body_hash,
        storage: seen.header("x-cowork-storage").unwrap_or(""),
        legacy_owner: seen.header("x-cowork-legacy-owner").unwrap_or(""),
        blocked: seen.header("x-cowork-storage-blocked").unwrap_or(""),
        now: NOW(),
    };
    tenant_assertion::verify(&r).unwrap_or_else(|e| panic!("assertion refused: {}", e.reason()));
}

fn field<'a>(v: &'a JValue, k: &str) -> &'a JValue {
    v.get(k)
}

fn keys(v: &JValue) -> Vec<&str> {
    match v {
        JValue::Obj(items) => items.iter().map(|(k, _)| k.as_str()).collect(),
        _ => panic!("object"),
    }
}

fn webdav(secret: &str) -> Conn {
    Conn {
        kind: "webdav",
        base_url: "https://cloud.example/dav",
        secret: secret.into(),
    }
}

// ── what every call carries ────────────────────────────────────────────────────

#[tokio::test]
async fn a_call_carries_bearer_tenant_local_storage_and_a_verifiable_assertion() {
    let stub = Stub::start(|_, _| Out::json(200, r#"{"months":[]}"#)).await;
    let w = World::new(&[]);
    let d = diary(&stub, TENANT_KEY);
    run(
        &d,
        &w,
        MEMBER,
        Route::Source,
        "GET",
        "/api/diary/source",
        b"",
    )
    .await;
    let seen = &stub.requests()[0];
    assert_eq!(
        (seen.method.as_str(), seen.target.as_str()),
        ("GET", "/api/months")
    );
    assert_eq!(
        seen.header("authorization"),
        Some(&*format!("Bearer {BEARER}"))
    );
    assert_eq!(seen.header("x-cowork-user-id"), Some(MEMBER));
    assert_eq!(seen.header("content-type"), Some("application/json"));
    assert_eq!(seen.header("x-cowork-legacy-owner"), None);
    assert_eq!(seen.header("x-cowork-storage-blocked"), None);
    // No row: the default local connection, without the two credential flags, Node's key order.
    let storage = seen.storage();
    assert_eq!(
        keys(&storage),
        [
            "kind",
            "baseUrl",
            "bucket",
            "region",
            "username",
            "corpusRoot"
        ]
    );
    assert_eq!(field(&storage, "kind").as_str(), Some("local"));
    assert_eq!(field(&storage, "region").as_str(), Some("us-east-1"));
    verify(seen, TENANT_KEY, MEMBER, &sha(b""));
    // A different key does not verify: the key is what makes the assertion mean something.
    let r = tenant_assertion::Request {
        key: "another-key",
        user_id: MEMBER,
        assertion: seen.header("x-cowork-tenant-assertion").unwrap(),
        method: "GET",
        path: "/api/months",
        query: b"",
        body_hash: &sha(b""),
        storage: seen.header("x-cowork-storage").unwrap(),
        legacy_owner: "",
        blocked: "",
        now: NOW(),
    };
    assert_eq!(
        tenant_assertion::verify(&r),
        Err(tenant_assertion::Reject::BadSignature)
    );
}

#[tokio::test]
async fn without_a_token_or_a_key_neither_header_is_sent() {
    let stub = Stub::start(|_, _| Out::json(200, r#"{"months":[]}"#)).await;
    let w = World::new(&[]);
    let d = Diary::new(config(&stub, "  ", "", "")).unwrap();
    run(
        &d,
        &w,
        MEMBER,
        Route::Source,
        "GET",
        "/api/diary/source",
        b"",
    )
    .await;
    let seen = &stub.requests()[0];
    assert_eq!(seen.header("authorization"), None);
    assert_eq!(seen.header("x-cowork-tenant-assertion"), None);
    assert_eq!(seen.header("x-cowork-user-id"), Some(MEMBER));
}

#[tokio::test]
async fn the_legacy_owner_marker_follows_migration_json_in_the_callers_own_workspace() {
    let stub = Stub::start(|_, _| Out::json(200, r#"{"months":[]}"#)).await;
    let w = World::new(&[]);
    w.mark_legacy(ADMIN);
    let d = diary(&stub, TENANT_KEY);
    run(
        &d,
        &w,
        ADMIN,
        Route::Source,
        "GET",
        "/api/diary/source",
        b"",
    )
    .await;
    run(
        &d,
        &w,
        MEMBER,
        Route::Source,
        "GET",
        "/api/diary/source",
        b"",
    )
    .await;
    let seen = stub.requests();
    assert_eq!(seen[0].header("x-cowork-legacy-owner"), Some("1"));
    assert_eq!(seen[1].header("x-cowork-legacy-owner"), None);
    // The marker is bound into the assertion: stripping it breaks the signature.
    verify(&seen[0], TENANT_KEY, ADMIN, &sha(b""));
    let mut stripped = seen[0].clone();
    stripped.headers.remove("x-cowork-legacy-owner");
    let result = std::panic::catch_unwind(|| verify(&stripped, TENANT_KEY, ADMIN, &sha(b"")));
    assert!(
        result.is_err(),
        "a request without its legacy marker must not verify"
    );
}

#[tokio::test]
async fn one_tenant_never_speaks_for_another() {
    let stub = Stub::start(|_, _| Out::json(200, r#"{"months":[]}"#)).await;
    let w = World::new(&[(MEMBER, webdav(SECRET))]);
    let d = diary(&stub, TENANT_KEY);
    // The query names another person; the identity the gate validated decides.
    run(
        &d,
        &w,
        OTHER,
        Route::Source,
        "GET",
        &format!("/api/diary/source?user_id={MEMBER}"),
        b"",
    )
    .await;
    run(
        &d,
        &w,
        MEMBER,
        Route::Source,
        "GET",
        "/api/diary/source",
        b"",
    )
    .await;
    let seen = stub.requests();
    assert_eq!(seen[0].header("x-cowork-user-id"), Some(OTHER));
    assert_eq!(field(&seen[0].storage(), "kind").as_str(), Some("local"));
    assert!(
        !seen[0].query().contains("user_id"),
        "the browser's query is not forwarded to /api/months"
    );
    assert_eq!(seen[1].header("x-cowork-user-id"), Some(MEMBER));
    assert_eq!(field(&seen[1].storage(), "kind").as_str(), Some("webdav"));
}

// ── the storage descriptor and the 428 retry ───────────────────────────────────

#[tokio::test]
async fn with_the_key_a_remote_secret_is_a_ref_and_a_428_repeats_the_call_once_with_it() {
    let stub = Stub::start(|_, n| {
        if n == 0 {
            Out::json(428, r#"{"detail":{"code":"storage_credential_required"}}"#)
        } else {
            Out::json(200, r#"{"months":[{"id":"2026-09"}]}"#)
        }
    })
    .await;
    let w = World::new(&[(MEMBER, webdav(SECRET))]);
    w.mark_legacy(MEMBER);
    let d = diary(&stub, TENANT_KEY);
    let got = run(
        &d,
        &w,
        MEMBER,
        Route::Source,
        "GET",
        "/api/diary/source",
        b"",
    )
    .await;
    assert_eq!(
        got.text(),
        r#"{"source":"sidecar","months":[{"id":"2026-09","label":"2026-09"}]}"#
    );
    let seen = stub.requests();
    assert_eq!(seen.len(), 2);
    let (first, second) = (&seen[0], &seen[1]);
    // First: no secret anywhere, a name for it instead.
    assert!(!first.headers.values().any(|v| v.contains(SECRET)));
    let s1 = first.storage();
    assert_eq!(
        keys(&s1),
        [
            "kind",
            "baseUrl",
            "bucket",
            "region",
            "username",
            "corpusRoot",
            "secretConfigured",
            "secretNeedsReauth",
            "secretRef"
        ]
    );
    let expected_ref = tenant_assertion::storage_secret_ref(TENANT_KEY, MEMBER, SECRET).unwrap();
    assert_eq!(
        field(&s1, "secretRef").as_str(),
        Some(expected_ref.as_str())
    );
    assert_eq!(field(&s1, "secret"), &JValue::Undefined);
    assert_eq!(field(&s1, "secretConfigured"), &JValue::Bool(true));
    assert_eq!(
        field(&s1, "baseUrl").as_str(),
        Some("https://cloud.example/dav")
    );
    // Second: the same ref plus the secret, a fresh nonce, both signatures valid, marker kept.
    let s2 = second.storage();
    assert_eq!(field(&s2, "secret").as_str(), Some(SECRET));
    assert_eq!(
        field(&s2, "secretRef").as_str(),
        Some(expected_ref.as_str())
    );
    assert_ne!(
        first.header("x-cowork-tenant-assertion"),
        second.header("x-cowork-tenant-assertion")
    );
    for s in [first, second] {
        assert_eq!(s.header("x-cowork-legacy-owner"), Some("1"));
        verify(s, TENANT_KEY, MEMBER, &sha(b""));
    }
}

#[tokio::test]
async fn a_second_428_is_the_answer_and_there_is_no_third_call() {
    let stub = Stub::start(|_, _| Out::json(428, r#"{"detail":"credential needed"}"#)).await;
    let w = World::new(&[(MEMBER, webdav(SECRET))]);
    let d = diary(&stub, TENANT_KEY);
    let got = run(
        &d,
        &w,
        MEMBER,
        Route::StorageImport,
        "POST",
        "/api/diary/storage-import",
        b"{}",
    )
    .await;
    assert_eq!(
        (got.status(), got.text()),
        (428, r#"{"error":"credential needed"}"#)
    );
    assert_eq!(stub.requests().len(), 2);
}

#[tokio::test]
async fn only_a_428_triggers_the_repeat() {
    for status in [401, 409, 424, 500, 503] {
        let stub = Stub::start(move |_, _| Out::json(status, r#"{"detail":"no"}"#)).await;
        let w = World::new(&[(MEMBER, webdav(SECRET))]);
        let d = diary(&stub, TENANT_KEY);
        run(
            &d,
            &w,
            MEMBER,
            Route::StorageImport,
            "POST",
            "/api/diary/storage-import",
            b"{}",
        )
        .await;
        assert_eq!(stub.requests().len(), 1, "{status}");
    }
}

#[tokio::test]
async fn without_the_key_the_whole_connection_rides_on_one_call_and_a_428_is_not_repeated() {
    let stub = Stub::start(|_, _| Out::json(428, r#"{"detail":"x"}"#)).await;
    let w = World::new(&[(MEMBER, webdav(SECRET))]);
    let d = Diary::new(config(&stub, BEARER, "", "https://cloud.example")).unwrap();
    run(
        &d,
        &w,
        MEMBER,
        Route::StorageImport,
        "POST",
        "/api/diary/storage-import",
        b"{}",
    )
    .await;
    let seen = stub.requests();
    assert_eq!(seen.len(), 1);
    let s = seen[0].storage();
    // getStorage's own order: the secret before the two flags, no ref.
    assert_eq!(
        keys(&s),
        [
            "kind",
            "baseUrl",
            "bucket",
            "region",
            "username",
            "corpusRoot",
            "secret",
            "secretConfigured",
            "secretNeedsReauth"
        ]
    );
    assert_eq!(field(&s, "secret").as_str(), Some(SECRET));
    assert_eq!(seen[0].header("x-cowork-tenant-assertion"), None);
}

#[tokio::test]
async fn a_remote_connection_with_no_secret_has_an_empty_ref_and_no_secret_field() {
    let stub = Stub::start(|_, n| {
        if n == 0 {
            Out::json(428, "{}")
        } else {
            Out::json(200, "{}")
        }
    })
    .await;
    let w = World::new(&[(MEMBER, webdav(""))]);
    let d = diary(&stub, TENANT_KEY);
    run(
        &d,
        &w,
        MEMBER,
        Route::StorageImport,
        "POST",
        "/api/diary/storage-import",
        b"{}",
    )
    .await;
    for s in stub.requests() {
        let storage = s.storage();
        assert_eq!(field(&storage, "secretRef").as_str(), Some(""));
        assert_eq!(field(&storage, "secret"), &JValue::Undefined);
        assert_eq!(field(&storage, "secretConfigured"), &JValue::Bool(false));
    }
}

#[tokio::test]
async fn an_unreadable_stored_secret_is_sent_as_not_configured_and_needing_reauth() {
    let stub = Stub::start(|_, _| Out::json(200, r#"{"months":[]}"#)).await;
    let w = World::new(&[(MEMBER, webdav(SECRET))]);
    // Another key: the stored credential cannot be opened (Node's secretNeedsReauth).
    std::fs::write(w.dir.path().join("secrets.key"), vec![1u8; 32]).unwrap();
    let d = diary(&stub, TENANT_KEY);
    run(
        &d,
        &w,
        MEMBER,
        Route::Source,
        "GET",
        "/api/diary/source",
        b"",
    )
    .await;
    let s = stub.requests()[0].storage();
    assert_eq!(field(&s, "secretConfigured"), &JValue::Bool(false));
    assert_eq!(field(&s, "secretNeedsReauth"), &JValue::Bool(true));
    assert_eq!(field(&s, "secretRef").as_str(), Some(""));
}

// ── the member-origin policy ────────────────────────────────────────────────────

#[tokio::test]
async fn a_members_unapproved_endpoint_is_blocked_and_its_credentials_never_leave() {
    let stub = Stub::start(|_, n| {
        if n == 0 {
            Out::json(428, "{}")
        } else {
            Out::json(200, r#"{"months":[]}"#)
        }
    })
    .await;
    let w = World::new(&[(MEMBER, webdav(SECRET))]);
    let d = Diary::new(config(&stub, BEARER, TENANT_KEY, "")).unwrap();
    run(
        &d,
        &w,
        MEMBER,
        Route::Source,
        "GET",
        "/api/diary/source",
        b"",
    )
    .await;
    let seen = stub.requests();
    // Blocked is not a reason to retry with credentials: the sidecar's 428 still gets the one
    // repeat, but the repeat is blocked as well and carries no secret.
    for s in &seen {
        assert_eq!(s.header("x-cowork-storage-blocked"), Some("1"));
        assert_eq!(
            s.storage(),
            JValue::Obj(vec![("kind".into(), JValue::Str("blocked".into()))])
        );
        assert!(!s
            .headers
            .values()
            .any(|v| v.contains(SECRET) || v.contains("cloud.example")));
        verify(s, TENANT_KEY, MEMBER, &sha(b""));
    }
}

#[tokio::test]
async fn an_admin_or_a_listed_origin_is_approved() {
    let stub = Stub::start(|_, _| Out::json(200, r#"{"months":[]}"#)).await;
    let w = World::new(&[(ADMIN, webdav(SECRET)), (MEMBER, webdav(SECRET))]);
    let d = diary(&stub, TENANT_KEY);
    run(
        &d,
        &w,
        ADMIN,
        Route::Source,
        "GET",
        "/api/diary/source",
        b"",
    )
    .await;
    let admin = &stub.requests()[0];
    assert_eq!(admin.header("x-cowork-storage-blocked"), None);
    assert_eq!(field(&admin.storage(), "kind").as_str(), Some("webdav"));
    // MEMBER_OUTBOUND_ORIGINS names the origin (trimmed, among others).
    let listed = Diary::new(config(
        &stub,
        BEARER,
        TENANT_KEY,
        " https://other.example , https://cloud.example ",
    ))
    .unwrap();
    run(
        &listed,
        &w,
        MEMBER,
        Route::Source,
        "GET",
        "/api/diary/source",
        b"",
    )
    .await;
    let member = &stub.requests()[1];
    assert_eq!(member.header("x-cowork-storage-blocked"), None);
    assert_eq!(field(&member.storage(), "kind").as_str(), Some("webdav"));
    // Another origin on the same host is not the listed one.
    let other = Diary::new(config(
        &stub,
        BEARER,
        TENANT_KEY,
        "https://cloud.example:8443",
    ))
    .unwrap();
    run(
        &other,
        &w,
        MEMBER,
        Route::Source,
        "GET",
        "/api/diary/source",
        b"",
    )
    .await;
    assert_eq!(
        stub.requests()[2].header("x-cowork-storage-blocked"),
        Some("1")
    );
}

#[tokio::test]
async fn blocking_is_decided_per_request_from_the_current_environment_value() {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    let stub = Stub::start(|_, _| Out::json(200, r#"{"months":[]}"#)).await;
    let w = World::new(&[(MEMBER, webdav(SECRET))]);
    let allow = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&allow);
    let d = Diary::new(server_diary::DiaryConfig::new(
        stub.base(),
        BEARER,
        TENANT_KEY,
        move || {
            if flag.load(Ordering::SeqCst) {
                "https://cloud.example".into()
            } else {
                String::new()
            }
        },
    ))
    .unwrap();
    run(
        &d,
        &w,
        MEMBER,
        Route::Source,
        "GET",
        "/api/diary/source",
        b"",
    )
    .await;
    allow.store(true, Ordering::SeqCst);
    run(
        &d,
        &w,
        MEMBER,
        Route::Source,
        "GET",
        "/api/diary/source",
        b"",
    )
    .await;
    let seen = stub.requests();
    assert_eq!(seen[0].header("x-cowork-storage-blocked"), Some("1"));
    assert_eq!(seen[1].header("x-cowork-storage-blocked"), None);
}

// ── gates ──────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn every_route_is_gated_on_the_addon_before_anything_else_and_guards_its_method() {
    let stub = Stub::start(|_, _| Out::json(200, "{}")).await;
    let w = World::new(&[]);
    let d = diary(&stub, TENANT_KEY);
    for (route, method) in [
        (Route::Source, "GET"),
        (Route::Today, "GET"),
        (Route::StorageImport, "POST"),
        (Route::WorkspaceExport, "GET"),
        (Route::WorkspaceImport, "POST"),
        // The method guard comes after the add-on gate: a wrong method is still a 404 when off.
        (Route::StorageImport, "GET"),
        (Route::WorkspaceExport, "DELETE"),
    ] {
        let got = run_as(&d, w.ctx(MEMBER, false), route, method, "/x", b"{}", None).await;
        assert_eq!(
            (got.status(), got.text()),
            (404, r#"{"error":"Diary add-on is disabled"}"#),
            "{route:?} {method}"
        );
    }
    for (route, method) in [
        (Route::StorageImport, "GET"),
        (Route::StorageImport, "PUT"),
        (Route::WorkspaceExport, "POST"),
        (Route::WorkspaceExport, "HEAD"),
        (Route::WorkspaceImport, "GET"),
        (Route::WorkspaceImport, "PUT"),
    ] {
        let got = run(&d, &w, MEMBER, route, method, "/x", b"{}").await;
        assert_eq!(
            (got.status(), got.text()),
            (405, r#"{"error":"Method not allowed"}"#),
            "{route:?} {method}"
        );
    }
    // source and today have no method guard in core: any method reads.
    for method in ["GET", "POST", "PUT", "DELETE", "OPTIONS"] {
        assert_eq!(
            run(&d, &w, MEMBER, Route::Source, method, "/x", b"")
                .await
                .status(),
            200,
            "{method}"
        );
    }
    assert!(
        stub.requests()
            .iter()
            .all(|r| r.target.starts_with("/api/months") || r.target.starts_with("/api/day")),
        "a refused request never reaches the sidecar"
    );
}

// ── source ─────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn source_lists_real_months_sorted_with_labels_as_core_does() {
    let stub = Stub::start(|_, _| {
        Out::json(
            200,
            r#"{"months":[{"id":"2026-09","label":"September 2026"},{"id":"nope"},{"id":"2026-08"},
               {"id":"2026-1x"},{"id":"2026-10","label":""},{"id":"2026-07","label":7},{"id":"2026-07\n"},
               "2026-06",null,{"id":5},{"id":"2025-12","label":0}]}"#,
        )
    })
    .await;
    let w = World::new(&[]);
    let d = diary(&stub, TENANT_KEY);
    let got = run(
        &d,
        &w,
        MEMBER,
        Route::Source,
        "GET",
        "/api/diary/source",
        b"",
    )
    .await;
    assert_eq!(got.status(), 200);
    assert_eq!(
        got.text(),
        r#"{"source":"sidecar","months":[{"id":"2025-12","label":"2025-12"},{"id":"2026-07","label":7},{"id":"2026-08","label":"2026-08"},{"id":"2026-09","label":"September 2026"},{"id":"2026-10","label":"2026-10"}]}"#
    );
}

#[tokio::test]
async fn source_is_an_empty_list_whatever_goes_wrong() {
    let empty = r#"{"source":"sidecar","months":[]}"#;
    let w = World::new(&[]);
    for out in [
        || Out::json(503, r#"{"detail":"down"}"#),
        || Out::json(200, r#"{"months":"x"}"#),
        || Out::json(200, "[]"),
        || Out::json(200, "null"),
        || Out::text(200, "not json"),
        || Out::json(302, "{}"),
        || Out::Hangup,
    ] {
        let stub = Stub::start(move |_, _| out()).await;
        let d = diary(&stub, TENANT_KEY);
        let got = run(
            &d,
            &w,
            MEMBER,
            Route::Source,
            "GET",
            "/api/diary/source",
            b"",
        )
        .await;
        assert_eq!((got.status(), got.text()), (200, empty));
    }
    // No sidecar listening at all.
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let dead = format!("http://{}", l.local_addr().unwrap());
    drop(l);
    let d = Diary::new(server_diary::DiaryConfig::new(
        dead,
        BEARER,
        TENANT_KEY,
        String::new,
    ))
    .unwrap();
    let got = run(
        &d,
        &w,
        MEMBER,
        Route::Source,
        "GET",
        "/api/diary/source",
        b"",
    )
    .await;
    assert_eq!(got.text(), empty);
    // A base that is not a URL fails like fetch did, and is the same empty list.
    let d = Diary::new(server_diary::DiaryConfig::new(
        "not a url",
        BEARER,
        "",
        String::new,
    ))
    .unwrap();
    assert_eq!(
        run(&d, &w, MEMBER, Route::Source, "GET", "/x", b"")
            .await
            .text(),
        empty
    );
}

#[tokio::test]
async fn source_gives_up_on_a_stalled_sidecar_after_its_read_timeout() {
    let stub = Stub::start(|_, _| {
        Out::json(200, r#"{"months":[{"id":"2026-09"}]}"#).slow(Duration::from_secs(5))
    })
    .await;
    let w = World::new(&[]);
    let d = diary_with(
        &stub,
        TENANT_KEY,
        Limits {
            read_timeout: Duration::from_millis(200),
            ..Limits::default()
        },
    );
    let started = std::time::Instant::now();
    let got = run(
        &d,
        &w,
        MEMBER,
        Route::Source,
        "GET",
        "/api/diary/source",
        b"",
    )
    .await;
    assert_eq!(got.text(), r#"{"source":"sidecar","months":[]}"#);
    assert!(started.elapsed() < Duration::from_secs(3));
}

#[tokio::test]
async fn a_reply_over_8_mib_is_not_buffered_and_reads_as_failed() {
    let big = format!(
        r#"{{"months":[{{"id":"2026-09"}}],"pad":"{}"}}"#,
        "x".repeat(8 * 1024 * 1024)
    );
    let stub = Stub::start(move |_, _| Out::json(200, &big)).await;
    let w = World::new(&[]);
    let d = diary(&stub, TENANT_KEY);
    let got = run(
        &d,
        &w,
        MEMBER,
        Route::Source,
        "GET",
        "/api/diary/source",
        b"",
    )
    .await;
    assert_eq!(got.text(), r#"{"source":"sidecar","months":[]}"#);
    // The same cap on storage-import: Node's overflow result keeps the sidecar's status and has
    // no body, so the error text is the fallback.
    let big = format!(r#"{{"pad":"{}"}}"#, "x".repeat(8 * 1024 * 1024));
    let stub = Stub::start(move |_, _| Out::json(200, &big)).await;
    let d = diary(&stub, TENANT_KEY);
    let got = run(
        &d,
        &w,
        MEMBER,
        Route::StorageImport,
        "POST",
        "/api/diary/storage-import",
        b"{}",
    )
    .await;
    assert_eq!(
        (got.status(), got.text()),
        (200, r#"{"error":"Diary storage request failed"}"#)
    );
}

// ── today ──────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn today_reads_a_day_or_a_month_and_encodes_the_month() {
    let stub = Stub::start(|s, _| {
        if s.target.contains("month=") {
            Out::json(200, r#"{"log":"MONTH","standing":"S"}"#)
        } else {
            Out::json(200, r#"{"today_log":"TODAY"}"#)
        }
    })
    .await;
    let w = World::new(&[]);
    let d = diary(&stub, TENANT_KEY);
    let got = run(
        &d,
        &w,
        MEMBER,
        Route::Today,
        "GET",
        "/api/diary/today?month=2026-09",
        b"",
    )
    .await;
    assert_eq!(got.text(), r#"{"todayLog":"MONTH","standing":"S"}"#);
    let got = run(&d, &w, MEMBER, Route::Today, "GET", "/api/diary/today", b"").await;
    assert_eq!(got.text(), r#"{"todayLog":"TODAY","standing":""}"#);
    // An empty month is no month; a hostile one is encoded, never spliced into the URL.
    run(
        &d,
        &w,
        MEMBER,
        Route::Today,
        "GET",
        "/api/diary/today?month=",
        b"",
    )
    .await;
    run(
        &d,
        &w,
        MEMBER,
        Route::Today,
        "GET",
        "/api/diary/today?month=a%26b%3D%2F%20c%23d",
        b"",
    )
    .await;
    let seen = stub.requests();
    assert_eq!(seen[0].target, "/api/day?month=2026-09");
    assert_eq!(seen[1].target, "/api/day");
    assert_eq!(seen[2].target, "/api/day");
    assert_eq!(seen[3].target, "/api/day?month=a%26b%3D%2F%20c%23d");
    verify(&seen[0], TENANT_KEY, MEMBER, &sha(b""));
    verify(&seen[3], TENANT_KEY, MEMBER, &sha(b""));
}

#[tokio::test]
async fn today_takes_js_truthiness_of_the_sidecars_fields() {
    let w = World::new(&[]);
    for (body, expect) in [
        (
            r#"{"log":null,"today_log":"T"}"#,
            r#"{"todayLog":"T","standing":""}"#,
        ),
        (
            r#"{"log":"","today_log":"T"}"#,
            r#"{"todayLog":"","standing":""}"#,
        ),
        (
            r#"{"log":0,"standing":null}"#,
            r#"{"todayLog":"","standing":""}"#,
        ),
        (
            r#"{"log":["a"],"standing":{"k":1.0}}"#,
            r#"{"todayLog":["a"],"standing":{"k":1}}"#,
        ),
        (r#"{"log":1e21}"#, r#"{"todayLog":1e+21,"standing":""}"#),
        ("\"just text\"", r#"{"todayLog":"","standing":""}"#),
        ("null", r#"{"todayLog":"","standing":""}"#),
        ("[1]", r#"{"todayLog":"","standing":""}"#),
    ] {
        let stub = Stub::start(move |_, _| Out::json(200, body)).await;
        let d = diary(&stub, TENANT_KEY);
        let got = run(&d, &w, MEMBER, Route::Today, "GET", "/api/diary/today", b"").await;
        assert_eq!((got.status(), got.text()), (200, expect), "{body}");
    }
}

const UNAVAILABLE: &str =
    r#"{"error":"Diary is unavailable right now. Try again shortly.","code":"diaryUnavailable"}"#;

/// sbstndalton/noevia#2232: a Diary that cannot be reached is a 503 the page can word, with a
/// stable code and nothing about the address or the cause. Replies that are not about
/// reachability (a 404, a redirect) stay core's generic 500.
#[tokio::test]
async fn an_unreachable_or_gateway_failing_diary_is_a_clear_503_on_every_route() {
    let w = World::new(&[]);
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let dead = format!("http://{}", l.local_addr().unwrap());
    drop(l);
    let d = Diary::new(server_diary::DiaryConfig::new(
        dead.clone(),
        BEARER,
        TENANT_KEY,
        String::new,
    ))
    .unwrap();
    for (route, method, path, body) in [
        (
            Route::Today,
            "GET",
            "/api/diary/today?month=2026-10",
            &b""[..],
        ),
        (
            Route::StorageImport,
            "POST",
            "/api/diary/storage-import",
            b"{}",
        ),
        (
            Route::StorageStatus,
            "GET",
            "/api/diary/storage-status",
            b"",
        ),
    ] {
        let got = run(&d, &w, MEMBER, route, method, path, body).await;
        assert_eq!((got.status(), got.text()), (503, UNAVAILABLE), "{path}");
        assert!(!got.text().contains("127.0.0.1"), "{path}");
    }
    // A gateway-class answer is the same on every route that relays the sidecar.
    for status in [502, 503, 504] {
        let stub = Stub::start(move |_, _| Out::json(status, "{}")).await;
        let d = diary(&stub, TENANT_KEY);
        for (route, method, path, body) in [
            (Route::Today, "GET", "/api/diary/today", &b""[..]),
            (
                Route::StorageStatus,
                "GET",
                "/api/diary/storage-status",
                b"",
            ),
            (
                Route::StorageImport,
                "POST",
                "/api/diary/storage-import",
                b"{}",
            ),
        ] {
            let got = run(&d, &w, MEMBER, route, method, path, body).await;
            assert_eq!(
                (got.status(), got.text()),
                (503, UNAVAILABLE),
                "{status} {path}"
            );
        }
    }
}

/// The sidecar's own coded answers about the person's storage server (app.py) are not "the Diary is down":
/// they keep their status and code, and a throttle its wait (body and `Retry-After`), on all three routes.
#[tokio::test]
async fn the_sidecars_storage_codes_pass_through_and_are_not_unavailable() {
    let w = World::new(&[]);
    let cases: [(u16, &str, &str); 3] = [
        (
            503,
            r#"{"detail":"Storage is limiting sign-ins","code":"storageThrottled","retryAfter":7}"#,
            r#"{"error":"Storage is limiting sign-ins","code":"storageThrottled","retryAfter":7}"#,
        ),
        (
            502,
            r#"{"detail":"Storage upstream failed","code":"storageUpstream"}"#,
            r#"{"error":"Storage upstream failed","code":"storageUpstream"}"#,
        ),
        (
            424,
            r#"{"detail":"Login rejected","code":"storageLoginRejected"}"#,
            r#"{"error":"Login rejected","code":"storageLoginRejected"}"#,
        ),
    ];
    for (status, sent, expect) in cases {
        let stub = Stub::start(move |_, _| Out::json(status, sent)).await;
        let d = diary(&stub, TENANT_KEY);
        for (route, method, path, body) in [
            (Route::Today, "GET", "/api/diary/today", &b""[..]),
            (
                Route::StorageStatus,
                "GET",
                "/api/diary/storage-status",
                b"",
            ),
            (
                Route::StorageImport,
                "POST",
                "/api/diary/storage-import",
                b"{}",
            ),
        ] {
            let got = run(&d, &w, MEMBER, route, method, path, body).await;
            assert_eq!((got.status(), got.text()), (status, expect), "{path}");
        }
    }
    // The wait also rides as Retry-After (clamped to a day), for the front to send.
    let stub = Stub::start(|_, _| {
        Out::json(
            503,
            r#"{"detail":"x","code":"storageThrottled","retryAfter":999999}"#,
        )
    })
    .await;
    let d = diary(&stub, TENANT_KEY);
    let reply = d
        .serve(
            Route::Today,
            w.ctx(MEMBER, true),
            Request {
                method: "GET".into(),
                target: "/api/diary/today".into(),
                content_length: None,
            },
            Full::new(Bytes::new()),
        )
        .await;
    assert_eq!(reply.retry_after, Some(86_400));
}

#[tokio::test]
async fn today_failures_are_nodes_generic_500() {
    let w = World::new(&[]);
    for out in [|| Out::json(404, "{}"), || Out::json(302, "{}")] {
        let stub = Stub::start(move |_, _| out()).await;
        let d = diary(&stub, TENANT_KEY);
        let got = run(&d, &w, MEMBER, Route::Today, "GET", "/api/diary/today", b"").await;
        assert_eq!(
            (got.status(), got.text()),
            (500, r#"{"error":"Internal error"}"#)
        );
    }
}

// ── storage-import ─────────────────────────────────────────────────────────────

#[tokio::test]
async fn storage_import_forwards_the_exact_body_signed_and_relays_the_reply() {
    let stub =
        Stub::start(|_, _| Out::json(200, r#"{"imported":true,"fileCount":3,"extra":[1.50,"é"]}"#))
            .await;
    let w = World::new(&[(MEMBER, webdav(SECRET))]);
    w.mark_legacy(MEMBER);
    let d = diary(&stub, TENANT_KEY);
    let body = br#"{"fingerprint":"abc"}"#;
    let got = run(
        &d,
        &w,
        MEMBER,
        Route::StorageImport,
        "POST",
        "/api/diary/storage-import",
        body,
    )
    .await;
    // Re-serialised as JSON.stringify writes it (1.50 -> 1.5).
    assert_eq!(
        (got.status(), got.text()),
        (200, r#"{"imported":true,"fileCount":3,"extra":[1.5,"é"]}"#)
    );
    let seen = &stub.requests()[0];
    assert_eq!(
        (seen.method.as_str(), seen.target.as_str()),
        ("POST", "/api/storage-import")
    );
    assert_eq!(seen.body, body);
    assert_eq!(seen.header("content-type"), Some("application/json"));
    verify(seen, TENANT_KEY, MEMBER, &sha(body));
    // A body that differs from the signed one does not verify (JSON bodies are bound).
    let r = std::panic::catch_unwind(|| verify(seen, TENANT_KEY, MEMBER, &sha(b"{}")));
    assert!(r.is_err());
}

#[tokio::test]
async fn storage_import_replaces_bad_utf8_as_node_decoded_it_and_signs_what_it_sends() {
    let stub = Stub::start(|_, _| Out::json(200, "{}")).await;
    let w = World::new(&[]);
    let d = diary(&stub, TENANT_KEY);
    run(
        &d,
        &w,
        MEMBER,
        Route::StorageImport,
        "POST",
        "/x",
        &[b'{', b'"', 0xff, b'"', b'}'],
    )
    .await;
    let seen = &stub.requests()[0];
    assert_eq!(seen.body, "{\"\u{fffd}\"}".as_bytes());
    verify(seen, TENANT_KEY, MEMBER, &sha(&seen.body));
}

#[tokio::test]
async fn storage_import_refuses_more_than_4096_bytes_before_calling_the_sidecar() {
    let stub = Stub::start(|_, _| Out::json(200, "{}")).await;
    let w = World::new(&[]);
    let d = diary(&stub, TENANT_KEY);
    let got = run(
        &d,
        &w,
        MEMBER,
        Route::StorageImport,
        "POST",
        "/x",
        &vec![b' '; 4097],
    )
    .await;
    assert_eq!(
        (got.status(), got.text()),
        (413, r#"{"error":"Request exceeds size limit"}"#)
    );
    assert!(stub.requests().is_empty());
    let got = run(
        &d,
        &w,
        MEMBER,
        Route::StorageImport,
        "POST",
        "/x",
        &vec![b' '; 4096],
    )
    .await;
    assert_eq!(got.status(), 200);
}

#[tokio::test]
async fn storage_import_error_shapes_follow_the_detail_or_fall_back() {
    let w = World::new(&[]);
    for (status, body, expect) in [
        (
            409,
            r#"{"detail":"The app diary is already active. Reload to see it."}"#,
            r#"{"error":"The app diary is already active. Reload to see it."}"#,
        ),
        (
            428,
            r#"{"detail":{"code":"cred"}}"#,
            r#"{"error":{"code":"cred"}}"#,
        ),
        (
            500,
            r#"{"detail":""}"#,
            r#"{"error":"Diary storage request failed"}"#,
        ),
        // A gateway-class status without one of the sidecar's storage codes is the Diary being down (#2232).
        (
            502,
            r#"{"error":"journal locked"}"#,
            r#"{"error":"Diary is unavailable right now. Try again shortly.","code":"diaryUnavailable"}"#,
        ),
        (
            503,
            "gateway down",
            r#"{"error":"Diary is unavailable right now. Try again shortly.","code":"diaryUnavailable"}"#,
        ),
        (400, "null", r#"{"error":"Diary storage request failed"}"#),
        (404, "", r#"{"error":"Diary storage request failed"}"#),
    ] {
        let stub = Stub::start(move |_, _| Out::json(status, body)).await;
        let d = diary(&stub, TENANT_KEY);
        let got = run(&d, &w, MEMBER, Route::StorageImport, "POST", "/x", b"{}").await;
        let status = if matches!(status, 502 | 503) {
            503
        } else {
            status
        };
        assert_eq!((got.status(), got.text()), (status, expect), "{body}");
    }
    // An OK reply that is not JSON is relayed as the JSON string it parsed to.
    let stub = Stub::start(|_, _| Out::text(200, "plain \"text\"")).await;
    let d = diary(&stub, TENANT_KEY);
    let got = run(&d, &w, MEMBER, Route::StorageImport, "POST", "/x", b"{}").await;
    assert_eq!((got.status(), got.text()), (200, r#""plain \"text\"""#));
}

#[tokio::test]
async fn storage_import_transport_failures_are_unavailable_and_a_redirect_is_nodes_generic_500() {
    let w = World::new(&[]);
    let stub = Stub::start(|_, _| Out::Hangup).await;
    let d = diary(&stub, TENANT_KEY);
    let got = run(&d, &w, MEMBER, Route::StorageImport, "POST", "/x", b"{}").await;
    assert_eq!((got.status(), got.text()), (503, UNAVAILABLE));
    let stub = Stub::start(|_, _| Out::json(307, "{}")).await;
    let d = diary(&stub, TENANT_KEY);
    let got = run(&d, &w, MEMBER, Route::StorageImport, "POST", "/x", b"{}").await;
    assert_eq!(
        (got.status(), got.text()),
        (500, r#"{"error":"Internal error"}"#)
    );
    let stub = Stub::start(|_, _| Out::json(200, "{}").slow(Duration::from_secs(5))).await;
    let d = diary_with(
        &stub,
        TENANT_KEY,
        Limits {
            storage_import_timeout: Duration::from_millis(200),
            ..Limits::default()
        },
    );
    let got = run(&d, &w, MEMBER, Route::StorageImport, "POST", "/x", b"{}").await;
    assert_eq!((got.status(), got.text()), (503, UNAVAILABLE));
}

// ── workspace-export ───────────────────────────────────────────────────────────

#[tokio::test]
async fn export_streams_the_archive_bytes_unchanged_with_the_tenants_signed_headers() {
    let archive = [0u8, 255, 80, 75, 0, 13, 10, 200, 201];
    let stub = Stub::start(move |_, _| Out::zip(&archive)).await;
    let w = World::new(&[]);
    let d = diary(&stub, TENANT_KEY);
    let got = run(
        &d,
        &w,
        MEMBER,
        Route::WorkspaceExport,
        "GET",
        "/api/diary/workspace-export",
        b"",
    )
    .await;
    let Got::Zip(status, bytes, clean) = got else {
        panic!("expected a stream")
    };
    assert_eq!(
        (status, bytes.as_slice(), clean),
        (200, archive.as_slice(), true)
    );
    let seen = &stub.requests()[0];
    assert_eq!(
        (seen.method.as_str(), seen.target.as_str()),
        ("GET", "/api/workspace-export")
    );
    assert_eq!(seen.header("x-cowork-user-id"), Some(MEMBER));
    verify(seen, TENANT_KEY, MEMBER, &sha(b""));
}

#[tokio::test]
async fn export_repeats_once_with_the_secret_after_a_428_and_streams_the_second_reply() {
    let stub = Stub::start(|_, n| {
        if n == 0 {
            Out::json(428, "{}")
        } else {
            Out::zip(b"PK-ok")
        }
    })
    .await;
    let w = World::new(&[(MEMBER, webdav(SECRET))]);
    let d = diary(&stub, TENANT_KEY);
    let Got::Zip(200, bytes, true) =
        run(&d, &w, MEMBER, Route::WorkspaceExport, "GET", "/x", b"").await
    else {
        panic!()
    };
    assert_eq!(bytes, b"PK-ok");
    let seen = stub.requests();
    assert_eq!(seen.len(), 2);
    assert_eq!(field(&seen[0].storage(), "secret"), &JValue::Undefined);
    assert_eq!(field(&seen[1].storage(), "secret").as_str(), Some(SECRET));
}

#[tokio::test]
async fn export_failure_answers_carry_the_sidecars_status_and_detail_or_fall_back() {
    let w = World::new(&[]);
    for (status, body, expect_status, expect) in [
        (
            409,
            r#"{"detail":"Finish pending writes"}"#,
            409,
            r#"{"error":"Finish pending writes"}"#,
        ),
        (
            500,
            r#"{"detail":{"x":1}}"#,
            500,
            r#"{"error":"Workspace export failed. Retry after checking storage."}"#,
        ),
        (
            503,
            "<html>",
            503,
            r#"{"error":"Workspace export failed. Retry after checking storage."}"#,
        ),
        (
            404,
            "",
            404,
            r#"{"error":"Workspace export failed. Retry after checking storage."}"#,
        ),
        // a JSON null body is a TypeError in core's `typeof body.detail`, answered by its catch
        (
            500,
            "null",
            502,
            r#"{"error":"Workspace export could not finish. Retry when storage is available."}"#,
        ),
    ] {
        let stub = Stub::start(move |_, _| Out::json(status, body)).await;
        let d = diary(&stub, TENANT_KEY);
        let got = run(&d, &w, MEMBER, Route::WorkspaceExport, "GET", "/x", b"").await;
        assert_eq!(
            (got.status(), got.text()),
            (expect_status, expect),
            "{body}"
        );
    }
    // A failure body over 64 KiB is not read; the fallback text stands.
    let big = format!(r#"{{"detail":"{}"}}"#, "x".repeat(70 * 1024));
    let stub = Stub::start(move |_, _| Out::json(500, &big)).await;
    let d = diary(&stub, TENANT_KEY);
    let got = run(&d, &w, MEMBER, Route::WorkspaceExport, "GET", "/x", b"").await;
    assert_eq!(
        (got.status(), got.text()),
        (
            500,
            r#"{"error":"Workspace export failed. Retry after checking storage."}"#
        )
    );
}

#[tokio::test]
async fn export_that_is_not_a_zip_or_cannot_reach_the_sidecar_is_a_502() {
    let w = World::new(&[]);
    let failed =
        r#"{"error":"Workspace export could not finish. Retry when storage is available."}"#;
    for out in [
        (|| Out::Full {
            status: 200,
            headers: vec![("content-type", "text/html".into())],
            body: b"x".to_vec(),
            delay: Duration::ZERO,
        }) as fn() -> Out,
        || Out::Full {
            status: 200,
            headers: vec![],
            body: b"x".to_vec(),
            delay: Duration::ZERO,
        },
        // core's startsWith is case-sensitive
        || Out::Full {
            status: 200,
            headers: vec![("content-type", "Application/Zip".into())],
            body: b"x".to_vec(),
            delay: Duration::ZERO,
        },
        || Out::Hangup,
        || Out::json(302, "{}"),
    ] {
        let stub = Stub::start(move |_, _| out()).await;
        let d = diary(&stub, TENANT_KEY);
        let got = run(&d, &w, MEMBER, Route::WorkspaceExport, "GET", "/x", b"").await;
        assert_eq!((got.status(), got.text()), (502, failed));
    }
    // application/zip with parameters still counts.
    let stub = Stub::start(|_, _| Out::Full {
        status: 200,
        headers: vec![("content-type", "application/zip; charset=binary".into())],
        body: b"PK".to_vec(),
        delay: Duration::ZERO,
    })
    .await;
    let d = diary(&stub, TENANT_KEY);
    assert!(matches!(
        run(&d, &w, MEMBER, Route::WorkspaceExport, "GET", "/x", b"").await,
        Got::Zip(200, _, true)
    ));
}

#[tokio::test]
async fn export_over_its_cap_is_cut_short_not_completed() {
    let chunk = vec![5u8; 1000];
    let c2 = chunk.clone();
    let stub = Stub::start(move |_, _| Out::Stream {
        status: 200,
        headers: vec![("content-type", "application/zip".into())],
        chunks: vec![c2.clone(); 10],
        forever: false,
    })
    .await;
    let w = World::new(&[]);
    // A cap of exactly the archive: complete. One byte less: cut.
    let d = diary_with(
        &stub,
        TENANT_KEY,
        Limits {
            export_cap: 10_000,
            ..Limits::default()
        },
    );
    let Got::Zip(200, bytes, clean) =
        run(&d, &w, MEMBER, Route::WorkspaceExport, "GET", "/x", b"").await
    else {
        panic!()
    };
    assert!(clean && bytes.len() == 10_000);
    let d = diary_with(
        &stub,
        TENANT_KEY,
        Limits {
            export_cap: 9_999,
            ..Limits::default()
        },
    );
    let Got::Zip(200, bytes, clean) =
        run(&d, &w, MEMBER, Route::WorkspaceExport, "GET", "/x", b"").await
    else {
        panic!()
    };
    assert!(!clean, "the client must see a broken download");
    assert!(
        bytes.len() <= 9_999,
        "nothing past the cap is delivered ({})",
        bytes.len()
    );
}

#[tokio::test]
async fn export_deadline_cuts_a_stalled_download_and_a_client_that_leaves_stops_the_sidecars_stream(
) {
    let w = World::new(&[]);
    let stub = Stub::start(|_, _| Out::Stream {
        status: 200,
        headers: vec![("content-type", "application/zip".into())],
        chunks: vec![b"PK".to_vec()],
        forever: true,
    })
    .await;
    // An endless stream hits the deadline.
    let d = diary_with(
        &stub,
        TENANT_KEY,
        Limits {
            export_deadline: Duration::from_millis(400),
            export_cap: usize::MAX,
            ..Limits::default()
        },
    );
    let Got::Zip(200, _, clean) =
        run(&d, &w, MEMBER, Route::WorkspaceExport, "GET", "/x", b"").await
    else {
        panic!()
    };
    assert!(!clean);
    // A client that reads one frame and drops the body: the stream stops growing.
    let d = diary(&stub, TENANT_KEY);
    let reply = d
        .serve(
            Route::WorkspaceExport,
            w.ctx(MEMBER, true),
            Request {
                method: "GET".into(),
                target: "/x".into(),
                content_length: None,
            },
            Full::new(Bytes::new()),
        )
        .await;
    let ReplyBody::Zip(mut body) = reply.body else {
        panic!()
    };
    let _ = body.frame().await;
    drop(body);
    tokio::time::sleep(Duration::from_millis(300)).await;
    let after_drop = stub.streamed.load(std::sync::atomic::Ordering::SeqCst);
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(
        stub.streamed.load(std::sync::atomic::Ordering::SeqCst),
        after_drop,
        "the sidecar stream was closed"
    );
}

#[tokio::test]
async fn a_client_that_leaves_while_the_sidecar_is_quiet_stops_the_sidecars_stream_at_once() {
    use std::sync::Arc;
    let w = World::new(&[]);
    let dropped = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let flag = Arc::clone(&dropped);
    let stub = Stub::start(move |_, _| Out::Stall {
        status: 200,
        headers: vec![("content-type", "application/zip".into())],
        chunk: b"PK".to_vec(),
        dropped: Arc::clone(&flag),
    })
    .await;
    // The deadline is far away: only the client leaving can end the copy within this test.
    let d = diary_with(
        &stub,
        TENANT_KEY,
        Limits {
            export_deadline: Duration::from_secs(60),
            export_cap: usize::MAX,
            ..Limits::default()
        },
    );
    let reply = d
        .serve(
            Route::WorkspaceExport,
            w.ctx(MEMBER, true),
            Request {
                method: "GET".into(),
                target: "/x".into(),
                content_length: None,
            },
            Full::new(Bytes::new()),
        )
        .await;
    let ReplyBody::Zip(mut body) = reply.body else {
        panic!()
    };
    let _ = body.frame().await;
    drop(body);
    for _ in 0..50 {
        if dropped.load(std::sync::atomic::Ordering::SeqCst) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("the sidecar's stream was still held a second after the client left");
}

// ── workspace-import ───────────────────────────────────────────────────────────

fn import_target(action: &str) -> String {
    format!(
        "/api/diary/workspace-import?action={action}&name=My%20Folder&fingerprint={}",
        "f".repeat(64)
    )
}

#[tokio::test]
async fn import_forwards_the_zip_and_query_signed_as_a_stream() {
    let preview = r#"{"fingerprint":"reviewed","fileCount":2}"#;
    let stub = Stub::start(move |_, _| Out::json(200, preview)).await;
    let w = World::new(&[]);
    w.mark_legacy(MEMBER);
    let d = diary(&stub, TENANT_KEY);
    let zip = [80u8, 75, 0, 255, 3, 4];
    let got = run(
        &d,
        &w,
        MEMBER,
        Route::WorkspaceImport,
        "POST",
        &import_target("preview"),
        &zip,
    )
    .await;
    assert_eq!((got.status(), got.text()), (200, preview));
    let seen = &stub.requests()[0];
    assert_eq!(seen.method, "POST");
    assert_eq!(seen.path(), "/api/workspace-import");
    assert_eq!(
        seen.query(),
        format!(
            "action=preview&name=My%20Folder&fingerprint={}",
            "f".repeat(64)
        )
    );
    assert_eq!(seen.body, zip);
    assert_eq!(seen.header("content-type"), Some("application/zip"));
    assert_eq!(seen.header("x-cowork-legacy-owner"), Some("1"));
    // The body of an upload is not covered (core: "stream"); everything else is.
    verify(seen, TENANT_KEY, MEMBER, "stream");
    let r = std::panic::catch_unwind(|| verify(seen, TENANT_KEY, MEMBER, &sha(&zip)));
    assert!(r.is_err());
}

#[tokio::test]
async fn import_428_resends_the_buffered_body_with_the_secret() {
    let stub = Stub::start(|_, n| {
        if n == 0 {
            Out::json(428, "{}")
        } else {
            Out::json(200, r#"{"imported":true}"#)
        }
    })
    .await;
    let w = World::new(&[(MEMBER, webdav(SECRET))]);
    let d = diary(&stub, TENANT_KEY);
    let got = run(
        &d,
        &w,
        MEMBER,
        Route::WorkspaceImport,
        "POST",
        &import_target("apply"),
        b"PKzip",
    )
    .await;
    assert_eq!((got.status(), got.text()), (200, r#"{"imported":true}"#));
    let seen = stub.requests();
    assert_eq!(seen.len(), 2);
    assert_eq!(seen[0].body, b"PKzip");
    assert_eq!(seen[1].body, b"PKzip");
    assert_eq!(field(&seen[1].storage(), "secret").as_str(), Some(SECRET));
    for s in &seen {
        verify(s, TENANT_KEY, MEMBER, "stream");
    }
}

#[tokio::test]
async fn import_refuses_over_32_mib_before_the_sidecar_by_header_or_by_bytes() {
    let stub = Stub::start(|_, _| Out::json(200, "{}")).await;
    let w = World::new(&[]);
    let too_large = r#"{"error":"Browser imports support ZIP files up to 32 MiB"}"#;
    let d = diary_with(
        &stub,
        TENANT_KEY,
        Limits {
            import_max_upload: 1000,
            ..Limits::default()
        },
    );
    // The declared length alone is enough (Node compares Number(content-length) with the cap).
    let got = run_as(
        &d,
        w.ctx(MEMBER, true),
        Route::WorkspaceImport,
        "POST",
        "/x",
        b"",
        Some("1001"),
    )
    .await;
    assert_eq!((got.status(), got.text()), (413, too_large));
    // A body that lies about its length is cut by the bytes.
    let got = run_as(
        &d,
        w.ctx(MEMBER, true),
        Route::WorkspaceImport,
        "POST",
        "/x",
        &vec![1u8; 1001],
        Some("5"),
    )
    .await;
    assert_eq!((got.status(), got.text()), (413, too_large));
    assert!(stub.requests().is_empty());
    // Exactly the cap is accepted; a garbled length is ignored, as NaN > MAX is false.
    let got = run_as(
        &d,
        w.ctx(MEMBER, true),
        Route::WorkspaceImport,
        "POST",
        "/x",
        &vec![1u8; 1000],
        Some("abc"),
    )
    .await;
    assert_eq!(got.status(), 200);
    // The shipped cap is Node's 32 MiB.
    assert_eq!(Limits::default().import_max_upload, 32 * 1024 * 1024);
}

#[tokio::test]
async fn import_reply_shapes_follow_core() {
    let w = World::new(&[]);
    let unavailable = r#"{"error":"Import response unavailable. Retry with the same file and folder to safely check whether it completed."}"#;
    for (status, body, expect_status, expect) in [
        (
            409,
            r#"{"detail":"Finish pending Diary writes before importing."}"#,
            409,
            r#"{"error":"Finish pending Diary writes before importing."}"#,
        ),
        (
            400,
            r#"{"detail":{"a":1}}"#,
            400,
            r#"{"error":"Workspace import failed"}"#,
        ),
        (
            413,
            r#"{"detail":"Browser imports support ZIP files up to 32 MiB"}"#,
            413,
            r#"{"error":"Browser imports support ZIP files up to 32 MiB"}"#,
        ),
        (500, "[1]", 500, r#"{"error":"Workspace import failed"}"#),
        (200, "null", 200, "null"),
        (
            200,
            r#"{"files":[],"bytes":1.0}"#,
            200,
            r#"{"files":[],"bytes":1}"#,
        ),
        // not JSON, an empty body, JSON null on an error: all throw in core and read as unavailable
        (200, "<html>", 502, unavailable),
        (500, "", 502, unavailable),
        (500, "null", 502, unavailable),
    ] {
        let stub = Stub::start(move |_, _| Out::json(status, body)).await;
        let d = diary(&stub, TENANT_KEY);
        let got = run(&d, &w, MEMBER, Route::WorkspaceImport, "POST", "/x", b"PK").await;
        assert_eq!(
            (got.status(), got.text()),
            (expect_status, expect),
            "{status} {body}"
        );
    }
}

#[tokio::test]
async fn import_caps_the_reply_and_keeps_secrets_out_of_failure_text() {
    let w = World::new(&[(MEMBER, webdav(SECRET))]);
    let unavailable = r#"{"error":"Import response unavailable. Retry with the same file and folder to safely check whether it completed."}"#;
    // A 5000-path duplicate preview over 1 MiB is parsed (#920) ...
    let files: Vec<String> = (0..5000)
        .map(|i| format!("{}/file-{i:05}", "d".repeat(100)))
        .collect();
    let array = format!(
        "[{}]",
        files
            .iter()
            .map(|f| format!("\"{f}\""))
            .collect::<Vec<_>>()
            .join(",")
    );
    let listing = format!(
        r#"{{"fingerprint":"{}","files":{array},"duplicates":{array}}}"#,
        "f".repeat(64)
    );
    assert!(listing.len() > 1024 * 1024 && listing.len() < 24 * 1024 * 1024);
    let l2 = listing.clone();
    let stub = Stub::start(move |_, _| Out::json(200, &l2)).await;
    let d = diary(&stub, TENANT_KEY);
    let got = run(&d, &w, MEMBER, Route::WorkspaceImport, "POST", "/x", b"PK").await;
    assert_eq!(got.status(), 200);
    assert_eq!(got.text().len(), listing.len());
    // ... and an endless reply is cut at the reply cap (here lowered).
    let stub = Stub::start(|_, _| Out::Stream {
        status: 200,
        headers: vec![("content-type", "application/json".into())],
        chunks: vec![],
        forever: true,
    })
    .await;
    let d = diary_with(
        &stub,
        TENANT_KEY,
        Limits {
            import_max_reply: 256 * 1024,
            ..Limits::default()
        },
    );
    let got = run(&d, &w, MEMBER, Route::WorkspaceImport, "POST", "/x", b"PK").await;
    assert_eq!((got.status(), got.text()), (502, unavailable));
    // The endless reply is dropped, not drained: once the answer is out the sidecar's stream stops growing
    // (how far it got before the cut depends on the kernel's socket buffers, so no byte count is asserted).
    tokio::time::sleep(Duration::from_millis(300)).await;
    let stopped_at = stub.streamed.load(std::sync::atomic::Ordering::SeqCst);
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(
        stub.streamed.load(std::sync::atomic::Ordering::SeqCst),
        stopped_at,
        "the endless reply was dropped"
    );
    assert_eq!(Limits::default().import_max_reply, 24 * 1024 * 1024);
    // A dead sidecar: the same fixed text, nothing about the connection or the credential.
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let dead = format!("http://{}", l.local_addr().unwrap());
    drop(l);
    let d = Diary::new(server_diary::DiaryConfig::new(
        dead,
        BEARER,
        TENANT_KEY,
        String::new,
    ))
    .unwrap();
    let got = run(&d, &w, MEMBER, Route::WorkspaceImport, "POST", "/x", b"PK").await;
    assert_eq!((got.status(), got.text()), (502, unavailable));
    assert!(!got.text().contains(SECRET) && !got.text().contains("127.0.0.1"));
}

#[tokio::test]
async fn import_deadline_covers_the_whole_call() {
    let w = World::new(&[]);
    let stub = Stub::start(|_, _| Out::json(200, "{}").slow(Duration::from_secs(5))).await;
    let d = diary_with(
        &stub,
        TENANT_KEY,
        Limits {
            import_deadline: Duration::from_millis(300),
            ..Limits::default()
        },
    );
    let started = std::time::Instant::now();
    let got = run(&d, &w, MEMBER, Route::WorkspaceImport, "POST", "/x", b"PK").await;
    assert_eq!(got.status(), 502);
    assert!(started.elapsed() < Duration::from_secs(3));
    assert_eq!(Limits::default().import_deadline, Duration::from_secs(120));
}

#[tokio::test]
async fn import_upload_that_stalls_past_the_deadline_is_answered_408() {
    use http_body_util::channel::Channel;
    let w = World::new(&[]);
    let stub = Stub::start(|_, _| Out::json(200, "{}")).await;
    let d = diary_with(
        &stub,
        TENANT_KEY,
        Limits {
            import_deadline: Duration::from_millis(200),
            ..Limits::default()
        },
    );
    let (mut tx, body) = Channel::<Bytes, std::convert::Infallible>::new(1);
    tx.send_data(Bytes::from_static(b"PK")).await.unwrap();
    let reply = d
        .serve(
            Route::WorkspaceImport,
            w.ctx(MEMBER, true),
            Request {
                method: "POST".into(),
                target: "/x".into(),
                content_length: None,
            },
            body,
        )
        .await;
    assert_eq!(reply.status, 408);
    assert!(stub.requests().is_empty());
    drop(tx);
}

// ── the shipped limits are Node's ──────────────────────────────────────────────

#[test]
fn default_limits_are_the_numbers_in_core() {
    let l = Limits::default();
    assert_eq!(l.read_timeout, Duration::from_millis(15000));
    assert_eq!(l.storage_import_timeout, Duration::from_millis(300000));
    assert_eq!(l.storage_import_body, 4096);
    assert_eq!(l.export_deadline, Duration::from_millis(300000));
    assert_eq!(l.export_cap, 272 * 1024 * 1024);
    assert_eq!(l.export_error_cap, 64 * 1024);
    assert_eq!(l.import_max_upload, 32 * 1024 * 1024);
    assert_eq!(l.import_max_reply, 24 * 1024 * 1024);
    assert_eq!(l.import_deadline, Duration::from_millis(120000));
}
