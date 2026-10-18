//! noevia#1318 step 3c: the model manager's passive request telemetry, ported from
//! noevia-services `model-manager/app/telemetry.py`.
//!
//! The pure parts, matching Python to the bit:
//!
//! - [`parse_log`] (`parse_log`): `docker logs -t` text of one backend into the request samples
//!   and the spawned server configurations, with the stored-instance attribution of a request
//!   whose spawn line scrolled out of the tail (#904) asked of a caller-supplied lookup (the
//!   store is step 3d);
//! - [`log_span`], [`argv_to_dict`], [`differing_keys`], [`pct`] (`_pct`), [`instance_key`];
//! - [`stats_for`] and [`config_history`] over rows the caller read (`db.recent_timings`,
//!   `db.timings_by_instance`, `db.server_configs_for`).
//!
//! Python's `re` is followed with its own character classes: `\s` is `str.isspace` (Unicode
//! whitespace and `\x1c`-`\x1f`), `\d` any Unicode decimal digit, `.` anything but `\n`; lines
//! are split as `str.splitlines` splits them. Where Python's answer depends on something this
//! side does not reproduce, the parse is refused ([`Error::Unsupported`]) and the caller keeps
//! Python: a timestamp in a shape other than `YYYY-MM-DDTHH:MM:SS[.fraction]` with a `Z` or
//! `+HH:MM` offset (a naive one depends on the local zone), a number written with non-ASCII
//! digits, an integer past 64 bits. Where Python raises (a timing value that is not a float,
//! such as `1.2.3`), [`Error::Raised`] says so.
#![forbid(unsafe_code)]

use regex::Regex;
use std::sync::OnceLock;

mod time;

/// `_MIN_GEN_TOKENS`.
pub const MIN_GEN_TOKENS: i64 = 16;
/// `_MIN_SAMPLES_PER_CONFIG`.
pub const MIN_SAMPLES_PER_CONFIG: usize = 5;
/// `_VOLATILE_ARGS`.
const VOLATILE_ARGS: [&str; 5] = ["--port", "-p", "--host", "--alias", "-a"];
/// `_SPEC_KNOB_FLAGS`.
const SPEC_KNOB_FLAGS: [&str; 6] = [
    "--spec-draft-n-max",
    "--spec-draft-n-min",
    "--spec-draft-p-min",
    "--draft-p-min",
    "--draft-max",
    "--draft-min",
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    /// Python raises here (its caller decides what that means).
    Raised(&'static str),
    /// Python's answer is not reproduced here; keep Python.
    Unsupported(&'static str),
}

/// An insertion-ordered dict of strings (assignment to a present key keeps its place).
pub type Dict = Vec<(String, String)>;

fn dict_set(d: &mut Dict, k: &str, v: String) {
    match d.iter_mut().find(|(key, _)| key == k) {
        Some((_, slot)) => *slot = v,
        None => d.push((k.to_owned(), v)),
    }
}

fn dict_get<'a>(d: &'a Dict, k: &str) -> Option<&'a str> {
    d.iter().find(|(key, _)| key == k).map(|(_, v)| v.as_str())
}

#[derive(Debug, Clone, PartialEq)]
pub struct Sample {
    pub ts: f64,
    pub instance: String,
    pub task: i64,
    pub model_path: String,
    pub alias: String,
    pub spec_type: String,
    pub prompt_tokens: i64,
    pub prompt_tps: f64,
    pub gen_tokens: i64,
    pub gen_tps: f64,
    pub draft_acc: Option<f64>,
    pub draft_len: Option<f64>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ServerConfig {
    pub instance: String,
    pub alias: String,
    pub model_path: String,
    pub argv: Dict,
    pub first_seen: f64,
}

/// One row of `db.server_instances_on_port(backend, port)`, newest first.
#[derive(Debug, Clone, PartialEq)]
pub struct Stored {
    pub instance: String,
    pub alias: String,
    pub model_path: String,
    pub argv: Dict,
    pub first_seen: f64,
}

/// The stored-instance attribution of `parse_log(text, backend, covered_until,
/// continuous_since)`; `lookup(port)` is `db.server_instances_on_port(backend, port)` (`Err`:
/// it raised, which Python treats as no rows).
pub struct Backend<'a> {
    pub covered_until: f64,
    pub continuous_since: f64,
    pub lookup: &'a mut dyn FnMut(&str) -> Result<Vec<Stored>, ()>,
}

