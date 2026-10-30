//! core storage-latch.cjs (sbstndalton/noevia#2191 round 8): the hold on an account's AUTOMATIC
//! storage calls after its saved login is refused (401) or storage throttles it (429), so a project's
//! source refresh and the Diary storage backup do not keep feeding Nextcloud's brute-force protection
//! (#1429).
//!
//! The latch used to be Node's memory, shared by two Node callers. Now the project source refresh is the
//! front's, so the latch is a durable file both processes use: `UI_DATA_DIR/storage-latch.db`, one row
//! per account, every transition a `BEGIN IMMEDIATE` read-modify-write (core `storage-latch.cjs` does
//! the same transitions on the same rows). Neither side keeps a copy of a row in memory.
//!
//! - [`note`]: a failure of a call made with the connection with the given [`fingerprint`]; only a
//!   login (401) and a throttle (429) latch. A refused login holds for 15 minutes, doubling after
//!   each refused probe up to 6 hours; a throttle holds for its `Retry-After` (60 s without one), and
//!   a throttle that lands on a refused login keeps that login's strikes (#1607).
//! - [`blocked`]: the hold that stops this automatic call, or none. A login hold whose wait has passed
//!   lets this one call through as a probe and holds the others for a minute (#1589).
//! - [`clear`]: a call that worked proves the credentials. So does a proof the front wrote after the hold
//!   was set (`UI_DATA_DIR/storage-login-proofs.json`, an explicit folder write or connection test with
//!   the saved login): [`blocked`] and [`note`] drop a hold older than the proof.
//! - A latch for other credentials (another fingerprint) is dropped.
use crate::source_storage::Failure;
use js_json::JValue;
use rusqlite::{params, Connection, OptionalExtension};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

pub const FILE: &str = "storage-latch.db";
const PROOFS_FILE: &str = "storage-login-proofs.json";
const DEFAULT_THROTTLE_WAIT_MS: i64 = 60 * 1000;
const LOGIN_PROBE_BASE_MS: i64 = 15 * 60 * 1000;
const LOGIN_PROBE_MAX_MS: i64 = 6 * 60 * 60 * 1000;
const PROBE_HOLD_MS: i64 = 60 * 1000;

/// How long an automatic call waits when the latch cannot be read (it counts as held; see
/// [`crate::sync::automatic_hold`]).
pub const UNREADABLE_HOLD_SECONDS: u64 = 60;

/// What holds an account's automatic calls.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Hold {
    /// `login` or `throttled`.
    pub kind: &'static str,
    /// Seconds a throttle has left.
    pub retry_after: Option<u64>,
}

