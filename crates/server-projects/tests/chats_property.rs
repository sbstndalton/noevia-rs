//! M4: properties of the chat-list port over generated workspaces and request bodies.
//!
//! - Always: a saved list is newest first, capped, never holds a tombstoned id, adds every new id
//!   no other list of the account holds, reports exactly those it skipped, and saving the same
//!   body twice changes nothing the second time; reading it back returns what was stored.
//! - With NOEVIA_CORE_CHECKOUT (CI, the job that boots core): the same generator's cases also run
//!   through noevia-core's own route (`tools/gen-project-chats-fixtures.cjs --cases`), and every
//!   answer and every projects.json must be Node's, byte for byte.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use proptest::prelude::*;
use proptest::strategy::ValueTree;
use proptest::test_runner::TestRunner;
use serde_json::{json, Map, Value};
use server_projects::{chats, Body, Ctx};
use server_store::RustProjects;
use std::collections::HashSet;

const USER: &str = "11111111-2222-4333-8444-555555555555";
const NOW: i64 = 1_700_000_000_123;
const POOL: &[&str] = &[
    "a", "b", "c", "d", "e", "f-1", "dead", "x y", "", "%%", "A_b",
];
const KINDS: &[&str] = &["search", "action", "idea", "question", "code", "bogus"];

fn text() -> impl Strategy<Value = String> {
    prop_oneof![
        "[a-zA-Z #\u{3000}\u{a0}]{0,8}",
        (115usize..122).prop_map(|n| format!("{}😀x", "t".repeat(n))),
        (195usize..202).prop_map(|n| format!("{}😀", "p".repeat(n))),
        Just("é😀#  #".to_string()),
    ]
}

fn loose() -> impl Strategy<Value = Value> {
    prop_oneof![
        3 => text().prop_map(Value::String),
        1 => Just(Value::Null),
        1 => (-3i32..3).prop_map(|n| json!(n)),
        1 => Just(json!(["x", null, 3])),
        1 => Just(json!({"a": 1})),
        1 => Just(json!(true)),
    ]
}

fn frame() -> impl Strategy<Value = Value> {
    let obj = (
        prop::sample::select(KINDS),
        prop::collection::vec(
            prop_oneof![text().prop_map(Value::String), Just(json!(4))],
            0..4,
        ),
        prop::collection::vec(prop::sample::select(POOL).prop_map(|s| json!(s)), 0..3),
        prop::option::of(prop::sample::select(POOL)),
        prop_oneof![Just(json!(true)), Just(json!(false)), Just(json!("yes"))],
        prop::sample::select(&["user", "suggested", "other"][..]),
    )
        .prop_map(|(kind, tags, links, project, confirmed, source)| {
            let mut m = Map::new();
            m.insert("kind".into(), json!(kind));
            m.insert("tags".into(), Value::Array(tags));
            m.insert("links".into(), Value::Array(links));
            if let Some(p) = project {
                m.insert("projectId".into(), json!(p));
            }
            m.insert("confirmed".into(), confirmed);
            m.insert("source".into(), json!(source));
            Value::Object(m)
        });
    prop_oneof![3 => obj, 1 => Just(Value::Null), 1 => Just(json!([1])), 1 => Just(json!("idea"))]
}

/// A meta as Node stores it (typed fields, a canonical frame when there is one).
fn stored_chat() -> impl Strategy<Value = Value> {
    (
        prop::sample::select(&POOL[..7]),
        "[a-z]{0,6}",
        prop::option::of(prop_oneof![(0i64..20).prop_map(|n| json!(n)), Just(Value::Null)]),
        any::<bool>(),
        prop::option::of(any::<bool>()),
        prop::option::of(0i64..50),
    )
        .prop_map(|(id, title, updated, framed, force, created)| {
            let mut m = Map::new();
            m.insert("id".into(), json!(id));
            m.insert("title".into(), json!(title));
            if let Some(u) = updated {
                m.insert("updatedAt".into(), u);
            }
            m.insert("preview".into(), json!(""));
            if framed {
                m.insert(
                    "frame".into(),
                    json!({"projectId": null, "kind": "idea", "tags": ["t"], "links": [], "confirmed": false, "source": "suggested"}),
                );
            }
            if let Some(f) = force {
                m.insert("forceLocal".into(), json!(f));
            }
            if let Some(c) = created {
                m.insert("createdAt".into(), json!(c));
            }
            Value::Object(m)
        })
}

