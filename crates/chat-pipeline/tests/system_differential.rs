//! chat-pipeline `system` against core chat.cjs's system message and its helpers (tests/fixtures/record-system.cjs).
#![allow(clippy::expect_used, clippy::indexing_slicing, clippy::panic)]
use chat_pipeline::system::{self as sys, Inputs, PartialRead};
use js_json::{wtf16, JValue};
use sha2::{Digest, Sha256};

fn arr(v: &JValue) -> &[JValue] {
    match v {
        JValue::Arr(a) => a,
        _ => &[],
    }
}

fn by_index(all: &JValue, ix: &JValue) -> Vec<JValue> {
    arr(ix).iter().map(|k| arr(all)[k.as_num().expect("index") as usize].clone()).collect()
}

fn opt(v: &JValue) -> Option<&str> {
    v.as_str()
}

fn sha(t: &str) -> String {
    Sha256::digest(t.as_bytes()).iter().map(|b| format!("{b:02x}")).collect()
}

fn reads(v: &JValue) -> Vec<PartialRead> {
    arr(v)
        .iter()
        .map(|p| PartialRead {
            name: p.get("name").as_str().unwrap_or("").to_owned(),
            length: p.get("length").as_num().unwrap_or(0.0),
            read: p.get("read").as_num().unwrap_or(0.0),
        })
        .collect()
}

#[test]
fn helpers_match_node() {
    let c = wtf16::parse(include_str!("fixtures/system-cases.json")).expect("fixture");
    let u = c.get("units");
    for q in arr(u.get("quote")) {
        assert_eq!(Some(sys::quote_name(arr(q)[0].as_str().unwrap_or(""))).as_deref(), arr(q)[1].as_str(), "{q:?}");
    }
    for q in arr(u.get("index")) {
        let skills = by_index(c.get("SKILLS"), &arr(q)[0]);
        assert_eq!(Some(sys::format_skill_index(&skills).as_str()), arr(q)[1].as_str(), "{q:?}");
    }
    for q in arr(u.get("waiting")) {
        let files = JValue::Arr(by_index(c.get("FILES"), &arr(q)[0]));
        assert_eq!(Some(sys::waiting_notice(&files).as_str()), arr(q)[1].as_str(), "{q:?}");
        let names: Vec<JValue> = sys::readable(&files).iter().map(|f| f.get("name").clone()).collect();
        assert_eq!(&JValue::Arr(names), &arr(q)[2], "readable {q:?}");
    }
    for q in arr(u.get("coverage")) {
        assert_eq!(Some(sys::coverage_note(&reads(&arr(q)[0])).as_str()), arr(q)[1].as_str(), "{q:?}");
    }
    for q in arr(u.get("skillBlock")) {
        assert_eq!(Some(sys::skill_block(std::slice::from_ref(&arr(q)[0])).as_str()), arr(q)[1].as_str(), "{q:?}");
    }
    for q in arr(u.get("pinned")) {
        let r = &arr(q)[0];
        let got = sys::pinned_skill_block(r.get("record"), r.get("content").as_str().unwrap_or(""));
        assert_eq!(Some(got.as_str()), arr(q)[1].as_str(), "{q:?}");
    }
    for q in arr(u.get("tasks")) {
        assert_eq!(Some(sys::code_tasks_part(arr(&arr(q)[0])).as_str()), arr(q)[1].as_str(), "{q:?}");
    }
}

#[test]
fn the_system_message_matches_node() {
    let c = wtf16::parse(include_str!("fixtures/system-cases.json")).expect("fixture");
    let files_all = c.get("FILES");
    let skills_all = c.get("SKILLS");
    let mut n = 0;
    for (i, k) in arr(c.get("cases")).iter().enumerate() {
        let out = k.get("out");
        let inp = out.get("inputs");
        let up = out.get("upstream");
        let project = match k.get("project") {
            JValue::Obj(items) => {
                let mut items = items.clone();
                wtf16::set(&mut items, "files", JValue::Arr(by_index(files_all, k.get("project").get("files"))));
                JValue::Obj(items)
            }
            _ => JValue::Null,
        };
        let has_project = project.truthy();
        let has_files = !arr(project.get("files")).is_empty();
        let skills = if has_project { by_index(skills_all, k.get("skills")) } else { Vec::new() };
        let auto = by_index(skills_all, inp.get("autoSkills"));
        let tasks = arr(inp.get("codeTasks")).to_vec();
        let not_ready = if has_project { arr(k.get("notReady")).to_vec() } else { Vec::new() };
        let partial = reads(inp.get("partialReads"));
        let files_block = if has_files { opt(k.get("filesBlock")) } else { None };
        let attach = k.get("kind").as_str() == Some("attach");
        let s = sys::assemble(&Inputs {
            account_part: opt(inp.get("accountPart")),
            time_part: inp.get("timePart").as_str().unwrap_or(""),
            memory_part: opt(inp.get("memoryPart")),
            project: &project,
            chat_attachments: attach,
            compact_only: false,
            code_tasks: matches!(inp.get("codeTasks"), JValue::Arr(_)).then_some(tasks.as_slice()),
            files_block,
            sources_not_ready: &not_ready,
            partial_reads: &partial,
            skills: &skills,
            auto_skills: &auto,
            pinned: None,
            frame_block: None,
            brain_block: None,
        });
        let off_box = has_project && k.get("providerId").as_str() == Some("ext");
        let text = if off_box { s.off_box(inp.get("zoneless").as_str().unwrap_or("")) } else { s.text() };
        if let Some(full) = up.get("system").as_str() {
            assert_eq!(text, full, "system {i}");
        }
        assert_eq!(Some(sha(&text).as_str()), up.get("systemSha").as_str(), "system {i}: {k:?}");
        let message = k.get("message").as_str().unwrap_or("");
        let mut msgs = vec![JValue::obj([("role", JValue::Str("user".into())), ("content", JValue::Str(message.into()))])];
        if let (true, Some(fb)) = (attach, files_block) {
            sys::with_attached_files(&mut msgs, message, fb);
        }
        assert_eq!(msgs[0].get("content"), up.get("last").get("content"), "user turn {i}");
        n += 1;
    }
    assert_eq!(n, 900);
}
