//! Properties over generated, adversarial URLs and arbitrary input: no panics, fixed public
//! errors that never echo input, and "allowed" only for what the JS also allows, judged by an
//! oracle written here independently of `egress::is_private_ip` (std's address types and the
//! CIDR table from ssrf.cjs's comments).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]

use proptest::prelude::*;
use ssrf_policy::{check_url, run_json, Kind, Mode};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

const PRIVATE_V4: [(u32, u32); 14] = [
    (0x0000_0000, 8),
    (0x0a00_0000, 8),
    (0x7f00_0000, 8),
    (0x6440_0000, 10),
    (0xa9fe_0000, 16),
    (0xac10_0000, 12),
    (0xc0a8_0000, 16),
    (0xc000_0000, 24),
    (0xc000_0200, 24),
    (0xc058_6300, 24),
    (0xc612_0000, 15),
    (0xc633_6400, 24),
    (0xcb00_7100, 24),
    (0xe000_0000, 3),
];

fn oracle_v4_public(a: Ipv4Addr) -> bool {
    let n = u32::from(a);
    !PRIVATE_V4
        .iter()
        .any(|&(net, bits)| (n ^ net) >> (32 - bits) == 0)
}

fn oracle_v6_public(a: Ipv6Addr) -> bool {
    let s = a.segments();
    if s[..5] == [0; 5] && s[5] == 0xffff {
        return oracle_v4_public(Ipv4Addr::new(
            (s[6] >> 8) as u8,
            s[6] as u8,
            (s[7] >> 8) as u8,
            s[7] as u8,
        ));
    }
    let global = s[0] & 0xe000 == 0x2000;
    let teredo = s[0] == 0x2001 && s[1] == 0;
    let doc = s[0] == 0x2001 && s[1] == 0x0db8;
    let six_to_four = s[0] == 0x2002;
    global && !teredo && !doc && !six_to_four
}

fn oracle_public(host: &str) -> bool {
    match host.parse::<IpAddr>() {
        Ok(IpAddr::V4(a)) => oracle_v4_public(a) && a.to_string() == host,
        Ok(IpAddr::V6(a)) => oracle_v6_public(a),
        Err(_) => false,
    }
}

/// An IPv4 address, half the time inside or just outside one of the private ranges.
fn v4() -> impl Strategy<Value = u32> {
    prop_oneof![
        any::<u32>(),
        (0..PRIVATE_V4.len(), any::<u32>(), 0u8..4).prop_map(|(i, r, edge)| {
            let (net, bits) = PRIVATE_V4[i];
            let host_mask = u32::MAX.checked_shr(bits).unwrap_or(0);
            match edge {
                0 => net.wrapping_sub(1),
                1 => (net | host_mask).wrapping_add(1),
                _ => net | (r & host_mask),
            }
        }),
    ]
}

/// IPv4 `n` in one of the encodings WHATWG accepts.
fn encode_v4(n: u32, style: u8) -> String {
    let [a, b, c, d] = n.to_be_bytes();
    match style % 9 {
        0 => format!("{a}.{b}.{c}.{d}"),
        1 => format!("{n}"),
        2 => format!("0x{n:x}"),
        3 => format!("0{n:o}"),
        4 => format!("0x{a:x}.0x{b:X}.0x{c:x}.0x{d:x}"),
        5 => format!("0{a:o}.0{b:o}.0{c:o}.0{d:o}"),
        6 => format!("{a}.{}", n & 0x00ff_ffff),
        7 => format!("{a}.{b}.{}", n & 0xffff),
        _ => format!("{a}.{b}.{c}.{d}."),
    }
}

fn encode_v6(seg: [u16; 8], style: u8) -> String {
    let full = seg
        .iter()
        .map(|g| format!("{g:x}"))
        .collect::<Vec<_>>()
        .join(":");
    match style % 4 {
        0 => full,
        1 => Ipv6Addr::from(seg).to_string(),
        2 => full.to_uppercase(),
        _ => seg
            .iter()
            .map(|g| format!("{g:04x}"))
            .collect::<Vec<_>>()
            .join(":"),
    }
}

fn v6_seg() -> impl Strategy<Value = [u16; 8]> {
    prop_oneof![
        any::<[u16; 8]>(),
        v4().prop_map(|n| [0, 0, 0, 0, 0, 0xffff, (n >> 16) as u16, n as u16]),
        any::<u32>().prop_map(|n| [0, 0, 0, 0, 0, 0, (n >> 16) as u16, n as u16]),
        (any::<u16>(), any::<[u16; 7]>()).prop_map(|(h, r)| {
            let firsts = [
                0x2001, 0x2002, 0x2000, 0x3fff, 0xfe80, 0xfc00, 0xfd00, 0xff02, 0x0064,
            ];
            let mut s = [0u16; 8];
            s[0] = firsts[h as usize % firsts.len()];
            s[1..].copy_from_slice(&r);
            if h % 3 == 0 {
                s[1] = [0, 0x0db8, 0x0db7, 0x0001][h as usize / 3 % 4];
            }
            s
        }),
    ]
}

fn host() -> impl Strategy<Value = String> {
    prop_oneof![
        (v4(), any::<u8>()).prop_map(|(n, s)| encode_v4(n, s)),
        (v6_seg(), any::<u8>()).prop_map(|(g, s)| format!("[{}]", encode_v6(g, s))),
        v4().prop_map(|n| format!("[::ffff:{}]", Ipv4Addr::from(n))),
        "[a-zA-Z0-9.%_@:\\[\\]\\\\-]{0,24}",
        Just("metadata.google.internal".to_owned()),
        Just("localhost".to_owned()),
    ]
}

