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
        importers: vec![
            "server/index.cjs".to_string(),
            format!("{}.test.cjs", file.trim_end_matches(".cjs")),
        ],
        string_refs: vec![],
        mentions: vec![],
        index_uses: vec!["refuseOwned".to_string()],
        pairing: vec!["deploy/tools/assemble-release.sh:1".to_string()],
        refusals,
        switched: vec![],
        owned_routes: None,
        behind: BTreeMap::new(),
        behind_methods: BTreeMap::new(),
        error: None,
    }
}

fn refusal(method: &str, path: &str, pattern: &str, body: &str) -> Refusal {
    Refusal {
        method: method.to_string(),
        path: path.to_string(),
        pattern: Some(pattern.to_string()),
        status: 503,
        headers: [
            ("content-type", "application/json"),
            ("cache-control", "no-store"),
        ]
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
        routes_toml_sha256: None,
        fills: vec![],
        probes: 1,
        stubs,
    }
}

const BODY: &str =
    r#"{"error":"Chat search is owned by the Rust front.","code":"RUST_CHAT_SEARCH_OWNED"}"#;

// ---- the recording itself ----

#[test]
fn the_recording_covers_core_s_request_chain() {
    let r = rec();
    assert_eq!(r.generator, "tools/gen-node-refusal-stubs.cjs");
    assert_eq!(
        r.methods,
        METHODS.iter().map(|m| m.to_string()).collect::<Vec<_>>()
    );
    // index.cjs held 68 refusal calls at the recorded core; a recording with far fewer read the wrong file.
    assert!(r.stubs.len() >= 60, "only {} stubs recorded", r.stubs.len());
    assert!(r.probes >= 400, "only {} probe paths", r.probes);
    let files: BTreeSet<&str> = r.stubs.iter().map(|s| s.file.as_str()).collect();
    for must in [
        "server/rust-chat-search.cjs",
        "server/rust-residual-fallbacks.cjs",
        "server/rust-auth.cjs",
    ] {
        assert!(files.contains(must), "{must} missing from the recording");
    }
}

#[test]
fn every_recorded_refusal_is_a_fixed_json_503_or_405() {
    for s in rec().stubs {
        assert!(s.error.is_none(), "{}: {:?}", s.file, s.error);
        for r in s
            .refusals
            .iter()
            .chain(s.switched.iter().flat_map(|w| w.refusals.iter()))
        {
            assert!(
                r.error.is_none(),
                "{} {} {}: {:?}",
                s.file,
                r.method,
                r.path,
                r.error
            );
            assert!(
                [503, 405].contains(&r.status),
                "{} {} {} answered {}",
                s.file,
                r.method,
                r.path,
                r.status
            );
            assert_eq!(
                r.headers.get("content-type").map(String::as_str),
                Some("application/json"),
                "{}",
                s.file
            );
            assert_eq!(
                r.headers.get("cache-control").map(String::as_str),
                Some("no-store"),
                "{}",
                s.file
            );
            assert!(
                r.body
                    .get("error")
                    .and_then(|e| e.as_str())
                    .is_some_and(|e| !e.is_empty()),
                "{}: {}",
                s.file,
                r.body
            );
            assert!(METHODS.contains(&r.method.as_str()));
        }
    }
}

#[test]
fn raw_bodies_are_the_parsed_bodies() {
    for s in rec().stubs {
        for r in &s.refusals {
            if let Some(t) = &r.body_text {
                let parsed: serde_json::Value =
                    serde_json::from_str(t).unwrap_or(serde_json::Value::Null);
                assert_eq!(parsed, r.body, "{} {} {}", s.file, r.method, r.path);
            }
        }
    }
}

#[test]
fn every_probe_names_its_routes_toml_pattern_or_a_page() {
    let pages = [
        "/about",
        "/about/",
        "/privacy",
        "/privacy/",
        "/device",
        "/device/",
        "/.well-known/webauthn",
        "/",
    ];
    for s in rec().stubs {
        for r in &s.refusals {
            match &r.pattern {
                Some(p) => assert!(p.starts_with('/'), "{p}"),
                None => assert!(
                    pages.contains(&r.path.as_str()) || r.path.starts_with("/api/"),
                    "{}",
                    r.path
                ),
            }
        }
    }
}

