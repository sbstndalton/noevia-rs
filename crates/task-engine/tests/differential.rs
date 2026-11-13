//! Replays the operations recorded in `fixtures/node-journals.v1.json` (written by jobs.cjs itself,
//! see tools/gen-node-journal-fixtures.cjs) through `task-engine` and compares every event row and
//! the folded view with what Node wrote and read.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use serde_json::Value;
use task_engine::{Event, JobStore, Limits, NewJob};

const FIXTURES: &str = include_str!("fixtures/node-journals.v1.json");

fn scenarios() -> Vec<Value> {
    let doc: Value = serde_json::from_str(FIXTURES).expect("fixture JSON");
    doc["scenarios"].as_array().expect("scenarios").clone()
}

fn row_to_event(row: &Value) -> Event {
    Event {
        job: row["job"].as_str().unwrap().to_string(),
        seq: row["seq"].as_u64().unwrap(),
        kind: row["type"].as_str().unwrap().to_string(),
        at: row["at"].as_u64().unwrap(),
        data: row["data"].clone(),
    }
}

/// Runs one recorded scenario. Returns the store, the job id and the refusal flags Rust produced.
fn replay(s: &Value) -> (JobStore, String, Vec<bool>) {
    let rows = s["journal"].as_array().unwrap();
    let id = rows[0]["job"].as_str().unwrap().to_string();
    let mut store = JobStore::new(Limits::default());
    let mut refused = Vec::new();
    for op in s["ops"].as_array().unwrap() {
        // The clock: each event takes the `at` of the next journal row, as the Node run did.
        let next_at = |store: &JobStore| -> u64 {
            let written = store.events(&id).map_or(0, |e| e.len());
            rows.get(written).map_or(0, |r| r["at"].as_u64().unwrap())
        };
        let at = next_at(&store);
        let result = match op["op"].as_str().unwrap() {
            "create" => {
                let caps: Vec<String> = op["capabilities"]
                    .as_array()
                    .map(|list| {
                        list.iter()
                            .filter_map(|c| c.as_str().map(str::to_string))
                            .collect()
                    })
                    .unwrap_or_default();
                let project = op["projectId"].as_str();
                store
                    .create(
                        &id,
                        at,
                        NewJob {
                            kind: op["kind"].as_str().unwrap(),
                            project_id: project,
                            parent_id: None,
                            capabilities: &caps,
                        },
                    )
                    .map(|_| ())
            }
            "start" => store.start(&id, at).map(|_| ()),
            "progress" => store
                .append(
                    &id,
                    at,
                    "progress",
                    serde_json::json!({ "stage": op["stage"] }),
                )
                .map(|_| ()),
            "request" => {
                // The fixture cards carry their own id; the host's approval id is that id.
                let approval_id = op["card"]["id"].as_str().unwrap().to_string();
                store
                    .request_approval(&id, at, &approval_id, op["card"].clone())
                    .map(|_| ())
            }
            "decide" => store
                .decide_approval(
                    &id,
                    at,
                    op["approvalId"].as_str().unwrap(),
                    op["decision"].as_str().unwrap(),
                )
                .map(|_| ()),
            "complete" => store.complete(&id, at, op["result"].clone()).map(|_| ()),
            "fail" => store
                .fail(&id, at, op["error"].as_str().unwrap())
                .map(|_| ()),
            "cancel" => store.cancel(&id, at).map(|_| ()),
            "interrupt" => store
                .interrupt(&id, at, op["reason"].as_str().unwrap())
                .map(|_| ()),
            "recover" => {
                store.recover(at, &|_| false);
                Ok(())
            }
            "append" => store
                .append(&id, at, op["type"].as_str().unwrap(), op["data"].clone())
                .map(|_| ()),
            other => panic!("unknown op {other}"),
        };
        refused.push(result.is_err());
    }
    (store, id, refused)
}

