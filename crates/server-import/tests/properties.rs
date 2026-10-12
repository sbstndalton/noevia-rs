//! Deterministic property controls written from the actual source contract; unexecuted until
//! a separate runtime grant. No proptest dependency, random fixture generator or Node oracle.
#![allow(clippy::unwrap_used, clippy::indexing_slicing)]
use js_json::JValue;
use serde_json::{json, Value};
use server_import::{
    plan_import, Error, Project, Snapshot, Unsupported, FORMAT, MAX_BYTES, MAX_NODES,
};

fn input(chats: Value) -> JValue {
    js_json::parse(&json!({"format":FORMAT,"chats":chats}).to_string()).unwrap()
}

#[test]
fn all_admitted_plans_are_deterministic_unique_and_count_consistent() {
    for count in [0, 1, 2, 10, 99, 2000] {
        let data =
            input(json!((0..count).map(|i| json!({
            "id":format!("c-{i}"),"title":format!("Chat{i}"),
            "updatedAt":i,"project":if i%3==0 {json!({"name":"Grouped"})} else {Value::Null},
            "history":[{"role":"user","content":"test"},{"role":"system","content":"omit"}]
        })).collect::<Vec<_>>()));
        let context = Snapshot::empty(&[], 12.0);
        let bytes = plan_import(&data, context).unwrap();
        assert_eq!(plan_import(&data, context).unwrap(), bytes);
        assert!(bytes.len() <= MAX_BYTES);
        let plan: Value = serde_json::from_str(&bytes).unwrap();
        let free = plan["freeChats"].as_array().unwrap();
        let grouped = plan["projectChats"]
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|g| g["chats"].as_array().unwrap())
            .collect::<Vec<_>>();
        let ids = free
            .iter()
            .chain(grouped)
            .map(|m| m["id"].as_str().unwrap())
            .collect::<std::collections::HashSet<_>>();
        assert_eq!(ids.len(), count);
        assert_eq!(plan["imported"].as_u64().unwrap() as usize, count);
        assert_eq!(plan["histories"].as_object().unwrap().len(), count);
        assert!(plan["histories"]
            .as_object()
            .unwrap()
            .keys()
            .all(|id| ids.contains(id.as_str())));
    }
}

#[test]
fn role_text_and_tools_never_retain_untrusted_unknown_fields() {
    for role in ["user", "assistant", "system", "tool", "developer", ""] {
        for content in [json!("ok"), json!(false), json!(null), json!(42), json!({})] {
            let data = input(json!([{"id":"c-x","history":[{
                "role":role,"content":content,"reasoning":"marker_PRIVATE","arbitrary":"marker_PRIVATE",
                "toolCalls":[{"name":"name","args":"marker_PRIVATE","result":"marker_PRIVATE","status":"running"}]
            }]}]));
            let encoded = plan_import(&data, Snapshot::empty(&[], 0.0)).unwrap();
            assert!(!encoded.contains("marker_PRIVATE"));
            let plan: Value = serde_json::from_str(&encoded).unwrap();
            let messages = plan["histories"]["c-x"].as_array().unwrap();
            let admitted = matches!(role, "user" | "assistant") && content.is_string();
            assert_eq!(messages.len(), usize::from(admitted));
            if let Some(message) = messages.first() {
                assert_eq!(message.as_object().unwrap().len(), 3);
                assert_eq!(
                    message["toolCalls"],
                    json!([{"name":"name","status":"done"}])
                );
            }
        }
    }
}

#[test]
fn every_generated_collision_is_consumed_in_order_and_exhaustion_terminates() {
    for count in [1, 2, 20, 100] {
        let generated = (0..count).map(|i| format!("c-new-{i}")).collect::<Vec<_>>();
        let refs = generated.iter().map(String::as_str).collect::<Vec<_>>();
        let data = input(json!(vec![json!({"id":"invalid"}); count]));
        let mut context = Snapshot::empty(&refs, 0.0);
        let first = plan_import(&data, context).unwrap();
        let plan: Value = serde_json::from_str(&first).unwrap();
        assert_eq!(
            plan["freeChats"]
                .as_array()
                .unwrap()
                .iter()
                .map(|m| m["id"].as_str().unwrap())
                .collect::<Vec<_>>(),
            refs
        );
        context.existing_chat_ids = &refs;
        assert_eq!(
            plan_import(&data, context),
            Err(Error::Unsupported(Unsupported::IdSupplyExhausted))
        );
    }
}