/// storage-latch.cjs `fingerprintOf(connection)`: SHA-256 of `JSON.stringify([kind, baseUrl, bucket,
/// username, secret])`, so a change of credentials clears a hold on its own. The secret itself is
/// never stored.
pub fn fingerprint(
    kind: &str,
    base_url: &str,
    bucket: &str,
    username: &str,
    secret: &str,
) -> String {
    let mut text = String::from("[");
    for (i, part) in [kind, base_url, bucket, username, secret]
        .iter()
        .enumerate()
    {
        if i > 0 {
            text.push(',');
        }
        js_json::quote(part, &mut text);
    }
    text.push(']');
    Sha256::digest(text.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

#[derive(Debug)]
pub enum Error {
    Db(rusqlite::Error),
}
impl From<rusqlite::Error> for Error {
    fn from(e: rusqlite::Error) -> Self {
        Error::Db(e)
    }
}

#[derive(Clone)]
struct Row {
    fingerprint: String,
    kind: String,
    until: i64,
    strikes: i64,
    probing: bool,
    since: i64,
    /// What a throttle over a refused login restores: (strikes, until).
    login: Option<(i64, i64)>,
}

fn open(data_dir: &Path) -> Result<Connection, Error> {
    let conn = Connection::open(data_dir.join(FILE))?;
    conn.busy_timeout(std::time::Duration::from_secs(5))?;
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS latch (
           user_id TEXT PRIMARY KEY, fingerprint TEXT NOT NULL, kind TEXT NOT NULL,
           until INTEGER NOT NULL, strikes INTEGER NOT NULL, probing INTEGER NOT NULL,
           since INTEGER NOT NULL, login_strikes INTEGER, login_until INTEGER)",
    )?;
    Ok(conn)
}

fn read(conn: &Connection, user: &str) -> Result<Option<Row>, Error> {
    Ok(conn
        .query_row(
            "SELECT fingerprint, kind, until, strikes, probing, since, login_strikes, login_until
             FROM latch WHERE user_id = ?1",
            params![user],
            |r| {
                let login = match (r.get::<_, Option<i64>>(6)?, r.get::<_, Option<i64>>(7)?) {
                    (Some(s), Some(u)) => Some((s, u)),
                    _ => None,
                };
                Ok(Row {
                    fingerprint: r.get(0)?,
                    kind: r.get(1)?,
                    until: r.get(2)?,
                    strikes: r.get(3)?,
                    probing: r.get::<_, i64>(4)? != 0,
                    since: r.get(5)?,
                    login,
                })
            },
        )
        .optional()?)
}

fn write(conn: &Connection, user: &str, row: &Row) -> Result<(), Error> {
    conn.execute(
        "INSERT OR REPLACE INTO latch
         (user_id, fingerprint, kind, until, strikes, probing, since, login_strikes, login_until)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        params![
            user,
            row.fingerprint,
            row.kind,
            row.until,
            row.strikes,
            i64::from(row.probing),
            row.since,
            row.login.map(|l| l.0),
            row.login.map(|l| l.1)
        ],
    )?;
    Ok(())
}

fn delete(conn: &Connection, user: &str) -> Result<(), Error> {
    conn.execute("DELETE FROM latch WHERE user_id = ?1", params![user])?;
    Ok(())
}

/// The proof the front wrote for `user` (epoch ms), if any.
fn proof_of(data_dir: &Path, user: &str) -> Option<i64> {
    let text = std::fs::read_to_string(data_dir.join(PROOFS_FILE)).ok()?;
    let doc = js_json::wtf16::parse(&text).ok()?;
    let at = doc.get("proofs").get(user).as_num()?;
    (at.is_finite()).then_some(at as i64)
}

/// `settle(userId)`: a hold a later proof ends, a throttle that has run its course dropped (or giving way
/// to the refused login it sat on, with its strikes).
fn settle(conn: &Connection, data_dir: &Path, user: &str, now: i64) -> Result<Option<Row>, Error> {
    let Some(mut row) = read(conn, user)? else {
        return Ok(None);
    };
    if proof_of(data_dir, user).is_some_and(|at| at >= row.since) {
        delete(conn, user)?;
        return Ok(None);
    }
    if row.kind == "throttled" && now >= row.until {
        match row.login {
            Some((strikes, until)) => {
                row = Row {
                    fingerprint: row.fingerprint.clone(),
                    kind: "login".into(),
                    until,
                    strikes,
                    probing: until <= now,
                    since: row.since,
                    login: None,
                };
                write(conn, user, &row)?;
            }
            None => {
                delete(conn, user)?;
                return Ok(None);
            }
        }
    }
    Ok(Some(row))
}

fn transaction<T>(
    data_dir: &Path,
    f: impl FnOnce(&Connection) -> Result<T, Error>,
) -> Result<T, Error> {
    let conn = open(data_dir)?;
    conn.execute_batch("BEGIN IMMEDIATE")?;
    match f(&conn) {
        Ok(v) => {
            conn.execute_batch("COMMIT")?;
            Ok(v)
        }
        Err(e) => {
            let _ = conn.execute_batch("ROLLBACK");
            Err(e)
        }
    }
}

/// `note(userId, connection, failure)`.
pub fn note(
    data_dir: &Path,
    user: &str,
    fingerprint: &str,
    failure: Failure,
    now: i64,
) -> Result<(), Error> {
    if user.is_empty() || !matches!(failure.kind, "login" | "throttled") {
        return Ok(());
    }
    transaction(data_dir, |conn| {
        let before = settle(conn, data_dir, user, now)?;
        let same = before.as_ref().filter(|b| b.fingerprint == fingerprint);
        if failure.kind == "login" {
            let same_login = same.filter(|b| b.kind == "login");
            let same_throttle = same.filter(|b| b.kind == "throttled" && b.login.is_some());
            // A probe that was refused again doubles the wait; an explicit press that was refused only
            // renews it.
            let strikes = match (same_login, same_throttle) {
                (Some(b), _) => b.strikes + i64::from(b.probing),
                (None, Some(b)) => b.login.map_or(1, |l| l.0),
                _ => 1,
            };
            let wait = LOGIN_PROBE_MAX_MS
                .min(LOGIN_PROBE_BASE_MS.saturating_mul(1i64 << (strikes - 1).clamp(0, 16)));
            write(
                conn,
                user,
                &Row {
                    fingerprint: fingerprint.to_owned(),
                    kind: "login".into(),
                    until: now + wait,
                    strikes,
                    probing: false,
                    since: now,
                    login: None,
                },
            )?;
        } else {
            let wait = failure
                .retry_after
                .filter(|n| *n > 0)
                .map_or(DEFAULT_THROTTLE_WAIT_MS, |n| {
                    (n as i64).saturating_mul(1000)
                });
            // The login strikes survive a throttle on the same credentials. A probe was past its base
            // wait, so the login is due again the moment the throttle ends.
            let login = match same {
                Some(b) if b.kind == "login" => {
                    Some((b.strikes, if b.probing { 0 } else { b.until }))
                }
                Some(b) if b.kind == "throttled" => b.login,
                _ => None,
            };
            write(
                conn,
                user,
                &Row {
                    fingerprint: fingerprint.to_owned(),
                    kind: "throttled".into(),
                    until: now + wait,
                    strikes: 0,
                    probing: false,
                    since: now,
                    login,
                },
            )?;
        }
        Ok(())
    })
}

/// `blocked(userId, connection)`: the hold that stops this automatic call, or none.
pub fn blocked(
    data_dir: &Path,
    user: &str,
    fingerprint: &str,
    now: i64,
) -> Result<Option<Hold>, Error> {
    transaction(data_dir, |conn| {
        let Some(mut row) = settle(conn, data_dir, user, now)? else {
            return Ok(None);
        };
        if row.fingerprint != fingerprint {
            delete(conn, user)?;
            return Ok(None);
        }
        if row.kind == "login" && now >= row.until {
            row.probing = true;
            row.until = now + PROBE_HOLD_MS;
            write(conn, user, &row)?;
            return Ok(None);
        }
        Ok(Some(Hold {
            kind: if row.kind == "login" {
                "login"
            } else {
                "throttled"
            },
            retry_after: (row.kind == "throttled")
                .then(|| u64::try_from(((row.until - now + 999) / 1000).max(1)).unwrap_or(1)),
        }))
    })
}

/// `clear(userId)`: a call that worked.
pub fn clear(data_dir: &Path, user: &str) -> Result<(), Error> {
    transaction(data_dir, |conn| delete(conn, user))
}

/// The file's path in the data directory.
pub fn path(data_dir: &Path) -> PathBuf {
    data_dir.join(FILE)
}

/// For tests of both sides: a row as a JSON value (`None` when the account has no latch).
pub fn dump(data_dir: &Path, user: &str) -> Result<Option<JValue>, Error> {
    let conn = open(data_dir)?;
    Ok(read(&conn, user)?.map(|r| {
        JValue::obj([
            ("kind", JValue::Str(r.kind)),
            ("until", JValue::Num(r.until as f64)),
            ("strikes", JValue::Num(r.strikes as f64)),
            ("probing", JValue::Bool(r.probing)),
            ("since", JValue::Num(r.since as f64)),
        ])
    }))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing, clippy::panic)]
