//! Additive diagnostic projection, plus every frozen old-operation row.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]
use prompt_framing::json::{self, Value};
use task_lifecycle::call;
fn ask(body: &str) -> (u32, Value) {
    let mut input = vec![6];
    input.extend(body.as_bytes());
    let (status, reply) = call(&input);
    (
        status,
        json::parse_utf8(reply.as_bytes(), 16).expect("diagnostic JSON"),
    )
}
fn text(value: &Value) -> String {
    String::from_utf16(value.as_str().unwrap()).unwrap()
}
fn legacy_answer(value: Value) -> Value {
    let Value::Obj(mut fields) = value else {
        panic!("reply object");
    };
    fields.retain(|(key, _)| String::from_utf16(key).unwrap() != "diagnostic");
    Value::Obj(fields)
}
#[test]
fn every_frozen_answer_is_unchanged_by_diagnostics() {
    let fixture = json::parse_utf8(include_bytes!("fixtures/task-lifecycle.v1.json"), 64).unwrap();
    for (section, ops) in [
        (
            "pairs",
            vec![(1, "canTransition"), (2, "transition"), (3, "stageMove")],
        ),
        ("folds", vec![(4, "")]),
        ("derive", vec![(5, "")]),
    ] {
        let Value::Arr(rows) = fixture.get(section).unwrap() else {
            panic!("rows");
        };
        for (index, row) in rows.iter().enumerate() {
            let wire = text(row.get("wire").unwrap());
            let tail = wire.strip_prefix('[').unwrap();
            for (op, key) in &ops {
                let body = format!("[{op},{tail}");
                let (status, reply) = ask(&body);
                assert_eq!(status, 0, "{section} {index}");
                let want = row.get("want").unwrap();
                let want = if key.is_empty() {
                    want
                } else {
                    want.get(key).unwrap()
                };
                assert_eq!(&legacy_answer(reply), want, "{section} {index} {op}");
            }
        }
    }
}
#[test]
fn fold_errors_identify_the_original_failure_once() {
    for (body, index, current) in [
        (r#"[5,[{"type":"job.completed"}]]"#, 0.0, "planned"),
        (
            r#"[5,[{"type":"job.started"},{"type":"job.failed"},{"type":"job.completed"}]]"#,
            2.0,
            "blocked",
        ),
    ] {
        let (status, reply) = ask(body);
        assert_eq!(status, 0);
        assert_eq!(reply.get("throws").map(text).as_deref(), Some("illegal"));
        let diag = reply.get("diagnostic").unwrap();
        assert_eq!(diag.get("event"), Some(&Value::Num(index)));
        assert_eq!(diag.get("current").map(text).as_deref(), Some(current));
        assert_eq!(diag.get("target").map(text).as_deref(), Some("verifying"));
    }
    let (_, reply) = ask(
        r#"[5,[{"type":"task.stage","data":{"from":"planned","to":"implementing"}},{"type":"task.stage","data":{"from":"planned","to":"verifying"}}]]"#,
    );
    assert_eq!(reply.get("throws").map(text).as_deref(), Some("stale"));
    assert_eq!(
        reply.get("diagnostic").unwrap().get("event"),
        Some(&Value::Num(1.0))
    );
}
#[test]
fn metadata_and_raw_unchanged_step_are_policy_derived() {
    let (status, reply) = ask("[0]");
    assert_eq!(status, 0);
    let Value::Arr(states) = reply.get("states").unwrap() else {
        panic!("states");
    };
    assert_eq!(states.len(), 7);
    assert_eq!(reply.get("initial").map(text).as_deref(), Some("planned"));
    assert_eq!(
        ask(r#"[6,null,{"type":"ignored"},false,false]"#)
            .1
            .get("unchanged"),
        Some(&Value::Bool(true))
    );
    assert_eq!(
        ask(r#"[6,null,{"type":"job.started"},false,false]"#)
            .1
            .get("diagnostic")
            .unwrap()
            .get("site")
            .map(text)
            .as_deref(),
        Some("from")
    );
    assert_eq!(
        ask(r#"[6,"planned",{"type":"job.started"},false,false]"#)
            .1
            .get("state")
            .map(text)
            .as_deref(),
        Some("implementing")
    );
}

#[test]
fn identity_projection_cannot_bypass_rust_stale_state_check() {
    let (_, reply) = ask(
        r#"[6,"planned",{"type":"task.stage","data":{"from":"wrong","to":"blocked"}},true,true]"#,
    );
    assert_eq!(reply.get("throws").map(text).as_deref(), Some("stale"));
}

#[test]
fn raw_advance_identity_precedes_validation_but_cannot_forge_known_states() {
    for body in [r#"[7,null,null,true]"#, r#"[7,"planned","planned",false]"#] {
        let (status, reply) = ask(body);
        assert_eq!(status, 0);
        assert_eq!(reply.get("unchanged"), Some(&Value::Bool(true)));
    }
    for body in [
        r#"[7,"planned","verifying",true]"#,
        r#"[7,"planned",null,true]"#,
        r#"[7,null,"planned",true]"#,
        r#"[7,null,null,false]"#,
    ] {
        let (status, reply) = ask(body);
        assert_eq!(status, 0);
        assert!(reply.get("throws").is_some(), "{body}");
    }
    let (_, reply) = ask(r#"[7,"planned","implementing",true]"#);
    assert_eq!(
        reply.get("state").map(text).as_deref(),
        Some("implementing")
    );
}