/// A meta as a client (or anyone) may send it.
fn incoming_chat() -> impl Strategy<Value = Value> {
    (
        prop_oneof![5 => prop::sample::select(POOL).prop_map(|s| json!(s)), 1 => Just(json!(7))],
        prop::option::of(loose()),
        prop::option::of(prop_oneof![
            (0i64..20).prop_map(|n| json!(n)),
            Just(json!("5")),
            Just(json!(-0.0))
        ]),
        prop::option::of(loose()),
        prop::option::of(frame()),
        prop::option::of(prop_oneof![
            Just(json!(true)),
            Just(json!(false)),
            Just(json!("yes"))
        ]),
        prop::option::of(prop_oneof![
            (0i64..50).prop_map(|n| json!(n)),
            Just(json!("3"))
        ]),
        prop::option::of(prop::sample::select(&["cowork", "chat"][..])),
        any::<bool>(),
    )
        .prop_map(
            |(id, title, updated, preview, frame, flag, created, mode, pinned)| {
                let mut m = Map::new();
                m.insert("id".into(), id);
                if let Some(v) = title {
                    m.insert("title".into(), v);
                }
                if let Some(v) = updated {
                    m.insert("updatedAt".into(), v);
                }
                if let Some(v) = preview {
                    m.insert("preview".into(), v);
                }
                if let Some(v) = frame {
                    m.insert("frame".into(), v);
                }
                if let Some(v) = flag {
                    m.insert("allowCloud".into(), v);
                }
                if let Some(v) = created {
                    m.insert("createdAt".into(), v);
                }
                if let Some(v) = mode {
                    m.insert("mode".into(), json!(v));
                }
                m.insert("pinned".into(), json!(pinned));
                Value::Object(m)
            },
        )
}

#[derive(Debug, Clone)]
struct Case {
    projects: String,
    free: String,
    tombstones: String,
    body: String,
    admin: bool,
}

fn case() -> impl Strategy<Value = Case> {
    (
        prop::collection::vec(stored_chat(), 0..8),
        prop::collection::vec(stored_chat(), 0..3),
        prop::collection::vec(prop::sample::select(&POOL[..7]), 0..3),
        prop::collection::vec(prop::sample::select(&POOL[..7]), 0..3),
        prop::collection::vec(incoming_chat(), 0..10),
        any::<bool>(),
    )
        .prop_map(|(a, b, free, dead, incoming, admin)| {
            let projects = json!({"projects": [
                {"id": "proj-a", "name": "A", "modes": ["chat"], "chats": a, "instructionSkills": {}},
                {"id": "proj-b", "name": "B", "modes": ["chat"], "chats": b, "instructionSkills": {}},
            ]});
            let free: Vec<Value> = free.iter().map(|id| json!({"id": id, "title": "F", "updatedAt": 1})).collect();
            Case {
                projects: serde_json::to_string_pretty(&projects).unwrap(),
                free: serde_json::to_string(&free).unwrap(),
                tombstones: serde_json::to_string(&dead).unwrap(),
                body: serde_json::to_string(&json!({ "chats": incoming })).unwrap(),
                admin,
            }
        })
}

struct Workspace {
    root: tempfile::TempDir,
}

impl Workspace {
    fn new(c: &Case) -> Self {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("users").join(USER);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("projects.json"), &c.projects).unwrap();
        std::fs::write(dir.join(chats::FREE_CHATS_FILE), &c.free).unwrap();
        std::fs::write(dir.join(chats::TOMBSTONES_FILE), &c.tombstones).unwrap();
        Workspace { root }
    }
    fn ctx(&self) -> Ctx<'_> {
        Ctx {
            data_dir: self.root.path(),
            user_id: USER,
            switch: RustProjects::owned(),
            now_ms: NOW,
        }
    }
    fn file(&self) -> String {
        std::fs::read_to_string(
            self.root
                .path()
                .join("users")
                .join(USER)
                .join("projects.json"),
        )
        .unwrap()
    }
}

/// serde_json for text that may hold `\udxxx` lone surrogate escapes (the generated texts hold no
/// backslashes of their own): each becomes the literal text `\udxxx`.
fn parse(text: &str) -> Value {
    serde_json::from_str(&text.replace("\\ud", "\\\\ud")).unwrap()
}

fn text_of(r: &server_projects::Reply) -> String {
    match &r.body {
        Body::Json(t) => t.clone(),
        Body::Bytes { .. } => panic!("not json"),
    }
}

fn clean_id(v: &Value) -> Option<String> {
    v.as_str().map(|s| {
        s.chars()
            .filter(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '-')
            .take(80)
            .collect()
    })
}

