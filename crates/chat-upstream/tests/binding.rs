//! The turn-bound binding: token round trip, every tamper and expiry refused, the per-request
//! re-check in its order, the pinned model; and the request plan's headers against the other Rust
//! port of `providerHeaders` (engine-facts), so the two can never disagree.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use chat_upstream::binding::{self, Binding, Now, Recheck, TokenError};
use chat_upstream::plan::{self, Call};
use chat_upstream::target::{Fetch, KeyUse, Target};
use js_json::JValue;
use proptest::prelude::*;

const SECRET: &[u8] = b"qa-per-process-secret";

fn row(id: &str, base: &str, shared: bool) -> JValue {
    JValue::obj([
        ("id", JValue::Str(id.into())),
        ("baseUrl", JValue::Str(base.into())),
        ("apiKey", JValue::Str("qa-key".into())),
        ("shared", JValue::Bool(shared)),
    ])
}

fn bound(r: &JValue) -> Binding {
    Binding {
        user: "qa-member".into(),
        provider_id: r.get("id").as_str().unwrap().into(),
        url: chat_upstream::target::upstream_url(r.get("baseUrl")).unwrap(),
        fingerprint: binding::fingerprint(r),
        model: "qa-model".into(),
        chat_key: Some("qa-chat".into()),
        expires_ms: 2_000.0,
    }
}

#[test]
fn round_trip_and_expiry() {
    let r = row("qa-own", "https://qa.example/v1", false);
    let b = bound(&r);
    let t = binding::mint(SECRET, &b).unwrap();
    assert!(t.starts_with("v1."));
    assert_eq!(binding::open(SECRET, &t, 1_999.0), Ok(b.clone()));
    assert_eq!(binding::open(SECRET, &t, 2_000.0), Err(TokenError::Expired));
    assert_eq!(
        binding::open(SECRET, &t, f64::NAN),
        Err(TokenError::Expired)
    );
    assert_eq!(binding::open(b"other", &t, 0.0), Err(TokenError::BadMac));
    assert_eq!(binding::open(b"", &t, 0.0), Err(TokenError::Malformed));
    assert_eq!(binding::mint(b"", &b), None);
    let mut inf = b.clone();
    inf.expires_ms = f64::INFINITY;
    assert_eq!(binding::mint(SECRET, &inf), None);
    let none = Binding {
        chat_key: None,
        ..b
    };
    let t = binding::mint(SECRET, &none).unwrap();
    assert_eq!(binding::open(SECRET, &t, 0.0), Ok(none));
}

