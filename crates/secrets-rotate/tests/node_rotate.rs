#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]
//! The secrets rotation replayed on recorded Node output (`fixtures/node-rotate.v1.json`, written by
//! `record_rotate.cjs` from core's own route, runRotation and key store over synthetic keys): the reply
//! byte for byte, the audit detail, and what each stored value holds afterwards (unchanged, or opening
//! under the current key alone to the same plaintext, bound as Node binds it). The front's three passes
//! are the recorded stand-ins; the front's route test runs the real ones.
use js_json::JValue;
use secret_envelope::KeyUsed;
use secrets_rotate::{
    audit_detail, drive_check, drive_front_error, merge, rotate, Counts, DriveFront, Failure,
    Keys, Pass, Row, Status, Table, DRIVE_TABLE,
};
use serde_json::Value;
use zeroize::Zeroizing;

const FIXTURE: &str = include_str!("fixtures/node-rotate.v1.json");

fn unhex(s: &str) -> Vec<u8> {
    s.as_bytes()
        .chunks(2)
        .map(|p| u8::from_str_radix(std::str::from_utf8(p).unwrap(), 16).unwrap())
        .collect()
}

struct TestKeys {
    current: Vec<u8>,
    previous: Option<Vec<u8>>,
    nonce: std::cell::Cell<u8>,
}

impl Keys for TestKeys {
    fn open(&self, value: &str, user: Option<&str>) -> Result<(Zeroizing<String>, KeyUsed), ()> {
        let units: Vec<u16> = value.encode_utf16().collect();
        let (used, plain) = secret_envelope::open(
            &self.current,
            self.previous.as_deref(),
            &units,
            user.map(str::as_bytes),
        )
        .map_err(|_| ())?;
        if used == KeyUsed::None {
            return Ok((Zeroizing::new(value.to_string()), used));
        }
        Ok((Zeroizing::new(String::from_utf8_lossy(&plain).into_owned()), used))
    }
    fn seal(&self, plain: &str, user: Option<&str>) -> Result<String, ()> {
        if plain.is_empty() {
            return Ok(String::new());
        }
        self.nonce.set(self.nonce.get().wrapping_add(1));
        let nonce = [self.nonce.get(); 12];
        secret_envelope::seal(&self.current, &nonce, plain.as_bytes(), user.map(str::as_bytes))
            .map_err(|_| ())
    }
}

/// A SQL table as the front reads it: rows in insertion order, compare-and-swap writes; `conflict`
/// changes that account's row between the read and its write (another writer meanwhile).
struct SqlTable {
    name: &'static str,
    present: bool,
    rows: Vec<(String, String)>,
    conflict: Option<String>,
}

impl Table for SqlTable {
    fn name(&self) -> &str {
        self.name
    }
    fn rows(&mut self) -> Result<Vec<Row>, String> {
        if !self.present {
            return Ok(Vec::new());
        }
        Ok(self
            .rows
            .iter()
            .filter(|(_, v)| !v.is_empty())
            .map(|(u, v)| Row {
                reference: JValue::Arr(vec![JValue::Str(u.clone())]),
                value: v.clone(),
                user: Some(u.clone()),
            })
            .collect())
    }
    fn write(&mut self, row: &Row, value: &str) -> Result<bool, String> {
        let user = row.user.clone().unwrap();
        if self.conflict.as_deref() == Some(user.as_str()) {
            for r in &mut self.rows {
                if r.0 == user {
                    r.1 = "qa-changed-meanwhile".into();
                }
            }
        }
        for r in &mut self.rows {
            if r.0 == user && r.1 == row.value {
                r.1 = value.to_string();
                return Ok(true);
            }
        }
        Ok(false)
    }
}

struct DriveTable {
    files: Option<Vec<(String, String)>>,
    sealer: google_token_seal::Sealer,
}

