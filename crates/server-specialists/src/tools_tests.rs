//! `space_page_save`: grants, the same-draft rule, limits and the crash window between a page and
//! its ledger entry.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]
use super::backing::{chat_id_of, project_id_of};
use super::tools::{SavePage, MAX_LEDGER};
use super::*;
use server_projects::store as projects;

const USER: &str = "11111111-2222-4333-8444-555555555555";
const OTHER: &str = "99999999-2222-4333-8444-555555555555";

fn store_for(root: &tempfile::TempDir, user: &str) -> Store {
    let dir = projects::user_dir(root.path(), user).unwrap();
    std::fs::create_dir_all(&dir).unwrap();
    Store::new(&dir).with_backing(Backing {
        data_dir: root.path().to_owned(),
        user_id: user.to_owned(),
    })
}

struct Rig {
    root: tempfile::TempDir,
    store: Store,
    /// A coworker granted `notes` (its default) and `ideas`; `private` is nobody's grant.
    who: Specialist,
    notes: String,
    ideas: String,
    private: String,
}

fn space(store: &Store, name: &str) -> String {
    store
        .create_space(NewSpace { name: name.into() })
        .unwrap()
        .1
        .id
}

fn rig() -> Rig {
    let root = tempfile::tempdir().unwrap();
    let store = store_for(&root, USER);
    let (notes, ideas, private) = (
        space(&store, "QA notes"),
        space(&store, "QA ideas"),
        space(&store, "QA private"),
    );
    let who = store
        .create_specialist(NewSpecialist {
            name: "QA writer".into(),
            spaces: Some(vec![notes.clone(), ideas.clone()]),
            default_space: Some(notes.clone()),
            ..Default::default()
        })
        .unwrap()
        .1;
    Rig {
        root,
        store,
        who,
        notes,
        ideas,
        private,
    }
}

fn draft(space: Option<&str>, title: &str, content: &str) -> SavePage {
    SavePage {
        space_id: space.map(str::to_owned),
        title: title.into(),
        content: content.into(),
    }
}

impl Rig {
    fn save(&self, d: &SavePage) -> Result<tools::Saved, Error> {
        self.store.space_page_save(
            self.who.project_id.as_deref().unwrap(),
            self.who.chat_id.as_deref().unwrap(),
            d,
        )
    }
    fn pages(&self, space: &str) -> usize {
        self.store.list_pages(space, None, None, 0).unwrap().total
    }
}

#[test]
fn saves_to_the_default_space_with_the_coworker_as_author() {
    let r = rig();
    let s = r.save(&draft(None, "  QA plan  ", "# plan\nbody")).unwrap();
    assert!(s.created);
    assert_eq!(s.space_id, r.notes);
    assert_eq!(s.title, "QA plan");
    assert_eq!(s.link, format!("/spaces/{}/{}", r.notes, s.page_id));
    let (row, content) = r.store.get_page(&r.notes, &s.page_id).unwrap();
    assert_eq!(content, "# plan\nbody");
    assert_eq!(row.saved_by, r.who.id);
}

#[test]
fn a_named_granted_space_is_used() {
    let r = rig();
    let s = r.save(&draft(Some(&r.ideas), "QA idea", "x")).unwrap();
    assert_eq!(s.space_id, r.ideas);
    assert_eq!((r.pages(&r.ideas), r.pages(&r.notes)), (1, 0));
}

#[test]
fn a_space_outside_the_grant_is_refused_and_nothing_is_written() {
    let r = rig();
    for bad in [r.private.as_str(), "../x", "not-an-id", &uuid_like()] {
        let e = r.save(&draft(Some(bad), "QA t", "x")).unwrap_err();
        assert!(matches!(e, Error::Invalid(_)), "{bad}: {e:?}");
    }
    assert_eq!(r.pages(&r.private), 0);
}

fn uuid_like() -> String {
    "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa".into()
}

