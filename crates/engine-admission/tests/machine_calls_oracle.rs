//! Replays `fixtures/machine-calls.v1.json` (written by `record_machine_calls.cjs` from noevia-core's own
//! chat-machines.cjs and vision.cjs over a scripted fetch). Not recorded yet (no build host this round): without the
//! file every test here says so and passes, unless MACHINE_CALLS_ORACLE_REQUIRED=1.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]
use engine_admission::machine_calls::{
    fetch_json, props, vision_for, Admit, AdmitFuture, Init, Props, PropsFailure, VisionProbe,
    FETCH_JSON_TIMEOUT,
};
use engine_admission::machine_health::{
    BodyError, Capped, Clock, Fetch, FetchError, HealthCache, Reply, Sent,
};
use engine_admission::machine_turn::{MachineTurns, CHAT_ROLES};
use engine_admission::machines::Target;
use js_json::JValue;
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

fn recorded() -> Option<JValue> {
    let file = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/machine-calls.v1.json"
    );
    match std::fs::read_to_string(file) {
        Ok(text) => Some(js_json::parse(&text).expect("fixture")),
        Err(_) if std::env::var("MACHINE_CALLS_ORACLE_REQUIRED").as_deref() == Ok("1") => {
            panic!("{file} is missing and MACHINE_CALLS_ORACLE_REQUIRED=1: record it with record_machine_calls.cjs")
        }
        Err(_) => {
            eprintln!("machine-calls oracle not recorded yet ({file}); skipped");
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

fn obj(v: &JValue) -> &[(String, JValue)] {
    match v {
        JValue::Obj(o) => o,
        JValue::Undefined | JValue::Null => &[],
        other => panic!("not an object: {other:?}"),
    }
}

fn s(v: &JValue) -> String {
    js_json::stringify(v).unwrap_or_else(|| "undefined".into())
}

fn text(v: &JValue) -> String {
    v.as_str().unwrap_or_default().to_owned()
}

fn headers_of(v: &JValue) -> Vec<(String, String)> {
    obj(v).iter().map(|(k, v)| (k.clone(), text(v))).collect()
}

fn target_of(v: &JValue) -> Target {
    Target {
        role: text(v.get("role")),
        machine_id: text(v.get("machine").get("id")),
        machine_name: text(v.get("machine").get("name")),
        port: v.get("port").as_num().unwrap() as u32,
        model: text(v.get("model")),
        base_url: text(v.get("baseUrl")),
    }
}

/// The recorder's error kinds as the front's fetch reports them.
fn error_of(kind: &str) -> FetchError {
    match kind {
        "EPUBLICADDR" => FetchError::NotOnThisNetwork,
        "ECONNREFUSED" => FetchError::Refused,
        "TimeoutError" => FetchError::Timeout,
        "ENOTFOUND" => FetchError::NotFound,
        "redirect" => FetchError::Redirect,
        _ => FetchError::Network,
    }
}

/// One recorded request: what the Node fetch saw.
#[derive(Debug, Clone, PartialEq)]
struct Call {
    url: String,
    method: String,
    headers: Vec<(String, String)>,
    body: Option<String>,
    busy: Option<BTreeMap<String, bool>>,
}

type Busy = Arc<Mutex<BTreeMap<String, u32>>>;

/// The recorder's scripted fetch: `answers[url]` = `{status, body?, repeat?}` | `{error}`; none: a network error.
struct Script {
    answers: Vec<(String, JValue)>,
    calls: Mutex<Vec<Call>>,
    busy: Option<Busy>,
}

impl Script {
    fn new(answers: &JValue, busy: Option<Busy>) -> Self {
        Script {
            answers: obj(answers).to_vec(),
            calls: Mutex::new(Vec::new()),
            busy,
        }
    }

    fn answer(&self, url: &str) -> Result<(u16, Vec<u8>), FetchError> {
        let Some((_, a)) = self.answers.iter().find(|(k, _)| k == url) else {
            return Err(FetchError::Network);
        };
        if let Some(kind) = a.get("error").as_str() {
            return Err(error_of(kind));
        }
        let mut body = text(a.get("body"));
        if let Some(n) = a.get("repeat").as_num() {
            body = body.repeat(n as usize);
        }
        Ok((a.get("status").as_num().unwrap() as u16, body.into_bytes()))
    }

    fn log(&self, url: &str, method: &str, headers: &[(String, String)], body: Option<String>) {
        let busy = self.busy.as_ref().map(|b| {
            let b = b.lock().unwrap();
            ["qa-m1", "qa-m2"]
                .iter()
                .map(|id| ((*id).to_owned(), b.get(*id).copied().unwrap_or(0) > 0))
                .collect()
        });
        self.calls.lock().unwrap().push(Call {
            url: url.into(),
            method: method.into(),
            headers: headers.to_vec(),
            body,
            busy,
        });
    }

    fn calls(&self) -> Vec<Call> {
        self.calls.lock().unwrap().clone()
    }
}

impl Fetch for Script {
    async fn get(
        &self,
        url: &str,
        headers: &[(String, String)],
        _timeout: Duration,
        body_cap: Option<usize>,
    ) -> Result<Reply, FetchError> {
        self.log(url, "GET", headers, None);
        let (status, bytes) = self.answer(url)?;
        let ok = (200..=299).contains(&status);
        let body = match body_cap {
            Some(cap) if ok => Some(if bytes.len() > cap {
                Err(BodyError::TooLarge)
            } else {
                Ok(String::from_utf8_lossy(&bytes).into_owned())
            }),
            _ => None,
        };
        Ok(Reply { status, body })
    }

    async fn send(
        &self,
        method: &str,
        url: &str,
        headers: &[(String, String)],
        body: Option<String>,
        _timeout: Duration,
        cap: usize,
    ) -> Result<Sent, FetchError> {
        self.log(url, method, headers, body);
        let (status, bytes) = self.answer(url)?;
        let capped = bytes.len() > cap;
        let bytes = bytes.into_iter().take(cap).collect();
        Ok(Sent {
            status,
            body: Ok(Capped { bytes, capped }),
        })
    }
}

fn recorded_calls(v: &JValue) -> Vec<Call> {
    arr(v)
        .iter()
        .map(|c| Call {
            url: text(c.get("url")),
            method: text(c.get("method")),
            headers: headers_of(c.get("headers")),
            body: c.get("body").as_str().map(str::to_owned),
            busy: match c.get("busy") {
                JValue::Obj(o) => Some(
                    o.iter()
                        .map(|(k, v)| (k.clone(), v.as_bool().unwrap_or(false)))
                        .collect(),
                ),
                _ => None,
            },
        })
        .collect()
}

/// A body longer than 1000 units is kept in the fixture as `<n chars>`.
fn shorten(mut v: JValue) -> JValue {
    if let JValue::Obj(items) = &mut v {
        for (k, val) in items.iter_mut() {
            if k == "body" {
                if let JValue::Str(t) = val {
                    let n = js_json::js_len(t);
                    if n > 1000 {
                        *val = JValue::Str(format!("<{n} chars>"));
                    }
                }
            }
        }
    }
    v
}

fn clock(at: u64) -> (Arc<AtomicU64>, Clock) {
    let now = Arc::new(AtomicU64::new(at));
    let read = now.clone();
    (now, Arc::new(move || read.load(Ordering::SeqCst)))
}

#[test]
fn sources_are_pinned() {
    let Some(r) = recorded() else { return };
    assert_eq!(obj(r.get("sources")).len(), 4);
}

#[tokio::test]
async fn props_source_answers_as_node() {
    let Some(r) = recorded() else { return };
    let t = Target {
        role: "smart".into(),
        machine_id: "qa-m1".into(),
        machine_name: "QA Pi".into(),
        port: 8080,
        model: "qa-remote".into(),
        base_url: "http://qa-pi.lan:8080".into(),
    };
    for case in arr(r.get("props")) {
        let model = text(case.get("model"));
        let script = Script::new(
            &JValue::obj([(text(case.get("url")), case.get("answer").clone())]),
            None,
        );
        let got = props(&script, &t, &model).await;
        let want = case.get("result");
        match (&got, want.get("throws").as_str()) {
            (Ok(p), None) => assert_eq!(
                s(&shorten(p.to_json())),
                s(want),
                "{model}: {}",
                s(case.get("answer"))
            ),
            (Err(PropsFailure::Body(BodyError::TooLarge)), Some("tooLarge"))
            | (Err(PropsFailure::Unparsable), Some("SyntaxError"))
            | (
                Err(PropsFailure::Fetch(_)),
                Some("TimeoutError" | "EPUBLICADDR" | "TypeError" | "Error"),
            ) => {}
            (got, want) => panic!(
                "{model} {}: native {got:?}, Node throws {want:?}",
                s(case.get("answer"))
            ),
        }
        if let Ok(Props::NotOk { .. }) = got {
            assert_eq!(want.get("ok").as_bool(), Some(false));
        }
        let urls: Vec<String> = script.calls().into_iter().map(|c| c.url).collect();
        let want_urls: Vec<String> = recorded_calls(case.get("calls"))
            .into_iter()
            .map(|c| c.url)
            .collect();
        assert_eq!(urls, want_urls, "{model}");
    }
}

struct Turn(Busy, String);

impl Drop for Turn {
    fn drop(&mut self) {
        let mut b = self.0.lock().unwrap();
        if let Some(n) = b.get_mut(&self.1) {
            *n -= 1;
        }
    }
}

#[tokio::test]
async fn fetch_json_answers_and_counts_as_node() {
    let Some(r) = recorded() else { return };
    let roles = r.get("roles");
    let targets: Vec<Target> = CHAT_ROLES
        .iter()
        .filter_map(|role| match roles.get(role) {
            JValue::Obj(_) => Some(target_of(roles.get(role))),
            _ => None,
        })
        .collect();
    for case in arr(r.get("fetchJson")) {
        let url = text(case.get("url"));
        let busy: Busy = Arc::new(Mutex::new(BTreeMap::new()));
        let script = Script::new(
            &JValue::obj([(url.clone(), case.get("answer").clone())]),
            Some(busy.clone()),
        );
        let i = case.get("init");
        let init = Init {
            method: i.get("method").as_str().map(str::to_owned),
            headers: headers_of(i.get("headers")),
            body: i.get("body").as_str().map(str::to_owned),
        };
        let b = busy.clone();
        let got = fetch_json(
            &script,
            &targets,
            &url,
            &init,
            FETCH_JSON_TIMEOUT,
            move |id| {
                *b.lock().unwrap().entry(id.to_owned()).or_insert(0) += 1;
                Turn(b.clone(), id.to_owned())
            },
        )
        .await;
        let want = case.get("result");
        match (&got, want.get("throws").as_str()) {
            (Ok(reply), None) => assert_eq!(s(&shorten(reply.to_json())), s(want), "{url}"),
            (Err(_), Some(_)) => {}
            (got, want) => panic!("{url}: native {got:?}, Node throws {want:?}"),
        }
        assert_eq!(script.calls(), recorded_calls(case.get("calls")), "{url}");
        let after = busy.lock().unwrap();
        for (id, was) in obj(case.get("after")) {
            assert_eq!(
                after.get(id).copied().unwrap_or(0) > 0,
                was.as_bool().unwrap(),
                "{url} {id} after"
            );
        }
    }
}

#[tokio::test]
async fn the_image_probe_answers_sends_and_caches_as_node() {
    let Some(r) = recorded() else { return };
    for case in arr(r.get("probes")) {
        let name = text(case.get("name"));
        let script = Arc::new(Script::new(case.get("answers"), None));
        let (now, clock) = clock(1_000_000);
        let probe = VisionProbe::with(script.clone(), clock, None);
        let headers = headers_of(case.get("headers"));
        for step in arr(case.get("steps")) {
            now.store(
                1_000_000 + step.get("at").as_num().unwrap() as u64,
                Ordering::SeqCst,
            );
            let before = script.calls().len();
            let got = probe
                .probe(&text(case.get("base")), &headers, "qa-vision")
                .await;
            assert_eq!(
                s(&got.to_json()),
                s(step.get("result")),
                "{name} at {}",
                s(step.get("at"))
            );
            assert_eq!(
                (script.calls().len() - before) as f64,
                step.get("fetched").as_num().unwrap(),
                "{name} at {}",
                s(step.get("at"))
            );
        }
        assert_eq!(script.calls(), recorded_calls(case.get("calls")), "{name}");
    }
}

#[tokio::test]
async fn admit_refusals_are_reported_not_cached() {
    let Some(r) = recorded() else { return };
    for case in arr(r.get("admits")) {
        let name = text(case.get("name"));
        let refusal: Option<Option<String>> = match name.as_str() {
            "admit refuses with a public message" => Some(Some("QA budget full.".into())),
            "admit refuses without one" => Some(None),
            _ => None,
        };
        let admit: Admit = Arc::new(move |_base, _model| {
            let refusal = refusal.clone();
            let fut: AdmitFuture = Box::pin(async move { refusal.map_or(Ok(()), Err) });
            fut
        });
        let answers = JValue::obj([(
            "http://qa-pi.lan:8080/v1/chat/completions",
            JValue::obj([("status", JValue::Num(200.0))]),
        )]);
        let script = Arc::new(Script::new(&answers, None));
        let (_, clock) = clock(1_000_000);
        let probe = VisionProbe::with(script.clone(), clock, Some(admit));
        let first = probe
            .probe("http://qa-pi.lan:8080/v1", &[], "qa-vision")
            .await;
        let second = probe
            .probe("http://qa-pi.lan:8080/v1", &[], "qa-vision")
            .await;
        assert_eq!(s(&first.to_json()), s(case.get("first")), "{name}");
        assert_eq!(s(&second.to_json()), s(case.get("second")), "{name}");
        assert_eq!(
            script.calls().len() as f64,
            case.get("fetched").as_num().unwrap(),
            "{name}"
        );
    }
}

#[tokio::test]
async fn vision_for_reads_props_then_probes_as_node() {
    let Some(r) = recorded() else { return };
    let t = Target {
        role: "vision".into(),
        machine_id: "qa-m2".into(),
        machine_name: "QA Rock".into(),
        port: 9000,
        model: "qa-v".into(),
        base_url: "http://qa-rock.lan:9000".into(),
    };
    for case in arr(r.get("visionFor")) {
        let name = text(case.get("name"));
        let script = Arc::new(Script::new(case.get("answers"), None));
        let (_, now) = clock(1_000_000);
        let health = HealthCache::with(
            script.clone(),
            30_000,
            Duration::from_millis(2500),
            32,
            now.clone(),
        );
        let turns = MachineTurns::with(script.clone(), Some(health));
        let probe = VisionProbe::with(script.clone(), now, None);
        let got = vision_for(&turns, &probe, &t, &[("X-QA".to_owned(), "1".to_owned())]).await;
        assert_eq!(s(&got.to_json()), s(case.get("result")), "{name}");
        // Both reads of the facts start together: compare as sets.
        let mut urls: Vec<String> = script.calls().into_iter().map(|c| c.url).collect();
        let mut want: Vec<String> = recorded_calls(case.get("calls"))
            .into_iter()
            .map(|c| c.url)
            .collect();
        urls.sort();
        want.sort();
        assert_eq!(urls, want, "{name}");
    }
}