/// Python's `\s` for `str`, inside a character class.
const WS: &str = r"\s\x1c-\x1f";

fn re(cell: &'static OnceLock<Regex>, pattern: &str) -> &'static Regex {
    cell.get_or_init(|| {
        let p = pattern
            .replace(r"\S", &format!("[^{WS}]"))
            .replace(r"\s", &format!("[{WS}]"));
        Regex::new(&p).unwrap_or_else(|e| unreachable!("{e}"))
    })
}

macro_rules! pattern {
    ($name:ident, $p:expr) => {
        fn $name() -> &'static Regex {
            static CELL: OnceLock<Regex> = OnceLock::new();
            re(&CELL, $p)
        }
    };
}

pattern!(
    re_spawn,
    r"srv\s+load: spawning server instance with name=(\S+) on port (\d+)"
);
pattern!(
    re_spawn_args,
    r"srv\s+load: spawning server instance with args:"
);
pattern!(re_argv, r"srv\s+load:\s{2,}(\S.*?)\s*$");
pattern!(
    re_load,
    r"\[(\d+)\].*?srv\s+load_model: loading model '([^']+)'"
);
pattern!(re_ts, r"^(\d{4}-\d{2}-\d{2}T\S+?)\s");
pattern!(
    re_prompt,
    r"\[(\d+)\].*?print_timing: id\s+(\d+) \| task (\d+) \| prompt eval time =\s*([\d.]+) ms /\s*(\d+) tokens \(\s*[\d.]+ ms per token,\s*([\d.]+) tokens per second\)"
);
pattern!(
    re_eval,
    r"\[(\d+)\].*?print_timing: id\s+(\d+) \| task (\d+) \|\s+eval time =\s*([\d.]+) ms /\s*(\d+) tokens \(\s*[\d.]+ ms per token,\s*([\d.]+) tokens per second\)"
);
pattern!(
    re_draft,
    r"\[(\d+)\].*?print_timing: id\s+(\d+) \| task (\d+) \| draft acceptance = ([\d.]+) \(\s*(\d+) accepted /\s*(\d+) generated\), mean len =\s*([\d.]+)"
);

/// `str.splitlines()`.
pub fn splitlines(text: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut start = 0;
    let mut chars = text.char_indices().peekable();
    while let Some((i, c)) = chars.next() {
        let end = match c {
            '\r' => {
                if let Some((_, '\n')) = chars.peek() {
                    chars.next();
                    i + 2
                } else {
                    i + 1
                }
            }
            '\n' | '\x0b' | '\x0c' | '\x1c' | '\x1d' | '\x1e' | '\u{85}' | '\u{2028}'
            | '\u{2029}' => i + c.len_utf8(),
            _ => continue,
        };
        out.push(&text[start..i]);
        start = end;
    }
    if start < text.len() {
        out.push(&text[start..]);
    }
    out
}

/// `int(s)` of a `\d+` capture.
fn int(s: &str) -> Result<i64, Error> {
    if !s.bytes().all(|b| b.is_ascii_digit()) {
        return Err(Error::Unsupported("a number in non-ASCII digits"));
    }
    s.parse()
        .map_err(|_| Error::Unsupported("an integer past 64 bits"))
}

/// `float(s)` of a `[\d.]+` capture.
fn float(s: &str) -> Result<f64, Error> {
    if !s.bytes().all(|b| b.is_ascii_digit() || b == b'.') {
        return Err(Error::Unsupported("a number in non-ASCII digits"));
    }
    s.parse()
        .map_err(|_| Error::Raised("ValueError: could not convert string to float"))
}

/// `_parse_ts(line)`.
pub fn parse_ts(line: &str) -> Result<f64, Error> {
    let Some(m) = re_ts().captures(line) else {
        return Ok(0.0);
    };
    let text = m.get(1).map_or("", |g| g.as_str()).replace('Z', "+00:00");
    time::fromisoformat_timestamp(&text)
}

/// `instance_key(port, spawn_ts)`.
pub fn instance_key(port: &str, spawn_ts: f64) -> Result<String, Error> {
    if spawn_ts == 0.0 {
        return Ok(port.to_owned());
    }
    let t = spawn_ts.trunc();
    if !t.is_finite() || t.abs() >= 9.0e15 {
        return Err(Error::Unsupported("a timestamp past 2**53"));
    }
    Ok(format!("{port}@{}", t as i64))
}

