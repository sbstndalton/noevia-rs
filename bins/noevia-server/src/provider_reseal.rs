//! Re-seals the provider keys in the files this front alone writes (sbstndalton/noevia#2190
//! round 6 review, M1): `POST /v1/providers/reseal { scope }` on the internal store, called by
//! core's secrets rotation (core secrets-rotate.cjs, rust-providers-reseal.cjs) in place of the
//! row-position rewrite Node used to make from outside the front's locks. Capability
//! `rust-providers-reseal`.
//!
//! One pass per file, inside the file's own lock ([`provider_rows::update_user`] and
//! [`provider_rows::update_shared`]: `providers.json.lock`, `shared-providers.json.lock`), with the
//! current and the previous key, as core `secrets.reseal(value)` for an unbound table:
//!
//! - a key sealed `enc:v1` under the current key is `current` and is left as it is;
//! - one sealed under the previous key is `rotated`, a plaintext one `upgraded`: both are sealed
//!   `enc:v1` under the current key;
//! - one that no available key opens, an `enc:v2` one (bound to an account, which a provider key
//!   never is) or one that is not text is `failed`: reported with its position, left exactly as it
//!   was (it may still be recovered with the right key), and never fatal to the pass;
//! - an empty or missing key is not a row of the pass.
//!
//! Because the pass reads and writes under the lock, a row another request of the front adds,
//! edits or deletes cannot shift under it: a delete before the pass is not seen, one after it is
//! applied to the re-sealed file. (The Node rotation wrote each key back by its position in a list
//! it had read earlier, so a delete in between put one row's key on another.)
//!
//! The answer is `{ ok: true, tables, failures }`: `tables` maps `shared_providers` and
//! `user_providers:<id>` to `{ current, rotated, upgraded, empty, failed }`; `failures` lists
//! `{ table, ref, error }` (`ref` the row's position, or null for a file that could not be read).
//! No key, ciphertext or plaintext is ever in it. Without usable credential keys the answer is
//! 503 `credential store unavailable` and nothing is touched.

use js_json::JValue;
use serde_json::{json, Value};
use server_auth::secrets::SecretKeys;
use server_projects::provider_rows;
use server_projects::store::{self, StoreError};
use std::path::Path;

/// Which files a pass covers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Scope {
    /// `shared-providers.json` only.
    Shared,
    /// Every account's `providers.json` (`Some(id)`: that account's alone).
    User(Option<String>),
    /// The shared file and every account's.
    All,
}

