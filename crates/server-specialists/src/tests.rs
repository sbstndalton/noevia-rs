#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]
use super::*;

fn store() -> (tempfile::TempDir, Store) {
    let t = tempfile::tempdir().unwrap();
    let s = Store::new(t.path());
    (t, s)
}

fn space(s: &Store, name: &str) -> String {
    s.create_space(NewSpace { name: name.into() }).unwrap().1.id
}

fn spec(name: &str) -> NewSpecialist {
    NewSpecialist {
        name: name.into(),
        ..Default::default()
    }
}

fn put(content: &str, version: Option<&str>) -> PagePut {
    PagePut {
        title: Some("QA page".into()),
        content: content.into(),
        version: version.map(str::to_owned),
    }
}

#[test]
fn ids_are_lowercase_uuid_shaped() {
    assert!(is_id(&new_id()));
    for bad in [
        "",
        "..",
        "../x",
        "AAAAAAAA-AAAA-4AAA-8AAA-AAAAAAAAAAAA",
        "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaa",
        "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaag",
        "aaaaaaaa_aaaa-4aaa-8aaa-aaaaaaaaaaaa",
    ] {
        assert!(!is_id(bad), "{bad}");
    }
}

#[test]
fn specialist_create_fills_null_backing_ids_and_bumps_revision() {
    let (_t, s) = store();
    assert_eq!(s.list_specialists().unwrap().revision, 0);
    let (rev, made) = s.create_specialist(spec("  QA Ada ")).unwrap();
    assert_eq!(rev, 1);
    assert_eq!(made.name, "QA Ada");
    assert!(made.project_id.is_none() && made.chat_id.is_none());
    let json = serde_json::to_value(&made).unwrap();
    assert!(json["projectId"].is_null() && json["chatId"].is_null());
    assert!(json["defaultSpace"].is_null());
    assert_eq!(s.get_specialist(&made.id).unwrap().0, 1);
}

#[test]
fn specialist_limits_and_validation() {
    let (_t, s) = store();
    for bad in ["", "   ", &"x".repeat(61), "a\nb"] {
        assert!(
            matches!(s.create_specialist(spec(bad)), Err(Error::Invalid(_))),
            "{bad:?}"
        );
    }
    let mut long = spec("ok");
    long.brief = Some("b".repeat(8001));
    assert!(matches!(s.create_specialist(long), Err(Error::Invalid(_))));
    let mut ok = spec("ok");
    ok.brief = Some("b".repeat(8000));
    s.create_specialist(ok).unwrap();
    for i in 1..MAX_SPECIALISTS {
        s.create_specialist(spec(&format!("QA {i}"))).unwrap();
    }
    let err = s.create_specialist(spec("one too many")).unwrap_err();
    assert_eq!(err.code(), "specialist_limit");
}

#[test]
fn specialist_grants_must_be_own_spaces_and_default_one_of_them() {
    let (_t, s) = store();
    let a = space(&s, "QA A");
    let mut n = spec("QA");
    n.spaces = Some(vec![new_id()]);
    assert!(matches!(s.create_specialist(n), Err(Error::Invalid(_))));
    let mut n = spec("QA");
    n.spaces = Some(vec![a.clone(), a.clone()]);
    n.default_space = Some(a.clone());
    let (_, made) = s.create_specialist(n).unwrap();
    assert_eq!(made.spaces, vec![a.clone()]);
    let mut n = spec("QA2");
    n.default_space = Some(a);
    assert!(matches!(s.create_specialist(n), Err(Error::Invalid(_))));
}

