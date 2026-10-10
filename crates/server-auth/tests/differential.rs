//! Differential fixtures (full-Rust migration M2): every expectation in
//! `fixtures/node-auth.v1.json` was produced by noevia-core's own auth code at
//! contracts/http/core.ref (tools/gen-auth-fixtures.cjs; CI regenerates it and requires the
//! committed copy to match). Rust must give the same verdict for every case; a Rust accept of a
//! request Node refuses is reported separately as the unsafe direction. Synthetic data only.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

mod common;

use common::*;
use serde_json::Value;
use server_auth::identity::{browser_only_path, Refusal};
use server_auth::request::{bearer_token, has_session_cookie, parse_cookies};
use server_auth::{Authenticator, Creds};

fn refusal_name(r: Result<server_auth::Identity, Refusal>) -> &'static str {
    match r {
        Ok(_) => "allow",
        Err(Refusal::Unauthorized) => "unauthorized",
        Err(Refusal::Csrf) => "csrf",
        Err(Refusal::BrowserOnly) => "browser_only",
    }
}

#[test]
fn request_verdicts_agree_with_node() {
    let f = fixtures();
    let mut mismatches = Vec::new();
    let mut unsafe_accepts = Vec::new();
    let (mut total, mut accepted) = (0usize, 0usize);
    for sc in f["scenarios"].as_array().unwrap() {
        let built = build(sc);
        let auth = Authenticator::new(config(sc));
        let name = sc["name"].as_str().unwrap();
        let named = sc["cases"].as_array().unwrap().iter().map(|c| {
            (
                c["name"].as_str().unwrap().to_string(),
                &c["input"],
                &c["expect"],
                true,
            )
        });
        let fuzz = sc["fuzz"]
            .as_array()
            .unwrap()
            .iter()
            .enumerate()
            .map(|(i, c)| (format!("fuzz #{i}"), &c["input"], &c["expect"], false));
        for (case, input, expect, gates) in named.chain(fuzz) {
            total += 1;
            let now = now(&f, input);
            let creds = creds(input, "GET");
            let got = built
                .store
                .read(|r| auth.authenticate_in(r, &creds, now))
                .unwrap();
            let got_v = verdict(got.as_ref());
            if !expect["authn"].is_null() {
                accepted += 1;
            }
            if got_v != expect["authn"] {
                if expect["authn"].is_null() {
                    unsafe_accepts.push(format!("{name} / {case}: Rust {got_v}"));
                }
                mismatches.push(format!(
                    "{name} / {case}: authn Rust {got_v} Node {}",
                    expect["authn"]
                ));
                continue;
            }
            if !gates {
                continue;
            }
            let csrf = got.as_ref().is_some_and(|id| auth.csrf_valid(&creds, id));
            if expect["csrf"] != csrf {
                if csrf {
                    unsafe_accepts.push(format!("{name} / {case}: csrf"));
                }
                mismatches.push(format!("{name} / {case}: csrf Rust {csrf}"));
            }
            let origin = built.store.read(|r| auth.origin_valid(r, &creds)).unwrap();
            if expect["origin"] != origin {
                if origin {
                    unsafe_accepts.push(format!("{name} / {case}: origin"));
                }
                mismatches.push(format!("{name} / {case}: origin Rust {origin}"));
            }
            for (i, (method, path)) in GATE_TARGETS.iter().enumerate() {
                let c = Creds {
                    method: (*method).to_string(),
                    ..creds.clone()
                };
                let g = refusal_name(auth.gate(&built.store, &c, path, now).unwrap());
                let want = expect["gates"][i].as_str().unwrap();
                if g != want {
                    if g == "allow" {
                        unsafe_accepts.push(format!("{name} / {case}: gate {method} {path}"));
                    }
                    mismatches.push(format!(
                        "{name} / {case}: gate {method} {path} Rust {g} Node {want}"
                    ));
                }
            }
        }
    }
    assert!(total >= 7000, "fixture shrank to {total} cases");
    assert!(
        accepted >= 500,
        "too few accepted cases ({accepted}) to mean anything"
    );
    assert!(
        unsafe_accepts.is_empty(),
        "Rust accepts what Node refuses:\n{}",
        unsafe_accepts.join("\n")
    );
    assert!(
        mismatches.is_empty(),
        "{} of {total} differ:\n{}",
        mismatches.len(),
        mismatches.join("\n")
    );
}

#[test]
fn app_password_verdicts_agree_with_node() {
    let f = fixtures();
    let mut bad = Vec::new();
    let mut ok = 0;
    for sc in f["scenarios"].as_array().unwrap() {
        let built = build(sc);
        for d in sc["dav"].as_array().unwrap() {
            let (u, p, s) = (
                d["username"].as_str().unwrap(),
                d["password"].as_str().unwrap(),
                d["scope"].as_str().unwrap(),
            );
            let got = server_auth::app_passwords::verify_dav(&built.store, u, p, s).unwrap();
            let got_v = got.as_ref().map_or(Value::Null, |g| serde_json::json!({"userId": g.user_id, "credentialId": g.credential_id, "scope": g.scope}));
            if got.is_some() {
                ok += 1;
            }
            if got_v != d["expect"] {
                bad.push(format!(
                    "{} / {u} {s}: Rust {got_v} Node {}",
                    sc["name"], d["expect"]
                ));
            }
        }
    }
    assert!(ok >= 8, "only {ok} accepted app passwords");
    assert!(bad.is_empty(), "{}", bad.join("\n"));
}

