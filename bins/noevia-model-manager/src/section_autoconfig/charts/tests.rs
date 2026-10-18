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
