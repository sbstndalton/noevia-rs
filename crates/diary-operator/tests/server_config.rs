//! `server_config::main_mode` against the start-up config the frozen Python child sends its Rust
//! parent, recorded by tests/generate_server_config_fixtures.py (synthetic values only): every
//! case Python starts with resolves to the same JSON value, and every case its start refuses is
//! refused here too.
#![allow(clippy::indexing_slicing, clippy::unwrap_used, clippy::panic)]
use diary_operator::config::load;
use diary_operator::server_config::{main_mode, Start};
use serde_json::Value;
use std::collections::BTreeMap;

const FIXTURE: &str = include_str!("fixtures/server-config-python312.json");

#[test]
fn every_recorded_case_resolves_as_the_python_child_sends_it() {
    let fixture: Value = serde_json::from_str(FIXTURE).unwrap();
    let start = Start {
        port: u16::try_from(fixture["port"].as_u64().unwrap()).unwrap(),
        control_token: fixture["control_token"].as_str().unwrap(),
        forward_secret: fixture["forward_secret"].as_str().unwrap(),
    };
    let cases = fixture["cases"].as_array().unwrap();
    assert!(cases.len() >= 20, "{}", cases.len());
    let mut refused = 0;
    for case in cases {
        let name = case["case"].as_str().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.yaml");
        std::fs::write(&path, case["yaml"].as_str().unwrap()).unwrap();
        let env: BTreeMap<String, String> = case["env"]
            .as_object()
            .unwrap()
            .iter()
            .map(|(k, v)| (k.clone(), v.as_str().unwrap().to_owned()))
            .collect();
        let resolved = load(Some(path.as_path()), dir.path(), &env)
            .and_then(|cfg| main_mode(&cfg.data, &env, &start));
        match (&case["config"], resolved) {
            (Value::Null, Err(_)) => refused += 1,
            (Value::Null, Ok(v)) => panic!("{name}: Python refuses this start, Rust resolved {v}"),
            (expected, Ok(v)) => assert_eq!(&v, expected, "{name}"),
            (_, Err(e)) => panic!("{name}: {e}"),
        }
    }
    assert!(refused >= 2, "{refused}");
}

#[test]
fn unresolvable_home_forms_leave_the_external_sources_to_the_child() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.yaml");
    std::fs::write(
        &path,
        "llm:\n  base_url: http://qa-engine.invalid/v1\n  aux:\n    base_url: http://qa-engine.invalid/v1\n",
    )
    .unwrap();
    let start = Start {
        port: 8010,
        control_token: "qa-control",
        forward_secret: "qa-forward",
    };
    for (sources, home) in [("~qa-user/x", Some("/home/qa")), ("~/x", None)] {
        let mut env = BTreeMap::from([("DIARY_EXTERNAL_SOURCES".to_owned(), sources.to_owned())]);
        if let Some(h) = home {
            env.insert("HOME".into(), h.into());
        }
        let cfg = load(Some(path.as_path()), dir.path(), &env).unwrap();
        let v = main_mode(&cfg.data, &env, &start).unwrap();
        assert!(v.get("external_sources").is_none(), "{sources}: {v}");
    }
    let env = BTreeMap::from([
        ("DIARY_EXTERNAL_SOURCES".to_owned(), "~//qa/./x/".to_owned()),
        ("HOME".to_owned(), "//".to_owned()),
    ]);
    let cfg = load(Some(path.as_path()), dir.path(), &env).unwrap();
    let v = main_mode(&cfg.data, &env, &start).unwrap();
    // os.path.expanduser("~") with HOME="//" is "/"; pathlib then joins the rest.
    assert_eq!(v["external_sources"], serde_json::json!(["/qa/x"]));
}
