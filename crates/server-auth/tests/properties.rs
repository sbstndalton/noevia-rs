//! Property: Rust never accepts a credential Node refuses.
//!
//! Node accepts a request only when a credential in it is exactly one Node accepts: a session
//! cookie whose SHA-256 is a live row, the legacy token byte for byte, or a live `nva_` token
//! (auth.cjs / device-auth.cjs compare digests or bytes, nothing fuzzier). The fixture's named
//! cases list, per scenario, every seeded credential with Node's verdict at the fixture clock.
//! So for arbitrary headers built around those credentials (mutated, re-encoded, padded,
//! duplicated, mixed with noise), a Rust accept must be explained by one of Node's accepted
//! credentials appearing verbatim (after Node's cookie decoding) in the request, and a Rust CSRF
//! pass by the header, the cookie and the session's CSRF token all being equal.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

mod common;

use common::*;
use proptest::prelude::*;
use server_auth::request::{bearer_token, legacy_supplied};
use server_auth::{Authenticator, Credential, Creds};
use std::sync::OnceLock;

struct Scenario {
    built: Built,
    auth: Authenticator,
    sessions: Vec<String>,
    devices: Vec<String>,
    legacy: Option<String>,
}

fn scenarios() -> &'static Vec<Scenario> {
    static S: OnceLock<Vec<Scenario>> = OnceLock::new();
    S.get_or_init(|| {
        let f = fixtures();
        f["scenarios"]
            .as_array()
            .unwrap()
            .iter()
            .map(|sc| {
                // The plain one-credential cases ("cookie sX" = `cowork_session=<token>`, "bearer dX" =
                // `Bearer <token>`) Node accepted at the fixture clock.
                let accepted = |prefix: &str, header: &str, strip: &str| -> Vec<String> {
                    sc["cases"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .filter(|c| {
                            c["name"].as_str().unwrap().starts_with(prefix)
                                && !c["expect"]["authn"].is_null()
                                && c["input"].get("now").is_none()
                        })
                        .filter_map(|c| {
                            c["input"][header]
                                .as_str()?
                                .strip_prefix(strip)
                                .map(str::to_string)
                        })
                        .filter(|t| !t.contains([';', ' ']))
                        .collect()
                };
                let legacy_ok = sc["cases"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|c| c["name"] == "legacy bearer" && !c["expect"]["authn"].is_null());
                Scenario {
                    built: build(sc),
                    auth: Authenticator::new(config(sc)),
                    sessions: accepted("cookie s", "cookie", "cowork_session="),
                    devices: accepted("bearer d", "authorization", "Bearer "),
                    legacy: legacy_ok
                        .then(|| sc["env"]["legacyToken"].as_str().unwrap().to_string()),
                }
            })
            .collect()
    })
}

/// Every seeded token (accepted or not), for building near-misses.
fn seeds() -> Vec<String> {
    let f = fixtures();
    let mut out = Vec::new();
    for c in f["scenarios"][1]["cases"].as_array().unwrap() {
        for k in ["cookie", "authorization", "csrf"] {
            if let Some(v) = c["input"][k].as_str() {
                for part in v.split([';', ' ', '=']) {
                    if part.len() > 20 {
                        out.push(part.to_string());
                    }
                }
            }
        }
    }
    out.sort();
    out.dedup();
    out
}

fn mutated() -> impl Strategy<Value = String> {
    let seeds = seeds();
    (0..seeds.len(), 0usize..64, 0u8..8, any::<char>()).prop_map(move |(i, at, op, ch)| {
        let s = &seeds[i];
        let at = s
            .char_indices()
            .nth(at % (s.chars().count() + 1))
            .map_or(s.len(), |(b, _)| b);
        match op {
            0 | 1 => s.clone(),
            2 => format!("{}{ch}{}", &s[..at], &s[at..]),
            3 => {
                let mut t = s.clone();
                if at < t.len() {
                    t.remove(at);
                }
                t
            }
            4 => s.to_uppercase(),
            5 => s
                .chars()
                .map(|c| {
                    if c == '-' {
                        "%2D".to_string()
                    } else {
                        c.to_string()
                    }
                })
                .collect(),
            6 => format!("{s}{ch}"),
            _ => format!("{ch}{s}"),
        }
    })
}

