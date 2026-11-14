//! The session cache and the refresh-once rule, case by case (core's sign-in provider module, `session`
//! and `authorised`; the full fetch path is replayed against core in node_replay.rs).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use js_json::JValue;
use signin_transport::request::{Refusal, Session};
use signin_transport::session::{
    broker_refusal, Authorise, BrokerAnswer, SessionCache, Step, REFRESH_MARGIN_MS,
    SESSION_CACHE_MAX, SESSION_CACHE_MS,
};
use signin_transport::texts::Texts;

fn texts() -> Texts {
    Texts {
        label: "Service".into(),
        disconnected: "disconnected".into(),
        reconnect: "reconnect".into(),
        transient: "transient".into(),
        not_included: "not included".into(),
        source: "service".into(),
    }
}

fn session(token: &str, expires_at: Option<f64>) -> Session {
    Session {
        access_token: token.into(),
        account_id: "acct".into(),
        restricted_region: false,
        expires_at,
        generation: JValue::Num(1.0),
    }
}

const NOW: f64 = 1_000_000.0;

#[test]
fn cached_for_a_minute_with_the_same_signal() {
    let mut c = SessionCache::default();
    c.store("u", &session("t", None), Some("s1"), NOW);
    assert_eq!(
        c.lookup("u", Some("s1"), false, NOW + SESSION_CACHE_MS - 1.0)
            .unwrap()
            .access_token,
        "t"
    );
    // Expired: dropped.
    assert!(c
        .lookup("u", Some("s1"), false, NOW + SESSION_CACHE_MS)
        .is_none());
    assert!(c.is_empty());
}

#[test]
fn signal_change_rejected_token_or_unreadable_signal_miss_and_drop() {
    for (signal, rejected) in [(Some("s2"), false), (Some("s1"), true), (None, false)] {
        let mut c = SessionCache::default();
        c.store("u", &session("t", None), Some("s1"), NOW);
        assert!(c.lookup("u", signal, rejected, NOW).is_none());
        assert!(c.is_empty(), "{signal:?} {rejected}");
    }
}

#[test]
fn never_inside_the_refresh_margin_and_never_without_a_signal() {
    let mut c = SessionCache::default();
    // Expires within the margin: not kept at all.
    c.store(
        "u",
        &session("t", Some(NOW + REFRESH_MARGIN_MS)),
        Some("s"),
        NOW,
    );
    assert!(c.is_empty());
    // Expires a little after the margin: kept only until the margin begins.
    c.store(
        "u",
        &session("t", Some(NOW + REFRESH_MARGIN_MS + 10.0)),
        Some("s"),
        NOW,
    );
    assert!(c.lookup("u", Some("s"), false, NOW + 9.0).is_some());
    assert!(c.lookup("u", Some("s"), false, NOW + 10.0).is_none());
    // An unreadable signal: never stored.
    c.store("u", &session("t", None), None, NOW);
    assert!(c.is_empty());
    // A non-positive or missing expiry means "no expiry known": the minute applies.
    c.store("u", &session("t", Some(0.0)), Some("s"), NOW);
    assert!(c
        .lookup("u", Some("s"), false, NOW + SESSION_CACHE_MS - 1.0)
        .is_some());
}

#[test]
fn oldest_user_goes_first_past_the_cap() {
    let mut c = SessionCache::default();
    for i in 0..=SESSION_CACHE_MAX {
        c.store(&format!("u{i}"), &session("t", None), Some("s"), NOW);
    }
    assert_eq!(c.len(), SESSION_CACHE_MAX);
    assert!(c.lookup("u0", Some("s"), false, NOW).is_none());
    assert!(c
        .lookup(&format!("u{SESSION_CACHE_MAX}"), Some("s"), false, NOW)
        .is_some());
}