impl Table for DriveTable {
    fn name(&self) -> &str {
        DRIVE_TABLE
    }
    fn rows(&mut self) -> Result<Vec<Row>, String> {
        let Some(files) = &self.files else {
            return Ok(Vec::new());
        };
        // `.filter((f) => f.endsWith('.sealed')).sort()`: UTF-16 code unit order.
        let mut names: Vec<&String> = files.iter().map(|(n, _)| n).filter(|n| n.ends_with(".sealed")).collect();
        names.sort_by(|a, b| a.encode_utf16().cmp(b.encode_utf16()));
        Ok(names
            .into_iter()
            .map(|n| Row {
                reference: JValue::Str(n.clone()),
                value: n.clone(),
                user: None,
            })
            .collect())
    }
    fn write(&mut self, _row: &Row, _value: &str) -> Result<bool, String> {
        panic!("the Drive files are never written by the rotation")
    }
    fn check(&mut self, row: &Row) -> Option<Result<Status, String>> {
        let text = &self.files.as_ref().unwrap().iter().find(|(n, _)| *n == row.value).unwrap().1;
        Some(drive_check(text, |t| self.sealer.open(t).map_err(|_| ())))
    }
}

fn counts(v: &Value) -> Counts {
    let n = |k: &str| v[k].as_u64().unwrap();
    Counts {
        current: n("current"),
        rotated: n("rotated"),
        upgraded: n("upgraded"),
        empty: n("empty"),
        failed: n("failed"),
    }
}

/// The front pass stand-in as Node's client returns it (read with JS key order).
fn pass(v: &JValue) -> Option<Pass> {
    if v.as_str() == Some("throws") {
        return None;
    }
    let num = |c: &JValue, k: &str| c.get(k).as_num().unwrap() as u64;
    let JValue::Obj(tables) = v.get("tables") else { panic!("tables") };
    let JValue::Arr(failures) = v.get("failures") else { panic!("failures") };
    Some(
        Pass {
            tables: tables
                .iter()
                .map(|(k, c)| {
                    (k.clone(), Counts { current: num(c, "current"), rotated: num(c, "rotated"), upgraded: num(c, "upgraded"), empty: num(c, "empty"), failed: num(c, "failed") })
                })
                .collect(),
            failures: failures
                .iter()
                .map(|f| Failure {
                    table: f.get("table").as_str().unwrap().to_string(),
                    reference: f.get("ref").clone(),
                    error: f.get("error").as_str().unwrap().to_string(),
                })
                .collect(),
        }
        .normalised(),
    )
}