/// `argv_to_dict(tokens)`.
pub fn argv_to_dict<S: AsRef<str>>(tokens: &[S]) -> Dict {
    let mut out = Dict::new();
    let mut i = 0;
    while let Some(tok) = tokens.get(i).map(AsRef::as_ref) {
        if !tok.starts_with('-') {
            i += 1;
            continue;
        }
        match tokens.get(i + 1).map(AsRef::as_ref) {
            Some(next) if !next.starts_with('-') => {
                dict_set(&mut out, tok, next.to_owned());
                i += 2;
            }
            _ => {
                dict_set(&mut out, tok, "true".to_owned());
                i += 1;
            }
        }
    }
    out.retain(|(k, _)| !VOLATILE_ARGS.contains(&k.as_str()));
    out
}

/// `differing_keys(configs)`.
pub fn differing_keys(configs: &[&Dict]) -> Vec<String> {
    if configs.len() < 2 {
        return Vec::new();
    }
    let mut keys: Vec<&str> = Vec::new();
    for c in configs {
        for (k, _) in c.iter() {
            if !keys.contains(&k.as_str()) {
                keys.push(k);
            }
        }
    }
    let mut out: Vec<String> = keys
        .into_iter()
        .filter(|k| {
            let first = configs
                .first()
                .and_then(|c| dict_get(c, k))
                .unwrap_or("\u{2014}");
            configs
                .iter()
                .any(|c| dict_get(c, k).unwrap_or("\u{2014}") != first)
        })
        .map(str::to_owned)
        .collect();
    out.sort();
    out
}

/// `_pct(values, q)`: nearest rank, Python's `round` (ties to even).
pub fn pct(values: &[f64], q: f64) -> f64 {
    if values.is_empty() {
        return 0.0;
    }
    let mut s = values.to_vec();
    s.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let i = (q * (s.len() - 1) as f64).round_ties_even();
    let last = (s.len() - 1) as f64;
    let i = i.clamp(0.0, last) as usize;
    s.get(i).copied().unwrap_or(0.0)
}

/// `_spec_label(argv)`.
fn spec_label(argv: &Dict) -> String {
    let spec = dict_get(argv, "--spec-type").unwrap_or("");
    let knobs: Vec<String> = SPEC_KNOB_FLAGS
        .iter()
        .filter_map(|k| {
            dict_get(argv, k).map(|v| format!("{}={v}", k.rsplit('-').next().unwrap_or("")))
        })
        .collect();
    if !spec.is_empty() && !knobs.is_empty() {
        format!("{spec} ({})", knobs.join(" "))
    } else {
        spec.to_owned()
    }
}

/// `log_span(text)`: the first and last line timestamps, 0.0 where there is none.
pub fn log_span(text: &str) -> Result<(f64, f64), Error> {
    let (mut first, mut last) = (0.0, 0.0f64);
    for line in splitlines(text) {
        let ts = parse_ts(line)?;
        if ts != 0.0 {
            if first == 0.0 {
                first = ts;
            }
            // `max(last, ts)`: the first of equal values (they are equal floats here).
            if ts > last {
                last = ts;
            }
        }
    }
    Ok((first, last))
}

fn get<'a>(v: &'a [(String, ServerConfig)], k: &str) -> Option<&'a ServerConfig> {
    v.iter().find(|(key, _)| key == k).map(|(_, c)| c)
}

fn put(v: &mut Vec<(String, ServerConfig)>, k: &str, c: ServerConfig) {
    match v.iter_mut().find(|(key, _)| key == k) {
        Some((_, slot)) => *slot = c,
        None => v.push((k.to_owned(), c)),
    }
}

