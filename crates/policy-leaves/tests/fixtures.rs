//! The shared fixture table (`tests/fixtures/policy-leaves.v1.json`, byte-identical to
//! noevia-core's, printed by its `tools/gen-policy-leaves-fixtures.cjs` from auth-tokens.cjs and
//! tool-policy.cjs themselves).
//!
//! auth and set rows must agree exactly. mode rows must agree exactly wherever the JS answers a
//! mode for a stored mode it knows; for a stored string outside the table's CHECK (which the JS
//! hands back, or answers `ask` for a write) the port must say `block`.
//! Every row also runs through the wire.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]

use policy_leaves::{auth_call, auth_tokens, check_set, mode, policy_call, Mode, SetRefusal};
use serde_json::Value;

fn fixtures() -> Value {
    serde_json::from_str(include_str!("fixtures/policy-leaves.v1.json")).unwrap()
}

fn units(v: &Value) -> Vec<u16> {
    v.as_array()
        .unwrap()
        .iter()
        .map(|u| u.as_u64().unwrap() as u16)
        .collect()
}

fn wire_string(out: &mut Vec<u8>, s: Option<&[u16]>) {
    match s {
        None => out.push(0),
        Some(s) => {
            out.push(1);
            out.extend((s.len() as u32).to_le_bytes());
            for u in s {
                out.extend(u.to_le_bytes());
            }
        }
    }
}

/// The reply's JSON string as UTF-16 units (serde_json would refuse lone surrogates).
fn reply_units(reply: &str, key: &str) -> Vec<u16> {
    let start = reply.find(&format!("\"{key}\":\"")).unwrap() + key.len() + 4;
    let mut out = Vec::new();
    let b = reply.as_bytes();
    let mut i = start;
    loop {
        match b[i] {
            b'"' => return out,
            b'\\' => {
                match b[i + 1] {
                    b'u' => {
                        out.push(u16::from_str_radix(&reply[i + 2..i + 6], 16).unwrap());
                        i += 6;
                        continue;
                    }
                    c => out.push(u16::from(c)),
                }
                i += 2;
            }
            c => {
                out.push(u16::from(c));
                i += 1;
            }
        }
    }
}

#[test]
fn auth_rows_match_the_js() {
    let f = fixtures();
    let rows = f["auth"].as_array().unwrap();
    assert!(rows.len() > 400);
    for row in rows {
        let env = &row["env"];
        let get = |k: &str| env.get(k).map(units);
        let diary = get("DIARY_AUTH_TOKEN").unwrap_or_default();
        let ui = get("UI_AUTH_TOKEN").unwrap_or_default();
        let compat = get("LEGACY_AUTH_COMPAT");
        let got = auth_tokens(&diary, &ui, compat.as_deref());
        let want = &row["want"];
        assert_eq!(got.diary_token, units(&want["diaryToken"]), "{row}");
        assert_eq!(got.ui_auth_token, units(&want["uiAuthToken"]), "{row}");
        assert_eq!(got.legacy_compat, want["legacyCompat"].as_bool().unwrap());
        let ws: Vec<&str> = want["warnings"]
            .as_array()
            .unwrap()
            .iter()
            .map(|w| w.as_str().unwrap())
            .collect();
        assert_eq!(got.warnings, ws);

        let mut input = Vec::new();
        wire_string(&mut input, Some(&diary));
        wire_string(&mut input, Some(&ui));
        wire_string(&mut input, compat.as_deref());
        let (status, reply) = auth_call(&input);
        assert_eq!(status, 0);
        assert!(reply.is_ascii());
        assert_eq!(reply_units(&reply, "diaryToken"), got.diary_token);
        assert_eq!(reply_units(&reply, "uiAuthToken"), got.ui_auth_token);
    }
}

#[test]
fn mode_rows_never_weaker_than_the_js() {
    let f = fixtures();
    let rows = f["mode"].as_array().unwrap();
    let (mut exact, mut stronger) = (0, 0);
    for row in rows {
        let stored = (!row["stored"].is_null()).then(|| units(&row["stored"]));
        let w = row["isWrite"].as_bool().unwrap();
        let got = mode(stored.as_deref(), w);
        let want = row["want"].as_str().unwrap();
        let known = stored
            .as_deref()
            .is_none_or(|s| s.is_empty() || Mode::parse(s).is_some());
        if known {
            let m = Mode::parse(&want.encode_utf16().collect::<Vec<_>>()).unwrap();
            assert_eq!(got, m, "{row}");
            exact += 1;
        } else {
            // A stored string outside the CHECK: the JS answers it back (or ask for a write);
            // the port blocks, which is never weaker.
            assert_eq!(got, Mode::Block, "{row}");
            if let Some(m) = Mode::parse(&want.encode_utf16().collect::<Vec<_>>()) {
                assert!(got >= m);
            }
            stronger += 1;
        }
        let mut input = vec![1];
        wire_string(&mut input, stored.as_deref());
        input.push(u8::from(w));
        assert_eq!(
            policy_call(&input),
            (0, format!("{{\"mode\":\"{}\"}}", got.name()))
        );
    }
    assert!(exact == 12 && stronger == 18, "{exact} {stronger}");
}

#[test]
fn set_rows_match_the_js() {
    let f = fixtures();
    for row in f["set"].as_array().unwrap() {
        let value = (!row["value"].is_null()).then(|| units(&row["value"]));
        let writes: Vec<bool> = row["writes"]
            .as_array()
            .unwrap()
            .iter()
            .map(|w| w.as_bool().unwrap())
            .collect();
        let got = check_set(value.as_deref(), &writes);
        let want = row["want"].as_str().unwrap();
        match got {
            Ok(_) => assert_eq!(want, "ok", "{row}"),
            Err(r) => assert_eq!(r.message(), want, "{row}"),
        }
        let mut input = vec![2];
        wire_string(&mut input, value.as_deref());
        input.extend((writes.len() as u32).to_le_bytes());
        input.extend(writes.iter().map(|w| u8::from(*w)));
        let (status, reply) = policy_call(&input);
        assert_eq!(status, 0);
        match got {
            Ok(m) => assert_eq!(reply, format!("{{\"ok\":true,\"mode\":\"{}\"}}", m.name())),
            Err(r) => assert_eq!(
                reply,
                format!("{{\"ok\":false,\"reason\":\"{}\"}}", r.code())
            ),
        }
    }
    // Writes are never allow.
    assert_eq!(
        check_set(
            Some(&"allow".encode_utf16().collect::<Vec<_>>()),
            &[false, true]
        ),
        Err(SetRefusal::WriteAllow)
    );
}
