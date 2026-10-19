//! Source-bound incremental ABI tests. Full normal/opaque/reference/read-order reports run
//! through the actual built WASM against the immutable independent Node oracle in core.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]
use completeness_report::call;
use prompt_framing::json::{self, Value};
fn ask(body: &str) -> (u32, Value) {
    let mut input = vec![2];
    input.extend(body.as_bytes());
    let (status, text) = call(&input);
    (
        status,
        json::parse_utf8(text.as_bytes(), 32).expect("reply"),
    )
}
fn text(value: &Value) -> String {
    String::from_utf16(value.as_str().expect("string")).expect("UTF16")
}
fn encode(value: &Value) -> String {
    let mut out = vec![];
    tool_exchange::canonical(&mut out, value).unwrap();
    String::from_utf16(&out).unwrap()
}
#[test]
fn metadata_and_original_opcode_remain_distinct() {
    let (status, reply) = ask(r#"{"version":true}"#);
    assert_eq!(status, 0);
    assert_eq!(reply.get("frameLimit"), Some(&Value::Num(65536.0)));
    let Value::Arr(names) = reply.get("checks").unwrap() else {
        panic!("names")
    };
    assert_eq!(names.len(), 5);
    assert_eq!(text(&names[0]), "tests-run");
    let (status, reply) = call(b"\x01[{},null]");
    assert_eq!(status, 0);
    assert!(reply.contains("\"report\""));
}
#[test]
fn build_requests_lazy_fields_without_serializing_the_job() {
    let (status, reply) = ask(
        r#"{"state":{"program":0,"pc":0,"registers":[{"ref":0,"kind":"object","truth":true}]}}"#,
    );
    assert_eq!(status, 0);
    let req = reply.get("request").unwrap();
    assert_eq!(text(req.get("operation").unwrap()), "get");
    assert_eq!(text(req.get("key").unwrap().get("literal").unwrap()), "job");
    let state = reply.get("continuation").unwrap();
    let pc = state.get("pc").unwrap();
    assert!(matches!(pc,Value::Num(n)if *n>0.0));
    let resume = format!(
        r#"{{"state":{},"answer":{{"ref":1,"kind":"object","truth":true}}}}"#,
        encode(state)
    );
    let (status, next) = ask(&resume);
    assert_eq!(status, 0);
    assert_eq!(
        text(
            next.get("request")
                .unwrap()
                .get("key")
                .unwrap()
                .get("literal")
                .unwrap()
        ),
        "expectedArtifacts"
    );
    assert!(
        matches!((pc,next.get("continuation").unwrap().get("pc").unwrap()),(Value::Num(a),Value::Num(b))if b>a)
    );
}
#[test]
fn rust_guard_and_callback_decide_while_raw_falsy_identity_survives() {
    let (_, reply) = ask(
        r#"{"state":{"program":6,"pc":0,"registers":[{"ref":0,"kind":"null","truth":false}]}}"#,
    );
    assert_eq!(
        reply.get("request").unwrap().get("value"),
        Some(&Value::Bool(false))
    );
    let resume = format!(
        r#"{{"state":{},"answer":{{"ref":1,"kind":"boolean","truth":false}}}}"#,
        encode(reply.get("continuation").unwrap())
    );
    assert_eq!(
        ask(&resume).1.get("done").unwrap().get("ref"),
        Some(&Value::Num(1.0))
    );
    let (_, reply) = ask(
        r#"{"state":{"program":11,"pc":0,"registers":[{"ref":7,"kind":"number","truth":false}]}}"#,
    );
    assert_eq!(
        reply.get("done").unwrap().get("ref"),
        Some(&Value::Num(7.0)),
        "a && predicate returns original falsy a"
    );
    let (_, reply) = ask(
        r#"{"state":{"program":10,"pc":0,"registers":[{"ref":0,"kind":"object","truth":true}]}}"#,
    );
    let resume = format!(
        r#"{{"state":{},"answer":{{"ref":1,"kind":"string","truth":true,"prefix":"tests","length":5}}}}"#,
        encode(reply.get("continuation").unwrap())
    );
    assert_eq!(
        ask(&resume).1.get("request").unwrap().get("value"),
        Some(&Value::Bool(true))
    );
}
#[test]
fn malformed_frames_and_unbounded_transport_are_refused() {
    for body in [
        "{}",
        r#"{"state":{"program":99,"pc":0,"registers":[]}}"#,
        r#"{"state":{"program":0,"pc":99999,"registers":[]}}"#,
        r#"{"state":{"program":0,"pc":0,"registers":[{"ref":0,"kind":"invented","truth":true}]}}"#,
        r#"{"state":{"program":0,"pc":0,"registers":[{"ref":-1,"kind":"object","truth":true}]}}"#,
        r#"{"state":{"program":0,"pc":0,"registers":[{"ref":0,"kind":"string","truth":true,"prefix":"x","length":null}]}}"#,
        r#"{"state":{"program":0,"pc":0,"registers":[],"destination":32},"answer":{"ref":0,"kind":"null","truth":false}}"#,
        r#"{"state":{"program":0,"pc":0,"registers":[],"destination":0}}"#,
    ] {
        assert_eq!(ask(body).0, 1, "{body}");
    }
    let oversized = format!(r#"{{"ignored":"{}"}}"#, "x".repeat(65537));
    let (status, reply) = ask(&oversized);
    assert_eq!(status, 1);
    assert_eq!(text(reply.get("error").unwrap()), "too_large");
}
