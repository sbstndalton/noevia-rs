use js_json::{wtf16, JValue};
use server_projects::task_journals::{
    writer::{Error, Store},
    Surface,
};
#[test]
fn native_code_journal_events_match_node_bounds_and_limits() {
    let fixture = wtf16::parse(include_str!("../tests/fixtures/journal-events-node.json")).unwrap();
    let root = tempfile::tempdir().unwrap();
    let user = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa";
    let id = "11111111-1111-4111-8111-111111111111";
    std::fs::create_dir_all(root.path().join("users").join(user)).unwrap();
    let store = Store::open(root.path(), user, Surface::Code).unwrap();
    store.create(id, "project", vec![], 1234).unwrap();
    store
        .append(
            id,
            "plan.proposed",
            wtf16::parse(r#"{"question":"ignored","subQuestions":["\ud800"],"truncated":false}"#)
                .unwrap(),
            1234,
        )
        .unwrap();
    let output = JValue::Obj(vec![
        ("text".into(), JValue::Str("😀".repeat(400))),
        ("ignored".into(), JValue::Str("not journalled".into())),
    ]);
    store.append(id, "assistant.output", output, 1234).unwrap();
    for _ in 1..64 {
        store
            .append(id, "assistant.output", JValue::Obj(vec![]), 1234)
            .unwrap();
    }
    assert!(matches!(
        store.append(id, "assistant.output", JValue::Obj(vec![]), 1234),
        Err(Error::InvalidEvent)
    ));
    let raw = std::fs::read_to_string(
        root.path()
            .join("users")
            .join(user)
            .join("jobs")
            .join(format!("{id}.jsonl")),
    )
    .unwrap();
    let rows = raw
        .lines()
        .map(|s| wtf16::parse(s).unwrap())
        .collect::<Vec<_>>();
    for (kind, key) in [("plan.proposed", "plan"), ("assistant.output", "output")] {
        let row = rows
            .iter()
            .find(|r| r.get("type").as_str() == Some(kind))
            .unwrap();
        assert_eq!(row.get("data"), fixture.get(key));
    }
    // The ordinary context cannot write pipeline authority, even with matching field names.
    assert!(matches!(
        store.append(id, "task.stage", JValue::Null, 1234),
        Err(Error::InvalidEvent)
    ));
}

#[test]
fn task_retention_is_surface_scoped_and_never_prunes_live_tasks() {
    let root = tempfile::tempdir().unwrap();
    let user = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa";
    std::fs::create_dir_all(root.path().join("users").join(user)).unwrap();
    let code = Store::open(root.path(), user, Surface::Code).unwrap();
    let browser = Store::open(root.path(), user, Surface::Browser).unwrap();
    let code_id = "11111111-1111-4111-8111-111111111111";
    let browser_id = "22222222-2222-4222-8222-222222222222";
    let live_id = "33333333-3333-4333-8333-333333333333";
    code.create(code_id, "project", vec![], 0).unwrap();
    code.append(code_id, "job.completed", JValue::Null, 0)
        .unwrap();
    browser.create(browser_id, "project", vec![], 0).unwrap();
    browser
        .append(browser_id, "job.completed", JValue::Null, 0)
        .unwrap();
    code.create(live_id, "project", vec![], 0).unwrap();
    let late = 10 * 86_400_000;
    browser
        .create(
            "44444444-4444-4444-8444-444444444444",
            "project",
            vec![],
            late,
        )
        .unwrap();
    let path = |id: &str| {
        root.path()
            .join("users")
            .join(user)
            .join("jobs")
            .join(format!("{id}.jsonl"))
    };
    assert!(!path(browser_id).exists());
    assert!(path(code_id).exists());
    assert!(path(live_id).exists());
    code.create(
        "55555555-5555-4555-8555-555555555555",
        "project",
        vec![],
        15 * 86_400_000,
    )
    .unwrap();
    assert!(!path(code_id).exists());
    assert!(path(live_id).exists());
    code.sync(live_id).unwrap();
}
