//! engine-admission machine_turn + machine_health against core chat-machines.cjs / machine-health.cjs behaviour
//! (their code and the #1840/#1849/#1851/#1854/#1856/#1861 tests), over a scripted fetch: no network.
#![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
use engine_admission::machine_health::{
    health_url, machine_failing, machine_reason, machine_state, probe_result, BodyError, Clock,
    Fetch, FetchError, HealthCache, Probe, ProbeOptions, Reply, State,
};
use engine_admission::machine_turn::{
    address_of, effective_roles, encode_uri_component, local_roles, machine_models, manual_target,
    model_ids, not_sent, pick, props_facts_of, provider_row, ready_of, remote_roles,
    unreachable_text, wrong_model_text, MachineFacts, MachineTurns, Pick, Ready, TurnFacts,
    CONTEXT_FALLBACK,
};
use engine_admission::machines::Target;
use js_json::JValue;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

fn j(text: &str) -> JValue {
    js_json::parse(text).unwrap()
}

fn target(role: &str, model: &str) -> Target {
    Target {
        role: role.into(),
        machine_id: "m1".into(),
        machine_name: "Synthetic Pi".into(),
        port: 8080,
        model: model.into(),
        base_url: "http://pi.lan:8080".into(),
    }
}

/// A scripted fetch: an answer per URL (absent: `Network`), every call logged, an optional gate to hold answers.
#[derive(Default)]
struct Script {
    answers: Mutex<HashMap<String, Result<Reply, FetchError>>>,
    calls: Mutex<Vec<(String, usize, Option<usize>)>>,
    gate: Option<Arc<tokio::sync::Notify>>,
}

impl Script {
    fn answer(&self, url: &str, a: Result<Reply, FetchError>) {
        self.answers.lock().unwrap().insert(url.into(), a);
    }
    fn calls(&self) -> Vec<String> {
        self.calls
            .lock()
            .unwrap()
            .iter()
            .map(|c| c.0.clone())
            .collect()
    }
}

fn status(s: u16) -> Result<Reply, FetchError> {
    Ok(Reply {
        status: s,
        body: None,
    })
}

fn body(s: u16, text: &str) -> Result<Reply, FetchError> {
    Ok(Reply {
        status: s,
        body: Some(Ok(text.into())),
    })
}

impl Fetch for Script {
    async fn get(
        &self,
        url: &str,
        headers: &[(String, String)],
        _timeout: Duration,
        body_cap: Option<usize>,
    ) -> Result<Reply, FetchError> {
        self.calls
            .lock()
            .unwrap()
            .push((url.into(), headers.len(), body_cap));
        if let Some(g) = &self.gate {
            g.notified().await;
        }
        let a = self.answers.lock().unwrap().get(url).cloned();
        let mut a = a.unwrap_or(Err(FetchError::Network));
        // A probe reads no body.
        if body_cap.is_none() {
            if let Ok(r) = &mut a {
                r.body = None;
            }
        }
        a
    }
}

fn clock() -> (Arc<AtomicU64>, Clock) {
    let at = Arc::new(AtomicU64::new(1_000_000));
    let read = at.clone();
    (at, Arc::new(move || read.load(Ordering::SeqCst)))
}

fn cache(script: Arc<Script>, max: usize) -> (Arc<AtomicU64>, HealthCache<Arc<Script>>) {
    let (at, now) = clock();
    (
        at,
        HealthCache::with(script, 30_000, Duration::from_millis(2500), max, now),
    )
}

// ---- pure: props, models, texts, row ----

