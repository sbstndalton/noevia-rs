//! noevia#1908: FastAPI's 422 bodies, byte for byte (key order included), against the actual
//! Python answers in `fixtures/validation-422-python.v1.json` (`tools/gen-validation-422-oracle.py`;
//! CI regenerates it from the pinned services ref and requires identical bytes).
#![cfg(feature = "native-prompts")]
#![allow(clippy::unwrap_used, clippy::indexing_slicing)]
use noevia_model_manager::{prompts, validation};
use serde_json::Value;

const API_SHA: &str = "800edae73e7f7764845510ddf4e108bc2ea0a639f721e06e825a4a4644600dd6";
const MAIN_SHA: &str = "87eeac941fa5c0687160f315517b60c02b29324266d780851c547f6a1d749ee2";

fn fixture() -> Value {
    serde_json::from_str(include_str!("fixtures/validation-422-python.v1.json")).unwrap()
}

#[test]
fn fixture_provenance() {
    let f = fixture();
    assert_eq!(f["oracle"]["api.py"], API_SHA);
    assert_eq!(f["oracle"]["main.py"], MAIN_SHA);
}

/// The native prompt writer's body pipeline, as `lib.rs` runs it.
fn body_answer(body: &str, content_type: Option<&str>) -> Option<String> {
    let bytes = body.as_bytes();
    if !prompts::json_content_type(content_type) && !bytes.is_empty() {
        return Some(
            prompts::non_json_validation(bytes)
                .unwrap()
                .get()
                .to_owned(),
        );
    }
    let decoded = prompts::decode_body(bytes).unwrap();
    prompts::validation_with_empty(decoded.as_bytes(), bytes.is_empty()).map(|v| v.get().to_owned())
}

#[test]
fn raw_bytes_match_python() {
    let mut checked = 0;
    for case in fixture()["cases"].as_array().unwrap() {
        let answer = case["answer"].as_str().unwrap();
        let got = match case["kind"].as_str().unwrap() {
            "body" => {
                let got = body_answer(
                    case["body"].as_str().unwrap(),
                    case["content_type"].as_str(),
                );
                if case["status"] == 500 {
                    // Non-finite input: Python fails to render it; the native writer keeps its 400.
                    assert_eq!(got, None, "{case}");
                    continue;
                }
                got.unwrap()
            }
            "pid" => {
                let raw = case["raw"].as_str().unwrap().replace("%20", " ");
                validation::parsing(
                    "int_parsing",
                    &[validation::key("path"), validation::key("pid")],
                    "Input should be a valid integer, unable to parse string as an integer",
                    &raw,
                )
                .get()
                .to_owned()
            }
            _ => validation::parsing(
                "bool_parsing",
                &[validation::key("query"), validation::key("defaults")],
                "Input should be a valid boolean, unable to interpret input",
                case["raw"].as_str().unwrap(),
            )
            .get()
            .to_owned(),
        };
        assert_eq!(case["status"], 422, "{case}");
        assert_eq!(got, answer, "{case}");
        // Through axum's Json the raw value is written verbatim.
        let raw: Box<serde_json::value::RawValue> =
            serde_json::value::RawValue::from_string(got).unwrap();
        assert_eq!(serde_json::to_string(&raw).unwrap(), answer);
        checked += 1;
    }
    assert!(checked >= 25);
}