fn url() -> impl Strategy<Value = String> {
    (
        prop::sample::select(vec!["http", "https", "HTTP", "ftp", "file", "ws", ""]),
        prop::sample::select(vec!["://", ":/", ":", ":\\\\", ":///", "://\\"]),
        prop::sample::select(vec!["", "u@", "u:p@", "@", "8.8.8.8@", "a@b@", ":p@"]),
        host(),
        prop::sample::select(vec!["", ":80", ":0", ":65536", ":x", ":"]),
        prop::sample::select(vec![
            "",
            "/",
            "/a?b#c",
            "\\@10.0.0.1/",
            "?@127.0.0.1",
            "#@10.0.0.1",
        ]),
    )
        .prop_map(|(s, sep, u, h, p, t)| format!("{s}{sep}{u}{h}{p}{t}"))
}

const REASONS: [&str; 7] = [
    "unparseable",
    "scheme",
    "credentials",
    "private_address",
    "blocked_name",
    "trailing_dot",
    "idn",
];

fn assert_sound(u: &str, mode: Mode, loopback: bool) -> Result<(), TestCaseError> {
    if let Ok(a) = check_url(u, mode, loopback) {
        match a.kind {
            Kind::Ip => {
                let exempt = mode == Mode::Fetch && loopback && a.host == "127.0.0.1";
                prop_assert!(exempt || oracle_public(&a.host), "{u:?} -> {:?}", a.host);
            }
            Kind::Name => {
                prop_assert!(a.host.parse::<IpAddr>().is_err(), "{u:?}");
                prop_assert!(!a.host.is_empty() && !a.host.ends_with('.'));
                prop_assert!(a.host != "metadata.google.internal");
                prop_assert!(
                    a.host.bytes().all(|b| b.is_ascii_graphic()
                        && !b.is_ascii_uppercase()
                        && !b"/\\?#@:[]%<>^|".contains(&b)),
                    "{u:?} -> {:?}",
                    a.host
                );
            }
        }
    }
    Ok(())
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 4096, ..ProptestConfig::default() })]

    #[test]
    fn generated_urls_never_reach_a_private_address(u in url(), loopback in any::<bool>()) {
        assert_sound(&u, Mode::Check, loopback)?;
        assert_sound(&u, Mode::Fetch, loopback)?;
    }

    #[test]
    fn every_ipv4_encoding_denotes_the_same_address(n in v4(), style in 0u8..8) {
        let u = format!("http://{}/", encode_v4(n, style));
        let want = Ipv4Addr::from(n);
        match check_url(&u, Mode::Check, false) {
            Ok(a) => {
                prop_assert_eq!(a.kind, Kind::Ip);
                prop_assert_eq!(&a.host, &want.to_string());
                prop_assert!(oracle_v4_public(want));
            }
            Err(e) => {
                prop_assert_eq!(e.code(), "private_address");
                prop_assert!(!oracle_v4_public(want));
            }
        }
    }

    #[test]
    fn ipv6_literals_agree_with_the_oracle(seg in v6_seg(), style in any::<u8>()) {
        let u = format!("https://[{}]:443/", encode_v6(seg, style));
        let public = oracle_v6_public(Ipv6Addr::from(seg));
        prop_assert_eq!(check_url(&u, Mode::Fetch, true).is_ok(), public, "{}", u);
    }

    #[test]
    fn arbitrary_text_never_panics_and_refuses_with_fixed_codes(s in "\\PC{0,80}", mode in any::<bool>()) {
        let m = if mode { Mode::Fetch } else { Mode::Check };
        if let Err(e) = check_url(&s, m, mode) {
            prop_assert!(REASONS.contains(&e.code()));
        }
        assert_sound(&s, m, mode)?;
    }

    #[test]
    fn json_errors_are_fixed_and_never_echo_input(s in "\\PC{0,200}") {
        let (status, reply) = run_json(&s);
        if status != 0 {
            let fixed = reply == r#"{"error":"input"}"# || reply == r#"{"error":"too_large"}"#;
            prop_assert!(fixed, "unexpected error reply");
        }
        let wrapped = serde_json::json!({"op":"url","url":s,"mode":"check"}).to_string();
        let (status, reply) = run_json(&wrapped);
        prop_assert_eq!(status, 0);
        let v: serde_json::Value = serde_json::from_str(&reply).unwrap();
        if v["ok"] == false {
            prop_assert_eq!(v.as_object().unwrap().len(), 2);
            prop_assert!(REASONS.contains(&v["reason"].as_str().unwrap()));
        }
        let addrs = serde_json::json!({"op":"addresses","addresses":[s]}).to_string();
        let (status, reply) = run_json(&addrs);
        prop_assert_eq!(status, 0);
        let fixed = reply == r#"{"public":true}"# || reply == r#"{"public":false}"#;
        prop_assert!(fixed, "unexpected addresses reply");
    }

    #[test]
    fn arbitrary_bytes_as_json_never_panic(b in prop::collection::vec(any::<u8>(), 0..256)) {
        let s = String::from_utf8_lossy(&b);
        let (status, reply) = run_json(&s);
        prop_assert!(status <= 1);
        prop_assert!(!reply.is_empty());
    }
}
