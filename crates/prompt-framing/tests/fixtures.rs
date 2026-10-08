//! Differential fixtures: every case in `fixtures/prompt-framing.v1.json` was evaluated by
//! noevia-core's JS references (server/prompt-framing.cjs, provenance-policy.cjs, task-packet.cjs)
//! on Node 22 (the runtime noevia ships). The file is byte-identical to noevia-core's
//! `tests/fixtures/prompt-framing.v1.json` (noevia-core CI compares the two and regenerates it with
//! `tools/gen-prompt-framing-fixtures.cjs`). Strings are compared as UTF-16 code units.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use prompt_framing::framing::{escape_closing, frame_untrusted};
use prompt_framing::json::{self, Value};
use prompt_framing::packet;
use prompt_framing::provenance::{self, Store};
use prompt_framing::{packet_call, provenance_call, store_from, store_json};

fn fixtures() -> Value {
    let bytes = std::fs::read(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/prompt-framing.v1.json"
    ))
    .expect("fixture file");
    let f = json::parse_utf8(&bytes, 64).expect("fixture JSON");
    assert_eq!(f.get("version"), Some(&Value::Num(1.0)));
    let l = f.get("limits").unwrap();
    assert_eq!(l.get("gram"), Some(&Value::Num(provenance::GRAM as f64)));
    assert_eq!(
        l.get("maxSources"),
        Some(&Value::Num(provenance::MAX_SOURCES as f64))
    );
    assert_eq!(
        l.get("maxValues"),
        Some(&Value::Num(provenance::MAX_VALUES as f64))
    );
    assert_eq!(
        l.get("defaultMaxChars"),
        Some(&Value::Num(provenance::DEFAULT_MAX_CHARS as f64))
    );
    let p = l.get("packet").unwrap();
    for (k, v) in [
        ("goalChars", packet::GOAL_CHARS),
        ("facts", packet::FACTS),
        ("factChars", packet::FACT_CHARS),
        ("quoteChars", packet::QUOTE_CHARS),
        ("refChars", packet::REF_CHARS),
        ("constraints", packet::CONSTRAINTS),
        ("open_questions", packet::OPEN_QUESTIONS),
        ("itemChars", packet::ITEM_CHARS),
        ("packetBytes", packet::PACKET_BYTES),
        ("inputChars", packet::INPUT_CHARS),
    ] {
        assert_eq!(p.get(k), Some(&Value::Num(v as f64)), "{k}");
    }
    let stems: Vec<String> = arr(f.get("stems")).iter().map(text).collect();
    assert_eq!(stems, provenance::SENSITIVE_STEMS);
    f
}

fn arr(v: Option<&Value>) -> &[Value] {
    match v {
        Some(Value::Arr(a)) => a,
        other => panic!("not an array: {other:?}"),
    }
}

fn units(v: &Value) -> Vec<u16> {
    v.as_str().expect("string").to_vec()
}

fn text(v: &Value) -> String {
    String::from_utf16_lossy(v.as_str().expect("string"))
}

fn show(u: &[u16]) -> String {
    format!("{:?}", String::from_utf16_lossy(u))
}

/// Run `check` on every case of a section; report every disagreement at once.
fn section(f: &Value, name: &str, min: usize, check: impl Fn(&Value) -> Option<String>) {
    let cases = arr(f.get(name));
    assert!(cases.len() >= min, "{name}: only {} cases", cases.len());
    let bad: Vec<String> = cases.iter().filter_map(&check).collect();
    assert!(
        bad.is_empty(),
        "{name}: {} of {} disagree:\n{}",
        bad.len(),
        cases.len(),
        bad.iter().take(20).cloned().collect::<Vec<_>>().join("\n")
    );
}

#[test]
fn frame_and_escape() {
    let f = fixtures();
    section(&f, "frame", 400, |c| {
        let got = frame_untrusted(
            &units(c.get("kind").unwrap()),
            &units(c.get("label").unwrap()),
            &units(c.get("text").unwrap()),
        );
        let want = units(c.get("expect").unwrap());
        (got != want).then(|| format!("got {} want {}", show(&got), show(&want)))
    });
    section(&f, "escape", 300, |c| {
        let got = escape_closing(
            &units(c.get("text").unwrap()),
            &units(c.get("tag").unwrap()),
        );
        let want = units(c.get("expect").unwrap());
        (got != want).then(|| format!("got {} want {}", show(&got), show(&want)))
    });
}