// ---- classify ----

#[test]
fn a_dedicated_stub_the_front_fully_answers_can_be_deleted() {
    let r = synthetic(vec![stub(
        "server/rust-chat-search.cjs",
        vec![refusal(
            "GET",
            "/api/chats/search",
            "/api/chats/search",
            BODY,
        )],
    )]);
    let all = |_: &str, _: &str| true;
    let list = classify(&r, &all);
    assert_eq!(list.len(), 1);
    assert_eq!(list[0].verdict, Verdict::DeleteFile);
    assert_eq!(
        totals(&list),
        Totals {
            delete_file: 1,
            delete_file_lines: 10,
            delete_file_tests: 0,
            delete_function: 0,
            with_handler: 0,
            keep: 0
        }
    );
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
    assert_eq!(
        list[0].verdict,
        Verdict::Live {
            leaks: vec![("GET".to_string(), "/api/chats/search/".to_string())]
        }
    );
}

#[test]
fn shared_files_and_live_files_lose_only_the_function() {
    let mut shared = stub(
        "server/rust-projects.cjs",
        vec![refusal("POST", "/api/projects", "/api/projects", BODY)],
    );
    shared
        .importers
        .push("server/project-store-client.cjs".to_string());
    let live = stub(
        "server/features.cjs",
        vec![refusal(
            "GET",
            "/api/integrations/storage",
            "/api/integrations/storage",
            BODY,
        )],
    );
    let r = synthetic(vec![shared, live]);
    let all = |_: &str, _: &str| true;
    let list = classify(&r, &all);
    assert_eq!(
        list[0].verdict,
        Verdict::DeleteFunction {
            other_importers: vec!["server/project-store-client.cjs".to_string()]
        }
    );
    assert_eq!(
        list[1].verdict,
        Verdict::DeleteFunction {
            other_importers: vec![]
        }
    );
}

#[test]
fn tests_and_fixtures_do_not_count_as_importers() {
    let mut s = stub(
        "server/rust-x.cjs",
        vec![refusal("GET", "/api/x", "/api/x", BODY)],
    );
    s.importers.extend(
        [
            "tests/server/x.test.cjs",
            "server/fixtures/x.cjs",
            "server/native-x.probe.cjs",
        ]
        .map(String::from),
    );
    s.string_refs
        .extend(["docs/port.md", "CHANGELOG.md"].map(String::from));
    s.mentions.push("server/routes/chat-lists.cjs".to_string());
    assert!(other_importers(&s).is_empty());
}

#[test]
fn a_production_string_reference_keeps_the_file() {
    let mut s = stub(
        "server/rust-x.cjs",
        vec![refusal("GET", "/api/x", "/api/x", BODY)],
    );
    s.string_refs = vec![
        "server/route-preload.cjs".to_string(),
        "server/response-read-guard-920.allowlist.json".to_string(),
    ];
    assert_eq!(
        other_importers(&s),
        vec![
            "server/response-read-guard-920.allowlist.json".to_string(),
            "server/route-preload.cjs".to_string()
        ]
    );
}

#[test]
fn a_stub_index_cjs_uses_for_more_than_its_refusal_loses_only_the_function() {
    let mut s = stub(
        "server/rust-health.cjs",
        vec![refusal("GET", "/api/health", "/api/health", BODY)],
    );
    s.index_uses = vec![
        "createRagStatusPublisher".to_string(),
        "refuseOwned".to_string(),
    ];
    let r = synthetic(vec![s]);
    let all = |_: &str, _: &str| true;
    assert_eq!(
        classify(&r, &all)[0].verdict,
        Verdict::DeleteFunction {
            other_importers: vec!["server/index.cjs (createRagStatusPublisher)".to_string()]
        }
    );
}

#[test]
fn every_recorded_stub_is_used_by_index_cjs_for_its_refusal() {
    for s in rec().stubs {
        assert!(
            s.index_uses.contains(&s.function),
            "{}: index.cjs uses {:?}",
            s.file,
            s.index_uses
        );
    }
}