#[test]
fn matching_cannot_select_foreign_project_ids_and_group_order_tracks_first_seen() {
    for names in [["a", "b", "a"], ["b", "a", "b"], ["A", " B ", "a"]] {
        let data = input(json!(names
            .iter()
            .enumerate()
            .map(|(i, n)| json!({"id":format!("c-{i}"),"project":{"id":"foreign","name":n}}))
            .collect::<Vec<_>>()));
        let local_name = JValue::from("a");
        let projects = [Project {
            id: "tenant-local",
            name: &local_name,
        }];
        let mut context = Snapshot::empty(&[], 0.0);
        context.projects = &projects;
        let encoded = plan_import(&data, context).unwrap();
        assert!(!encoded.contains("foreign"));
        let plan: Value = serde_json::from_str(&encoded).unwrap();
        let groups = plan["projectChats"].as_array().unwrap();
        assert_eq!(groups.len(), 2);
        assert_eq!(
            groups[0]["name"].as_str().unwrap().to_ascii_lowercase(),
            names[0].to_ascii_lowercase()
        );
        assert!(groups
            .iter()
            .all(|g| g["projectId"].is_null() || g["projectId"] == "tenant-local"));
    }
}

#[test]
fn unsupported_admission_does_not_become_a_public_node400() {
    let data = input(json!([]));
    for (messages, chars) in [(0, 200000), (5001, 200000), (5000, 0), (5000, 200001)] {
        let mut context = Snapshot::empty(&[], 0.0);
        context.max_messages = messages;
        context.max_chars = chars;
        let error = plan_import(&data, context).unwrap_err();
        assert_eq!(error, Error::Unsupported(Unsupported::Configuration));
        assert_eq!(error.node_public_error(), None);
    }
    for (member, value) in [
        ("unused", JValue::Num(f64::NAN)),
        ("unused", JValue::Undefined),
    ] {
        let data = JValue::obj([
            ("format", JValue::from(FORMAT)),
            ("chats", JValue::Arr(vec![])),
            (member, value),
        ]);
        assert_eq!(
            plan_import(&data, Snapshot::empty(&[], 0.0)),
            Err(Error::Unsupported(Unsupported::Input))
        );
    }
    let data = JValue::Obj(vec![
        ("format".into(), JValue::from(FORMAT)),
        ("chats".into(), JValue::Arr(vec![])),
        ("chats".into(), JValue::Arr(vec![])),
    ]);
    assert_eq!(
        plan_import(&data, Snapshot::empty(&[], 0.0)),
        Err(Error::Unsupported(Unsupported::Input))
    );
}

#[test]
fn depth_nodes_bytes_and_snapshot_limits_refuse_before_planning() {
    let mut nested = JValue::Null;
    for _ in 0..65 {
        nested = JValue::Arr(vec![nested]);
    }
    let data = JValue::obj([
        ("format", JValue::from(FORMAT)),
        ("chats", JValue::Arr(vec![])),
        ("unused", nested),
    ]);
    assert_eq!(
        plan_import(&data, Snapshot::empty(&[], 0.0)),
        Err(Error::Limit)
    );
    let too_many_nodes = JValue::Arr(vec![JValue::Null; MAX_NODES]);
    let data = JValue::obj([
        ("format", JValue::from(FORMAT)),
        ("chats", JValue::Arr(vec![])),
        ("unused", too_many_nodes),
    ]);
    assert_eq!(
        plan_import(&data, Snapshot::empty(&[], 0.0)),
        Err(Error::Limit)
    );
    let data = JValue::obj([
        ("format", JValue::from(FORMAT)),
        ("chats", JValue::Arr(vec![])),
        ("unused", JValue::Str("x".repeat(MAX_BYTES))),
    ]);
    assert_eq!(
        plan_import(&data, Snapshot::empty(&[], 0.0)),
        Err(Error::Limit)
    );
    let small = input(json!([]));
    let ids = vec!["c-x"; 16001];
    for list in 0..3 {
        let mut context = Snapshot::empty(&[], 0.0);
        match list {
            0 => context.existing_chat_ids = &ids,
            1 => context.tombstones = &ids,
            _ => context.generated_ids = &ids,
        }
        assert_eq!(plan_import(&small, context), Err(Error::Limit));
    }
    let name = JValue::from("x");
    let projects = vec![
        Project {
            id: "p-x",
            name: &name
        };
        2001
    ];
    let mut context = Snapshot::empty(&[], 0.0);
    context.projects = &projects;
    assert_eq!(plan_import(&small, context), Err(Error::Limit));
}

#[test]
fn compact_snapshot_name_depth_includes_three_scaffold_levels() {
    let data = input(json!([]));
    for (nested_arrays, accepted) in [(61, true), (62, false)] {
        let mut name = JValue::Null;
        for _ in 0..nested_arrays {
            name = JValue::Arr(vec![name]);
        }
        let projects = [Project {
            id: "p-depth",
            name: &name,
        }];
        let mut context = Snapshot::empty(&[], 0.0);
        context.projects = &projects;
        let result = plan_import(&data, context);
        if accepted {
            assert!(
                result.is_ok(),
                "61 arrays plus snapshot scaffolding reaches64"
            );
        } else {
            assert_eq!(result, Err(Error::Limit));
        }
    }
}