impl Scope {
    /// The request's `scope` (`user`, `shared`, `all`) and optional `user`.
    pub fn parse(scope: Option<&str>, user: Option<&str>) -> Result<Scope, &'static str> {
        match (scope, user) {
            (Some("shared"), None) => Ok(Scope::Shared),
            (Some("user"), user) => Ok(Scope::User(user.map(str::to_owned))),
            (Some("all"), None) => Ok(Scope::All),
            (Some("shared" | "all"), Some(_)) => Err("user is only for scope user"),
            _ => Err("scope must be user, shared or all"),
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Counts {
    pub current: usize,
    pub rotated: usize,
    pub upgraded: usize,
    pub empty: usize,
    pub failed: usize,
}

impl Counts {
    fn json(self) -> Value {
        json!({
            "current": self.current, "rotated": self.rotated, "upgraded": self.upgraded,
            "empty": self.empty, "failed": self.failed,
        })
    }
}

#[derive(Debug, Default)]
pub struct Report {
    pub tables: Vec<(String, Counts)>,
    /// `(table, row position, why)`.
    pub failures: Vec<(String, Option<usize>, &'static str)>,
}

impl Report {
    pub fn json(&self) -> Value {
        let tables: serde_json::Map<String, Value> = self
            .tables
            .iter()
            .map(|(name, counts)| (name.clone(), counts.json()))
            .collect();
        let failures: Vec<Value> = self
            .failures
            .iter()
            .map(|(table, at, why)| json!({ "table": table, "ref": at, "error": why }))
            .collect();
        json!({ "ok": true, "tables": tables, "failures": failures })
    }
}

const NO_KEY_OPENS: &str = "no available key opens it";
const BOUND: &str = "credential is bound to an unknown account";
const NOT_TEXT: &str = "the key is not text";
const DAMAGED: &str = "the file is damaged; nothing was changed";
const BUSY: &str = "the file is busy; run the rotation again";
const UNWRITABLE: &str = "the file could not be read or written; nothing was changed";

/// One file's rows, re-sealed in place: the counts, the failures (row position and why) and
/// whether any row changed.
fn pass(
    rows: &mut [JValue],
    keys: &SecretKeys,
    nonce: &mut impl FnMut() -> [u8; 12],
) -> (Counts, Vec<(usize, &'static str)>, bool) {
    let mut counts = Counts::default();
    let mut failures = Vec::new();
    let mut changed = false;
    for (at, row) in rows.iter_mut().enumerate() {
        let JValue::Obj(items) = row else { continue };
        let Some(slot) = items
            .iter_mut()
            .find(|(k, _)| k == "apiKey")
            .map(|(_, v)| v)
        else {
            continue;
        };
        // The rotation's own filter: `.filter((r) => r.value)`.
        if !slot.truthy() {
            continue;
        }
        let JValue::Str(stored) = &*slot else {
            counts.failed += 1;
            failures.push((at, NOT_TEXT));
            continue;
        };
        // secret-envelope `version_of`: 2 for `enc:v2:`, 1 for `enc:v1:`, else 0.
        let version = match stored.as_bytes() {
            [b'e', b'n', b'c', b':', b'v', b'2', b':', ..] => 2,
            [b'e', b'n', b'c', b':', b'v', b'1', b':', ..] => 1,
            _ => 0,
        };
        if version == 2 {
            counts.failed += 1;
            failures.push((at, BOUND));
            continue;
        }
        let Ok((plain, used)) = keys.open(stored, None) else {
            counts.failed += 1;
            failures.push((at, NO_KEY_OPENS));
            continue;
        };
        if plain.is_empty() {
            counts.empty += 1;
            continue;
        }
        // `used.tag()`: 0 not an envelope (plaintext), 1 the current key, 2 the previous one.
        if used.tag() == 1 && version == 1 {
            counts.current += 1;
            continue;
        }
        match keys.seal(&plain, None, &nonce()) {
            Ok(sealed) if !sealed.is_empty() => {
                *slot = JValue::Str(sealed);
                changed = true;
                if used.tag() == 2 {
                    counts.rotated += 1;
                } else {
                    counts.upgraded += 1;
                }
            }
            _ => {
                counts.failed += 1;
                failures.push((at, NO_KEY_OPENS));
            }
        }
    }
    (counts, failures, changed)
}

fn failed_table(report: &mut Report, name: &str, e: &StoreError) {
    let why = match e {
        StoreError::Corrupt => DAMAGED,
        StoreError::Busy => BUSY,
        _ => UNWRITABLE,
    };
    report.tables.push((
        name.to_owned(),
        Counts {
            failed: 1,
            ..Counts::default()
        },
    ));
    report.failures.push((name.to_owned(), None, why));
}

fn add(report: &mut Report, name: String, outcome: (Counts, Vec<(usize, &'static str)>)) {
    let (counts, failures) = outcome;
    report.failures.extend(
        failures
            .into_iter()
            .map(|(at, why)| (name.clone(), Some(at), why)),
    );
    report.tables.push((name, counts));
}

/// The ids of the accounts whose directory holds a `providers.json`, in order.
fn accounts_with_rows(data_dir: &Path, only: Option<&str>) -> Vec<String> {
    let mut ids: Vec<String> = match only {
        Some(id) => vec![id.to_owned()],
        None => std::fs::read_dir(data_dir.join("users"))
            .map(|entries| {
                entries
                    .filter_map(Result::ok)
                    .filter_map(|e| e.file_name().into_string().ok())
                    .collect()
            })
            .unwrap_or_default(),
    };
    ids.sort();
    ids.retain(|id| {
        store::user_dir(data_dir, id).is_ok_and(|dir| dir.join(provider_rows::FILE).exists())
    });
    ids
}

/// One pass over the files `scope` names, each under its lock.
pub fn run(
    data_dir: &Path,
    scope: &Scope,
    keys: &SecretKeys,
    mut nonce: impl FnMut() -> [u8; 12],
) -> Report {
    let mut report = Report::default();
    let switch = server_store::RustProjects::owned();
    if matches!(scope, Scope::Shared | Scope::All) {
        let name = "shared_providers";
        match provider_rows::update_shared(data_dir, switch, |rows| {
            let (counts, failures, changed) = pass(rows, keys, &mut nonce);
            ((counts, failures), changed)
        }) {
            Ok(outcome) => add(&mut report, name.to_owned(), outcome),
            Err(e) => failed_table(&mut report, name, &e),
        }
    }
    let only = match scope {
        Scope::User(only) => only.as_deref(),
        _ => None,
    };
    if matches!(scope, Scope::User(_) | Scope::All) {
        for id in accounts_with_rows(data_dir, only) {
            let name = format!("user_providers:{id}");
            let Ok(dir) = store::user_dir(data_dir, &id) else {
                continue;
            };
            match provider_rows::update_user(&dir, switch, |rows| {
                let (counts, failures, changed) = pass(rows, keys, &mut nonce);
                ((counts, failures), changed)
            }) {
                Ok(outcome) => add(&mut report, name, outcome),
                // Removed meanwhile: there is nothing to re-seal.
                Err(StoreError::Gone) => {}
                Err(e) => failed_table(&mut report, &name, &e),
            }
        }
    }
    report
}

/// A fresh nonce per sealed key, from the OS.
pub fn fresh_nonce() -> [u8; 12] {
    crate::provider_routes::nonce()
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    const CURRENT: [u8; 32] = [3; 32];
    const PREVIOUS: [u8; 32] = [4; 32];

    fn keys() -> SecretKeys {
        SecretKeys::from_existing_bytes(
            zeroize::Zeroizing::new(CURRENT.to_vec()),
            Some(zeroize::Zeroizing::new(PREVIOUS.to_vec())),
        )
        .unwrap()
    }

    fn row(key: JValue) -> JValue {
        JValue::obj([("id", JValue::Str("r".into())), ("apiKey", key)])
    }

    #[test]
    fn scopes_are_the_three_the_rotation_names() {
        assert_eq!(Scope::parse(Some("shared"), None), Ok(Scope::Shared));
        assert_eq!(Scope::parse(Some("all"), None), Ok(Scope::All));
        assert_eq!(Scope::parse(Some("user"), None), Ok(Scope::User(None)));
        assert_eq!(
            Scope::parse(Some("user"), Some("u")),
            Ok(Scope::User(Some("u".into())))
        );
        for bad in [(None, None), (Some("x"), None), (Some("all"), Some("u"))] {
            assert!(Scope::parse(bad.0, bad.1).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn a_pass_counts_each_kind_of_key_and_changes_only_what_it_must() {
        let keys = keys();
        let mut n = 0u8;
        let mut nonce = || {
            n += 1;
            [n; 12]
        };
        let current = keys.seal("cur", None, &[100; 12]).unwrap();
        let previous = secret_envelope::seal(&PREVIOUS, &[101; 12], b"old", None).unwrap();
        let lost = secret_envelope::seal(&[9; 32], &[102; 12], b"lost", None).unwrap();
        let bound = secret_envelope::seal(&CURRENT, &[103; 12], b"b", Some(b"user")).unwrap();
        let mut rows = vec![
            row(JValue::Str(current.clone())),
            row(JValue::Str(previous)),
            row(JValue::Str("plain".into())),
            row(JValue::Str(lost.clone())),
            row(JValue::Str(bound.clone())),
            row(JValue::Str(String::new())),
            row(JValue::Null),
            row(JValue::Num(5.0)),
            JValue::Str("not a row".into()),
        ];
        let (counts, failures, changed) = pass(&mut rows, &keys, &mut nonce);
        assert!(changed);
        assert_eq!(
            counts,
            Counts {
                current: 1,
                rotated: 1,
                upgraded: 1,
                empty: 0,
                failed: 3
            }
        );
        assert_eq!(failures, vec![(3, NO_KEY_OPENS), (4, BOUND), (7, NOT_TEXT)]);
        assert_eq!(rows[0].get("apiKey").as_str(), Some(current.as_str()));
        assert_eq!(rows[3].get("apiKey").as_str(), Some(lost.as_str()));
        assert_eq!(rows[4].get("apiKey").as_str(), Some(bound.as_str()));
        for (at, plain) in [(1, "old"), (2, "plain")] {
            let sealed = rows[at].get("apiKey").as_str().unwrap();
            assert!(sealed.starts_with("enc:v1:"));
            let (opened, used) = keys.open(sealed, None).unwrap();
            assert_eq!((opened.as_str(), used.tag()), (plain, 1));
        }
        // Nothing left to do: a second pass changes nothing.
        let (again, failures, changed) = pass(&mut rows, &keys, &mut nonce);
        assert!(!changed);
        assert_eq!((again.current, again.rotated, again.upgraded), (3, 0, 0));
        assert_eq!(failures.len(), 3);
    }
}
