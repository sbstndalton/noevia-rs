//! The backing project and conversation: what create, patch and delete write, in which order,
//! and what each crash window leaves for the next read (see `backing`).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]
use super::*;
use js_json::JValue;
use server_projects::store as projects;
use std::path::Path;

const USER: &str = "11111111-2222-4333-8444-555555555555";
const OTHER: &str = "99999999-2222-4333-8444-555555555555";

struct Rig {
    root: tempfile::TempDir,
    store: Store,
}

fn rig_for(root: &tempfile::TempDir, user: &str) -> Store {
    let dir = projects::user_dir(root.path(), user).unwrap();
    std::fs::create_dir_all(&dir).unwrap();
    Store::new(&dir)
        .with_backing(Backing {
            data_dir: root.path().to_owned(),
            user_id: user.to_owned(),
        })
        .with_known_toolboxes(vec!["core".into(), "web-search".into(), "calc".into()])
}

fn rig() -> Rig {
    let root = tempfile::tempdir().unwrap();
    let store = rig_for(&root, USER);
    Rig { root, store }
}

impl Rig {
    fn dir(&self) -> std::path::PathBuf {
        projects::user_dir(self.root.path(), USER).unwrap()
    }
    fn projects(&self) -> Vec<JValue> {
        projects::read(&self.dir()).unwrap()
    }
    fn project(&self, id: &str) -> Option<JValue> {
        projects::find(&self.projects(), id).cloned()
    }
    fn tombstones(&self) -> String {
        std::fs::read_to_string(self.dir().join("deleted-chats.json")).unwrap_or_default()
    }
    fn make(&self, name: &str, brief: &str, boxes: &[&str]) -> Specialist {
        self.store
            .create_specialist(NewSpecialist {
                name: name.into(),
                brief: Some(brief.into()),
                toolboxes: Some(boxes.iter().map(|b| (*b).to_owned()).collect()),
                ..Default::default()
            })
            .unwrap()
            .1
    }
    /// A project of the account's own that the coworkers must never touch.
    fn other_project(&self) {
        let text =
            r#"{"projects":[{"id":"proj-1-aaaaaa","name":"QA keep","chats":[{"id":"qa-keep"}]}]}"#;
        std::fs::write(self.dir().join("projects.json"), text).unwrap();
    }
}

fn strings(v: &JValue) -> Vec<String> {
    match v {
        JValue::Arr(a) => a.iter().map(|x| x.as_str().unwrap().to_owned()).collect(),
        other => panic!("not a list: {other:?}"),
    }
}

fn chat_ids(project: &JValue) -> Vec<String> {
    match project.get("chats") {
        JValue::Arr(c) => c
            .iter()
            .map(|c| c.get("id").as_str().unwrap().to_owned())
            .collect(),
        _ => Vec::new(),
    }
}

#[test]
fn create_makes_the_hidden_project_with_the_brief_and_toolboxes_and_one_chat() {
    let r = rig();
    r.other_project();
    let made = r.make(
        "QA Scribe",
        "Write short notes.",
        &["core", "web-search", "core"],
    );
    assert_eq!(made.toolboxes, vec!["core", "web-search"]);
    let pid = made.project_id.clone().unwrap();
    let cid = made.chat_id.clone().unwrap();
    assert_eq!(pid, format!("specialist-{}", made.id));
    assert_eq!(cid, format!("specialist-chat-{}", made.id));
    let p = r.project(&pid).expect("project made");
    assert_eq!(p.get("instructions").as_str(), Some("Write short notes."));
    assert_eq!(strings(p.get("toolboxes")), vec!["core", "web-search"]);
    assert_eq!(p.get("toolsMode").as_str(), Some("manual"));
    assert_eq!(chat_ids(&p), vec![cid]);
    assert!(projects::internal_project_id(&pid));
    // The account's other projects are untouched.
    assert_eq!(
        chat_ids(&r.project("proj-1-aaaaaa").unwrap()),
        vec!["qa-keep"]
    );
}