fn header_piece() -> impl Strategy<Value = String> {
    prop_oneof![
        mutated(),
        Just(String::new()),
        "[ ;=%a-zA-Z0-9_\\-\u{a0}\u{e9}]{0,12}",
    ]
}

fn cookie_header() -> impl Strategy<Value = Option<String>> {
    let name = prop_oneof![
        Just("cowork_session"),
        Just("cowork_csrf"),
        Just(" cowork_session "),
        Just("theme"),
        Just("__proto__")
    ];
    proptest::option::of(
        proptest::collection::vec(
            (
                name,
                header_piece(),
                prop_oneof![Just(";"), Just("; "), Just(";;")],
            ),
            0..4,
        )
        .prop_map(|parts| {
            parts
                .into_iter()
                .map(|(n, v, sep)| format!("{n}={v}{sep}"))
                .collect::<String>()
        }),
    )
}

fn auth_header() -> impl Strategy<Value = Option<String>> {
    proptest::option::of(
        (
            prop_oneof![
                Just("Bearer "),
                Just("bearer\t"),
                Just("Bearer  "),
                Just(""),
                Just("Basic ")
            ],
            header_piece(),
            prop_oneof![Just(""), Just(" "), Just(" x")],
        )
            .prop_map(|(a, b, c)| format!("{a}{b}{c}")),
    )
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 3000, ..ProptestConfig::default() })]

    #[test]
    fn never_accepts_what_node_refuses(
        which in 0usize..5,
        cookie in cookie_header(),
        authorization in auth_header(),
        csrf in proptest::option::of(header_piece()),
        method in prop_oneof![Just("GET"), Just("POST")],
    ) {
        let sc = &scenarios()[which % scenarios().len()];
        let f_now = 1_800_000_000_000i64;
        let creds = Creds { cookie, authorization, origin: None, csrf, method: method.to_string() };
        let got = sc.built.store.read(|r| sc.auth.authenticate_in(r, &creds, f_now)).unwrap();
        if let Some(id) = &got {
            match &id.credential {
                Credential::Session { .. } => {
                    let v = creds.cookie("cowork_session").unwrap_or_default();
                    prop_assert!(sc.sessions.contains(&v), "accepted session {v:?} Node never accepted");
                }
                Credential::Device { .. } => {
                    let t = bearer_token(&creds).to_string();
                    prop_assert!(sc.devices.contains(&t), "accepted device token {t:?} Node never accepted");
                }
                Credential::Legacy => {
                    prop_assert_eq!(Some(legacy_supplied(&creds).to_string()), sc.legacy.clone());
                }
            }
            if sc.auth.csrf_valid(&creds, id) {
                if let Credential::Session { csrf_hash, .. } = &id.credential {
                    let h = creds.csrf.clone().unwrap_or_default();
                    prop_assert!(!h.is_empty());
                    prop_assert_eq!(Some(h.clone()), creds.cookie("cowork_csrf"));
                    prop_assert_eq!(&server_auth::js::digest(&h), csrf_hash);
                }
            }
        }
    }
}

/// The property above is not vacuous: Node's accepted credentials are known per scenario and
/// Rust accepts each of them, so generated requests carrying one verbatim reach the accept arm.
#[test]
fn accepted_sets_are_populated_and_reachable() {
    let all = scenarios();
    let (mut sessions, mut devices, mut legacy) = (0, 0, 0);
    for sc in all {
        for t in &sc.sessions {
            let c = Creds {
                cookie: Some(format!("cowork_session={t}")),
                ..Creds::default()
            };
            assert!(sc
                .built
                .store
                .read(|r| sc.auth.authenticate_in(r, &c, 1_800_000_000_000))
                .unwrap()
                .is_some());
            sessions += 1;
        }
        for t in &sc.devices {
            let c = Creds {
                authorization: Some(format!("Bearer {t}")),
                ..Creds::default()
            };
            assert!(sc
                .built
                .store
                .read(|r| sc.auth.authenticate_in(r, &c, 1_800_000_000_000))
                .unwrap()
                .is_some());
            devices += 1;
        }
        legacy += usize::from(sc.legacy.is_some());
    }
    assert!(
        sessions >= 20 && devices >= 4 && legacy >= 2,
        "{sessions} {devices} {legacy}"
    );
}
