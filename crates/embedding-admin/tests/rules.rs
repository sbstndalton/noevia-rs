//! The request and answer rules of noevia-core's `routes/embedding.cjs`, on synthetic bodies. Each
//! expected message or status is quoted from the Node code it mirrors (the comments name it).
#![allow(clippy::unwrap_used, clippy::indexing_slicing)]

use embedding_admin::{
    aggregate_status, js_string, js_truthy, read_request, served_probes, Action, Reply, Scope,
};
use serde_json::json;

const ACTION_LIST: &str =
    "action must be one of set-next, clear-next, switch, rollback, finish, service-switch, service-off";

fn refusal(status: u16, error: &str) -> Reply {
    Reply {
        status,
        error: error.to_string(),
    }
}

#[test]
fn a_body_must_be_a_json_object() {
    // `!body || typeof body !== 'object' || Array.isArray(body)`
    for body in [
        json!(null),
        json!([]),
        json!("switch"),
        json!(3),
        json!(true),
    ] {
        assert_eq!(
            read_request(&body),
            Err(refusal(400, "A JSON object is required")),
            "{body}"
        );
    }
}

#[test]
fn the_action_is_matched_after_js_string_coercion() {
    // `String(body.action || '')`: a missing, empty or false action is '' (not an action).
    for body in [
        json!({}),
        json!({"action": ""}),
        json!({"action": false}),
        json!({"action": "SWITCH"}),
        json!({"action": 7}),
    ] {
        assert_eq!(
            read_request(&body),
            Err(refusal(400, ACTION_LIST)),
            "{body}"
        );
    }
    // An array of one name coerces to that name in JavaScript (`String(["switch"])`).
    let request = read_request(&json!({"action": ["switch"]})).unwrap();
    assert_eq!(request.action, Action::Switch);
}

#[test]
fn service_actions_never_read_the_scope() {
    // SERVICE_ACTIONS return before the scope check.
    let request = read_request(&json!({"action": "service-off", "scope": "bogus"})).unwrap();
    assert_eq!(request.action, Action::ServiceOff);
    assert_eq!(request.scope, Scope::All);
    assert!(request.target.is_none());
    let request = read_request(&json!({"action": "service-switch", "model": "m"})).unwrap();
    assert_eq!(request.action, Action::ServiceSwitch);
}

#[test]
fn a_bad_scope_is_refused_but_a_falsy_one_is_all() {
    assert_eq!(
        read_request(&json!({"action": "finish", "scope": "bogus"})),
        Err(refusal(400, "scope must be all, projects or diary"))
    );
    // `String(body.scope || 'all')`: false, 0, '' and null all mean 'all'.
    for scope in [json!(false), json!(0), json!(""), json!(null)] {
        let request = read_request(&json!({"action": "finish", "scope": scope})).unwrap();
        assert_eq!(request.scope, Scope::All);
    }
    assert_eq!(
        read_request(&json!({"action": "finish", "scope": "diary"}))
            .unwrap()
            .scope,
        Scope::Diary
    );
}

#[test]
fn set_next_needs_an_object_or_array_target() {
    // `!body.target || typeof body.target !== 'object'`; an array passes (typeof is 'object').
    let message = "set-next needs a target { baseUrl, model, profile }";
    for target in [json!(null), json!("http://a"), json!(0), json!(true)] {
        assert_eq!(
            read_request(&json!({"action": "set-next", "target": target})),
            Err(refusal(400, message))
        );
    }
    assert_eq!(
        read_request(&json!({"action": "set-next"})),
        Err(refusal(400, message))
    );
    let request = read_request(&json!({"action": "set-next", "target": []})).unwrap();
    assert_eq!(request.target.unwrap().model, "");
}

#[test]
fn set_next_reads_the_address_model_and_the_diary_fallback() {
    let request = read_request(&json!({
        "action": "set-next",
        "target": {"baseUrl": " http://embed.local:8080 ", "model": " nomic ", "profile": "p"}
    }))
    .unwrap();
    let target = request.target.unwrap();
    assert_eq!(target.model, "nomic");
    assert_eq!(target.base_url.as_deref(), Some("http://embed.local:8080"));
    // `diaryBaseUrl ?? baseUrl`: an absent or null Diary address falls back to the projects one.
    assert_eq!(
        target.diary_base_url.as_deref(),
        Some("http://embed.local:8080")
    );

    let request = read_request(&json!({
        "action": "set-next",
        "target": {"baseUrl": "http://a", "diaryBaseUrl": null}
    }))
    .unwrap();
    assert_eq!(
        request.target.unwrap().diary_base_url.as_deref(),
        Some("http://a")
    );
}

#[test]
fn an_empty_diary_address_does_not_fall_back() {
    // `??` falls back on null and undefined only: '' stays, and is then empty (the engine).
    let request = read_request(&json!({
        "action": "set-next",
        "target": {"baseUrl": "http://a", "diaryBaseUrl": ""}
    }))
    .unwrap();
    let target = request.target.clone().unwrap();
    assert_eq!(target.base_url.as_deref(), Some("http://a"));
    assert_eq!(target.diary_base_url, None);
    // With scope all, the Diary's server is unnamed: refused before any probe.
    assert_eq!(
        served_probes(&request),
        Err(refusal(
            409,
            "Name the Diary's embeddings server so noevia can check it serves this model."
        ))
    );
}

