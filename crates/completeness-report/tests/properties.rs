//! Properties of the port: it never panics, never reports "pass" without the evidence the JS
//! requires, ignores forged claims, and stays bounded on large and deep jobs.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use completeness_report::{build, call, Status, Unhashable, MAX_INPUT_BYTES};
use prompt_framing::json::{self, Value};
use proptest::prelude::*;

fn run(wire: &str) -> (u32, String) {
    let mut input = vec![1u8];
    input.extend(wire.as_bytes());
    call(&input)
}

fn parse(wire: &str) -> (Value, Value) {
    let Value::Arr(mut parts) = json::parse_utf8(wire.as_bytes(), 80).unwrap() else {
        panic!()
    };
    let e = parts.pop().unwrap();
    (parts.pop().unwrap(), e)
}

fn esc(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

fn step() -> impl Strategy<Value = String> {
    (
        prop::sample::select(vec![
            "tests",
            "test",
            "run-tests",
            "test-suite",
            "build",
            "Tests",
        ]),
        prop::sample::select(vec!["completed", "running", "failed", "skipped"]),
    )
        .prop_map(|(id, st)| format!(r#"{{"id":"{id}","status":"{st}"}}"#))
}

fn artifact() -> impl Strategy<Value = String> {
    prop_oneof![
        "[a-z]{0,6}".prop_map(|n| format!(r#"{{"name":"{n}","kind":"file"}}"#)),
        any::<bool>().prop_map(|p| format!(r#"{{"kind":"test-report","passed":{p}}}"#)),
        Just("null".to_owned()),
    ]
}

fn job() -> impl Strategy<Value = (String, String)> {
    (
        prop::collection::vec(step(), 0..5),
        prop::collection::vec(artifact(), 0..4),
        prop::option::of(prop::sample::select(vec!["proposed", "skipped", "edited"])),
        0usize..3,
        any::<bool>(),
        prop::option::of((
            prop::sample::select(vec!["sha", "headSha", "commit", "other"]),
            prop::sample::select(vec!["abcdef0", "ABCDEF0123", "xyz", "", "0000000g"]),
        )),
        prop::option::of(prop::collection::vec("[a-z]{0,6}", 0..3)),
    )
        .prop_map(|(steps, arts, plan, unc, pending, cp, expected)| {
            let plan = plan.map_or("null".to_owned(), |p| format!(r#"{{"status":"{p}"}}"#));
            let cp = cp.map_or("null".to_owned(), |(k, v)| format!(r#"{{"{k}":"{v}"}}"#));
            let unc = vec!["{}"; unc].join(",");
            let job = format!(
                r#"{{"id":"j","steps":[{}],"artifacts":[{}],"plan":{plan},"uncertain":[{unc}],"pendingApproval":{},"checkpoint":{cp}}}"#,
                steps.join(","),
                arts.join(","),
                if pending { r#"{"tool":"write"}"# } else { "null" }
            );
            let expected = expected.map_or("null".to_owned(), |xs| {
                format!(
                    "[{}]",
                    xs.iter().map(|x| format!("\"{}\"", esc(x))).collect::<Vec<_>>().join(",")
                )
            });
            (job, expected)
        })
}

fn str_is(v: Option<&Value>, s: &str) -> bool {
    v.and_then(Value::as_str)
        .is_some_and(|u| u.iter().copied().eq(s.encode_utf16()))
}

proptest! {
    #[test]
    fn random_bytes_never_panic(bytes in prop::collection::vec(any::<u8>(), 0..512)) {
        let _ = call(&bytes);
    }

    #[test]
    fn random_json_never_panics(s in "[\\[\\]{}\",:0-9a-z\\\\ntrufalse]{0,200}") {
        let _ = run(&s);
        let _ = run(&format!("[{s},null]"));
    }

    // Pass needs exactly the evidence the JS requires; never on a forged or partial job.
    #[test]
    fn pass_only_with_full_evidence((job, expected) in job()) {
        let wire = format!("[{job},{expected}]");
        let (status, reply) = run(&wire);
        prop_assert_eq!(status, 0, "{}", reply);
        let (j, e) = parse(&wire);
        let r = build(&j, &e).unwrap();
        prop_assert_eq!(r.can_enter_reviewing(), r.overall == Status::Pass);
        if r.overall == Status::Pass {
            let Some(Value::Arr(steps)) = j.get("steps") else { panic!() };
            let Some(Value::Arr(arts)) = j.get("artifacts") else { panic!() };
            prop_assert!(!steps.is_empty());
            prop_assert!(steps.iter().all(|s| str_is(s.get("status"), "completed")));
            prop_assert!(steps.iter().any(|s| ["tests", "test", "run-tests", "test-suite"].iter().any(|t| str_is(s.get("id"), t)))
                || arts.iter().any(|a| str_is(a.get("kind"), "test-report") && a.get("passed") == Some(&Value::Bool(true))));
            prop_assert!(arts.iter().all(|a| a.get("passed") != Some(&Value::Bool(false))));
            prop_assert!(!matches!(e, Value::Null));
            prop_assert_eq!(j.get("uncertain"), Some(&Value::Arr(vec![])));
            prop_assert_eq!(j.get("pendingApproval"), Some(&Value::Null));
            prop_assert!(reply.contains(r#""overall":"pass""#));
        }
    }

    // Caller- or model-chosen claims on the job are never read: the reply is byte-identical.
    #[test]
    fn forged_claims_are_inert((job, expected) in job(), claim in prop::sample::select(vec![
        r#""testsPassed":true"#, r#""review":"approved""#, r#""merged":true"#, r#""result":{"tests":"pass"}"#,
        r#""lifecycle":"reviewing""#, r#""report":{"overall":"pass"}"#,
    ])) {
        let base = run(&format!("[{job},{expected}]"));
        let forged = format!("{{{claim},{}", &job[1..]);
        prop_assert_eq!(run(&format!("[{forged},{expected}]")), base);
    }
}

#[test]
fn large_reports_refuse_to_hash_and_stay_bounded() {
    // One 1.5M-unit failing test step is serialised three times (tests-run, steps, open).
    let title = "x".repeat(1_500_000);
    let wire =
        format!(r#"[{{"steps":[{{"id":"tests","status":"failed","title":"{title}"}}]}},null]"#);
    let (j, e) = parse(&wire);
    let r = build(&j, &e).unwrap();
    assert_eq!(r.canonical, Err(Unhashable::Large));
    assert_eq!(r.overall, Status::Fail);
    let (status, reply) = run(&wire);
    assert_eq!(status, 0);
    assert_eq!(
        reply,
        r#"{"unhashable":"large","overall":"fail","statuses":["fail","unknown","fail","pass","unknown"]}"#
    );
    // Just under the budget it hashes.
    let small = format!(
        r#"[{{"steps":[{{"id":"tests","status":"failed","title":"{}"}}]}},null]"#,
        "x".repeat(1_300_000)
    );
    assert!(run(&small).1.starts_with(r#"{"hash":""#));
}

#[test]
fn oversized_input_is_refused() {
    let mut input = vec![1u8];
    input.resize(MAX_INPUT_BYTES + 1, b' ');
    assert_eq!(call(&input), (1, r#"{"error":"too_large"}"#.to_owned()));
}

#[test]
fn many_steps_and_names_stay_linear() {
    let steps: Vec<String> = (0..20_000)
        .map(|i| format!(r#"{{"id":"s{i}","status":"completed"}}"#))
        .collect();
    let arts: Vec<String> = (0..20_000)
        .map(|i| format!(r#"{{"name":"a{i}"}}"#))
        .collect();
    let expected: Vec<String> = (0..20_000).map(|i| format!(r#""a{}""#, i * 2)).collect();
    let wire = format!(
        r#"[{{"steps":[{}],"artifacts":[{}],"plan":{{"status":"proposed"}}}},[{}]]"#,
        steps.join(","),
        arts.join(","),
        expected.join(",")
    );
    let t = std::time::Instant::now();
    let (status, reply) = run(&wire);
    assert_eq!(status, 0);
    assert!(reply.contains("Missing expected artifact(s): a20000, a20002"));
    assert!(t.elapsed().as_secs() < 20);
}

#[test]
fn very_deep_input_is_bounded() {
    let depth = 100_000;
    let wire = format!(
        r#"[{{"checkpoint":{}"sha":"abcdef0"{}}},null]"#,
        r#"{"d":"#.repeat(depth) + "{",
        "}".repeat(depth + 1)
    );
    let (status, reply) = run(&wire);
    assert_eq!(status, 0, "{reply}");
    assert!(reply.starts_with(r#"{"unhashable":"deep""#), "{reply}");
}
