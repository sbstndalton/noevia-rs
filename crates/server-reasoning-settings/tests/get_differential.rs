#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]
//! `GET /api/reasoning-settings` replayed on recorded Node output (`fixtures/node-reasoning-get.v1.json`,
//! written by `record_get.cjs` from core's own route, reasoning-effort.cjs, provider registry and workspace
//! store): every reply, from the same environment, settings row, query, projects and provider files. The
//! front's route test replays the same file through HTTP. Cases marked `difference` must fail closed here.
use js_json::{wtf16, JValue};
use serde_json::Value;
use server_reasoning_settings::read::{self, Defaults, File};

const FIXTURE: &str = include_str!("fixtures/node-reasoning-get.v1.json");

fn file(v: &Value) -> File {
    match v.as_str() {
        Some(text) => File::Bytes(text.as_bytes().to_vec()),
        None => File::Missing,
    }
}

/// `PROJECTS.find((p) => p.id === id) || null` over the store's list (`projects` or none).
fn find_project(projects: &Value, id: &str) -> Option<JValue> {
    let text = projects.as_str()?;
    let doc = wtf16::parse(text).ok()?;
    match doc.get("projects") {
        JValue::Arr(items) => items
            .iter()
            .find(|p| p.get("id").as_str() == Some(id))
            .cloned(),
        _ => None,
    }
}

#[test]
fn recorded_node_get_matrix() {
    let fixture: Value = serde_json::from_str(FIXTURE).unwrap();
    let cases = fixture["cases"].as_array().unwrap();
    assert!(cases.len() >= 100, "corpus shrank to {}", cases.len());
    let mut differences = 0;
    for case in cases {
        let name = case["name"].as_str().unwrap();
        let input = &case["input"];
        let expected = &case["expected"];
        let env = input["env"].clone();
        let defaults = Defaults::from_lookup(|k| env[k].as_str().map(str::to_owned));
        let global = input["setting"]
            .as_str()
            .filter(|s| !s.is_empty())
            .unwrap_or("default")
            .to_string();
        let query = input["query"].as_str().unwrap();
        let query = query.strip_prefix('?');
        let project = match read::project_param(query) {
            None => None,
            Some(id) => match find_project(&input["projects"], &id) {
                Some(p) => Some(p),
                None => {
                    assert_eq!(expected["status"], 404, "{name}");
                    assert_eq!(
                        expected["reply"], r#"{"error":"no such project"}"#,
                        "{name}"
                    );
                    continue;
                }
            },
        };
        let wanted = project
            .as_ref()
            .map(|p| p.get("provider"))
            .filter(|v| v.truthy())
            .cloned()
            .unwrap_or_else(|| JValue::Str(defaults.id.clone()));
        let provider = read::provider(
            &defaults,
            &file(&input["shared"]),
            &file(&input["private"]),
            &wanted,
        );
        if case["difference"] == "unholdable-providers" {
            differences += 1;
            assert!(provider.is_err(), "{name}: must fail closed");
            continue;
        }
        let provider = provider.unwrap();
        let effort = read::resolve_effort(project.as_ref(), &global);
        let model = project
            .as_ref()
            .map(|p| p.get("model"))
            .filter(|v| v.truthy())
            .cloned()
            .unwrap_or_else(|| JValue::Str(String::new()));
        let mode = read::mode_for(provider.as_ref(), &model, &effort, &defaults);
        if !expected["threw"].is_null() {
            assert_eq!(
                mode,
                Err(read::Thrown),
                "{name}: Node threw {}",
                expected["threw"]
            );
            continue;
        }
        let mode = mode.unwrap();
        let admin = input["role"] == "admin";
        assert_eq!(expected["status"], 200, "{name}");
        assert_eq!(
            read::answer(&global, &effort, mode, admin),
            expected["reply"].as_str().unwrap(),
            "{name}"
        );
    }
    assert_eq!(differences, 2, "the documented differences");
}

#[test]
fn recorder_sources_are_recorded() {
    let fixture: Value = serde_json::from_str(FIXTURE).unwrap();
    for file in [
        "server/routes/reasoning-settings.cjs",
        "server/reasoning-effort.cjs",
        "server/providers.cjs",
        "server/workspace.cjs",
        "server/index.cjs",
    ] {
        assert_eq!(
            fixture["sources"][file].as_str().unwrap().len(),
            64,
            "{file}"
        );
    }
}