#[test]
fn an_empty_projects_address_is_the_engine() {
    let request = read_request(&json!({
        "action": "set-next",
        "scope": "projects",
        "target": {"baseUrl": "  ", "model": "m"}
    }))
    .unwrap();
    assert_eq!(request.target.as_ref().unwrap().base_url, None);
    assert_eq!(served_probes(&request), Ok(vec![None]));
}

#[test]
fn probes_follow_the_scope_and_are_deduplicated() {
    let probes = |scope: &str, base: &str, diary: &str| {
        let request = read_request(&json!({
            "action": "set-next",
            "scope": scope,
            "target": {"baseUrl": base, "diaryBaseUrl": diary, "model": "m"}
        }))
        .unwrap();
        served_probes(&request)
    };
    assert_eq!(
        probes("all", "http://a", "http://b"),
        Ok(vec![Some("http://a".into()), Some("http://b".into())])
    );
    assert_eq!(
        probes("all", "http://a", "http://a"),
        Ok(vec![Some("http://a".into())])
    );
    assert_eq!(
        probes("diary", "http://a", "http://b"),
        Ok(vec![Some("http://b".into())])
    );
    assert_eq!(
        probes("projects", "http://a", "http://b"),
        Ok(vec![Some("http://a".into())])
    );
    // The engine (no projects address) and a named Diary server are two different probes.
    assert_eq!(
        probes("all", "", "http://b"),
        Ok(vec![None, Some("http://b".into())])
    );
}

#[test]
fn only_set_next_has_a_target_and_probes() {
    let request =
        read_request(&json!({"action": "switch", "target": {"baseUrl": "http://a"}})).unwrap();
    assert!(request.target.is_none());
    assert_eq!(served_probes(&request), Ok(vec![]));
}

#[test]
fn force_is_only_true_when_it_is_the_boolean_true() {
    // `body.force === true`
    for (force, expected) in [
        (json!(true), true),
        (json!("true"), false),
        (json!(1), false),
    ] {
        let request = read_request(&json!({"action": "switch", "force": force})).unwrap();
        assert_eq!(request.force, expected);
    }
}

#[test]
fn a_status_is_200_when_no_half_failed() {
    assert_eq!(aggregate_status(Some(&json!({})), Some(&json!({}))), 200);
    // `error` must be truthy: an empty string is no error.
    assert_eq!(aggregate_status(Some(&json!({"error": ""})), None), 200);
    assert_eq!(aggregate_status(None, None), 200);
}

#[test]
fn a_status_is_the_failed_halfs_own_when_every_half_failed() {
    let failed = json!({"error": "no", "status": 404});
    assert_eq!(aggregate_status(Some(&failed), None), 404);
    assert_eq!(aggregate_status(Some(&failed), Some(&failed)), 404);
    // The first failed half decides: the projects one.
    let other = json!({"error": "no", "status": 409});
    assert_eq!(aggregate_status(Some(&failed), Some(&other)), 404);
    assert_eq!(aggregate_status(None, Some(&other)), 409);
}

#[test]
fn a_failed_half_status_outside_400_to_599_is_502() {
    for half in [
        json!({"error": "no"}),
        json!({"error": "no", "status": 200}),
        json!({"error": "no", "status": 600}),
        json!({"error": "no", "status": "409"}),
    ] {
        assert_eq!(aggregate_status(Some(&half), None), 502, "{half}");
    }
}

#[test]
fn a_status_is_207_when_only_some_halves_failed() {
    let failed = json!({"error": "no", "status": 500});
    assert_eq!(aggregate_status(Some(&failed), Some(&json!({}))), 207);
    assert_eq!(aggregate_status(Some(&json!({})), Some(&failed)), 207);
}

#[test]
fn js_truthiness_matches_the_values_a_body_can_hold() {
    for falsy in [
        None,
        Some(json!(null)),
        Some(json!(false)),
        Some(json!(0)),
        Some(json!("")),
    ] {
        assert!(!js_truthy(falsy.as_ref()), "{falsy:?}");
    }
    for truthy in [
        Some(json!(true)),
        Some(json!(1)),
        Some(json!("0")),
        Some(json!([])),
        Some(json!({})),
    ] {
        assert!(js_truthy(truthy.as_ref()), "{truthy:?}");
    }
}

#[test]
fn js_string_matches_string_of_the_values_a_body_can_hold() {
    assert_eq!(js_string(None), "");
    assert_eq!(js_string(Some(&json!(null))), "");
    assert_eq!(js_string(Some(&json!(1.0))), "1");
    assert_eq!(js_string(Some(&json!(2.5))), "2.5");
    assert_eq!(js_string(Some(&json!(["a", null, "b"]))), "a,,b");
    assert_eq!(js_string(Some(&json!({"a": 1}))), "[object Object]");
}
