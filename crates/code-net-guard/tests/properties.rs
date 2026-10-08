//! Invariants over generated specs, answers and local addresses: the port never serves a request
//! the JS's string comparison would refuse (the exact normalised match always refuses), everything
//! it keeps is an IP address or a host name of the JS's pattern, and no input panics.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use code_net_guard::{call, is_ip, normalize, parse_spec, refuses, resolved_addresses, Spec};
use prompt_framing::js::units;
use proptest::prelude::*;

fn piece() -> impl Strategy<Value = String> {
    prop_oneof![
        Just("172.30.0.2".to_owned()),
        Just("::ffff:172.30.0.2".to_owned()),
        Just("::ffff:ac1e:2".to_owned()),
        Just("::1".to_owned()),
        Just("0:0::1".to_owned()),
        Just("FE80::1".to_owned()),
        Just("fe80::1%eth0".to_owned()),
        Just("egress".to_owned()),
        Just("xn--a".to_owned()),
        Just("2130706433".to_owned()),
        "[0-9a-fA-F:.%]{0,16}",
        "[a-z0-9._-]{0,10}",
        "[ -~]{0,12}",
        ".{0,6}",
    ]
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(4000))]

    #[test]
    fn spec_keeps_the_rules(p in prop::collection::vec(piece(), 0..6), sep in "[ ,\t]{1,2}") {
        let raw = p.join(&sep);
        if let Ok(Spec::Ok { literals, hosts }) = parse_spec(&units(&raw)) {
            for l in &literals {
                prop_assert!(is_ip(l.as_bytes()), "{l}");
                prop_assert!(!l.contains('%'));
            }
            for h in &hosts {
                prop_assert!(!h.is_empty() && h.len() <= 253 && h.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'.' || b == b'-'), "{h}");
                prop_assert!(!h.contains("xn--"));
            }
        }
    }

    #[test]
    fn never_serves_what_the_js_refuses(addrs in prop::collection::vec(piece(), 0..5), local in piece()) {
        // The JS set: the normalised IP addresses among the pieces (as resolveOnce keeps them).
        let answers: Vec<Vec<u16>> = addrs.iter().map(|a| units(a)).collect();
        let answers: Vec<Option<&[u16]>> = answers.iter().map(|a| Some(a.as_slice())).collect();
        let Ok(set) = resolved_addresses(&answers) else { return Ok(()) };
        let set_u: Vec<Vec<u16>> = set.iter().map(|s| units(s)).collect();
        let set_u: Vec<&[u16]> = set_u.iter().map(Vec::as_slice).collect();
        let js = local.is_ascii() && set.iter().any(|s| s.as_bytes() == normalize(local.as_bytes()).as_slice());
        // Any refusal makes the host refuse, so only an answer can serve.
        if let Ok(port) = refuses(&set_u, Some(&units(&local))) {
            prop_assert!(port || !js, "served {local} the JS refuses");
        }
    }

    #[test]
    fn random_bytes_never_panic(op in 0u8..5, body in prop::collection::vec(any::<u8>(), 0..64)) {
        let mut input = vec![op];
        input.extend(body);
        let (status, reply) = call(&input);
        prop_assert!(status <= 1 && !reply.is_empty());
    }

    #[test]
    fn json_shaped_input_never_panics(op in 1u8..4, s in "[\\[\\]\",nul0-9a-f:.% ]{0,40}") {
        let mut input = vec![op];
        input.extend(s.as_bytes());
        let (status, _) = call(&input);
        prop_assert!(status <= 1);
    }
}

#[test]
fn large_inputs_stay_linear_and_bounded() {
    // 1,024 guarded addresses and a 60 KiB spec: hashed, so this is quick even unoptimised.
    let addrs: Vec<String> = (0..1024)
        .map(|i| format!("10.0.{}.{}", i / 256, i % 256))
        .collect();
    let mut wire = b"\x03[[".to_vec();
    wire.extend(
        addrs
            .iter()
            .map(|a| format!("\"{a}\""))
            .collect::<Vec<_>>()
            .join(",")
            .bytes(),
    );
    wire.extend(b"],\"10.0.3.255\"]");
    assert_eq!(call(&wire), (0, r#"{"refuses":true}"#.to_owned()));
    let mut too_many = b"\x03[[".to_vec();
    too_many.extend(vec!["\"10.0.0.1\""; 1025].join(",").bytes());
    too_many.extend(b"],null]");
    assert_eq!(call(&too_many).0, 1);
    let spec = format!("\x01\"{}\"", "egress ".repeat(60 * 1024 / 7));
    assert_eq!(call(spec.as_bytes()).0, 0);
    let big = vec![b' '; code_net_guard::MAX_INPUT_BYTES + 1];
    assert_eq!(call(&big), (1, r#"{"error":"too_large"}"#.to_owned()));
}
