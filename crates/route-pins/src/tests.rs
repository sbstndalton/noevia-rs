#![allow(clippy::indexing_slicing, clippy::expect_used)]

use super::*;

fn rec() -> Recording {
    embedded().expect("the compiled-in recording parses")
}

fn stub(file: &str, refusals: Vec<Refusal>) -> Stub {
    Stub {
        file: file.to_string(),
        function: "refuseOwned".to_string(),
        pathname_arg: false,
        lines: 10,
        env: vec![],
        importers: vec!["server/index.cjs".to_string(), format!("{}.test.cjs", file.trim_end_matches(".cjs"))],
        string_refs: vec![],
        mentions: vec![],
        index_uses: vec!["refuseOwned".to_string()],
        pairing: vec!["deploy/tools/assemble-release.sh:1".to_string()],
        refusals,
        switched: vec![],
        error: None,
    }
}

fn refusal(method: &str, path: &str, pattern: &str, body: &str) -> Refusal {
    Refusal {
        method: method.to_string(),
        path: path.to_string(),
        pattern: Some(pattern.to_string()),
        status: 503,
        headers: [("content-type", "application/json"), ("cache-control", "no-store")]
            .into_iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect(),
        body_text: Some(body.to_string()),
        body: serde_json::from_str(body).unwrap_or(serde_json::Value::Null),
        error: None,
    }
}

fn synthetic(stubs: Vec<Stub>) -> Recording {
    Recording {
        generator: "test".to_string(),
        core: "0123456789abcdef".to_string(),
        noevia: "fedcba9876543210".to_string(),
        methods: METHODS.iter().map(|m| m.to_string()).collect(),
        probes: 1,
        stubs,
    }
}

const BODY: &str = r#"{"error":"Chat search is owned by the Rust front.","code":"RUST_CHAT_SEARCH_OWNED"}"#;

// ---- the recording itself ----

#[test]
fn the_recording_covers_core_s_request_chain() {
    let r = rec();
    assert_eq!(r.generator, "tools/gen-node-refusal-stubs.cjs");
    assert_eq!(r.methods, METHODS.iter().map(|m| m.to_string()).collect::<Vec<_>>());
    // index.cjs held 68 refusal calls at the recorded core; a recording with far fewer read the wrong file.
    assert!(r.stubs.len() >= 60, "only {} stubs recorded", r.stubs.len());
    assert!(r.probes >= 400, "only {} probe paths", r.probes);
    let files: BTreeSet<&str> = r.stubs.iter().map(|s| s.file.as_str()).collect();
    for must in ["server/rust-chat-search.cjs", "server/rust-residual-fallbacks.cjs", "server/rust-auth.cjs"] {
        assert!(files.contains(must), "{must} missing from the recording");
    }
}

#[test]
fn every_recorded_refusal_is_a_fixed_json_503_or_405() {
    for s in rec().stubs {
        assert!(s.error.is_none(), "{}: {:?}", s.file, s.error);
        for r in s.refusals.iter().chain(s.switched.iter().flat_map(|w| w.refusals.iter())) {
            assert!(r.error.is_none(), "{} {} {}: {:?}", s.file, r.method, r.path, r.error);
            assert!([503, 405].contains(&r.status), "{} {} {} answered {}", s.file, r.method, r.path, r.status);
            assert_eq!(r.headers.get("content-type").map(String::as_str), Some("application/json"), "{}", s.file);
            assert_eq!(r.headers.get("cache-control").map(String::as_str), Some("no-store"), "{}", s.file);
            assert!(r.body.get("error").and_then(|e| e.as_str()).is_some_and(|e| !e.is_empty()), "{}: {}", s.file, r.body);
            assert!(METHODS.contains(&r.method.as_str()));
        }
    }
}

#[test]
fn raw_bodies_are_the_parsed_bodies() {
    for s in rec().stubs {
        for r in &s.refusals {
            if let Some(t) = &r.body_text {
                let parsed: serde_json::Value = serde_json::from_str(t).unwrap_or(serde_json::Value::Null);
                assert_eq!(parsed, r.body, "{} {} {}", s.file, r.method, r.path);
            }
        }
    }
}

#[test]
fn every_probe_names_its_routes_toml_pattern_or_a_page() {
    let pages = ["/about", "/about/", "/privacy", "/privacy/", "/device", "/device/", "/.well-known/webauthn", "/"];
    for s in rec().stubs {
        for r in &s.refusals {
            match &r.pattern {
                Some(p) => assert!(p.starts_with('/'), "{p}"),
                None => assert!(pages.contains(&r.path.as_str()) || r.path.starts_with("/api/"), "{}", r.path),
            }
        }
    }
}

