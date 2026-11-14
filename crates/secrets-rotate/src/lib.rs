//! The secrets rotation's own part (track R12): core `server/secrets.cjs` `reseal` and `rotate`, and
//! `server/secrets-rotate.cjs` `runRotation`'s merge, for `POST /api/admin/secrets/rotate` answered by
//! the Rust front.
//!
//! Node's rotation runs, in a fixed order: the front's provider pass, the front's MCP pass, the front's
//! Google Drive pass (soft), then its own [`rotate`] over `storage_connections` and
//! `chatgpt_oauth_tokens` (each value re-sealed under the current key, bound to its account,
//! written back by compare-and-swap) and a read-only check that every Drive token file opens under
//! the current key; then [`merge`] puts the front's passes into the report, relabels the Drive files
//! the front re-sealed as rotated, and the audit row gets [`audit_detail`]. This crate is that logic
//! over two seams the caller fills: [`Keys`] (the key store) and [`Table`] (rows and their writes).
//!
//! Nothing here ever puts a key, a ciphertext or a plaintext in a report or an error: failures carry
//! the table, the row's reference and a fixed reason.

use js_json::{wtf16, JValue};
use secret_envelope::KeyUsed;
use zeroize::Zeroizing;

/// The five counts of a table, in Node's key order.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Counts {
    pub current: u64,
    pub rotated: u64,
    pub upgraded: u64,
    pub empty: u64,
    pub failed: u64,
}

impl Counts {
    fn add(&mut self, other: &Counts) {
        self.current += other.current;
        self.rotated += other.rotated;
        self.upgraded += other.upgraded;
        self.empty += other.empty;
        self.failed += other.failed;
    }

    fn bump(&mut self, status: Status) {
        match status {
            Status::Current => self.current += 1,
            Status::Rotated => self.rotated += 1,
            Status::Upgraded => self.upgraded += 1,
            Status::Empty => self.empty += 1,
        }
    }

    fn json(&self) -> JValue {
        let n = |v: u64| JValue::Num(v as f64);
        JValue::obj([
            ("current", n(self.current)),
            ("rotated", n(self.rotated)),
            ("upgraded", n(self.upgraded)),
            ("empty", n(self.empty)),
            ("failed", n(self.failed)),
        ])
    }
}

/// What happened to one stored value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Current,
    Rotated,
    Upgraded,
    Empty,
}

/// One failure: the table, the row (an id list, a file name, a position, or `null`) and why.
#[derive(Debug, Clone, PartialEq)]
pub struct Failure {
    pub table: String,
    pub reference: JValue,
    pub error: String,
}

/// A pass's result in the report's shape: `{ tables, failures }`.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct Pass {
    /// Table name and counts, in the order the pass reported them.
    pub tables: Vec<(String, Counts)>,
    pub failures: Vec<Failure>,
}

/// `{ tables, totals, failures, previousKey }`.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct Report {
    pub tables: Vec<(String, Counts)>,
    pub totals: Counts,
    pub failures: Vec<Failure>,
    pub previous_key: bool,
}

/// The key store: secrets.cjs `open` (current key first, then the previous one; a value that is not an
/// envelope comes back as it is with [`KeyUsed::None`]) and `encrypt` under the current key (`''` for
/// an empty value; `enc:v2:` bound to `user` when there is one, else `enc:v1:`).
/// A key store refusal: the value does not open, or cannot be sealed. It carries no detail on
/// purpose (a reason could name a key or a value).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KeyError;

pub trait Keys {
    fn open(
        &self,
        value: &str,
        user: Option<&str>,
    ) -> Result<(Zeroizing<String>, KeyUsed), KeyError>;
    fn seal(&self, plain: &str, user: Option<&str>) -> Result<String, KeyError>;
}

/// One stored value to re-seal.
#[derive(Debug, Clone, PartialEq)]
pub struct Row {
    /// The row's reference in the report (`[user_id]` for the SQL tables, a file name for Drive).
    pub reference: JValue,
    pub value: String,
    /// The owning account for a bound table (`enc:v2`), `None` for an unbound one.
    pub user: Option<String>,
}

/// A table of stored values, as secrets.cjs `rotate({ tables })` takes it.
pub trait Table {
    fn name(&self) -> &str;
    /// Every value to look at, read before any is written. `Err` is the reason the table could not be
    /// listed (one failure with `ref: null`, the table's other counts stay 0).
    fn rows(&mut self) -> Result<Vec<Row>, String>;
    /// Writes `value` in place of `old` only while the row still holds `old`: `Ok(false)` when another
    /// writer changed it meanwhile.
    fn write(&mut self, row: &Row, value: &str) -> Result<bool, String>;
    /// A table whose values are not `enc:` strings (the Drive token files) brings its own check; `None`
    /// uses [`reseal`].
    fn check(&mut self, _row: &Row) -> Option<Result<Status, String>> {
        None
    }
}

