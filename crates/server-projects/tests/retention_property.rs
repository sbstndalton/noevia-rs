//! sbstndalton/noevia#1071: the retention expiry rule (chat-retention.cjs `expired`) over generated
//! chat lists.
//!
//! - Always: a longer period never makes more chats due; every due chat is unpinned, dated and
//!   older than the cutoff, and every such chat is due; free chats come first.
//! - With NOEVIA_CORE_CHECKOUT (CI, the job that boots core): the same generator's cases also run
//!   through core's own `expired()` (`tools/retention-expired-oracle.cjs`), and every result (or
//!   throw) must be Node's, byte for byte as JSON.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use js_json::JValue;
use proptest::prelude::*;
use proptest::strategy::ValueTree;
use proptest::test_runner::TestRunner;
use serde_json::{json, Value};
use server_projects::retention::{expired, Thrown, PERIODS};

const NOW: f64 = 1_700_000_000_123.0;
const DAY: f64 = 86_400_000.0;

fn updated() -> impl Strategy<Value = Value> {
    prop_oneof![
        4 => (0i64..500).prop_map(|d| json!(NOW - d as f64 * DAY)),
        // Exactly at a cutoff, and a millisecond either side.
        2 => (prop::sample::select(&PERIODS[..]), -1i64..=1)
            .prop_map(|(p, e)| json!(NOW - f64::from(p) * DAY + e as f64)),
        1 => Just(json!("1")),
        1 => Just(Value::Null),
        1 => Just(json!(1.5)),
    ]
}

fn chat() -> impl Strategy<Value = Value> {
    let obj = (
        prop::option::of(prop_oneof![
            "qa-[a-z]{1,3}".prop_map(Value::String),
            Just(json!(7))
        ]),
        prop::option::of(updated()),
        prop::option::of(prop_oneof![
            Just(json!(true)),
            Just(json!(false)),
            Just(json!(1)),
            Just(json!("")),
            Just(json!("yes")),
            Just(Value::Null)
        ]),
    )
        .prop_map(|(id, at, pinned)| {
            let mut m = serde_json::Map::new();
            if let Some(id) = id {
                m.insert("id".into(), id);
            }
            if let Some(at) = at {
                m.insert("updatedAt".into(), at);
            }
            if let Some(p) = pinned {
                m.insert("pinned".into(), p);
            }
            Value::Object(m)
        });
    prop_oneof![
        8 => obj,
        1 => Just(Value::Null),
        1 => Just(json!("stray")),
        1 => Just(json!(0)),
        1 => Just(json!([])),
    ]
}

fn project() -> impl Strategy<Value = Value> {
    let obj = (
        prop::option::of(prop_oneof![
            "qa-p[a-z]{0,2}".prop_map(Value::String),
            Just(json!(3))
        ]),
        prop_oneof![
            6 => prop::collection::vec(chat(), 0..5).prop_map(Value::Array),
            1 => Just(Value::Null),
            1 => Just(json!(0)),
            1 => Just(json!("")),
            1 => Just(json!("x")),
            1 => Just(json!({"a": 1})),
            1 => Just(json!(true)),
        ],
        any::<bool>(),
    )
        .prop_map(|(id, chats, has_chats)| {
            let mut m = serde_json::Map::new();
            if let Some(id) = id {
                m.insert("id".into(), id);
            }
            if has_chats {
                m.insert("chats".into(), chats);
            }
            Value::Object(m)
        });
    prop_oneof![
        10 => obj,
        1 => Just(Value::Null),
        1 => Just(json!(5)),
        1 => Just(json!([])),
    ]
}

#[derive(Debug, Clone)]
struct Case {
    free: Vec<Value>,
    projects: Vec<Value>,
    days: u32,
}

fn case() -> impl Strategy<Value = Case> {
    (
        prop::collection::vec(chat(), 0..6),
        prop::collection::vec(project(), 0..4),
        prop::sample::select(&[0u32, 30, 90, 365][..]),
    )
        .prop_map(|(free, projects, days)| Case {
            free,
            projects,
            days,
        })
}

fn jv(v: &[Value]) -> Vec<JValue> {
    v.iter()
        .map(|x| js_json::parse(&x.to_string()).unwrap())
        .collect()
}

