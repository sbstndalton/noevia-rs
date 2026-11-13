//! The runner's pure helpers against noevia-core's own exports, recorded by
//! `tests/pure/record-node.cjs` into `tests/pure/pure.node.json` (source sha256 in the file).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use autotune_runner::{kv, phases, quality};
use js_json::{wtf16, JValue};

fn recording() -> JValue {
    let text = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/pure/pure.node.json"
    ))
    .expect("fixture file");
    wtf16::parse(&text).expect("fixture JSON")
}

fn arr(v: &JValue) -> &[JValue] {
    match v {
        JValue::Arr(items) => items,
        other => panic!("not an array: {other:?}"),
    }
}

fn block_on<F: std::future::Future>(f: F) -> F::Output {
    // The quality check never really waits (its chat is a scripted reply): poll it to the end.
    let waker = std::task::Waker::noop();
    let mut cx = std::task::Context::from_waker(waker);
    let mut f = std::pin::pin!(f);
    loop {
        if let std::task::Poll::Ready(v) = f.as_mut().poll(&mut cx) {
            return v;
        }
    }
}

#[test]
fn probes_and_constants() {
    let f = recording();
    assert_eq!(f.get("sourceSha256").as_str().map(str::len), Some(64));
    let probes = arr(f.get("quality"));
    assert_eq!(probes.len(), quality::QUALITY.len());
    for (q, p) in quality::QUALITY.iter().zip(probes) {
        assert_eq!(p.get("id").as_str(), Some(q.id));
        assert_eq!(p.get("prompt").as_str(), Some(q.prompt));
        assert_eq!(p.get("expected").as_str(), Some(q.expected));
    }
}

#[test]
fn quality_check_matches_node() {
    let f = recording();
    let cases = arr(f.get("qualityCases"));
    assert!(cases.len() >= 100);
    for case in cases {
        let name = case.get("name").as_str().unwrap();
        let replies = arr(case.get("replies"));
        let baseline = match case.get("baseline") {
            JValue::Null => None,
            b => Some(b),
        };
        let asked = arr(case.get("asked"));
        let budget = asked
            .first()
            .and_then(|a| a.get("max").as_num())
            .unwrap_or(64.0) as u32;
        let mut seen = Vec::new();
        let result = block_on(quality::quality_check::<()>(
            budget,
            baseline,
            async |prompt, max| {
                seen.push((prompt, max));
                let i = quality::QUALITY
                    .iter()
                    .position(|q| q.prompt == prompt)
                    .unwrap();
                Ok(replies[i].clone())
            },
        ))
        .unwrap();
        let want_asked: Vec<(&str, u32)> = asked
            .iter()
            .map(|a| {
                (
                    a.get("prompt").as_str().unwrap(),
                    a.get("max").as_num().unwrap() as u32,
                )
            })
            .collect();
        assert_eq!(seen, want_asked, "{name}: probes asked");
        assert_eq!(
            wtf16::stringify(&result),
            wtf16::stringify(case.get("result")),
            "{name}: result"
        );
        assert_eq!(
            quality::quality_failure(&result),
            case.get("failure").as_str().unwrap(),
            "{name}: failure text"
        );
    }
}

#[test]
fn kv_candidates_match_node() {
    let f = recording();
    for case in arr(f.get("kv")) {
        let env = case.get("env").as_str();
        // kvCandidates checks `allowQ5Kv === true`.
        let allow_q5 = case.get("allowQ5Kv") == &JValue::Bool(true);
        let got: Vec<JValue> = kv::candidates(allow_q5, kv::allow_below_q5(env))
            .into_iter()
            .map(JValue::from)
            .collect();
        assert_eq!(&JValue::Arr(got), case.get("result"), "{case:?}");
    }
}

#[test]
fn prefer_kv_matches_node() {
    let f = recording();
    for case in arr(f.get("prefer")) {
        let passed: Vec<String> = arr(case.get("passed"))
            .iter()
            .map(|v| v.as_str().unwrap().to_string())
            .collect();
        let ceilings = match case.get("ceilings") {
            JValue::Null => None,
            c => Some(c),
        };
        let got = kv::prefer(&passed, ceilings);
        assert_eq!(JValue::from(got), *case.get("result"), "{case:?}");
    }
}

#[test]
fn bf16_unsupported_matches_node() {
    let f = recording();
    for case in arr(f.get("bf16")) {
        let got = kv::bf16_unsupported(case.get("text").as_str());
        assert_eq!(JValue::Bool(got), *case.get("result"), "{case:?}");
    }
}

#[test]
fn new_model_matches_node() {
    let f = recording();
    for case in arr(f.get("models")) {
        let planned = case.get("planned") == &JValue::Bool(true);
        let got = phases::new_model(case.get("model"), planned, arr(case.get("kv")));
        assert_eq!(
            wtf16::stringify(&got),
            wtf16::stringify(case.get("result")),
            "{case:?}"
        );
    }
}