#[test]
fn keys_normalise_candidates_blocks() {
    let f = fixtures();
    section(&f, "keys", 600, |c| {
        let key = units(c.get("key").unwrap());
        let want = c.get("expect") == Some(&Value::Bool(true));
        (provenance::is_sensitive_key(&key) != want)
            .then(|| format!("key {} want {want}", show(&key)))
    });
    section(&f, "normalise", 400, |c| {
        let got = provenance::normalise(&units(c.get("value").unwrap()));
        let want = units(c.get("expect").unwrap());
        (got != want).then(|| format!("got {} want {}", show(&got), show(&want)))
    });
    section(&f, "unicode", 40, |c| {
        let d = units(c.get("domain").unwrap());
        let got = provenance::domain_to_unicode(&d);
        let want = units(c.get("expect").unwrap());
        (got != want).then(|| format!("{}: got {} want {}", show(&d), show(&got), show(&want)))
    });
    section(&f, "candidates", 700, |c| {
        let v = units(c.get("value").unwrap());
        let got = provenance::candidates(&v);
        let want: Vec<Vec<u16>> = arr(c.get("expect")).iter().map(units).collect();
        (got != want).then(|| {
            format!(
                "{}:\n  got  {:?}\n  want {:?}",
                show(&v),
                got.iter()
                    .map(|u| String::from_utf16_lossy(u))
                    .collect::<Vec<_>>(),
                want.iter()
                    .map(|u| String::from_utf16_lossy(u))
                    .collect::<Vec<_>>()
            )
        })
    });
    section(&f, "blocks", 130, |c| {
        let got = provenance::framed_blocks(&units(c.get("content").unwrap()));
        let want: Vec<provenance::Block> = arr(c.get("expect"))
            .iter()
            .map(|b| {
                let b = arr(Some(b));
                let label = match &b[1] {
                    Value::Null => None,
                    v => Some(units(v)),
                };
                (units(&b[0]), label, units(&b[2]))
            })
            .collect();
        (got != want).then(|| format!("got {got:?} want {want:?}"))
    });
}

/// ingestMessages' text parts of a JSON message list.
fn parts(messages: &Value) -> Vec<Vec<u16>> {
    let mut out = Vec::new();
    for m in arr(Some(messages)) {
        match m.get("content") {
            Some(Value::Str(s)) => out.push(s.clone()),
            Some(Value::Arr(ps)) => {
                for p in ps {
                    out.push(
                        p.get("text")
                            .and_then(Value::as_str)
                            .map(<[u16]>::to_vec)
                            .unwrap_or_default(),
                    );
                }
            }
            _ => {}
        }
    }
    out
}

fn hits_value(r: &Result<Vec<provenance::Hit>, provenance::Unchecked>) -> Value {
    let s = |u: &[u16]| Value::Str(u.to_vec());
    match r {
        Err(_) => Value::Arr(vec![Value::Obj(vec![
            (prompt_framing::js::units("field"), Value::Null),
            (prompt_framing::js::units("source"), Value::Null),
            (prompt_framing::js::units("unchecked"), Value::Bool(true)),
        ])]),
        Ok(h) => Value::Arr(
            h.iter()
                .map(|h| {
                    Value::Obj(vec![
                        (prompt_framing::js::units("field"), s(&h.field)),
                        (prompt_framing::js::units("source"), s(&h.source)),
                    ])
                })
                .collect(),
        ),
    }
}

/// The store after a JSON round trip, as a host keeps it between calls.
fn round_trip(s: &Store) -> Store {
    let mut out = Vec::new();
    store_json(&mut out, s);
    store_from(Some(&json::parse_utf8(&out, 8).unwrap())).unwrap()
}

#[test]
fn stores() {
    let f = fixtures();
    section(&f, "stores", 40, |c| {
        let name = text(c.get("name").unwrap());
        let Some(Value::Num(max)) = c.get("maxChars") else {
            panic!()
        };
        let mut store = Store::new(*max as usize);
        let mut bad = Vec::new();
        for (i, step) in arr(c.get("steps")).iter().enumerate() {
            if let Some(m) = step.get("ingest") {
                store.ingest(&parts(m));
                store = round_trip(&store);
            } else if let Some(a) = step.get("add") {
                let a = arr(Some(a));
                store.add(&units(&a[0]), &units(&a[1]));
                store = round_trip(&store);
            } else if let Some(v) = step.get("source") {
                let got = store.index().source_of(&units(v));
                let want = match step.get("expect") {
                    Some(Value::Null) => None,
                    Some(v) => Some(units(v)),
                    None => panic!(),
                };
                if got != want {
                    bad.push(format!(
                        "step {i} source {}: got {got:?} want {want:?}",
                        show(&units(v))
                    ));
                }
            } else if let Some(st) = step.get("stats") {
                let s = store.stats();
                let want = (
                    st.get("chars"),
                    st.get("grams"),
                    st.get("sources"),
                    st.get("saturated"),
                );
                let got = (
                    Some(&Value::Num(s.chars as f64)),
                    Some(&Value::Num(s.grams as f64)),
                    Some(&Value::Num(s.sources as f64)),
                    Some(&Value::Bool(s.saturated)),
                );
                if got != want {
                    bad.push(format!("step {i} stats: got {s:?} want {st:?}"));
                }
            } else {
                let (args, object) = match (step.get("args"), step.get("argsJson")) {
                    (Some(a), None) => (units(a), false),
                    (None, Some(a)) => (units(a), true),
                    _ => panic!("step {i}"),
                };
                let r = provenance::parse_args(&args, object)
                    .and_then(|a| provenance::check_write(&store.index(), &a));
                let got = hits_value(&r);
                if Some(&got) != step.get("expect") {
                    bad.push(format!(
                        "step {i} check {}: got {got:?} want {:?}",
                        show(&args),
                        step.get("expect")
                    ));
                }
            }
        }
        (!bad.is_empty()).then(|| format!("{name}: {}", bad.join("\n  ")))
    });
}

