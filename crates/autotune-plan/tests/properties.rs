//! Properties: no panics on any input, a simulated run (any outcome sequence, however
//! inconsistent) always ends within the step caps without planning a measured step twice, and
//! the first probe follows the KV policy (noevia#1057): the most precise type that fits, unless a
//! more compact one fits at least twice its context.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use autotune_plan::{
    estimate_bytes, plan, plan_json, step_json, usable_bytes, Entry, Facts, Input, KvType, Memory,
    Outcome, ProbeOutcome, Step, MAX_PROBES, MAX_VERIFY,
};
use proptest::prelude::*;

const LADDER: [u64; 12] = [
    4096, 8192, 12288, 16384, 24576, 32768, 49152, 65536, 98304, 131072, 196608, 262144,
];
const PROBE_OUTCOMES: [ProbeOutcome; 6] = [
    ProbeOutcome::Passed,
    ProbeOutcome::Oom,
    ProbeOutcome::LoadFailed,
    ProbeOutcome::RecallFailed,
    ProbeOutcome::OverTime,
    ProbeOutcome::QualityFailed,
];
const OUTCOMES: [Outcome; 3] = [Outcome::Passed, Outcome::Skipped, Outcome::Failed];

fn facts() -> impl Strategy<Value = Facts> {
    (
        (0u64..300_000, 1u64..96, 1u64..64, 0u64..16, 64u64..8192),
        (0u64..3, 0u64..4096, 0u64..4, 0u64..3),
        (
            1u64..40_000_000_000,
            0u64..2_000_000_000,
            0u64..4096,
            1u64..4,
        ),
    )
        .prop_map(
            |(
                (n_ctx_train, block_count, head_count, kv, embedding_length),
                (layout, window, interval, nextn),
                (model_bytes, mmproj_bytes, ubatch, slots),
            )| Facts {
                n_ctx_train,
                block_count,
                head_count,
                head_count_kv: if kv == 0 { vec![] } else { vec![kv] },
                embedding_length,
                sliding_window: if layout == 1 { window } else { 0 },
                sliding_window_pattern: if layout == 1 {
                    (0..block_count).map(|i| u64::from(i % 6 != 5)).collect()
                } else {
                    vec![]
                },
                full_attention_interval: if layout == 2 { interval } else { 0 },
                nextn_predict_layers: nextn,
                model_bytes,
                mmproj_bytes,
                ubatch,
                slots,
                ..Facts::default()
            },
        )
}

fn memory() -> impl Strategy<Value = Memory> {
    (
        8192u64..65536,
        proptest::option::of(16384u64..131072),
        0u64..4096,
        0u64..4096,
        0u64..4096,
    )
        .prop_map(
            |(budget_mib, avail, reserve_mib, floor_mib, cache_ram_mib)| Memory {
                budget_mib,
                mem_available_mib: avail,
                reserve_mib,
                floor_mib,
                cache_ram_mib,
            },
        )
}

fn kv_list() -> impl Strategy<Value = Vec<KvType>> {
    proptest::sample::subsequence(
        vec![
            KvType::Bf16,
            KvType::F16,
            KvType::Q8_0,
            KvType::Q5_1,
            KvType::Q5_0,
            KvType::Q4_0,
        ],
        1..=6,
    )
}