#[test]
fn specialist_patch_checks_revision_and_can_clear_default() {
    let (_t, s) = store();
    let a = space(&s, "QA A");
    let mut n = spec("QA");
    n.spaces = Some(vec![a.clone()]);
    n.default_space = Some(a);
    let (rev, made) = s.create_specialist(n).unwrap();
    let stale = SpecialistPatch {
        revision: rev + 1,
        name: Some("x".into()),
        ..Default::default()
    };
    match s.update_specialist(&made.id, stale) {
        Err(Error::Stale {
            revision: Some(r),
            version: None,
        }) => assert_eq!(r, rev),
        other => panic!("{other:?}"),
    }
    let patch: SpecialistPatch = serde_json::from_str(&format!(
        r#"{{"revision":{rev},"defaultSpace":null,"name":"QA B"}}"#
    ))
    .unwrap();
    let (rev2, got) = s.update_specialist(&made.id, patch).unwrap();
    assert_eq!(
        (rev2, got.name.as_str(), got.default_space),
        (rev + 1, "QA B", None)
    );
    // Absent defaultSpace leaves it alone; unknown fields are refused.
    let r: Result<SpecialistPatch, _> = serde_json::from_str(r#"{"revision":1,"projectId":"x"}"#);
    assert!(r.is_err());
}

#[test]
fn specialist_delete_checks_revision() {
    let (_t, s) = store();
    let (rev, made) = s.create_specialist(spec("QA")).unwrap();
    assert!(matches!(
        s.delete_specialist(&made.id, rev + 5),
        Err(Error::Stale { .. })
    ));
    assert_eq!(s.delete_specialist(&made.id, rev).unwrap(), rev + 1);
    assert!(matches!(s.get_specialist(&made.id), Err(Error::NotFound)));
    assert!(matches!(
        s.delete_specialist("../x", 0),
        Err(Error::NotFound)
    ));
}

#[test]
fn space_limit_and_name_rules() {
    let (_t, s) = store();
    assert!(s
        .create_space(NewSpace {
            name: "x".repeat(81)
        })
        .is_err());
    assert!(s.create_space(NewSpace { name: " ".into() }).is_err());
    for i in 0..MAX_SPACES {
        space(&s, &format!("QA {i}"));
    }
    let e = s
        .create_space(NewSpace {
            name: "more".into(),
        })
        .unwrap_err();
    assert_eq!(e.code(), "space_limit");
}

#[test]
fn page_create_get_and_version_is_sha256_of_content() {
    let (_t, s) = store();
    let sp = space(&s, "QA");
    let w = s
        .create_page(&sp, "QA note", "# hello", SAVED_BY_YOU)
        .unwrap();
    assert!(w.created && w.changed);
    assert_eq!(w.page.version, version_of("# hello"));
    assert_eq!(w.page.version.len(), 64);
    let (row, content) = s.get_page(&sp, &w.page.id).unwrap();
    assert_eq!((content.as_str(), row.bytes), ("# hello", 7));
}

#[test]
fn page_put_revision_rule() {
    let (_t, s) = store();
    let sp = space(&s, "QA");
    let id = new_id();
    let created = s
        .put_page(&sp, &id, put("one", None), SAVED_BY_YOU)
        .unwrap();
    // Create again with null: 409 with the current version.
    match s.put_page(&sp, &id, put("two", None), SAVED_BY_YOU) {
        Err(Error::Stale {
            version: Some(v), ..
        }) => assert_eq!(v, created.page.version),
        other => panic!("{other:?}"),
    }
    // Identical content is a no-op with the same version and updatedAt.
    let same = s
        .put_page(
            &sp,
            &id,
            put("one", Some(&created.page.version)),
            SAVED_BY_YOU,
        )
        .unwrap();
    assert!(!same.changed);
    assert_eq!(same.page, created.page);
    // A real change moves the version.
    let next = s
        .put_page(
            &sp,
            &id,
            put("two", Some(&created.page.version)),
            SAVED_BY_YOU,
        )
        .unwrap();
    assert!(next.changed && !next.created);
    assert_eq!(next.page.version, version_of("two"));
    // The old base is now stale and carries the current version.
    match s.put_page(
        &sp,
        &id,
        put("three", Some(&created.page.version)),
        SAVED_BY_YOU,
    ) {
        Err(Error::Stale {
            version: Some(v),
            revision: None,
        }) => assert_eq!(v, next.page.version),
        other => panic!("{other:?}"),
    }
    // A version for a page that does not exist is 404, not a create.
    assert!(matches!(
        s.put_page(&sp, &new_id(), put("x", Some("abc")), SAVED_BY_YOU),
        Err(Error::NotFound)
    ));
}

#[test]
fn page_title_only_change_keeps_version_and_needs_matching_base() {
    let (_t, s) = store();
    let sp = space(&s, "QA");
    let w = s.create_page(&sp, "Old", "body", SAVED_BY_YOU).unwrap();
    let mut p = put("body", Some(&w.page.version));
    p.title = Some("New".into());
    let got = s.put_page(&sp, &w.page.id, p, SAVED_BY_YOU).unwrap();
    assert!(got.changed);
    assert_eq!(
        (got.page.title.as_str(), got.page.version.as_str()),
        ("New", w.page.version.as_str())
    );
}

#[test]
fn page_limits() {
    let (_t, s) = store();
    let sp = space(&s, "QA");
    let big = "a".repeat(MAX_PAGE_BYTES);
    s.create_page(&sp, "max", &big, SAVED_BY_YOU).unwrap();
    let e = s
        .create_page(&sp, "over", &format!("{big}a"), SAVED_BY_YOU)
        .unwrap_err();
    assert_eq!(e.code(), "page_too_large");
    assert!(s
        .create_page(&sp, &"t".repeat(201), "x", SAVED_BY_YOU)
        .is_err());
    assert!(s.create_page(&sp, "t", "x", "someone").is_err());
}

#[test]
fn page_count_limit() {
    let (_t, s) = store();
    let sp = space(&s, "QA");
    let mut index = pages::PageIndex::default();
    for i in 0..MAX_PAGES {
        index.pages.push(PageRow {
            id: new_id(),
            title: format!("p{i}"),
            version: String::new(),
            bytes: 0,
            created_at: 1,
            updated_at: 1,
            saved_by: SAVED_BY_YOU.into(),
        });
    }
    s.write(&s.space_dir(&sp), "index.json", &index).unwrap();
    let e = s
        .create_page(&sp, "one more", "x", SAVED_BY_YOU)
        .unwrap_err();
    assert_eq!(e.code(), "page_limit");
}

#[test]
fn search_is_case_folded_over_title_and_content_and_bounded() {
    let (_t, s) = store();
    let sp = space(&s, "QA");
    s.create_page(&sp, "Alpha Notes", "nothing", SAVED_BY_YOU)
        .unwrap();
    s.create_page(&sp, "Other", "the QUICK fox", SAVED_BY_YOU)
        .unwrap();
    s.create_page(&sp, "Third", "quick again", SAVED_BY_YOU)
        .unwrap();
    let hit = |q: &str| s.list_pages(&sp, Some(q), None, 0).unwrap();
    assert_eq!(hit("ALPHA").pages.len(), 1);
    assert_eq!(hit("quick").pages.len(), 2);
    assert_eq!(hit("nomatch").pages.len(), 0);
    let all = s.list_pages(&sp, None, None, 0).unwrap();
    assert_eq!((all.pages.len(), all.total, all.truncated), (3, 3, false));
    let two = s.list_pages(&sp, Some("quick"), Some(1), 0).unwrap();
    assert_eq!((two.pages.len(), two.truncated), (1, true));
    let exact = s.list_pages(&sp, Some("quick"), Some(2), 0).unwrap();
    assert_eq!((exact.pages.len(), exact.truncated), (2, false));
    let rest = s.list_pages(&sp, Some("quick"), Some(1), 1).unwrap();
    assert_eq!((rest.pages.len(), rest.truncated), (1, false));
    assert!(s.list_pages(&sp, None, Some(101), 0).is_err());
    assert!(s.list_pages(&sp, None, Some(0), 0).is_err());
    assert!(s.list_pages(&sp, Some(&"q".repeat(201)), None, 0).is_err());
}

#[test]
fn search_stops_at_the_read_budget() {
    let (_t, s) = store();
    let sp = space(&s, "QA");
    let mut index = pages::PageIndex::default();
    for i in 0..3 {
        index.pages.push(PageRow {
            id: new_id(),
            title: format!("p{i}"),
            version: String::new(),
            // Claims 20 MiB each; nothing is read once the budget is spent.
            bytes: 20 * 1024 * 1024,
            created_at: 1,
            updated_at: 1,
            saved_by: SAVED_BY_YOU.into(),
        });
    }
    s.write(&s.space_dir(&sp), "index.json", &index).unwrap();
    let r = s.list_pages(&sp, Some("zzz"), None, 0).unwrap();
    assert!(r.truncated && r.pages.is_empty());
}

#[test]
fn space_delete_needs_with_pages_and_removes_grants_and_files() {
    let (t, s) = store();
    let sp = space(&s, "QA");
    let mut n = spec("QA");
    n.spaces = Some(vec![sp.clone()]);
    n.default_space = Some(sp.clone());
    let (_, who) = s.create_specialist(n).unwrap();
    let w = s.create_page(&sp, "p", "x", SAVED_BY_YOU).unwrap();
    let rev = s.list_spaces().unwrap().revision;
    assert!(matches!(
        s.delete_space(&sp, rev, false),
        Err(Error::SpaceNotEmpty)
    ));
    assert!(matches!(
        s.delete_space(&sp, rev + 1, true),
        Err(Error::Stale { .. })
    ));
    assert!(t
        .path()
        .join("spaces")
        .join(&sp)
        .join(format!("{}.md", w.page.id))
        .exists());
    s.delete_space(&sp, rev, true).unwrap();
    assert!(!t.path().join("spaces").join(&sp).exists());
    let (_, after) = s.get_specialist(&who.id).unwrap();
    assert!(after.spaces.is_empty() && after.default_space.is_none());
    assert!(matches!(s.get_page(&sp, &w.page.id), Err(Error::NotFound)));
}

#[test]
fn empty_space_deletes_without_with_pages() {
    let (_t, s) = store();
    let sp = space(&s, "QA");
    let rev = s.list_spaces().unwrap().revision;
    s.delete_space(&sp, rev, false).unwrap();
}

#[test]
fn page_delete_removes_row_and_file() {
    let (t, s) = store();
    let sp = space(&s, "QA");
    let w = s.create_page(&sp, "p", "x", SAVED_BY_YOU).unwrap();
    s.delete_page(&sp, &w.page.id).unwrap();
    assert!(!t
        .path()
        .join("spaces")
        .join(&sp)
        .join(format!("{}.md", w.page.id))
        .exists());
    assert!(matches!(
        s.delete_page(&sp, &w.page.id),
        Err(Error::NotFound)
    ));
}

#[test]
fn malformed_ids_never_reach_the_disk() {
    let (t, s) = store();
    for bad in ["../x", "a/b", "", "A"] {
        assert!(matches!(s.get_space(bad), Err(Error::NotFound)));
        assert!(matches!(
            s.list_pages(bad, None, None, 0),
            Err(Error::NotFound)
        ));
        assert!(matches!(s.get_page(bad, bad), Err(Error::NotFound)));
        assert!(matches!(s.delete_space(bad, 0, true), Err(Error::NotFound)));
    }
    assert_eq!(std::fs::read_dir(t.path()).unwrap().count(), 0);
}

#[test]
fn unreadable_store_file_is_never_overwritten() {
    let (t, s) = store();
    std::fs::write(t.path().join("spaces.json"), "{not json").unwrap();
    assert!(matches!(
        s.create_space(NewSpace { name: "QA".into() }),
        Err(Error::Corrupt(_))
    ));
    assert_eq!(
        std::fs::read_to_string(t.path().join("spaces.json")).unwrap(),
        "{not json"
    );
}

#[test]
fn concurrent_puts_from_one_base_have_one_winner() {
    let (_t, s) = store();
    let sp = space(&s, "QA");
    let w = s.create_page(&sp, "p", "base", SAVED_BY_YOU).unwrap();
    let results: Vec<_> = (0..8)
        .map(|i| {
            let (s, sp, id, v) = (
                s.clone(),
                sp.clone(),
                w.page.id.clone(),
                w.page.version.clone(),
            );
            std::thread::spawn(move || {
                s.put_page(
                    &sp,
                    &id,
                    put(&format!("writer {i}"), Some(&v)),
                    SAVED_BY_YOU,
                )
            })
        })
        .collect::<Vec<_>>()
        .into_iter()
        .map(|h| h.join().unwrap())
        .collect();
    let won = results.iter().filter(|r| r.is_ok()).count();
    let stale = results
        .iter()
        .filter(|r| matches!(r, Err(Error::Stale { .. })))
        .count();
    assert_eq!((won, stale), (1, 7));
}

#[test]
fn space_delete_removes_the_files_even_when_the_grants_cannot_be_updated() {
    let (t, s) = store();
    let sp = space(&s, "QA");
    s.create_page(&sp, "p", "x", SAVED_BY_YOU).unwrap();
    // specialists.json is a directory: reading it fails, as a broken disk would.
    std::fs::create_dir(t.path().join("specialists.json")).unwrap();
    let rev = s.list_spaces().unwrap().revision;
    assert!(s.delete_space(&sp, rev, true).is_err());
    assert!(!t.path().join("spaces").join(&sp).exists());
    assert!(s.list_spaces().unwrap().spaces.is_empty());
}

#[test]
fn a_row_left_stale_by_a_crash_is_repaired_by_the_next_write_and_a_missing_file_is_404() {
    let (t, s) = store();
    let sp = space(&s, "QA");
    let w = s.create_page(&sp, "p", "one", SAVED_BY_YOU).unwrap();
    // The file changed but the index write never happened.
    let file = t
        .path()
        .join("spaces")
        .join(&sp)
        .join(format!("{}.md", w.page.id));
    std::fs::write(&file, "two").unwrap();
    assert_eq!(
        s.list_pages(&sp, None, None, 0).unwrap().pages[0].version,
        version_of("one")
    );
    let mut same = put("two", Some(&version_of("two")));
    same.title = None;
    let noop = s.put_page(&sp, &w.page.id, same, SAVED_BY_YOU).unwrap();
    assert!(!noop.changed);
    assert_eq!(
        s.list_pages(&sp, None, None, 0).unwrap().pages[0].version,
        version_of("two")
    );
    // A row without its file: 404 on read and write, and it can still be deleted.
    std::fs::remove_file(&file).unwrap();
    assert!(matches!(s.get_page(&sp, &w.page.id), Err(Error::NotFound)));
    assert!(matches!(
        s.put_page(
            &sp,
            &w.page.id,
            put("x", Some(&version_of("two"))),
            SAVED_BY_YOU
        ),
        Err(Error::NotFound)
    ));
    s.delete_page(&sp, &w.page.id).unwrap();
    assert!(s.list_pages(&sp, None, None, 0).unwrap().pages.is_empty());
}