pub const UNKNOWN_ACCOUNT: &str = "credential is bound to an unknown account";
/// secret-envelope.cjs `OPEN_FAILURE`.
pub const OPEN_FAILURE: &str = "credential could not be opened";
/// secret-envelope.cjs `SEAL_FAILURE`.
pub const SEAL_FAILURE: &str = "credential could not be sealed";
pub const CHANGED: &str = "changed while rotating; run the rotation again";

/// secrets.cjs `reseal(value, userId)`: the status, and the new value when it is to be written.
pub fn reseal<K: Keys + ?Sized>(
    keys: &K,
    value: &str,
    user: Option<&str>,
) -> Result<(Status, Option<String>), &'static str> {
    if value.is_empty() {
        return Ok((Status::Empty, None));
    }
    let version = if value.starts_with("enc:v2:") {
        2
    } else if value.starts_with("enc:v1:") {
        1
    } else {
        0
    };
    let user = user.filter(|u| !u.is_empty());
    let wanted = if user.is_some() { 2 } else { 1 };
    if version == 2 && wanted == 1 {
        return Err(UNKNOWN_ACCOUNT);
    }
    let (plain, used) = keys.open(value, user).map_err(|KeyError| OPEN_FAILURE)?;
    if used == KeyUsed::Current && version == wanted {
        return Ok((Status::Current, None));
    }
    let next = keys.seal(&plain, user).map_err(|KeyError| SEAL_FAILURE)?;
    let status = if used == KeyUsed::Previous {
        Status::Rotated
    } else {
        Status::Upgraded
    };
    Ok((status, Some(next)))
}

/// `String(e.message).slice(0, 200)`: at most 200 UTF-16 units.
fn clip(text: &str) -> String {
    wtf16::slice(text, 200)
}

/// secrets.cjs `rotate({ tables })`: every table in order, one bad row counted and reported, never
/// fatal.
pub fn rotate<K: Keys + ?Sized>(keys: &K, tables: &mut [&mut dyn Table]) -> Report {
    let mut report = Report::default();
    for table in tables.iter_mut() {
        let name = table.name().to_string();
        let mut counts = Counts::default();
        let rows = match table.rows() {
            Ok(rows) => rows,
            Err(error) => {
                counts.failed += 1;
                report.failures.push(Failure {
                    table: name.clone(),
                    reference: JValue::Null,
                    error,
                });
                Vec::new()
            }
        };
        for row in &rows {
            let outcome = match table.check(row) {
                Some(checked) => checked,
                None => match reseal(keys, &row.value, row.user.as_deref()) {
                    Err(e) => Err(e.to_string()),
                    Ok((status, None)) => Ok(status),
                    Ok((status, Some(next))) => match table.write(row, &next) {
                        Ok(true) => Ok(status),
                        Ok(false) => Err(CHANGED.to_string()),
                        Err(e) => Err(e),
                    },
                },
            };
            match outcome {
                Ok(status) => counts.bump(status),
                Err(error) => {
                    counts.failed += 1;
                    report.failures.push(Failure {
                        table: name.clone(),
                        reference: row.reference.clone(),
                        error: clip(&error),
                    });
                }
            }
        }
        set_table(&mut report.tables, &name, counts);
        report.totals.add(&counts);
    }
    report
}

/// `report.tables[name] = counts`: in place when the name is there, else at the end.
fn set_table(tables: &mut Vec<(String, Counts)>, name: &str, counts: Counts) {
    match tables.iter_mut().find(|(n, _)| n == name) {
        Some(slot) => slot.1 = counts,
        None => tables.push((name.to_string(), counts)),
    }
}

/// The front's Drive re-seal as `resealGoogleDrive` reports it: its counts, or why it could not.
#[derive(Debug, Clone, PartialEq)]
pub enum DriveFront {
    Counts(Counts),
    Error(String),
}

/// The Drive table's name.
pub const DRIVE_TABLE: &str = "google_drive_users";

