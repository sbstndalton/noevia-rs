//! The sign-in lift of [`refusal::with_caps`] (council p9-3): a front that names both
//! [`refusal::CAP`] and [`refusal::SIGNIN_CAP`] runs the sign-in row itself; every other caps set
//! gives exactly the answer it gave before the lift. Checked over every combination of the seven
//! facts `refusal()` reads, against a decision table written out here, and against Node's own
//! answers recorded in `fixtures/node-upstream.v1.json` (the caps sets without the sign-in token).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use chat_upstream::refusal::{self, Facts, CAP, SIGNIN_CAP};
use js_json::JValue;

const FIXTURE: &str = include_str!("fixtures/node-upstream.v1.json");

/// Every combination: the row id against the default (same, other, absent on one side), and the
/// five flags.
fn every_facts() -> Vec<Facts> {
    let ids: [(Option<&str>, Option<&str>); 4] = [
        (Some("qa-default"), Some("qa-default")),
        (Some("qa-row"), Some("qa-default")),
        (None, Some("qa-default")),
        (None, None),
    ];
    let mut out = Vec::new();
    for (provider, default) in ids {
        for bits in 0u8..32 {
            out.push(Facts {
                provider_id: provider.map(str::to_string),
                default_provider_id: default.map(str::to_string),
                machine: bits & 1 != 0,
                external: bits & 2 != 0,
                sign_in: bits & 4 != 0,
                route_target: bits & 8 != 0,
                authorized: bits & 16 != 0,
            });
        }
    }
    out
}

/// The answer before the lift: the old `with_caps` (the sign-in row held with `providers`).
fn before_lift(f: &Facts, caps: &[&str]) -> Option<&'static str> {
    if !caps.contains(&CAP) {
        return refusal::node_reason(f);
    }
    (f.machine || f.sign_in || f.route_target).then_some("provider")
}

/// The decision table: which caps sets lift what.
fn table(f: &Facts, providers: bool, signin: bool) -> Option<&'static str> {
    match (providers, signin) {
        // Neither, or the sign-in token alone: Node's own answer.
        (false, _) => refusal::node_reason(f),
        // Provider rows lifted; machines, the sign-in row and route targets held.
        (true, false) => (f.machine || f.sign_in || f.route_target).then_some("provider"),
        // The sign-in row lifted too; machines and route targets still held.
        (true, true) => (f.machine || f.route_target).then_some("provider"),
    }
}

const HEADERS: [(&str, bool, bool); 11] = [
    ("", false, false),
    ("skills, once", false, false),
    ("providers", true, false),
    ("signin-transport-v1", false, true),
    ("providers,signin-transport-v1", true, true),
    (" signin-transport-v1 , skills ,providers ", true, true),
    // Near misses name nothing.
    ("providers, signin-transport-v10", true, false),
    ("providers, Signin-transport-v1", true, false),
    ("providers, signin-transport", true, false),
    ("providers signin-transport-v1", false, false),
    (",,providers,,signin-transport-v1,,", true, true),
];

#[test]
fn every_fact_combination_follows_the_table() {
    let facts = every_facts();
    assert_eq!(facts.len(), 128);
    for (header, providers, signin) in HEADERS {
        let caps = refusal::caps(header);
        assert_eq!(caps.contains(&CAP), providers, "{header:?}");
        assert_eq!(caps.contains(&SIGNIN_CAP), signin, "{header:?}");
        for f in &facts {
            assert_eq!(
                refusal::with_caps(f, &caps),
                table(f, providers, signin),
                "{header:?} {f:?}"
            );
        }
    }
}

#[test]
fn only_both_tokens_together_change_an_answer() {
    let mut changed = 0;
    for (header, providers, signin) in HEADERS {
        let caps = refusal::caps(header);
        for f in &every_facts() {
            let now = refusal::with_caps(f, &caps);
            let was = before_lift(f, &caps);
            if now != was {
                changed += 1;
                // Only a sign-in row that is neither a machine nor a route target, and only with both.
                assert!(providers && signin, "{header:?} {f:?}");
                assert!(f.sign_in && !f.machine && !f.route_target, "{header:?} {f:?}");
                assert_eq!((was, now), (Some("provider"), None), "{header:?} {f:?}");
            }
        }
    }
    // 4 id pairs x (sign_in, !machine, !route_target) x external x authorized = 16 per header,
    // and three headers name both tokens.
    assert_eq!(changed, 16 * 3);
}

#[test]
fn a_header_over_the_limit_names_neither_token() {
    let mut header = String::from("providers,signin-transport-v1,");
    while js_json::js_len(&header) <= 1000 {
        header.push('x');
    }
    let caps = refusal::caps(&header);
    assert!(caps.is_empty());
    for f in &every_facts() {
        assert_eq!(refusal::with_caps(f, &caps), refusal::node_reason(f), "{f:?}");
    }
}

#[test]
fn the_sign_in_token_alone_keeps_nodes_recorded_answers() {
    let recorded = js_json::parse(FIXTURE).expect("fixture");
    let JValue::Arr(cases) = recorded.get("refusals") else {
        panic!("fixture has no refusals list");
    };
    assert_eq!(cases.len(), 96);
    let mut sign_in_rows = 0;
    for r in cases {
        let f = r.get("facts");
        let text = |v: &JValue| v.as_str().map(str::to_string);
        let facts = Facts {
            provider_id: text(f.get("providerId")),
            default_provider_id: text(f.get("defaultProviderId")),
            machine: f.get("machine").truthy(),
            external: f.get("external").truthy(),
            sign_in: f.get("chatgpt").truthy(),
            route_target: f.get("routeTarget").truthy(),
            authorized: f.get("authorized").truthy(),
        };
        let want = r.get("node").as_str();
        assert_eq!(
            refusal::with_caps(&facts, &[SIGNIN_CAP, "skills"]),
            want,
            "{f:?}"
        );
        if facts.sign_in {
            sign_in_rows += 1;
            // Node refuses every recorded sign-in row; both tokens lift it unless it is also a
            // machine or a route target.
            assert_eq!(want, Some("provider"), "{f:?}");
            let both = refusal::with_caps(&facts, &[CAP, SIGNIN_CAP]);
            let held = facts.machine || facts.route_target;
            assert_eq!(both, held.then_some("provider"), "{f:?}");
        }
    }
    assert_eq!(sign_in_rows, 48);
}
