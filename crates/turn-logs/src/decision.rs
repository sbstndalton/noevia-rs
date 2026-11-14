//! core `server/decision-log.cjs`: a durable, text-free record of System-One decisions (routing
//! role and margin, supervision action, tool-gate decision, resend outcome), so real distributions
//! survive a container recreation. Callers pass only the fields they already log; nothing here
//! adds message text. Bounded: one current file plus one rotated file. Never fails a chat turn.
use crate::{bump, parse_line};
use js_json::{wtf16, JValue};
use std::fs::{self, OpenOptions};
use std::io::Write as _;
use std::os::unix::fs::OpenOptionsExt as _;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// core's default file name in the data directory.
pub const FILE_NAME: &str = "system-one-decisions.jsonl";
/// core's default bound for each of the two files, in bytes.
pub const MAX_BYTES: u64 = 1024 * 1024;

/// The clock: `now().toISOString()`.
pub type Clock = Box<dyn Fn() -> String + Send + Sync>;
/// The echo of each record (core: `console.info('[system-one] <kind>', JSON.stringify(entry))`).
pub type Echo = Box<dyn Fn(&str, &str) + Send + Sync>;

/// `createDecisionLog({ dir, name, maxBytes, now, echo })`: the writer.
pub struct DecisionLog {
    file: PathBuf,
    max_bytes: u64,
    now: Clock,
    echo: Option<Echo>,
}

/// One echo line exactly as `console.info` joins its two arguments.
pub fn echo_line(kind: &str, entry: &JValue) -> String {
    let json = wtf16::stringify(entry).unwrap_or_else(|| "undefined".into());
    format!("[system-one] {kind} {json}")
}

impl DecisionLog {
    /// core's defaults: `system-one-decisions.jsonl`, 1 MiB, the system clock, echo to stdout.
    pub fn new(dir: &Path) -> Self {
        Self {
            file: dir.join(FILE_NAME),
            max_bytes: MAX_BYTES,
            now: Box::new(|| iso_millis(SystemTime::now())),
            echo: Some(Box::new(|kind: &str, json: &str| {
                let _ = writeln!(std::io::stdout().lock(), "[system-one] {kind} {json}");
            })),
        }
    }

    /// Another file name in the same directory (core `name`).
    pub fn with_name(mut self, dir: &Path, name: &str) -> Self {
        self.file = dir.join(name);
        self
    }

    /// core `maxBytes`.
    pub fn with_max_bytes(mut self, max_bytes: u64) -> Self {
        self.max_bytes = max_bytes;
        self
    }

    /// core `now`.
    pub fn with_clock(mut self, now: Clock) -> Self {
        self.now = now;
        self
    }

    /// core `echo` (`None` = core's `echo: null`).
    pub fn with_echo(mut self, echo: Option<Echo>) -> Self {
        self.echo = echo;
        self
    }

    /// The current file (the rotated one is the same path with `.1`).
    pub fn file(&self) -> &Path {
        &self.file
    }

    /// The line core appends for `record(kind, entry)`: `{ at, kind, ...entry }` + `\n`. A field of
    /// `entry` named `at` or `kind` replaces that value in its place, as the spread does.
    pub fn line(&self, kind: &str, entry: &JValue) -> String {
        let mut items = vec![
            ("at".to_owned(), JValue::Str((self.now)())),
            ("kind".to_owned(), JValue::Str(kind.to_owned())),
        ];
        for (k, v) in spread(entry) {
            wtf16::set(&mut items, &k, v);
        }
        let mut line = wtf16::stringify(&JValue::Obj(items)).unwrap_or_default();
        line.push('\n');
        line
    }

    /// `record(kind, entry)`: echo, then append, rotating first when the line would take the file
    /// over the bound. Every failure (missing directory, full disk) is swallowed, as in core.
    ///
    /// Core compares the file's size in bytes with the line's length in UTF-16 units, so a line with
    /// non-ASCII text could take the file past `maxBytes`; this port counts bytes on both sides (the
    /// documented bound; sbstndalton/noevia#2776).
    pub fn record(&self, kind: &str, entry: &JValue) {
        if let Some(echo) = &self.echo {
            let json = wtf16::stringify(entry).unwrap_or_else(|| "undefined".into());
            echo(kind, &json);
        }
        let _ = self.append(&self.line(kind, entry));
    }

    fn append(&self, line: &str) -> std::io::Result<()> {
        let size = fs::metadata(&self.file).map(|m| m.len()).unwrap_or(0);
        if size + line.len() as u64 > self.max_bytes {
            fs::rename(&self.file, rotated(&self.file))?;
        }
        let mut f = OpenOptions::new()
            .append(true)
            .create(true)
            .mode(0o600)
            .open(&self.file)?;
        f.write_all(line.as_bytes())
    }
}