/// `parse_log(text, backend, covered_until, continuous_since)`: `backend` is `None` for the pure
/// `parse_log(text)`.
pub fn parse_log(
    text: &str,
    mut backend: Option<Backend<'_>>,
) -> Result<(Vec<Sample>, Vec<ServerConfig>), Error> {
    let mut pending_alias = String::new();
    let mut pending_port = String::new();
    let mut pending_argv: Vec<String> = Vec::new();
    let mut by_pid: Vec<(String, ServerConfig)> = Vec::new();
    let mut spec_of: Vec<(String, String)> = Vec::new();
    let mut key_of_port: Vec<(String, String)> = Vec::new();
    let mut samples: Vec<((String, i64), Sample)> = Vec::new();
    let mut stored_by_port: Vec<(String, Vec<Stored>)> = Vec::new();
    let first_ts = if backend.is_some() {
        log_span(text)?.0
    } else {
        0.0
    };
    let gap_free = backend
        .as_ref()
        .is_some_and(|b| first_ts != 0.0 && b.covered_until != 0.0 && first_ts <= b.covered_until);

    let flush = |pending_alias: &mut String,
                 pending_port: &mut String,
                 pending_argv: &mut Vec<String>,
                 by_pid: &mut Vec<(String, ServerConfig)>,
                 spec_of: &mut Vec<(String, String)>| {
        if !pending_port.is_empty() {
            let argv = argv_to_dict(pending_argv);
            let path = dict_get(&argv, "--model")
                .filter(|p| !p.is_empty())
                .or_else(|| dict_get(&argv, "-m"))
                .unwrap_or("")
                .to_owned();
            let spec = spec_label(&argv);
            let prev = get(by_pid, pending_port).cloned();
            let rec = ServerConfig {
                instance: pending_port.clone(),
                alias: if pending_alias.is_empty() {
                    prev.as_ref().map_or(String::new(), |p| p.alias.clone())
                } else {
                    pending_alias.clone()
                },
                model_path: if path.is_empty() {
                    prev.as_ref()
                        .map_or(String::new(), |p| p.model_path.clone())
                } else {
                    path
                },
                argv: if argv.is_empty() {
                    prev.as_ref().map_or(Dict::new(), |p| p.argv.clone())
                } else {
                    argv
                },
                first_seen: prev.as_ref().map_or(0.0, |p| p.first_seen),
            };
            put(by_pid, pending_port, rec);
            match spec_of.iter_mut().find(|(k, _)| k == pending_port.as_str()) {
                Some((_, slot)) => *slot = spec,
                None => spec_of.push((pending_port.clone(), spec)),
            }
        }
        pending_alias.clear();
        pending_port.clear();
        pending_argv.clear();
    };

    for line in splitlines(text) {
        let ts = parse_ts(line)?;
        if let Some(m) = re_spawn().captures(line) {
            flush(
                &mut pending_alias,
                &mut pending_port,
                &mut pending_argv,
                &mut by_pid,
                &mut spec_of,
            );
            let port = m.get(2).map_or("", |g| g.as_str()).to_owned();
            pending_alias = m.get(1).map_or("", |g| g.as_str()).to_owned();
            pending_port = instance_key(&port, ts)?;
            match key_of_port.iter_mut().find(|(k, _)| *k == port) {
                Some((_, slot)) => *slot = pending_port.clone(),
                None => key_of_port.push((port, pending_port.clone())),
            }
            put(
                &mut by_pid,
                &pending_port.clone(),
                ServerConfig {
                    instance: pending_port.clone(),
                    alias: pending_alias.clone(),
                    model_path: String::new(),
                    argv: Dict::new(),
                    first_seen: ts,
                },
            );
            continue;
        }
        if !pending_port.is_empty() {
            if re_spawn_args().is_match(line) {
                continue;
            }
            if let Some(m) = re_argv().captures(line) {
                pending_argv.push(m.get(1).map_or("", |g| g.as_str()).to_owned());
                continue;
            }
            flush(
                &mut pending_alias,
                &mut pending_port,
                &mut pending_argv,
                &mut by_pid,
                &mut spec_of,
            );
        }
        if let Some(m) = re_load().captures(line) {
            flush(
                &mut pending_alias,
                &mut pending_port,
                &mut pending_argv,
                &mut by_pid,
                &mut spec_of,
            );
            let port = m.get(1).map_or("", |g| g.as_str());
            let path = m.get(2).map_or("", |g| g.as_str()).to_owned();
            let pid = key_of_port
                .iter()
                .find(|(k, _)| k == port)
                .map_or(port.to_owned(), |(_, v)| v.clone());
            let mut rec = get(&by_pid, &pid).cloned().unwrap_or(ServerConfig {
                instance: pid.clone(),
                alias: String::new(),
                model_path: String::new(),
                argv: Dict::new(),
                first_seen: 0.0,
            });
            rec.model_path = path;
            if rec.first_seen == 0.0 {
                rec.first_seen = ts;
            }
            put(&mut by_pid, &pid, rec);
            continue;
        }
        for kind in 0..3 {
            let rx = match kind {
                0 => re_prompt(),
                1 => re_eval(),
                _ => re_draft(),
            };
            let Some(m) = rx.captures(line) else {
                continue;
            };
            let g = |i: usize| m.get(i).map_or("", |g| g.as_str());
            let port = g(1);
            let task = int(g(3))?;
            let mut pid = key_of_port
                .iter()
                .find(|(k, _)| k == port)
                .map_or(port.to_owned(), |(_, v)| v.clone());
            let stored = if key_of_port.iter().any(|(k, _)| k == port) {
                None
            } else {
                stored_instance(
                    backend.as_mut(),
                    gap_free,
                    &mut stored_by_port,
                    &by_pid,
                    port,
                    ts,
                )
            };
            if let Some(s) = &stored {
                pid = s.instance.clone();
            }
            let key = (pid.clone(), task);
            let idx = match samples.iter().position(|(k, _)| *k == key) {
                Some(i) => i,
                None => {
                    let s = match &stored {
                        Some(st) => Sample {
                            ts,
                            instance: pid.clone(),
                            task,
                            model_path: st.model_path.clone(),
                            alias: st.alias.clone(),
                            spec_type: spec_label(&st.argv),
                            ..empty_sample()
                        },
                        None => {
                            let rec = get(&by_pid, &pid);
                            Sample {
                                ts,
                                instance: pid.clone(),
                                task,
                                model_path: rec.map_or(String::new(), |r| r.model_path.clone()),
                                alias: rec.map_or(String::new(), |r| r.alias.clone()),
                                spec_type: spec_of
                                    .iter()
                                    .find(|(k, _)| *k == pid)
                                    .map_or(String::new(), |(_, v)| v.clone()),
                                ..empty_sample()
                            }
                        }
                    };
                    samples.push((key, s));
                    samples.len() - 1
                }
            };
            let Some((_, s)) = samples.get_mut(idx) else {
                continue;
            };
            if ts != 0.0 {
                s.ts = ts;
            }
            match kind {
                0 => {
                    s.prompt_tokens = int(g(5))?;
                    s.prompt_tps = float(g(6))?;
                }
                1 => {
                    s.gen_tokens = int(g(5))?;
                    s.gen_tps = float(g(6))?;
                }
                _ => {
                    s.draft_acc = Some(float(g(4))?);
                    s.draft_len = Some(float(g(7))?);
                }
            }
            break;
        }
    }
    Ok((
        samples.into_iter().map(|(_, s)| s).collect(),
        by_pid.into_iter().map(|(_, c)| c).collect(),
    ))
}

