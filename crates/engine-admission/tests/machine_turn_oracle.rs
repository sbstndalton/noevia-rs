//! Replays `fixtures/machine-turn.v1.json` (written by `record_machine_turn.cjs` from noevia-core's own
//! chat-machines.cjs and machine-health.cjs over a scripted fetch). Not recorded yet (no build host this round):
//! without the file every test here says so and passes, unless MACHINE_TURN_ORACLE_REQUIRED=1.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]
use engine_admission::machine_health::{health_url, Fetch, FetchError, HealthCache, Reply};
use engine_admission::machine_turn::{
    connect_error, effective_roles, local_roles, machine_models, manual_target, model_ids,
    props_facts_of, ConnectWhy, MachineTurns, Ready,
};
use engine_admission::machines::Target;
use js_json::JValue;
use std::sync::{Arc, Mutex};
use std::time::Duration;

fn recorded() -> Option<JValue> {
    let file = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/machine-turn.v1.json"
    );
    match std::fs::read_to_string(file) {
        Ok(text) => Some(js_json::parse(&text).expect("fixture")),
        Err(_) if std::env::var("MACHINE_TURN_ORACLE_REQUIRED").as_deref() == Ok("1") => {
            panic!("{file} is missing and MACHINE_TURN_ORACLE_REQUIRED=1: record it with record_machine_turn.cjs")
        }
        Err(_) => {
            eprintln!("machine-turn oracle not recorded yet ({file}); skipped");
            None
        }
    }
}

fn arr(v: &JValue) -> &[JValue] {
    match v {
        JValue::Arr(a) => a,
        other => panic!("not an array: {other:?}"),
    }
}

fn s(v: &JValue) -> String {
    js_json::stringify(v).unwrap_or_else(|| "undefined".into())
}

fn opt_num(v: Option<u32>) -> JValue {
    v.map_or(JValue::Null, |n| JValue::Num(f64::from(n)))
}

fn opt_bool(v: Option<bool>) -> JValue {
    v.map_or(JValue::Null, JValue::Bool)
}

fn target(role: &str, model: &str, id: &str, name: &str, address: &str, port: u32) -> Target {
    Target {
        role: role.into(),
        machine_id: id.into(),
        machine_name: name.into(),
        port,
        model: model.into(),
        base_url: format!("{address}:{port}"),
    }
}

#[test]
fn sources_are_pinned() {
    let Some(r) = recorded() else { return };
    let JValue::Obj(sources) = r.get("sources") else {
        panic!("no sources")
    };
    assert_eq!(sources.len(), 2);
}

#[test]
fn props_models_and_urls() {
    let Some(r) = recorded() else { return };
    for case in arr(r.get("props")) {
        let f = props_facts_of(case.get("body"));
        let want = case.get("facts");
        assert_eq!(
            opt_num(f.context_tokens),
            *want.get("contextTokens"),
            "{}",
            s(case)
        );
        assert_eq!(opt_bool(f.thinking), *want.get("thinking"), "{}", s(case));
        assert_eq!(opt_bool(f.vision), *want.get("vision"), "{}", s(case));
    }
    for case in arr(r.get("models")) {
        let ids: Vec<JValue> = model_ids(case.get("body"))
            .into_iter()
            .map(JValue::from)
            .collect();
        assert_eq!(JValue::Arr(ids), *case.get("ids"));
    }
    for case in arr(r.get("urls")) {
        let got = health_url(
            case.get("base").as_str().unwrap(),
            case.get("path").as_str().unwrap(),
        );
        assert_eq!(
            got.map_or(JValue::Null, JValue::from),
            *case.get("url"),
            "{}",
            s(case)
        );
    }
}

/// The recorded answers, replayed: `{ status, body }` or `{ error }`.
struct Replayed {
    answers: JValue,
    calls: Mutex<Vec<String>>,
}

impl Fetch for Replayed {
    async fn get(
        &self,
        url: &str,
        _headers: &[(String, String)],
        _timeout: Duration,
        body_cap: Option<usize>,
    ) -> Result<Reply, FetchError> {
        self.calls.lock().unwrap().push(url.into());
        let a = self.answers.get(url);
        if a.is_nullish() {
            return Err(FetchError::Network);
        }
        if let Some(e) = a.get("error").as_str() {
            return Err(match e {
                "EPUBLICADDR" => FetchError::NotOnThisNetwork,
                "ECONNREFUSED" => FetchError::Refused,
                "TimeoutError" => FetchError::Timeout,
                "ENOTFOUND" => FetchError::NotFound,
                "redirect" => FetchError::Redirect,
                _ => FetchError::Network,
            });
        }
        let status = a.get("status").as_num().unwrap() as u16;
        // A Response with no body reads as '' in Node.
        let body = body_cap
            .filter(|_| (200..=299).contains(&status))
            .map(|_| Ok(a.get("body").as_str().unwrap_or("").to_owned()));
        Ok(Reply { status, body })
    }
}

