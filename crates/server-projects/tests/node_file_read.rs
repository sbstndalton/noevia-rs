//! The recorded answers of core's `read_project_file` tool (`fixtures/node-file-read.v1.json`,
//! written by `record_file_read.cjs`): every call the port answers must be core's text and failure
//! flag; a call it leaves to core must be one core answers from an error the port does not copy.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use js_json::{wtf16, JValue};
use server_projects::file_read::{inapp_result, read_project_file, Outcome};

const FIXTURE: &str = include_str!("fixtures/node-file-read.v1.json");

#[test]
fn read_project_file_matches_core() {
    let f = wtf16::parse(FIXTURE).expect("the fixture is JSON");
    // The account directory as the recorder wrote it.
    let dir = std::env::temp_dir().join(format!("qa-file-read-{}", std::process::id()));
    let JValue::Obj(files) = f.get("files") else {
        panic!("files is an object")
    };
    for (rel, body) in files {
        let path = dir.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, body.as_str().unwrap()).unwrap();
    }
    let JValue::Arr(calls) = f.get("calls") else {
        panic!("calls is an array")
    };
    let (mut native, mut core, mut fails) = (0, 0, Vec::new());
    for call in calls {
        let project = call
            .get("projectId")
            .as_str()
            .map(|id| f.get("projects").get(id));
        let args = call.get("args");
        let want = call.get("result");
        let shown = || wtf16::stringify(args).unwrap_or_default();
        match read_project_file(&dir, project, args) {
            Outcome::Done(got) => {
                native += 1;
                if want.get("threw").truthy()
                    || want.get("text").as_str() != Some(got.text.as_str())
                    || want.get("failed").as_bool() != Some(got.failed)
                {
                    fails.push(format!(
                        "{}\n  core: {}\n  port: {:?} failed={}",
                        shown(),
                        wtf16::stringify(want).unwrap_or_default(),
                        got.text.chars().take(300).collect::<String>(),
                        got.failed
                    ));
                }
            }
            Outcome::Core => {
                core += 1;
                let allowed = want.get("threw").truthy()
                    || call.get("coreOnly").truthy()
                    || call.get("contentless").truthy();
                if !allowed {
                    fails.push(format!(
                        "{}: left to core, but core answered {}",
                        shown(),
                        wtf16::stringify(want).unwrap_or_default()
                    ));
                }
            }
        }
    }
    let _ = std::fs::remove_dir_all(&dir);
    assert!(
        fails.is_empty(),
        "{} of {} calls differ; first ones:\n{}",
        fails.len(),
        calls.len(),
        fails.iter().take(6).cloned().collect::<Vec<_>>().join("\n")
    );
    assert!(native > 90, "most calls are answered natively ({native})");
    assert!(core < 15, "few calls are left to core ({core})");
}

/// The in-app MCP server's read-only project tools, recorded through core's mcp-internal.cjs handler:
/// every result the port gives must be core's JSON-RPC `result`; a call it leaves to core must be one
/// core answers from an error the port does not copy.
#[test]
fn inapp_project_reads_match_cores_server() {
    let f = wtf16::parse(FIXTURE).expect("the fixture is JSON");
    let dir = std::env::temp_dir().join(format!("qa-file-read-inapp-{}", std::process::id()));
    let JValue::Obj(files) = f.get("files") else {
        panic!("files is an object")
    };
    for (rel, body) in files {
        let path = dir.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, body.as_str().unwrap()).unwrap();
    }
    let JValue::Arr(calls) = f.get("inapp") else {
        panic!("inapp is an array")
    };
    let (mut native, mut fails) = (0, Vec::new());
    for call in calls {
        assert_eq!(
            call.get("status").as_num(),
            Some(200.0),
            "core's server answered 200"
        );
        let project = call
            .get("projectId")
            .as_str()
            .map(|id| f.get("projects").get(id))
            .filter(|p| p.is_object());
        let name = call.get("name").as_str().unwrap();
        let shown = || {
            wtf16::stringify(&JValue::obj([
                ("name", JValue::Str(name.into())),
                ("args", call.get("args").clone()),
            ]))
            .unwrap_or_default()
        };
        match inapp_result(&dir, project, name, call.get("args")) {
            Some(got) => {
                native += 1;
                let (want, got) = (wtf16::stringify(call.get("result")), wtf16::stringify(&got));
                if want != got {
                    fails.push(format!("{}\n  core: {want:?}\n  port: {got:?}", shown()));
                }
            }
            None => {
                if !(call.get("coreOnly").truthy() || call.get("contentless").truthy()) {
                    fails.push(format!(
                        "{}: left to core, but core answered {:?}",
                        shown(),
                        wtf16::stringify(call.get("result"))
                    ));
                }
            }
        }
    }
    let _ = std::fs::remove_dir_all(&dir);
    assert!(
        fails.is_empty(),
        "{} of {} differ:\n{}",
        fails.len(),
        calls.len(),
        fails.iter().take(6).cloned().collect::<Vec<_>>().join("\n")
    );
    assert!(
        native > 30,
        "most in-app calls are answered natively ({native})"
    );
}
