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
    let (mut total, mut accepted, mut ambiguous_refusals) = (0usize, 0usize, 0usize);
    for sc in f["scenarios"].as_array().unwrap() {
        let built = build(sc);
        let auth = Authenticator::new(config(sc));
        let name = sc["name"].as_str().unwrap();
        // Node may hold either the setup address or PUBLIC_ORIGIN here (review F1); Rust refuses a
        // request whose Origin is PUBLIC_ORIGIN that this one Node instance accepts. Only that one
        // difference, in the strict direction, is allowed.
        let ambiguous = sc["originAmbiguous"].as_bool().unwrap();
        let p_origin = sc["env"]["publicOrigin"].as_str().unwrap();
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
            let strict_ok = ambiguous
                && !origin
                && expect["origin"] == true
                && creds.origin.as_deref() == Some(p_origin);
            if strict_ok {
                ambiguous_refusals += 1;
            } else if expect["origin"] != origin {
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
                if strict_ok && g == "csrf" && want == "allow" {
                    continue;
                }
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
        ambiguous_refusals > 0,
        "no case exercises the ambiguous-origin refusal"
    );
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

/// Rust's verdict on every origin of one setup scenario, as `(origin, rust, live, restarted)`.
fn setup_verdicts(sc: &Value, origin_from_settings: bool) -> Vec<(String, bool, bool, bool)> {
    let mut db_sc = sc.clone();
    db_sc["db"]["rows"]
        .as_object_mut()
        .unwrap()
        .entry("settings")
        .or_insert(Value::Array(vec![]));
    let built = build(&db_sc);
    let mut cfg_sc = sc.clone();
    cfg_sc["nativeClientAuth"] = Value::Bool(false);
    let mut cfg = config(&cfg_sc);
    cfg.origin_from_settings = origin_from_settings;
    let auth = Authenticator::new(cfg);
    sc["cases"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| {
            let origin = c["origin"].as_str().unwrap();
            let creds = Creds {
                origin: (!origin.is_empty()).then(|| origin.to_string()),
                method: "POST".to_string(),
                ..Creds::default()
            };
            (
                origin.to_string(),
                built.store.read(|r| auth.origin_valid(r, &creds)).unwrap(),
                c["live"].as_bool().unwrap(),
                c["restarted"].as_bool().unwrap(),
            )
        })
        .collect()
}

/// First-run setup chose an address (S) other than PUBLIC_ORIGIN (P). Since noevia#1254 setup also
/// stores public_origin_admin = S, so Node's live and restarted origin agree (S) and Rust, which
/// reads the admin row, must accept exactly what Node accepts.
#[test]
fn setup_origin_matches_node_now_that_setup_stores_the_admin_row() {
    let f = fixtures();
    let sc = &f["setupOrigin"];
    assert_eq!(sc["liveOrigin"], sc["restartedOrigin"]);
    assert_eq!(sc["liveOrigin"], "https://setup.example.test");
    let rows = sc["db"]["rows"]["settings"].as_array().unwrap();
    let setting = |k: &str| rows.iter().find(|r| r["key"] == k).map(|r| r["value"].clone());
    assert_eq!(
        setting("public_origin_admin"),
        Some(Value::from("https://setup.example.test"))
    );
    let mut accepted = 0;
    for (origin, got, live, restarted) in setup_verdicts(sc, false) {
        assert_eq!(live, restarted, "origin {origin:?}: Node states diverge");
        assert_eq!(got, live, "origin {origin:?}: Rust {got}, Node {live}");
        accepted += usize::from(got);
    }
    assert!(accepted >= 3, "scenario accepts almost nothing ({accepted})");
}

/// Review F1, legacy state: a database set up before noevia#1254 has settings.public_origin = S, no
/// public_origin_admin row, and PUBLIC_ORIGIN = P. Node accepts S until it restarts and P after;
/// Rust must accept neither where either Node refuses, and (both being ambiguous to it) exactly
/// what both accept.
#[test]
fn legacy_setup_origin_never_wider_than_either_node() {
    let f = fixtures();
    let sc = &f["legacySetupOrigin"];
    assert_ne!(
        sc["liveOrigin"], sc["restartedOrigin"],
        "scenario lost its point"
    );
    let rows = sc["db"]["rows"]["settings"].as_array().unwrap();
    assert!(
        rows.iter().all(|r| r["key"] != "public_origin_admin"),
        "legacy scenario must have no public_origin_admin row"
    );
    let (mut live_only, mut restarted_only) = (0, 0);
    for (origin, got, live, restarted) in setup_verdicts(sc, false) {
        live_only += usize::from(live && !restarted);
        restarted_only += usize::from(restarted && !live);
        assert_eq!(
            got,
            live && restarted,
            "origin {origin:?}: Rust {got}, Node live {live}, restarted {restarted}"
        );
    }
    assert_eq!((live_only, restarted_only), (1, 1));
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
            origin_from_settings: false,
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

/// M3: the writes Node's `requestAuth.authenticate` makes as a side effect (a live session's
/// `last_seen_at`, a rejected session's DELETE, a device grant's `last_used_at`), which Rust's
/// upkeep takes over while it owns those tables. Every named and fuzz case, every scenario.
#[test]
fn upkeep_writes_agree_with_node() {
    use server_store::rusqlite::Connection;
    use std::collections::BTreeMap;
    type Snap = (BTreeMap<String, i64>, BTreeMap<String, i64>);
    fn snap(c: &Connection) -> Snap {
        let read = |sql: &str| -> BTreeMap<String, i64> {
            let mut st = c.prepare(sql).unwrap();
            st.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))
                .unwrap()
                .map(Result::unwrap)
                .collect()
        };
        (
            read("SELECT id_hash, last_seen_at FROM sessions"),
            read("SELECT id, last_used_at FROM device_grants"),
        )
    }
    fn diff(
        a: &BTreeMap<String, i64>,
        b: &BTreeMap<String, i64>,
    ) -> serde_json::Map<String, Value> {
        let mut out = serde_json::Map::new();
        for k in a.keys().chain(b.keys()) {
            if a.get(k) != b.get(k) {
                out.insert(k.clone(), b.get(k).map_or(Value::Null, |v| Value::from(*v)));
            }
        }
        out
    }
    let f = fixtures();
    let switch = server_store::RustAuth::from_env_value(Some("1")).unwrap();
    let (mut total, mut wrote, mut bad) = (0usize, 0usize, Vec::new());
    for sc in f["scenarios"].as_array().unwrap() {
        let built = build(sc);
        let auth = Authenticator::new(config(sc));
        let writer = server_store::Writer::open(built.dir.path(), switch).unwrap();
        let node = Connection::open(server_store::db_path(built.dir.path())).unwrap();
        let base = snap(&node);
        let name = sc["name"].as_str().unwrap();
        let cases = sc["cases"]
            .as_array()
            .unwrap()
            .iter()
            .chain(sc["fuzz"].as_array().unwrap().iter());
        for (i, c) in cases.enumerate() {
            let input = &c["input"];
            let want = c["expect"]
                .get("writes")
                .expect("fixture predates M3: regenerate it with tools/gen-auth-fixtures.cjs");
            total += 1;
            auth.upkeep(&writer, &creds(input, "GET"), now(&f, input))
                .unwrap();
            let after = snap(&node);
            let mut got = serde_json::Map::new();
            let s = diff(&base.0, &after.0);
            let g = diff(&base.1, &after.1);
            if !s.is_empty() {
                got.insert("sessions".into(), Value::Object(s));
            }
            if !g.is_empty() {
                got.insert("grants".into(), Value::Object(g));
            }
            let got = Value::Object(got);
            if got != Value::Object(serde_json::Map::new()) {
                wrote += 1;
            }
            if &got != want {
                bad.push(format!("{name} #{i} {input}: Rust {got} Node {want}"));
            }
            // Back to the scenario's rows for the next case.
            node.execute_batch("DELETE FROM sessions").unwrap();
            for (table, rows) in sc["db"]["rows"].as_object().unwrap() {
                if table != "sessions" && table != "device_grants" {
                    continue;
                }
                for row in rows.as_array().unwrap() {
                    let o = row.as_object().unwrap();
                    if table == "sessions" {
                        node.execute(
                            "INSERT INTO sessions VALUES(?1,?2,?3,?4,?5,?6,?7,?8)",
                            (
                                o["id_hash"].as_str(),
                                o["user_id"].as_str(),
                                o["csrf_hash"].as_str(),
                                o["created_at"].as_i64(),
                                o["last_seen_at"].as_i64(),
                                o["expires_at"].as_i64(),
                                o["user_agent"].as_str(),
                                o["ip"].as_str(),
                            ),
                        )
                        .unwrap();
                    } else {
                        node.execute(
                            "UPDATE device_grants SET last_used_at=?1 WHERE id=?2",
                            (o["last_used_at"].as_i64(), o["id"].as_str()),
                        )
                        .unwrap();
                    }
                }
            }
            assert_eq!(snap(&node), base);
        }
    }
    assert!(total >= 7000, "fixture shrank to {total} cases");
    assert!(wrote >= 500, "only {wrote} cases wrote anything");
    assert!(
        bad.is_empty(),
        "{} of {total} differ:\n{}",
        bad.len(),
        bad.join("\n")
    );
}

/// M3: with NOEVIA_RUST_AUTH on, Node re-reads the address on every use (core auth.cjs
/// refreshOrigin), which is exactly what a restarted Node does; Rust then agrees with that Node
/// on every origin, with no ambiguous case left. Holds for the legacy state (no admin row) and for
/// the current one.
#[test]
fn with_rust_auth_the_origin_is_the_restart_rule() {
    let f = fixtures();
    for key in ["legacySetupOrigin", "setupOrigin"] {
        for (origin, got, _, restarted) in setup_verdicts(&f[key], true) {
            assert_eq!(got, restarted, "{key}: origin {origin:?}");
        }
    }
}