#[test]
fn default_toolboxes_are_empty_and_need_no_catalogue() {
    let root = tempfile::tempdir().unwrap();
    let dir = projects::user_dir(root.path(), USER).unwrap();
    std::fs::create_dir_all(&dir).unwrap();
    let s = Store::new(&dir).with_backing(Backing {
        data_dir: root.path().to_owned(),
        user_id: USER.to_owned(),
    });
    let made = s
        .create_specialist(NewSpecialist {
            name: "QA".into(),
            ..Default::default()
        })
        .unwrap()
        .1;
    assert!(made.toolboxes.is_empty());
    let p = projects::find(&projects::read(&dir).unwrap(), &made.project_id.unwrap())
        .unwrap()
        .clone();
    assert!(strings(p.get("toolboxes")).is_empty());
}

#[test]
fn toolboxes_are_checked_against_the_known_ones_and_nothing_is_written_on_refusal() {
    let r = rig();
    for bad in [vec!["nope"], vec!["core", "../x"], vec![""]] {
        let e = r
            .store
            .create_specialist(NewSpecialist {
                name: "QA".into(),
                toolboxes: Some(bad.iter().map(|b| (*b).to_owned()).collect()),
                ..Default::default()
            })
            .unwrap_err();
        assert!(matches!(e, Error::Invalid(_)), "{e}");
    }
    assert!(!r.dir().join("specialists.json").exists());
    assert!(r.projects().is_empty());
    // Too many.
    let many: Vec<String> = (0..=MAX_TOOLBOXES).map(|i| format!("t{i}")).collect();
    assert!(matches!(
        r.store.create_specialist(NewSpecialist {
            name: "QA".into(),
            toolboxes: Some(many),
            ..Default::default()
        }),
        Err(Error::Invalid(_))
    ));
    // Without a catalogue a non-empty list is refused, never accepted unchecked.
    let bare = Store::new(r.dir()).with_backing(Backing {
        data_dir: r.root.path().to_owned(),
        user_id: USER.to_owned(),
    });
    assert!(matches!(
        bare.create_specialist(NewSpecialist {
            name: "QA".into(),
            toolboxes: Some(vec!["core".into()]),
            ..Default::default()
        }),
        Err(Error::Invalid(_))
    ));
}

#[test]
fn patch_mirrors_brief_toolboxes_and_name_and_a_noop_patch_writes_nothing() {
    let r = rig();
    let made = r.make("QA Scribe", "v1", &["core"]);
    let pid = made.project_id.clone().unwrap();
    let (rev, _) = r.store.get_specialist(&made.id).unwrap();
    let (rev, v) = r
        .store
        .update_specialist(
            &made.id,
            SpecialistPatch {
                revision: rev,
                brief: Some("v2".into()),
                toolboxes: Some(vec!["calc".into()]),
                name: Some("QA Scribe 2".into()),
                ..Default::default()
            },
        )
        .unwrap();
    assert_eq!(v.toolboxes, vec!["calc"]);
    let p = r.project(&pid).unwrap();
    assert_eq!(p.get("instructions").as_str(), Some("v2"));
    assert_eq!(strings(p.get("toolboxes")), vec!["calc"]);
    assert_eq!(p.get("name").as_str(), Some("QA Scribe 2"));
    assert_eq!(
        chat_ids(&p),
        vec![made.chat_id.clone().unwrap()],
        "still one chat"
    );
    // A patch that changes neither leaves projects.json byte for byte.
    let before = std::fs::read(r.dir().join("projects.json")).unwrap();
    r.store
        .update_specialist(
            &made.id,
            SpecialistPatch {
                revision: rev,
                glyph: Some("Q".into()),
                ..Default::default()
            },
        )
        .unwrap();
    assert_eq!(
        before,
        std::fs::read(r.dir().join("projects.json")).unwrap()
    );
    // A refused toolbox changes neither the record nor the project.
    let (rev, _) = r.store.get_specialist(&made.id).unwrap();
    assert!(r
        .store
        .update_specialist(
            &made.id,
            SpecialistPatch {
                revision: rev,
                brief: Some("v3".into()),
                toolboxes: Some(vec!["nope".into()]),
                ..Default::default()
            },
        )
        .is_err());
    assert_eq!(r.store.get_specialist(&made.id).unwrap().1.brief, "v2");
    assert_eq!(
        r.project(&pid).unwrap().get("instructions").as_str(),
        Some("v2")
    );
}

