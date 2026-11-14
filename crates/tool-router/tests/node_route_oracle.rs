//! core's own `route` answers, recorded by `tests/record_route.cjs` into
//! `tests/fixtures/node-route.v1.json`, replayed through [`tool_router::route`] and compared as the
//! `JSON.stringify` text. Until the fixture is recorded (on the build host or in CI) the test reports
//! that it compared nothing; with `TOOL_ROUTER_ORACLE_REQUIRED=1` a missing fixture fails.
#![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]

use js_json::JValue;
use tool_router::{route, Input, ToolBox, DEFAULT_THRESHOLD, DEFAULT_TOP_K};

fn strings(v: &JValue) -> Vec<String> {
    match v {
        JValue::Arr(items) => items
            .iter()
            .map(|s| s.as_str().unwrap().to_owned())
            .collect(),
        _ => vec![],
    }
}

fn numbers(v: &JValue) -> Vec<f64> {
    match v {
        JValue::Arr(items) => items.iter().map(|n| n.as_num().unwrap()).collect(),
        _ => panic!("an embedding array: {v:?}"),
    }
}

fn num_or(v: &JValue, default: f64) -> f64 {
    v.as_num().unwrap_or(default)
}

#[test]
fn replays_core_route_answers() {
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/node-route.v1.json"
    );
    let Ok(text) = std::fs::read_to_string(path) else {
        assert!(
            std::env::var("TOOL_ROUTER_ORACLE_REQUIRED").as_deref() != Ok("1"),
            "TOOL_ROUTER_ORACLE_REQUIRED=1 but {path} is missing: record it with tests/record_route.cjs"
        );
        eprintln!("COUNTS tool-router oracle: 0 compared (fixture not recorded)");
        return;
    };
    let fixture = js_json::parse(&text).unwrap();
    let JValue::Arr(cases) = fixture.get("cases") else {
        panic!("no cases")
    };
    let mut compared = 0;
    let mut diffs = vec![];
    for case in cases {
        let input = case.get("input");
        let JValue::Arr(rows) = input.get("boxes") else {
            panic!("rows")
        };
        let boxes: Vec<ToolBox> = rows
            .iter()
            .map(|r| ToolBox {
                id: r.get("id").as_str().unwrap().to_owned(),
                tools: strings(r.get("tools")),
                never_auto_load: r.get("autoLoad").as_str() == Some("never"),
                requires: strings(r.get("requires")),
                embedding: match r.get("embedding") {
                    JValue::Undefined => None,
                    e => Some(numbers(e)),
                },
            })
            .collect();
        let not_offered = input.get("notOffered").as_str().map(str::to_owned);
        let offered = move |id: &str| not_offered.as_deref() != Some(id);
        let task = match input.get("taskEmbedding") {
            JValue::Arr(_) => Some(numbers(input.get("taskEmbedding"))),
            _ => None,
        };
        let user = strings(input.get("userSelection"));
        let fallback = strings(input.get("fallback"));
        let out = route(&Input {
            boxes: &boxes,
            task_embedding: task.as_deref(),
            offered: &offered,
            user_selection: &user,
            fallback: &fallback,
            max_tools: num_or(input.get("maxTools"), f64::NAN),
            top_k: num_or(input.get("topK"), DEFAULT_TOP_K),
            threshold: num_or(input.get("threshold"), DEFAULT_THRESHOLD),
        });
        let got = js_json::stringify(&out.to_json()).unwrap();
        let want = case.get("output").as_str().unwrap();
        if got != want {
            diffs.push(format!(
                "{}: got {got}\n  want {want}",
                case.get("name").as_str().unwrap_or("?")
            ));
        }
        compared += 1;
    }
    eprintln!(
        "COUNTS tool-router oracle: {compared} compared, {} differ",
        diffs.len()
    );
    assert!(diffs.is_empty(), "{}", diffs.join("\n"));
    assert!(compared >= 300, "the recorder writes 300 cases");
}
