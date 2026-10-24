#![allow(clippy::expect_used, clippy::unwrap_used, clippy::indexing_slicing)]
use serde_json::Value;
use server_features::{evaluate, parse_env, spec, Feature};

#[test]
fn native_catalog_and_evaluation_match_selected_core_feature_service() {
    let fixture: Value = serde_json::from_str(include_str!("fixtures/core-feature-eval-v1.json"))
        .expect("selected-core oracle JSON");
    assert_eq!(
        fixture["source"]["core"],
        "21c65f2039632a77a3827533429ff87c4a936f7b"
    );
    let cases = fixture["cases"].as_array().expect("oracle cases");
    assert!(cases.len() > 500);
    for case in cases {
        let name = case["name"].as_str().expect("feature name");
        let feature = Feature::from_name(name).expect("catalog entry");
        let expected = &case["expected"];
        let raw = case["envRaw"].as_str();
        let parsed = parse_env(raw);
        if let Some(error) = expected["error"].as_str() {
            assert_eq!(parsed.unwrap_err(), error, "{case}");
            continue;
        }
        let result = evaluate(
            feature,
            parsed.expect("valid oracle env"),
            case["stored"].as_str(),
            case["trustProxy"].as_bool().expect("proxy boolean"),
        );
        let definition = spec(feature);
        assert_eq!(definition.name, name, "{case}");
        assert_eq!(definition.env, expected["env"], "{case}");
        assert_eq!(definition.setting, format!("feature:{name}"), "{case}");
        assert!(!definition.default, "{case}");
        assert_eq!(result.value, expected["value"], "{case}");
        assert_eq!(result.source.as_str(), expected["source"], "{case}");
        assert_eq!(result.locked, expected["locked"], "{case}");
        assert_eq!(result.available, expected["available"], "{case}");
        assert_eq!(result.enabled, expected["enabled"], "{case}");
        assert_eq!(result.enabled, expected["flag"], "{case}");
        assert_eq!(expected["pendingRestart"], false, "{case}");
    }
}

#[test]
fn unlisted_flag_names_are_rejected_and_fresh_settings_are_re_evaluated() {
    assert_eq!(Feature::from_name("astraReview"), None);
    assert_eq!(Feature::from_name("plannerReview"), None);
    assert_eq!(Feature::from_name("nativeClientAuth "), None);
    for feature in Feature::ALL {
        assert!(Feature::from_name(spec(feature).name).is_some());
        assert!(!evaluate(feature, None, Some("false"), true).enabled);
        assert!(evaluate(feature, None, Some("true"), true).enabled);
    }
    assert!(!evaluate(Feature::NativeClientAuth, Some(true), None, false).enabled);
}
