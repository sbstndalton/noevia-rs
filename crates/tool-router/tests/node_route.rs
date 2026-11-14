//! core's `server/tool-router.test.cjs`, case by case, with the same rows and numbers (the tool names
//! made generic), plus the JS edges the module doc names: object key order of `reasons`, an undefined
//! cap, a negative `topK`, duplicate ids, a `requires` cycle, the whole answer as `JSON.stringify`
//! writes it.
#![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]

use tool_router::{route, Input, Routed, ToolBox, DEFAULT_THRESHOLD, DEFAULT_TOP_K};

fn b(id: &str, tools: &[&str], embedding: &[f64]) -> ToolBox {
    ToolBox {
        id: id.into(),
        tools: tools.iter().map(|t| (*t).to_owned()).collect(),
        embedding: Some(embedding.to_vec()),
        ..ToolBox::default()
    }
}

/// The test file's `boxes`.
fn boxes() -> Vec<ToolBox> {
    vec![
        b(
            "web-search",
            &["web_search", "web_extract"],
            &[1.0, 0.0, 0.0],
        ),
        ToolBox {
            never_auto_load: true,
            ..b("web-crawl", &["web_crawl"], &[0.9, 0.1, 0.0])
        },
        ToolBox {
            requires: vec!["contacts".into()],
            ..b("calendar", &["cal_list", "cal_create"], &[0.0, 1.0, 0.0])
        },
        b("contacts", &["contacts_find"], &[0.0, 0.2, 1.0]),
        b(
            "files",
            &["files_read", "files_write", "files_list"],
            &[0.0, 0.95, 0.1],
        ),
        b("files-mirror", &["files_read"], &[0.05, 0.9, 0.1]),
        b("admin-only", &["admin_x"], &[1.0, 0.0, 0.0]),
    ]
}

fn offered(id: &str) -> bool {
    id != "admin-only"
}

fn ids(v: &[&str]) -> Vec<String> {
    v.iter().map(|s| (*s).to_owned()).collect()
}

/// `{ boxes, offered, maxTools: 20, threshold: 0.3 }` plus the case's fields.
struct Case {
    boxes: Vec<ToolBox>,
    task: Option<Vec<f64>>,
    user: Vec<String>,
    fallback: Vec<String>,
    max_tools: f64,
    top_k: f64,
    threshold: f64,
}

impl Case {
    fn base(task: Option<&[f64]>) -> Case {
        Case {
            boxes: boxes(),
            task: task.map(<[f64]>::to_vec),
            user: vec![],
            fallback: vec![],
            max_tools: 20.0,
            top_k: DEFAULT_TOP_K,
            threshold: 0.3,
        }
    }
    fn run(&self) -> Routed {
        route(&Input {
            boxes: &self.boxes,
            task_embedding: self.task.as_deref(),
            offered: &offered,
            user_selection: &self.user,
            fallback: &self.fallback,
            max_tools: self.max_tools,
            top_k: self.top_k,
            threshold: self.threshold,
        })
    }
}

fn sorted(mut v: Vec<String>) -> Vec<String> {
    v.sort();
    v
}

#[test]
fn routes_to_the_best_matches_above_threshold_within_the_deployment_ceiling() {
    let r = Case {
        top_k: 3.0,
        ..Case::base(Some(&[1.0, 0.0, 0.0]))
    }
    .run();
    assert!(r.routed);
    assert!(r.boxes.contains(&"web-search".to_owned()));
    assert!(
        !r.boxes.contains(&"admin-only".to_owned()),
        "not offered by this deployment"
    );
    assert!(!r.boxes.contains(&"web-crawl".to_owned()), "autoLoad never");
    assert!(r
        .reason("web-search")
        .unwrap()
        .contains("matched this task"));
    // Exactly: web-search (1.00) only; files-mirror's cosine is 0.05/~0.906 = 0.055 < 0.3.
    assert_eq!(r.boxes, ids(&["web-search"]));
    assert_eq!(r.reason("web-search"), Some("matched this task (1.00)"));
}