/// `{"op":…,"k":<value>}` as UTF-8 JSON.
fn request(op: &str, fields: &[(&str, Vec<u8>)]) -> Vec<u8> {
    let mut out = format!("{{\"op\":\"{op}\"").into_bytes();
    for (k, v) in fields {
        out.extend_from_slice(format!(",\"{k}\":").as_bytes());
        out.extend_from_slice(v);
    }
    out.push(b'}');
    out
}

fn jstr(u: &[u16]) -> Vec<u8> {
    let mut o = Vec::new();
    json::push_str(&mut o, u);
    o
}

/// The reply as a value, the way the host reads it.
fn reply(bytes: &[u8]) -> Value {
    json::parse_utf8(bytes, 16).expect("reply is JSON")
}

/// Compare a reply with the JS result, as values (key order included).
fn same(got: &Value, want: &Value) -> bool {
    got == want
}

#[test]
fn packets() {
    let f = fixtures();
    section(&f, "packets", 200, |c| {
        let out = units(c.get("output").unwrap());
        let r = reply(&packet_call(&request("parse", &[("output", jstr(&out))])).unwrap());
        let want = c.get("expect").unwrap();
        (!same(&r, want)).then(|| format!("{}:\n  got  {r:?}\n  want {want:?}", show(&out)))
    });
    section(&f, "validate", 60, |c| {
        let j = units(c.get("json").unwrap());
        let mut req = b"{\"op\":\"validate\",\"packet\":".to_vec();
        req.extend(String::from_utf16(&j).unwrap().as_bytes());
        req.push(b'}');
        let r = reply(&packet_call(&req).unwrap());
        let want = c.get("expect").unwrap();
        (!same(&r, want)).then(|| format!("{}:\n  got  {r:?}\n  want {want:?}", show(&j)))
    });
    section(&f, "renders", 100, |c| {
        let p = units(c.get("packet").unwrap());
        let mut req = b"{\"op\":\"render\",\"packet\":".to_vec();
        req.extend(String::from_utf16_lossy(&p).as_bytes());
        req.extend_from_slice(b",\"label\":");
        req.extend(jstr(&units(c.get("label").unwrap())));
        req.push(b'}');
        let r = reply(&packet_call(&req).unwrap());
        let want = units(c.get("expect").unwrap());
        let got = r.get("text").map(units).unwrap_or_default();
        (got != want).then(|| format!("got {} want {}", show(&got), show(&want)))
    });
}

#[test]
fn provenance_requests_agree_with_the_library() {
    let f = fixtures();
    let c = &arr(f.get("stores"))[0];
    let new = reply(&provenance_call(br#"{"op":"new","maxChars":400000}"#).unwrap());
    let mut state = Vec::new();
    store_json(&mut state, &store_from(new.get("state")).unwrap());
    let step = &arr(c.get("steps"))[0];
    let mut contents = b"[".to_vec();
    for (i, p) in parts(step.get("ingest").unwrap()).iter().enumerate() {
        if i > 0 {
            contents.push(b',');
        }
        contents.extend(jstr(p));
    }
    contents.push(b']');
    let r = reply(
        &provenance_call(&request(
            "ingest",
            &[("state", state), ("contents", contents)],
        ))
        .unwrap(),
    );
    let mut state = Vec::new();
    store_json(&mut state, &store_from(r.get("state")).unwrap());
    let check = reply(
        &provenance_call(&request(
            "check",
            &[
                ("state", state.clone()),
                (
                    "args",
                    jstr(&prompt_framing::js::units(r#"{"to":"exfil@evil.io"}"#)),
                ),
                ("object", b"false".to_vec()),
            ],
        ))
        .unwrap(),
    );
    assert_eq!(arr(check.get("found")).len(), 1, "{check:?}");
    let src = reply(
        &provenance_call(&request(
            "source",
            &[
                ("state", state),
                ("value", jstr(&prompt_framing::js::units("exfil@evil.io"))),
            ],
        ))
        .unwrap(),
    );
    assert!(matches!(src.get("source"), Some(Value::Str(_))));
}