fn empty_sample() -> Sample {
    Sample {
        ts: 0.0,
        instance: String::new(),
        task: 0,
        model_path: String::new(),
        alias: String::new(),
        spec_type: String::new(),
        prompt_tokens: 0,
        prompt_tps: 0.0,
        gen_tokens: 0,
        gen_tps: 0.0,
        draft_acc: None,
        draft_len: None,
    }
}

/// `_stored_instance(port, ts)`.
fn stored_instance(
    backend: Option<&mut Backend<'_>>,
    gap_free: bool,
    cache: &mut Vec<(String, Vec<Stored>)>,
    by_pid: &[(String, ServerConfig)],
    port: &str,
    ts: f64,
) -> Option<Stored> {
    let backend = backend?;
    if ts == 0.0 || !gap_free {
        return None;
    }
    if !cache.iter().any(|(k, _)| k == port) {
        let rows = (backend.lookup)(port).unwrap_or_default();
        cache.push((port.to_owned(), rows));
    }
    let rows = &cache.iter().find(|(k, _)| k == port)?.1;
    let found = rows.iter().find(|r| r.first_seen <= ts)?;
    if found.first_seen < backend.continuous_since {
        return None;
    }
    if let Some(seen) = get(by_pid, port) {
        if !seen.model_path.is_empty() && seen.model_path != found.model_path {
            return None;
        }
    }
    Some(found.clone())
}

/// A `db.recent_timings` / `db.timings_by_instance` row.
#[derive(Debug, Clone, PartialEq)]
pub struct Row {
    pub instance: String,
    pub ts: f64,
    pub gen_tps: f64,
    pub prompt_tps: f64,
    pub draft_acc: Option<f64>,
    pub draft_len: Option<f64>,
    pub spec_type: Option<String>,
}

