//! The fit against the answers Node gave (tests/fixtures/record.cjs, at the Core head in `cases.json`).
#![allow(clippy::unwrap_used, clippy::indexing_slicing, clippy::panic)]
use context_fit::{sampling, thinking, Meter, Refused, Saved};
use js_json::{wtf16, JValue};

fn cases() -> JValue {
    wtf16::parse(include_str!("fixtures/cases.json")).unwrap()
}

fn arr(v: &JValue) -> &[JValue] {
    match v {
        JValue::Arr(a) => a,
        _ => &[],
    }
}

fn num(v: &JValue) -> u64 {
    v.as_num().unwrap() as u64
}

fn meter_is(m: &Meter, want: &JValue, what: &str) {
    assert_eq!(m.limit, num(want.get("limit")), "{what}");
    assert_eq!(m.used, num(want.get("used")), "{what}");
    assert_eq!(m.reserve, num(want.get("reserve")), "{what}");
    assert_eq!(m.safety, num(want.get("safety")), "{what}");
    assert_eq!(m.free, num(want.get("free")), "{what}");
    assert_eq!(
        m.threshold as f64,
        want.get("threshold").as_num().unwrap(),
        "{what}"
    );
    for (p, w) in m.parts.iter().zip(arr(want.get("parts"))) {
        assert_eq!(Some(p.name), w.get("name").as_str(), "{what}");
        assert_eq!(p.tokens, num(w.get("tokens")), "{what}");
    }
}

#[test]
fn tokens_measure_and_budgets() {
    let c = cases();
    for k in arr(c.get("tokens")) {
        assert_eq!(
            context_fit::tokens(k.get("v")),
            num(k.get("out")),
            "{:?}",
            k.get("v")
        );
    }
    for k in arr(c.get("measure")) {
        let m = context_fit::measure(arr(k.get("m")), k.get("t"), num(k.get("l")));
        meter_is(&m, k.get("out"), "measure");
    }
    for k in arr(c.get("budget")) {
        assert_eq!(thinking::output_budget(num(k.get("l"))), num(k.get("out")));
    }
    for k in arr(c.get("cap")) {
        assert_eq!(
            thinking::thinking_cap(k.get("e").as_str().unwrap(), num(k.get("b"))),
            num(k.get("out"))
        );
    }
    for k in arr(c.get("sampling")) {
        let role = k.get("role").as_str();
        let got = sampling::select(
            role,
            k.get("message").as_str().unwrap(),
            k.get("explicit"),
            k.get("autoEnabled").as_bool().unwrap(),
        );
        assert_eq!(
            wtf16::stringify(&got),
            wtf16::stringify(k.get("out")),
            "{:?}",
            k.get("message")
        );
    }
}

fn summarizer(
    mode: &str,
) -> impl FnMut(String, Vec<JValue>, u64) -> std::future::Ready<Result<JValue, Refused>> {
    let mode = mode.to_owned();
    move |sum, batch, _| {
        std::future::ready(Ok(match mode.as_str() {
            "bad" => JValue::Str("   ".into()),
            "huge" => JValue::Str("x".repeat(30000)),
            "num" => JValue::Num(7.0),
            _ => JValue::Str(format!(
                "S({}){}",
                batch.len(),
                if sum.is_empty() { "" } else { "+" }
            )),
        }))
    }
}

#[tokio::test]
async fn compaction_between_tool_rounds() {
    let c = cases();
    for k in arr(c.get("continuation")) {
        let what = format!("{:?} {:?}", k.get("name"), k.get("mode"));
        let mut status = Vec::new();
        let got = context_fit::compact_continuation(
            arr(k.get("m")),
            &JValue::Arr(Vec::new()),
            num(k.get("limit")),
            summarizer(k.get("mode").as_str().unwrap()),
            &mut |t, kind| status.push((t.to_owned(), kind.to_owned())),
        )
        .await;
        let want = k.get("out");
        match got {
            Ok(r) => {
                assert!(
                    want.get("error").as_str().is_none(),
                    "{what}: node refused {:?}",
                    want.get("error")
                );
                assert_eq!(
                    wtf16::stringify(&JValue::Arr(r.messages)),
                    wtf16::stringify(want.get("messages")),
                    "{what}"
                );
                meter_is(&r.meter, want.get("meter"), &what);
                assert_eq!(Some(r.compacted), want.get("compacted").as_bool(), "{what}");
                assert_eq!(r.covered as u64, num(want.get("covered")), "{what}");
                assert_eq!(r.truncated as u64, num(want.get("truncated")), "{what}");
            }
            Err(Refused(text)) => {
                assert_eq!(Some(text.as_str()), want.get("error").as_str(), "{what}")
            }
        }
        assert_eq!(status.len(), arr(k.get("status")).len(), "{what}");
    }
}