#[test]
fn crash_after_the_record_leaves_a_record_whose_project_the_next_read_makes() {
    let r = rig();
    r.other_project();
    let made = r.make("QA", "brief", &["core"]);
    let pid = made.project_id.clone().unwrap();
    // The crash: the project write never happened.
    std::fs::write(
        r.dir().join("projects.json"),
        r#"{"projects":[{"id":"proj-1-aaaaaa","name":"QA keep","chats":[{"id":"qa-keep"}]}]}"#,
    )
    .unwrap();
    assert!(r.project(&pid).is_none());
    let list = r.store.list_specialists().unwrap();
    assert_eq!(list.specialists.len(), 1);
    let p = r.project(&pid).expect("repaired");
    assert_eq!(p.get("instructions").as_str(), Some("brief"));
    assert_eq!(chat_ids(&p), vec![made.chat_id.unwrap()]);
    assert!(r.project("proj-1-aaaaaa").is_some());
}

#[test]
fn crash_after_a_patched_record_leaves_a_stale_project_the_next_get_mirrors() {
    let r = rig();
    let made = r.make("QA", "old", &[]);
    let pid = made.project_id.clone().unwrap();
    let (rev, _) = r.store.get_specialist(&made.id).unwrap();
    r.store
        .update_specialist(
            &made.id,
            SpecialistPatch {
                revision: rev,
                brief: Some("new".into()),
                toolboxes: Some(vec!["core".into()]),
                ..Default::default()
            },
        )
        .unwrap();
    // Put the project back as it was before the mirror.
    let mut ps = r.projects();
    let p = projects::find_mut(&mut ps, &pid).unwrap();
    projects::set(p, "instructions", JValue::from("old"));
    projects::set(p, "toolboxes", JValue::Arr(Vec::new()));
    std::fs::write(r.dir().join("projects.json"), projects::serialise(ps)).unwrap();
    r.store.get_specialist(&made.id).unwrap();
    let p = r.project(&pid).unwrap();
    assert_eq!(p.get("instructions").as_str(), Some("new"));
    assert_eq!(strings(p.get("toolboxes")), vec!["core"]);
}

#[test]
fn a_coworker_from_before_this_slice_gets_its_project_and_chat_on_the_next_read() {
    let r = rig();
    let id = new_id();
    std::fs::write(
        r.dir().join("specialists.json"),
        format!(
            r#"{{"revision":3,"specialists":[{{"id":"{id}","name":"QA old","glyph":"","tint":"","brief":"old brief","projectId":null,"chatId":null,"spaces":[],"defaultSpace":null,"createdAt":1,"updatedAt":1}}]}}"#
        ),
    )
    .unwrap();
    let (_, s) = r.store.get_specialist(&id).unwrap();
    assert_eq!(
        s.project_id.as_deref(),
        Some(format!("specialist-{id}").as_str())
    );
    assert_eq!(
        s.chat_id.as_deref(),
        Some(format!("specialist-chat-{id}").as_str())
    );
    assert!(s.toolboxes.is_empty());
    let p = r.project(&format!("specialist-{id}")).unwrap();
    assert_eq!(p.get("instructions").as_str(), Some("old brief"));
    assert_eq!(chat_ids(&p), vec![format!("specialist-chat-{id}")]);
    // Repaired once: a second read changes nothing.
    let before = std::fs::read(r.dir().join("projects.json")).unwrap();
    let rev = r.store.list_specialists().unwrap().revision;
    assert_eq!(rev, 4);
    assert_eq!(
        before,
        std::fs::read(r.dir().join("projects.json")).unwrap()
    );
}

