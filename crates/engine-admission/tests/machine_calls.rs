//! engine-admission machine_calls against core chat-machines.cjs (propsSource, fetchJson, visionFor) and vision.cjs
//! (createVisionProbe) behaviour, read from their code and the #1849/#1850/#1854/#1880 tests, over a scripted fetch:
//! no network.
#![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
use engine_admission::machine_calls::{
    classifier_machine, fetch_json, fetch_json_once, probe_url, props, vision_for, Admit,
    AdmitFuture, Init, JsonReply, Props, PropsFailure, Vision, VisionProbe, FETCH_JSON_TIMEOUT,
    MACHINE_NO_VISION_TEXT, MACHINE_VISION_TEXT, NOT_ADMITTED_TEXT, PROJECTOR_TEXT, UNREACHED_TEXT,
    VISION_BODY_CAP, VISION_MAX_ENTRIES,
};
use engine_admission::machine_health::{
    BodyError, Capped, Clock, Fetch, FetchError, HealthCache, Reply, Sent,
};
use engine_admission::machine_turn::{MachineTurns, MODELS_BODY_CAP, PROPS_BODY_CAP};
use engine_admission::machines::Target;
use js_json::JValue;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

fn target(role: &str, id: &str, base: &str, model: &str) -> Target {
    Target {
        role: role.into(),
        machine_id: id.into(),
        machine_name: format!("QA {id}"),
        port: 8080,
        model: model.into(),
        base_url: base.into(),
    }
}

#[derive(Clone)]
enum A {
    Get(Result<Reply, FetchError>),
    Send(Result<Sent, FetchError>),
}

/// A scripted fetch: an answer per URL (absent: `Network`), every call logged with its method and body.
#[derive(Default)]
struct Script {
    answers: Mutex<HashMap<String, A>>,
    calls: Mutex<Vec<(String, String, Vec<(String, String)>, Option<String>, usize)>>,
    /// Read while a request runs (the turn count).
    during: Mutex<Option<Arc<AtomicUsize>>>,
    seen_during: Mutex<Vec<usize>>,
}

impl Script {
    fn get(&self, url: &str, a: Result<Reply, FetchError>) {
        self.answers.lock().unwrap().insert(url.into(), A::Get(a));
    }
    fn send(&self, url: &str, a: Result<Sent, FetchError>) {
        self.answers.lock().unwrap().insert(url.into(), A::Send(a));
    }
    fn urls(&self) -> Vec<String> {
        self.calls
            .lock()
            .unwrap()
            .iter()
            .map(|c| c.0.clone())
            .collect()
    }
    fn count(&self) -> usize {
        self.calls.lock().unwrap().len()
    }
    fn note(&self) {
        if let Some(d) = &*self.during.lock().unwrap() {
            self.seen_during
                .lock()
                .unwrap()
                .push(d.load(Ordering::SeqCst));
        }
    }
}

fn sent(status: u16, text: &str) -> Result<Sent, FetchError> {
    Ok(Sent {
        status,
        body: Ok(Capped {
            bytes: text.as_bytes().to_vec(),
            capped: false,
        }),
    })
}

fn got(status: u16, body: Option<Result<String, BodyError>>) -> Result<Reply, FetchError> {
    Ok(Reply { status, body })
}

impl Fetch for Script {
    async fn get(
        &self,
        url: &str,
        headers: &[(String, String)],
        _timeout: Duration,
        body_cap: Option<usize>,
    ) -> Result<Reply, FetchError> {
        self.calls.lock().unwrap().push((
            url.into(),
            "GET".into(),
            headers.to_vec(),
            None,
            body_cap.unwrap_or(0),
        ));
        self.note();
        match self.answers.lock().unwrap().get(url).cloned() {
            Some(A::Get(a)) => a,
            _ => Err(FetchError::Network),
        }
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
        self.calls
            .lock()
            .unwrap()
            .push((url.into(), method.into(), headers.to_vec(), body, cap));
        self.note();
        match self.answers.lock().unwrap().get(url).cloned() {
            Some(A::Send(a)) => a,
            _ => Err(FetchError::Network),
        }
    }
}

