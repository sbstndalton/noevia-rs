#![allow(clippy::unwrap_used, clippy::indexing_slicing)]
use super::*;
use serde_json::Value as Json;
fn fixture() -> Json {
    serde_json::from_str(include_str!(
        "../../../tests/fixtures/bench-chart-python.v1.json"
    ))
    .unwrap()
}
fn value(v: &Json) -> Value {
    model_files::json::parse(&v.to_string()).unwrap()
}
#[test]
fn actual_python_predictions_and_capacity() {
    let fx = fixture();
    for case in fx["cases"].as_array().unwrap() {
        let dir = tempfile::tempdir().unwrap();
        let models = super::super::tests::build(dir.path(), case);
        let settings = super::super::tests::settings(&models, case["cache_cap"].as_i64().unwrap());
        let backends = value(&case["backends"]);
        assert_eq!(
            capacity(Some(&backends)),
            case["runtime"]["capacity_gb"].as_f64().unwrap()
        );
        for alias in case["aliases"].as_array().unwrap() {
            let alias = alias.as_str().unwrap();
            let got = prediction(
                &settings,
                crate::sections::access::Root::synthetic,
                alias,
                Some(&backends),
            )
            .unwrap_or(Value::Null);
            let mut text = String::new();
            render::write(&mut text, &got);
            assert_eq!(
                serde_json::from_str::<Json>(&text).unwrap(),
                case["runtime"]["predicted"][alias],
                "{} {alias}",
                case["id"]
            );
        }
    }
}
#[test]
fn actual_python_context_selection_and_first_tie() {
    for case in fixture()["selections"].as_array().unwrap() {
        let got =
            select(&value(&case["plan"]), case["ctx"].as_str().unwrap()).unwrap_or(Value::Null);
        let mut text = String::new();
        render::write(&mut text, &got);
        assert_eq!(
            serde_json::from_str::<Json>(&text).unwrap(),
            case["expected"],
            "{}",
            case["ctx"]
        );
    }
}

#[test]
fn fixture_records_the_pinned_helper_source() {
    assert_eq!(
        fixture()["oracle"]["helpers.py"],
        "fcf2ccb14e5e068f97bb7a4b7985ab7f4f4855b940eb6d9e3838f239ad97918d"
    );
}

/// Chart timeouts and optional autoconfig inventory use the same sampler cooldown.
/// A responsive positive control still supplies capacity; an actual hung Unix peer gets only
/// one read while both optional consumers keep their documented zero/null or child fallback.
#[tokio::test]
async fn chart_timeout_cooldown_is_shared_with_optional_inventory() {
    use crate::backend_stats::fit::tests::{fixture, hung_stats, stats_for};
    use crate::section_autoconfig::{tests::first_case, Runtime};
    use std::sync::{atomic::Ordering, Arc};
    use std::time::{Duration, Instant};
    let fit = fixture();
    let case = fit["cases"]
        .as_array()
        .unwrap()
        .iter()
        .find(|case| case["id"] == "baselines")
        .unwrap();
    let positive_dir = tempfile::tempdir().unwrap();
    let positive_stats = Arc::new(stats_for(case, positive_dir.path()));
    let models = tempfile::tempdir().unwrap();
    let (positive, _) = first_case(models.path());
    positive.set_stats(positive_stats.clone());
    let expected = capacity(Some(
        &inventory::backend_list(&positive_stats.descriptors().await.unwrap()).unwrap(),
    ));
    assert!(expected > 0.0);
    let positive_body: Json =
        serde_json::from_slice(&positive.chart_runtime(&[]).await.unwrap()).unwrap();
    assert_eq!(positive_body["capacity_gb"], expected);

    let dir = tempfile::tempdir().unwrap();
    let (stats, calls, peer) = hung_stats(dir.path());
    let models = tempfile::tempdir().unwrap();
    let (autoconfig, _) = first_case(models.path());
    autoconfig.set_stats(Arc::new(stats));
    let aliases = vec!["qa-absent-alias".to_owned()];
    let started = Instant::now();
    let body = tokio::time::timeout(Duration::from_secs(20), autoconfig.chart_runtime(&aliases))
        .await
        .unwrap()
        .unwrap();
    assert!(started.elapsed() >= Duration::from_secs(9));
    assert!(started.elapsed() < Duration::from_secs(20));
    assert_eq!(
        serde_json::from_slice::<Json>(&body).unwrap(),
        serde_json::json!({"capacity_gb":0.0,"predicted":{"qa-absent-alias":null}})
    );
    assert_eq!(calls.load(Ordering::Relaxed), 1);
    let child = model_files::json::parse(r#"[{"name":"qa-child"}]"#).unwrap();
    let mut runtime = Runtime {
        measured: "{}".into(),
        history: "[]".into(),
        prompt_tps: 0.0,
        backends: child.clone(),
    };
    let retry = Instant::now();
    for _ in 0..10 {
        tokio::time::timeout(Duration::from_secs(2), autoconfig.inventory(&mut runtime))
            .await
            .unwrap();
        assert_eq!(runtime.backends, child);
        let repeated =
            tokio::time::timeout(Duration::from_secs(2), autoconfig.chart_runtime(&aliases))
                .await
                .unwrap()
                .unwrap();
        assert_eq!(repeated, body);
    }
    assert!(retry.elapsed() < Duration::from_secs(2));
    assert_eq!(calls.load(Ordering::Relaxed), 1);
    peer.abort();
}
