//! Node's own answers for the in-app server's three project write tools, replayed through
//! [`project_tools::call`] behind [`Handler`].
//!
//! `tests/fixtures/node-inapp-writes.v1.json` is recorded from core's real `mcp-internal.cjs`
//! `createHandler` + `mcp-internal-tools.cjs` `createInternalTools` + `project-edit-target.cjs` by track
//! R14's `tools/gen-inapp-writes-fixtures.cjs` (copied unchanged from noevia-rs
//! `track/R14-inapp-writes` @d73a3156, `crates/edit-target-lookup/tests/fixtures/`): per case the
//! project, the arguments, the token's pin, the result core sent and every `writeTextFile` it made.
//!
//! planEdit is not this module's (the host gives it as a port), so a case is replayed only where the
//! plan is the plain one: no storage account, the named file present once by its exact name with text,
//! not from a folder, not an original and with a usable name; and no lone surrogate in the arguments,
//! the answer, the writes or that file (a Rust string cannot hold one; the js-json port maps them).
//! The rest are counted as skipped, and both counts are pinned.
#![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]

use js_json::{wtf16, JValue};
use mcp_internal::project_tools::{self, EditPlan, Expect, Hit, Ports, Project, ProjectFile};
use mcp_internal::{mint, Handler, Mint, Step};
use std::cell::RefCell;

const FIXTURE: &str = include_str!("fixtures/node-inapp-writes.v1.json");
const KEY: &[u8] = b"qa-key-replay-0123456789abcdef012";
const NOW: f64 = 1_700_000_000_000.0;
const USER: &str = "aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee";
/// The generator's "wrong" pin.
const WRONG_PIN: &str = "0000000000000000000000000000000000000000000000000000000000000000";

/// The project as `getProject` gave it, the case's write failure, and what was written.
struct Replay {
    project: Option<Project>,
    folder: JValue,
    right_pin: String,
    write_fails: Option<String>,
    writes: RefCell<Vec<(String, String, Option<Expect>)>>,
}

impl Ports for Replay {
    fn get_project(&self, id: &JValue) -> Option<Project> {
        self.project
            .clone()
            .filter(|p| id.as_str() == Some(p.id.as_str()))
    }
    fn read_project_file(&self, _: &Project, _: &JValue) -> Result<Option<String>, String> {
        Ok(Some(String::new()))
    }
    fn rag_available(&self) -> bool {
        false
    }
    fn search(&self, _: &str, _: &str, _: &JValue) -> Result<Vec<Hit>, String> {
        Ok(vec![])
    }
    fn write_text_file(
        &self,
        _: &Project,
        name: &str,
        text: &str,
        expect: Option<&Expect>,
    ) -> Result<(), String> {
        self.writes
            .borrow_mut()
            .push((name.into(), text.into(), expect.cloned()));
        match &self.write_fails {
            Some(message) => Err(message.clone()),
            None => Ok(()),
        }
    }
    /// project-edit-target `uploadPathFor`: `${projectFolder}/${classify(base)}/${base}` when the project
    /// has a folder. Every name a replayed case asks about is a text file (classify: `Text`).
    fn upload_path_for(&self, _: &Project, name: &str) -> String {
        if self.folder.truthy() {
            format!(
                "{}/Text/{name}",
                js_json::to_js_string(&self.folder).unwrap()
            )
        } else {
            name.to_owned()
        }
    }
    /// The plain plan (the only one replayed): the file under its own name, written in place.
    fn plan_edit(&self, project: &Project, name: &JValue) -> Result<EditPlan, String> {
        let wanted = name.as_str().unwrap_or_default();
        let file = project
            .files
            .iter()
            .find(|f| f.name == wanted)
            .unwrap_or_else(|| panic!("a replayed case names a present file: {wanted}"));
        Ok(EditPlan {
            file: file.clone(),
            write_name: file.name.clone(),
            target: file.name.clone(),
            account: None,
            adopt: false,
        })
    }
    fn target_digest(&self, _: &str, account: Option<&str>) -> String {
        assert_eq!(account, None);
        self.right_pin.clone()
    }
}

fn has_lone(s: &str) -> bool {
    s.chars().any(|c| wtf16::lone_unit(c).is_some())
}

/// A lone surrogate written as a JSON escape inside recorded JSON text (`\ud83d` with no pair).
fn has_lone_escape(text: &str) -> bool {
    match wtf16::parse(text) {
        Ok(v) => has_lone(&deep_strings(&v)),
        Err(_) => true,
    }
}

fn deep_strings(v: &JValue) -> String {
    match v {
        JValue::Str(s) => s.clone(),
        JValue::Arr(items) => items.iter().map(deep_strings).collect(),
        JValue::Obj(items) => items
            .iter()
            .map(|(k, v)| format!("{k}{}", deep_strings(v)))
            .collect(),
        _ => String::new(),
    }
}

fn str_of(v: &JValue) -> Option<String> {
    v.as_str().map(str::to_owned)
}

/// `getProject`'s project as the tools read it; `None` when its files are not a list of named entries
/// (core's tools then throw JS TypeErrors: not replayed).
fn project_of(v: &JValue) -> Option<Option<Project>> {
    if v.is_nullish() {
        return Some(None);
    }
    let JValue::Arr(files) = v.get("files") else {
        return None;
    };
    let mut out = Vec::new();
    for f in files {
        let name = str_of(f.get("name"))?;
        out.push(ProjectFile {
            name,
            content: str_of(f.get("content")),
            source: str_of(f.get("source")).filter(|s| !s.is_empty()),
            document: f.get("document").truthy(),
            size: None,
            attachment_id: str_of(f.get("attachment").get("id")),
        });
    }
    Some(Some(Project {
        id: str_of(v.get("id"))?,
        files: out,
    }))
}

