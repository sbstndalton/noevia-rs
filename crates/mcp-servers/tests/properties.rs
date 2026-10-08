//! Invariants over generated server lists: whatever the port accepts is something the JS's rules
//! accept (http(s), no credentials, internal only on a loopback IP literal and at most once, ids
//! well-formed and unique, the URL taken verbatim from the list), and no input panics.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use mcp_servers::{call, is_loopback_literal, parse_servers, toolbox_offered, Auth};
use prompt_framing::js::units;
use proptest::prelude::*;
use url::Url;

fn piece() -> impl Strategy<Value = String> {
    prop_oneof![
        Just("http://h.example/".to_owned()),
        Just("https://127.0.0.1:9/m".to_owned()),
        Just("http://127.1/".to_owned()),
        Just("http://[::1]/".to_owned()),
        Just("http://localhost/".to_owned()),
        Just("http://u:p@h.example/".to_owned()),
        Just("ftp://h.example/".to_owned()),
        Just("internal".to_owned()),
        Just("nextcloud".to_owned()),
        Just("bearer:TOKEN".to_owned()),
        Just("none".to_owned()),
        "[a-c!]{0,3}",
        "[ -~]{0,12}",
        ".{0,6}",
    ]
}

fn list() -> impl Strategy<Value = String> {
    prop::collection::vec(
        prop::collection::vec(piece(), 0..4).prop_map(|p| p.join("|")),
        0..6,
    )
    .prop_map(|e| e.join(","))
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(2000))]

    #[test]
    fn accepted_servers_keep_the_rules(l in list()) {
        let text = units(&l);
        if let Ok(p) = parse_servers(Some(&text), None) {
            let mut ids = Vec::new();
            let mut internal = 0;
            for s in &p.servers {
                prop_assert!(!s.id.is_empty() && s.id.len() <= 40);
                prop_assert!(!ids.contains(&s.id));
                ids.push(s.id.clone());
                let url = String::from_utf16(&s.url).unwrap();
                prop_assert!(l.contains(&url));
                let u = Url::parse(&url).unwrap();
                prop_assert!(matches!(u.scheme(), "http" | "https"));
                prop_assert!(u.username().is_empty() && u.password().is_none_or(str::is_empty));
                if s.auth == Auth::Internal {
                    internal += 1;
                    prop_assert!(is_loopback_literal(&units(u.host_str().unwrap())));
                }
                prop_assert_eq!(s.auth == Auth::Bearer, s.token_env.is_some());
            }
            prop_assert!(internal <= 1);
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
    fn core_and_dir_are_always_offered(ids in prop::collection::vec("[a-z-]{0,6}", 0..4), id in "[a-z-]{0,8}") {
        let ids: Vec<Vec<u16>> = ids.iter().map(|s| units(s)).collect();
        prop_assert!(toolbox_offered(Some(&ids), &units("core")));
        let dir = units(&format!("dir-{id}"));
        prop_assert!(toolbox_offered(Some(&ids), &dir));
        prop_assert!(toolbox_offered(None, &units(&id)));
    }
}
