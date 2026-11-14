//! The recorded Node answers (`fixtures/node-document-read.v1.json`, written by
//! `record_document_read.cjs` from noevia-core's document-sources.cjs): every notice, problem,
//! note list, note text, problemNotes and readPages answer must be the one core gave.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use document_read::{
    documents_dir, note_text, notes, notice, problem, problem_notes, read_pages, stored_result,
    ReadError, Thrown, Unreadable, READ_CAP,
};
use js_json::{wtf16, JValue};
use std::path::PathBuf;

const FIXTURE: &str = include_str!("fixtures/node-document-read.v1.json");

fn fixture() -> JValue {
    wtf16::parse(FIXTURE).expect("the fixture is JSON")
}

fn items<'a>(v: &'a JValue, key: &str) -> &'a [JValue] {
    match v.get(key) {
        JValue::Arr(items) => items,
        other => panic!("{key} is not an array: {other:?}"),
    }
}

fn show(v: &JValue) -> String {
    wtf16::stringify(v).unwrap_or_else(|| "undefined".into())
}

/// What the port answered, in the recorder's shape.
fn recorded<T>(r: Result<T, Thrown>, ok: impl FnOnce(T) -> JValue) -> JValue {
    match r {
        Ok(v) => JValue::obj([("ok", ok(v))]),
        Err(Thrown) => JValue::obj([("error", JValue::Str("thrown".into()))]),
    }
}

fn read_recorded(r: Result<document_read::Pages, ReadError>) -> JValue {
    match r {
        Ok(p) => JValue::obj([("ok", p.to_json())]),
        Err(ReadError::Status(status, message)) => JValue::obj([
            ("status", JValue::Num(f64::from(status))),
            ("message", JValue::Str(message.into())),
        ]),
        Err(e) => JValue::obj([(
            "error",
            JValue::Str(
                match e {
                    ReadError::InvalidVersion => "invalid",
                    ReadError::Unreadable => "unreadable",
                    _ => "thrown",
                }
                .into(),
            ),
        )]),
    }
}

struct Failures(Vec<String>);

impl Failures {
    fn check(&mut self, what: &str, input: &JValue, want: &JValue, got: &JValue) {
        // Compared as JSON text: what Node's JSON.stringify of the answer wrote.
        if show(want) != show(got) {
            self.0.push(format!(
                "{what} of {}\n  node: {}\n  port: {}",
                show(input),
                show(want),
                show(got)
            ));
        }
    }

    fn finish(self, total: usize) {
        assert!(
            self.0.is_empty(),
            "{} of {total} answers differ; first ones:\n{}",
            self.0.len(),
            self.0
                .iter()
                .take(8)
                .cloned()
                .collect::<Vec<_>>()
                .join("\n")
        );
    }
}

#[test]
fn file_answers_match_node() {
    let f = fixture();
    let mut fails = Failures(Vec::new());
    let cases = items(&f, "files");
    assert!(cases.len() > 250, "the recording has its random records");
    for case in cases {
        let file = case.get("file");
        let str_ok = |s: String| JValue::Str(s);
        let arr_ok = |v: Vec<JValue>| JValue::Arr(v);
        fails.check(
            "notice",
            file,
            case.get("notice"),
            &recorded(notice(file), str_ok),
        );
        fails.check(
            "problem",
            file,
            case.get("problem"),
            &recorded(problem(file, true), str_ok),
        );
        fails.check(
            "problem without notes",
            file,
            case.get("problemBase"),
            &recorded(problem(file, false), str_ok),
        );
        fails.check(
            "notes",
            file,
            case.get("notes"),
            &recorded(notes(file), arr_ok),
        );
        fails.check(
            "problemNotes",
            file,
            case.get("problemNotes"),
            &recorded(problem_notes(file), |v| v),
        );
    }
    fails.finish(cases.len() * 5);
}

#[test]
fn note_texts_match_node() {
    let f = fixture();
    let mut fails = Failures(Vec::new());
    let cases = items(&f, "notes");
    for case in cases {
        let note = case.get("note");
        fails.check(
            "noteText",
            note,
            case.get("text"),
            &recorded(note_text(note), JValue::Str),
        );
    }
    fails.finish(cases.len());
}

/// The recorded stored results, written where document-sources.cjs keeps them.
fn data_dir(f: &JValue) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "qa-document-read-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    let project = f.get("project").as_str().unwrap();
    let docs = documents_dir(&dir, project);
    std::fs::create_dir_all(&docs).unwrap();
    let JValue::Obj(stored) = f.get("stored") else {
        panic!("stored is an object")
    };
    for (name, body) in stored {
        let version = f.get("versions").get(name).as_str().unwrap();
        // The body as Node wrote it (fs.writeFileSync of a JS string: UTF-8, lone surrogates as U+FFFD).
        let text: String = body
            .as_str()
            .unwrap()
            .chars()
            .map(|c| {
                if wtf16::lone_unit(c).is_some() {
                    '\u{fffd}'
                } else {
                    c
                }
            })
            .collect();
        std::fs::write(docs.join(format!("{version}.json")), text).unwrap();
    }
    dir
}

#[test]
fn read_pages_matches_node() {
    let f = fixture();
    let dir = data_dir(&f);
    let project = f.get("project").as_str().unwrap();
    let mut fails = Failures(Vec::new());
    let cases = items(&f, "pages");
    for case in cases {
        let file = case.get("file");
        let args = case.get("args");
        let cap = args.get("cap").as_num().map_or(READ_CAP, |c| c as usize);
        let got = read_pages(
            file,
            args.get("start"),
            args.get("end"),
            args.get("offset"),
            cap,
            |version| stored_result(&dir, project, version),
        );
        let input = JValue::obj([("file", file.clone()), ("args", args.clone())]);
        fails.check("readPages", &input, case.get("result"), &read_recorded(got));
    }
    let _ = std::fs::remove_dir_all(&dir);
    fails.finish(cases.len());
}

#[test]
fn stored_result_refuses_a_symlink_and_a_bad_version() {
    let dir = std::env::temp_dir().join(format!("qa-document-read-link-{}", std::process::id()));
    let docs = documents_dir(&dir, "qa-p");
    std::fs::create_dir_all(&docs).unwrap();
    let real = docs.join("real.json");
    std::fs::write(&real, "{\"pages\":1}").unwrap();
    let v = "c".repeat(64);
    std::os::unix::fs::symlink(&real, docs.join(format!("{v}.json"))).unwrap();
    assert_eq!(stored_result(&dir, "qa-p", &v), Err(Unreadable));
    assert_eq!(stored_result(&dir, "qa-p", "../real"), Err(Unreadable));
    let w = "d".repeat(64);
    std::fs::write(docs.join(format!("{w}.json")), "{\"pages\":1}").unwrap();
    assert_eq!(
        stored_result(&dir, "qa-p", &w),
        Ok(JValue::obj([("pages", JValue::Num(1.0))]))
    );
    let _ = std::fs::remove_dir_all(&dir);
}
