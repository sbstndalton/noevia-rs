//! Track R14 step 1: crates/server-projects `workspace_view` against noevia-core's own workspace load
//! (workspace.cjs `get`) and projects.cjs `getProject`. tests/fixtures/node-workspace-view.v1.json is
//! written by `node tools/gen-workspace-view-fixtures.cjs <noevia-core> <out>` (synthetic, QA-named
//! workspaces only): per case the projects, each source's text, the free chats, the preferences, the
//! saved roles, and per looked-up id the project getProject answers and whether it saved.
//! The view must give the same, as JSON text, except where workspace_view.rs documents otherwise.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use js_json::{wtf16, JValue};
use server_projects::workspace_view::{self, Refused};

/// Node keeps a number or string project and the view refuses it (workspace_view.rs header).
const DAMAGED: &[&str] = &["number-project", "string-project"];
/// getProject over a project shape Node never writes (`files` a string, a file that is a list):
/// refused (skills.rs).
const UNSUPPORTED: &[&str] = &["qa-archived", "qa-m10"];
/// A source with inline text: Node moves the text to its file on load, the view keeps it inline.
const INLINE: &[&str] = &["inline-text"];

// The fixture holds lone surrogates (escaped), so it is read as JavaScript reads it.
fn fixture() -> JValue {
    wtf16::parse(include_str!("fixtures/node-workspace-view.v1.json")).unwrap()
}

fn s(v: &JValue) -> &str {
    v.as_str().unwrap()
}

fn arr(v: &JValue) -> &[JValue] {
    match v {
        JValue::Arr(items) => items,
        other => panic!("not a list: {other:?}"),
    }
}

fn obj(v: &JValue) -> &[(String, JValue)] {
    match v {
        JValue::Obj(items) => items,
        other => panic!("not an object: {other:?}"),
    }
}

fn json(v: &JValue) -> String {
    wtf16::stringify(v).expect("representable")
}

/// For INLINE: the projects without the fields #1806 moves (`content`, `textRef`, `chars`,
/// `skillHeader` of each file).
fn without_moved(v: &JValue) -> JValue {
    match v {
        JValue::Arr(items) => JValue::Arr(items.iter().map(without_moved).collect()),
        JValue::Obj(items) => JValue::Obj(
            items
                .iter()
                .map(|(k, v)| {
                    if k == "files" {
                        let files = match v {
                            JValue::Arr(files) => JValue::Arr(
                                files
                                    .iter()
                                    .map(|f| match f {
                                        JValue::Obj(fields) => JValue::Obj(
                                            fields
                                                .iter()
                                                .filter(|(k, _)| {
                                                    !matches!(
                                                        k.as_str(),
                                                        "content"
                                                            | "textRef"
                                                            | "chars"
                                                            | "skillHeader"
                                                    )
                                                })
                                                .cloned()
                                                .collect(),
                                        ),
                                        other => other.clone(),
                                    })
                                    .collect(),
                            ),
                            other => other.clone(),
                        };
                        (k.clone(), files)
                    } else {
                        (k.clone(), without_moved(v))
                    }
                })
                .collect(),
        ),
        other => other.clone(),
    }
}

/// The bytes Node's `fs.writeFileSync(file, text)` writes: UTF-8, a lone surrogate as U+FFFD.
fn utf8(text: &str) -> String {
    text.chars()
        .map(|c| {
            if wtf16::lone_unit(c).is_some() {
                '\u{fffd}'
            } else {
                c
            }
        })
        .collect()
}

fn setup(doc: &JValue, c: &JValue) -> tempfile::TempDir {
    let root = tempfile::tempdir().unwrap();
    if c.get("absent").as_bool() == Some(true) {
        return root;
    }
    let dir = root.path().join("users").join(s(c.get("user")));
    std::fs::create_dir_all(&dir).unwrap();
    for (name, text) in obj(c.get("files")) {
        std::fs::write(dir.join(name), utf8(s(text))).unwrap();
    }
    for (id, texts) in obj(doc.get("sources")) {
        let folder = dir.join("project-sources").join(id);
        std::fs::create_dir_all(&folder).unwrap();
        for text in arr(texts) {
            let text = utf8(s(text));
            use sha2::Digest;
            let name = format!("{:x}.txt", sha2::Sha256::digest(text.as_bytes()));
            std::fs::write(folder.join(name), text).unwrap();
        }
    }
    root
}

#[test]
fn the_fixture_names_its_node_sources() {
    let doc = fixture();
    let read = doc.get("sourcesRead");
    for f in [
        "server/workspace.cjs",
        "server/projects.cjs",
        "server/rust-projects.cjs",
        "server/source-text.cjs",
        "server/project-modes.cjs",
        "server/instruction-skills.cjs",
    ] {
        assert_eq!(s(read.get(f)).len(), 64, "{f}");
    }
    assert!(arr(doc.get("cases")).len() >= 25);
}