/// secrets-rotate.cjs `runRotation` after `secrets.rotate`: the providers' and the MCP passes merged
/// (`mergeProviderPass`), the Drive files the front re-sealed relabelled as rotated (or, when it could
/// not and a file failed the check, why), and `previousKey`.
pub fn merge(
    mut report: Report,
    providers: &Pass,
    mcp: &Pass,
    drive: &DriveFront,
    previous_key: bool,
) -> Report {
    for pass in [providers, mcp] {
        for (name, counts) in &pass.tables {
            set_table(&mut report.tables, name, *counts);
            report.totals.add(counts);
        }
        report.failures.extend(pass.failures.iter().cloned());
    }
    if let Some((_, t)) = report.tables.iter_mut().find(|(n, _)| n == DRIVE_TABLE) {
        match drive {
            DriveFront::Counts(front) => {
                let moved = front.rotated.min(t.current);
                t.current -= moved;
                t.rotated += moved;
                report.totals.current -= moved;
                report.totals.rotated += moved;
            }
            DriveFront::Error(error) if t.failed > 0 => report.failures.push(Failure {
                table: DRIVE_TABLE.to_string(),
                reference: JValue::Null,
                error: error.clone(),
            }),
            DriveFront::Error(_) => {}
        }
    }
    report.previous_key = previous_key;
    report
}

fn failure_json(f: &Failure, with_error: bool) -> JValue {
    let mut items = vec![
        ("table".to_string(), JValue::Str(f.table.clone())),
        ("ref".to_string(), f.reference.clone()),
    ];
    if with_error {
        items.push(("error".to_string(), JValue::Str(f.error.clone())));
    }
    JValue::Obj(items)
}

impl Report {
    /// The reply: `JSON.stringify(report)`.
    pub fn json(&self) -> String {
        let tables = JValue::Obj(
            self.tables
                .iter()
                .map(|(n, c)| (n.clone(), c.json()))
                .collect(),
        );
        let failures = JValue::Arr(
            self.failures
                .iter()
                .map(|f| failure_json(f, true))
                .collect(),
        );
        let value = JValue::obj([
            ("tables", tables),
            ("totals", self.totals.json()),
            ("failures", failures),
            ("previousKey", JValue::Bool(self.previous_key)),
        ]);
        wtf16::stringify(&value).unwrap_or_default()
    }
}

/// The audit row's detail: `{ totals, failed: failures.map(({ table, ref })).slice(0, 50) }`.
pub fn audit_detail(report: &Report) -> String {
    let failed = JValue::Arr(
        report
            .failures
            .iter()
            .take(50)
            .map(|f| failure_json(f, false))
            .collect(),
    );
    wtf16::stringify(&JValue::obj([
        ("totals", report.totals.json()),
        ("failed", failed),
    ]))
    .unwrap_or_default()
}

/// The audit row's action.
pub const AUDIT_ACTION: &str = "secrets.rotate";

/// JS `\s`: what `String.prototype.trim` strips.
fn is_space(c: char) -> bool {
    matches!(
        c,
        '\t' | '\n' | '\u{b}' | '\u{c}' | '\r' | ' ' | '\u{a0}' | '\u{1680}' | '\u{2000}'
            ..='\u{200a}'
                | '\u{2028}'
                | '\u{2029}'
                | '\u{202f}'
                | '\u{205f}'
                | '\u{3000}'
                | '\u{feff}'
    )
}

/// gdrive-seal.cjs `checkTokenFile`'s reason.
pub const DRIVE_NOT_CURRENT: &str =
    "not sealed under the current key; the Rust front has not re-sealed it";

/// gdrive-seal.cjs `checkTokenFile(file, derive('google-drive-user'))` on the file's text: `Empty` for
/// a blank file, `Current` when `open` (the current key's sealer) opens it to JSON, else the reason
/// (Node's `sealer.open` parses the plaintext, so a sealed non-JSON value does not pass either).
pub fn drive_check(
    text: &str,
    open: impl Fn(&str) -> Result<Zeroizing<Vec<u8>>, KeyError>,
) -> Result<Status, String> {
    if text.trim_matches(is_space).is_empty() {
        return Ok(Status::Empty);
    }
    let plain = open(text).map_err(|KeyError| DRIVE_NOT_CURRENT.to_string())?;
    let parsed = wtf16::parse(&String::from_utf8_lossy(&plain));
    match parsed {
        Ok(_) | Err(wtf16::Refusal::Unsupported) => Ok(Status::Current),
        Err(wtf16::Refusal::Invalid) => Err(DRIVE_NOT_CURRENT.to_string()),
    }
}

/// `resealGoogleDrive`'s reason when the front could not re-seal the files: its status, or
/// `unreachable`.
pub fn drive_front_error(status: Option<u16>) -> String {
    let why = status.map_or_else(|| "unreachable".to_string(), |s| s.to_string());
    format!("the Rust front could not re-seal the Google Drive token files ({why})")
}

impl Pass {
    /// What core's clients of the front's passes keep (rust-providers-reseal.cjs,
    /// rust-mcp-credentials.cjs): the first 1,000 failures, each reason cut to 200 UTF-16 units.
    pub fn normalised(mut self) -> Pass {
        self.failures.truncate(1000);
        for f in &mut self.failures {
            f.error = clip(&f.error);
        }
        self
    }
}