// ---- classify ----

#[test]
fn a_dedicated_stub_the_front_fully_answers_can_be_deleted() {
    let r = synthetic(vec![stub("server/rust-chat-search.cjs", vec![refusal("GET", "/api/chats/search", "/api/chats/search", BODY)])]);
    let all = |_: &str, _: &str| true;
    let list = classify(&r, &all);
    assert_eq!(list.len(), 1);
    assert_eq!(list[0].verdict, Verdict::DeleteFile);
    assert_eq!(totals(&list), Totals { delete_file: 1, delete_file_lines: 10, delete_function: 0, keep: 0 });
}

#[test]
fn a_forwarded_refusal_keeps_the_stub_live_and_names_the_leak() {
    let r = synthetic(vec![stub(
        "server/rust-chat-search.cjs",
        vec![
            refusal("GET", "/api/chats/search", "/api/chats/search", BODY),
            refusal("GET", "/api/chats/search/", "/api/chats/search", BODY),
        ],
    )]);
    let exact_only = |m: &str, p: &str| m == "GET" && p == "/api/chats/search";
    let list = classify(&r, &exact_only);
    assert_eq!(list[0].verdict, Verdict::Live { leaks: vec![("GET".to_string(), "/api/chats/search/".to_string())] });
}

#[test]
fn shared_files_and_live_files_lose_only_the_function() {
    let mut shared = stub("server/rust-projects.cjs", vec![refusal("POST", "/api/projects", "/api/projects", BODY)]);
    shared.importers.push("server/project-store-client.cjs".to_string());
    let live = stub("server/features.cjs", vec![refusal("GET", "/api/integrations/storage", "/api/integrations/storage", BODY)]);
    let r = synthetic(vec![shared, live]);
    let all = |_: &str, _: &str| true;
    let list = classify(&r, &all);
    assert_eq!(
        list[0].verdict,
        Verdict::DeleteFunction { other_importers: vec!["server/project-store-client.cjs".to_string()] }
    );
    assert_eq!(list[1].verdict, Verdict::DeleteFunction { other_importers: vec![] });
}

#[test]
fn tests_and_fixtures_do_not_count_as_importers() {
    let mut s = stub("server/rust-x.cjs", vec![refusal("GET", "/api/x", "/api/x", BODY)]);
    s.importers.extend(["tests/server/x.test.cjs", "server/fixtures/x.cjs", "server/native-x.probe.cjs"].map(String::from));
    s.string_refs.extend(["docs/port.md", "CHANGELOG.md"].map(String::from));
    s.mentions.push("server/routes/chat-lists.cjs".to_string());
    assert!(other_importers(&s).is_empty());
}

#[test]
fn a_production_string_reference_keeps_the_file() {
    let mut s = stub("server/rust-x.cjs", vec![refusal("GET", "/api/x", "/api/x", BODY)]);
    s.string_refs = vec!["server/route-preload.cjs".to_string(), "server/response-read-guard-920.allowlist.json".to_string()];
    assert_eq!(
        other_importers(&s),
        vec!["server/response-read-guard-920.allowlist.json".to_string(), "server/route-preload.cjs".to_string()]
    );
}

#[test]
fn a_stub_index_cjs_uses_for_more_than_its_refusal_loses_only_the_function() {
    let mut s = stub("server/rust-health.cjs", vec![refusal("GET", "/api/health", "/api/health", BODY)]);
    s.index_uses = vec!["createRagStatusPublisher".to_string(), "refuseOwned".to_string()];
    let r = synthetic(vec![s]);
    let all = |_: &str, _: &str| true;
    assert_eq!(
        classify(&r, &all)[0].verdict,
        Verdict::DeleteFunction { other_importers: vec!["server/index.cjs (createRagStatusPublisher)".to_string()] }
    );
}

#[test]
fn every_recorded_stub_is_used_by_index_cjs_for_its_refusal() {
    for s in rec().stubs {
        assert!(s.index_uses.contains(&s.function), "{}: index.cjs uses {:?}", s.file, s.index_uses);
    }
}

