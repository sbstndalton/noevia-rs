//! sbstndalton/noevia#1071 slice 4: the transcript sanitiser (src/history_sanitize.rs) against
//! core's own `sanitizeHistory`, case for case: tests/fixtures/node-history-sanitize.v1.json,
//! recorded by tools/gen-history-sanitize-fixtures.cjs from the core commit it names. Synthetic
//! data only.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use js_json::{wtf16, JValue};
use server_projects::history_sanitize::sanitize_history;

#[test]
fn every_recorded_case_matches_core() {
    let text = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/node-history-sanitize.v1.json"
    ))
    .unwrap();
    let fixture = wtf16::parse(&text).unwrap();
    let JValue::Arr(cases) = fixture.get("cases") else {
        panic!("no cases")
    };
    assert!(cases.len() > 400, "{}", cases.len());
    let mut threw = 0;
    for case in cases {
        let name = case.get("name").as_str().unwrap();
        let JValue::Arr(input) = wtf16::parse(case.get("input").as_str().unwrap()).unwrap() else {
            panic!("{name}: input is not a list")
        };
        let got = sanitize_history(&input);
        if case.get("threw").as_bool() == Some(true) {
            threw += 1;
            assert!(got.is_err(), "{name}: core threw, the port did not");
            continue;
        }
        let got = got.unwrap_or_else(|_| panic!("{name}: the port threw, core did not"));
        assert_eq!(
            wtf16::stringify(&JValue::Arr(got)).unwrap(),
            case.get("output").as_str().unwrap(),
            "{name}"
        );
    }
    assert!(threw > 0, "the fixture exercises Node's throws");
}