/// The own enumerable properties `{ ...value }` copies: an object's fields, an array's or a string's
/// indexes (a string by UTF-16 unit, as JS does), nothing for any other value.
fn spread(value: &JValue) -> Vec<(String, JValue)> {
    match value {
        JValue::Obj(fields) => fields.clone(),
        JValue::Arr(items) => items.iter().enumerate().map(|(i, v)| (i.to_string(), v.clone())).collect(),
        JValue::Str(s) => {
            let mut out = Vec::new();
            for c in s.chars() {
                let mut buf = [0u16; 2];
                let units = c.encode_utf16(&mut buf);
                if units.len() == 1 || wtf16::lone_unit(c).is_some() {
                    out.push((out.len().to_string(), JValue::Str(c.to_string())));
                } else {
                    for unit in units.iter() {
                        let half = wtf16::lone_char(*unit).map(String::from).unwrap_or_default();
                        out.push((out.len().to_string(), JValue::Str(half)));
                    }
                }
            }
            out
        }
        _ => Vec::new(),
    }
}

/// `<file>.1`.
pub fn rotated(file: &Path) -> PathBuf {
    let mut name = file.as_os_str().to_owned();
    name.push(".1");
    PathBuf::from(name)
}

const MARGIN_LOW: &str = "<0.05";
const MARGIN_MID: &str = "0.05-0.2";
const MARGIN_HIGH: &str = ">=0.2";

#[derive(Default)]
struct Counts {
    n: f64,
    a: Vec<(String, JValue)>,
    b: Vec<(String, JValue)>,
    c: Vec<(String, JValue)>,
}

impl Counts {
    fn out(self, names: [&str; 3]) -> Vec<(String, JValue)> {
        let [a, b, c] = names;
        vec![
            ("n".into(), JValue::Num(self.n)),
            (a.into(), JValue::Obj(self.a)),
            (b.into(), JValue::Obj(self.b)),
            (c.into(), JValue::Obj(self.c)),
        ]
    }
}

/// `e.x || null` as a bucket key: a falsy value is not counted.
fn or_null(v: &JValue) -> JValue {
    if v.truthy() {
        v.clone()
    } else {
        JValue::Null
    }
}

/// core `summarize(lines)`: counts by selection and fallback, the text-free fallback causes,
/// margin buckets, tool-gate reasons and resend outcomes. Lines that are not JSON (or hold `null`,
/// where core throws) are skipped.
pub fn summarize(lines: impl IntoIterator<Item = &str>) -> JValue {
    let mut route = Counts::default();
    let (mut low, mut mid, mut high) = (0.0, 0.0, 0.0);
    let mut supervise = Counts::default();
    let mut gate = Counts::default();
    let mut outcome_n = 0.0;
    let mut outcome_event = Vec::new();
    let mut outcome_prev = Vec::new();
    for raw in lines {
        let Some(e) = parse_line(raw) else { continue };
        match e.get("kind").as_str() {
            Some("route") => {
                route.n += 1.0;
                bump(&mut route.a, e.get("selected"));
                bump(&mut route.b, &or_null(e.get("fellBack")));
                bump(&mut route.c, &or_null(e.get("cause")));
                if let Some(m) = e.get("margin").as_num() {
                    if m < 0.05 {
                        low += 1.0;
                    } else if m < 0.2 {
                        mid += 1.0;
                    } else {
                        high += 1.0;
                    }
                }
            }
            Some("supervise") => {
                supervise.n += 1.0;
                bump(&mut supervise.a, e.get("action"));
                bump(&mut supervise.b, &or_null(e.get("fellBack")));
                bump(&mut supervise.c, &or_null(e.get("cause")));
            }
            Some("tool-gate") if e.get("event").as_str() == Some("decision") => {
                gate.n += 1.0;
                bump(&mut gate.a, e.get("source"));
                bump(&mut gate.b, &or_null(e.get("reason")));
                bump(&mut gate.c, &or_null(e.get("cause")));
            }
            Some("outcome") => {
                outcome_n += 1.0;
                bump(&mut outcome_event, e.get("event"));
                bump(&mut outcome_prev, &or_null(e.get("previousRole")));
            }
            _ => {}
        }
    }
    let mut route_out = route.out(["selected", "fellBack", "cause"]);
    route_out.push((
        "margin".into(),
        JValue::obj([
            (MARGIN_LOW, JValue::Num(low)),
            (MARGIN_MID, JValue::Num(mid)),
            (MARGIN_HIGH, JValue::Num(high)),
        ]),
    ));
    JValue::obj([
        ("route", JValue::Obj(route_out)),
        ("supervise", JValue::Obj(supervise.out(["action", "fellBack", "cause"]))),
        ("toolGate", JValue::Obj(gate.out(["source", "reason", "cause"]))),
        (
            "outcome",
            JValue::obj([
                ("n", JValue::Num(outcome_n)),
                ("event", JValue::Obj(outcome_event)),
                ("previousRole", JValue::Obj(outcome_prev)),
            ]),
        ),
    ])
}

