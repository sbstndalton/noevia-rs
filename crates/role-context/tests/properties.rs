//! Invariants over generated states: whatever the port hands out holds only the role's allowlisted
//! fields, stays under the total cap, and never carries a planted Diary, other-tenant, approval or
//! credential canary in any searched form; no input panics; large inputs stay bounded.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use prompt_framing::json::{self, Value};
use proptest::prelude::*;
use role_context::{call, project, MAX_INPUT_BYTES};
use std::time::{Duration, Instant};

const CANARIES: [&str; 5] = [
    "DIARY-CANARY-1111 private entry",
    "BOB-CANARY-2222 other tenant",
    "APPROVAL-TOKEN-CANARY-3333",
    "cred-CANARY-4444-value",
    "tenant-bob-5555",
];

fn js(s: &str) -> String {
    let mut out = Vec::new();
    json::push_str(&mut out, &s.encode_utf16().collect::<Vec<u16>>());
    String::from_utf8(out).unwrap()
}

/// Text that may quote a canary, a credential shape or a fragment of either.
fn piece() -> impl Strategy<Value = String> {
    prop_oneof![
        12 => Just("plain words".to_owned()),
        4 => Just("Planner".to_owned()),
        4 => Just("capabilities".to_owned()),
        12 => "[ -~]{0,40}",
        6 => ".{0,12}",
        1 => (0..CANARIES.len()).prop_map(|i| CANARIES[i].to_owned()),
        1 => (0..CANARIES.len()).prop_map(|i| CANARIES[i].to_lowercase()),
        2 => (0..CANARIES.len(), 0usize..20).prop_map(|(i, k)| CANARIES[i][k.min(CANARIES[i].len())..].to_owned()),
        1 => Just("sk-abcd1234efgh5678ijkl".to_owned()),
        1 => Just("ghp_abcdefghijklmnopqrstuvwxyz".to_owned()),
        1 => Just("-----BEGIN PRIVATE KEY-----".to_owned()),
        1 => Just("\\u0042OB-CANARY-2222 other tenant".to_owned()),
        1 => Just("ＤＩＡＲＹ－ＣＡＮＡＲＹ－１１１１ private entry".to_owned()),
        1 => Just("DIARY-\u{200b}CANARY-1111 private entry".to_owned()),
    ]
}

fn text_list(n: usize) -> impl Strategy<Value = Vec<String>> {
    prop::collection::vec(
        prop::collection::vec(piece(), 1..4).prop_map(|p| p.join(" ")),
        0..n,
    )
}

fn arr(xs: &[String]) -> String {
    format!(
        "[{}]",
        xs.iter().map(|x| js(x)).collect::<Vec<_>>().join(",")
    )
}

fn state() -> impl Strategy<Value = String> {
    (
        text_list(3),
        text_list(4),
        text_list(4),
        text_list(3),
        any::<bool>(),
        0usize..4,
    )
        .prop_map(|(request, constraints, plan, snippet_texts, own, role)| {
            let snippets: Vec<String> = snippet_texts
                .iter()
                .enumerate()
                .map(|(i, t)| {
                    let tenant = if own || i % 2 == 0 { "tenant-alice" } else { "tenant-bob-5555" };
                    let source = ["project", "selected", "diary", "repo-public"][i % 4];
                    format!("{{\"source\":{},\"tenantId\":{},\"text\":{}}}", js(source), js(tenant), js(t))
                })
                .collect();
            let state = format!(
                concat!(
                    "{{\"taskId\":\"t-1\",\"tenantId\":\"tenant-alice\",\"request\":{req},",
                    "\"projectInstructions\":{req},\"constraints\":{cons},",
                    "\"plan\":{{\"goal\":{req},\"constraints\":{plan},\"steps\":[{{\"do\":{req}}}]}},",
                    "\"capabilities\":{plan},\"feedback\":{cons},",
                    "\"execution\":{{\"summary\":{req},\"changedFiles\":{plan}}},",
                    "\"change\":{{\"files\":[{{\"path\":\"a\",\"patch\":{req}}}]}},",
                    "\"snippets\":[{snips}],",
                    "\"diary\":{{\"e\":{c0}}},\"otherTenants\":{{\"tenant-bob-5555\":{c1}}},",
                    "\"approvals\":[{{\"id\":\"appr-1\",\"token\":{c2},\"decision\":\"approve\"}}],",
                    "\"credentials\":{{\"k\":{c3}}},",
                    "\"roleSystemPrompts\":{{\"planner\":\"You plan.\",\"executor\":\"You execute.\"}}}}"
                ),
                req = js(&request.join(" ")),
                cons = arr(&constraints),
                plan = arr(&plan),
                snips = snippets.join(","),
                c0 = js(CANARIES[0]),
                c1 = js(CANARIES[1]),
                c2 = js(CANARIES[2]),
                c3 = js(CANARIES[3]),
            );
            let r = ["planner", "executor", "auditor", "reviewer"][role];
            format!("[{},{}]", js(r), state)
        })
}