/// `telemetry.Stats`.
#[derive(Debug, Clone, PartialEq)]
pub struct Stats {
    pub n: usize,
    pub gen_p50: f64,
    pub gen_p25: f64,
    pub gen_p75: f64,
    pub prompt_p50: f64,
    pub draft_acc_p50: Option<f64>,
    pub draft_len_p50: Option<f64>,
    pub draft_n: usize,
    pub last_ts: f64,
    pub spec_types: Vec<String>,
    pub single_config: bool,
    pub config_count: usize,
}

impl Default for Stats {
    fn default() -> Self {
        Stats {
            n: 0,
            gen_p50: 0.0,
            gen_p25: 0.0,
            gen_p75: 0.0,
            prompt_p50: 0.0,
            draft_acc_p50: None,
            draft_len_p50: None,
            draft_n: 0,
            last_ts: 0.0,
            spec_types: Vec::new(),
            single_config: true,
            config_count: 1,
        }
    }
}

fn distinct<'a>(items: impl Iterator<Item = &'a str>) -> Vec<&'a str> {
    let mut out: Vec<&str> = Vec::new();
    for i in items {
        if !out.contains(&i) {
            out.push(i);
        }
    }
    out
}

/// `max(values, default=0.0)` with Python's comparison (the first of equal maxima).
fn max_of(values: impl Iterator<Item = f64>) -> f64 {
    let mut best: Option<f64> = None;
    for v in values {
        if best.is_none_or(|b| v > b) {
            best = Some(v);
        }
    }
    best.unwrap_or(0.0)
}

/// `stats_for(...)` over the rows `db.recent_timings` returned (newest first).
pub fn stats_for(rows: &[Row]) -> Stats {
    if rows.is_empty() {
        return Stats::default();
    }
    let all = distinct(rows.iter().map(|r| r.instance.as_str())).len();
    let Some(newest) = rows.first().map(|r| &r.instance) else {
        return Stats::default();
    };
    let current: Vec<&Row> = rows.iter().filter(|r| &r.instance == newest).collect();
    let (rows, single): (Vec<&Row>, bool) = if current.len() >= MIN_SAMPLES_PER_CONFIG {
        (current, true)
    } else {
        (rows.iter().collect(), all <= 1)
    };
    let gen: Vec<f64> = rows
        .iter()
        .map(|r| r.gen_tps)
        .filter(|v| *v != 0.0)
        .collect();
    let prompt: Vec<f64> = rows
        .iter()
        .map(|r| r.prompt_tps)
        .filter(|v| *v != 0.0)
        .collect();
    let acc: Vec<f64> = rows.iter().filter_map(|r| r.draft_acc).collect();
    let dlen: Vec<f64> = rows.iter().filter_map(|r| r.draft_len).collect();
    let mut spec: Vec<String> = Vec::new();
    for r in &rows {
        let s = r.spec_type.clone().unwrap_or_default();
        if !s.is_empty() && !spec.contains(&s) {
            spec.push(s);
        }
    }
    spec.sort();
    Stats {
        n: rows.len(),
        gen_p50: pct(&gen, 0.50),
        gen_p25: pct(&gen, 0.25),
        gen_p75: pct(&gen, 0.75),
        prompt_p50: pct(&prompt, 0.50),
        draft_acc_p50: (!acc.is_empty()).then(|| pct(&acc, 0.50)),
        draft_len_p50: (!dlen.is_empty()).then(|| pct(&dlen, 0.50)),
        draft_n: acc.len(),
        last_ts: max_of(rows.iter().map(|r| r.ts)),
        spec_types: spec,
        single_config: single,
        config_count: distinct(rows.iter().map(|r| r.instance.as_str())).len(),
    }
}

/// `telemetry.ConfigRun`.
#[derive(Debug, Clone, PartialEq)]
pub struct ConfigRun {
    pub instance: String,
    pub n: usize,
    pub gen_p50: f64,
    pub gen_p25: f64,
    pub gen_p75: f64,
    pub prompt_p50: f64,
    pub draft_acc_p50: Option<f64>,
    pub draft_len_p50: Option<f64>,
    pub last_ts: f64,
    pub argv: Dict,
    pub is_current: bool,
    pub diff: Dict,
    pub rel_pct: i64,
}

