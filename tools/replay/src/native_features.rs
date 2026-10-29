//! Required native feature runtime proof. The paired front records one incarnation epoch in
//! cowork.db when Node's feature runtime registers with it; Node never writes that row (core
//! server/rust-auth.cjs refuses it), so a Node-direct reference cannot have it. Only the exactly
//! validated row leaves the differential comparison COPY; the full snapshot keeps it.
use crate::{at, state};
use serde_json::{json, Value};
use std::path::Path;

pub const KEY: &str = "native:feature-runtime:incarnation";

fn epoch(s: &str) -> bool {
    s.len() == 32
        && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
        && s.bytes().any(|b| b != b'0')
}

/// Reads the raw (unmasked) row: exactly one, TEXT, a non-zero 32-hex epoch.
pub fn validate(dir: &Path) -> Result<String, String> {
    let rows = state::sqlite_json(
        &dir.join("cowork.db"),
        &format!("SELECT typeof(value) AS t, value FROM settings WHERE key='{KEY}'"),
    )?;
    let rows = rows.as_array().ok_or("feature runtime row query")?;
    let [row] = rows.as_slice() else {
        return Err(format!(
            "native feature runtime registration absent/duplicated ({} rows)",
            rows.len()
        ));
    };
    match (at(row, "t").as_str(), at(row, "value").as_str()) {
        (Some("text"), Some(v)) if epoch(v) => Ok(v.to_string()),
        _ => Err("native feature runtime incarnation is not a non-zero 32-hex epoch".into()),
    }
}

pub fn evidence(incarnation: &str) -> Value {
    json!({ "settingsKey": KEY, "incarnation": incarnation })
}

/// Removes exactly the validated row (and the evidence) from a comparison copy.
pub fn project(snapshot: &Value) -> Result<Value, String> {
    let mut projected = snapshot.clone();
    if let Some(object) = projected.as_object_mut() {
        object.remove("nativeFeatureRuntimeProof");
    }
    let settings = projected
        .get_mut("db")
        .and_then(|db| db.get_mut("settings"))
        .and_then(Value::as_array_mut)
        .ok_or("missing settings snapshot")?;
    let before = settings.len();
    settings.retain(|row| at(row, "key").as_str() != Some(KEY));
    if before - settings.len() != 1 {
        return Err("proven feature runtime row absent/duplicated in snapshot".into());
    }
    Ok(projected)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use std::process::Command;

    fn db(rows: &str) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let sql = format!("CREATE TABLE settings(key TEXT PRIMARY KEY, value TEXT); {rows}");
        let status = Command::new("sqlite3")
            .arg(dir.path().join("cowork.db"))
            .arg(sql)
            .status()
            .unwrap();
        assert!(status.success());
        dir
    }

    #[test]
    fn exactly_one_valid_epoch_is_proven_and_only_it_is_projected() {
        let dir = db(&format!(
            "INSERT INTO settings VALUES('{KEY}','0123456789abcdef0123456789abcdef'); INSERT INTO settings VALUES('public_origin','x');"
        ));
        assert_eq!(
            validate(dir.path()).unwrap(),
            "0123456789abcdef0123456789abcdef"
        );
        let snap = json!({
            "db": {"settings": [{"key": KEY, "value": "<opaque>"}, {"key": "public_origin", "value": "x"}]},
            "nativeFeatureRuntimeProof": evidence("0123456789abcdef0123456789abcdef"),
            "files": []
        });
        assert_eq!(
            project(&snap).unwrap(),
            json!({"db": {"settings": [{"key": "public_origin", "value": "x"}]}, "files": []})
        );
    }

    #[test]
    fn absent_zero_short_uppercase_and_non_text_rows_fail() {
        for rows in [
            String::new(),
            format!("INSERT INTO settings VALUES('{KEY}','{}');", "0".repeat(32)),
            format!("INSERT INTO settings VALUES('{KEY}','0123456789abcdef');"),
            format!("INSERT INTO settings VALUES('{KEY}','0123456789ABCDEF0123456789ABCDEF');"),
            format!("INSERT INTO settings VALUES('{KEY}',X'30313233343536373839616263646566');"),
        ] {
            assert!(validate(db(&rows).path()).is_err(), "{rows}");
        }
    }

    #[test]
    fn projection_requires_the_row_once() {
        let missing = json!({"db": {"settings": [{"key": "public_origin"}]}});
        assert!(project(&missing).is_err());
        let twice = json!({"db": {"settings": [{"key": KEY}, {"key": KEY}]}});
        assert!(project(&twice).is_err());
        assert!(project(&json!({"files": []})).is_err());
    }
}