#[test]
fn node_recorded_in_app_writes_replay_byte_for_byte() {
    let doc = wtf16::parse(FIXTURE).unwrap();
    let JValue::Arr(cases) = doc.get("cases") else {
        panic!("no cases");
    };
    let (mut replayed, mut skipped) = (0usize, Vec::new());
    for c in cases {
        let name = str_of(c.get("name")).unwrap();
        let tool = str_of(c.get("tool")).unwrap();
        let node = c.get("node");
        let args_text = str_of(c.get("args")).unwrap();
        let result_text = str_of(node.get("result"));
        let JValue::Arr(node_writes) = node.get("writes") else {
            panic!("{name}: writes");
        };
        let writes_text: Vec<String> = node_writes.iter().map(|w| str_of(w).unwrap()).collect();
        let project_raw = match str_of(c.get("project")) {
            Some(text) => wtf16::parse(&text).unwrap(),
            None => JValue::Null,
        };
        let args = wtf16::parse(&args_text).unwrap();
        // The selection the module docs name.
        let lone = has_lone_escape(&args_text)
            || result_text.as_deref().is_some_and(has_lone_escape)
            || writes_text.iter().any(|w| has_lone_escape(w));
        let project = project_of(&project_raw);
        let plain_target = || {
            let Some(Some(p)) = &project else {
                return true; // no project: the tools refuse before planEdit
            };
            let Some(wanted) = args.get("name").as_str() else {
                return false;
            };
            let found: Vec<&ProjectFile> = p.files.iter().filter(|f| f.name == wanted).collect();
            let JValue::Arr(raw_files) = project_raw.get("files") else {
                return false;
            };
            let stored = raw_files.iter().any(|f| {
                f.get("name").as_str() == Some(wanted)
                    && f.get("attachment").get("state").as_str() == Some("stored")
            });
            found.len() == 1
                && found[0].source.is_none()
                && !stored
                && !wanted.contains(['"', '\\'])
                && found[0]
                    .content
                    .as_deref()
                    .is_some_and(|text| !has_lone(text) && !has_lone(wanted))
        };
        let why = if lone {
            Some("lone")
        } else if !c.get("account").is_nullish() {
            Some("account")
        } else if node.get("status").as_num() != Some(200.0) || !node.get("error").is_nullish() {
            Some("status")
        } else if project.is_none() {
            Some("files")
        } else if tool != "project_create_file" && !plain_target() {
            Some("plan")
        } else {
            None
        };
        if let Some(why) = why {
            skipped.push(format!("{name}:{why}"));
            continue;
        }
        replayed += 1;

        let pin = str_of(node.get("pin"));
        let ports = Replay {
            project: project.unwrap(),
            folder: project_raw.get("projectFolder").clone(),
            right_pin: pin
                .clone()
                .filter(|p| p != WRONG_PIN)
                .unwrap_or_else(|| "qa-right-pin".to_owned()),
            write_fails: str_of(c.get("writeFails")),
            writes: RefCell::new(vec![]),
        };
        let pid = str_of(c.get("pid"));
        let token = mint(
            KEY,
            &Mint {
                uid: Some(USER),
                pid: pid.as_deref(),
                write: true,
                target: pin.as_deref(),
                ..Mint::default()
            },
            &format!("jti-{name}"),
            NOW,
        );
        let message = JValue::obj([
            ("jsonrpc", JValue::from("2.0")),
            ("id", JValue::Num(1.0)),
            ("method", JValue::from("tools/call")),
            (
                "params",
                JValue::obj([("name", JValue::from(tool.as_str())), ("arguments", args)]),
            ),
        ]);
        let handler = Handler::new(KEY, project_tools::definitions());
        let reply = match handler.handle(&message, &token, NOW) {
            Step::Reply(r) => r,
            Step::Invoke(inv) => {
                inv.finish(project_tools::call(&ports, &inv.name, &inv.args, &inv.ctx).unwrap())
            }
        };
        assert_eq!(reply.status, 200, "{name}");
        let got = js_json::stringify(reply.body.as_ref().unwrap().get("result")).unwrap();
        assert_eq!(Some(got), result_text, "{name}: result");

        let mine = ports.writes.borrow();
        assert_eq!(mine.len(), writes_text.len(), "{name}: number of writes");
        for ((w_name, w_text, w_expect), recorded) in mine.iter().zip(&writes_text) {
            let r = wtf16::parse(recorded).unwrap();
            assert_eq!(
                r.get("name").as_str(),
                Some(w_name.as_str()),
                "{name}: write name"
            );
            assert_eq!(
                r.get("text").as_str(),
                Some(w_text.as_str()),
                "{name}: write text"
            );
            let options = r.get("options");
            match w_expect {
                None => assert!(options.is_nullish(), "{name}: a create has no options"),
                Some(e) => {
                    assert_eq!(
                        options.get("expectName").as_str(),
                        Some(e.expect_name.as_str()),
                        "{name}"
                    );
                    assert_eq!(
                        options.get("expectContent").as_str(),
                        Some(e.expect_content.as_str()),
                        "{name}"
                    );
                    // An attachment without an id is `undefined` there (left out); none is `null`.
                    assert_eq!(
                        options.get("expectAttachment").as_str(),
                        e.expect_attachment.as_deref(),
                        "{name}"
                    );
                    assert!(
                        options.get("adoptTo").is_nullish() && e.adopt.is_none(),
                        "{name}"
                    );
                }
            }
        }
    }
    eprintln!(
        "COUNTS replayed={replayed} skipped={} [{}]",
        skipped.len(),
        skipped.join(" ")
    );
    assert_eq!(replayed + skipped.len(), 88);
    assert_eq!(replayed, 67, "skipped: {skipped:?}");
}
