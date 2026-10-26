#![allow(clippy::expect_used, clippy::unwrap_used, clippy::indexing_slicing)]
use serde_json::Value;
use server_features::{
    evaluate, evaluate_with_context, parse_env, spec, Availability, AvailabilityKind, Feature,
};

fn input<'a>(case: &'a Value, key: &str) -> &'a str {
    case[key].as_str().expect("source-derived text")
}

fn assert_evaluation(result: server_features::Evaluation, expected: &Value, case: &Value) {
    assert_eq!(result.value, expected["value"], "{case}");
    assert_eq!(result.source.as_str(), expected["source"], "{case}");
    assert_eq!(result.locked, expected["locked"], "{case}");
    assert_eq!(result.available, expected["available"], "{case}");
    assert_eq!(result.enabled, expected["enabled"], "{case}");
    assert_eq!(result.enabled, expected["flag"], "{case}");
    assert_eq!(
        result.pending_restart,
        expected["pendingRestart"].as_bool(),
        "{case}"
    );
}

#[test]
fn complete_catalog_and_evaluation_match_selected_core() {
    let fixture: Value = serde_json::from_str(include_str!("fixtures/core-feature-eval-v2.json"))
        .expect("selected-core oracle JSON");
    let contract: Value =
        serde_json::from_str(include_str!("../../../contracts/features/catalog.json"))
            .expect("canonical catalog JSON");
    assert_eq!(
        fixture["source"]["core"],
        "21c65f2039632a77a3827533429ff87c4a936f7b"
    );
    assert_eq!(contract["version"], 1);
    assert_eq!(contract["source"]["commit"], fixture["source"]["core"]);
    assert_eq!(
        contract["source"]["featuresSha256"],
        fixture["source"]["featuresSha256"]
    );
    let definitions = fixture["definitions"].as_array().expect("definitions");
    let catalog = contract["features"].as_array().expect("catalog features");
    assert_eq!(definitions.len(), 23);
    assert_eq!(catalog.len(), 23);
    assert_eq!(Feature::ALL.len(), 23);
    for (i, (definition, canonical)) in definitions.iter().zip(catalog).enumerate() {
        let name = input(definition, "name");
        let feature = Feature::from_name(name).expect("known exact feature name");
        assert_eq!(feature, Feature::ALL[i], "{name}");
        let metadata = spec(feature);
        assert_eq!(metadata.name, name);
        assert_eq!(metadata.env, definition["env"]);
        assert_eq!(metadata.setting, definition["setting"]);
        assert_eq!(metadata.default, definition["default"]);
        assert_eq!(metadata.experimental, definition["experimental"]);
        assert_eq!(metadata.restart, definition["restart"]);
        assert_eq!(metadata.legacy, definition["legacy"].as_str());
        assert_eq!(metadata.label, definition["label"]);
        assert_eq!(metadata.description, definition["description"]);
        assert_eq!(
            metadata.availability != AvailabilityKind::Static,
            definition["dynamicAvailability"],
            "{name}"
        );
        assert_eq!(canonical["id"], name);
        assert_eq!(canonical["env"], definition["env"]);
        assert_eq!(canonical["default"], definition["default"]);
        assert_eq!(canonical["experimental"], definition["experimental"]);
        assert_eq!(canonical["restart"], definition["restart"]);
        assert_eq!(canonical["legacy"], definition["legacy"]);
        assert_eq!(canonical["label"], definition["label"]);
        assert_eq!(canonical["description"], definition["description"]);
    }
    let cases = fixture["cases"].as_array().expect("oracle cases");
    assert_eq!(cases.len(), 4508);
    for case in cases {
        let feature = Feature::from_name(input(case, "name")).expect("known feature");
        let expected = &case["expected"];
        let parsed = parse_env(case["envRaw"].as_str());
        if let Some(error) = expected["error"].as_str() {
            assert_eq!(parsed.unwrap_err(), error, "{case}");
            continue;
        }
        let available = if case["available"].as_bool() == Some(true) {
            Availability::Available
        } else {
            Availability::Unavailable
        };
        let boot = expected["value"].as_bool();
        let result = evaluate_with_context(
            feature,
            parsed.expect("valid oracle env"),
            case["stored"].as_str(),
            available,
            boot,
        );
        assert_evaluation(result, expected, case);
    }
    let transitions = fixture["transitions"].as_array().expect("transitions");
    assert_eq!(transitions.len(), 92);
    for case in transitions {
        let feature = Feature::from_name(input(case, "name")).expect("known feature");
        let next = case["next"].as_bool().expect("new value");
        let result = evaluate_with_context(
            feature,
            None,
            Some(if next { "true" } else { "false" }),
            Availability::Available,
            case["boot"].as_bool(),
        );
        assert_evaluation(result, &case["expected"], case);
    }
    let legacy = &fixture["legacy"];
    assert_eq!(Feature::from_name("astraReview"), None);
    assert_eq!(
        Feature::from_input_name("astraReview"),
        Some(Feature::PlannerReview)
    );
    assert_eq!(legacy["resolved"], "plannerReview");
    assert_eq!(legacy["migrated"], "true");
    assert_eq!(legacy["original"], "true");
    assert_eq!(legacy["before"]["source"], "admin");
    assert_eq!(legacy["before"]["enabled"], true);
    assert_eq!(legacy["updated"], "false");
    assert_eq!(legacy["after"]["enabled"], false);
}

#[test]
fn unknown_runtime_facts_fail_closed_and_three_native_consumers_keep_their_api() {
    assert_eq!(Feature::from_name("nativeClientAuth "), None);
    assert_eq!(Feature::from_input_name("rustStorageReads"), None);
    assert!(
        !evaluate_with_context(
            Feature::BrowserExecutor,
            Some(true),
            None,
            Availability::Unknown,
            None
        )
        .enabled
    );
    let restart = evaluate_with_context(
        Feature::Kiwix,
        Some(true),
        None,
        Availability::Available,
        None,
    );
    assert!(!restart.enabled);
    assert_eq!(restart.pending_restart, None);
    for feature in [
        Feature::NativeClientAuth,
        Feature::RoutingModes,
        Feature::ChatgptOAuth,
    ] {
        assert!(!evaluate(feature, None, Some("false"), true).enabled);
        assert!(evaluate(feature, None, Some("true"), true).enabled);
    }
    assert!(!evaluate(Feature::NativeClientAuth, Some(true), None, false).enabled);
    assert!(!evaluate(Feature::BrowserExecutor, Some(true), None, true).enabled);
}
