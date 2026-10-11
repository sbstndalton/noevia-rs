//! Static contract examples derived by reading core0a204 chat-import.cjs and its tests.
//! These are NOT executed Node goldens or current differential evidence.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
use js_json::JValue;
use serde_json::{json, Value};
use server_import::{plan_import, Error, Project, Snapshot, Unsupported, FORMAT};

fn input(value: Value) -> JValue {
    js_json::parse(&value.to_string()).unwrap()
}
fn result(value: Value, ids: &[&str]) -> Value {
    let value = input(value);
    serde_json::from_str(&plan_import(&value, Snapshot::empty(ids, 123.0)).unwrap()).unwrap()
}
fn envelope(chats: Value) -> Value {
    json!({"format": FORMAT, "chats": chats})
}

#[test]
fn actual_two_readable_errors_and_order() {
    for bad in [
        Value::Null,
        json!([]),
        json!({}),
        json!({"format":"other","chats":[]}),
        json!({"format":FORMAT,"chats":"x"}),
    ] {
        assert_eq!(
            plan_import(&input(bad), Snapshot::empty(&[], 0.0)),
            Err(Error::Envelope)
        );
    }
    let too_many = envelope(json!(vec![Value::Null; 2001]));
    let mut invalid_context = Snapshot::empty(&[], f64::NAN);
    invalid_context.max_messages = 0;
    assert_eq!(
        plan_import(&input(too_many), invalid_context),
        Err(Error::TooManyChats)
    );
    assert_eq!(
        Error::Envelope.node_public_error(),
        Some((
            400,
            "Choose a conversations.json from a noevia conversations export."
        ))
    );
    assert_eq!(
        Error::TooManyChats.node_public_error(),
        Some((
            400,
            "An import holds at most 2000 chats. Split the file and import each part."
        ))
    );
    for error in [
        Error::Limit,
        Error::Allocation,
        Error::Unsupported(Unsupported::IdSupplyExhausted),
    ] {
        assert_eq!(error.node_public_error(), None);
    }
    assert_eq!(
        result(envelope(json!(vec![Value::Null; 2000])), &[])["imported"],
        0
    );
}

#[test]
fn actual_free_existing_and_new_project_contract() {
    let data = input(envelope(json!([
        {"id":"c-1","title":"Packing","updatedAt":5,"project":null,"history":[{"role":"user","content":"hi"}]},
        {"id":"c-2","title":"Cells","updatedAt":6,"project":{"id":"p-old","name":"battery NOTES"},"history":[]},
        {"id":"c-3","title":"Other","updatedAt":7,"project":{"id":"p-x","name":"New place"},"history":[]}
    ])));
    let name = JValue::from("Battery notes");
    let projects = [Project {
        id: "proj-1",
        name: &name,
    }];
    let mut context = Snapshot::empty(&[], 0.0);
    context.projects = &projects;
    let actual: Value = serde_json::from_str(&plan_import(&data, context).unwrap()).unwrap();
    assert_eq!(
        actual,
        json!({
            "freeChats":[{"id":"c-1","title":"Packing","updatedAt":5}],
            "projectChats":[
                {"projectId":"proj-1","name":"Battery notes","chats":[{"id":"c-2","title":"Cells","updatedAt":6}]},
                {"projectId":null,"name":"New place","chats":[{"id":"c-3","title":"Other","updatedAt":7}]}
            ],
            "histories":{"c-1":[{"role":"user","content":"hi"}],"c-2":[],"c-3":[]},
            "skipped":[],"imported":3
        })
    );
}

#[test]
fn existing_skip_duplicate_collision_and_actual_generated_tombstone_policy() {
    let data = input(envelope(json!([
        {"id":"c-here","title":false,"history":[]},
        {"id":"c-gone","title":"Deleted","history":[]},
        {"id":"c-raw","history":[]},
        {"id":"c-raw","history":[]}
    ])));
    let generated = ["c-here", "c-gone", "c-raw", "c-gone", "c-final"];
    let mut context = Snapshot::empty(&generated, 7.0);
    context.existing_chat_ids = &["c-here"];
    context.tombstones = &["c-gone"];
    let actual: Value = serde_json::from_str(&plan_import(&data, context).unwrap()).unwrap();
    assert_eq!(
        actual["skipped"],
        json!([{"title":"Untitled chat","reason":"already in this account"}])
    );
    assert_eq!(
        actual["freeChats"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v["id"].as_str().unwrap())
            .collect::<Vec<_>>(),
        ["c-gone", "c-raw", "c-final"]
    );
    // Node's generated-ID loop intentionally ignores tombstones, so a generated c-gone is kept.
    assert_eq!(actual["histories"]["c-gone"], json!([]));
}

