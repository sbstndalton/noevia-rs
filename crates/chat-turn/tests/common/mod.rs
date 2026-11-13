//! The recorded Node turns (`fixtures/node-turns.v1.json`, written by `record_turns.cjs`).
#![allow(
    dead_code,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing
)]

use serde_json::Value;

pub const FIXTURE: &str = include_str!("../fixtures/node-turns.v1.json");

pub fn fixture() -> Value {
    serde_json::from_str(FIXTURE).unwrap()
}

/// Every recorded case, in recording order.
pub fn cases() -> Vec<Value> {
    fixture()["cases"].as_array().unwrap().clone()
}

pub fn case(name: &str) -> Value {
    cases()
        .into_iter()
        .find(|c| c["name"] == name)
        .unwrap_or_else(|| panic!("no recorded case {name}"))
}

/// Whether the client went away during the case (its script has a `leave` marker, or it leaves while a
/// card is open or a tool runs).
pub fn client_left(case: &Value) -> bool {
    case["input"]["leaveOnCard"] == true
        || case["input"]["leaveInTool"].is_string()
        || case["input"]["rounds"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|r| r["frames"].as_array())
            .flatten()
            .any(|f| f["leave"] == true)
}

/// A request body with its `{ "$tools": key }` reference replaced by the recorded tool list.
pub fn request_body(fixture: &Value, request: &Value) -> Value {
    let mut body = request["body"].clone();
    if let Some(key) = body["tools"]["$tools"].as_str() {
        body["tools"] = fixture["tools"][key].clone();
    }
    body
}

/// JSON equality as JavaScript sees it: numbers compare by value (Node writes `0`, serde_json may
/// hold `0.0`), objects by key set, arrays in order.
pub fn same_json(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Number(x), Value::Number(y)) => x.as_f64() == y.as_f64(),
        (Value::Array(x), Value::Array(y)) => {
            x.len() == y.len() && x.iter().zip(y).all(|(p, q)| same_json(p, q))
        }
        (Value::Object(x), Value::Object(y)) => {
            x.len() == y.len()
                && x.iter()
                    .all(|(k, v)| y.get(k).is_some_and(|w| same_json(v, w)))
        }
        _ => a == b,
    }
}

/// The length JavaScript gives a string: UTF-16 code units.
pub fn js_len(s: &str) -> usize {
    s.encode_utf16().count()
}