#[test]
fn parse_cookies_agrees_with_node() {
    let f = fixtures();
    let cases = f["parseCookies"].as_array().unwrap();
    assert!(cases.len() >= 2000);
    let mut bad = Vec::new();
    for c in cases {
        let header = c["header"].as_str().unwrap();
        let got = serde_json::to_value(parse_cookies(header)).unwrap();
        if got != c["expect"] {
            bad.push(format!("{header:?}: Rust {got} Node {}", c["expect"]));
        }
    }
    assert!(bad.is_empty(), "{} differ:\n{}", bad.len(), bad.join("\n"));
}

#[test]
fn pure_helpers_agree_with_node() {
    let f = fixtures();
    for c in f["bearerToken"].as_array().unwrap() {
        let a = c["authorization"].as_str().unwrap();
        let creds = Creds {
            authorization: Some(a.to_string()),
            ..Creds::default()
        };
        assert_eq!(bearer_token(&creds), c["expect"].as_str().unwrap(), "{a:?}");
    }
    for c in f["hasSessionCookie"].as_array().unwrap() {
        let k = c["cookie"].as_str().unwrap();
        let creds = Creds {
            cookie: Some(k.to_string()),
            ..Creds::default()
        };
        assert_eq!(
            has_session_cookie(&creds),
            c["expect"].as_bool().unwrap(),
            "{k:?}"
        );
    }
    for c in f["browserOnly"].as_array().unwrap() {
        let (p, m) = (c["path"].as_str().unwrap(), c["method"].as_str().unwrap());
        assert_eq!(
            browser_only_path(p, m),
            c["expect"].as_bool().unwrap(),
            "{m} {p}"
        );
    }
    for c in f["features"].as_array().unwrap() {
        let env = c["env"].as_str();
        let parsed = server_auth::config::parse_feature_env(env);
        let cfg = |native| server_auth::AuthConfig {
            public_origin: String::new(),
            additional_origins: vec![],
            legacy_token: String::new(),
            legacy_compat: false,
            trust_proxy: c["trustProxy"].as_str() == Some("true"),
            native_client_auth_env: native,
        };
        match (&c["expect"], parsed) {
            (Value::String(e), Err(_)) if e == "error" => {}
            (Value::Bool(want), Ok(native)) => {
                assert_eq!(
                    cfg(native).native_client_auth(c["stored"].as_str()),
                    *want,
                    "{c}"
                );
            }
            (want, got) => panic!("{c}: Node {want}, Rust {got:?}"),
        }
    }
}

#[test]
fn schema_is_the_one_node_creates() {
    let f = fixtures();
    let sc = &f["scenarios"][0];
    let migrations: Vec<i64> = sc["db"]["migrations"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_i64().unwrap())
        .collect();
    assert_eq!(
        migrations.iter().max().copied(),
        Some(server_store::KNOWN_SCHEMA_VERSION),
        "Node's migrations moved: review server-store"
    );
    let node_cols = sc["db"]["columns"].as_object().unwrap();
    for (table, cols) in server_store::REQUIRED_COLUMNS {
        let have: Vec<&str> = node_cols[*table]
            .as_array()
            .unwrap_or_else(|| panic!("Node has no {table}"))
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        for c in *cols {
            assert!(have.contains(c), "Node's {table} has no {c}");
        }
    }
    // server-store's hand-written test schema has exactly Node's columns for every table it lists.
    let sql = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../server-store/tests/fixtures/node-schema.sql"
    ))
    .unwrap();
    let conn = server_store::rusqlite::Connection::open_in_memory().unwrap();
    conn.execute_batch(&sql).unwrap();
    let mut tables = conn
        .prepare("SELECT name FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%'")
        .unwrap();
    let names: Vec<String> = tables
        .query_map([], |r| r.get(0))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    for t in names {
        let mut sorted_ours: Vec<String> = conn
            .prepare(&format!("SELECT name FROM pragma_table_info('{t}')"))
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .map(Result::unwrap)
            .collect();
        let mut sorted_node: Vec<String> = node_cols[&t]
            .as_array()
            .unwrap_or_else(|| panic!("Node has no {t}"))
            .iter()
            .map(|v| v.as_str().unwrap().to_string())
            .collect();
        sorted_ours.sort();
        sorted_node.sort();
        assert_eq!(sorted_ours, sorted_node, "{t}");
    }
}

#[test]
fn json_stringify_agrees_with_node() {
    let f = fixtures();
    for c in f["jsonStringify"].as_array().unwrap() {
        use server_store::json::{stringify, Format};
        assert_eq!(
            stringify(&c["value"], Format::Pretty).unwrap(),
            c["pretty"].as_str().unwrap(),
            "{}",
            c["value"]
        );
        assert_eq!(
            stringify(&c["value"], Format::Compact).unwrap(),
            c["compact"].as_str().unwrap(),
            "{}",
            c["value"]
        );
    }
}