fn input(op: u8, wire: &str) -> Vec<u8> {
    let mut v = vec![op];
    v.extend(wire.as_bytes());
    v
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(1500))]

    #[test]
    fn handed_out_projections_are_safe(wire in state()) {
        let (status, reply) = call(&input(1, &wire));
        if status != 0 {
            prop_assert!(reply == r#"{"error":"ambiguous"}"# || reply == r#"{"error":"too_large"}"#, "{reply}");
            return Ok(());
        }
        let v = json::parse_utf8(reply.as_bytes(), 16).unwrap();
        let Some(p @ Value::Obj(fields)) = v.get("projection") else { return Ok(()) };
        let role = String::from_utf16(p.get("role").and_then(Value::as_str).unwrap()).unwrap();
        let allowed = project::spec_keys(&role).unwrap();
        for (k, _) in fields {
            let k = String::from_utf16(k).unwrap();
            prop_assert!(allowed.contains(&k.as_str()), "{k}");
        }
        let body = reply.to_lowercase();
        prop_assert!(reply.chars().count() <= 40_100);
        for c in CANARIES {
            prop_assert!(!body.contains(&c.to_lowercase()), "{c} in {reply}");
        }
        prop_assert!(!body.contains("ghp_abcdefghijklmnopqrstuvwxyz"));
        prop_assert!(!body.contains("begin private key"));
    }

    #[test]
    fn dossiers_hold_only_shared_fields(wire in state()) {
        // wire is `["role",{state}]`; the role has no comma.
        let state = &wire[wire.find(',').unwrap() + 1..wire.len() - 1];
        let w = format!("[[\"planner\",\"executor\"],{state}]");
        let (status, reply) = call(&input(2, &w));
        if status == 0 {
            let v = json::parse_utf8(reply.as_bytes(), 16).unwrap();
            if let Some(Value::Obj(fields)) = v.get("dossier") {
                for (k, _) in fields {
                    let k = String::from_utf16(k).unwrap();
                    prop_assert!(["capabilities", "project_instructions", "request", "snippets", "task_id"].contains(&k.as_str()), "{k}");
                }
                for c in CANARIES {
                    prop_assert!(!reply.to_lowercase().contains(&c.to_lowercase()));
                }
            }
        }
    }

    #[test]
    fn random_bytes_never_panic(op in 0u8..4, body in prop::collection::vec(any::<u8>(), 0..300)) {
        let mut v = vec![op];
        v.extend(body);
        let (status, _) = call(&v);
        prop_assert!(status <= 1);
    }

    #[test]
    fn random_json_never_panics(op in 1u8..3, a in "[ -~]{0,30}", s in "[ -~]{0,200}") {
        let _ = call(&input(op, &format!("[{a},{s}]")));
        let _ = call(&input(op, &format!("[\"planner\",{{\"tenantId\":\"t\",\"request\":{}}}]", js(&s))));
    }
}

#[test]
fn oversized_input_is_refused() {
    let big = vec![b' '; MAX_INPUT_BYTES + 1];
    assert_eq!(call(&big), (1, r#"{"error":"too_large"}"#.to_owned()));
    assert_eq!(call(&[]), (1, r#"{"error":"input"}"#.to_owned()));
    assert_eq!(
        call(&input(3, "[1,2]")),
        (1, r#"{"error":"input"}"#.to_owned())
    );
    assert_eq!(
        call(&input(1, "[1]")),
        (1, r#"{"error":"input"}"#.to_owned())
    );
}

/// Large states finish fast: a 4 MiB request, 2,000 known credentials against it, deep nesting,
/// and many other-tenant ids are linear or stopped by the work bound, never slow.
#[test]
fn large_inputs_stay_bounded() {
    let cases = [
        format!(
            "[\"planner\",{{\"tenantId\":\"t\",\"request\":{}}}]",
            js(&"sk-x ".repeat(800_000))
        ),
        format!(
            "[\"planner\",{{\"tenantId\":\"t\",\"request\":{},\"credentials\":[{}]}}]",
            js(&"abcdefgh".repeat(250_000)),
            (0..2000)
                .map(|i| js(&format!("credential-{i:06}")))
                .collect::<Vec<_>>()
                .join(",")
        ),
        format!(
            "[\"auditor\",{{\"tenantId\":\"t\",\"diary\":{}1{}}}]",
            "[".repeat(100_000),
            "]".repeat(100_000)
        ),
        format!(
            "[\"planner\",{{\"tenantId\":\"t\",\"constraints\":[{}],\"snippets\":[{}]}}]",
            (0..12)
                .map(|_| js(&"aaa ".repeat(100)))
                .collect::<Vec<_>>()
                .join(","),
            (0..20000)
                .map(|i| format!("{{\"tenantId\":\"a{i}\",\"text\":\"x\"}}"))
                .collect::<Vec<_>>()
                .join(",")
        ),
        format!(
            "[\"planner\",{{\"tenantId\":\"t\",\"constraints\":[{}],\"diary\":[{}]}}]",
            (0..12)
                .map(|_| js(&"a".repeat(400)))
                .collect::<Vec<_>>()
                .join(","),
            (0..20000)
                .map(|_| js(&"a".repeat(300)))
                .collect::<Vec<_>>()
                .join(",")
        ),
    ];
    for (i, c) in cases.iter().enumerate() {
        let t = Instant::now();
        let (status, reply) = call(&input(1, c));
        let took = t.elapsed();
        assert!(status <= 1, "case {i}: {reply}");
        assert!(took < Duration::from_secs(30), "case {i} took {took:?}");
    }
}

/// The safety property above is not vacuous: a fair share of generated states are handed out,
/// and a fair share are refused as leaks.
#[test]
fn generated_states_cover_both_outcomes() {
    use proptest::strategy::ValueTree;
    use proptest::test_runner::TestRunner;
    let mut runner = TestRunner::deterministic();
    let (mut handed, mut leaked) = (0, 0);
    for _ in 0..400 {
        let wire = state().new_tree(&mut runner).unwrap().current();
        let (_, reply) = call(&input(1, &wire));
        handed += usize::from(reply.starts_with("{\"projection\""));
        leaked += usize::from(reply.starts_with("{\"leak\""));
    }
    assert!(
        handed >= 60 && leaked >= 60,
        "{handed} handed out, {leaked} leaked"
    );
}
