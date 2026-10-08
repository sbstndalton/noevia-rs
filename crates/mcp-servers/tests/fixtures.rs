//! Every row of tests/fixtures/mcp-servers.v1.json (printed by noevia-core's
//! tools/gen-mcp-servers-fixtures.cjs from the JS itself; byte-identical to core's copy) through
//! the wasm call's wire format and the library: the same servers and the same warnings in the same
//! order (a bearer warning rendered as the host renders it), the same box filter and the same
//! toolboxOffered; the strict rows are refused as ambiguous.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use mcp_servers::{call, parse_servers, Refusal};
use prompt_framing::js::units;
use prompt_framing::json::{self, Value};

fn fixtures() -> Value {
    json::parse_utf8(include_bytes!("fixtures/mcp-servers.v1.json"), 16).expect("fixture JSON")
}

fn rows<'a>(f: &'a Value, key: &str) -> &'a [Value] {
    let Some(Value::Arr(rows)) = f.get(key) else {
        panic!("no {key}")
    };
    rows
}

fn wire(op: u8, row: &Value) -> Vec<u8> {
    let w = String::from_utf16(row.get("wire").and_then(Value::as_str).unwrap()).unwrap();
    let mut input = vec![op];
    input.extend(w.as_bytes());
    input
}

fn opt(v: Option<&Value>) -> Option<Vec<u16>> {
    match v {
        Some(Value::Str(s)) if !s.is_empty() => Some(s.clone()),
        _ => None,
    }
}

/// The reply's warnings as the host prints them, given the variables that are set.
fn render(warnings: &Value, set: &[Vec<u16>]) -> Vec<Value> {
    let Value::Arr(ws) = warnings else { panic!() };
    ws.iter()
        .filter_map(|w| match w {
            Value::Str(_) => Some(w.clone()),
            Value::Obj(_) => {
                let env = w.get("bearer").and_then(Value::as_str).unwrap().to_vec();
                let id = w.get("id").and_then(Value::as_str).unwrap();
                (!set.contains(&env)).then(|| {
                    let mut t = units("[mcp] server \"");
                    t.extend(id);
                    t.extend(units("\": "));
                    t.extend(&env);
                    t.extend(units(" is not set, so its tools will not authenticate"));
                    Value::Str(t)
                })
            }
            _ => panic!("warning shape"),
        })
        .collect()
}

#[test]
fn parse_rows() {
    let f = fixtures();
    let rows = rows(&f, "parse");
    assert!(rows.len() > 100);
    let (mut servers, mut warnings) = (0, 0);
    for (i, row) in rows.iter().enumerate() {
        let (status, reply) = call(&wire(1, row));
        assert_eq!(status, 0, "row {i}: {reply}");
        let reply = json::parse_utf8(reply.as_bytes(), 16).unwrap();
        let want = row.get("want").unwrap();
        assert_eq!(
            reply.get("servers"),
            want.get("servers"),
            "row {i}: servers"
        );
        let Some(Value::Arr(set)) = row.get("set") else {
            panic!()
        };
        let set: Vec<Vec<u16>> = set.iter().map(|s| s.as_str().unwrap().to_vec()).collect();
        let got = render(reply.get("warnings").unwrap(), &set);
        let Some(Value::Arr(want_w)) = want.get("warnings") else {
            panic!()
        };
        assert_eq!(&got, want_w, "row {i}: warnings");
        let Some(Value::Arr(s)) = want.get("servers") else {
            panic!()
        };
        servers += s.len();
        warnings += want_w.len();
        // The library agrees with the wire.
        let lib = parse_servers(
            opt(row.get("servers")).as_deref(),
            opt(row.get("url")).as_deref(),
        )
        .unwrap();
        assert_eq!(lib.servers.len(), s.len(), "row {i}");
    }
    assert!(
        servers > 60 && warnings > 50,
        "{servers} servers, {warnings} warnings"
    );
}

#[test]
fn strict_rows_are_refused() {
    let f = fixtures();
    let rows = rows(&f, "strict");
    assert!(rows.len() >= 10);
    for (i, row) in rows.iter().enumerate() {
        assert_eq!(
            call(&wire(1, row)),
            (1, Refusal::Ambiguous.json().to_owned()),
            "row {i}"
        );
    }
}

#[test]
fn toolbox_rows() {
    let f = fixtures();
    for (i, row) in rows(&f, "toolboxes").iter().enumerate() {
        let (status, reply) = call(&wire(2, row));
        assert_eq!(status, 0, "row {i}");
        let reply = json::parse_utf8(reply.as_bytes(), 16).unwrap();
        assert_eq!(reply.get("enabled"), row.get("want"), "row {i}");
    }
    let offered = rows(&f, "offered");
    assert!(offered.len() >= 60);
    for (i, row) in offered.iter().enumerate() {
        let (status, reply) = call(&wire(3, row));
        assert_eq!(status, 0, "row {i}");
        let want = row.get("want") == Some(&Value::Bool(true));
        assert_eq!(reply, format!("{{\"offered\":{want}}}"), "row {i}");
    }
}
