//! Invariants of the dedupe key over generated JSON: it is JSON.stringify([name, canonical]), the
//! canonical form is a fixed point (canonical of canonical), member order and whitespace never
//! change it, and random bytes never panic the call.
#![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]

use prompt_framing::json::{self, Value};
use proptest::prelude::*;
use tool_exchange::{call, check, Check};

#[derive(Clone, Debug)]
enum J {
    Lit(&'static str),
    Str(String),
    Arr(Vec<J>),
    Obj(Vec<(String, J)>),
}

fn write(j: &J, out: &mut String, ws: &str, reverse: bool) {
    match j {
        J::Lit(l) => out.push_str(l),
        J::Str(s) => {
            out.push('"');
            out.push_str(s);
            out.push('"');
        }
        J::Arr(xs) => {
            out.push('[');
            for (i, x) in xs.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                    out.push_str(ws);
                }
                write(x, out, ws, reverse);
            }
            out.push(']');
        }
        J::Obj(m) => {
            out.push('{');
            out.push_str(ws);
            let mut members: Vec<&(String, J)> = m.iter().collect();
            if reverse {
                members.reverse();
            }
            for (i, (k, x)) in members.into_iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                out.push_str(&format!("\"{k}\"{ws}:{ws}"));
                write(x, out, ws, reverse);
            }
            out.push('}');
        }
    }
}

fn text() -> impl Strategy<Value = String> {
    prop::collection::vec(
        prop_oneof![
            Just("a"),
            Just("B"),
            Just("é"),
            Just("😀"),
            Just("\u{ff5e}"),
            Just("\\u0000"),
            Just("\\ud800"),
            Just("\\n"),
            Just("\\\""),
            Just("\\\\"),
            Just("\\/"),
            Just("\\udc00"),
            Just("1"),
            Just("_"),
        ],
        0..6,
    )
    .prop_map(|v| v.concat())
}

fn value() -> impl Strategy<Value = J> {
    let leaf = prop_oneof![
        prop_oneof![
            Just("null"),
            Just("true"),
            Just("false"),
            Just("0"),
            Just("-0"),
            Just("1.50"),
            Just("1e21"),
            Just("1E-7"),
            Just("123456789012345678901234567890"),
            Just("1e400"),
            Just("-2.5e-3"),
            Just("5e-324"),
        ]
        .prop_map(J::Lit),
        text().prop_map(J::Str),
    ];
    leaf.prop_recursive(5, 64, 6, |inner| {
        prop_oneof![
            prop::collection::vec(inner.clone(), 0..6).prop_map(J::Arr),
            // Unique keys, so reversing member order cannot change which duplicate wins.
            prop::collection::btree_map(text(), inner, 0..6)
                .prop_map(|m| J::Obj(m.into_iter().collect())),
        ]
    })
}

fn key(args: &str) -> Vec<u16> {
    let units: Vec<u16> = args.encode_utf16().collect();
    match check(false, true, &[0x74], Some(&units)).unwrap() {
        Check::Run(k) => k,
        Check::Answer(a) => panic!("{args}: {}", String::from_utf16_lossy(&a)),
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(2000))]

    #[test]
    fn keys_are_canonical(members in prop::collection::btree_map(text(), value(), 0..6)) {
        let obj = J::Obj(members.into_iter().collect());
        let (mut plain, mut spaced) = (String::new(), String::new());
        write(&obj, &mut plain, "", false);
        write(&obj, &mut spaced, " \n\t", true);
        let k = key(&plain);
        prop_assert_eq!(&k, &key(&spaced));
        // ["t", canonical]: and canonical is its own canonical form.
        let Some(Value::Arr(pair)) = json::parse(&k, 8) else { panic!("key is not JSON") };
        prop_assert_eq!(pair.len(), 2);
        let canon = String::from_utf16(pair[1].as_str().unwrap()).ok();
        if let Some(canon) = canon {
            prop_assert_eq!(&key(&canon), &k);
        }
    }

    #[test]
    fn call_never_panics(bytes in prop::collection::vec(any::<u8>(), 0..200)) {
        let (status, _) = call(&bytes);
        prop_assert!(status <= 1);
    }
}