fn check_invariants(c: &Case) -> Result<(), TestCaseError> {
    let ws = Workspace::new(c);
    let r = chats::post(ws.ctx(), "proj-a", c.body.as_bytes(), false, c.admin);
    prop_assert_eq!(r.status, 200, "{}", text_of(&r));
    let after = ws.file();
    let saved = parse(&after);
    let list = saved["projects"][0]["chats"].as_array().unwrap().clone();
    let keys: Vec<f64> = list
        .iter()
        .map(|x| x["updatedAt"].as_f64().unwrap_or(0.0))
        .collect();
    prop_assert!(
        keys.windows(2).all(|w| w[0] >= w[1]),
        "newest first: {keys:?}"
    );
    prop_assert!(list.len() <= chats::LIST_CAP);
    let dead: HashSet<String> = serde_json::from_str(&c.tombstones).unwrap();
    let ids: Vec<String> = list
        .iter()
        .map(|x| x["id"].as_str().unwrap().to_string())
        .collect();
    prop_assert!(ids.iter().all(|id| !dead.contains(id)), "{ids:?} {dead:?}");
    prop_assert_eq!(
        ids.iter().collect::<HashSet<_>>().len(),
        ids.len(),
        "unique ids"
    );
    let free: Vec<Value> = serde_json::from_str(&c.free).unwrap();
    let mut elsewhere: HashSet<String> = free
        .iter()
        .map(|x| x["id"].as_str().unwrap().to_string())
        .collect();
    let before: Value = serde_json::from_str(&c.projects).unwrap();
    for x in before["projects"][1]["chats"].as_array().unwrap() {
        elsewhere.insert(x["id"].as_str().unwrap().to_string());
    }
    let stored: HashSet<String> = before["projects"][0]["chats"]
        .as_array()
        .unwrap()
        .iter()
        .map(|x| x["id"].as_str().unwrap().to_string())
        .collect();
    let body: Value = serde_json::from_str(&c.body).unwrap();
    let mut skipped = Vec::new();
    for m in body["chats"].as_array().unwrap() {
        let Some(id) = clean_id(&m["id"]) else {
            continue;
        };
        if elsewhere.contains(&id) {
            if !skipped.contains(&id) {
                skipped.push(id.clone());
            }
            prop_assert_eq!(
                ids.contains(&id),
                stored.contains(&id) && !dead.contains(&id),
                "{}",
                id
            );
        } else if !id.is_empty() && !dead.contains(&id) {
            prop_assert!(ids.contains(&id), "{id} added");
        }
    }
    let reply = parse(&text_of(&r));
    let expected = if skipped.is_empty() {
        json!({"ok": true})
    } else {
        json!({"ok": true, "skipped": skipped})
    };
    prop_assert_eq!(reply, expected);
    // The same save again changes nothing.
    let again = chats::post(ws.ctx(), "proj-a", c.body.as_bytes(), false, c.admin);
    prop_assert_eq!(again.status, 200);
    prop_assert_eq!(ws.file(), after);
    // Read back: the stored list.
    let g = chats::get(ws.ctx(), "proj-a");
    let got = parse(&text_of(&g));
    prop_assert_eq!(&got["chats"], &Value::Array(list));
    Ok(())
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(400))]

    #[test]
    fn saved_lists_keep_the_merge_rules(c in case()) {
        check_invariants(&c)?;
    }
}

/// The generated cases through Node as well (needs a core checkout and node; CI sets
/// NOEVIA_CORE_CHECKOUT).
#[test]
fn generated_cases_match_node() {
    let Ok(core) = std::env::var("NOEVIA_CORE_CHECKOUT") else {
        eprintln!("NOEVIA_CORE_CHECKOUT not set: skipped");
        return;
    };
    let mut runner = TestRunner::deterministic();
    let strategy = case();
    let cases: Vec<Case> = (0..300)
        .map(|_| strategy.new_tree(&mut runner).unwrap().current())
        .collect();
    let input: Vec<Value> = cases
        .iter()
        .enumerate()
        .map(|(i, c)| {
            json!({
                "name": format!("generated-{i}"),
                "ws": {"projects": c.projects, "free": c.free, "tombstones": c.tombstones},
                "method": "POST", "project": "proj-a", "body": c.body, "admin": c.admin, "now": NOW,
            })
        })
        .collect();
    let dir = tempfile::tempdir().unwrap();
    let cases_file = dir.path().join("cases.json");
    let out_file = dir.path().join("out.json");
    std::fs::write(&cases_file, serde_json::to_string(&input).unwrap()).unwrap();
    let generator = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../tools/gen-project-chats-fixtures.cjs");
    let status = std::process::Command::new("node")
        .arg(&generator)
        .arg(&core)
        .arg(&out_file)
        .arg("--cases")
        .arg(&cases_file)
        .status()
        .expect("node on PATH");
    assert!(status.success());
    let node: Value = serde_json::from_str(&std::fs::read_to_string(&out_file).unwrap()).unwrap();
    let results = node["cases"].as_array().unwrap();
    assert_eq!(results.len(), cases.len());
    for (c, n) in cases.iter().zip(results) {
        let ws = Workspace::new(c);
        let before = ws.file();
        let r = chats::post(ws.ctx(), "proj-a", c.body.as_bytes(), false, c.admin);
        let after = ws.file();
        let expected = &n["expected"];
        let name = n["name"].as_str().unwrap();
        assert_eq!(
            u64::from(r.status),
            expected["status"].as_u64().unwrap(),
            "{name}"
        );
        assert_eq!(text_of(&r), expected["body"].as_str().unwrap(), "{name}");
        let wrote = if after == before { None } else { Some(after) };
        assert_eq!(
            wrote.as_deref(),
            expected["projectsAfter"].as_str(),
            "{name}: {}",
            c.body
        );
    }
}