#[test]
fn malformed_tokens() {
    for t in [
        "", "v1.", "v1..", "v2.00.00", "v1.zz.00", "v1.00.0", "v1.7b7d", "x",
    ] {
        assert!(binding::open(SECRET, t, 0.0).is_err(), "{t}");
    }
    let long = format!("v1.{}", "0".repeat(binding::TOKEN_CAP));
    assert_eq!(
        binding::open(SECRET, &long, 0.0),
        Err(TokenError::Malformed)
    );
    // Upper-case hex is not the token's.
    let r = row("qa-own", "https://qa.example", false);
    let t = binding::mint(SECRET, &bound(&r)).unwrap();
    let upper = t.to_uppercase().replacen("V1.", "v1.", 1);
    assert!(binding::open(SECRET, &upper, 0.0).is_err());
    // A good MAC over a body that is not a binding.
    use hmac::Mac;
    let hex = |bytes: &[u8]| -> String { bytes.iter().map(|b| format!("{b:02x}")).collect() };
    let signed = format!("v1.{}", hex(br#"{"user":1}"#));
    let mut m = hmac::Hmac::<sha2::Sha256>::new_from_slice(SECRET).unwrap();
    m.update(signed.as_bytes());
    let token = format!("{signed}.{}", hex(&m.finalize().into_bytes()));
    assert_eq!(
        binding::open(b"qa-other-secret", &token, 0.0),
        Err(TokenError::BadMac)
    );
    assert_eq!(
        binding::open(SECRET, &token, 0.0),
        Err(TokenError::Malformed)
    );
}

#[test]
fn debug_output_carries_no_key_or_address_secret() {
    let base = "https://qa-user:qa-pass@qa.example/v1?key=qa-query-secret#qa-frag";
    let r = row("qa-own", base, false);
    let b = Binding {
        url: "https://qa-user:qa-pass@qa.example/v1/chat/completions?key=qa-query-secret#qa-frag"
            .into(),
        ..bound(&r)
    };
    let t = Target {
        url: b.url.clone(),
        ..target(KeyUse::RowKey, Fetch::Direct)
    };
    let req = plan::request(&t, Call::Round, Some("qa-row-key"));
    assert!(req.headers.iter().any(|(_, v)| v == "Bearer qa-row-key"));
    let now = Now {
        user: "qa-member",
        user_active: true,
        row: Some(&r),
        member_own: true,
        origin_approved: true,
    };
    let user = chat_upstream::target::User {
        id: Some("qa-member".into()),
        admin: false,
    };
    let turn = chat_upstream::target::Turn {
        row: &r,
        default_id: "default",
        user: Some(&user),
        member_origins: "https://qa.example",
        machine: None,
        sign_in: false,
        sign_in_enabled: false,
        connection: chat_upstream::target::Connection::Missing,
        workspace_user: None,
        chat_id: Some("qa-chat"),
        space_id: None,
    };
    for printed in [
        format!("{b:?}"),
        format!("{t:?}"),
        format!("{req:?}"),
        format!("{now:?}"),
        format!("{turn:?}"),
    ] {
        for secret in [
            "qa-user",
            "qa-key",
            "qa-row-key",
            "qa-pass",
            "qa-query-secret",
            "qa-frag",
        ] {
            assert!(!printed.contains(secret), "{secret} in {printed}");
        }
    }
    assert!(format!("{req:?}").contains("https://qa.example/v1/chat/completions"));
    // Without a scheme the userinfo would parse as an opaque path: not printed at all.
    for odd in [
        "qa-user:qa-pass@qa.example/v1/chat/completions",
        "qa-user:qa-pass@10.0.0.5:8080/v1",
        "mailto:qa-user:qa-pass@qa.example",
        "not a url qa-pass",
    ] {
        let printed = chat_upstream::target::printable_url(odd);
        assert!(!printed.contains("qa-pass"), "{printed}");
    }
    assert!(format!("{now:?}").contains("qa-own"));
}

#[test]
fn recheck_order() {
    let r = row("qa-own", "https://qa.example/v1", false);
    let b = bound(&r);
    let now = |user: &'static str, active, row: Option<&'static JValue>, member_own, ok| Now {
        user,
        user_active: active,
        row,
        member_own,
        origin_approved: ok,
    };
    let same: &'static JValue = Box::leak(Box::new(r.clone()));
    let moved: &'static JValue = Box::leak(Box::new(row(
        "qa-own",
        "https://qa-other.example/v1",
        false,
    )));
    let reshared: &'static JValue =
        Box::leak(Box::new(row("qa-own", "https://qa.example/v1", true)));
    let other: &'static JValue =
        Box::leak(Box::new(row("qa-else", "https://qa.example/v1", false)));
    // Same address, different spelling of the base: the fingerprint is of the stored base, so it ends the turn.
    let respelled: &'static JValue =
        Box::leak(Box::new(row("qa-own", "https://qa.example/v1/", false)));
    let mut rotated = r.clone();
    if let JValue::Obj(items) = &mut rotated {
        items[2].1 = JValue::Str("qa-new-key".into());
    }
    let rotated: &'static JValue = Box::leak(Box::new(rotated));

    assert_eq!(
        binding::recheck(&b, &now("qa-member", true, Some(same), true, true)),
        Recheck::Ok
    );
    assert_eq!(
        binding::recheck(&b, &now("qa-member", true, Some(rotated), true, true)),
        Recheck::Ok
    );
    assert_eq!(
        binding::recheck(&b, &now("qa-member", true, Some(same), false, false)),
        Recheck::Ok
    );
    assert_eq!(
        binding::recheck(&b, &now("qa-other", true, None, true, false)),
        Recheck::UserChanged
    );
    assert_eq!(
        binding::recheck(&b, &now("qa-member", false, Some(same), true, true)),
        Recheck::UserChanged
    );
    assert_eq!(
        binding::recheck(&b, &now("qa-member", true, None, true, false)),
        Recheck::RowGone
    );
    assert_eq!(
        binding::recheck(&b, &now("qa-member", true, Some(other), true, true)),
        Recheck::RowGone
    );
    assert_eq!(
        binding::recheck(&b, &now("qa-member", true, Some(moved), true, false)),
        Recheck::RowChanged
    );
    assert_eq!(
        binding::recheck(&b, &now("qa-member", true, Some(reshared), false, true)),
        Recheck::RowChanged
    );
    assert_eq!(
        binding::recheck(&b, &now("qa-member", true, Some(respelled), true, true)),
        Recheck::RowChanged
    );
    assert_eq!(
        binding::recheck(&b, &now("qa-member", true, Some(same), true, false)),
        Recheck::OriginRefused
    );
}

