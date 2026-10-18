#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]
//! The actual Python differential: for every case of `tests/fixtures/fit-python.v1.json`, the
//! native backend list (descriptors plus `parse_baseline`) is `helpers._backend_list()`, and
//! [`KNOWN_KEYS`] is `sorted(ini.ALL_KNOWN_KEYS)`. Every name is synthetic.
use super::*;
use crate::backend_stats::fit::tests::{fixture, stats_for};
use std::sync::Arc;

fn shown(v: &Value) -> String {
    let mut out = String::new();
    crate::section_autoconfig::render::write(&mut out, v);
    out
}

#[test]
fn the_known_keys_are_pythons() {
    let fx = fixture();
    let python: Vec<&str> = fx["known_keys"]
        .as_array()
        .unwrap()
        .iter()
        .map(|k| k.as_str().unwrap())
        .collect();
    assert_eq!(KNOWN_KEYS, python.as_slice());
}

#[tokio::test]
async fn the_backend_list_is_pythons() {
    let fx = fixture();
    let mut with_baseline = 0;
    for case in fx["cases"].as_array().unwrap() {
        let id = case["id"].as_str().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let stats = stats_for(case, dir.path());
        let native = backend_list(&stats.descriptors().await.unwrap()).expect(id);
        let python = model_files::json::parse(case["backend_list"].as_str().unwrap()).unwrap();
        assert_eq!(shown(&native), shown(&python), "{id}");
        if shown(&python).contains("\"baseline\":{\"") {
            with_baseline += 1;
        }
    }
    assert!(with_baseline >= 2);
}

/// The route uses the native list once a sampler is set: its answer is the one the child would
/// give carrying that list, whatever list the child itself carried.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_route_takes_the_native_list() {
    use crate::section_autoconfig::tests::{first_case, run, shown, stand_in};
    let fx = fixture();
    let fit_case = fx["cases"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["id"] == "baselines")
        .unwrap();
    let dir = tempfile::tempdir().unwrap();
    let stats = Arc::new(stats_for(fit_case, dir.path()));
    let native = backend_list(&stats.descriptors().await.unwrap()).unwrap();
    let (base, stand) = stand_in().await;
    let client = reqwest::Client::new();
    let fixture = crate::section_autoconfig::tests::fixture();
    let path = fixture["cases"][0]["results"][0]["path"]
        .as_str()
        .unwrap()
        .to_owned();
    let body = fixture["cases"][0]["results"][0]["runtime"]["body"]
        .as_str()
        .unwrap()
        .to_owned();
    let with = |list: &str| {
        let mut runtime: serde_json::Map<String, serde_json::Value> =
            serde_json::from_str(&body).unwrap();
        runtime.insert("backends".into(), serde_json::Value::String(list.into()));
        serde_json::to_string(&runtime).unwrap()
    };
    let mut answers = Vec::new();
    for (list, sampler) in [
        (shown_json(&native), false),
        ("[]".to_owned(), true),
        ("[]".to_owned(), false),
    ] {
        let models = tempfile::tempdir().unwrap();
        let (autoconfig, _) = first_case(models.path());
        if sampler {
            autoconfig.set_stats(stats.clone());
        }
        stand.lock().unwrap().reply = Some((200, with(&list)));
        answers.push(shown(run(&autoconfig, &path, &base, &client).await));
    }
    assert_eq!(answers[0].0, 200);
    assert_eq!(answers[1], answers[0], "the native list is used");
    assert_ne!(
        answers[2], answers[0],
        "the child list alone answers otherwise"
    );
}

/// The list as the child carries it (`json.dumps`, NaN allowed; these values are finite).
fn shown_json(v: &Value) -> String {
    shown(v)
}

/// A command this side does not render (a non-string entry), and one the baseline rule refuses
/// (past its argument cap), keep the child's list for that answer.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_route_keeps_the_child_list_when_it_cannot_build_its_own() {
    use crate::section_autoconfig::tests::{first_case, run, shown, stand_in};
    let fx = fixture();
    let fit_case = fx["cases"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["id"] == "baselines")
        .unwrap();
    let (base, stand) = stand_in().await;
    let client = reqwest::Client::new();
    let fixture = crate::section_autoconfig::tests::fixture();
    let path = fixture["cases"][0]["results"][0]["path"]
        .as_str()
        .unwrap()
        .to_owned();
    let body = fixture["cases"][0]["results"][0]["runtime"]["body"]
        .as_str()
        .unwrap()
        .to_owned();
    stand.lock().unwrap().reply = Some((200, body));
    let answer = |stats: Option<Arc<crate::backend_stats::BackendStats>>| {
        let (path, base, client) = (&path, &base, &client);
        async move {
            let models = tempfile::tempdir().unwrap();
            let (autoconfig, _) = first_case(models.path());
            if let Some(stats) = stats {
                autoconfig.set_stats(stats);
            }
            shown(run(&autoconfig, path, base, client).await)
        }
    };
    let child_only = answer(None).await;
    assert_eq!(child_only.0, 200);
    let long: Vec<serde_json::Value> = (0..=model_autoconfig::baseline::MAX_ARGS)
        .map(|i| serde_json::Value::String(format!("--synthetic-{i}")))
        .collect();
    for cmd in [
        serde_json::json!(["-ngl", 99]),
        serde_json::Value::Array(long),
    ] {
        let mut case = fit_case.clone();
        case["docker"]["containers"][0]["Config"]["Cmd"] = cmd;
        let dir = tempfile::tempdir().unwrap();
        let stats = Arc::new(stats_for(&case, dir.path()));
        assert_eq!(answer(Some(stats)).await, child_only);
    }
}

#[test]
fn each_fallback_reason_is_logged_once() {
    assert!(fell_back("a synthetic reason for this test"));
    assert!(!fell_back("a synthetic reason for this test"));
    assert!(fell_back("another synthetic reason for this test"));
}