fn clock() -> (Arc<AtomicU64>, Clock) {
    let at = Arc::new(AtomicU64::new(1_000_000));
    let read = at.clone();
    (at, Arc::new(move || read.load(Ordering::SeqCst)))
}

const BASE: &str = "http://qa-pi.lan:8080";
const PROBE: &str = "http://qa-pi.lan:8080/v1/chat/completions";

// ---- propsSource ----

#[tokio::test]
async fn props_reads_the_model_props_with_the_cap() {
    let s = Script::default();
    let t = target("smart", "qa-m1", BASE, "qa-remote");
    s.get(
        "http://qa-pi.lan:8080/props?model=qa%20remote%2F%C3%BC%3F%26",
        got(200, Some(Ok(r#"{"chat_template":"{{ tools }}"}"#.into()))),
    );
    let p = props(&s, &t, "qa remote/ü?&").await.unwrap();
    assert_eq!(
        p,
        Props::Ok {
            status: 200,
            body: js_json::parse(r#"{"chat_template":"{{ tools }}"}"#).unwrap()
        }
    );
    assert_eq!(
        js_json::stringify(&p.to_json()).unwrap(),
        r#"{"ok":true,"status":200,"body":{"chat_template":"{{ tools }}"}}"#
    );
    assert_eq!(s.calls.lock().unwrap()[0].4, PROPS_BODY_CAP);
}

#[tokio::test]
async fn props_answers_not_ok_and_throws_as_node() {
    let t = target("smart", "qa-m1", BASE, "qa-remote");
    let url = "http://qa-pi.lan:8080/props?model=qa-remote";
    let cases: Vec<(Result<Reply, FetchError>, Result<Props, PropsFailure>)> = vec![
        (got(404, None), Ok(Props::NotOk { status: 404 })),
        (
            got(200, Some(Ok(String::new()))),
            Err(PropsFailure::Unparsable),
        ),
        (got(200, None), Err(PropsFailure::Unparsable)),
        (
            got(200, Some(Ok("not json".into()))),
            Err(PropsFailure::Unparsable),
        ),
        (
            got(200, Some(Err(BodyError::TooLarge))),
            Err(PropsFailure::Body(BodyError::TooLarge)),
        ),
        (
            Err(FetchError::Timeout),
            Err(PropsFailure::Fetch(FetchError::Timeout)),
        ),
        (
            Err(FetchError::Redirect),
            Err(PropsFailure::Fetch(FetchError::Redirect)),
        ),
    ];
    for (answer, want) in cases {
        let s = Script::default();
        s.get(url, answer.clone());
        assert_eq!(props(&s, &t, "qa-remote").await, want, "{answer:?}");
    }
    assert_eq!(
        js_json::stringify(&Props::NotOk { status: 404 }.to_json()).unwrap(),
        r#"{"ok":false,"status":404}"#
    );
}

// ---- fetchJson ----

#[test]
fn the_classifier_counts_on_the_first_role_with_the_origin() {
    let fast = target("fast", "qa-m1", "http://qa-pi.lan:8080", "qa-f");
    let smart = target("smart", "qa-m9", "http://QA-PI.lan:8080/", "qa-s");
    let vision = target("vision", "qa-m2", "http://qa-rock.lan:9000", "qa-v");
    let all = vec![fast, smart, vision];
    let on = |url: &str| {
        classifier_machine(url, &all)
            .unwrap()
            .map(|t| t.machine_id.clone())
    };
    assert_eq!(
        on("http://qa-pi.lan:8080/v1/chat/completions").as_deref(),
        Some("qa-m1")
    );
    // Case and a default path do not change an origin.
    assert_eq!(on("http://Qa-Pi.LAN:8080").as_deref(), Some("qa-m1"));
    assert_eq!(on("http://qa-rock.lan:9000/x").as_deref(), Some("qa-m2"));
    // Another port, scheme or host: no machine.
    assert_eq!(on("http://qa-pi.lan:8081/v1"), None);
    assert_eq!(on("https://qa-pi.lan:8080/v1"), None);
    // A URL that does not parse counts nowhere (the request fails by itself).
    assert_eq!(on("not a url"), None);
    // A target whose base does not parse throws before any request, as `new URL(t.baseUrl)` does.
    let bad = vec![target("fast", "qa-bad", "qa-pi:no", "qa-f")];
    assert!(classifier_machine("http://qa-pi.lan:8080/", &bad).is_err());
    // ... but only when it is reached: a match before it answers.
    let first = vec![
        target("fast", "qa-m1", "http://qa-pi.lan:8080", "qa-f"),
        target("smart", "qa-bad", "::", "qa-s"),
    ];
    assert_eq!(
        classifier_machine("http://qa-pi.lan:8080/", &first)
            .unwrap()
            .map(|t| t.machine_id.as_str()),
        Some("qa-m1")
    );
}

#[tokio::test]
async fn fetch_json_reads_json_text_and_too_large() {
    let url = "http://qa-pi.lan:8080/v1/chat/completions";
    let init = Init {
        method: Some("POST".into()),
        headers: vec![("Content-Type".into(), "application/json".into())],
        body: Some(r#"{"q":1}"#.into()),
    };
    let cases: Vec<(Result<Sent, FetchError>, &str)> = vec![
        (
            sent(200, r#"{"choices":[]}"#),
            r#"{"ok":true,"status":200,"body":{"choices":[]}}"#,
        ),
        (
            sent(200, "plain text"),
            r#"{"ok":true,"status":200,"body":"plain text"}"#,
        ),
        (
            sent(500, r#"{"error":{"message":"busy"}}"#),
            r#"{"ok":false,"status":500,"body":{"error":{"message":"busy"}}}"#,
        ),
        (sent(404, ""), r#"{"ok":false,"status":404,"body":""}"#),
        // A BOM is text to JSON.parse.
        (
            sent(200, "\u{feff}{}"),
            "{\"ok\":true,\"status\":200,\"body\":\"\u{feff}{}\"}",
        ),
        (
            Ok(Sent {
                status: 200,
                body: Ok(Capped {
                    bytes: vec![b'y'; MODELS_BODY_CAP],
                    capped: true,
                }),
            }),
            r#"{"ok":false,"status":200,"error":"response too large"}"#,
        ),
        (
            Ok(Sent {
                status: 502,
                body: Err(BodyError::Failed),
            }),
            r#"{"ok":false,"status":502,"error":"response too large"}"#,
        ),
    ];
    for (answer, want) in cases {
        let s = Script::default();
        s.send(url, answer.clone());
        let r = fetch_json_once(&s, url, &init, FETCH_JSON_TIMEOUT)
            .await
            .unwrap();
        assert_eq!(
            js_json::stringify(&r.to_json()).unwrap(),
            want,
            "{answer:?}"
        );
        let calls = s.calls.lock().unwrap();
        assert_eq!(calls[0].1, "POST");
        assert_eq!(calls[0].3.as_deref(), Some(r#"{"q":1}"#));
        assert_eq!(calls[0].4, MODELS_BODY_CAP);
    }
    // No answer: fetchJson rejects.
    let s = Script::default();
    s.send(url, Err(FetchError::Refused));
    assert_eq!(
        fetch_json_once(&s, url, &Init::default(), FETCH_JSON_TIMEOUT).await,
        Err(FetchError::Refused)
    );
    assert_eq!(s.calls.lock().unwrap()[0].1, "GET");
    assert!(matches!(
        JsonReply::TooLarge { status: 1 }.to_json(),
        JValue::Obj(_)
    ));
}

struct Held(Arc<AtomicUsize>);

impl Drop for Held {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

#[tokio::test]
async fn fetch_json_counts_a_turn_while_it_runs_answer_or_not() {
    let roles = vec![
        target("fast", "qa-m1", "http://qa-pi.lan:8080", "qa-f"),
        target("vision", "qa-m2", "http://qa-rock.lan:9000", "qa-v"),
    ];
    for (url, answer, on) in [
        (
            "http://qa-pi.lan:8080/v1/chat/completions",
            sent(200, "{}"),
            Some("qa-m1"),
        ),
        (
            "http://qa-rock.lan:9000/v1/chat/completions",
            Err(FetchError::Timeout),
            Some("qa-m2"),
        ),
        (
            "http://qa-pi.lan:8081/v1/chat/completions",
            sent(200, "{}"),
            None,
        ),
    ] {
        let n = Arc::new(AtomicUsize::new(0));
        let s = Script::default();
        *s.during.lock().unwrap() = Some(n.clone());
        s.send(url, answer);
        let began = Mutex::new(Vec::new());
        let held = n.clone();
        let _ = fetch_json(
            &s,
            &roles,
            url,
            &Init::default(),
            FETCH_JSON_TIMEOUT,
            |id| {
                began.lock().unwrap().push(id.to_owned());
                held.fetch_add(1, Ordering::SeqCst);
                Held(held.clone())
            },
        )
        .await;
        assert_eq!(
            began.lock().unwrap().first().map(String::as_str),
            on,
            "{url}"
        );
        assert_eq!(
            *s.seen_during.lock().unwrap(),
            vec![usize::from(on.is_some())],
            "{url}"
        );
        assert_eq!(n.load(Ordering::SeqCst), 0, "{url}: released");
    }
    // A target base that does not parse: no request at all.
    let s = Script::default();
    let bad = vec![target("fast", "qa-bad", "::", "qa-f")];
    let r = fetch_json(
        &s,
        &bad,
        "http://qa-pi.lan:8080/",
        &Init::default(),
        FETCH_JSON_TIMEOUT,
        |_| (),
    )
    .await;
    assert_eq!(r, Err(FetchError::Network));
    assert_eq!(s.count(), 0);
}

// ---- vision.cjs ----

#[tokio::test]
async fn the_probe_sends_one_pixel_and_reads_the_refusal() {
    let cases: Vec<(Result<Sent, FetchError>, Vision)> = vec![
        (
            sent(200, ""),
            Vision {
                supported: true,
                reason: None,
            },
        ),
        (
            sent(400, "image input is not supported, you may need to provide the MMPROJ"),
            Vision {
                supported: false,
                reason: Some(PROJECTOR_TEXT.into()),
            },
        ),
        (
            sent(500, "no Projector loaded"),
            Vision {
                supported: false,
                reason: Some(PROJECTOR_TEXT.into()),
            },
        ),
        (
            sent(422, "bad request"),
            Vision {
                supported: false,
                reason: Some("The inference server rejected the image probe (HTTP 422). Check model configuration and server availability.".into()),
            },
        ),
        (
            Ok(Sent {
                status: 400,
                body: Err(BodyError::Failed),
            }),
            Vision {
                supported: false,
                reason: Some(UNREACHED_TEXT.into()),
            },
        ),
        (
            Err(FetchError::Redirect),
            Vision {
                supported: false,
                reason: Some(UNREACHED_TEXT.into()),
            },
        ),
        (
            Err(FetchError::Timeout),
            Vision {
                supported: false,
                reason: Some(UNREACHED_TEXT.into()),
            },
        ),
    ];
    for (answer, want) in cases {
        let s = Arc::new(Script::default());
        s.send(PROBE, answer.clone());
        let (_, now) = clock();
        let probe = VisionProbe::with(s.clone(), now, None);
        assert_eq!(
            probe
                .probe("http://qa-pi.lan:8080/v1/", &[], "qa-vision")
                .await,
            want,
            "{answer:?}"
        );
        let calls = s.calls.lock().unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, PROBE);
        assert_eq!(calls[0].1, "POST");
        assert_eq!(
            calls[0].2,
            vec![("Content-Type".to_owned(), "application/json".to_owned())]
        );
        assert!(calls[0]
            .3
            .as_deref()
            .unwrap()
            .starts_with(r#"{"model":"qa-vision","max_tokens":1,"#));
        assert_eq!(calls[0].4, VISION_BODY_CAP);
    }
    assert_eq!(probe_url(BASE), PROBE);
}

#[tokio::test]
async fn a_hint_past_the_cap_is_not_read() {
    let s = Arc::new(Script::default());
    // What a fetch gives for 64 KiB of text then the hint: the first 64 KiB, capped.
    s.send(
        PROBE,
        Ok(Sent {
            status: 400,
            body: Ok(Capped {
                bytes: vec![b'z'; VISION_BODY_CAP],
                capped: true,
            }),
        }),
    );
    let (_, now) = clock();
    let probe = VisionProbe::with(s.clone(), now, None);
    let v = probe.probe(BASE, &[], "qa-vision").await;
    assert_eq!(
        v.reason.as_deref(),
        Some("The inference server rejected the image probe (HTTP 400). Check model configuration and server availability.")
    );
}

#[tokio::test]
async fn the_probe_cache_keeps_answers_five_minutes_or_thirty_seconds() {
    for (answer, life) in [(sent(200, ""), 300_000u64), (sent(400, "mmproj"), 30_000)] {
        let s = Arc::new(Script::default());
        s.send(PROBE, answer);
        let (at, now) = clock();
        let probe = VisionProbe::with(s.clone(), now, None);
        probe.probe(BASE, &[], "qa-vision").await;
        at.fetch_add(life - 1, Ordering::SeqCst);
        probe.probe(BASE, &[], "qa-vision").await;
        assert_eq!(s.count(), 1, "kept for {life} ms");
        // `until > now`: at exactly the end it is gone.
        at.fetch_add(1, Ordering::SeqCst);
        probe.probe(BASE, &[], "qa-vision").await;
        assert_eq!(s.count(), 2, "gone after {life} ms");
        assert_eq!(probe.len(), 1, "the key set again keeps one entry");
    }
}

#[tokio::test]
async fn the_probe_cache_is_keyed_by_address_headers_and_model() {
    let s = Arc::new(Script::default());
    s.send(PROBE, sent(200, ""));
    s.send("http://qa-rock.lan:9000/v1/chat/completions", sent(200, ""));
    let (_, now) = clock();
    let probe = VisionProbe::with(s.clone(), now, None);
    let auth = vec![("Authorization".to_owned(), "Bearer qa-1".to_owned())];
    let other = vec![("Authorization".to_owned(), "Bearer qa-2".to_owned())];
    probe.probe(BASE, &[], "qa-vision").await;
    probe.probe(&format!("{BASE}/v1"), &[], "qa-vision").await; // another base text, another key
    probe.probe(BASE, &auth, "qa-vision").await;
    probe.probe(BASE, &other, "qa-vision").await;
    probe.probe(BASE, &auth, "qa-vision-2").await;
    probe
        .probe("http://qa-rock.lan:9000", &[], "qa-vision")
        .await;
    assert_eq!(s.count(), 6);
    probe.probe(BASE, &auth, "qa-vision").await;
    assert_eq!(s.count(), 6, "cached");
    // The caller's headers ride after the content type.
    let calls = s.calls.lock().unwrap();
    assert_eq!(
        calls[2].2,
        vec![
            ("Content-Type".to_owned(), "application/json".to_owned()),
            ("Authorization".to_owned(), "Bearer qa-1".to_owned())
        ]
    );
}

#[tokio::test]
async fn the_probe_cache_drops_its_oldest_key_past_128() {
    let s = Arc::new(Script::default());
    s.send(PROBE, sent(200, ""));
    let (_, now) = clock();
    let probe = VisionProbe::with(s.clone(), now, None);
    for i in 0..=VISION_MAX_ENTRIES {
        probe.probe(BASE, &[], &format!("qa-m{i}")).await;
    }
    assert_eq!(probe.len(), VISION_MAX_ENTRIES);
    let before = s.count();
    probe.probe(BASE, &[], "qa-m1").await; // still there
    assert_eq!(s.count(), before);
    probe.probe(BASE, &[], "qa-m0").await; // the oldest went
    assert_eq!(s.count(), before + 1);
}

#[tokio::test]
async fn a_refused_admission_is_reported_and_not_cached() {
    let runs = Arc::new(AtomicUsize::new(0));
    for (refusal, want) in [
        (Some(Some("QA budget full.".to_owned())), "QA budget full."),
        (Some(None), NOT_ADMITTED_TEXT),
    ] {
        let s = Arc::new(Script::default());
        s.send(PROBE, sent(200, ""));
        let seen = runs.clone();
        let admit: Admit = Arc::new(move |base, model| {
            assert_eq!((base.as_str(), model.as_str()), (BASE, "qa-vision"));
            seen.fetch_add(1, Ordering::SeqCst);
            let refusal = refusal.clone();
            let fut: AdmitFuture = Box::pin(async move { refusal.map_or(Ok(()), Err) });
            fut
        });
        let (_, now) = clock();
        let probe = VisionProbe::with(s.clone(), now, Some(admit));
        for _ in 0..2 {
            let v = probe.probe(BASE, &[], "qa-vision").await;
            assert_eq!(
                v,
                Vision {
                    supported: false,
                    reason: Some(want.into())
                }
            );
        }
        assert_eq!(s.count(), 0);
        assert!(probe.is_empty());
    }
    assert_eq!(runs.load(Ordering::SeqCst), 4);
}

// ---- visionFor ----

#[tokio::test]
async fn vision_for_trusts_the_machine_props_then_probes() {
    let t = target("vision", "qa-m2", "http://qa-rock.lan:9000", "qa-v");
    let models = "http://qa-rock.lan:9000/v1/models";
    let props_url = "http://qa-rock.lan:9000/props?model=qa-v";
    let probe_at = "http://qa-rock.lan:9000/v1/chat/completions";
    for (props_body, probe_answer, want, probed) in [
        (
            Some(r#"{"modalities":{"vision":true}}"#),
            None,
            Vision {
                supported: true,
                reason: Some(MACHINE_VISION_TEXT.into()),
            },
            false,
        ),
        (
            Some(r#"{"modalities":{"vision":false}}"#),
            None,
            Vision {
                supported: false,
                reason: Some(MACHINE_NO_VISION_TEXT.into()),
            },
            false,
        ),
        (
            Some("{}"),
            Some(sent(200, "")),
            Vision {
                supported: true,
                reason: None,
            },
            true,
        ),
        (
            None,
            Some(sent(400, "mmproj missing")),
            Vision {
                supported: false,
                reason: Some(PROJECTOR_TEXT.into()),
            },
            true,
        ),
    ] {
        let s = Arc::new(Script::default());
        s.get(models, got(200, Some(Ok(r#"{"data":["qa-v"]}"#.into()))));
        match props_body {
            Some(b) => s.get(props_url, got(200, Some(Ok(b.into())))),
            None => s.get(props_url, got(500, None)),
        }
        if let Some(a) = probe_answer {
            s.send(probe_at, a);
        }
        let (_, now) = clock();
        let health = HealthCache::with(
            s.clone(),
            30_000,
            Duration::from_millis(2500),
            32,
            now.clone(),
        );
        let turns = MachineTurns::with(s.clone(), Some(health));
        let probe = VisionProbe::with(s.clone(), now, None);
        let headers = vec![("X-QA".to_owned(), "1".to_owned())];
        assert_eq!(
            vision_for(&turns, &probe, &t, &headers).await,
            want,
            "{props_body:?}"
        );
        let urls = s.urls();
        assert_eq!(
            urls.contains(&probe_at.to_owned()),
            probed,
            "{props_body:?}: {urls:?}"
        );
        if probed {
            let calls = s.calls.lock().unwrap();
            let call = calls.iter().find(|c| c.0 == probe_at).unwrap();
            assert_eq!(call.2[1], ("X-QA".to_owned(), "1".to_owned()));
            assert!(call.3.as_deref().unwrap().contains(r#""model":"qa-v""#));
        }
        // The facts are cached with the health: a second ask reads nothing new.
        let n = s.count();
        vision_for(&turns, &probe, &t, &headers).await;
        assert_eq!(s.count(), n, "{props_body:?}: cached");
    }
}