#[test]
fn fingerprint_separates_fields() {
    let a = binding::fingerprint(&row("ab", "c", false));
    assert_ne!(a, binding::fingerprint(&row("a", "bc", false)));
    assert_ne!(a, binding::fingerprint(&row("ab", "c", true)));
    let numeric = JValue::obj([
        ("id", JValue::Num(1.0)),
        ("baseUrl", JValue::Str("c".into())),
    ]);
    let text = JValue::obj([
        ("id", JValue::Str("1".into())),
        ("baseUrl", JValue::Str("c".into())),
    ]);
    assert_ne!(binding::fingerprint(&numeric), binding::fingerprint(&text));
    assert_eq!(a.len(), 64);
}

#[test]
fn pin_model() {
    let mut body = js_json::parse(r#"{"model":"qa-other","messages":[],"stream":true}"#).unwrap();
    binding::pin_model(&mut body, "qa-model");
    assert_eq!(
        js_json::stringify(&body).unwrap(),
        r#"{"model":"qa-model","messages":[],"stream":true}"#
    );
    let mut body = js_json::parse(r#"{"messages":[]}"#).unwrap();
    binding::pin_model(&mut body, "qa-model");
    assert_eq!(
        js_json::stringify(&body).unwrap(),
        r#"{"messages":[],"model":"qa-model"}"#
    );
    let mut arr = js_json::parse("[1]").unwrap();
    binding::pin_model(&mut arr, "qa-model");
    assert_eq!(js_json::stringify(&arr).unwrap(), "[1]");
}

fn target(key: KeyUse, fetch: Fetch) -> Target {
    Target {
        provider_id: "qa-own".into(),
        label: "QA own".into(),
        url: "https://qa.example/v1/chat/completions".into(),
        fetch,
        key,
        member_own: true,
    }
}

#[test]
fn plan_matches_engine_facts_headers() {
    for key in [
        None,
        Some(""),
        Some("local"),
        Some("LOCAL"),
        Some("qa-key"),
        Some(" local "),
    ] {
        let r = plan::request(&target(KeyUse::RowKey, Fetch::Direct), Call::Round, key);
        let row = JValue::obj([(
            "apiKey",
            key.map(|k| JValue::Str(k.into()))
                .unwrap_or(JValue::Undefined),
        )]);
        let JValue::Obj(want) = engine_facts::provider::headers(&row, &JValue::Undefined).unwrap()
        else {
            panic!("headers")
        };
        let want: Vec<(String, String)> = want
            .into_iter()
            .map(|(k, v)| (k, v.as_str().unwrap().to_string()))
            .collect();
        assert_eq!(r.headers, want, "{key:?}");
        assert_eq!(r.deadline_ms, None);
    }
    let m = plan::request(
        &target(KeyUse::None, Fetch::Machine { id: "qa-m1".into() }),
        Call::Fallback,
        Some("qa-key"),
    );
    assert_eq!(m.headers.len(), 1);
    assert_eq!(m.deadline_ms, Some(300_000));
    assert_eq!(Call::Summary.deadline_ms(), Some(180_000));
    // The key never shows in a debug print.
    let r = plan::request(
        &target(KeyUse::RowKey, Fetch::Direct),
        Call::Round,
        Some("qa-secret-key"),
    );
    assert!(!format!("{r:?}").contains("qa-secret-key"));
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 256, ..ProptestConfig::default() })]

    #[test]
    fn any_binding_round_trips(user in ".{0,40}", id in ".{0,40}", url in ".{0,80}", model in ".{0,40}",
                               chat in proptest::option::of(".{0,40}"), exp in -1.0e15f64..1.0e15) {
        let b = Binding { user, provider_id: id, url, fingerprint: "f".repeat(64), model, chat_key: chat, expires_ms: exp };
        let t = binding::mint(SECRET, &b).unwrap();
        prop_assert_eq!(binding::open(SECRET, &t, exp - 1.0), Ok(b.clone()));
        prop_assert_eq!(binding::open(SECRET, &t, exp), Err(TokenError::Expired));
    }

    #[test]
    fn any_single_change_is_refused(pos in 0usize..10_000, ch in prop::sample::select(vec!['0', '7', 'a', 'f', '.', 'g'])) {
        let r = row("qa-own", "https://qa.example/v1", false);
        let t = binding::mint(SECRET, &bound(&r)).unwrap();
        let pos = pos % t.len();
        let mut s: Vec<char> = t.chars().collect();
        prop_assume!(s[pos] != ch);
        s[pos] = ch;
        let changed: String = s.into_iter().collect();
        prop_assert!(binding::open(SECRET, &changed, 0.0).is_err());
    }

    #[test]
    fn open_never_panics(s in ".{0,300}") {
        let _ = binding::open(SECRET, &s, 0.0);
        let _ = binding::open(SECRET, &format!("v1.{s}"), 0.0);
    }
}

