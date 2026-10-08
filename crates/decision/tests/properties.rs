//! Invariants: an answer the port accepts stays inside what was offered (every score key is an
//! offered string id with a finite score; a ranking holds offered ids once each; a choice is null
//! or offered), and no input panics.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use decision::{call, invalid_result, Js};
use prompt_framing::js::units;
use proptest::prelude::*;

fn leaf() -> impl Strategy<Value = Js> {
    prop_oneof![
        Just(Js::Undefined),
        Just(Js::Null),
        Just(Js::Opaque),
        Just(Js::Func),
        any::<bool>().prop_map(Js::Bool),
        prop_oneof![
            Just(0.0),
            Just(-0.0),
            Just(1.0),
            Just(f64::NAN),
            Just(f64::INFINITY)
        ]
        .prop_map(Js::Num),
        "[a-d]{0,2}".prop_map(|s| Js::Str(units(&s))),
    ]
}

fn obj(pairs: Vec<(String, Js)>) -> Js {
    let mut seen: Vec<Vec<u16>> = Vec::new();
    Js::Obj(
        pairs
            .into_iter()
            .filter_map(|(k, v)| {
                let k = units(&k);
                (!seen.contains(&k)).then(|| {
                    seen.push(k.clone());
                    (k, v)
                })
            })
            .collect(),
    )
}

fn request() -> impl Strategy<Value = Js> {
    let item = prop_oneof![leaf().prop_map(|id| obj(vec![("id".into(), id)])), leaf()];
    (
        prop_oneof![Just("rank"), Just("choice"), Just("multi"), Just("score")],
        prop::collection::vec(item, 0..4),
    )
        .prop_map(|(kind, items)| {
            obj(vec![
                ("kind".into(), Js::Str(units(kind))),
                ("items".into(), Js::Arr(items.clone())),
                ("options".into(), Js::Arr(items)),
            ])
        })
}

fn result() -> impl Strategy<Value = Js> {
    (
        prop::collection::vec(("[a-d]{0,2}", leaf()), 0..4),
        prop_oneof![
            leaf(),
            prop::collection::vec(leaf(), 0..4).prop_map(Js::Arr)
        ],
    )
        .prop_map(|(scores, selected)| {
            obj(vec![
                ("scores".into(), obj(scores)),
                ("selected".into(), selected),
            ])
        })
}

fn ids(r: &Js) -> Vec<Js> {
    let Js::Obj(m) = r else { return vec![] };
    let rank = m
        .iter()
        .any(|(k, v)| *k == units("kind") && *v == Js::Str(units("rank")));
    let key = units(if rank { "items" } else { "options" });
    let Some((_, Js::Arr(items))) = m.iter().find(|(k, _)| *k == key) else {
        return vec![];
    };
    items
        .iter()
        .map(|o| match o {
            Js::Obj(p) => p.first().map_or(Js::Undefined, |(_, v)| v.clone()),
            _ => Js::Undefined,
        })
        .collect()
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(4000))]

    #[test]
    fn accepted_answers_stay_inside(r in request(), res in result()) {
        if let Ok(None) = invalid_result(&r, &res) {
            let allowed = ids(&r);
            let Js::Obj(m) = &res else { unreachable!() };
            let Js::Obj(scores) = &m[0].1 else { unreachable!() };
            for (k, v) in scores {
                prop_assert!(allowed.contains(&Js::Str(k.clone())));
                prop_assert!(matches!(v, Js::Num(n) if n.is_finite()));
            }
            let selected = &m[1].1;
            match &r {
                Js::Obj(rm) if rm[0].1 == Js::Str(units("rank")) => {
                    let Js::Arr(sel) = selected else { panic!("rank accepted without an array") };
                    for (i, a) in sel.iter().enumerate() {
                        prop_assert!(matches!(a, Js::Num(n) if n.is_nan()) || allowed.contains(a) || allowed.iter().any(|b| matches!((a, b), (Js::Num(x), Js::Num(y)) if x == y)));
                        prop_assert!(!sel[i + 1..].iter().any(|b| a == b && !matches!(a, Js::Num(_))));
                    }
                }
                Js::Obj(rm) if rm[0].1 == Js::Str(units("choice")) => {
                    prop_assert!(*selected == Js::Null || allowed.contains(selected) || matches!(selected, Js::Num(_)));
                }
                _ => {}
            }
        }
    }

    #[test]
    fn no_input_panics(op in 0u8..5, body in prop::collection::vec(any::<u8>(), 0..256)) {
        let mut input = vec![op];
        input.extend(body);
        let (status, _) = call(&input);
        prop_assert!(status <= 1);
    }

    #[test]
    fn no_tagged_input_panics(body in "[\\[\\]\"a-z,0-9{}:. -]{0,64}", op in 1u8..4) {
        let mut input = vec![op];
        input.extend(body.as_bytes());
        let (status, _) = call(&input);
        prop_assert!(status <= 1);
    }
}