#[test]
fn requires_is_resolved_transitively_and_missing_dependencies_load_nothing() {
    let r = Case {
        top_k: 1.0,
        ..Case::base(Some(&[0.0, 1.0, 0.0]))
    }
    .run();
    assert_eq!(sorted(r.boxes.clone()), ids(&["calendar", "contacts"]));
    assert_eq!(r.reason("contacts"), Some("required by calendar"));
    assert_eq!(r.reason("calendar"), Some("matched this task (1.00)"));

    let broken = Case {
        boxes: boxes()
            .into_iter()
            .map(|b| {
                if b.id == "contacts" {
                    ToolBox {
                        never_auto_load: true,
                        ..b
                    }
                } else {
                    b
                }
            })
            .collect(),
        top_k: 1.0,
        ..Case::base(Some(&[0.0, 1.0, 0.0]))
    }
    .run();
    assert!(broken.boxes.is_empty());
    assert_eq!(
        broken.skipped[0].reason,
        "a required toolbox is unavailable"
    );
    assert_eq!(broken.skipped[0].id, "calendar");
}

#[test]
fn a_closure_that_would_break_the_tool_cap_loads_nothing_extra_never_part_of_a_box() {
    let r = Case {
        top_k: 1.0,
        max_tools: 2.0,
        ..Case::base(Some(&[0.0, 1.0, 0.0]))
    }
    .run();
    assert!(r.boxes.is_empty());
    assert_eq!(r.skipped[0].reason, "would exceed the tool cap");
}

#[test]
fn two_boxes_exposing_the_same_tool_name_are_never_loaded_together_the_higher_score_wins() {
    let r = Case {
        boxes: boxes()
            .into_iter()
            .filter(|b| !["calendar", "contacts"].contains(&b.id.as_str()))
            .collect(),
        top_k: 3.0,
        ..Case::base(Some(&[0.0, 1.0, 0.1]))
    }
    .run();
    assert!(r.boxes.contains(&"files".to_owned()));
    assert!(!r.boxes.contains(&"files-mirror".to_owned()));
    let mirror = r.skipped.iter().find(|s| s.id == "files-mirror").unwrap();
    assert_eq!(mirror.reason, "duplicates a loaded tool name");
}

#[test]
fn the_users_own_selection_is_always_kept_and_wins_over_routing() {
    let r = Case {
        user: ids(&["files", "admin-only"]),
        ..Case::base(Some(&[1.0, 0.0, 0.0]))
    }
    .run();
    assert!(r.boxes.contains(&"files".to_owned()));
    assert_eq!(r.reason("files"), Some("selected by you"));
    assert!(
        !r.boxes.contains(&"admin-only".to_owned()),
        "a user selection still cannot exceed the deployment ceiling"
    );
}

#[test]
fn without_embeddings_the_router_falls_back_to_the_project_selection() {
    for task in [None, Some(vec![])] {
        let r = Case {
            task,
            fallback: ids(&["web-search", "admin-only"]),
            ..Case::base(None)
        }
        .run();
        assert!(!r.routed);
        assert_eq!(r.boxes, ids(&["web-search"]));
        assert!(r
            .reason("web-search")
            .unwrap()
            .contains("router unavailable"));
    }
}

#[test]
fn nothing_above_threshold_loads_nothing() {
    assert!(Case::base(Some(&[-1.0, -1.0, -1.0])).run().boxes.is_empty());
}

// ── the JS edges ──

#[test]
fn the_whole_answer_as_json_stringify_writes_it() {
    let r = Case {
        user: ids(&["web-search", "web-search"]),
        top_k: 1.0,
        ..Case::base(Some(&[0.0, 1.0, 0.0]))
    }
    .run();
    assert_eq!(
        js_json::stringify(&r.to_json()).unwrap(),
        r#"{"boxes":["web-search","calendar","contacts"],"reasons":{"web-search":"selected by you","calendar":"matched this task (1.00)","contacts":"required by calendar"},"skipped":[],"routed":true}"#
    );
}