#[test]
fn safe_id_lengths_and_lazy_generated_id_refusals() {
    let longest = format!("c-{}", "x".repeat(80));
    let too_long = format!("c-{}", "x".repeat(81));
    let actual = result(
        envelope(json!([
            {"id":longest}, {"id":too_long}, {"id":"c-"}, {"id":"../../etc"}, {"id":"c-a_b-9"}
        ])),
        &["c-new1", "c-new2", "c-new3"],
    );
    assert_eq!(actual["freeChats"][0]["id"], longest);
    assert_eq!(actual["freeChats"][1]["id"], "c-new1");
    assert_eq!(actual["freeChats"][2]["id"], "c-new2");
    assert_eq!(actual["freeChats"][3]["id"], "c-new3");
    assert_eq!(actual["freeChats"][4]["id"], "c-a_b-9");
    let data = input(envelope(json!([{}])));
    assert_eq!(
        plan_import(&data, Snapshot::empty(&[], 0.0)),
        Err(Error::Unsupported(Unsupported::IdSupplyExhausted))
    );
    assert_eq!(
        plan_import(&data, Snapshot::empty(&["bad"], 0.0)),
        Err(Error::Unsupported(Unsupported::GeneratedId))
    );
    let valid = input(envelope(json!([{"id":"c-safe"}])));
    assert!(plan_import(&valid, Snapshot::empty(&["unused unsafe id"], 0.0)).is_ok());
}

#[test]
fn raw_arrays_are_objects_and_falsy_scalar_chats_are_skipped() {
    let actual = result(
        envelope(json!([null, false, 0, "text", [], ["ignored"], {}])),
        &["c-a", "c-b", "c-c"],
    );
    assert_eq!(actual["imported"], 3);
    assert_eq!(
        actual["freeChats"],
        json!([
            {"id":"c-a","title":"","updatedAt":123},
            {"id":"c-b","title":"","updatedAt":123},
            {"id":"c-c","title":"","updatedAt":123}
        ])
    );
}

