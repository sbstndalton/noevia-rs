//! Commit metadata uses the persisted canonical decimal counter and CPython's
//! finite-float spelling. It does not own schema creation or backup execution.
use super::Fault;
use rusqlite::{params, OptionalExtension, Transaction};

// The pinned CPython 3.12 profile uses its default decimal conversion limit.
// App-managed writes produce canonical nonnegative decimal counters.
const MAX_GENERATION_DIGITS: usize = 4300;
fn next_generation(tx: &Transaction<'_>) -> Result<String, Fault> {
    let stored: Option<Option<String>> = tx.query_row(
        "SELECT CASE WHEN typeof(value)='text' AND length(CAST(value AS BLOB)) BETWEEN 1 AND 4300 THEN value END FROM meta WHERE key='generation'",
        [],|row|row.get(0)).optional()?;
    let old = match stored {
        None => "0".to_owned(),
        Some(Some(value)) => value,
        Some(None) => return Err(Fault::Schema),
    };
    if !old.bytes().all(|c| c.is_ascii_digit()) || (old.len() > 1 && old.starts_with('0')) {
        return Err(Fault::Schema);
    }
    let mut digits = old.into_bytes();
    let mut carry = true;
    for digit in digits.iter_mut().rev() {
        if *digit == b'9' {
            *digit = b'0';
        } else {
            *digit += 1;
            carry = false;
            break;
        }
    }
    if carry {
        digits.insert(0, b'1');
    }
    if digits.len() > MAX_GENERATION_DIGITS {
        return Err(Fault::Schema);
    }
    String::from_utf8(digits).map_err(|_| Fault::Internal)
}
fn timestamp(value: f64) -> Result<String, Fault> {
    if !value.is_finite() {
        return Err(Fault::Invalid);
    }
    // Reuse the already merged pure formatter and its frozen CPython fixtures;
    // this imports no backup worker/operator or filesystem/runtime authority.
    Ok(diary_backup_format::python_float(value))
}
pub(super) fn advance(tx: &Transaction<'_>, now: f64, debounce: f64) -> Result<(), Fault> {
    if !debounce.is_finite() || debounce < 0.0 {
        return Err(Fault::Invalid);
    }
    let generation = next_generation(tx)?;
    let due = timestamp(now + debounce)?;
    let snapshot = timestamp(now)?;
    for (key, value) in [
        ("generation", generation),
        ("due", due),
        ("snapshot_time", snapshot),
    ] {
        tx.execute(
            "INSERT OR REPLACE INTO meta(key,value) VALUES(?,?)",
            params![key, value],
        )?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_fixture::fixture;
    use rusqlite::Connection;
    #[test]
    fn finite_timestamp_spelling_matches_frozen_cpython_bits(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let fixture: serde_json::Value = serde_json::from_str(include_str!(
            "../../diary-backup-format/tests/fixtures/python.json"
        ))?;
        for row in fixture
            .get("floats")
            .and_then(serde_json::Value::as_array)
            .ok_or("floats")?
        {
            let bits = u64::from_str_radix(
                row.get("bits")
                    .and_then(serde_json::Value::as_str)
                    .ok_or("bits")?,
                16,
            )?;
            let value = f64::from_bits(bits);
            if value.is_finite() {
                assert_eq!(
                    timestamp(value)?,
                    row.get("python")
                        .and_then(serde_json::Value::as_str)
                        .ok_or("python")?,
                    "bits {bits:016x}"
                );
            } else {
                assert_eq!(timestamp(value), Err(Fault::Invalid));
            }
        }
        Ok(())
    }
    #[test]
    fn counters_advance_beyond_machine_integer_width() -> Result<(), Box<dyn std::error::Error>> {
        let (_dir, path) = fixture()?;
        let mut db = Connection::open(path)?;
        for (old, expected) in [
            ("0".to_owned(), "1".to_owned()),
            ("9".to_owned(), "10".to_owned()),
            ("9".repeat(100), format!("1{}", "0".repeat(100))),
            (
                format!("8{}", "9".repeat(4299)),
                format!("9{}", "0".repeat(4299)),
            ),
        ] {
            db.execute("UPDATE meta SET value=? WHERE key='generation'", [&old])?;
            let tx = db.transaction()?;
            advance(&tx, 1700000000.0, 3.0)?;
            tx.commit()?;
            assert_eq!(
                db.query_row::<String, _, _>(
                    "SELECT value FROM meta WHERE key='generation'",
                    [],
                    |r| r.get(0)
                )?,
                expected
            );
            assert_eq!(
                db.query_row::<String, _, _>("SELECT value FROM meta WHERE key='due'", [], |r| r
                    .get(0))?,
                "1700000003.0"
            );
            assert_eq!(
                db.query_row::<String, _, _>(
                    "SELECT value FROM meta WHERE key='snapshot_time'",
                    [],
                    |r| r.get(0)
                )?,
                "1700000000.0"
            );
        }
        Ok(())
    }
    #[test]
    fn invalid_counter_or_nonfinite_sum_never_commits_partial_metadata(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let (_dir, path) = fixture()?;
        let mut db = Connection::open(path)?;
        for old in [
            format!("1\0{}", "x".repeat(100_000)),
            "9".repeat(4300),
            "01".into(),
            "-1".into(),
        ] {
            db.execute("UPDATE meta SET value=? WHERE key='generation'", [&old])?;
            let tx = db.transaction()?;
            assert_eq!(advance(&tx, 1.0, 0.0), Err(Fault::Schema));
            tx.rollback()?;
            assert_eq!(
                db.query_row::<String, _, _>(
                    "SELECT value FROM meta WHERE key='generation'",
                    [],
                    |r| r.get(0)
                )?,
                old
            );
        }
        db.execute("UPDATE meta SET value='0' WHERE key='generation'", [])?;
        let tx = db.transaction()?;
        assert_eq!(advance(&tx, f64::MAX, f64::MAX), Err(Fault::Invalid));
        tx.rollback()?;
        assert_eq!(
            db.query_row::<String, _, _>("SELECT value FROM meta WHERE key='due'", [], |r| r
                .get(0))?,
            "0"
        );
        assert_eq!(
            db.query_row::<String, _, _>(
                "SELECT value FROM meta WHERE key='generation'",
                [],
                |r| r.get(0)
            )?,
            "0"
        );
        Ok(())
    }
}
