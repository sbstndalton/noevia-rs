#![allow(clippy::unwrap_used, clippy::panic)]
use server_projects::{source_startup, store};
use server_store::RustProjects;
#[test]
fn replay_node_source_text_oracle() {
    let fixture = js_json::wtf16::parse(include_str!("fixtures/source-startup/node.json")).unwrap();
    let root = tempfile::tempdir().unwrap();
    let input = fixture.get("input").as_str().unwrap();
    std::fs::write(root.path().join("projects.json"), input).unwrap();
    assert!(source_startup::migrate(root.path(), RustProjects::owned()).unwrap());
    let output = store::read_json(&root.path().join("projects.json"))
        .unwrap()
        .unwrap();
    assert_eq!(&output, fixture.get("expected"));
    let js_json::JValue::Obj(files) = fixture.get("files") else {
        panic!()
    };
    for (path, text) in files {
        assert_eq!(
            std::fs::read_to_string(root.path().join(path)).unwrap(),
            text.as_str().unwrap()
        );
    }
}