/// The largest ladder rung `kv` fits by the estimate, at most the trained context.
fn ceiling(input: &Input, kv: KvType) -> Option<u64> {
    let usable = usable_bytes(&input.memory);
    input
        .ladder
        .iter()
        .copied()
        .filter(|&c| input.facts.n_ctx_train == 0 || c <= input.facts.n_ctx_train)
        .filter(|&c| {
            estimate_bytes(&input.facts, &input.memory, c, kv).is_some_and(|b| b <= usable)
        })
        .max()
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(512))]

    #[test]
    fn the_first_probe_is_precision_first_with_the_doubling_rule(
        facts in facts(),
        memory in memory(),
        kv in kv_list(),
    ) {
        let input = Input { facts, memory, ladder: LADDER.to_vec(), kv, results: vec![] };
        if let Step::Probe { ctx, kv, .. } = plan(&input) {
            let at = input.kv.iter().position(|&k| k == kv).unwrap();
            prop_assert_eq!(Some(ctx), ceiling(&input, kv));
            // The most precise type that fits at all.
            let first = input.kv.iter().position(|&k| ceiling(&input, k).is_some()).unwrap();
            prop_assert!(at >= first);
            let first_cap = ceiling(&input, input.kv[first]).unwrap();
            if at > first {
                prop_assert!(ctx >= 2 * first_cap, "{} at {} does not double {}", kv.name(), ctx, first_cap);
            }
            // No more compact type doubles the choice.
            for &k in &input.kv[at + 1..] {
                if let Some(c) = ceiling(&input, k) {
                    prop_assert!(c < 2 * ctx, "{} at {} doubles {} at {}", k.name(), c, kv.name(), ctx);
                }
            }
        }
    }

    #[test]
    fn any_text_gets_a_fixed_reply(t in ".{0,300}") {
        let (status, reply) = plan_json(&t);
        prop_assert!(status <= 1);
        if status == 1 {
            let fixed = reply == r#"{"error":"input"}"# || reply == r#"{"error":"too_large"}"#;
            prop_assert!(fixed, "unexpected refusal");
        }
    }

    #[test]
    fn any_json_shape_gets_a_fixed_reply(
        n in proptest::collection::vec(any::<i64>(), 0..20),
        s in proptest::collection::vec("[a-z_0-9]{0,8}", 0..6),
    ) {
        let text = serde_json::json!({
            "facts": {"nCtxTrain": n.first(), "blockCount": n.get(1), "headCount": n.get(2),
                "headCountKv": n.get(3..6), "embeddingLength": n.get(6), "modelBytes": n.get(7),
                "slidingWindow": n.get(8), "slidingWindowPattern": n.get(9..12)},
            "memory": {"budgetMib": n.get(12), "memAvailableMib": n.get(13), "cacheRamMib": n.get(14)},
            "ladder": n.get(15..),
            "kv": s,
            "results": [{"step": s.first(), "ctx": n.get(16), "kv": s.get(1), "outcome": s.get(2), "id": s.get(3)}],
        }).to_string();
        let (status, reply) = plan_json(&text);
        prop_assert!(status <= 1);
        prop_assert!(!reply.is_empty());
    }

    #[test]
    fn a_run_ends_within_the_caps_and_never_repeats_a_step(
        facts in facts(),
        memory in memory(),
        kv in kv_list(),
        probe_outcomes in proptest::collection::vec(0usize..12, 32),
        phase_outcomes in proptest::collection::vec(0usize..6, 4),
    ) {
        let mut input = Input { facts, memory, ladder: LADDER.to_vec(), kv, results: vec![] };
        let usable = usable_bytes(&input.memory);
        let mut probes = 0;
        let mut verifies = 0;
        let mut phases = 0;
        let mut tried_probe = std::collections::HashSet::new();
        let mut tried_verify = std::collections::HashSet::new();
        let mut passed_ctx: Option<u64> = None;
        let limit = MAX_PROBES + 3 + MAX_VERIFY + 2;
        let mut end = None;
        for _ in 0..limit {
            let step = plan(&input);
            prop_assert!(!step_json(&step).is_empty());
            match step {
                Step::Probe { ctx, kv, fill, estimate_mib } => {
                    prop_assert!(tried_probe.insert((ctx, kv.name())), "repeated probe {ctx} {}", kv.name());
                    prop_assert!(input.kv.contains(&kv));
                    prop_assert!(input.facts.n_ctx_train == 0 || ctx <= input.facts.n_ctx_train);
                    prop_assert!(estimate_bytes(&input.facts, &input.memory, ctx, kv).unwrap() <= usable);
                    prop_assert!(fill < ctx);
                    prop_assert!(estimate_mib > 0);
                    let outcome = PROBE_OUTCOMES[probe_outcomes[probes % 32].min(5) * usize::from(probe_outcomes[probes % 32] < 6)];
                    if outcome == ProbeOutcome::Passed {
                        passed_ctx = Some(passed_ctx.map_or(ctx, |p: u64| p.max(ctx)));
                    }
                    probes += 1;
                    input.results.push(Entry::Probe { ctx, kv, outcome });
                }
                Step::Phase { id, ctx, .. } => {
                    prop_assert!(probes > 0);
                    prop_assert!(Some(ctx) <= passed_ctx);
                    let outcome = OUTCOMES[phase_outcomes[phases % 4] * usize::from(phase_outcomes[phases % 4] < 3)];
                    phases += 1;
                    input.results.push(Entry::Phase { id, outcome });
                }
                Step::Verify { ctx, kv, .. } => {
                    prop_assert_eq!(phases, 3);
                    prop_assert!(tried_verify.insert(ctx), "repeated verify {}", ctx);
                    let outcome = PROBE_OUTCOMES[probe_outcomes[(probes + verifies + 7) % 32].min(5) * usize::from(probe_outcomes[(probes + verifies + 7) % 32] < 6)];
                    verifies += 1;
                    input.results.push(Entry::Verify { ctx, kv, outcome });
                }
                Step::Serving { .. } => {
                    input.results.push(Entry::Serving { outcome: OUTCOMES[phase_outcomes[3] * usize::from(phase_outcomes[3] < 3)] });
                }
                s @ (Step::Done { .. } | Step::Fail(_)) => { end = Some(s); break; }
            }
            prop_assert!(probes <= MAX_PROBES);
            prop_assert!(verifies <= MAX_VERIFY);
        }
        let end = end.expect("the plan ended within the step cap");
        if std::env::var_os("PLAN_STATS").is_some() { eprintln!("END {}", step_json(&end)); }
        if let Step::Done { ctx, .. } = end {
            prop_assert!(Some(ctx) <= passed_ctx);
            let verified = input.results.iter().any(|e| matches!(e, Entry::Verify { ctx: c, outcome: ProbeOutcome::Passed, .. } if *c == ctx));
            prop_assert!(verified);
        }
        // The same results always give the same step.
        prop_assert_eq!(plan(&input), plan(&input.clone()));
    }
}