#[test]
fn every_agreed_scenario_matches_node() {
    for s in scenarios() {
        let name = s["name"].as_str().unwrap().to_string();
        if s["agree"] != Value::Bool(true) {
            continue;
        }
        let (store, id, refused) = replay(&s);

        // Refusals: a Node refusal is a Rust refusal, and nothing else is.
        let node_refused: Vec<bool> = s["refusals"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| !r.is_null())
            .collect();
        assert_eq!(refused, node_refused, "{name}: refusals");

        // Every event row, in order, as Node wrote it.
        let rows = s["journal"].as_array().unwrap();
        // The engine adds its approvalId key to approval.requested data; Node's rows do not have it.
        let got: Vec<Event> = store
            .events(&id)
            .expect("journal")
            .iter()
            .map(|e| {
                let mut e = e.clone();
                if e.kind == "approval.requested" {
                    e.data.as_object_mut().unwrap().remove("approvalId");
                }
                e
            })
            .collect();
        let want: Vec<Event> = rows.iter().map(row_to_event).collect();
        assert_eq!(got, want, "{name}: journal rows");

        // The folded view, as Node's get() derived it.
        let view = store.get(&id).expect("view");
        let e = &s["expect"];
        assert_eq!(
            view.status.name(),
            e["status"].as_str().unwrap(),
            "{name}: status"
        );
        assert_eq!(view.kind, e["kind"].as_str().unwrap(), "{name}: kind");
        assert_eq!(
            view.project_id.as_deref(),
            e["projectId"].as_str(),
            "{name}: projectId"
        );
        assert_eq!(
            view.parent_id.as_deref(),
            e["parentId"].as_str(),
            "{name}: parentId"
        );
        let caps: Vec<&str> = view.capabilities.iter().map(String::as_str).collect();
        let want_caps: Vec<&str> = e["capabilities"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| c.as_str().unwrap())
            .collect();
        assert_eq!(caps, want_caps, "{name}: capabilities");
        assert_eq!(view.stage.as_deref(), e["stage"].as_str(), "{name}: stage");
        let pending = view.pending_approval.clone().unwrap_or(Value::Null);
        assert_eq!(pending, e["pendingApproval"], "{name}: pendingApproval");
        assert_eq!(view.result, e["result"], "{name}: result");
        assert_eq!(view.error.as_deref(), e["error"].as_str(), "{name}: error");
        assert_eq!(view.lifecycle, e["lifecycle"].as_str(), "{name}: lifecycle");
        assert_eq!(
            view.created_at,
            e["createdAt"].as_u64().unwrap(),
            "{name}: createdAt"
        );
        assert_eq!(
            view.updated_at,
            e["updatedAt"].as_u64().unwrap(),
            "{name}: updatedAt"
        );
    }
}

#[test]
fn journal_round_trips_through_jsonl_and_loads_back() {
    for s in scenarios() {
        if s["agree"] != Value::Bool(true) {
            continue;
        }
        let rows = s["journal"].as_array().unwrap();
        let events: Vec<Event> = rows.iter().map(row_to_event).collect();
        let text = task_engine::to_jsonl(&events);
        let parsed = task_engine::parse_jsonl(&text).expect("parse");
        assert_eq!(parsed, events, "{}", s["name"]);
        let mut fresh = JobStore::new(Limits::default());
        let loaded = fresh.load(parsed).expect("load");
        let folded = task_engine::fold(&events).expect("fold");
        assert_eq!(loaded, &folded, "{}", s["name"]);
    }
}

/// The one deliberate difference from jobs.cjs (see the scenario's name): a second start is refused.
#[test]
fn second_start_is_refused_where_node_accepts_it() {
    let all = scenarios();
    let s = all
        .iter()
        .find(|s| s["name"] == "start-twice-refused-by-the-engine-only")
        .unwrap();
    let (store, id, refused) = replay(s);
    assert_eq!(refused, vec![false, false, true]);
    assert_eq!(store.get(&id).unwrap().status.name(), "running");
    assert_eq!(store.events(&id).unwrap().len(), 2);
}
