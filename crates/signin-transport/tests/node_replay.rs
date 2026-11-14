//! Replays core's recorded transport (tests/fixtures/node-transport.v1.json, written by
//! tests/record_transport.cjs from core's own sign-in provider module) through this crate: every request
//! mapping, every event stream (whole and cut at every byte) and every full fetch, from the
//! catalogue read and the refresh-once rule to the bytes the chat loop receives.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use js_json::JValue;
use signin_transport::events::{parts_of, Part};
use signin_transport::reply::{completion, error_body, ChatStream, ReplyId, Start};
use signin_transport::request::{self, headers, plan, uuid_from, Protocol, Refusal, Session};
use signin_transport::session::{Authorise, BrokerAnswer, SessionCache, Step};
use signin_transport::texts::Texts;

fn fixture() -> JValue {
    let text = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/node-transport.v1.json"
    ))
    .unwrap();
    js_json::parse(&text).unwrap()
}

fn s(v: &JValue) -> String {
    v.as_str()
        .unwrap_or_else(|| panic!("not a string: {v:?}"))
        .to_string()
}

fn texts(f: &JValue) -> Texts {
    let t = f.get("texts");
    Texts {
        label: s(t.get("label")),
        disconnected: s(t.get("disconnected")),
        reconnect: s(t.get("reconnect")),
        transient: s(t.get("transient")),
        not_included: s(t.get("notIncluded")),
        source: s(t.get("source")),
    }
}

fn b64(text: &str) -> Vec<u8> {
    let val = |c: u8| match c {
        b'A'..=b'Z' => c - b'A',
        b'a'..=b'z' => c - b'a' + 26,
        b'0'..=b'9' => c - b'0' + 52,
        b'+' => 62,
        b'/' => 63,
        _ => panic!("bad base64"),
    };
    let bytes: Vec<u8> = text.bytes().filter(|&c| c != b'=').collect();
    let mut out = Vec::new();
    for chunk in bytes.chunks(4) {
        let n = chunk.iter().enumerate().fold(0u32, |acc, (i, &c)| {
            acc | (u32::from(val(c)) << (18 - 6 * i))
        });
        for i in 0..chunk.len().saturating_sub(1) {
            out.push((n >> (16 - 8 * i)) as u8);
        }
    }
    out
}

fn chunks_of(v: &JValue) -> Vec<Vec<u8>> {
    match v {
        JValue::Arr(items) => items.iter().map(|c| b64(&s(c))).collect(),
        _ => Vec::new(),
    }
}

/// A part in core's shape: `{ delta }`, `{ error, status? }`, `{ finish, usage }`.
fn part_json(p: &Part) -> JValue {
    match p {
        Part::Delta(d) => JValue::obj([("delta", d.clone())]),
        Part::Error { message, status } => {
            let mut o = vec![("error".to_string(), JValue::from(message.as_str()))];
            if let Some(st) = status {
                o.push(("status".to_string(), JValue::Num(f64::from(*st))));
            }
            JValue::Obj(o)
        }
        Part::Finish { finish, usage } => JValue::obj([
            ("finish", JValue::from(finish.as_str())),
            ("usage", usage.clone()),
        ]),
    }
}

fn json(v: &JValue) -> String {
    js_json::stringify(v).unwrap()
}

#[test]
fn requests_match_core() {
    let f = fixture();
    let JValue::Arr(cases) = f.get("requests") else {
        panic!()
    };
    assert!(cases.len() > 100, "{}", cases.len());
    for c in cases {
        let body = js_json::parse(&s(c.get("body"))).unwrap();
        let out = json(&JValue::Obj(request::to_responses_request(&body)));
        assert_eq!(out, s(c.get("out")), "body {}", s(c.get("body")));
    }
}

#[test]
fn streams_match_core_whole_and_cut_everywhere() {
    let f = fixture();
    let t = texts(&f);
    let now = f.get("now").as_num().unwrap();
    let JValue::Obj(streams) = f.get("streams") else {
        panic!()
    };
    assert!(streams.len() > 25);
    let mut differences = 0;
    for (name, st) in streams {
        let chunks = chunks_of(st.get("chunks"));
        let mut want = json(st.get("parts"));
        if want.contains("\"thrown\":\"Invalid time value\"") {
            // Named difference (noevia bug, see the crate report): core throws while formatting a
            // reset time past JS's date range, which kills the reply; the port leaves the time out.
            assert_eq!(name, "errorFarDate");
            want = format!(
                "[{{\"error\":\"{}\",\"status\":429}}]",
                t.usage_limit("", "")
            );
            differences += 1;
        }
        let refs: Vec<&[u8]> = chunks.iter().map(Vec::as_slice).collect();
        let got = JValue::Arr(parts_of(&t, now, &refs).iter().map(part_json).collect());
        assert_eq!(json(&got), want, "stream {name}");
        // The same bytes as one chunk, and cut at every single position: same parts.
        let whole: Vec<u8> = chunks.concat();
        let one = JValue::Arr(parts_of(&t, now, &[&whole]).iter().map(part_json).collect());
        assert_eq!(json(&one), want, "stream {name} as one chunk");
        for cut in 0..=whole.len() {
            let (a, b) = whole.split_at(cut);
            let two = JValue::Arr(parts_of(&t, now, &[a, b]).iter().map(part_json).collect());
            assert_eq!(json(&two), want, "stream {name} cut at {cut}");
        }
    }
    assert_eq!(differences, 1);
}