#[test]
fn props_facts_read_as_props_facts_of() {
    let f = props_facts_of(&j(
        r#"{"default_generation_settings":{"n_ctx":32768},"chat_template":"{% if enable_thinking %}x{% endif %}","modalities":{"vision":true}}"#,
    ));
    assert_eq!(
        (f.context_tokens, f.thinking, f.vision),
        (Some(32768), Some(true), Some(true))
    );
    // Number(): a numeric string counts; out of 2048..=2000000 or fractional does not.
    for (n_ctx, want) in [
        ("\"8192\"", Some(8192)),
        ("\" 0x2000 \"", Some(8192)),
        ("2048", Some(2048)),
        ("2047", None),
        ("2000000", Some(2_000_000)),
        ("2000001", None),
        ("4096.5", None),
        ("null", None),
        ("[4096]", Some(4096)),
        ("{}", None),
        ("true", None),
    ] {
        let f = props_facts_of(&j(&format!(
            r#"{{"default_generation_settings":{{"n_ctx":{n_ctx}}}}}"#
        )));
        assert_eq!(f.context_tokens, want, "{n_ctx}");
    }
    // /\benable_thinking\b/ on a non-empty template; no template says nothing.
    for (tpl, want) in [
        ("\"enable_thinking\"", Some(true)),
        ("\"x enable_thinking=1\"", Some(true)),
        ("\"my_enable_thinking\"", Some(false)),
        ("\"enable_thinking2\"", Some(false)),
        ("\"é-enable_thinking-é\"", Some(true)),
        ("\"\"", None),
        ("7", None),
    ] {
        assert_eq!(
            props_facts_of(&j(&format!(r#"{{"chat_template":{tpl}}}"#))).thinking,
            want,
            "{tpl}"
        );
    }
    for (m, want) in [
        (r#"{"vision":false}"#, Some(false)),
        (r#"{"vision":"yes"}"#, None),
        ("null", None),
        ("{}", None),
    ] {
        assert_eq!(
            props_facts_of(&j(&format!(r#"{{"modalities":{m}}}"#))).vision,
            want,
            "{m}"
        );
    }
    assert_eq!(props_facts_of(&JValue::Null).context_tokens, None);
}

#[test]
fn model_ids_as_core_model_ids() {
    let ids = model_ids(&j(
        r#"{"data":[{"id":"a"},"b",{"id":"a"},{"id":" "},{"id":"c\u0001"},{"id":null,"model":"d"},{"id":5,"model":"e"},{"name":"f"},null,{"id":"g\u007f"}]}"#,
    ));
    assert_eq!(ids, ["a", "b", "d", "f"]);
    // `data` must be an array to win; else `models`.
    assert_eq!(
        model_ids(&j(r#"{"data":{},"models":[{"name":"x"}]}"#)),
        ["x"]
    );
    assert!(model_ids(&j(r#"{"data":"no"}"#)).is_empty());
    assert!(model_ids(&JValue::Null).is_empty());
    // 200 UTF-16 units at most (an astral character counts two).
    let long = "x".repeat(200);
    let over = format!("{}😀", "x".repeat(199));
    assert_eq!(
        model_ids(&j(&format!(r#"{{"data":["{long}","{over}"]}}"#))),
        [long]
    );
    // At most 200 models.
    let many: Vec<String> = (0..250).map(|i| format!("\"m{i}\"")).collect();
    assert_eq!(
        model_ids(&j(&format!(r#"{{"data":[{}]}}"#, many.join(",")))).len(),
        200
    );
}

#[test]
fn texts_say_core_s_words() {
    let t = target("smart", "synthetic-remote");
    assert_eq!(
        unreachable_text(&t, ""),
        "Smart model\u{2019}s machine (Synthetic Pi) is unreachable. Check that it is on and serving synthetic-remote on port 8080, or move the Smart role back to this machine in Settings \u{2192} Inference machines."
    );
    assert!(unreachable_text(&t, "no answer in time")
        .contains("is unreachable: no answer in time. Check"));
    let served = |n: usize| (0..n).map(|i| format!("s{i}")).collect::<Vec<_>>();
    assert_eq!(
        wrong_model_text(&t, &served(1)),
        "The Smart machine (Synthetic Pi) serves s0, not synthetic-remote. Start synthetic-remote on port 8080, or choose the model it serves for the Smart role in Settings \u{2192} Inference machines."
    );
    assert!(wrong_model_text(&t, &served(3)).contains("serves s0, s1, s2, not"));
    assert!(wrong_model_text(&t, &served(4)).contains("serves s0, s1, s2 and more, not"));
    assert!(wrong_model_text(&t, &[]).contains("serves no model, not"));
    use engine_admission::machine_turn::{connect_error, ConnectWhy};
    assert!(connect_error(&t, ConnectWhy::Timeout).contains("unreachable: no answer in time."));
    assert!(connect_error(&t, ConnectWhy::Redirect)
        .contains("unreachable: it answered with a redirect, which is never followed."));
    assert!(connect_error(&t, ConnectWhy::NotOnThisNetwork)
        .contains("unreachable: its address is not on this network."));
    assert!(connect_error(&t, ConnectWhy::Other).contains("unreachable. Check"));
}

#[test]
fn the_provider_row_is_core_s_row() {
    let t = target("code", "synthetic-code");
    assert_eq!(
        js_json::stringify(&provider_row(&t, Some(16384), Some(true))).unwrap(),
        r#"{"id":"machine:m1","label":"Code model’s machine (Synthetic Pi)","baseUrl":"http://pi.lan:8080/v1","apiKey":"","shared":true,"contextTokens":16384,"machine":{"id":"m1","name":"Synthetic Pi","role":"code","port":8080,"model":"synthetic-code","thinkingSwitch":true}}"#
    );
    // No context, a thinking switch that is not true: neither key.
    assert_eq!(
        js_json::stringify(&provider_row(&t, None, Some(false))).unwrap(),
        r#"{"id":"machine:m1","label":"Code model’s machine (Synthetic Pi)","baseUrl":"http://pi.lan:8080/v1","apiKey":"","shared":true,"machine":{"id":"m1","name":"Synthetic Pi","role":"code","port":8080,"model":"synthetic-code"}}"#
    );
    assert_eq!(address_of(&t), "http://pi.lan");
}

#[test]
fn encode_uri_component_as_js() {
    assert_eq!(
        encode_uri_component("a b/c?d=é~*'()!-_."),
        "a%20b%2Fc%3Fd%3D%C3%A9~*'()!-_."
    );
}

// ---- roles, Manual pick, the turn's machine ----

const ROW: &str = r#"{"version":1,"machines":[{"id":"m1","name":"Synthetic Pi","address":"http://pi.lan"},{"id":"m2","name":"Rock","address":"http://rock.lan"}],"assignments":{"chatSmart":{"machine":"m1","port":8080,"model":"remote-a"},"chatCode":{"machine":"m1","port":8080,"model":"remote-a"},"vision":{"machine":"m2","port":9000,"model":"remote-v"}}}"#;

#[test]
fn roles_as_a_turn_sees_them() {
    let remote = remote_roles(Some(ROW));
    assert_eq!(
        remote.iter().map(|t| t.role.as_str()).collect::<Vec<_>>(),
        ["smart", "code", "vision"]
    );
    let roles = j(r#"{"fast":"local-f","smart":"local-s","creative":"local-c"}"#);
    assert_eq!(
        js_json::stringify(&effective_roles(&roles, &remote)).unwrap(),
        r#"{"fast":"local-f","smart":"remote-a","creative":"local-c","code":"remote-a","vision":"remote-v"}"#
    );
    assert_eq!(
        js_json::stringify(&local_roles(&roles, &remote)).unwrap(),
        r#"{"fast":"local-f","creative":"local-c"}"#
    );
    // Nothing on a machine, or no roles: unchanged.
    assert_eq!(effective_roles(&roles, &[]), roles);
    assert_eq!(effective_roles(&JValue::Null, &remote), JValue::Null);
    let rows = machine_models(&remote);
    assert_eq!(rows.len(), 2);
    assert_eq!(
        (rows[0].model.as_str(), rows[0].roles.clone()),
        ("remote-a", vec!["smart".to_owned(), "code".to_owned()])
    );
    assert_eq!(
        (rows[1].machine_name.as_str(), rows[1].roles.clone()),
        ("Rock", vec!["vision".to_owned()])
    );
}

#[tokio::test]
async fn a_manual_pick_runs_on_a_machine_only_when_this_machine_does_not_list_it() {
    let remote = remote_roles(Some(ROW));
    let reads = AtomicU64::new(0);
    let catalogue = |rows: &'static str| {
        reads.fetch_add(1, Ordering::SeqCst);
        async move { Ok::<_, ()>(j(rows)) }
    };
    let got = manual_target("remote-a", &remote, || catalogue(r#"[{"name":"local-s"}]"#)).await;
    assert_eq!(
        got.map(|t| t.role),
        Some("smart".into()),
        "the first role in role order"
    );
    assert_eq!(
        manual_target("remote-a", &remote, || catalogue(
            r#"[{"name":"remote-a"}]"#
        ))
        .await,
        None,
        "listed here: stays"
    );
    assert!(
        manual_target("remote-a", &remote, || catalogue("null"))
            .await
            .is_some(),
        "not a list: the machine"
    );
    assert_eq!(reads.load(Ordering::SeqCst), 3);
    // No machine serves the name, or no name: the catalogue is never read.
    assert_eq!(
        manual_target("local-s", &remote, || catalogue("[]")).await,
        None
    );
    assert_eq!(manual_target("", &remote, || catalogue("[]")).await, None);
    assert_eq!(reads.load(Ordering::SeqCst), 3);
    // An unreadable catalogue cannot say the pick is not here (#1855).
    assert_eq!(
        manual_target("remote-a", &remote, || async { Err::<JValue, ()>(()) }).await,
        None
    );
}

#[test]
fn which_machine_a_turn_goes_to() {
    let auto = |role: Option<&'static str>| TurnFacts {
        automatic: true,
        route_row: false,
        default_provider: true,
        routed_role: role,
        model: "",
    };
    match pick(&auto(Some("smart")), Some(ROW)) {
        Pick::Role(t) => assert_eq!(
            (t.machine_id.as_str(), t.model.as_str()),
            ("m1", "remote-a")
        ),
        other => panic!("{other:?}"),
    }
    assert_eq!(pick(&auto(Some("fast")), Some(ROW)), Pick::Here);
    assert_eq!(pick(&auto(None), Some(ROW)), Pick::Here);
    assert_eq!(pick(&auto(Some("nope")), Some(ROW)), Pick::Here);
    assert_eq!(
        pick(
            &TurnFacts {
                route_row: true,
                ..auto(Some("smart"))
            },
            Some(ROW)
        ),
        Pick::Here
    );
    assert_eq!(
        pick(
            &TurnFacts {
                default_provider: false,
                ..auto(Some("smart"))
            },
            Some(ROW)
        ),
        Pick::Here
    );
    assert_eq!(
        pick(
            &TurnFacts {
                automatic: false,
                ..auto(None)
            },
            Some(ROW)
        ),
        Pick::Manual
    );
    assert_eq!(
        pick(
            &TurnFacts {
                automatic: false,
                default_provider: false,
                ..auto(None)
            },
            Some(ROW)
        ),
        Pick::Here
    );
    assert_eq!(pick(&auto(Some("smart")), None), Pick::Here);
}

// ---- the health cache ----

#[test]
fn health_urls_and_probe_states() {
    assert_eq!(
        health_url(" http://Pi.LAN:8080/v1/x?y ", "/health").as_deref(),
        Some("http://pi.lan:8080/health")
    );
    assert_eq!(
        health_url("https://pi.lan:443", "/health").as_deref(),
        Some("https://pi.lan/health")
    );
    assert_eq!(
        health_url("http://[fd00::1]:9/", "/v1/models").as_deref(),
        Some("http://[fd00::1]:9/v1/models")
    );
    assert_eq!(health_url("ftp://pi.lan", "/health"), None);
    assert_eq!(health_url("not a url", "/health"), None);
    let p = |a, sent| {
        let r: Probe = probe_result(a, sent);
        (r.state, r.reason)
    };
    assert_eq!(p(Ok(204), false), (State::Up, None));
    assert_eq!(p(Ok(401), false), (State::Up, Some("needs-auth".into())));
    assert_eq!(p(Ok(403), true), (State::Down, Some("http-403".into())));
    assert_eq!(p(Ok(500), false), (State::Down, Some("http-500".into())));
    for (e, why) in [
        (FetchError::NotOnThisNetwork, "not-on-this-network"),
        (FetchError::Refused, "refused"),
        (FetchError::Timeout, "timeout"),
        (FetchError::NotFound, "not-found"),
        (FetchError::Redirect, "network"),
        (FetchError::Network, "network"),
    ] {
        assert_eq!(p(Err(e), false), (State::Unreachable, Some(why.into())));
    }
    let of = |state, reason: Option<&str>| Probe {
        state,
        reason: reason.map(Into::into),
        checked_at_ms: None,
    };
    assert_eq!(machine_state(&[]), "idle");
    assert_eq!(machine_state(&[of(State::Unknown, None)]), "idle");
    assert_eq!(
        machine_state(&[of(State::Up, None), of(State::Down, Some("http-500"))]),
        "down"
    );
    let mixed = [
        of(State::Down, Some("http-500")),
        of(State::Unreachable, Some("timeout")),
    ];
    assert_eq!(machine_state(&mixed), "unreachable");
    assert_eq!(machine_reason(&mixed), "timeout");
    assert_eq!(machine_reason(&[of(State::Up, None)]), "");
    let named = [
        ("ocr".to_owned(), mixed[0].clone()),
        ("embedding".to_owned(), mixed[1].clone()),
    ];
    assert_eq!(machine_failing(&named), "embedding");
}

#[tokio::test]
async fn a_probe_is_cached_for_the_ttl_failures_too() {
    let script = Arc::new(Script::default());
    script.answer("http://pi.lan:8080/health", status(200));
    let (at, c) = cache(script.clone(), 32);
    let first = c
        .probe("http://pi.lan:8080", &ProbeOptions::default())
        .await;
    assert_eq!(first.state, State::Up);
    assert_eq!(first.checked_at_ms, Some(1_000_000));
    script.answer("http://pi.lan:8080/health", Err(FetchError::Refused));
    at.fetch_add(29_999, Ordering::SeqCst);
    assert_eq!(
        c.probe("http://pi.lan:8080/v1", &ProbeOptions::default())
            .await,
        first,
        "same origin, same entry"
    );
    assert_eq!(script.calls().len(), 1);
    at.fetch_add(1, Ordering::SeqCst);
    let second = c
        .probe("http://pi.lan:8080", &ProbeOptions::default())
        .await;
    assert_eq!(
        (second.state, second.reason.as_deref()),
        (State::Unreachable, Some("refused"))
    );
    at.fetch_add(10, Ordering::SeqCst);
    assert_eq!(
        c.probe("http://pi.lan:8080", &ProbeOptions::default())
            .await,
        second,
        "a failure is reused too"
    );
    assert_eq!(script.calls().len(), 2);
    assert_eq!(
        c.probe("ftp://x", &ProbeOptions::default()).await.state,
        State::Unknown
    );
    assert_eq!(script.calls().len(), 2, "no request for an unknown address");
}

#[tokio::test]
async fn one_probe_per_address_at_a_time() {
    let gate = Arc::new(tokio::sync::Notify::new());
    let script = Arc::new(Script {
        gate: Some(gate.clone()),
        ..Script::default()
    });
    script.answer("http://pi.lan:8080/health", status(200));
    let (_, c) = cache(script.clone(), 32);
    let opts = ProbeOptions::default();
    let release = async {
        // Both callers are waiting before the one request is answered.
        while script.calls().is_empty() {
            tokio::task::yield_now().await;
        }
        for _ in 0..10 {
            tokio::task::yield_now().await;
        }
        gate.notify_waiters();
        gate.notify_one();
    };
    let (a, b, ()) = tokio::time::timeout(Duration::from_secs(5), async {
        tokio::join!(
            c.probe("http://pi.lan:8080", &opts),
            c.probe("http://pi.lan:8080", &opts),
            release
        )
    })
    .await
    .unwrap();
    assert_eq!(a, b);
    assert_eq!(script.calls().len(), 1);
}

#[tokio::test]
async fn a_404_health_asks_the_fallback_path_and_headers_count() {
    let script = Arc::new(Script::default());
    script.answer("http://kev.lan:11435/health", status(404));
    script.answer("http://kev.lan:11435/v1/models", status(401));
    let (_, c) = cache(script.clone(), 32);
    let opts = ProbeOptions {
        fallback_path: Some("/v1/models".into()),
        ..ProbeOptions::default()
    };
    let p = c.probe("http://kev.lan:11435", &opts).await;
    assert_eq!(
        (p.state, p.reason.as_deref()),
        (State::Up, Some("needs-auth"))
    );
    assert_eq!(
        script.calls(),
        [
            "http://kev.lan:11435/health",
            "http://kev.lan:11435/v1/models"
        ]
    );
    // A key sent and refused is a fault.
    let script = Arc::new(Script::default());
    script.answer("http://kev.lan:11435/health", status(401));
    let (_, c) = cache(script.clone(), 32);
    let opts = ProbeOptions {
        headers: vec![("authorization".into(), "Bearer synthetic".into())],
        ..ProbeOptions::default()
    };
    let p = c.probe("http://kev.lan:11435", &opts).await;
    assert_eq!(
        (p.state, p.reason.as_deref()),
        (State::Down, Some("http-401"))
    );
    assert_eq!(script.calls.lock().unwrap()[0].1, 1, "the header went out");
}

#[tokio::test]
async fn the_cache_drops_the_oldest_past_its_cap() {
    let script = Arc::new(Script::default());
    let (_, c) = cache(script.clone(), 2);
    for host in ["a", "b", "c"] {
        c.probe(&format!("http://{host}.lan"), &ProbeOptions::default())
            .await;
    }
    assert_eq!(c.len(), 2);
    c.probe("http://b.lan", &ProbeOptions::default()).await;
    assert_eq!(script.calls().len(), 3, "b is still cached");
    c.probe("http://a.lan", &ProbeOptions::default()).await;
    assert_eq!(script.calls().len(), 4, "a was dropped first");
    c.clear();
    assert!(c.is_empty());
}

#[tokio::test]
async fn remember_keeps_the_last_value_and_shares_the_cache() {
    let script = Arc::new(Script::default());
    let (at, c) = cache(script, 32);
    let facts = |n: u32| MachineFacts {
        context_tokens: Some(n),
        ..MachineFacts::default()
    };
    assert_eq!(
        c.remember("k", |prev| async move {
            assert_eq!(prev, None);
            Some(facts(4096))
        })
        .await,
        Some(facts(4096))
    );
    assert_eq!(
        c.remember("k", |_| async { None }).await,
        Some(facts(4096)),
        "fresh: not loaded"
    );
    at.fetch_add(30_000, Ordering::SeqCst);
    assert_eq!(
        c.remember("k", |prev| async move { prev }).await,
        Some(facts(4096)),
        "load gets the last value"
    );
    assert_eq!(c.len(), 1);
}

// ---- ready, facts, the measured row, models_at ----

fn machines(script: Arc<Script>) -> MachineTurns<Arc<Script>> {
    let (_, c) = cache(script.clone(), 32);
    MachineTurns::with(script, Some(c))
}

const PROPS: &str = r#"{"default_generation_settings":{"n_ctx":16384},"chat_template":"enable_thinking","modalities":{"vision":false}}"#;

#[tokio::test]
async fn ready_when_up_and_serving_the_model() {
    let t = target("smart", "remote-a");
    let script = Arc::new(Script::default());
    script.answer("http://pi.lan:8080/health", status(200));
    script.answer(
        "http://pi.lan:8080/v1/models",
        body(200, r#"{"data":[{"id":"remote-a"}]}"#),
    );
    script.answer("http://pi.lan:8080/props?model=remote-a", body(200, PROPS));
    let m = machines(script.clone());
    assert_eq!(m.ready(&t).await, Ready::Ok);
    assert_eq!(
        js_json::stringify(&m.measured_provider(&t).await).unwrap(),
        r#"{"id":"machine:m1","label":"Smart model’s machine (Synthetic Pi)","baseUrl":"http://pi.lan:8080/v1","apiKey":"","shared":true,"contextTokens":16384,"machine":{"id":"m1","name":"Synthetic Pi","role":"smart","port":8080,"model":"remote-a","thinkingSwitch":true}}"#
    );
    let ms = m.measurements(&t).await;
    assert_eq!(
        (ms.context_tokens, ms.thinking, ms.vision),
        (Some(16384), Some(true), Some(false))
    );
    assert_eq!(
        ms.not_measured,
        [
            "autotune",
            "speculativeDecoding",
            "longContextProfile",
            "presets"
        ]
    );
    // One probe, one models read, one props read: the rest came from the cache.
    assert_eq!(script.calls().len(), 3, "{:?}", script.calls());
    let caps: Vec<Option<usize>> = script.calls.lock().unwrap().iter().map(|c| c.2).collect();
    assert_eq!(caps, [None, Some(256 * 1024), Some(1024 * 1024)]);
}

#[tokio::test]
async fn ready_refuses_an_unreachable_machine_in_core_s_words() {
    let t = target("smart", "remote-a");
    for (answer, state, detail) in [
        (
            Err(FetchError::Timeout),
            "unreachable",
            ": no answer in time.",
        ),
        (
            Err(FetchError::NotOnThisNetwork),
            "unreachable",
            ": its address is not on this network.",
        ),
        (Err(FetchError::Refused), "unreachable", "."),
        (status(503), "down", ": it answered HTTP 503."),
    ] {
        let script = Arc::new(Script::default());
        script.answer("http://pi.lan:8080/health", answer);
        let m = machines(script.clone());
        let Ready::Refused {
            state: s,
            code,
            error,
        } = m.ready(&t).await
        else {
            panic!("{state}")
        };
        assert_eq!((s.as_str(), code), (state, "machineUnreachable"));
        assert!(
            error.starts_with(&format!(
                "Smart model\u{2019}s machine (Synthetic Pi) is unreachable{detail} Check"
            )),
            "{error}"
        );
        assert_eq!(
            script.calls().len(),
            1,
            "no facts read for a machine that is not up"
        );
    }
}

#[tokio::test]
async fn ready_refuses_a_machine_serving_another_model_but_not_an_unreadable_list() {
    let t = target("smart", "synthetic-remote-1849");
    let script = Arc::new(Script::default());
    script.answer("http://pi.lan:8080/health", status(200));
    script.answer(
        "http://pi.lan:8080/v1/models",
        body(200, r#"{"data":[{"id":"synthetic-other"}]}"#),
    );
    let ready = machines(script).ready(&t).await;
    let Ready::Refused { state, code, error } = &ready else {
        panic!("{ready:?}")
    };
    assert_eq!((state.as_str(), *code), ("wrongModel", "machineWrongModel"));
    assert!(error.contains("serves synthetic-other, not synthetic-remote-1849"));
    let ns = not_sent(&ready, Some("smart"), &t).unwrap();
    assert_eq!(
        (ns.status, ns.code, ns.log503),
        (503, "machineWrongModel", "machine-wrong-model")
    );
    assert_eq!(
        ns.warning,
        "[machines] smart role's machine is wrongModel; the turn is not sent"
    );
    assert_eq!(not_sent(&Ready::Ok, None, &t), None);
    // Models that cannot be read: the request itself will say what is wrong.
    let script = Arc::new(Script::default());
    script.answer("http://pi.lan:8080/health", status(200));
    script.answer(
        "http://pi.lan:8080/v1/models",
        Ok(Reply {
            status: 200,
            body: Some(Err(BodyError::TooLarge)),
        }),
    );
    let m = machines(script);
    assert_eq!(m.ready(&t).await, Ready::Ok);
    assert_eq!(m.context_tokens(&t).await, CONTEXT_FALLBACK);
    // No health cache: every machine is ready.
    let m = MachineTurns::with(Arc::new(Script::default()), None);
    assert_eq!(m.ready(&t).await, Ready::Ok);
}

#[tokio::test]
async fn a_failed_read_keeps_the_last_known_facts() {
    let t = target("smart", "remote-a");
    let script = Arc::new(Script::default());
    script.answer(
        "http://pi.lan:8080/v1/models",
        body(200, r#"{"data":["remote-a"]}"#),
    );
    script.answer("http://pi.lan:8080/props?model=remote-a", body(200, PROPS));
    let (at, c) = cache(script.clone(), 32);
    let m = MachineTurns::with(script.clone(), Some(c));
    assert_eq!(m.facts(&t).await.context_tokens, Some(16384));
    script.answer("http://pi.lan:8080/v1/models", Err(FetchError::Timeout));
    script.answer("http://pi.lan:8080/props?model=remote-a", status(500));
    at.fetch_add(30_000, Ordering::SeqCst);
    let f = m.facts(&t).await;
    assert_eq!(
        (f.models, f.context_tokens, f.thinking),
        (Some(vec!["remote-a".to_owned()]), Some(16384), Some(true))
    );
    assert_eq!(script.calls().len(), 4);
}

#[tokio::test]
async fn models_at_errors_say_what_settings_shows() {
    let script = Arc::new(Script::default());
    script.answer(
        "http://pi.lan:8080/v1/models",
        Err(FetchError::NotOnThisNetwork),
    );
    script.answer("http://pi.lan:8081/v1/models", status(404));
    script.answer("http://pi.lan:8082/v1/models", body(200, "not json"));
    script.answer(
        "http://pi.lan:8083/v1/models",
        body(200, r#"{"data":["x"]}"#),
    );
    let m = machines(script);
    let e = m.models_at("http://pi.lan", 8080, "Pi").await.unwrap_err();
    assert_eq!(
        (e.status, e.code, e.message.as_str()),
        (
            502,
            "publicAddress",
            "Machines must be on this network (a LAN or Tailscale address)."
        )
    );
    let e = m.models_at("http://pi.lan", 8081, "Pi").await.unwrap_err();
    assert_eq!(
        e.message,
        "Pi answered HTTP 404 on port 8081; it does not look like an OpenAI-compatible server."
    );
    let e = m.models_at("http://pi.lan", 8082, "Pi").await.unwrap_err();
    assert_eq!(
        (e.code, e.message.as_str()),
        (
            "machineUnreachable",
            "Pi did not list its models on port 8082."
        )
    );
    let e = m.models_at("http://pi.lan", 8079, "Pi").await.unwrap_err();
    assert_eq!(e.message, "Pi did not answer on port 8079.");
    assert_eq!(
        m.models_at("http://pi.lan", 8083, "Pi").await.unwrap(),
        ["x"]
    );
}

#[test]
fn ready_of_is_the_decision_alone() {
    let t = target("vision", "v");
    let up = Probe {
        state: State::Up,
        reason: None,
        checked_at_ms: None,
    };
    assert_eq!(ready_of(&t, &up, None), Ready::Ok);
    let unknown = Probe {
        state: State::Unknown,
        reason: None,
        checked_at_ms: None,
    };
    let Ready::Refused { state, .. } = ready_of(&t, &unknown, None) else {
        panic!()
    };
    assert_eq!(state, "unknown");
    let f = MachineFacts {
        models: Some(vec![]),
        ..MachineFacts::default()
    };
    let Ready::Refused { error, .. } = ready_of(&t, &up, Some(&f)) else {
        panic!()
    };
    assert!(error.starts_with("The Vision machine (Synthetic Pi) serves no model, not v."));
}
