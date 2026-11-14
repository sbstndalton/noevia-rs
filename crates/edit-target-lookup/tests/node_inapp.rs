//! Track R14 step 6: `edit_target_lookup::inapp` against noevia-core's own in-app MCP server and its
//! project write tools (mcp-internal.cjs createHandler over mcp-internal-tools.cjs), recorded by
//! `tools/gen-inapp-writes-fixtures.cjs`: per `tools/call` the result the server answers and the
//! write the handler asked for. Every result and every write must be the same, as JSON.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use edit_target_lookup::inapp::{finish, prepare, Prepared};
use js_json::{wtf16, JValue};

fn fixture() -> JValue {
    wtf16::parse(include_str!("fixtures/node-inapp-writes.v1.json")).unwrap()
}

fn arr(v: &JValue) -> &[JValue] {
    match v {
        JValue::Arr(items) => items,
        other => panic!("not a list: {other:?}"),
    }
}

fn json(v: &JValue) -> String {
    wtf16::stringify(v).unwrap()
}

fn inline(file: &JValue) -> String {
    file.get("content").as_str().unwrap_or_default().to_owned()
}

/// The write as the recorder wrote it: `{ name, text, options }` (options `null` for a create,
/// keys left out where core's value was `undefined`).
fn write_json(w: &edit_target_lookup::inapp::Write) -> String {
    let options = match &w.options {
        None => JValue::Null,
        Some(o) => {
            let mut fields = vec![
                ("expectName".to_owned(), JValue::Str(o.expect_name.clone())),
                (
                    "expectContent".to_owned(),
                    JValue::Str(o.expect_content.clone()),
                ),
                ("expectAttachment".to_owned(), o.expect_attachment.clone()),
            ];
            if let Some(t) = &o.adopt_to {
                fields.push(("adoptTo".to_owned(), JValue::Str(t.clone())));
            }
            if let Some(a) = &o.adopt_account {
                fields.push(("adoptAccount".to_owned(), JValue::Str(a.clone())));
            }
            JValue::Obj(fields)
        }
    };
    json(&JValue::obj([
        ("name", JValue::Str(w.name.clone())),
        ("text", JValue::Str(w.text.clone())),
        ("options", options),
    ]))
}

#[test]
fn the_fixture_names_its_node_sources() {
    let doc = fixture();
    for f in [
        "server/mcp-internal.cjs",
        "server/mcp-internal-tools.cjs",
        "server/project-edit-target.cjs",
        "server/project-file-names.cjs",
    ] {
        assert_eq!(
            doc.get("sourcesRead").get(f).as_str().unwrap().len(),
            64,
            "{f}"
        );
    }
    assert!(arr(doc.get("cases")).len() >= 50);
}

#[test]
fn every_call_answers_as_core() {
    let doc = fixture();
    let mut writes = 0;
    for c in arr(doc.get("cases")) {
        let name = c.get("name").as_str().unwrap();
        let node = c.get("node");
        assert_eq!(
            node.get("status").as_num(),
            Some(200.0),
            "{name}: an rpc error is not recorded here"
        );
        let project = c.get("project").as_str().map(|p| wtf16::parse(p).unwrap());
        let args = wtf16::parse(c.get("args").as_str().unwrap()).unwrap();
        let got = prepare(
            c.get("tool").as_str().unwrap(),
            &args,
            project.as_ref(),
            c.get("account").as_str(),
            node.get("pin").as_str(),
            &inline,
        );
        let want_result = node.get("result").as_str().unwrap();
        let want_writes = arr(node.get("writes"));
        match got {
            Prepared::Answer(result) => {
                assert!(want_writes.is_empty(), "{name}: core wrote");
                assert_eq!(json(&result), want_result, "{name}: result");
            }
            Prepared::Write(w) => {
                assert_eq!(want_writes.len(), 1, "{name}: core made no write");
                assert_eq!(
                    write_json(&w),
                    want_writes[0].as_str().unwrap(),
                    "{name}: write"
                );
                let outcome = match c.get("writeFails").as_str() {
                    Some(m) => Err(m.to_owned()),
                    None => Ok(()),
                };
                assert_eq!(
                    json(&finish(&w.success, outcome)),
                    want_result,
                    "{name}: result"
                );
                writes += 1;
            }
            Prepared::Unsupported => panic!("{name}: unsupported"),
        }
    }
    assert!(writes >= 20, "{writes}");
}

#[test]
fn shapes_core_never_stores_go_to_core() {
    let project = wtf16::parse(r#"{"id":"p","files":"abc"}"#).unwrap();
    let args = wtf16::parse(r#"{"name":"a.md","text":"x"}"#).unwrap();
    assert_eq!(
        prepare(
            "project_create_file",
            &args,
            Some(&project),
            None,
            None,
            &inline
        ),
        Prepared::Unsupported
    );
    let project = wtf16::parse(r#"{"id":"p","files":[]}"#).unwrap();
    assert_eq!(
        prepare(
            "diary_append",
            &args_obj(),
            Some(&project),
            None,
            None,
            &inline
        ),
        Prepared::Unsupported
    );
}

fn args_obj() -> JValue {
    JValue::Obj(Vec::new())
}
