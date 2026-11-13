//! crates/code-harness files: every read and write core's own code-harness.cjs `defaultFiles` made
//! in tests/fixtures/files-node.v1.json (tools/gen-files-fixtures.cjs: a synthetic tree full of
//! links, hard links and directories), replayed through the Rust port in a fresh tree: each answer
//! and the whole tree after each step must match.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]

use code_harness::files;
use js_json::{wtf16, JValue};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

/// core a72ae94's code-harness.cjs: a fixture regenerated from another source fails until updated.
const PINNED: &str = "ec017f978fbe11b4905e7964f5959a251db362de6f41b29c8a13d66cb03f6fe6";
const FIXTURE: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/files-node.v1.json"
);

fn walk(base: &Path, rel: &str, b: &str) -> Vec<JValue> {
    let mut out = Vec::new();
    let Ok(entries) = fs::read_dir(base.join(rel)) else {
        return out;
    };
    let mut entries: Vec<_> = entries.flatten().collect();
    entries.sort_by_key(|e| e.file_name());
    for e in entries {
        let name = e.file_name().into_string().unwrap();
        let r = if rel.is_empty() {
            name.clone()
        } else {
            format!("{rel}/{name}")
        };
        let full = base.join(&r);
        let kind = e.file_type().unwrap();
        if kind.is_symlink() {
            let to = fs::read_link(&full)
                .unwrap()
                .to_str()
                .unwrap()
                .replace(b, "<BASE>");
            out.push(JValue::Str(format!("{r} -> {to}")));
        } else if kind.is_dir() {
            out.push(JValue::Str(format!("{r}/")));
            out.extend(walk(base, &r, b));
        } else {
            let text = String::from_utf8_lossy(&fs::read(&full).unwrap()).into_owned();
            let mode = fs::metadata(&full).unwrap().permissions().mode() & 0o777;
            out.push(JValue::Str(format!(
                "{r} = {} ({mode:o})",
                wtf16::stringify(&JValue::Str(text)).unwrap()
            )));
        }
    }
    out
}

#[test]
fn every_node_read_and_write_replays() {
    let f = wtf16::parse(&fs::read_to_string(FIXTURE).unwrap()).unwrap();
    assert_eq!(
        f.get("sources").get("code-harness.cjs").as_str(),
        Some(PINNED)
    );
    // The fixture was recorded with umask 022; files the port creates must match it.
    rustix::process::umask(rustix::fs::Mode::from_raw_mode(0o022));
    let dir = tempfile::tempdir().unwrap();
    let base = fs::canonicalize(dir.path()).unwrap();
    let b = base.to_str().unwrap().to_string();
    let at = |p: &str| base.join(p).to_str().unwrap().to_string();
    let JValue::Arr(setup) = f.get("setup") else {
        panic!()
    };
    for s in setup {
        if let Some(d) = s.get("dir").as_str() {
            fs::create_dir_all(at(d)).unwrap();
        } else if let Some(p) = s.get("file").as_str() {
            fs::write(at(p), s.get("text").as_str().unwrap()).unwrap();
        } else if let Some(p) = s.get("link").as_str() {
            std::os::unix::fs::symlink(s.get("to").as_str().unwrap().replace("<BASE>", &b), at(p))
                .unwrap();
        } else if let Some(p) = s.get("hard").as_str() {
            fs::hard_link(at(s.get("to").as_str().unwrap()), at(p)).unwrap();
        }
    }
    let JValue::Arr(steps) = f.get("steps") else {
        panic!()
    };
    for (i, s) in steps.iter().enumerate() {
        let root = match s.get("root") {
            JValue::Undefined => Some(at("tree")),
            JValue::Null => None,
            r => Some(at(r.as_str().unwrap())),
        };
        let got = if let Some(target) = s.get("read").as_str() {
            files::read(
                &at(target),
                s.get("max").as_num().unwrap() as u64,
                root.as_deref(),
            )
            .map(JValue::Str)
        } else {
            let target = s.get("write").as_str().unwrap();
            files::write(
                &at(target),
                s.get("text").as_str().unwrap(),
                root.as_deref(),
            )
            .map(|()| JValue::Null)
        };
        let got = match got {
            Ok(v) => JValue::obj([("ok", v)]),
            Err(e) => JValue::obj([(
                "error",
                JValue::obj([
                    (
                        "code",
                        e.code.map_or(JValue::Null, |c| JValue::Num(c as f64)),
                    ),
                    (
                        "message",
                        JValue::Str(if e.code.is_some() {
                            e.message
                        } else {
                            "<os error>".into()
                        }),
                    ),
                ]),
            )]),
        };
        let text = |v: &JValue| wtf16::stringify(v).unwrap();
        assert_eq!(text(&got), text(s.get("result")), "step {i}: {}", text(s));
        assert_eq!(
            text(&JValue::Arr(walk(&base, "", &b))),
            text(s.get("tree")),
            "step {i} tree: {}",
            text(s)
        );
    }
}
