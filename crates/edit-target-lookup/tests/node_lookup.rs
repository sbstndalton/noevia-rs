//! Track R14 step 3: `edit_target_lookup` against noevia-core's own project-edit-target.cjs
//! `resolveEditTarget` and `targetDigest`, recorded by `tools/gen-edit-target-fixtures.cjs` with the
//! dav-parse.wasm core pins (synthetic QA projects; a source's text inline). Every answer must be
//! the same, byte for byte: the path, the bound account and the digest, or the error text.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use edit_target_lookup::{resolve_edit_target, target_digest, Lookup};
use js_json::{wtf16, JValue};

fn fixture() -> JValue {
    wtf16::parse(include_str!("fixtures/node-edit-target.v1.json")).unwrap()
}

fn arr(v: &JValue) -> &[JValue] {
    match v {
        JValue::Arr(items) => items,
        other => panic!("not a list: {other:?}"),
    }
}

/// The text `file.content` reads in the recording: inline only.
fn inline(file: &JValue) -> String {
    file.get("content").as_str().unwrap_or_default().to_owned()
}

#[test]
fn the_fixture_names_its_node_sources() {
    let doc = fixture();
    for f in [
        "server/project-edit-target.cjs",
        "server/project-file-names.cjs",
        "server/source-readability.cjs",
        "server/quoted-name.cjs",
        "server/uploads.cjs",
        "server/dav-parse-wasm.cjs",
    ] {
        assert_eq!(
            doc.get("sourcesRead").get(f).as_str().unwrap().len(),
            64,
            "{f}"
        );
    }
    assert_eq!(doc.get("wasmSha256").as_str().unwrap().len(), 64);
    assert!(arr(doc.get("cases")).len() > 140);
}

#[test]
fn every_lookup_answers_as_core() {
    let doc = fixture();
    let (mut targets, mut refusals) = (0, 0);
    for (i, c) in arr(doc.get("cases")).iter().enumerate() {
        let project = c.get("project").as_str().map(|p| wtf16::parse(p).unwrap());
        let args = c.get("args").as_str().unwrap();
        let account = c.get("account").as_str();
        let node = c.get("node");
        let got = resolve_edit_target(project.as_ref(), args, account, &inline);
        let case = format!("case {i}: {args} / {account:?}");
        match node.get("error").as_str() {
            Some(error) => {
                assert_eq!(got, Lookup::Refused(error.to_owned()), "{case}");
                refusals += 1;
            }
            None => {
                let Lookup::Target { path, account } = got else {
                    panic!("{case}: {got:?}")
                };
                assert_eq!(path, node.get("path").as_str().unwrap(), "{case}");
                assert_eq!(account.as_deref(), node.get("account").as_str(), "{case}");
                assert_eq!(
                    target_digest(&path, account.as_deref()),
                    node.get("digest").as_str().unwrap(),
                    "{case}"
                );
                targets += 1;
            }
        }
    }
    assert!(targets > 20 && refusals > 60, "{targets} {refusals}");
}

#[test]
fn digests_answer_as_core() {
    let doc = fixture();
    for d in arr(doc.get("digests")) {
        let path = d.get("path").as_str().unwrap();
        let account = d.get("account").as_str();
        assert_eq!(
            target_digest(path, account),
            d.get("digest").as_str().unwrap(),
            "{path:?} {account:?}"
        );
    }
}

#[test]
fn shapes_core_never_stores_ask_core() {
    let project = wtf16::parse(r#"{"projectFolder":5,"files":[{"name":"a.txt"}]}"#).unwrap();
    assert_eq!(
        resolve_edit_target(Some(&project), r#"{"name":"a.txt"}"#, None, &inline),
        Lookup::Unsupported
    );
    let project = wtf16::parse(r#"{"files":[{"name":"a.txt","source":true}]}"#).unwrap();
    assert_eq!(
        resolve_edit_target(Some(&project), r#"{"name":"a.txt"}"#, None, &inline),
        Lookup::Unsupported
    );
    // Arguments JSON.parse takes and wtf16::parse refuses.
    let project = wtf16::parse(r#"{"files":[{"name":"a.txt"}]}"#).unwrap();
    for args in [
        r#"{"name":1e400}"#,
        "{\"name\":\"\u{10f800}\"}",
        &format!("{}{}", "[".repeat(200), "]".repeat(200)),
    ] {
        assert_eq!(
            resolve_edit_target(Some(&project), args, None, &inline),
            Lookup::Unsupported,
            "{args}"
        );
    }
}