#[test]
fn title_truthiness_and_string_hint_coercion() {
    let titles = json!([null, false, 0, "", true, 1e21, [1,[2,null],"x"], {"valueOf":1}, {}]);
    let chats = titles
        .as_array()
        .unwrap()
        .iter()
        .enumerate()
        .map(|(i, t)| json!({"id":format!("c-{i}"),"title":t}))
        .collect::<Vec<_>>();
    let actual = result(envelope(json!(chats)), &[]);
    let actual = actual["freeChats"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v["title"].as_str().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(
        actual,
        [
            "",
            "",
            "",
            "",
            "true",
            "1e+21",
            "1,2,,x",
            "[object Object]",
            "[object Object]"
        ]
    );
    let data = input(envelope(json!([{"id":"c-x","title":{"toString":1}}])));
    assert_eq!(
        plan_import(&data, Snapshot::empty(&[], 0.0)),
        Err(Error::Unsupported(Unsupported::Coercion))
    );
    // Coercion of the tail must still fail after the200-unit prefix was filled.
    let data = input(envelope(
        json!([{"id":"c-x","title":["x".repeat(201),{"toString":1}]}]),
    ));
    assert_eq!(
        plan_import(&data, Snapshot::empty(&[], 0.0)),
        Err(Error::Unsupported(Unsupported::Coercion))
    );
}

#[test]
fn finite_number_timestamps_only_and_explicit_clock() {
    let actual = result(
        envelope(json!([
            {"id":"c-0","updatedAt":0}, {"id":"c-negative","updatedAt":-2.5},
            {"id":"c-string","updatedAt":"10"}, {"id":"c-bool","updatedAt":true},
            {"id":"c-null","updatedAt":null}, {"id":"c-missing"}
        ])),
        &[],
    );
    assert_eq!(
        actual["freeChats"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v["updatedAt"].as_f64().unwrap())
            .collect::<Vec<_>>(),
        [0.0, -2.5, 123.0, 123.0, 123.0, 123.0]
    );
    let data = input(envelope(json!([])));
    assert_eq!(
        plan_import(&data, Snapshot::empty(&[], f64::INFINITY)),
        Err(Error::Unsupported(Unsupported::Configuration))
    );
}

#[test]
fn filter_before_last_window_and_only_safe_fields() {
    let data = input(envelope(json!([{"id":"c-history","history":[
        {"role":"user","content":"first"}, {"role":"user","content":"second"},
        {"role":"assistant","content":"last","reasoning":"PRIVATE","extra":1,"toolCalls":[null,{}, {"name":"ok","args":"PRIVATE","result":"PRIVATE"},{"name":true}]},
        {"role":"system","content":"injection"}, {"role":"tool","content":"raw"},
        {"role":"user","content":123},null,"junk"
    ]}])));
    let mut context = Snapshot::empty(&[], 0.0);
    context.max_messages = 2;
    context.max_chars = 3;
    let encoded = plan_import(&data, context).unwrap();
    assert!(!encoded.contains("PRIVATE"));
    let actual: Value = serde_json::from_str(&encoded).unwrap();
    assert_eq!(
        actual["histories"]["c-history"],
        json!([
            {"role":"user","content":"sec"},
            {"role":"assistant","content":"las","toolCalls":[{"name":"ok","status":"done"}]}
        ])
    );
    assert_eq!(
        result(envelope(json!([{"id":"c-x","history":"not array"}])), &[])["histories"]["c-x"],
        json!([])
    );
}

#[test]
fn actual_default_message_and_character_caps_and_tool50() {
    let messages = (0..5001)
        .map(|i| json!({"role":"user","content":i.to_string()}))
        .collect::<Vec<_>>();
    let tools = (0..51)
        .map(|i| json!({"name":format!("{i}{}", "x".repeat(150))}))
        .collect::<Vec<_>>();
    let actual = result(
        envelope(json!([
            {"id":"c-window","history":messages},
            {"id":"c-chars","title":"t".repeat(201),"history":[{"role":"assistant","content":"x".repeat(200001),"toolCalls":tools}]}
        ])),
        &[],
    );
    let history = actual["histories"]["c-window"].as_array().unwrap();
    assert_eq!(history.len(), 5000);
    assert_eq!(history.first().unwrap()["content"], "1");
    assert_eq!(history.last().unwrap()["content"], "5000");
    assert_eq!(actual["freeChats"][1]["title"].as_str().unwrap().len(), 200);
    let last = &actual["histories"]["c-chars"][0];
    assert_eq!(last["content"].as_str().unwrap().len(), 200000);
    assert_eq!(last["toolCalls"].as_array().unwrap().len(), 50);
    assert_eq!(last["toolCalls"][0]["name"].as_str().unwrap().len(), 120);
    assert_eq!(last["toolCalls"][49]["name"].as_str().unwrap().len(), 120);
}

#[test]
fn trim_first_then_truncate_and_group_first_seen_snapshot_last_wins() {
    let a = JValue::from(" ALPHA ");
    let b = JValue::from("alpha");
    let projects = [
        Project {
            id: "p-first",
            name: &a,
        },
        Project {
            id: "p-last",
            name: &b,
        },
    ];
    let data = input(envelope(json!([
        {"id":"c-z","project":{"name":" Zed "}},
        {"id":"c-a","project":{"id":"foreign","name":"\u{feff}ALPHA\u{a0}"}},
        {"id":"c-z2","project":{"name":"zed"}},
        {"id":"c-long","project":{"name":format!(" {} ","x".repeat(121))}},
        {"id":"c-false","project":false}
    ])));
    let mut context = Snapshot::empty(&[], 1.0);
    context.projects = &projects;
    let actual: Value = serde_json::from_str(&plan_import(&data, context).unwrap()).unwrap();
    assert_eq!(actual["projectChats"][0]["name"], "Zed");
    assert_eq!(
        actual["projectChats"][0]["chats"].as_array().unwrap().len(),
        2
    );
    assert_eq!(actual["projectChats"][1]["projectId"], "p-last");
    assert_eq!(actual["projectChats"][1]["name"], "alpha");
    assert_eq!(
        actual["projectChats"][2]["name"].as_str().unwrap().len(),
        120
    );
    assert_eq!(actual["freeChats"][0]["id"], "c-false");
}

#[test]
fn snapshot_name_coercion_and_original_value_emission() {
    let number = JValue::Num(42.0);
    let object = input(json!({"valueOf":true}));
    let projects = [
        Project {
            id: "p-number",
            name: &number,
        },
        Project {
            id: "p-object",
            name: &object,
        },
    ];
    let data = input(envelope(json!([
        {"id":"c-num","project":{"name":"42"}},
        {"id":"c-obj","project":{"name":"[object Object]"}}
    ])));
    let mut context = Snapshot::empty(&[], 0.0);
    context.projects = &projects;
    let actual: Value = serde_json::from_str(&plan_import(&data, context).unwrap()).unwrap();
    assert_eq!(actual["projectChats"][0]["name"], 42);
    assert_eq!(actual["projectChats"][1]["name"], json!({"valueOf":true}));
}

#[test]
fn utf16_exact_prefix_and_split_surrogate_refusal() {
    let data = input(envelope(
        json!([{"id":"c-x","title":format!("{}😀tail","a".repeat(198)),"history":[{"role":"user","content":"a😀z"}]}]),
    ));
    let mut context = Snapshot::empty(&[], 0.0);
    context.max_chars = 3;
    let actual: Value = serde_json::from_str(&plan_import(&data, context).unwrap()).unwrap();
    assert_eq!(
        actual["freeChats"][0]["title"],
        format!("{}😀", "a".repeat(198))
    );
    assert_eq!(actual["histories"]["c-x"][0]["content"], "a😀");
    for chat in [
        json!({"id":"c-x","title":format!("{}😀","a".repeat(199))}),
        json!({"id":"c-x","history":[{"role":"user","content":"ab😀"}]}),
        json!({"id":"c-x","history":[{"role":"user","content":"ok","toolCalls":[{"name":format!("{}😀","a".repeat(119))}]}]}),
    ] {
        assert_eq!(
            plan_import(&input(envelope(json!([chat]))), context),
            Err(Error::Unsupported(Unsupported::SurrogateCut))
        );
    }
    assert_eq!(
        plan_import(
            &input(envelope(json!([{"id":"c-x","project":{"name":"Ω"}}]))),
            Snapshot::empty(&[], 0.0)
        ),
        Err(Error::Unsupported(Unsupported::GroupingName))
    );
}

#[test]
fn exact_serialized_field_and_history_order_and_json_escaping() {
    let data = input(envelope(
        json!([{"id":"c-z","title":"\n\"\\\u{1}"},{"id":"c-a","history":[]}]),
    ));
    let out = plan_import(&data, Snapshot::empty(&[], 0.0)).unwrap();
    assert_eq!(out,"{\"freeChats\":[{\"id\":\"c-z\",\"title\":\"\\n\\\"\\\\\\u0001\",\"updatedAt\":0},{\"id\":\"c-a\",\"title\":\"\",\"updatedAt\":0}],\"projectChats\":[],\"histories\":{\"c-z\":[],\"c-a\":[]},\"skipped\":[],\"imported\":2}");
}

#[test]
fn snapshot_object_json_uses_actual_integer_key_order_recursively() {
    // Preserve literal source insertion order in JValue rather than serde_json's map order.
    let name=js_json::parse(r#"{"10":"ten","2":"two","01":"leading","4294967295":"ordinary","4294967294":{"10":0,"2":1},"0":[{"2":2,"1":1}],"last":true}"#).unwrap();
    let projects = [Project {
        id: "p-index",
        name: &name,
    }];
    let data = input(envelope(
        json!([{"id":"c-x","project":{"name":"[object Object]"}}]),
    ));
    let mut context = Snapshot::empty(&[], 0.0);
    context.projects = &projects;
    let encoded = plan_import(&data, context).unwrap();
    assert!(encoded.contains(r#""name":{"0":[{"1":1,"2":2}],"2":"two","10":"ten","4294967294":{"2":1,"10":0},"01":"leading","4294967295":"ordinary","last":true}"#));
}