#[test]
fn no_default_space_needs_a_space_id() {
    let r = rig();
    let bare = r
        .store
        .create_specialist(NewSpecialist {
            name: "QA bare".into(),
            spaces: Some(vec![r.ideas.clone()]),
            ..Default::default()
        })
        .unwrap()
        .1;
    let e = r
        .store
        .space_page_save(
            bare.project_id.as_deref().unwrap(),
            bare.chat_id.as_deref().unwrap(),
            &draft(None, "QA t", "x"),
        )
        .unwrap_err();
    assert!(matches!(e, Error::Invalid(_)));
}

#[test]
fn another_coworkers_space_is_refused() {
    let r = rig();
    let other = r
        .store
        .create_specialist(NewSpecialist {
            name: "QA other".into(),
            spaces: Some(vec![r.private.clone()]),
            default_space: Some(r.private.clone()),
            ..Default::default()
        })
        .unwrap()
        .1;
    // The other coworker may write its own Space, and ours may not.
    r.store
        .space_page_save(
            other.project_id.as_deref().unwrap(),
            other.chat_id.as_deref().unwrap(),
            &draft(None, "QA t", "x"),
        )
        .unwrap();
    assert!(r.save(&draft(Some(&r.private), "QA t2", "y")).is_err());
    assert_eq!(r.pages(&r.private), 1);
}

#[test]
fn only_a_coworkers_own_project_and_chat_resolve() {
    let r = rig();
    let d = draft(None, "QA t", "x");
    let (p, c) = (
        r.who.project_id.clone().unwrap(),
        r.who.chat_id.clone().unwrap(),
    );
    let other_chat = chat_id_of("aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa");
    let other_project = project_id_of("aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa");
    for (pp, cc) in [
        (p.as_str(), other_chat.as_str()),
        (other_project.as_str(), c.as_str()),
        ("proj-1-aaaaaa", c.as_str()),
        (p.as_str(), "qa-free-chat"),
        ("", ""),
    ] {
        assert!(matches!(
            r.store.space_page_save(pp, cc, &d),
            Err(Error::NotFound)
        ));
    }
    assert_eq!(r.pages(&r.notes), 0);
}

#[test]
fn another_accounts_ids_resolve_to_nothing() {
    let r = rig();
    let theirs = store_for(&r.root, OTHER);
    let e = theirs
        .space_page_save(
            r.who.project_id.as_deref().unwrap(),
            r.who.chat_id.as_deref().unwrap(),
            &draft(Some(&r.notes), "QA t", "x"),
        )
        .unwrap_err();
    assert!(matches!(e, Error::NotFound));
    assert_eq!(r.pages(&r.notes), 0);
}

#[test]
fn the_same_draft_is_one_page_and_a_changed_one_is_another() {
    let r = rig();
    let a = r.save(&draft(None, "QA t", "one")).unwrap();
    let b = r.save(&draft(Some(&r.notes), "QA t", "one")).unwrap();
    assert_eq!((a.page_id.as_str(), b.created), (b.page_id.as_str(), false));
    assert_eq!(r.pages(&r.notes), 1);
    let c = r.save(&draft(None, "QA t", "two")).unwrap();
    let d = r.save(&draft(None, "QA t2", "one")).unwrap();
    assert!(c.created && d.created);
    assert_eq!(r.pages(&r.notes), 3);
    // The same text in another Space is another draft.
    let e = r.save(&draft(Some(&r.ideas), "QA t", "one")).unwrap();
    assert!(e.created);
}

#[test]
fn an_edited_page_still_answers_for_the_draft() {
    let r = rig();
    let a = r.save(&draft(None, "QA t", "one")).unwrap();
    let (_, _) = r.store.get_page(&r.notes, &a.page_id).unwrap();
    let v = version_of("one");
    r.store
        .put_page(
            &r.notes,
            &a.page_id,
            PagePut {
                title: None,
                content: "edited by a person".into(),
                version: Some(v),
            },
            SAVED_BY_YOU,
        )
        .unwrap();
    let b = r.save(&draft(None, "QA t", "one")).unwrap();
    assert_eq!((b.page_id, b.created), (a.page_id, false));
}