#[test]
fn recorded_node_rotations() {
    let fixture: Value = serde_json::from_str(FIXTURE).unwrap();
    let keys = &fixture["keys"];
    let current = unhex(keys["current"].as_str().unwrap());
    let previous_key = unhex(keys["previous"].as_str().unwrap());
    let cases = fixture["cases"].as_array().unwrap();
    assert!(cases.len() >= 20, "corpus shrank to {}", cases.len());
    let ordered = js_json::wtf16::parse(FIXTURE).unwrap();
    let JValue::Arr(jcases) = ordered.get("cases") else { panic!("cases") };
    for (case, jcase) in cases.iter().zip(jcases) {
        let jfront = jcase.get("input").get("front");
        let name = case["name"].as_str().unwrap();
        let input = &case["input"];
        let expected = &case["expected"];
        if input["role"] == "member" {
            assert_eq!(expected["status"], 403, "{name}");
            continue;
        }
        let previous = match input["previous"].as_str().unwrap() {
            "file" | "env-hex" | "env-base64" => Some(previous_key.clone()),
            _ => None,
        };
        let (Some(providers), Some(mcp)) = (pass(jfront.get("providers")), pass(jfront.get("mcp"))) else {
            // A front pass that cannot run stops the rotation before anything is written.
            assert_eq!(expected["status"], 500, "{name}");
            assert!(expected["audits"].as_array().unwrap().is_empty(), "{name}");
            for table in ["storage_connections", "chatgpt_oauth_tokens"] {
                assert!(expected["after"][table].as_array().unwrap().iter().all(|r| r["same"] == true), "{name}");
            }
            continue;
        };
        let k = TestKeys { current: current.clone(), previous: previous.clone(), nonce: Default::default() };
        let rows_of = |v: &Value| -> Vec<(String, String)> {
            v.as_array().map_or_else(Vec::new, |rows| {
                rows.iter()
                    .map(|r| (r["user"].as_str().unwrap().to_string(), r["value"].as_str().unwrap().to_string()))
                    .collect()
            })
        };
        let conflict = |table: &str| {
            (input["conflict"]["table"] == table).then(|| input["conflict"]["user"].as_str().unwrap().to_string())
        };
        let mut storage = SqlTable { name: "storage_connections", present: true, rows: rows_of(&input["storage"]), conflict: conflict("storage_connections") };
        let mut chatgpt = SqlTable { name: "chatgpt_oauth_tokens", present: !input["chatgpt"].is_null(), rows: rows_of(&input["chatgpt"]), conflict: conflict("chatgpt_oauth_tokens") };
        let derived = google_token_seal::derive_secret_key(&current, google_token_seal::USER_LABEL);
        let mut drive = DriveTable {
            files: input["drive"].as_array().map(|files| {
                files.iter().map(|f| (f["name"].as_str().unwrap().to_string(), f["text"].as_str().unwrap().to_string())).collect()
            }),
            sealer: google_token_seal::Sealer::new(derived.as_slice()).unwrap(),
        };
        let report = rotate(&k, &mut [&mut storage, &mut chatgpt, &mut drive]);
        let front = match &input["front"]["gdrive"] {
            Value::String(s) if s == "throws" => DriveFront::Error(drive_front_error(None)),
            v if v["ok"] == true => DriveFront::Counts(Counts {
                current: v["counts"]["current"].as_u64().unwrap(),
                rotated: v["counts"]["rotated"].as_u64().unwrap(),
                upgraded: 0,
                empty: v["counts"]["empty"].as_u64().unwrap(),
                failed: v["counts"]["failed"].as_u64().unwrap(),
            }),
            v => DriveFront::Error(drive_front_error(v["status"].as_u64().map(|s| u16::try_from(s).unwrap()))),
        };
        let report = merge(report, &providers, &mcp, &front, previous.is_some());
        assert_eq!(expected["status"], 200, "{name}");
        assert_eq!(report.json(), expected["reply"].as_str().unwrap(), "{name}: reply");
        let audits = expected["audits"].as_array().unwrap();
        assert_eq!(audits.len(), 1, "{name}");
        assert_eq!(audits[0]["action"], secrets_rotate::AUDIT_ACTION, "{name}");
        assert_eq!(audits[0]["detail"].as_str().unwrap(), audit_detail(&report), "{name}: audit");
        // The stored values afterwards.
        for (table, rows) in [("storage_connections", &storage.rows), ("chatgpt_oauth_tokens", &chatgpt.rows)] {
            let original = rows_of(&input[if table == "storage_connections" { "storage" } else { "chatgpt" }]);
            for (i, after) in expected["after"][table].as_array().unwrap().iter().enumerate() {
                let (user, now) = &rows[i];
                assert_eq!(after["user"].as_str().unwrap(), user, "{name}");
                if after["same"] == true {
                    assert_eq!(now, &original[i].1, "{name}: {table} {user} unchanged");
                    continue;
                }
                if let Some(raw) = after["raw"].as_str() {
                    assert_eq!(now, raw, "{name}");
                    continue;
                }
                assert_eq!(&now.chars().take(7).collect::<String>(), after["version"].as_str().unwrap(), "{name}");
                let only_current = TestKeys { current: current.clone(), previous: None, nonce: Default::default() };
                let (plain, used) = only_current.open(now, Some(user).filter(|u| !u.is_empty()).map(String::as_str)).unwrap();
                let opened = &after["opened"];
                let want_used = match opened["keyUsed"].as_str().unwrap() {
                    "current" => KeyUsed::Current,
                    "none" => KeyUsed::None,
                    other => panic!("{other}"),
                };
                assert_eq!(used, want_used, "{name}: {table} {user}");
                if used == KeyUsed::Current {
                    assert_eq!(plain.as_str(), opened["plain"].as_str().unwrap(), "{name}: {table} {user}");
                } else {
                    assert_eq!(now, "", "{name}: {table} {user}");
                }
            }
        }
        assert_eq!(expected["unchangedDrive"], true, "{name}");
    }
}

#[test]
fn recorder_sources_are_recorded() {
    let fixture: Value = serde_json::from_str(FIXTURE).unwrap();
    assert_eq!(fixture["wasmSha256"], "2305d64d1a75a57b3c52eda673b384c819718aae540c570837b4aeb4ef5e5cb9");
    for file in ["server/routes/auth.cjs", "server/secrets-rotate.cjs", "server/secrets.cjs", "server/secret-envelope.cjs", "server/gdrive-seal.cjs"] {
        assert_eq!(fixture["sources"][file].as_str().unwrap().len(), 64, "{file}");
    }
}