mod tests {
    use super::*;

    const LOGIN: Failure = Failure {
        kind: "login",
        retry_after: None,
    };
    const MIN: i64 = 60_000;

    fn fixture() -> (tempfile::TempDir, String) {
        (
            tempfile::tempdir().unwrap(),
            fingerprint("nextcloud", "https://cloud.example/dav", "", "ada", "pw"),
        )
    }

    fn held(d: &Path, fp: &str, now: i64) -> Option<(&'static str, Option<u64>)> {
        blocked(d, "u1", fp, now)
            .unwrap()
            .map(|h| (h.kind, h.retry_after))
    }

    #[test]
    fn the_fingerprint_is_the_digest_of_the_json_array() {
        // JSON.stringify(["a","b","","c","d"]) hashed by Node's createHash('sha256').
        let expected: String = Sha256::digest(br#"["a","b","","c","d"]"#)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        assert_eq!(fingerprint("a", "b", "", "c", "d"), expected);
        assert_ne!(
            fingerprint("a", "b", "", "c", "d"),
            fingerprint("a", "b", "", "c", "e")
        );
    }

    #[test]
    fn a_refused_login_holds_until_its_credentials_change() {
        let (d, fp) = fixture();
        note(d.path(), "u1", &fp, LOGIN, 0).unwrap();
        assert_eq!(held(d.path(), &fp, 1), Some(("login", None)));
        assert_eq!(
            blocked(d.path(), "u2", &fp, 1).unwrap(),
            None,
            "another account is not held"
        );
        let other = fingerprint("nextcloud", "https://cloud.example/dav", "", "ada", "new");
        assert_eq!(
            held(d.path(), &other, 2),
            None,
            "a changed password clears it"
        );
        assert_eq!(
            held(d.path(), &fp, 3),
            None,
            "and the old latch is gone for good"
        );
    }

    #[test]
    fn a_throttle_holds_for_its_retry_after_with_a_sixty_second_default() {
        let (d, fp) = fixture();
        let throttled = |retry_after| Failure {
            kind: "throttled",
            retry_after,
        };
        note(d.path(), "u1", &fp, throttled(Some(30)), 0).unwrap();
        assert_eq!(held(d.path(), &fp, 29_000), Some(("throttled", Some(1))));
        assert_eq!(held(d.path(), &fp, 30_000), None);
        note(d.path(), "u1", &fp, throttled(None), 30_000).unwrap();
        assert!(held(d.path(), &fp, 89_000).is_some());
        assert_eq!(held(d.path(), &fp, 90_000), None);
    }

    #[test]
    fn only_a_login_or_a_throttle_latches_and_a_working_call_clears() {
        let (d, fp) = fixture();
        for kind in ["timeout", "unreachable", "forbidden"] {
            note(
                d.path(),
                "u1",
                &fp,
                Failure {
                    kind,
                    retry_after: None,
                },
                0,
            )
            .unwrap();
        }
        assert_eq!(held(d.path(), &fp, 1), None);
        note(d.path(), "u1", &fp, LOGIN, 0).unwrap();
        clear(d.path(), "u1").unwrap();
        assert_eq!(held(d.path(), &fp, 1), None);
    }

    #[test]
    fn a_refused_login_waits_fifteen_minutes_then_lets_one_probe_through_and_doubles() {
        let (d, fp) = fixture();
        note(d.path(), "u1", &fp, LOGIN, 0).unwrap();
        assert!(held(d.path(), &fp, 14 * MIN).is_some());
        // The wait has passed: this call is the probe; the others are held for a minute.
        assert_eq!(held(d.path(), &fp, 15 * MIN), None);
        assert!(held(d.path(), &fp, 15 * MIN + 30_000).is_some());
        // The probe was refused again: twice as long.
        note(d.path(), "u1", &fp, LOGIN, 15 * MIN + 1).unwrap();
        assert!(held(d.path(), &fp, 15 * MIN + 29 * MIN).is_some());
        assert_eq!(held(d.path(), &fp, 15 * MIN + 1 + 30 * MIN), None);
        // It is capped at six hours however often the probes fail.
        let mut at = 100 * MIN;
        for _ in 0..12 {
            assert_eq!(held(d.path(), &fp, at + 7 * 60 * MIN), None);
            note(d.path(), "u1", &fp, LOGIN, at + 7 * 60 * MIN).unwrap();
            at += 7 * 60 * MIN;
        }
        let row = dump(d.path(), "u1").unwrap().unwrap();
        assert_eq!(row.get("until").as_num(), Some((at + 360 * MIN) as f64));
    }

    #[test]
    fn a_throttle_on_a_refused_login_gives_the_login_back_with_its_strikes() {
        let (d, fp) = fixture();
        note(d.path(), "u1", &fp, LOGIN, 0).unwrap();
        assert_eq!(held(d.path(), &fp, 15 * MIN), None, "the probe is out");
        // The probe drew a 429: the throttle runs its course, then the login is due again at once.
        note(
            d.path(),
            "u1",
            &fp,
            Failure {
                kind: "throttled",
                retry_after: Some(120),
            },
            15 * MIN + 1,
        )
        .unwrap();
        assert_eq!(
            held(d.path(), &fp, 15 * MIN + 60_000).map(|h| h.0),
            Some("throttled")
        );
        assert_eq!(
            held(d.path(), &fp, 15 * MIN + 125_000),
            None,
            "the login probe is let through"
        );
        // A refusal right after the throttle is the refused probe: the wait is 30 min, not 15.
        note(d.path(), "u1", &fp, LOGIN, 15 * MIN + 126_000).unwrap();
        assert!(held(d.path(), &fp, 15 * MIN + 126_000 + 29 * MIN).is_some());
    }

    #[test]
    fn a_later_proof_ends_a_hold_and_an_earlier_one_does_not() {
        let (d, fp) = fixture();
        note(d.path(), "u1", &fp, LOGIN, 100).unwrap();
        std::fs::write(d.path().join(PROOFS_FILE), r#"{"proofs":{"u1":50}}"#).unwrap();
        assert!(
            held(d.path(), &fp, 200).is_some(),
            "a proof from before the hold proves nothing"
        );
        std::fs::write(d.path().join(PROOFS_FILE), r#"{"proofs":{"u1":100}}"#).unwrap();
        assert_eq!(
            held(d.path(), &fp, 200),
            None,
            "a proof at or after it ends the hold"
        );
        assert_eq!(dump(d.path(), "u1").unwrap(), None);
    }

    /// The fingerprint is how a change of credentials clears a hold, and both processes must agree on it:
    /// core's `fingerprintOf` (storage-latch.cjs) is `sha256(JSON.stringify([kind, baseUrl, bucket,
    /// username, secret]))`. These digests were produced by core's own function for these rows (core's
    /// storage-latch-shared-2191.test.cjs asserts the same table), so a drift of either side shows in
    /// whichever test runs against the changed one. JSON.stringify's escapes are exercised: quotes and
    /// backslashes, control characters, DEL, U+2028, non-BMP text and empty fields.
    #[test]
    fn the_fingerprint_is_the_one_cores_function_gives_the_same_row() {
        let rows: [([&str; 5], &str); 4] = [
            (
                [
                    "nextcloud",
                    "https://cloud.example/remote.php/dav/files/ada",
                    "",
                    "ada",
                    "synthetic-app-pw",
                ],
                "376c824306ea55b08290b7b2521e766fe39afff0757b891649d850c05bbdd256",
            ),
            (
                ["s3", "https://s3.example", "bkt", "AKIASYNTH", "a\"b\\c"],
                "209700f4ed745b523d6a56309e9648f12b6de049d726e0538f7751872c35fe12",
            ),
            (
                [
                    "webdav",
                    "https://dav.example/\u{e9}",
                    "",
                    "\u{65e5}\u{672c}\u{8a9e}",
                    "\u{1f600} line\nbreak\ttab \u{1} \u{7f} \u{2028} </script>",
                ],
                "7170827e27c9ba8d1dcc86c74f8e8e744a9b53722a41d56a75cf2e1410f7dde8",
            ),
            (
                ["", "", "", "", ""],
                "6cfebf804e787d7f6f42ab4ec170a33635fc041b4fc37ad1e2f3c4a2b5bc8ece",
            ),
        ];
        for ([kind, base, bucket, user, secret], expected) in rows {
            assert_eq!(fingerprint(kind, base, bucket, user, secret), expected);
        }
    }
}
