//! Properties (noevia#1004): no input panics; a rule verdict or a measurement is never changed by
//! advice; advice decides only when no rule matched and it is confident enough; nothing from the
//! input reaches the reason; the same input always gives the same reply.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use load_verdict::{
    classify, verdict, verdict_json, Advice, Cause, Evidence, Label, Request, Source,
    MIN_ADVICE_PERMILLE,
};
use proptest::prelude::*;

fn cause() -> impl Strategy<Value = Cause> {
    prop::sample::select(Cause::ALL.to_vec())
}

fn label() -> impl Strategy<Value = Label> {
    prop::sample::select(Label::ALL.to_vec())
}

fn text() -> impl Strategy<Value = String> {
    prop_oneof![
        ".{0,200}",
        prop::sample::select(vec![
            "out of memory",
            "ErrorOutOfDeviceMemory",
            "failed to parse chat template",
            "timed out",
            "invalid magic",
            "context shift is disabled",
            "server error",
            "",
        ])
        .prop_map(str::to_owned),
    ]
}

fn evidence() -> impl Strategy<Value = Evidence> {
    (
        prop::option::of(0u16..1000),
        prop::option::of(-1024i32..1024),
        text(),
    )
        .prop_map(|(status, exit_code, text)| Evidence {
            status,
            exit_code,
            text,
        })
}

fn advice() -> impl Strategy<Value = Option<Advice>> {
    prop::option::of(
        (label(), 0u16..=1000).prop_map(|(label, permille)| Advice { label, permille }),
    )
}

proptest! {
    #[test]
    fn never_panics_on_bytes(s in ".{0,400}") {
        let (status, reply) = verdict_json(&s);
        prop_assert!(status <= 1);
        prop_assert!(serde_json::from_str::<serde_json::Value>(&reply).is_ok());
    }

    #[test]
    fn advice_never_overrides(c in cause(), e in prop::option::of(evidence()), a in advice()) {
        let r = Request { cause: c, evidence: e.clone(), advice: a };
        let v = verdict(&r);
        let without = verdict(&Request { advice: None, ..r.clone() });
        match &e {
            None => {
                prop_assert_eq!(v.source, Source::Measured);
                prop_assert_eq!(v.outcome, c.outcome());
            }
            Some(ev) => {
                let (rule, _) = classify(ev);
                prop_assert_eq!(v.rule, rule);
                if let Some(o) = rule.outcome() {
                    prop_assert_eq!(v.source, Source::Rule);
                    prop_assert_eq!(v.outcome, o);
                } else if v.advice_used {
                    let a = a.unwrap();
                    prop_assert!(a.permille >= MIN_ADVICE_PERMILLE);
                    prop_assert_eq!(Some(v.outcome), a.label.outcome());
                    prop_assert_eq!(v.source, Source::Advisor);
                } else {
                    prop_assert_eq!(v.source, Source::Fallback);
                    prop_assert_eq!(v.outcome, c.outcome());
                }
            }
        }
        // Without advice the outcome is the rules' (or the cause's) alone.
        prop_assert!(!without.advice_used);
        if !v.advice_used { prop_assert_eq!(v.outcome, without.outcome); }
        // Asking is only offered when advice could matter.
        if v.ask { prop_assert!(a.is_none() && without.source == Source::Fallback); }
    }

    #[test]
    fn reason_never_echoes_input(c in cause(), marker in "[A-Za-z]{12}", status in prop::option::of(0u16..1000), a in advice()) {
        let input = serde_json::json!({
            "cause": c.name(),
            "evidence": {"status": status, "text": format!("{marker} out of memory {marker}")},
            "advice": a.map(|a| serde_json::json!({"label": a.label.name(), "confidence": f64::from(a.permille) / 1000.0})),
        }).to_string();
        let (s, reply) = verdict_json(&input);
        prop_assert_eq!(s, 0);
        prop_assert!(!reply.contains(&marker));
        prop_assert_eq!(verdict_json(&input), (s, reply));
    }
}
