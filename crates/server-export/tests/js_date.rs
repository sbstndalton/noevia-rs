//! `new Date(x).toISOString()` against Node's own answers: every case of fixtures/node-dates.v1.json
//! (made by js-date-oracle.cjs with CI's Node, in UTC) through `js_date::time_value` and `iso`.
#![allow(clippy::unwrap_used, clippy::panic)]
use js_json::JValue;
use server_export::js_date::{self, Error};

#[test]
fn every_node_date_case_matches() {
    // NOEVIA_DATES_FIXTURE: an oracle run made in this process's own zone (another TZ, say).
    let path = std::env::var("NOEVIA_DATES_FIXTURE").ok();
    // Zone-less strings read in local time; the committed fixture is UTC's answer.
    if path.is_none() && js_date::parse("2026-01-01T00:00") != Some(1_767_225_600_000.0) {
        // A skip that nobody reads must not pass on CI: there the zone is set, or the fixture is.
        assert!(
            std::env::var_os("CI").is_none(),
            "this process does not run in UTC and no NOEVIA_DATES_FIXTURE (an oracle run in its own zone) was given"
        );
        eprintln!("skipped: this process does not run in UTC (CI sets TZ=UTC)");
        return;
    }
    let text = std::fs::read_to_string(path.unwrap_or_else(|| {
        concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/node-dates.v1.json"
        )
        .into()
    }))
    .unwrap();
    let fx = js_json::wtf16::parse(&text).unwrap();
    let JValue::Arr(cases) = fx.get("cases") else {
        panic!("no cases")
    };
    assert!(cases.len() > 3000, "{}", cases.len());
    let mut wrong = Vec::new();
    for c in cases {
        let v = c.get("v");
        let got = match js_date::time_value(v) {
            Ok(t) => js_date::iso(t),
            Err(Error::Invalid) => "RangeError".into(),
            Err(Error::Type) => "TypeError".into(),
        };
        let want = c
            .get("iso")
            .as_str()
            .or(c.get("error").as_str())
            .unwrap()
            .to_owned();
        if got != want {
            wrong.push(format!("{:?}: {got} != {want}", js_json::stringify(v)));
        }
    }
    assert!(
        wrong.is_empty(),
        "{} of {}:\n{}",
        wrong.len(),
        cases.len(),
        wrong.join("\n")
    );
}