#[test]
fn reasons_read_back_with_array_index_keys_first() {
    let rows = vec![
        b("web", &["w"], &[1.0, 0.0]),
        b("10", &["t10"], &[1.0, 0.0]),
        b("2", &["t2"], &[1.0, 0.0]),
    ];
    let user = ids(&["web", "10", "2"]);
    let r = route(&Input {
        boxes: &rows,
        task_embedding: None,
        offered: &|_| true,
        user_selection: &user,
        fallback: &[],
        max_tools: 20.0,
        top_k: DEFAULT_TOP_K,
        threshold: DEFAULT_THRESHOLD,
    });
    // `[...chosen]` keeps insertion order; the object puts "2" and "10" first.
    assert_eq!(r.boxes, ids(&["web", "10", "2"]));
    let keys: Vec<&str> = r.reasons().into_iter().map(|(k, _)| k).collect();
    assert_eq!(keys, ["2", "10", "web"]);
}

#[test]
fn an_undefined_cap_never_refuses_and_a_negative_top_k_counts_from_the_end() {
    let r = Case {
        max_tools: f64::NAN,
        top_k: 3.0,
        ..Case::base(Some(&[0.0, 1.0, 0.0]))
    }
    .run();
    // calendar (1.00) + contacts; files (0.99) next; files-mirror duplicates files_read.
    assert_eq!(r.boxes, ids(&["calendar", "contacts", "files"]));
    assert_eq!(r.reason("files"), Some("matched this task (0.99)"));
    assert_eq!(r.skipped.len(), 1);
    assert_eq!(r.skipped[0].id, "files-mirror");

    // Ranked above 0.3: calendar 1.00, files 0.99, files-mirror ~0.99, contacts 0.20 (below) ->
    // three rows; slice(0, -1) keeps the first two.
    let r = Case {
        top_k: -1.0,
        ..Case::base(Some(&[0.0, 1.0, 0.0]))
    }
    .run();
    assert_eq!(r.boxes, ids(&["calendar", "contacts", "files"]));
    assert!(r.skipped.is_empty());
    // NaN threshold keeps nothing.
    let r = Case {
        threshold: f64::NAN,
        ..Case::base(Some(&[0.0, 1.0, 0.0]))
    }
    .run();
    assert!(r.boxes.is_empty() && r.routed);
}

#[test]
fn duplicate_ids_and_requires_cycles() {
    // Of two rows with one id the later is looked up (Map), but ranking walks both rows.
    let rows = vec![
        b("dup", &["first_tool"], &[1.0, 0.0]),
        b("dup", &["second_tool"], &[0.9, 0.1]),
        ToolBox {
            requires: vec!["b".into()],
            ..b("a", &["ta"], &[0.8, 0.2])
        },
        ToolBox {
            requires: vec!["a".into()],
            ..b("b", &["tb"], &[0.0, 1.0])
        },
    ];
    let r = route(&Input {
        boxes: &rows,
        task_embedding: Some(&[1.0, 0.0]),
        offered: &|_| true,
        user_selection: &[],
        fallback: &[],
        max_tools: 20.0,
        top_k: 5.0,
        threshold: 0.5,
    });
    // dup (1.00) loads the later row's tools; dup again (0.99: ranked before the loop, so still a
    // candidate) passes the checks with an empty `adding` and changes no reason; a (0.97) brings b
    // through the cycle.
    assert_eq!(r.boxes, ids(&["dup", "a", "b"]));
    assert_eq!(r.reason("dup"), Some("matched this task (1.00)"));
    assert_eq!(r.reason("a"), Some("matched this task (0.97)"));
    assert_eq!(r.reason("b"), Some("required by a"));
    assert!(r.skipped.is_empty());
}