#[test]
fn refresh_once_then_reject_the_sign_in() {
    let t = texts();
    let mut a = Authorise::new();
    assert_eq!(
        a.start(),
        Step::AskBroker {
            rejected_token: None
        }
    );
    assert!(
        matches!(a.on_session(&t, BrokerAnswer::Ok(session("t1", None))), Step::Send(s) if s.access_token == "t1")
    );
    assert_eq!(
        a.on_status(&t, 401),
        Step::AskBroker {
            rejected_token: Some("t1".into())
        }
    );
    let mut second = session("t2", None);
    second.generation = JValue::Num(7.0);
    assert!(
        matches!(a.on_session(&t, BrokerAnswer::Ok(second)), Step::Send(s) if s.access_token == "t2")
    );
    assert_eq!(
        a.on_status(&t, 401),
        Step::Reject {
            generation: JValue::Num(7.0),
            then: Refusal {
                status: 401,
                message: "reconnect".into(),
                code: Some("reconnect".into())
            }
        }
    );
}

#[test]
fn any_other_status_passes() {
    let t = texts();
    for status in [200, 400, 403, 429, 500] {
        let mut a = Authorise::new();
        a.on_session(&t, BrokerAnswer::Ok(session("t", None)));
        assert_eq!(a.on_status(&t, status), Step::Pass);
    }
}

#[test]
fn broker_answers_as_core_errors() {
    let t = texts();
    assert_eq!(
        broker_refusal(&t, &BrokerAnswer::Unreachable),
        Some(Refusal {
            status: 503,
            message: "transient".into(),
            code: Some("transient".into())
        })
    );
    let r = |error: Option<&str>, status: Option<f64>, code: Option<&str>| BrokerAnswer::Refused {
        error: error.map(str::to_string),
        status,
        code: code.map(str::to_string),
    };
    assert_eq!(
        broker_refusal(&t, &r(Some("gone"), Some(409.0), Some("disconnected"))),
        Some(Refusal {
            status: 409,
            message: "gone".into(),
            code: Some("disconnected".into())
        })
    );
    for odd in [Some(200.0), Some(600.0), Some(404.5), None] {
        assert_eq!(
            broker_refusal(&t, &r(None, odd, None)).unwrap().status,
            503,
            "{odd:?}"
        );
    }
    assert_eq!(
        broker_refusal(&t, &r(Some(&"x".repeat(400)), None, None))
            .unwrap()
            .message
            .len(),
        300
    );
    let mut a = Authorise::new();
    assert_eq!(
        a.on_session(&t, BrokerAnswer::Unreachable),
        Step::Refuse(Refusal {
            status: 503,
            message: "transient".into(),
            code: Some("transient".into())
        })
    );
}

#[test]
fn session_debug_never_prints_the_token() {
    let printed = format!("{:?}", session("secret-token-value", None));
    assert!(!printed.contains("secret-token-value"), "{printed}");
}

#[test]
fn step_and_authorise_debug_never_print_the_token_or_the_account() {
    let t = texts();
    let qa = Session {
        account_id: "qa-account-77".into(),
        ..session("qa-access-token", None)
    };
    let mut a = Authorise::new();
    let send = a.on_session(&t, BrokerAnswer::Ok(qa.clone()));
    // Holding the session it is about to send.
    let holding = format!("{a:?}");
    let answer = format!("{:?}", BrokerAnswer::Ok(qa.clone()));
    let again = a.on_status(&t, 401);
    assert_eq!(
        again,
        Step::AskBroker {
            rejected_token: Some("qa-access-token".into())
        }
    );
    let mut cache = SessionCache::default();
    cache.store("qa-user", &qa, Some("s1"), 0.0);
    for printed in [
        format!("{send:?}"),
        format!("{again:?}"),
        holding,
        answer,
        format!("{a:?}"),
        format!("{cache:?}"),
        format!("{qa:?}"),
    ] {
        assert!(!printed.contains("qa-access-token"), "{printed}");
        assert!(!printed.contains("qa-account-77"), "{printed}");
    }
    assert!(format!("{again:?}").contains("<redacted>"));
}
