//! A running reply's partial saves (core chat-partial.cjs #1507, #2146) against the files core's own
//! code wrote for the same synthetic replies (fixtures/record-partial-saves.cjs).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]
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

const START: &str = "0123456789abcdef0123456789abcdef";

fn entry(text: &str) -> JValue {
    js_json::parse(&format!(
        r#"{{"role":"assistant","content":"{text}","tools":[{{"name":"nc_notes_write","applied":true}}]}}"#
    ))
    .unwrap()
}

#[test]
fn a_reply_dropped_without_finish_keeps_its_last_entry_as_interrupted() {
    let tmp = tempfile::tempdir().unwrap();
    let saves = Saves::start(tmp.path(), "c", START).unwrap();
    saves.save(Some(&entry("so far")), 1.0);
    let saved = partial::read_partial(tmp.path(), "c").unwrap();
    assert_eq!(partial::state(&saved, START), partial::State::Running);
    // Cancelled before its end ran: no transcript copy was made.
    drop(saves);
    let kept = partial::read_partial(tmp.path(), "c").expect("the only copy of the reply survives");
    assert_eq!(partial::state(&kept, START), partial::State::Interrupted);
    assert_eq!(kept.get("content").as_str(), Some("so far"));
    assert_eq!(
        kept.get("tools"),
        entry("x").get("tools"),
        "the applied write is kept"
    );
    assert_eq!(kept.get("interrupted"), &JValue::Bool(true));
    assert_eq!(kept.get("running"), &JValue::Undefined);
    // A turn that saved nothing leaves nothing behind.
    drop(Saves::start(tmp.path(), "d", START).unwrap());
    assert!(partial::read_partial(tmp.path(), "d").is_none());
}

#[test]
fn a_superseded_turn_neither_publishes_nor_cleans_up() {
    let tmp = tempfile::tempdir().unwrap();
    let a = Saves::start(tmp.path(), "c", START).unwrap();
    a.save(Some(&entry("a1")), 1.0);
    let b = Saves::start(tmp.path(), "c", START).unwrap();
    assert!(
        partial::read_partial(tmp.path(), "c").is_none(),
        "b supersedes a's partial"
    );
    b.save(Some(&entry("b1")), 2.0);
    a.save(Some(&entry("a2")), 3.0);
    let now = partial::read_partial(tmp.path(), "c").unwrap();
    assert_eq!(
        now.get("content").as_str(),
        Some("b1"),
        "a late tick of a does not overwrite b"
    );
    a.finish(None, 4.0);
    assert_eq!(
        partial::read_partial(tmp.path(), "c")
            .unwrap()
            .get("content")
            .as_str(),
        Some("b1"),
        "a's end leaves b's file"
    );
    let c = Saves::start(tmp.path(), "c", START).unwrap();
    drop(b);
    assert!(
        partial::read_partial(tmp.path(), "c").is_none(),
        "b's drop does not keep its entry over c"
    );
    c.finish(None, 5.0);
}

#[test]
fn concurrent_ticks_of_one_turn_never_share_or_leave_a_temporary_file() {
    let tmp = tempfile::tempdir().unwrap();
    let saves = std::sync::Arc::new(Saves::start(tmp.path(), "c", START).unwrap());
    let threads: Vec<_> = (0..8)
        .map(|t| {
            let saves = std::sync::Arc::clone(&saves);
            std::thread::spawn(move || {
                for k in 0..40 {
                    saves.save(Some(&entry(&format!("t{t}-{k}"))), f64::from(k));
                }
            })
        })
        .collect();
    for t in threads {
        t.join().unwrap();
    }
    let saved = partial::read_partial(tmp.path(), "c").expect("a whole file");
    assert!(
        saved.get("content").as_str().unwrap().starts_with('t'),
        "{saved:?}"
    );
    let folder = partial::file_for(tmp.path(), "c")
        .parent()
        .unwrap()
        .to_owned();
    let names: Vec<String> = std::fs::read_dir(&folder)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(names.len(), 1, "no temporary file is left: {names:?}");
    std::sync::Arc::try_unwrap(saves)
        .ok()
        .unwrap()
        .finish(None, 50.0);
    assert!(partial::read_partial(tmp.path(), "c").is_none());
}

#[test]
fn a_chat_deleted_during_its_turn_gets_no_late_partial() {
    let tmp = tempfile::tempdir().unwrap();
    let saves = Saves::start(tmp.path(), "c", START).unwrap();
    saves.save(Some(&entry("before")), 1.0);
    // The store's delete: supersede, then remove the file.
    partial::supersede(tmp.path(), "c");
    std::fs::remove_file(partial::file_for(tmp.path(), "c")).unwrap();
    saves.save(Some(&entry("late")), 2.0);
    assert!(
        partial::read_partial(tmp.path(), "c").is_none(),
        "a late tick re-created the deleted chat's partial"
    );
    drop(saves);
    assert!(
        partial::read_partial(tmp.path(), "c").is_none(),
        "a drop after the delete keeps nothing"
    );
}

#[test]
fn a_deleted_account_directory_is_never_recreated() {
    let tmp = tempfile::tempdir().unwrap();
    let user = tmp.path().join("u");
    std::fs::create_dir(&user).unwrap();
    let saves = Saves::start(&user, "c", START).unwrap();
    std::fs::remove_dir_all(&user).unwrap();
    saves.save(Some(&entry("late")), 1.0);
    drop(saves);
    assert!(!user.exists());
}
