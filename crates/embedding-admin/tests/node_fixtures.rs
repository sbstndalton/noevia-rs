//! Replays fixtures recorded from noevia-core's own `server/routes/embedding.cjs` (see
//! `generate_fixtures.cjs` and the source hashes in the fixture file): every request the Node route
//! answered, and the status of every two-half answer it built.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]

use embedding_admin::{aggregate_status, read_request, served_probes, Action, Scope};
use serde_json::Value;

const FIXTURE: &str = include_str!("fixtures/node-embedding.v1.json");

fn fixture() -> Value {
    serde_json::from_str(FIXTURE).unwrap()
}

#[test]
fn the_fixture_names_its_node_sources() {
    let f = fixture();
    assert_eq!(f["version"], 1);
    assert_eq!(
        f["sources"]["server/routes/embedding.cjs"],
        "ae34b6be30f2b8880a5c6c0c141ed9d1e709e87ef92202bb3cc0e79465db9854"
    );
    assert!(f["reads"].as_array().unwrap().len() >= 90);
    assert!(f["aggregates"].as_array().unwrap().len() >= 36);
}

#[test]
fn every_recorded_request_is_read_as_node_read_it() {
    for case in fixture()["reads"].as_array().unwrap() {
        let raw = case["raw"].as_str().unwrap();
        let body: Value = serde_json::from_str(raw).unwrap();
        let status = u16::try_from(case["status"].as_u64().unwrap()).unwrap();
        let code = case["code"].as_str();
        let read = read_request(&body);
        // A 400 is always a reading refusal, with Node's message.
        if status == 400 {
            let refusal = read.expect_err(raw);
            assert_eq!(refusal.status, 400, "{raw}");
            assert_eq!(
                Some(refusal.error.as_str()),
                case["error"].as_str(),
                "{raw}"
            );
            continue;
        }
        let request =
            read.unwrap_or_else(|r| panic!("{raw}: refused {r:?}, Node answered {status}"));
        if request.action.is_service() {
            // The service path answers from the embedding service's state (here: not managed).
            assert_eq!(code, Some("notManaged"), "{raw}");
            continue;
        }
        let probes = served_probes(&request);
        if code == Some("diaryEndpointRequired") {
            let refusal = probes.expect_err(raw);
            assert_eq!(
                (refusal.status, Some(refusal.error.as_str())),
                (409, case["error"].as_str()),
                "{raw}"
            );
            continue;
        }
        let probes = probes.unwrap_or_else(|r| panic!("{raw}: probes refused {r:?}"));
        let recorded: Vec<Option<String>> = case["served_probes"]
            .as_array()
            .unwrap()
            .iter()
            .map(|p| p.as_str().map(str::to_string))
            .collect();
        assert_eq!(probes, recorded, "{raw}");
        if let Some(target) = &request.target {
            assert_eq!(
                Some(target.model.as_str()),
                case["served_model"].as_str(),
                "{raw}"
            );
        }
        // Which halves ran, and the force flag the projects switch received.
        let projects_ran = !case["projects"].as_array().unwrap().is_empty();
        let diary_ran = !case["diary"].as_array().unwrap().is_empty();
        if probes.is_empty() || code.is_none() {
            assert_eq!(request.scope.includes_projects(), projects_ran, "{raw}");
            assert_eq!(request.scope.includes_diary(), diary_ran, "{raw}");
        }
        if request.action == Action::Switch {
            for force in case["switch_force"].as_array().unwrap() {
                assert_eq!(force.as_bool(), Some(request.force), "{raw}");
            }
        }
        for sent in case["diary"].as_array().unwrap() {
            assert_eq!(
                sent["action"].as_str(),
                Some(request.action.as_str()),
                "{raw}"
            );
            assert_eq!(sent.get("force").is_some(), request.force, "{raw}");
        }
        if request.scope == Scope::All && request.action != Action::SetNext {
            assert!(projects_ran && diary_ran, "{raw}");
        }
    }
}

#[test]
fn every_recorded_two_half_answer_has_nodes_status() {
    for case in fixture()["aggregates"].as_array().unwrap() {
        let scope = Scope::parse(case["scope"].as_str().unwrap()).unwrap();
        let projects = (!case["projects"].is_null()).then(|| &case["projects"]);
        let diary = (!case["diary"].is_null()).then(|| &case["diary"]);
        assert_eq!(projects.is_some(), scope.includes_projects(), "{case}");
        assert_eq!(diary.is_some(), scope.includes_diary(), "{case}");
        assert_eq!(
            u64::from(aggregate_status(projects, diary)),
            case["status"].as_u64().unwrap(),
            "{case}"
        );
    }
}