/// The CLI's input: `<dir>/system-one-decisions.jsonl.1` then `<dir>/system-one-decisions.jsonl`,
/// non-empty lines; a missing or unreadable file reads as no lines.
pub fn read_dir_lines(dir: &Path) -> Vec<String> {
    let read = |p: PathBuf| -> Vec<String> {
        fs::read(&p)
            .map(|b| {
                String::from_utf8_lossy(&b)
                    .split('\n')
                    .filter(|l| !l.is_empty())
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default()
    };
    let current = dir.join(FILE_NAME);
    let mut lines = read(rotated(&current));
    lines.extend(read(current));
    lines
}

/// `Date.prototype.toISOString()` for a system time (UTC, milliseconds, `Z`).
pub fn iso_millis(t: SystemTime) -> String {
    let ms = match t.duration_since(UNIX_EPOCH) {
        Ok(d) => i64::try_from(d.as_millis()).unwrap_or(i64::MAX),
        Err(e) => -i64::try_from(e.duration().as_millis()).unwrap_or(i64::MAX),
    };
    iso_from_epoch_ms(ms)
}

/// `new Date(ms).toISOString()` for years 0..=9999 (outside that JS writes a signed six-digit year;
/// the clock never gets there).
pub fn iso_from_epoch_ms(ms: i64) -> String {
    let days = ms.div_euclid(86_400_000);
    let rem = ms.rem_euclid(86_400_000);
    let (y, m, d) = civil_from_days(days);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}.{:03}Z",
        rem / 3_600_000,
        rem / 60_000 % 60,
        rem / 1000 % 60,
        rem % 1000
    )
}

/// Days since 1970-01-01 to a proleptic Gregorian date (H. Hinnant's algorithm).
fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    (y, m, d)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn iso_matches_date_to_iso_string() {
        assert_eq!(iso_from_epoch_ms(0), "1970-01-01T00:00:00.000Z");
        // new Date('2026-09-22T08:00:00Z').getTime()
        assert_eq!(iso_from_epoch_ms(1_790_064_000_000), "2026-09-22T08:00:00.000Z");
        assert_eq!(iso_from_epoch_ms(951_782_400_123), "2000-02-29T00:00:00.123Z");
        assert_eq!(iso_from_epoch_ms(-1), "1969-12-31T23:59:59.999Z");
        assert_eq!(iso_from_epoch_ms(4_102_444_799_999), "2099-12-31T23:59:59.999Z");
    }

    #[test]
    fn line_spreads_entry_after_at_and_kind() {
        let dir = Path::new("/nonexistent");
        let log = DecisionLog::new(dir).with_clock(Box::new(|| "T".to_owned())).with_echo(None);
        let entry = wtf16::parse(r#"{"kind":"other","b":1,"2":true,"at":"x","1":null}"#).unwrap();
        // JS: { at:'T', kind:'route', ...entry } keeps at/kind in place, index keys first ascending.
        assert_eq!(log.line("route", &entry), "{\"1\":null,\"2\":true,\"at\":\"x\",\"kind\":\"other\",\"b\":1}\n");
        assert_eq!(log.line("route", &JValue::Str("ab".into())), "{\"0\":\"a\",\"1\":\"b\",\"at\":\"T\",\"kind\":\"route\"}\n");
        assert_eq!(log.line("route", &JValue::Null), "{\"at\":\"T\",\"kind\":\"route\"}\n");
        assert_eq!(log.line("route", &JValue::Num(3.0)), "{\"at\":\"T\",\"kind\":\"route\"}\n");
    }

    #[test]
    fn echo_line_is_console_info_of_two_arguments() {
        let e = wtf16::parse(r#"{"selected":"fast","margin":0.3}"#).unwrap();
        assert_eq!(echo_line("route", &e), r#"[system-one] route {"selected":"fast","margin":0.3}"#);
        assert_eq!(echo_line("route", &JValue::Undefined), "[system-one] route undefined");
    }
}
