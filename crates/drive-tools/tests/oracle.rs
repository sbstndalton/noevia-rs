//! Differential test against the Node oracle: `tests/fixtures/drive-oracle.v1.json` was recorded
//! by `handoff/foundation/R5/scripts/record-oracles.cjs` running noevia-core's gdrive-tools.cjs
//! `execute` and `describeTarget` with the Drive file module stubbed (synthetic data only).
//! Each case replays the same call through this crate's pure functions, with the stub's answer
//! as input, and must give the recorded text, the recorded expectVersion and the recorded plan.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]

use drive_tools::*;
use serde_json::Value;

const ORACLE: &str = include_str!("fixtures/drive-oracle.v1.json");
const USER: &str = "user-1";

fn s<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key).and_then(Value::as_str)
}

fn file_from(v: &Value) -> FileLine {
    FileLine {
        name: s(v, "name").unwrap_or("").to_owned(),
        id: s(v, "id").unwrap_or("").to_owned(),
        mime_type: s(v, "mimeType").map(str::to_owned),
        size: s(v, "size").map(str::to_owned),
        modified_time: s(v, "modifiedTime").map(str::to_owned),
        web_view_link: s(v, "webViewLink").map(str::to_owned),
    }
}

fn files_from(v: &Value) -> Vec<FileLine> {
    v.as_array()
        .map(|a| a.iter().map(file_from).collect())
        .unwrap_or_default()
}

/// The stub's error (`{error: {publicMessage}}`) as the public message, if it threw.
fn thrown(stub: &Value) -> Option<Option<&str>> {
    stub.get("error").map(|e| s(e, "publicMessage"))
}

fn plan_json(tool: &Value, args: &Value) -> Value {
    let name = s(args, "name");
    let file_id = s(args, "fileId");
    let Some(t) = tool.as_str().and_then(Tool::from_name) else {
        return serde_json::json!({"kind": "null"});
    };
    match plan_write(t, name, file_id) {
        WritePlan::NoTarget => serde_json::json!({"kind": "null"}),
        WritePlan::NewFile { target } => serde_json::json!({"target": target, "kind": "drive-new"}),
        WritePlan::ExistingFile { .. } => serde_json::json!({"kind": "drive"}),
        WritePlan::Refused(text) => serde_json::json!({"error": text}),
    }
}

#[test]
fn every_recorded_case_replays_to_the_node_answer() {
    let oracle: Value = serde_json::from_str(ORACLE).unwrap();
    let cap = oracle["cap"].as_u64().unwrap() as usize;
    let cases = oracle["cases"].as_array().unwrap();
    let mut gate = ReadGate::new();
    let mut checked = 0;

    for case in cases {
        match s(case, "op").unwrap() {
            "plan" => {
                let got = plan_json(&case["tool"], &case["args"]);
                assert_eq!(got, case["expect"], "plan case {}", case["args"]);
            }
            "execute" => {
                let tool = s(case, "tool").unwrap();
                let scope_v = &case["scope"];
                let scope = scope_key(s(scope_v, "exchangeKey"), s(scope_v, "chatKey"));
                let now = case["now"].as_u64().unwrap();
                let stub = &case["stub"];
                let expect = s(case, "expect").unwrap();

                let got: String = if case["connected"] == Value::Bool(false) {
                    NOT_CONNECTED.to_owned()
                } else if let Some(err) = thrown(stub) {
                    error_reply(err)
                } else {
                    match tool {
                        "drive_search_files" => found_reply(&files_from(&stub["value"])),
                        "drive_list_recent" => recent_reply(&files_from(&stub["value"])),
                        "drive_get_metadata" => metadata_reply(&file_from(&stub["value"])),
                        "drive_create_file" => {
                            let v = &stub["value"];
                            created_reply(
                                s(v, "name").unwrap(),
                                s(v, "id").unwrap(),
                                s(v, "webViewLink"),
                            )
                        }
                        "drive_trash_file" => trashed_reply(s(&stub["value"], "name").unwrap()),
                        "drive_read_file" => {
                            let v = &stub["value"];
                            let meta = &v["meta"];
                            let truncated = v["truncated"].as_bool().unwrap();
                            let reply = shape_read(
                                s(meta, "name").unwrap(),
                                s(v, "text").unwrap(),
                                truncated,
                                cap,
                            );
                            gate.remember(
                                USER,
                                &scope,
                                s(meta, "id").unwrap(),
                                s(meta, "version"),
                                reply.complete,
                                now,
                            );
                            reply.text
                        }
                        "drive_update_file" => {
                            let args = &case["args"];
                            match check_update(&gate, USER, &scope, s(args, "fileId"), now) {
                                Err(text) => text.to_owned(),
                                Ok(check) => {
                                    let expected_version = match &check {
                                        UpdateCheck::ExpectVersion(v) => Some(v.as_str()),
                                        UpdateCheck::NotDriveId => None,
                                    };
                                    assert_eq!(
                                        case.get("expectVersion").and_then(|v| v
                                            .as_str()
                                            .map(str::to_owned)
                                            .or_else(|| v.as_u64().map(|n| n.to_string()))),
                                        expected_version.map(str::to_owned),
                                        "expectVersion for {}",
                                        args
                                    );
                                    let f = &stub["value"];
                                    let id = s(f, "id").unwrap();
                                    gate.remember(USER, &scope, id, s(f, "version"), true, now);
                                    updated_reply(s(f, "name").unwrap(), id)
                                }
                            }
                        }
                        other => panic!("unexpected tool {other}"),
                    }
                };
                assert_eq!(got, expect, "{tool} at {now} in {scope}");
            }
            other => panic!("unexpected op {other}"),
        }
        checked += 1;
    }
    assert_eq!(checked, cases.len());
    assert!(
        checked >= 30,
        "the fixture should hold the whole recorded set"
    );
}