struct Run {
    status: u16,
    content_type: Option<String>,
    provider_message: Option<String>,
    text: String,
    sent: Vec<JValue>,
    broker_calls: Vec<JValue>,
}

fn refused(r: &Refusal, run: &mut Run) {
    run.status = r.status;
    run.content_type = Some("application/json".into());
    run.provider_message = Some("1".into());
    run.text = error_body(r);
}

fn session_of(v: &JValue) -> BrokerAnswer {
    if v.as_str() == Some("throw") {
        return BrokerAnswer::Unreachable;
    }
    if v.get("ok").as_bool() != Some(true) {
        return BrokerAnswer::Refused {
            error: v.get("error").as_str().map(str::to_string),
            status: v.get("status").as_num(),
            code: v.get("code").as_str().map(str::to_string),
        };
    }
    let x = v.get("session");
    BrokerAnswer::Ok(Session {
        access_token: s(x.get("accessToken")),
        account_id: s(x.get("accountId")),
        restricted_region: x.get("fedRamp").truthy(),
        expires_at: x.get("expiresAt").as_num(),
        generation: x.get("generation").clone(),
    })
}

/// core's fetchFor, step for step, with the crate's pieces and the case's fake broker and backend.
fn run_fetch(t: &Texts, protocol: &Protocol, now: f64, input: &JValue) -> Run {
    let user = "qa-user";
    let mut run = Run {
        status: 0,
        content_type: None,
        provider_message: None,
        text: String::new(),
        sent: Vec::new(),
        broker_calls: Vec::new(),
    };
    let url = input
        .get("url")
        .as_str()
        .unwrap_or("http://x/v1/chat/completions");
    let parsed = match request::parse(t, url, &s(input.get("body"))) {
        Ok(p) => p,
        Err(r) => {
            refused(&r, &mut run);
            return run;
        }
    };
    run.broker_calls
        .push(JValue::Arr(vec!["catalogue".into(), user.into()]));
    let catalogue = match input.get("catalogue") {
        JValue::Undefined => Some(fixture_catalogue()),
        JValue::Str(x) if x == "throw" => None,
        c if c.get("ok").as_bool() == Some(true) => Some(c.get("models").clone()),
        _ => None,
    };
    let conversation = input
        .get("conversation")
        .as_str()
        .map(|c| uuid_from(&format!("{user}:{c}")));
    let planned = match plan(t, &parsed, catalogue.as_ref(), conversation.as_deref()) {
        Ok(p) => p,
        Err(r) => {
            refused(&r, &mut run);
            return run;
        }
    };
    let JValue::Arr(mut sessions) = input.get("sessions").clone() else {
        panic!()
    };
    let JValue::Arr(mut upstream) = input.get("upstream").clone() else {
        panic!()
    };
    let mut cache = SessionCache::default();
    let mut auth = Authorise::new();
    let mut step = auth.start();
    let answer = loop {
        match step {
            Step::AskBroker { rejected_token } => {
                if let Some(hit) = cache.lookup(user, Some("sig-1"), rejected_token.is_some(), now)
                {
                    step = auth.on_session(t, BrokerAnswer::Ok(hit));
                    continue;
                }
                run.broker_calls.push(JValue::Arr(vec![
                    "session".into(),
                    user.into(),
                    rejected_token.map_or(JValue::Null, JValue::from),
                ]));
                let answer = session_of(&sessions.remove(0));
                if let BrokerAnswer::Ok(sess) = &answer {
                    cache.store(user, sess, Some("sig-1"), now);
                }
                step = auth.on_session(t, answer);
            }
            Step::Send(sess) => {
                let h = headers(
                    protocol,
                    &sess,
                    "text/event-stream",
                    conversation.as_deref(),
                );
                run.sent.push(JValue::obj([
                    (
                        "url",
                        JValue::from(request::responses_url("https://backend.invalid/api/")),
                    ),
                    (
                        "headers",
                        JValue::Obj(h.into_iter().map(|(k, v)| (k, JValue::from(v))).collect()),
                    ),
                    ("body", JValue::from(planned.body.as_str())),
                    ("redirect", JValue::from("error")),
                ]));
                // core's fake backend answers the text stream once a case's list is used up.
                let u = if upstream.is_empty() {
                    JValue::obj([
                        ("status", JValue::Num(200.0)),
                        (
                            "chunks",
                            fixture().get("streams").get("text").get("chunks").clone(),
                        ),
                    ])
                } else {
                    upstream.remove(0)
                };
                let status = u.get("status").as_num().map_or(200, |x| x as u16);
                let bytes = if u.is_nullish() {
                    Vec::new()
                } else {
                    chunks_of(u.get("chunks")).concat()
                };
                step = auth.on_status(t, status);
                if step == Step::Pass {
                    break Some((status, bytes));
                }
            }
            Step::Reject { generation, then } => {
                cache.forget(user);
                run.broker_calls
                    .push(JValue::Arr(vec!["reject".into(), user.into(), generation]));
                refused(&then, &mut run);
                break None;
            }
            Step::Refuse(r) => {
                refused(&r, &mut run);
                break None;
            }
            Step::Pass => unreachable!(),
        }
    };
    let Some((status, bytes)) = answer else {
        return run;
    };
    if !(200..300).contains(&status) {
        let text = String::from_utf8_lossy(&bytes).to_string();
        refused(
            &Refusal::new(status, t.upstream_message(&text, status, now)),
            &mut run,
        );
        return run;
    }
    let parts = parts_of(t, now, &[&bytes]);
    let reply = ReplyId {
        id: "chatcmpl-00000000-0000-4000-8000-000000000000".into(),
        created: (now / 1000.0).floor(),
        model: planned.model.clone(),
    };
    if planned.stream {
        match ChatStream::start(t, reply, parts.first()) {
            (Start::Refuse(r), _) => refused(&r, &mut run),
            (Start::Stream(frames), Some(mut st)) => {
                let mut text = frames.concat();
                for p in parts.iter().skip(1) {
                    text.push_str(&st.frames(Some(p)).concat());
                }
                text.push_str(&st.frames(None).concat());
                run.status = 200;
                run.content_type = Some("text/event-stream".into());
                run.text = text;
            }
            (Start::Stream(_), None) => unreachable!(),
        }
    } else {
        match completion(&reply, &parts) {
            Ok(body) => {
                run.status = 200;
                run.content_type = Some("application/json".into());
                run.text = body;
            }
            Err(r) => refused(&r, &mut run),
        }
    }
    run
}