#[test]
fn a_deleted_page_saves_again() {
    let r = rig();
    let a = r.save(&draft(None, "QA t", "one")).unwrap();
    r.store.delete_page(&r.notes, &a.page_id).unwrap();
    let b = r.save(&draft(None, "QA t", "one")).unwrap();
    assert!(b.created);
    assert_ne!(a.page_id, b.page_id);
}

#[test]
fn a_crash_between_page_and_ledger_does_not_make_a_second_page() {
    let r = rig();
    let a = r.save(&draft(None, "QA t", "one")).unwrap();
    std::fs::remove_file(r.store.dir().join("page-saves.json")).unwrap();
    let b = r.save(&draft(None, "QA t", "one")).unwrap();
    assert_eq!((b.page_id.as_str(), b.created), (a.page_id.as_str(), false));
    assert!(r.store.dir().join("page-saves.json").exists());
    assert_eq!(r.pages(&r.notes), 1);
}

#[test]
fn limits_are_the_stores() {
    let r = rig();
    assert!(matches!(
        r.save(&draft(None, "", "x")),
        Err(Error::Invalid(_))
    ));
    assert!(matches!(
        r.save(&draft(None, &"t".repeat(MAX_TITLE_CHARS + 1), "x")),
        Err(Error::Invalid(_))
    ));
    assert!(matches!(
        r.save(&draft(None, "two\nlines", "x")),
        Err(Error::Invalid(_))
    ));
    assert!(matches!(
        r.save(&draft(None, "QA t", "  \n")),
        Err(Error::Invalid(_))
    ));
    assert!(matches!(
        r.save(&draft(None, "QA t", &"x".repeat(MAX_PAGE_BYTES + 1))),
        Err(Error::PageTooLarge)
    ));
    assert!(r
        .save(&draft(None, "QA t", &"x".repeat(MAX_PAGE_BYTES)))
        .is_ok());
    // The ledger keeps at most MAX_LEDGER drafts.
    const { assert!(MAX_LEDGER >= MAX_PAGES) };
}

#[test]
fn a_full_space_refuses_with_the_page_limit() {
    let r = rig();
    let rows: Vec<PageRow> = (0..MAX_PAGES)
        .map(|i| PageRow {
            id: format!("{i:08x}-0000-4000-8000-000000000000"),
            title: "QA".into(),
            version: "v".into(),
            bytes: 0,
            created_at: 0,
            updated_at: 0,
            saved_by: SAVED_BY_YOU.into(),
        })
        .collect();
    let dir = r.store.space_dir(&r.notes);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("index.json"),
        serde_json::to_string(&serde_json::json!({ "pages": rows })).unwrap(),
    )
    .unwrap();
    assert!(matches!(
        r.save(&draft(None, "QA t", "x")),
        Err(Error::Limit {
            code: "page_limit",
            ..
        })
    ));
}

#[test]
fn a_space_deleted_after_the_grant_is_refused() {
    let r = rig();
    let (rev, _) = r.store.list_spaces().map(|l| (l.revision, ())).unwrap();
    r.store.delete_space(&r.notes, rev, true).unwrap();
    assert!(matches!(
        r.save(&draft(None, "QA t", "x")),
        Err(Error::Invalid(_))
    ));
}

#[test]
fn an_unreadable_ledger_does_not_stop_saves() {
    let r = rig();
    let a = r.save(&draft(None, "QA t", "one")).unwrap();
    std::fs::write(r.store.dir().join("page-saves.json"), "{").unwrap();
    // The page is found by its row, and the ledger is rewritten.
    let b = r.save(&draft(None, "QA t", "one")).unwrap();
    assert_eq!((b.page_id.as_str(), b.created), (a.page_id.as_str(), false));
    let c = r.save(&draft(None, "QA t", "two")).unwrap();
    assert!(c.created);
    let text = std::fs::read_to_string(r.store.dir().join("page-saves.json")).unwrap();
    assert!(serde_json::from_str::<serde_json::Value>(&text).is_ok());
}