/// `config_history(...)` over `db.timings_by_instance` rows (newest first) and
/// `db.server_configs_for` (`None`: it raised, which Python treats as `{}`).
pub fn config_history(
    rows: &[Row],
    argvs: Option<&[(String, Dict)]>,
    min_runs: usize,
    min_samples: usize,
) -> Result<Vec<ConfigRun>, Error> {
    if rows.is_empty() {
        return Ok(Vec::new());
    }
    let mut by_instance: Vec<(&str, Vec<&Row>)> = Vec::new();
    for r in rows {
        match by_instance.iter_mut().find(|(k, _)| *k == r.instance) {
            Some((_, rs)) => rs.push(r),
            None => by_instance.push((&r.instance, vec![r])),
        }
    }
    let empty = Dict::new();
    let complete = |a: &Dict| {
        dict_get(a, "--model").is_some_and(|v| !v.is_empty())
            || dict_get(a, "-m").is_some_and(|v| !v.is_empty())
    };
    struct Fold<'a> {
        sig: Vec<(&'a str, &'a str)>,
        argv: &'a Dict,
        rows: Vec<&'a Row>,
        instances: Vec<&'a str>,
    }
    let mut folded: Vec<Fold<'_>> = Vec::new();
    for (inst, rs) in &by_instance {
        let argv = argvs
            .and_then(|a| a.iter().find(|(k, _)| k == inst).map(|(_, v)| v))
            .unwrap_or(&empty);
        if !complete(argv) {
            continue;
        }
        let mut sig: Vec<(&str, &str)> =
            argv.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
        sig.sort();
        match folded.iter_mut().find(|f| f.sig == sig) {
            Some(f) => {
                f.rows.extend(rs.iter().copied());
                f.instances.push(inst);
            }
            None => folded.push(Fold {
                sig,
                argv,
                rows: rs.clone(),
                instances: vec![inst],
            }),
        }
    }
    let newest = rows.first().map_or("", |r| r.instance.as_str());
    let mut runs: Vec<ConfigRun> = Vec::new();
    for f in &folded {
        let rs = &f.rows;
        if rs.len() < min_samples {
            continue;
        }
        let gen: Vec<f64> = rs.iter().map(|r| r.gen_tps).filter(|v| *v != 0.0).collect();
        let prompt: Vec<f64> = rs
            .iter()
            .map(|r| r.prompt_tps)
            .filter(|v| *v != 0.0)
            .collect();
        let acc: Vec<f64> = rs.iter().filter_map(|r| r.draft_acc).collect();
        let dl: Vec<f64> = rs.iter().filter_map(|r| r.draft_len).collect();
        runs.push(ConfigRun {
            instance: f.instances.join(","),
            n: rs.len(),
            gen_p50: pct(&gen, 0.50),
            gen_p25: pct(&gen, 0.25),
            gen_p75: pct(&gen, 0.75),
            prompt_p50: pct(&prompt, 0.50),
            draft_acc_p50: (!acc.is_empty()).then(|| pct(&acc, 0.50)),
            draft_len_p50: (!dl.is_empty()).then(|| pct(&dl, 0.50)),
            last_ts: max_of(rs.iter().map(|r| r.ts)),
            argv: f.argv.clone(),
            is_current: f.instances.contains(&newest),
            diff: Dict::new(),
            rel_pct: 0,
        });
    }
    if runs.len() < min_runs {
        return Ok(Vec::new());
    }
    let keys = {
        let argvs: Vec<&Dict> = runs.iter().map(|r| &r.argv).collect();
        differing_keys(&argvs)
    };
    for r in &mut runs {
        r.diff = keys
            .iter()
            .map(|k| {
                (
                    k.clone(),
                    dict_get(&r.argv, k).unwrap_or("unset").to_owned(),
                )
            })
            .collect();
    }
    let best = max_of(runs.iter().map(|r| r.gen_p50));
    for r in &mut runs {
        r.rel_pct = if best != 0.0 {
            let x = (100.0 * r.gen_p50 / best).round_ties_even();
            // `int(round(nan or inf))` raises.
            if !x.is_finite() || x.abs() >= 9.0e18 {
                return Err(Error::Raised("ValueError: cannot convert float to integer"));
            }
            x as i64
        } else {
            0
        };
    }
    // A stable sort, newest first: equal times keep their order, as `sort(reverse=True)` keeps it.
    runs.sort_by(|a, b| {
        b.last_ts
            .partial_cmp(&a.last_ts)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    Ok(runs)
}

#[cfg(test)]
mod tests;
