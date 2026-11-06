//! A running reply's partial saves (core chat-partial.cjs #1507, #2146) against the files core's own
//! code wrote for the same synthetic replies (fixtures/record-partial-saves.cjs).
#![allow(clippy::unwrap_used, clippy::indexing_slicing, clippy::panic)]
use js_json::{wtf16, JValue};
use server_projects::partial::{self, Saves};
use sha2::{Digest, Sha256};
use std::os::unix::fs::PermissionsExt;

fn big() -> String {
    format!("{}{}{}", "a".repeat(65530), "😀".repeat(8), "z".repeat(3))
}

fn expand(v: &JValue) -> JValue {
    match v {
        JValue::Str(s) if s == "<BIG>" => JValue::Str(big()),
        JValue::Arr(a) => JValue::Arr(a.iter().map(expand).collect()),
        JValue::Obj(o) => JValue::Obj(o.iter().map(|(k, x)| (k.clone(), expand(x))).collect()),
        other => other.clone(),
    }
}

fn file(dir: &std::path::Path, id: &str) -> JValue {
    match std::fs::read(partial::file_for(dir, id)) {
        Err(_) => JValue::Null,
        Ok(bytes) => {
            let sha: String = Sha256::digest(&bytes)
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect();
            let mut o = vec![
                ("sha256".to_owned(), JValue::Str(sha)),
                ("bytes".to_owned(), JValue::Num(bytes.len() as f64)),
            ];
            if bytes.len() < 2000 {
                o.push((
                    "text".to_owned(),
                    JValue::Str(String::from_utf8(bytes).unwrap()),
                ));
            }
            JValue::Obj(o)
        }
    }
}

fn arr(v: &JValue) -> &[JValue] {
    match v {
        JValue::Arr(a) => a,
        _ => &[],
    }
}

fn num(v: &JValue) -> f64 {
    match v {
        JValue::Num(n) => *n,
        _ => panic!("{v:?}"),
    }
}

#[test]
fn partial_saves_write_what_node_wrote() {
    let c = wtf16::parse(include_str!("fixtures/partial-saves-cases.json")).unwrap();
    let start = c.get("start").as_str().unwrap().to_owned();
    for k in arr(c.get("cases")) {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        let id = k.get("id").as_str().unwrap();
        let what = wtf16::stringify(k).unwrap();
        let what = what.get(..600).unwrap_or(&what);
        if let Some(earlier) = k.get("earlier").as_str() {
            let f = partial::file_for(dir, id);
            std::fs::create_dir_all(f.parent().unwrap()).unwrap();
            std::fs::write(f, earlier).unwrap();
        }
        let steps = arr(k.get("steps"));
        let mut saves = Some(Saves::start(dir, id, &start).unwrap());
        assert_eq!(file(dir, id), *steps[0].get("file"), "start {what}");
        for step in steps {
            match step.get("op").as_str().unwrap() {
                "tick" => {
                    let entry = expand(step.get("entry"));
                    saves.as_ref().unwrap().save(
                        (!entry.is_nullish()).then_some(&entry),
                        num(step.get("now")),
                    );
                    assert_eq!(file(dir, id), *step.get("file"), "tick {what}");
                }
                "finish" => {
                    let keep = expand(step.get("keep"));
                    let stuck = step.get("stuck") == &JValue::Bool(true);
                    let folder = partial::file_for(dir, id).parent().unwrap().to_owned();
                    if stuck {
                        std::fs::set_permissions(&folder, std::fs::Permissions::from_mode(0o500))
                            .unwrap();
                    }
                    saves
                        .take()
                        .unwrap()
                        .finish((!keep.is_nullish()).then_some(&keep), num(step.get("now")));
                    if stuck {
                        std::fs::set_permissions(&folder, std::fs::Permissions::from_mode(0o700))
                            .unwrap();
                    }
                    assert_eq!(file(dir, id), *step.get("file"), "finish {what}");
                }
                // A tick after the end: the saves are gone, nothing is written.
                _ => assert_eq!(file(dir, id), *step.get("file"), "late {what}"),
            }
        }
    }
}

#[test]
fn a_reply_dropped_without_finish_leaves_no_running_marker() {
    let tmp = tempfile::tempdir().unwrap();
    let start = "0123456789abcdef0123456789abcdef";
    let saves = Saves::start(tmp.path(), "c", start).unwrap();
    let entry = js_json::parse(r#"{"role":"assistant","content":"so far"}"#).unwrap();
    saves.save(Some(&entry), 1.0);
    let saved = partial::read_partial(tmp.path(), "c").unwrap();
    assert_eq!(partial::state(&saved, start), partial::State::Running);
    drop(saves);
    assert!(
        partial::read_partial(tmp.path(), "c").is_none(),
        "a dropped reply's partial stays marked running"
    );
}