fn fixture_catalogue() -> JValue {
    js_json::parse(
        r#"[{"slug":"m-large","effort":"medium","summary":"detailed","summaryParam":true,"levels":["low","medium","high","xhigh"]},{"slug":"m-small","summaryParam":false},{"slug":"m-none","summary":"none","summaryParam":true,"levels":[]},{"slug":"m-auto","summaryParam":true,"effort":""}]"#,
    )
    .unwrap()
}

#[test]
fn fetch_path_matches_core() {
    let f = fixture();
    let t = texts(&f);
    let now = f.get("now").as_num().unwrap();
    let protocol = Protocol {
        account_header: "service-account-id".into(),
        region_header: "x-vendor-fedramp".into(),
        user_agent: s(f.get("userAgent")),
    };
    let JValue::Arr(cases) = f.get("fetch") else {
        panic!()
    };
    assert!(cases.len() > 50, "{}", cases.len());
    let mut checked = 0;
    for c in cases {
        let (input, want) = (c.get("input"), c.get("output"));
        let name = s(input.get("name"));
        assert!(
            want.get("thrown").is_undefined_like(),
            "core threw on {name}: {:?}",
            want.get("thrown")
        );
        let got = run_fetch(&t, &protocol, now, input);
        assert_eq!(
            f64::from(got.status),
            want.get("status").as_num().unwrap(),
            "{name} status ({})",
            got.text
        );
        assert_eq!(
            got.content_type.as_deref(),
            want.get("contentType").as_str(),
            "{name} content-type"
        );
        assert_eq!(
            got.provider_message.as_deref(),
            want.get("providerMessage").as_str(),
            "{name} provider-message"
        );
        assert_eq!(got.text, s(want.get("text")), "{name} text");
        assert_eq!(
            json(&JValue::Arr(got.sent)),
            json(want.get("sent")),
            "{name} sent"
        );
        assert_eq!(
            json(&JValue::Arr(got.broker_calls)),
            json(want.get("brokerCalls")),
            "{name} broker calls"
        );
        checked += 1;
    }
    assert_eq!(checked, cases.len());
}

trait UndefinedLike {
    fn is_undefined_like(&self) -> bool;
}
impl UndefinedLike for JValue {
    fn is_undefined_like(&self) -> bool {
        matches!(self, JValue::Undefined)
    }
}