mod relay {
    use super::*;
    use chat_upstream::relay::{self, Facts, Refusal};

    fn facts<'a>(row: Option<&'a JValue>, user: &'a str, approved: bool) -> Facts<'a> {
        Facts {
            secret: SECRET,
            now_ms: 1_000.0,
            now: Now {
                user,
                user_active: true,
                row,
                member_own: true,
                origin_approved: approved,
            },
            key: KeyUse::RowKey,
            row_key: Some("qa-key"),
            call: Call::Round,
        }
    }

    fn raw(token: &str, body: &str) -> Vec<u8> {
        format!(
            r#"{{"turnToken":{},"body":{body}}}"#,
            js_json::stringify(&JValue::Str(token.into())).unwrap()
        )
        .into_bytes()
    }

    #[test]
    fn admits_and_pins() {
        let r = row("qa-own", "https://qa.example/v1", false);
        let t = binding::mint(SECRET, &bound(&r)).unwrap();
        let a = relay::admit(
            &raw(&t, r#"{"model":"qa-elsewhere","stream":true}"#),
            &facts(Some(&r), "qa-member", true),
        )
        .unwrap();
        assert_eq!(
            js_json::stringify(&a.body).unwrap(),
            r#"{"model":"qa-model","stream":true}"#
        );
        assert_eq!(a.request.url, "https://qa.example/v1/chat/completions");
        assert_eq!(
            a.request.headers[1],
            ("Authorization".to_string(), "Bearer qa-key".to_string())
        );
    }

    #[test]
    fn refusals_in_order() {
        let r = row("qa-own", "https://qa.example/v1", false);
        let moved = row("qa-own", "https://qa-other.example/v1", false);
        let t = binding::mint(SECRET, &bound(&r)).unwrap();
        let f = facts(Some(&r), "qa-member", true);
        assert_eq!(relay::admit(b"{", &f).unwrap_err(), Refusal::BadRequest);
        assert_eq!(
            relay::admit(&raw(&t, "[]"), &f).unwrap_err(),
            Refusal::BadRequest
        );
        assert_eq!(
            relay::admit(br#"{"turnToken":1,"body":{}}"#, &f).unwrap_err(),
            Refusal::BadRequest
        );
        assert_eq!(
            relay::admit(&raw("v1.00.00", "{}"), &f).unwrap_err(),
            Refusal::BadToken
        );
        let late = Facts {
            now_ms: 2_000.0,
            ..facts(Some(&r), "qa-member", true)
        };
        assert_eq!(
            relay::admit(&raw(&t, "{}"), &late).unwrap_err(),
            Refusal::Expired
        );
        assert_eq!(
            relay::admit(&raw(&t, "{}"), &facts(Some(&r), "qa-other", true)).unwrap_err(),
            Refusal::UserChanged
        );
        assert_eq!(
            relay::admit(&raw(&t, "{}"), &facts(None, "qa-member", true)).unwrap_err(),
            Refusal::RowChanged
        );
        assert_eq!(
            relay::admit(&raw(&t, "{}"), &facts(Some(&moved), "qa-member", true)).unwrap_err(),
            Refusal::RowChanged
        );
        assert_eq!(
            relay::admit(&raw(&t, "{}"), &facts(Some(&r), "qa-member", false)).unwrap_err(),
            Refusal::OriginRefused
        );
        for (r, s) in [
            (Refusal::BadRequest, 400),
            (Refusal::BadToken, 401),
            (Refusal::Expired, 410),
            (Refusal::UserChanged, 403),
            (Refusal::RowChanged, 409),
            (Refusal::OriginRefused, 403),
        ] {
            assert_eq!(r.status(), s);
            assert!(!r.code().is_empty());
        }
    }

    #[test]
    fn transport_errors() {
        let a = relay::transport_error("connect ECONNREFUSED");
        assert_eq!(
            relay::read_transport_error(&a),
            Some("connect ECONNREFUSED")
        );
        assert_eq!(
            relay::read_transport_error(&JValue::obj([("x", JValue::Null)])),
            None
        );
        assert_eq!(
            chat_upstream::target::unreachable_text("QA own", "connect ECONNREFUSED"),
            "QA own unreachable: connect ECONNREFUSED"
        );
    }
}
