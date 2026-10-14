#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]
//! server_export::assemble against the actual Node buildExport (tools/gen-export-assembly-fixtures.cjs).
use base64::Engine;
use js_json::JValue;
use server_export::assemble::{build_export, too_large_message, Error, Limits};

fn run(input: &JValue) -> Result<Vec<u8>, Error<()>> {
    let histories = input.get("histories").clone();
    let mut limits = Limits::default();
    if let Some(n) = input.get("maxBytes").as_num() {
        limits.max_bytes = n as usize;
    }
    if let Some(n) = input.get("maxEntries").as_num() {
        limits.max_entries = n as usize;
    }
    let now = input.get("now").as_num().map(|n| n as i64).unwrap_or(NOW);
    build_export(
        input.get("freeChats"),
        input.get("projects"),
        |id: &JValue| {
            let key = js_json::to_js_string(id).unwrap();
            Ok(match &histories {
                JValue::Obj(items) => items
                    .iter()
                    .find(|(k, _)| *k == key)
                    .map(|(_, v)| v.clone())
                    .unwrap_or(JValue::Arr(vec![])),
                _ => JValue::Arr(vec![]),
            })
        },
        now,
        limits,
    )
}

const NOW: i64 = 1791981296789;

#[test]
fn actual_node_bytes_and_error_classes() {
    let fixtures: serde_json::Value =
        serde_json::from_str(include_str!("fixtures/node-assembly.v1.json")).unwrap();
    assert_eq!(fixtures["core"], "22dcb9d387a30fca090d970a2662268abac22c64");
    assert_eq!(
        fixtures["sourceSha256"],
        "866139d47d51e05848b55d497596bd7d1de30f76c97c5b268d2b3ad6858a8538"
    );
    assert_eq!(fixtures["now"].as_i64().unwrap(), NOW);
    let mut zips = 0;
    for case in fixtures["cases"].as_array().unwrap() {
        let id = case["id"].as_str().unwrap();
        let input = js_json::parse(case["input"].as_str().unwrap()).unwrap();
        let result = run(&input);
        if case["refusal"] == "Unsupported" {
            assert_eq!(result, Err(Error::Unsupported), "{id}");
            continue;
        }
        if let Some(zip) = case["node"]["zip"].as_str() {
            let want = base64::engine::general_purpose::STANDARD
                .decode(zip)
                .unwrap();
            let got = result.unwrap_or_else(|e| panic!("{id}: {e:?}"));
            assert!(
                got == want,
                "{id}: bytes differ ({} vs {})",
                got.len(),
                want.len()
            );
            zips += 1;
            continue;
        }
        match case["node"]["error"].as_str().unwrap() {
            "TooLarge" => {
                assert_eq!(result, Err(Error::TooLarge), "{id}");
                let mut limits = Limits::default();
                if let Some(n) = input.get("maxBytes").as_num() {
                    limits.max_bytes = n as usize;
                }
                if let Some(n) = input.get("maxEntries").as_num() {
                    limits.max_entries = n as usize;
                }
                assert_eq!(too_large_message(limits), case["node"]["message"], "{id}");
            }
            "TypeError" => assert_eq!(result, Err(Error::Type), "{id}"),
            other => panic!("{id}: unexpected {other}"),
        }
    }
    assert_eq!(zips, 18, "every success case compared");
}

#[test]
fn history_is_read_once_per_chat_in_node_order_and_never_after_the_entry_check() {
    let chats = js_json::parse(r#"[{"id":"a"},{"id":"b"}]"#).unwrap();
    let projects = js_json::parse(r#"[{"id":"p","chats":[{"id":"c"}]}]"#).unwrap();
    let mut seen = Vec::new();
    build_export(
        &chats,
        &projects,
        |id: &JValue| -> Result<JValue, ()> {
            seen.push(id.as_str().unwrap().to_owned());
            Ok(JValue::Arr(vec![]))
        },
        NOW,
        Limits::default(),
    )
    .unwrap();
    assert_eq!(seen, ["a", "b", "c"]);
    let limits = Limits {
        max_entries: 4,
        ..Limits::default()
    };
    let r = build_export(
        &chats,
        &projects,
        |_: &JValue| -> Result<JValue, ()> { panic!("read") },
        NOW,
        limits,
    );
    assert_eq!(r, Err(Error::TooLarge));
    let r = build_export(
        &chats,
        &JValue::Undefined,
        |_: &JValue| Err("disk"),
        NOW,
        Limits::default(),
    );
    assert_eq!(r, Err(Error::History("disk")));
}

#[test]
fn ten_thousand_identical_titles_stay_linear() {
    let chats = JValue::Arr(
        (0..10_000)
            .map(|_| JValue::obj([("id", JValue::from("k")), ("title", JValue::from("Same"))]))
            .collect(),
    );
    let started = std::time::Instant::now();
    let zip = build_export(
        &chats,
        &JValue::Undefined,
        |_: &JValue| -> Result<JValue, ()> { Ok(JValue::Arr(vec![])) },
        NOW,
        Limits::default(),
    )
    .unwrap();
    assert!(started.elapsed() < std::time::Duration::from_secs(20));
    let text = String::from_utf8_lossy(&zip);
    assert!(text.contains("chats/same-k-10000.md"));
    assert!(!text.contains("chats/same-k-10001.md"));
}