fn run(c: &Case, days: u32) -> Result<String, Thrown> {
    let due = expired(&jv(&c.free), &jv(&c.projects), days, NOW)?;
    let arr = JValue::Arr(due.iter().map(|d| d.to_json()).collect());
    Ok(js_json::wtf16::stringify(&arr).unwrap())
}

fn is_old(chat: &Value, cutoff: f64) -> bool {
    let truthy = |v: &Value| match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().is_some_and(|f| f != 0.0),
        Value::String(s) => !s.is_empty(),
        _ => true,
    };
    truthy(chat)
        && !chat.get("pinned").is_some_and(truthy)
        && chat
            .get("updatedAt")
            .and_then(Value::as_f64)
            .is_some_and(|t| t < cutoff)
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(500))]

    #[test]
    fn expiry_rule_holds(c in case()) {
        let mut last: Option<usize> = None;
        for days in PERIODS {
            let Ok(due) = expired(&jv(&c.free), &jv(&c.projects), days, NOW) else {
                // A throw is a null project or a truthy non-list `chats`, whatever the period.
                prop_assert!(c.projects.iter().any(|p| p.is_null()
                    || p.get("chats").is_some_and(|ch| !ch.is_array()
                        && !matches!(ch, Value::Null | Value::Bool(false))
                        && ch != &json!(0) && ch != &json!(""))));
                return Ok(());
            };
            if let Some(prev) = last {
                prop_assert!(due.len() <= prev);
            }
            last = Some(due.len());
            let cutoff = NOW - f64::from(days) * DAY;
            let free_due = c.free.iter().filter(|ch| is_old(ch, cutoff)).count();
            prop_assert!(due[..free_due].iter().all(|d| d.project_id == JValue::Null));
            let in_projects: usize = c.projects.iter()
                .filter_map(|p| p.get("chats").and_then(Value::as_array))
                .map(|chats| chats.iter().filter(|ch| is_old(ch, cutoff)).count())
                .sum();
            prop_assert_eq!(due.len(), free_due + in_projects);
        }
        prop_assert_eq!(expired(&jv(&c.free), &jv(&c.projects), 0, NOW).map(|d| d.len()).unwrap_or(0), 0);
    }
}

/// The generated cases through core's `expired()` as well (needs a core checkout and node; CI sets
/// NOEVIA_CORE_CHECKOUT).
#[test]
fn generated_cases_match_node() {
    let Ok(core) = std::env::var("NOEVIA_CORE_CHECKOUT") else {
        eprintln!("NOEVIA_CORE_CHECKOUT not set: skipped");
        return;
    };
    let mut runner = TestRunner::deterministic();
    let strategy = case();
    let cases: Vec<Case> = (0..600)
        .map(|_| strategy.new_tree(&mut runner).unwrap().current())
        .collect();
    let input: Vec<Value> = cases
        .iter()
        .map(|c| json!({"freeChats": c.free, "projects": c.projects, "days": c.days, "now": NOW}))
        .collect();
    let dir = tempfile::tempdir().unwrap();
    let cases_file = dir.path().join("cases.json");
    let out_file = dir.path().join("out.json");
    std::fs::write(&cases_file, serde_json::to_string(&input).unwrap()).unwrap();
    let oracle = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../tools/retention-expired-oracle.cjs");
    let status = std::process::Command::new("node")
        .arg(&oracle)
        .arg(&core)
        .arg(&cases_file)
        .arg(&out_file)
        .status()
        .expect("node on PATH");
    assert!(status.success());
    let node: Value = serde_json::from_str(&std::fs::read_to_string(&out_file).unwrap()).unwrap();
    let results = node["results"].as_array().unwrap();
    assert_eq!(results.len(), cases.len());
    let mut threw = 0;
    for (i, (c, n)) in cases.iter().zip(results).enumerate() {
        match run(c, c.days) {
            Ok(text) => assert_eq!(Some(text.as_str()), n["due"].as_str(), "case {i}: {c:?}"),
            Err(Thrown) => {
                threw += 1;
                assert_eq!(n["threw"], json!(true), "case {i}: {c:?}");
            }
        }
    }
    // The generator reaches both outcomes.
    assert!(threw > 0 && threw < cases.len());
}