#[tokio::test]
async fn the_fit_of_a_whole_turn_and_the_saved_summary() {
    let c = cases();
    for k in arr(c.get("prepare")) {
        let what = format!("{:?} {:?}", k.get("name"), k.get("mode"));
        let tools = JValue::Arr(Vec::new());
        let got = context_fit::prepare(
            arr(k.get("m")),
            &tools,
            num(k.get("limit")),
            k.get("force").as_bool().unwrap(),
            &Saved::default(),
            1,
            summarizer(k.get("mode").as_str().unwrap()),
            &mut |_, _| {},
        )
        .await;
        let want = k.get("out");
        match got {
            Ok(r) => {
                assert!(
                    want.get("error").as_str().is_none(),
                    "{what}: node refused {:?}",
                    want.get("error")
                );
                assert_eq!(
                    wtf16::stringify(&JValue::Arr(r.messages.clone())),
                    wtf16::stringify(want.get("messages")),
                    "{what}"
                );
                meter_is(&r.meter, want.get("meter"), &what);
                assert_eq!(r.max_tokens, num(want.get("maxTokens")), "{what}");
                assert_eq!(r.state.covered as u64, num(want.get("stCovered")), "{what}");
                assert_eq!(
                    r.state.summary.as_deref(),
                    want.get("summary").as_str(),
                    "{what}"
                );
                assert_eq!(
                    r.state.prefix.as_deref(),
                    want.get("prefix").as_str(),
                    "{what}"
                );
                // The second pass reads the saved summary and asks for no other.
                let again = context_fit::prepare(
                    arr(k.get("m")),
                    &tools,
                    num(k.get("limit")),
                    false,
                    &r.state,
                    2,
                    |_, _, _| std::future::ready(Err(Refused("no second summary".into()))),
                    &mut |_, _| {},
                )
                .await
                .unwrap();
                assert_eq!(
                    wtf16::stringify(&JValue::Arr(again.messages)),
                    wtf16::stringify(want.get("again").get("messages")),
                    "{what}"
                );
                assert_eq!(
                    again.state.covered as u64,
                    num(want.get("again").get("covered")),
                    "{what}"
                );
            }
            Err(Refused(text)) => {
                assert_eq!(Some(text.as_str()), want.get("error").as_str(), "{what}")
            }
        }
    }
}

#[test]
fn shortening_keeps_a_frame_and_a_pair_whole() {
    // keep_head_and_tail's output is covered through the recorded tool-shrink scenarios; these two pin the
    // edges those do not reach: a surrogate pair at either cut, and a word that only starts like the frame.
    let cut = context_fit::keep_head_and_tail(&"\u{1F600}".repeat(1000), 601);
    assert!(cut.contains("[truncated"));
    assert!(!cut.contains('\u{FFFD}'));
    let framed = format!("<untrusted k=\"v\"\n{}\n</untrusted>", "x".repeat(1000));
    let cut = context_fit::keep_head_and_tail(&framed, 600);
    assert!(cut.starts_with("<untrusted k=\"v\"\nx"));
    assert!(cut.ends_with("x\n</untrusted>"));
    let unframed = format!("<untrustedly\n{}\n</untrusted>", "x".repeat(1000));
    assert!(context_fit::keep_head_and_tail(&unframed, 600).starts_with("<untrustedly\nxx"));
}