#[test]
fn a_patch_of_an_older_coworker_gives_it_its_project_too() {
    let r = rig();
    let id = new_id();
    std::fs::write(
        r.dir().join("specialists.json"),
        format!(
            r#"{{"revision":1,"specialists":[{{"id":"{id}","name":"QA old","glyph":"","tint":"","brief":"","projectId":null,"chatId":null,"spaces":[],"defaultSpace":null,"createdAt":1,"updatedAt":1}}]}}"#
        ),
    )
    .unwrap();
    let (_, s) = r
        .store
        .update_specialist(
            &id,
            SpecialistPatch {
                revision: 1,
                brief: Some("now".into()),
                ..Default::default()
            },
        )
        .unwrap();
    let p = r.project(&s.project_id.unwrap()).unwrap();
    assert_eq!(p.get("instructions").as_str(), Some("now"));
    assert_eq!(chat_ids(&p), vec![s.chat_id.unwrap()]);
}

#[test]
fn delete_removes_the_chat_tombstone_first_then_the_project_and_nothing_else() {
    let r = rig();
    r.other_project();
    let a = r.make("QA A", "a", &[]);
    let b = r.make("QA B", "b", &[]);
    let (cid, pid) = (a.chat_id.clone().unwrap(), a.project_id.clone().unwrap());
    // The conversation has a history file.
    let hist = server_projects::history::history_file(&r.dir(), &cid);
    std::fs::write(&hist, r#"{"history":[{"role":"user","content":"hi"}]}"#).unwrap();
    let rev = r.store.list_specialists().unwrap().revision;
    r.store.delete_specialist(&a.id, rev).unwrap();
    assert!(r.project(&pid).is_none());
    assert!(r.tombstones().contains(&cid), "{}", r.tombstones());
    assert!(!hist.exists(), "history file removed");
    // B and the account's own project are intact.
    assert!(r.project(&b.project_id.unwrap()).is_some());
    assert!(r.project("proj-1-aaaaaa").is_some());
    assert!(!r.tombstones().contains(&b.chat_id.unwrap()));
    assert!(matches!(
        r.store.get_specialist(&a.id),
        Err(Error::NotFound)
    ));
}

#[test]
fn a_chat_that_cannot_be_deleted_keeps_the_project_and_the_next_read_finishes_the_delete() {
    let r = rig();
    let made = r.make("QA", "x", &[]);
    let (cid, pid) = (
        made.chat_id.clone().unwrap(),
        made.project_id.clone().unwrap(),
    );
    // The tombstone file cannot be written: a directory sits where it goes.
    std::fs::create_dir(r.dir().join("deleted-chats.json")).unwrap();
    let rev = r.store.list_specialists().unwrap().revision;
    // The record is gone (that is the delete) but the project stays: the chat is not removed, and
    // removing the project first would strand its files.
    r.store.delete_specialist(&made.id, rev).unwrap();
    assert!(r.project(&pid).is_some());
    std::fs::remove_dir(r.dir().join("deleted-chats.json")).unwrap();
    let list = r.store.list_specialists().unwrap();
    assert!(list.specialists.is_empty());
    assert!(r.project(&pid).is_none(), "orphan removed");
    assert!(r.tombstones().contains(&cid));
}

#[test]
fn an_orphan_project_without_a_record_is_removed_but_a_live_one_never_is() {
    let r = rig();
    let live = r.make("QA live", "l", &[]);
    let ghost = new_id();
    let mut ps = r.projects();
    ps.push(JValue::obj([
        ("id", JValue::from(format!("specialist-{ghost}"))),
        ("name", JValue::from("ghost")),
        (
            "chats",
            JValue::Arr(vec![JValue::obj([(
                "id",
                JValue::from(format!("specialist-chat-{ghost}")),
            )])]),
        ),
    ]));
    std::fs::write(r.dir().join("projects.json"), projects::serialise(ps)).unwrap();
    r.store.list_specialists().unwrap();
    assert!(r.project(&format!("specialist-{ghost}")).is_none());
    assert!(r.tombstones().contains(&format!("specialist-chat-{ghost}")));
    assert!(r.project(&live.project_id.unwrap()).is_some());
}

#[test]
fn no_backing_means_no_project_and_null_ids_as_before() {
    let t = tempfile::tempdir().unwrap();
    let s = Store::new(t.path());
    let made = s
        .create_specialist(NewSpecialist {
            name: "QA".into(),
            ..Default::default()
        })
        .unwrap()
        .1;
    assert!(made.project_id.is_none() && made.chat_id.is_none());
    assert!(!Path::new(&t.path().join("projects.json")).exists());
}

#[test]
fn a_conversation_resolves_to_its_own_coworker_only() {
    let r = rig();
    let a = r.make("QA A", "a", &[]);
    let b = r.make("QA B", "b", &[]);
    let (ap, ac) = (a.project_id.clone().unwrap(), a.chat_id.clone().unwrap());
    let (bp, bc) = (b.project_id.clone().unwrap(), b.chat_id.clone().unwrap());
    assert_eq!(
        r.store.resolve_conversation(&ap, &ac).unwrap().unwrap().id,
        a.id
    );
    assert_eq!(
        r.store.resolve_conversation(&bp, &bc).unwrap().unwrap().id,
        b.id
    );
    // A mixed pair, a free chat, an ordinary project, and an id that is nobody's.
    assert!(r.store.resolve_conversation(&ap, &bc).unwrap().is_none());
    assert!(r.store.resolve_conversation(&bp, &ac).unwrap().is_none());
    assert!(r
        .store
        .resolve_conversation("proj-1-aaaaaa", &ac)
        .unwrap()
        .is_none());
    assert!(r
        .store
        .resolve_conversation(&ap, "c-1-abc")
        .unwrap()
        .is_none());
    assert!(r.store.resolve_conversation("", "").unwrap().is_none());
    // Another account's store holds none of these ids.
    let other = rig_for(&r.root, OTHER);
    assert!(other.resolve_conversation(&ap, &ac).unwrap().is_none());
    // A deleted coworker's conversation resolves to nobody.
    let rev = r.store.list_specialists().unwrap().revision;
    r.store.delete_specialist(&a.id, rev).unwrap();
    assert!(r.store.resolve_conversation(&ap, &ac).unwrap().is_none());
}

#[test]
fn two_accounts_never_see_each_others_projects() {
    let r = rig();
    let mine = r.make("QA mine", "m", &[]);
    let other = rig_for(&r.root, OTHER);
    let theirs = other
        .create_specialist(NewSpecialist {
            name: "QA theirs".into(),
            ..Default::default()
        })
        .unwrap()
        .1;
    other.list_specialists().unwrap();
    r.store.list_specialists().unwrap();
    assert!(r.project(&mine.project_id.unwrap()).is_some());
    assert!(r.project(&theirs.project_id.unwrap()).is_none());
}

#[test]
fn concurrent_creates_and_reads_keep_one_project_per_coworker() {
    let r = rig();
    let store = r.store.clone();
    let handles: Vec<_> = (0..6)
        .map(|i| {
            let s = store.clone();
            std::thread::spawn(move || {
                s.create_specialist(NewSpecialist {
                    name: format!("QA {i}"),
                    ..Default::default()
                })
                .map(|(_, v)| v.id)
                .ok();
                s.list_specialists().unwrap();
            })
        })
        .collect();
    for h in handles {
        h.join().unwrap();
    }
    let list = r.store.list_specialists().unwrap();
    assert_eq!(list.specialists.len(), 6);
    let ps = r.projects();
    assert_eq!(
        ps.iter()
            .filter(|p| p.get("id").as_str().unwrap().starts_with("specialist-"))
            .count(),
        6
    );
    for s in &list.specialists {
        let p = projects::find(&ps, s.project_id.as_ref().unwrap()).unwrap();
        assert_eq!(chat_ids(p), vec![s.chat_id.clone().unwrap()]);
    }
}