#[tokio::test]
async fn every_recorded_ready_row_and_measurement() {
    let Some(r) = recorded() else { return };
    let cases = arr(r.get("ready"));
    assert_eq!(cases.len(), 13);
    for case in cases {
        let name = case.get("name").as_str().unwrap();
        let fetch = Arc::new(Replayed {
            answers: case.get("answers").clone(),
            calls: Mutex::new(Vec::new()),
        });
        let health = HealthCache::with(
            fetch.clone(),
            30_000,
            Duration::from_millis(2500),
            32,
            Arc::new(|| 1_000_000_u64),
        );
        let m = MachineTurns::with(fetch.clone(), Some(health));
        let t = target(
            "smart",
            "qa-remote",
            "qa-m1",
            "QA Pi",
            "http://qa-pi.lan",
            8080,
        );
        let ready = match m.ready(&t).await {
            Ready::Ok => JValue::obj([("ok", JValue::Bool(true))]),
            Ready::Refused { state, code, error } => JValue::obj([
                ("ok", JValue::Bool(false)),
                ("state", JValue::from(state)),
                ("code", JValue::from(code)),
                ("error", JValue::from(error)),
            ]),
        };
        assert_eq!(s(&ready), s(case.get("ready")), "{name}");
        assert_eq!(
            s(&m.measured_provider(&t).await),
            s(case.get("row")),
            "{name}"
        );
        let ms = m.measurements(&t).await;
        let want = case.get("measured");
        assert_eq!(
            opt_num(ms.context_tokens),
            *want.get("contextTokens"),
            "{name}"
        );
        assert_eq!(opt_bool(ms.thinking), *want.get("thinking"), "{name}");
        assert_eq!(opt_bool(ms.vision), *want.get("vision"), "{name}");
        let calls: Vec<JValue> = fetch
            .calls
            .lock()
            .unwrap()
            .iter()
            .cloned()
            .map(JValue::from)
            .collect();
        assert_eq!(s(&JValue::Arr(calls)), s(case.get("calls")), "{name}");
    }
}

#[test]
fn connect_errors() {
    let Some(r) = recorded() else { return };
    let t = target(
        "code",
        "qa-code",
        "qa-m1",
        "QA Pi",
        "http://qa-pi.lan",
        8080,
    );
    for case in arr(r.get("connect")) {
        let why = match case.get("kind").as_str().unwrap() {
            "EPUBLICADDR" => ConnectWhy::NotOnThisNetwork,
            "redirect" => ConnectWhy::Redirect,
            "TimeoutError" => ConnectWhy::Timeout,
            _ => ConnectWhy::Other,
        };
        assert_eq!(
            Some(connect_error(&t, why).as_str()),
            case.get("text").as_str(),
            "{}",
            s(case)
        );
    }
}

fn remote() -> Vec<Target> {
    vec![
        target(
            "smart",
            "qa-remote-a",
            "qa-m1",
            "QA Pi",
            "http://qa-pi.lan",
            8080,
        ),
        target(
            "code",
            "qa-remote-a",
            "qa-m1",
            "QA Pi",
            "http://qa-pi.lan",
            8080,
        ),
        target(
            "vision",
            "qa-remote-v",
            "qa-m2",
            "QA Rock",
            "http://qa-rock.lan",
            9000,
        ),
    ]
}

#[tokio::test]
async fn manual_picks_roles_and_the_manual_list() {
    let Some(r) = recorded() else { return };
    let remote = remote();
    for case in arr(r.get("manual")) {
        let reads = std::sync::atomic::AtomicU32::new(0);
        let catalogue = case.get("catalogue").clone();
        let got = manual_target(case.get("model").as_str().unwrap(), &remote, || {
            reads.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let rows = catalogue.clone();
            async move {
                if rows.as_str() == Some("throws") {
                    Err(())
                } else {
                    Ok(rows)
                }
            }
        })
        .await;
        assert_eq!(
            got.map_or(JValue::Null, |t| JValue::from(t.role)),
            *case.get("role"),
            "{}",
            s(case)
        );
        assert_eq!(
            f64::from(reads.load(std::sync::atomic::Ordering::SeqCst)),
            case.get("reads").as_num().unwrap(),
            "{}",
            s(case)
        );
    }
    for case in arr(r.get("roles")) {
        assert_eq!(
            s(&effective_roles(case.get("roles"), &remote)),
            s(case.get("effective")),
            "{}",
            s(case)
        );
        assert_eq!(
            s(&local_roles(case.get("roles"), &remote)),
            s(case.get("local")),
            "{}",
            s(case)
        );
    }
    let rows: Vec<JValue> = machine_models(&remote)
        .into_iter()
        .map(|m| {
            JValue::obj([
                ("model", JValue::from(m.model)),
                (
                    "machine",
                    JValue::obj([
                        ("id", JValue::from(m.machine_id)),
                        ("name", JValue::from(m.machine_name)),
                    ]),
                ),
                (
                    "roles",
                    JValue::Arr(m.roles.into_iter().map(JValue::from).collect()),
                ),
            ])
        })
        .collect();
    assert_eq!(s(&JValue::Arr(rows)), s(r.get("machineModels")));
}