#[test]
fn switches_and_empty_recordings_are_kept() {
    let mut gated = stub("server/rust-model-writes.cjs", vec![]);
    gated.env = vec!["NOEVIA_RUST_MODEL_WRITES".to_string()];
    let mut switched = stub(
        "server/rust-y.cjs",
        vec![refusal("GET", "/api/y", "/api/y", BODY)],
    );
    switched.switched = vec![Switched {
        env: BTreeMap::new(),
        refusals: vec![],
    }];
    let empty = stub("server/rust-z.cjs", vec![]);
    let mut broken = stub("server/rust-w.cjs", vec![]);
    broken.error = Some("require: boom".to_string());
    let r = synthetic(vec![gated, switched, empty, broken]);
    let all = |_: &str, _: &str| true;
    let v: Vec<Verdict> = classify(&r, &all).into_iter().map(|a| a.verdict).collect();
    assert_eq!(
        v,
        vec![
            Verdict::SwitchGated,
            Verdict::SwitchGated,
            Verdict::Unrecorded,
            Verdict::Unrecorded
        ]
    );
}

#[test]
fn with_a_front_that_answers_nothing_no_recorded_stub_is_deletable() {
    let none = |_: &str, _: &str| false;
    for a in classify(&rec(), &none) {
        assert!(
            !matches!(
                a.verdict,
                Verdict::DeleteFile
                    | Verdict::DeleteFileEditTests { .. }
                    | Verdict::DeleteFunction { .. }
            ),
            "{}",
            a.file
        );
    }
}

#[test]
fn with_a_front_that_answers_everything_every_plain_stub_is_deletable() {
    let all = |_: &str, _: &str| true;
    for a in classify(&rec(), &all) {
        if !matches!(
            a.verdict,
            Verdict::SwitchGated | Verdict::Unrecorded | Verdict::Incomplete { .. }
        ) {
            assert!(
                matches!(
                    a.verdict,
                    Verdict::DeleteFile
                        | Verdict::DeleteFileEditTests { .. }
                        | Verdict::DeleteFunction { .. }
                        | Verdict::GuardsHandler { .. }
                ),
                "{}: {:?}",
                a.file,
                a.verdict
            );
        }
    }
}

// ---- the pin table ----