#[test]
fn switches_and_empty_recordings_are_kept() {
    let mut gated = stub("server/rust-model-writes.cjs", vec![]);
    gated.env = vec!["NOEVIA_RUST_MODEL_WRITES".to_string()];
    let mut switched = stub("server/rust-y.cjs", vec![refusal("GET", "/api/y", "/api/y", BODY)]);
    switched.switched = vec![Switched { env: BTreeMap::new(), refusals: vec![] }];
    let empty = stub("server/rust-z.cjs", vec![]);
    let mut broken = stub("server/rust-w.cjs", vec![]);
    broken.error = Some("require: boom".to_string());
    let r = synthetic(vec![gated, switched, empty, broken]);
    let all = |_: &str, _: &str| true;
    let v: Vec<Verdict> = classify(&r, &all).into_iter().map(|a| a.verdict).collect();
    assert_eq!(v, vec![Verdict::SwitchGated, Verdict::SwitchGated, Verdict::Unrecorded, Verdict::Unrecorded]);
}

#[test]
fn with_a_front_that_answers_nothing_no_recorded_stub_is_deletable() {
    let none = |_: &str, _: &str| false;
    for a in classify(&rec(), &none) {
        assert!(!matches!(a.verdict, Verdict::DeleteFile | Verdict::DeleteFunction { .. }), "{}", a.file);
    }
}

#[test]
fn with_a_front_that_answers_everything_every_plain_stub_is_deletable() {
    let all = |_: &str, _: &str| true;
    for a in classify(&rec(), &all) {
        if !matches!(a.verdict, Verdict::SwitchGated | Verdict::Unrecorded) {
            assert!(matches!(a.verdict, Verdict::DeleteFile | Verdict::DeleteFunction { .. }), "{}: {:?}", a.file, a.verdict);
        }
    }
}

// ---- the pin table ----

#[test]
fn the_pin_table_keeps_node_s_exact_bytes_and_the_first_stub_in_chain_order() {
    let first = stub("server/rust-a.cjs", vec![refusal("GET", "/api/a/", "/api/a", BODY)]);
    let second = stub("server/rust-b.cjs", vec![refusal("GET", "/api/a/", "/api/a", r#"{"error":"other"}"#)]);
    let r = synthetic(vec![first, second]);
    let none = |_: &str, _: &str| false;
    let t = PinTable::leaked(&r, &none);
    assert_eq!(t.len(), 1);
    let reply = t.lookup("GET", "/api/a", true);
    assert_eq!(reply.map(|x| x.body.as_slice()), Some(BODY.as_bytes()), "key order is JSON.stringify's: error before code");
    assert_eq!(reply.map(|x| x.status), Some(503));
    assert!(t.lookup("GET", "/api/a", false).is_none());
    assert!(t.lookup("POST", "/api/a", true).is_none());
}

#[test]
fn the_pin_table_never_pins_a_pattern_whose_probes_disagree_or_a_switch() {
    let two = stub(
        "server/rust-a.cjs",
        vec![refusal("GET", "/api/a/qa-1", "/api/a/{id}", BODY), refusal("GET", "/api/a/qa-seg", "/api/a/{id}", r#"{"error":"x"}"#)],
    );
    let mut gated = stub("server/rust-g.cjs", vec![refusal("PUT", "/api/g", "/api/g", BODY)]);
    gated.env = vec!["NOEVIA_RUST_G".to_string()];
    let r = synthetic(vec![two, gated]);
    let none = |_: &str, _: &str| false;
    let t = PinTable::leaked(&r, &none);
    assert!(t.is_empty());
    assert!(t.conflicts().contains(&("GET".to_string(), "/api/a/{id}".to_string(), false)));
}

#[test]
fn the_pin_table_is_empty_when_the_front_answers_everything() {
    let all = |_: &str, _: &str| true;
    assert!(PinTable::leaked(&rec(), &all).is_empty());
}

#[test]
fn the_delete_list_names_every_stub_and_its_pairing_lines() {
    let r = synthetic(vec![stub("server/rust-chat-search.cjs", vec![refusal("GET", "/api/chats/search", "/api/chats/search", BODY)])]);
    let all = |_: &str, _: &str| true;
    let md = delete_list(&r, &classify(&r, &all));
    assert!(md.contains("noevia-core `01234567`"));
    assert!(md.contains("| `server/rust-chat-search.cjs` | `refuseOwned` | 10 | 1 | delete file | pairing: deploy/tools/assemble-release.sh:1 |"));
    assert!(md.contains("1 files can go (10 lines)"));
}