#[test]
fn the_view_reads_what_core_reads() {
    let doc = fixture();
    let default = s(doc.get("defaultProvider"));
    let mut checked = 0;
    for c in arr(doc.get("cases")) {
        let name = s(c.get("name"));
        let node = c.get("node");
        let root = setup(&doc, c);
        let view = workspace_view::load(root.path(), s(c.get("user")), default);
        if let Some(threw) = node.get("threw").as_str() {
            let want = if threw == "invalid user id" {
                Refused::InvalidUser
            } else {
                Refused::Damaged
            };
            assert_eq!(view.unwrap_err(), want, "{name}");
            continue;
        }
        if DAMAGED.contains(&name) {
            assert_eq!(view.unwrap_err(), Refused::Damaged, "{name}");
            continue;
        }
        let view = view.unwrap_or_else(|e| panic!("{name}: {e:?}"));
        if INLINE.contains(&name) {
            assert_eq!(
                json(&without_moved(&JValue::Arr(view.projects.clone()))),
                json(&without_moved(
                    &wtf16::parse(s(node.get("projects"))).unwrap()
                )),
                "{name}: projects"
            );
        } else {
            assert_eq!(
                json(&JValue::Arr(view.projects.clone())),
                s(node.get("projects")),
                "{name}: projects"
            );
        }
        assert_eq!(
            json(&JValue::Arr(view.free_chats.clone())),
            s(node.get("freeChats")),
            "{name}: free chats"
        );
        assert_eq!(
            json(&view.preferences),
            s(node.get("preferences")),
            "{name}: preferences"
        );
        assert_eq!(
            json(&view.auto_roles().unwrap()),
            s(node.get("autoRoles")),
            "{name}: roles"
        );
        for t in arr(node.get("texts")) {
            let t = arr(t);
            let (pi, fi) = (
                t[0].as_num().unwrap() as usize,
                t[1].as_num().unwrap() as usize,
            );
            let project = &view.projects[pi];
            let files = arr(project.get("files"));
            let got = view.text(project.get("id").as_str().unwrap_or_default(), &files[fi]);
            assert_eq!(got.as_deref(), t[2].as_str(), "{name}: text {pi}/{fi}");
        }
        for q in arr(node.get("queries")) {
            let id = s(q.get("id"));
            if UNSUPPORTED.contains(&id) && q.get("project").as_str().is_some() {
                assert_eq!(view.project(id), Err(Refused::Unsupported), "{name}: {id}");
                continue;
            }
            let got = view
                .project(id)
                .unwrap_or_else(|e| panic!("{name}: {id}: {e:?}"));
            match q.get("project").as_str() {
                None => assert!(got.is_none(), "{name}: {id} should be missing"),
                Some(want) => {
                    let got = got.unwrap_or_else(|| panic!("{name}: {id} missing"));
                    if INLINE.contains(&name) {
                        assert_eq!(
                            json(&without_moved(&got.project)),
                            json(&without_moved(&wtf16::parse(want).unwrap())),
                            "{name}: getProject {id}"
                        );
                    } else {
                        assert_eq!(json(&got.project), want, "{name}: getProject {id}");
                    }
                    assert_eq!(
                        got.save.is_some(),
                        q.get("saved").as_bool().unwrap(),
                        "{name}: {id} saved"
                    );
                    if let Some(save) = &got.save {
                        assert_eq!(save, &got.project, "{name}: {id} saves what it answers");
                        // ... and what Node's save wrote for it.
                        if let Some(written) = q.get("written").as_str() {
                            assert_eq!(json(save), written, "{name}: {id} written");
                        }
                    }
                }
            }
        }
        checked += 1;
    }
    assert!(checked >= 20, "{checked}");
}

#[test]
fn the_view_writes_nothing() {
    let doc = fixture();
    let c = arr(doc.get("cases"))
        .iter()
        .find(|c| c.get("name").as_str() == Some("full"))
        .unwrap();
    let root = setup(&doc, c);
    let user = s(c.get("user"));
    let dir = root.path().join("users").join(user);
    let before = std::fs::read(dir.join("projects.json")).unwrap();
    let view = workspace_view::load(root.path(), user, "local").unwrap();
    assert!(view
        .project("cowork-chat-context-qa-heal")
        .unwrap()
        .unwrap()
        .save
        .is_some());
    assert_eq!(std::fs::read(dir.join("projects.json")).unwrap(), before);
    assert!(!dir.join("migration-backups").exists());
    // A missing account directory is an empty view and is not made.
    let absent = tempfile::tempdir().unwrap();
    let view = workspace_view::load(absent.path(), user, "local").unwrap();
    assert!(view.projects.is_empty() && view.free_chats.is_empty());
    assert!(!absent.path().join("users").exists());
}

#[test]
fn a_list_file_that_cannot_be_read_is_refused() {
    // Node's readProjectsFile/readFreeChatsFile throw on anything but a missing file (here EISDIR).
    let user = "aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee";
    for name in ["projects.json", "free-chats.json"] {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("users").join(user);
        std::fs::create_dir_all(dir.join(name)).unwrap();
        assert_eq!(
            workspace_view::load(root.path(), user, "local").unwrap_err(),
            Refused::Unreadable,
            "{name}"
        );
    }
    // The other two files fall back, as readJson does.
    let root = tempfile::tempdir().unwrap();
    let dir = root.path().join("users").join(user);
    std::fs::create_dir_all(dir.join("preferences.json")).unwrap();
    std::fs::create_dir_all(dir.join("auto-roles.json")).unwrap();
    let view = workspace_view::load(root.path(), user, "local").unwrap();
    assert_eq!(view.preferences, JValue::Obj(Vec::new()));
    assert_eq!(view.auto_roles().unwrap(), JValue::Null);
}