#[test]
fn the_pin_table_keeps_node_s_exact_bytes_and_the_first_stub_in_chain_order() {
    let first = stub(
        "server/rust-a.cjs",
        vec![refusal("GET", "/api/a/", "/api/a", BODY)],
    );
    let second = stub(
        "server/rust-b.cjs",
        vec![refusal("GET", "/api/a/", "/api/a", r#"{"error":"other"}"#)],
    );
    let r = synthetic(vec![first, second]);
    let none = |_: &str, _: &str| false;
    let t = PinTable::leaked(&r, &none);
    assert_eq!(t.len(), 1);
    let reply = t.lookup("GET", "/api/a", true);
    assert_eq!(
        reply.map(|x| x.body.as_slice()),
        Some(BODY.as_bytes()),
        "key order is JSON.stringify's: error before code"
    );
    assert_eq!(reply.map(|x| x.status), Some(503));
    assert!(t.lookup("GET", "/api/a", false).is_none());
    assert!(t.lookup("POST", "/api/a", true).is_none());
}

#[test]
fn the_pin_table_never_pins_a_pattern_whose_probes_disagree_or_a_switch() {
    let two = stub(
        "server/rust-a.cjs",
        vec![
            refusal("GET", "/api/a/qa-1", "/api/a/{id}", BODY),
            refusal("GET", "/api/a/qa-seg", "/api/a/{id}", r#"{"error":"x"}"#),
        ],
    );
    let mut gated = stub(
        "server/rust-g.cjs",
        vec![refusal("PUT", "/api/g", "/api/g", BODY)],
    );
    gated.env = vec!["NOEVIA_RUST_G".to_string()];
    let r = synthetic(vec![two, gated]);
    let none = |_: &str, _: &str| false;
    let t = PinTable::leaked(&r, &none);
    assert!(t.is_empty());
    assert!(t
        .conflicts()
        .contains(&("GET".to_string(), "/api/a/{id}".to_string(), false)));
}

#[test]
fn the_pin_table_is_empty_when_the_front_answers_everything() {
    let all = |_: &str, _: &str| true;
    assert!(PinTable::leaked(&rec(), &all).is_empty());
}

#[test]
fn the_delete_list_names_every_stub_and_its_pairing_lines() {
    let r = synthetic(vec![stub(
        "server/rust-chat-search.cjs",
        vec![refusal(
            "GET",
            "/api/chats/search",
            "/api/chats/search",
            BODY,
        )],
    )]);
    let all = |_: &str, _: &str| true;
    let md = delete_list(&r, &classify(&r, &all));
    assert!(md.contains("noevia-core `01234567`"));
    assert!(md.contains("| `server/rust-chat-search.cjs` | `refuseOwned` | 10 | 1 | delete file | pairing: deploy/tools/assemble-release.sh:1 |"));
    assert!(md.contains("1 files can go (10 lines)"));
}

// ---- council p5: the re-recording, OWNED_ROUTES, handlers behind, switches, the manifest front ----

/// The front's `noevia-server --owned-routes` at the recording's rs commit (this branch), pinned beside it.
const MANIFEST: &str = include_str!("../fixtures/front-owned-routes.txt");
const ROUTES_TOML: &str = include_str!("../../../contracts/http/routes.toml");
/// Distinct `path = "…"` rows in that routes.toml.
const ROUTES_TOML_ROWS: usize = 211;
const UUID: &str = "00000000-0000-4000-8000-000000000001";

fn manifest() -> Manifest {
    Manifest::parse(MANIFEST).expect("the pinned manifest parses")
}

fn hex(s: &str, len: usize) -> bool {
    s.len() == len && s.chars().all(|c| c.is_ascii_hexdigit())
}

#[test]
fn the_recording_names_its_commits_its_routes_toml_and_both_fillings() {
    let r = rec();
    assert!(hex(&r.core, 40), "core {}", r.core);
    assert!(hex(&r.noevia, 40), "noevia {}", r.noevia);
    assert!(hex(r.routes_toml_sha256.as_deref().unwrap_or(""), 64));
    assert_eq!(r.fills, ["qa-1", UUID]);
    assert_eq!(r.probes, 527);
    assert_eq!(r.stubs.len(), 68);
}

#[test]
fn every_recorded_pattern_is_still_a_routes_toml_row() {
    // A routes.toml row removed since the recording makes the fixture stale: re-record.
    let rows: BTreeSet<&str> = ROUTES_TOML
        .lines()
        .filter_map(|l| l.strip_prefix("path = \"")?.strip_suffix('"'))
        .collect();
    // sbstndalton/noevia#2725 item 3: the exact row count of the routes.toml the recording names (its sha256 is
    // pinned in the fixture; a changed routes.toml fails the stale guard above or this count).
    assert_eq!(rows.len(), ROUTES_TOML_ROWS);
    for s in &rec().stubs {
        for r in &s.refusals {
            if let Some(p) = &r.pattern {
                assert!(rows.contains(p.as_str()), "{}: {p} is not in routes.toml", s.file);
            }
        }
    }
}

#[test]
fn uuid_only_stubs_are_recorded_on_uuid_paths() {
    // sbstndalton/noevia#2713 item 1: rust-research.cjs refuses only [0-9a-f-]{36} job ids.
    let r = rec();
    let research = r.stubs.iter().find(|s| s.file == "server/rust-research.cjs").expect("research stub");
    assert!(research
        .refusals
        .iter()
        .any(|x| x.path.contains(&format!("/research/{UUID}"))));
    assert!(unrecorded_owned(research).is_empty(), "{:?}", unrecorded_owned(research));
}

#[test]
fn pairing_lines_cover_every_script_and_the_stub_s_markers() {
    // sbstndalton/noevia#2713 item 2: the marker reader and the residual pairing suite were missed before.
    let r = rec();
    let residual = r
        .stubs
        .iter()
        .find(|s| s.file == "server/rust-residual-fallbacks.cjs")
        .expect("residual stub");
    for want in [
        "deploy/tools/assemble-release.sh:4270",
        "deploy/tools/test-residual-fallback-pairing.sh:9",
        "deploy/tools/test-assemble-release.sh:3437",
    ] {
        assert!(residual.pairing.iter().any(|p| p == want), "{want} missing: {:?}", residual.pairing);
    }
    assert!(r.stubs.iter().all(|s| s.pairing.iter().all(|p| p.rsplit_once(':').is_some_and(|(_, n)| n.parse::<usize>().is_ok()))));
}

#[test]
fn exact_totals_with_a_front_that_answers_everything() {
    // sbstndalton/noevia#2713 item 4: these numbers move only with a re-recording.
    let all = |_: &str, _: &str| true;
    let list = classify(&rec(), &all);
    assert_eq!(
        totals(&list),
        Totals {
            delete_file: 33,
            delete_file_tests: 8,
            delete_file_lines: 842,
            delete_function: 7,
            with_handler: 24,
            keep: 4
        }
    );
    let gated: Vec<&str> = list
        .iter()
        .filter(|a| a.verdict == Verdict::SwitchGated)
        .map(|a| a.file.as_str())
        .collect();
    assert_eq!(
        gated,
        [
            "server/rust-model-writes.cjs",
            "server/rust-model-manager-proxy.cjs",
            "server/rust-projects.cjs"
        ]
    );
    let incomplete: Vec<&Assessment> = list
        .iter()
        .filter(|a| matches!(a.verdict, Verdict::Incomplete { .. }))
        .collect();
    assert_eq!(incomplete.len(), 1);
    assert_eq!(incomplete[0].file, "server/rust-project-reads.cjs");
    assert_eq!(
        incomplete[0].verdict,
        Verdict::Incomplete {
            missing: vec!["GET /api/projects/{id}/{*}/original".to_string()]
        }
    );
}

#[test]
fn exact_totals_from_the_front_s_own_manifest() {
    let r = rec();
    let m = manifest();
    assert_eq!(m.len(), 700);
    let list = classify(&r, &m.front(&r));
    assert_eq!(
        totals(&list),
        Totals {
            delete_file: 32,
            delete_file_lines: 799,
            delete_file_tests: 8,
            delete_function: 5,
            with_handler: 22,
            keep: 9
        }
    );
    // Live only through what the manifest cannot show: trailing slashes, the residual answers, the page paths.
    let live: Vec<&str> = list
        .iter()
        .filter(|a| matches!(a.verdict, Verdict::Live { .. }))
        .map(|a| a.file.as_str())
        .collect();
    assert_eq!(
        live,
        [
            "server/routes/features.cjs",
            "server/rust-public-pages.cjs",
            "server/rust-residual-fallbacks.cjs",
            "server/rust-mcp-credentials.cjs",
            "server/rust-mcp-directory.cjs"
        ]
    );
    // Methods a Rust row does not list that the front still answers itself (bins/noevia-server residual_fallback.rs,
    // Node's exact 404/405 shapes): not route-table ownership, so not in the manifest.
    let residual_answers = [
        ("PUT", "/api/mcp-keys/servers"),
        ("DELETE", "/api/mcp-keys/servers"),
        ("DELETE", "/api/mcp-oauth/callback"),
        ("DELETE", "/api/mcp-oauth/servers"),
    ];
    let mut seen_residual = BTreeSet::new();
    for a in &list {
        if let Verdict::Live { leaks } = &a.verdict {
            for (method, path) in leaks {
                let slash = path.len() > 1 && path.ends_with('/');
                let unpatterned = r
                    .stubs
                    .iter()
                    .flat_map(|s| s.refusals.iter())
                    .any(|x| x.path == *path && x.pattern.is_none());
                let residual_stub = a.file == "server/rust-residual-fallbacks.cjs";
                let residual = residual_answers.contains(&(method.as_str(), path.as_str()));
                if residual {
                    seen_residual.insert((method.clone(), path.clone()));
                }
                assert!(
                    slash || unpatterned || residual_stub || residual,
                    "{}: {method} {path}",
                    a.file
                );
            }
        }
    }
    assert_eq!(seen_residual.len(), residual_answers.len());
}

#[test]
fn the_manifest_backs_every_owned_routes_claim() {
    // Every route a stub claims in OWNED_ROUTES is in the front's manifest. Council p7: the `{*}/original` row is
    // listed now that the manifest fills `{*}` with one segment (the recording still has the two-segment probe, so
    // rust-project-reads stays Incomplete until a re-record).
    assert!(manifest().unowned_claims(&rec()).is_empty());
}

#[test]
fn every_delete_file_stub_names_the_foreign_tests_that_require_it() {
    // sbstndalton/noevia#2725 item 1: rust-offsite-backup.cjs is required by the kept rust-google-drive.test.cjs.
    let r = rec();
    let m = manifest();
    let list = classify(&r, &m.front(&r));
    let offsite = list
        .iter()
        .find(|a| a.file == "server/rust-offsite-backup.cjs")
        .expect("offsite stub");
    assert_eq!(
        offsite.verdict,
        Verdict::DeleteFileEditTests {
            tests: vec!["server/rust-google-drive.test.cjs".to_string()]
        }
    );
    let edits: Vec<(&str, usize)> = list
        .iter()
        .filter_map(|a| match &a.verdict {
            Verdict::DeleteFileEditTests { tests } => Some((a.file.as_str(), tests.len())),
            _ => None,
        })
        .collect();
    assert_eq!(
        edits,
        [
            ("server/rust-model-downloads-read.cjs", 1),
            ("server/rust-plugin-directory.cjs", 1),
            ("server/rust-toolboxes-permitted.cjs", 1),
            ("server/rust-calibration-routes.cjs", 2),
            ("server/rust-model-manager-scan.cjs", 2),
            ("server/rust-offsite-backup.cjs", 1),
            ("server/rust-chat-search.cjs", 1),
            ("server/rust-chat-export.cjs", 1),
        ]
    );
    // A plain DeleteFile has no foreign test left.
    for s in &r.stubs {
        if let Some(a) = list.iter().find(|a| a.file == s.file && a.verdict == Verdict::DeleteFile) {
            assert!(foreign_tests(s).is_empty(), "{}", a.file);
        }
    }
}

#[test]
fn the_manifest_parser_refuses_what_is_not_a_manifest() {
    assert!(Manifest::parse("").is_err());
    assert!(Manifest::parse("\n\n").is_err());
    assert!(Manifest::parse("GET /api/x\nnot a line\n").is_err());
    assert!(Manifest::parse("FETCH /api/x\n").is_err());
    assert!(Manifest::parse("GET api/x\n").is_err());
    assert!(Manifest::parse("GET /api/x y\n").is_err());
    let m = Manifest::parse("GET /api/a/{id}\r\nDELETE /api/b\n").expect("parses");
    assert_eq!(m.len(), 2);
    assert!(m.owns("GET", "/api/a/{name}"));
    assert!(!m.owns("POST", "/api/a/{id}"));
    assert!(m.owns("DELETE", "/api/b"));
}

#[test]
fn the_manifest_front_answers_patterned_probes_only_without_a_trailing_slash() {
    let r = synthetic(vec![stub(
        "server/rust-a.cjs",
        vec![
            refusal("GET", "/api/a/qa-1", "/api/a/{id}", BODY),
            refusal("GET", "/api/a/qa-1/", "/api/a/{id}", BODY),
        ],
    )]);
    let m = Manifest::parse("GET /api/a/{x}\n").expect("parses");
    let f = m.front(&r);
    assert!(f.answers("GET", "/api/a/qa-1"));
    assert!(!f.answers("GET", "/api/a/qa-1/"));
    assert!(!f.answers("POST", "/api/a/qa-1"));
    assert!(!f.answers("GET", "/api/unrecorded"));
    assert_eq!(
        classify(&r, &f)[0].verdict,
        Verdict::Live {
            leaks: vec![("GET".to_string(), "/api/a/qa-1/".to_string())]
        }
    );
}

#[test]
fn an_owned_route_without_a_recorded_refusal_makes_the_stub_incomplete() {
    let mut s = stub(
        "server/rust-y.cjs",
        vec![refusal("GET", "/api/y/qa-1", "/api/y/{id}", BODY)],
    );
    s.owned_routes = Some(vec![
        "GET /api/y/{name}".to_string(),
        "ALL /api/z".to_string(),
        "garbage".to_string(),
    ]);
    let all = |_: &str, _: &str| true;
    assert_eq!(
        classify(&synthetic(vec![s.clone()]), &all)[0].verdict,
        Verdict::Incomplete {
            missing: vec!["ALL /api/z".to_string(), "garbage".to_string()]
        }
    );
    // ALL needs every method.
    s.owned_routes = Some(vec!["ALL /api/y/{id}".to_string()]);
    assert_eq!(unrecorded_owned(&s), ["ALL /api/y/{id}"]);
    s.refusals = METHODS
        .iter()
        .map(|m| refusal(m, "/api/y/qa-1", "/api/y/{id}", BODY))
        .collect();
    assert!(unrecorded_owned(&s).is_empty());
    assert_eq!(classify(&synthetic(vec![s]), &all)[0].verdict, Verdict::DeleteFile);
}

#[test]
fn a_core_handler_behind_the_stub_must_go_with_it() {
    // sbstndalton/noevia#2700 item 1.
    let mut s = stub(
        "server/rust-y.cjs",
        vec![refusal("POST", "/api/y", "/api/y", BODY)],
    );
    s.behind = [
        ("/api/y".to_string(), vec!["server/routes/y.cjs:9".to_string(), "server/index.cjs:3".to_string()]),
        ("/api/y2".to_string(), vec!["server/routes/y.cjs:9".to_string()]),
    ]
    .into_iter()
    .collect();
    let all = |_: &str, _: &str| true;
    let r = synthetic(vec![s]);
    let list = classify(&r, &all);
    assert_eq!(
        list[0].verdict,
        Verdict::GuardsHandler {
            handlers: vec!["server/index.cjs:3".to_string(), "server/routes/y.cjs:9".to_string()]
        }
    );
    assert_eq!(totals(&list).with_handler, 1);
    let md = delete_list(&r, &list);
    assert!(md.contains("| delete with handler | core still names the route at server/index.cjs:3, server/routes/y.cjs:9; pairing: deploy/tools/assemble-release.sh:1 |"), "{md}");
    assert!(md.contains("1 can go only with the core handler they guard"));
    // A leak still wins: the stub is live before anything else.
    let none = |_: &str, _: &str| false;
    assert!(matches!(classify(&r, &none)[0].verdict, Verdict::Live { .. }));
}

#[test]
fn a_named_switch_keeps_the_stub_even_when_the_recorded_values_did_not_change_it() {
    // sbstndalton/noevia#2713 item 3.
    let mut s = stub(
        "server/rust-y.cjs",
        vec![refusal("GET", "/api/y", "/api/y", BODY)],
    );
    s.env = vec!["NOEVIA_RUST_Y".to_string()];
    let all = |_: &str, _: &str| true;
    let r = synthetic(vec![s]);
    assert_eq!(classify(&r, &all)[0].verdict, Verdict::SwitchGated);
    let none = |_: &str, _: &str| false;
    assert!(PinTable::leaked(&r, &none).is_empty());
}

#[test]
fn the_delete_list_names_the_routes_toml_revision() {
    let r = rec();
    let all = |_: &str, _: &str| true;
    let md = delete_list(&r, &classify(&r, &all));
    let sha = r.routes_toml_sha256.clone().expect("hash");
    assert!(md.contains(&format!("routes.toml sha256 `{}`", &sha[..8])));
    assert!(md.contains("33 files can go (842 lines), 7"));
    assert!(md.contains("24 can go only with the core handler they guard, 4 stay"));
}

#[test]
fn normalised_is_the_assembler_s_sed() {
    assert_eq!(normalised("/api/chats/{chatId}/export"), "/api/chats/{}/export");
    assert_eq!(normalised("/api/models/{*}"), "/api/models/{}");
    assert_eq!(normalised("/api/open/{x"), "/api/open/{x");
    assert_eq!(owned_entry("ALL /api/x/{id}"), Some(("ALL", "/api/x/{}".to_string())));
    assert_eq!(owned_entry("/api/x"), None);
    assert_eq!(owned_entry("GET api/x"), None);
}

/// The safe delete list from the pinned recording and the pinned front manifest (no live routes.toml or dispatch, so
/// only a re-recording moves it). `ROUTE_PINS_WRITE=1 cargo test -p route-pins` rewrites it.
#[test]
fn the_delete_list_golden_is_current() {
    let r = rec();
    let m = manifest();
    let mut md = delete_list(&r, &classify(&r, &m.front(&r)));
    md.push_str(
        "\nVerdicts from `fixtures/front-owned-routes.txt` (`noevia-server --owned-routes`). Live rows are \
         conservative: the manifest cannot show trailing-slash tolerance, the residual 404/405 answers or the page \
         paths, all of which the front answers itself. Node's public headers are not part of the recorded replies.\n",
    );
    let at = concat!(env!("CARGO_MANIFEST_DIR"), "/fixtures/delete-list.md");
    if std::env::var_os("ROUTE_PINS_WRITE").is_some() {
        std::fs::write(at, &md).expect("write the golden");
    }
    let want = std::fs::read_to_string(at).unwrap_or_default();
    assert!(want == md, "fixtures/delete-list.md is stale: ROUTE_PINS_WRITE=1 cargo test -p route-pins");
}

#[test]
fn a_handler_for_another_method_does_not_guard_the_stub() {
    // Council p7 (CONTINUE 3 of p5): idle-unload's stub refuses the GET; core's PUT handler behind it is not exposed
    // by deleting the stub.
    let mut s = stub(
        "server/rust-z.cjs",
        vec![
            refusal("GET", "/api/z", "/api/z", BODY),
            refusal("HEAD", "/api/z", "/api/z", BODY),
        ],
    );
    s.behind = [(
        "/api/z".to_string(),
        vec!["server/routes/z.cjs:4".to_string(), "server/routes/z.cjs:9".to_string(), "server/x.cjs:1".to_string()],
    )]
    .into_iter()
    .collect();
    s.behind_methods = [
        ("server/routes/z.cjs:4".to_string(), vec!["PUT".to_string()]),
        ("server/routes/z.cjs:9".to_string(), vec!["GET".to_string()]),
    ]
    .into_iter()
    .collect();
    assert_eq!(
        handlers_behind(&s),
        ["server/routes/z.cjs:9".to_string(), "server/x.cjs:1".to_string()]
    );
    // Only the PUT handler and an unrelated line: the PUT line drops, the unknown one stays.
    s.behind = [("/api/z".to_string(), vec!["server/routes/z.cjs:4".to_string()])].into_iter().collect();
    assert!(handlers_behind(&s).is_empty());
    let all = |_: &str, _: &str| true;
    let list = classify(&synthetic(vec![s.clone()]), &all);
    assert_eq!(list[0].verdict, Verdict::DeleteFile);
    // GET covers a HEAD-only refusal.
    s.refusals = vec![refusal("HEAD", "/api/z", "/api/z", BODY)];
    s.behind_methods.insert("server/routes/z.cjs:4".to_string(), vec!["GET".to_string()]);
    assert_eq!(handlers_behind(&s), ["server/routes/z.cjs:4".to_string()]);
}

#[test]
fn the_pinned_recording_has_no_method_facts_yet_so_every_line_counts() {
    // Re-recording with the p7 recorder fills behind_methods; until then the verdicts are p5's.
    for s in &rec().stubs {
        assert!(s.behind_methods.is_empty(), "{}", s.file);
        let all: BTreeSet<&String> = s.behind.values().flatten().collect();
        assert_eq!(handlers_behind(s).len(), all.len(), "{}", s.file);
    }
}
