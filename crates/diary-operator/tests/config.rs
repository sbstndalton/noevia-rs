#![allow(clippy::indexing_slicing, clippy::unwrap_used)]
use diary_operator::config::{enforce_open_mode_policy, load, system_prompt};
use std::collections::BTreeMap;
#[test]
fn policy() {
    let mut env = BTreeMap::new();
    assert!(enforce_open_mode_policy("", &env).is_err());
    assert!(enforce_open_mode_policy(" ", &env).is_ok());
    env.insert("DIARY_ALLOW_OPEN".into(), " true ".into());
    assert!(enforce_open_mode_policy("", &env).is_err());
    env.insert("DIARY_ALLOW_OPEN".into(), " 1 ".into());
    assert!(enforce_open_mode_policy("", &env).unwrap().is_some());
    env.insert("DIARY_TENANT_KEY".into(), " synthetic ".into());
    assert!(enforce_open_mode_policy("", &env).unwrap().is_none());
}
#[test]
fn loader_and_prompt() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join("prompts")).unwrap();
    std::fs::write(dir.path().join("prompts/system.md"), "synthetic\r\nprompt").unwrap();
    std::fs::write(
        dir.path().join("config.yaml"),
        "llm:\n  chat_model: synthetic-chat\n  aux:\n    model: default\n",
    )
    .unwrap();
    let env = BTreeMap::from([
        ("CORPUS_ROOT".into(), "new".into()),
        ("CORPUS_REMOTE_ROOT".into(), "legacy".into()),
        ("DIARY_PORT".into(), " 8123 ".into()),
        ("DIARY_INDEX_ENABLED".into(), " OFF ".into()),
    ]);
    let cfg = load(None, dir.path(), &env).unwrap();
    assert_eq!(
        diary_operator::config::get(&cfg.data, "corpus.root")
            .unwrap()
            .as_str()
            .unwrap(),
        "legacy"
    );
    assert_eq!(
        diary_operator::config::get(&cfg.data, "corpus.index_enabled")
            .unwrap()
            .pretty()
            .unwrap(),
        "false"
    );
    assert_eq!(
        diary_operator::config::get(&cfg.data, "ui.port")
            .unwrap()
            .pretty()
            .unwrap(),
        "8123"
    );
    assert_eq!(
        diary_operator::config::get(&cfg.data, "llm.aux.model")
            .unwrap()
            .as_str()
            .unwrap(),
        "synthetic-chat"
    );
    assert_eq!(
        system_prompt(&cfg.data, dir.path()).unwrap(),
        "synthetic\nprompt"
    );
}
