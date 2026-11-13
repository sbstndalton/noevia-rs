//! Node's own answers from `fixtures/autotune-start.node.json` (recorded by
//! `fixtures/record-node.cjs` from noevia-core `llamacpp-full-autotune.cjs` `start`; the source's
//! sha256 is in the file) replayed against [`autotune_start::admit`].
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use autotune_start::{admit, Mode, Plan, Start};
use js_json::JValue;

fn recording() -> JValue {
    let text = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/autotune-start.node.json"
    ))
    .expect("fixture file");
    js_json::parse(&text).expect("fixture JSON")
}

fn present<'a>(input: &'a JValue, key: &str) -> Option<&'a JValue> {
    let v = input.get(key);
    (!matches!(v, JValue::Undefined)).then_some(v)
}

/// What the route does with a body Node throws on while coercing it (an own non-callable
/// `toString`: `Number()` raises a TypeError, which reaches the router as an internal error). The
/// native admission refuses it as any other value that is not a number: a named difference.
const NAMED_DIFFERENCE_THROWS: (u16, &str) = (
    400,
    "Choose a prompt time limit between 15 and 1800 seconds.",
);

#[test]
fn every_case_matches_the_recorded_node_answer() {
    let f = recording();
    assert_eq!(f.get("version").as_num(), Some(2.0));
    let sha = f.get("sourceSha256").as_str().expect("source sha");
    assert_eq!(sha.len(), 64, "source sha256");
    let JValue::Arr(cases) = f.get("cases") else {
        panic!("cases array")
    };
    assert!(
        cases.len() >= 70,
        "fixture case count shrank: {}",
        cases.len()
    );
    let (mut admitted, mut refused, mut throws) = (0, 0, 0);
    for case in cases {
        let name = case.get("name").as_str().expect("name");
        let input = case.get("input");
        let start = Start {
            confirm_pause: present(input, "confirmPause"),
            mode: present(input, "mode"),
            prompt_budget_seconds: present(input, "promptBudgetSeconds"),
            // routes/models.cjs: `untuned: body?.untuned === true`.
            bulk: input.get("untuned") == &JValue::Bool(true),
            run_active: input.get("runActive") == &JValue::Bool(true),
            system_model: input.get("systemModel") == &JValue::Bool(true),
        };
        let got = admit(&start).map_err(|r| (r.status, r.message.to_owned()));
        let node = case.get("node");
        if !matches!(node.get("throws"), JValue::Undefined) {
            throws += 1;
            let (status, message) = NAMED_DIFFERENCE_THROWS;
            assert_eq!(got, Err((status, message.to_owned())), "{name}");
            continue;
        }
        let status = node.get("status").as_num().expect("status") as u16;
        if status == 202 {
            admitted += 1;
            let mode = match node.get("mode").as_str() {
                Some("fast") => Mode::Fast,
                Some("long") => Mode::Long,
                other => panic!("{name}: recorded mode {other:?}"),
            };
            let seconds = node.get("promptBudgetSeconds").as_num().expect("seconds") as u32;
            assert_eq!(
                got,
                Ok(Plan {
                    mode,
                    prompt_budget_seconds: seconds
                }),
                "{name}"
            );
        } else {
            refused += 1;
            let message = node.get("error").as_str().expect("error");
            assert_eq!(got, Err((status, message.to_owned())), "{name}");
        }
    }
    assert!(
        admitted >= 20 && refused >= 40 && throws == 1,
        "{admitted} {refused} {throws}"
    );
}
