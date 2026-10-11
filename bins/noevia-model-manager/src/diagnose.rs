//! Pure current-run diagnosis. No transport, state access or route registration.
//! Logical admission bounds do not promise a hard RSS limit or OOM recovery.
use crate::Redactor;
use regex::{Captures, Regex};
use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    io::{self, Write},
    sync::atomic::{AtomicBool, Ordering},
    time::Instant,
};

pub const LOG_CAP: usize = 8 * 1024 * 1024;
pub const LINE_CAP: usize = 16 * 1024;
pub const LINE_COUNT: usize = 80_000;
pub const ENTRY_CAP: usize = 1024;
pub const RETAINED_CAP: usize = 8 * 1024 * 1024;
pub const REDACTED_CAP: usize = 16 * 1024 * 1024;
pub const JSON_CAP: usize = 4 * 1024 * 1024;
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    LogCap,
    LineCap,
    LineCount,
    EntryCap,
    NameCap,
    PortCap,
    StatusDialect,
    RetainedCap,
    RedactedCap,
    JsonCap,
    Cancelled,
    Deadline,
    Allocation,
    Internal,
}
/// The caller supplies one absolute deadline and cancellation flag for the entire job.
pub struct Work<'a> {
    pub deadline: Instant,
    pub cancelled: &'a AtomicBool,
}
impl Work<'_> {
    fn check(&self) -> Result<(), Refusal> {
        if self.cancelled.load(Ordering::Relaxed) {
            Err(Refusal::Cancelled)
        } else if Instant::now() >= self.deadline {
            Err(Refusal::Deadline)
        } else {
            Ok(())
        }
    }
}
struct Rule {
    id: &'static str,
    title: &'static str,
    advice: &'static str,
    patterns: Vec<Regex>,
}
const RULES: &[(&str, &str, &[&str], &str)] = &[
("kv-cache", "The KV cache did not fit", &[r"failed to allocate.*kv",r"kv.?cache.*(alloc|fail)",r"llama_kv_cache.*failed"], "Lower the context size or parallel slots, or quantize the cache (cache-type-k/v = q8_0)."),
("gpu-memory", "The GPU ran out of memory", &[r"out of (device )?memory",r"ErrorOutOfDeviceMemory",r"Device memory allocation of size .* failed",r"cudaMalloc failed",r"unable to allocate .*buffer",r"failed to allocate .*buffer",r"hipMalloc.*fail"], "Lower the context size or GPU layers, remove the vision projector if you do not need it, or set fit = on so llama.cpp places layers itself."),
("missing-file", "A model file is missing", &[r"failed to open",r"No such file or directory",r"model file not found",r"mmproj.*(not found|failed to load)"], "Check the model = and mmproj = paths in this model's settings; the file may have been moved or deleted."),
("corrupt-file", "The model file is corrupt or incomplete", &[r"invalid magic",r"failed to read (the )?(magic|header|tensor)",r"tensor .* data is not within the file bounds",r"file is truncated",r"unexpected end of file"], "The download is probably incomplete. Delete the file and download it again."),
("architecture", "This llama.cpp build does not support the model", &[r"unknown model architecture",r"unsupported model architecture",r"unknown (pre-tokenizer|tokenizer) type"], "Use a newer llama.cpp image, or choose a model whose architecture this build supports."),
("bad-option", "A setting is not valid for this llama.cpp build", &[r"error: (invalid|unknown) argument",r"unknown argument",r"invalid value for"], "Remove or correct the setting named in the log; it may be misspelled or unsupported by this build."),
("port", "The server port is already in use", &[r"address already in use",r"couldn't bind",r"failed to bind"], "Another process holds the port. Restart the llama.cpp container."),
("context", "The context exceeds what the model was trained for", &[r"exceeds the (model's )?(training|trained) context",r"n_ctx_train"], "Reduce the context size to the model's trained length or configure RoPE scaling deliberately."),
];
// Python re.I adds these ASCII letter equivalents. Patterns here are closed literals
// and regex operators, with no escapes/classes containing these letters.
fn insensitive(pattern: &str) -> String {
    let mut p = String::from("(?i)");
    for c in pattern.chars() {
        match c {
            'i' | 'I' => p.push_str("[iİı]"),
            's' | 'S' => p.push_str("[sſ]"),
            'k' | 'K' => p.push_str("[kK]"),
            _ => p.push(c),
        }
    }
    p
}
#[derive(Default)]
struct Output<'a> {
    hits: Vec<VecDeque<&'a str>>,
    errors: VecDeque<&'a str>,
    tail: VecDeque<&'a str>,
    loaded: bool,
}
fn ring<'a>(ring: &mut VecDeque<&'a str>, line: &'a str) -> Result<(), Refusal> {
    if ring.len() == 4 {
        ring.pop_front();
    }
    ring.try_reserve(1).map_err(|_| Refusal::Allocation)?;
    ring.push_back(line);
    Ok(())
}
struct Failure<'a> {
    model: &'a str,
    port: &'a str,
    status: i64,
    rule: Option<usize>,
    evidence: Vec<&'a str>,
}
/// Only safe, already-redacted response bytes are exposed. Raw evidence has no public
/// serialization implementation and never leaves analyse_json.
pub struct Analyzer {
    rules: Vec<Rule>,
    prefix: Regex,
    spawn: Regex,
    forced: Regex,
    exit: Regex,
    error: Regex,
    word: Regex,
    redactor: Redactor,
}
impl Analyzer {
    pub fn new() -> Result<Self, regex::Error> {
        let rules = RULES
            .iter()
            .map(|(id, title, patterns, advice)| {
                Ok(Rule {
                    id,
                    title,
                    advice,
                    patterns: patterns
                        .iter()
                        .map(|p| Regex::new(&insensitive(p)))
                        .collect::<Result<_, _>>()?,
                })
            })
            .collect::<Result<_, regex::Error>>()?;
        Ok(Self {
            rules,
            prefix: Regex::new(r"^\[(\p{Nd}+)\][\s\x1c-\x1f]?(.*)$")?,
            spawn: Regex::new(
                r"spawning server instance with name=([^\s\x1c-\x1f]+) on port (\p{Nd}+)",
            )?,
            forced: Regex::new(
                r"(?:force-killing model instance name=([^\s\x1c-\x1f]+))|(?:unload: stopping model instance name=([^\s\x1c-\x1f]+))",
            )?,
            exit: Regex::new(r"instance name=([^\s\x1c-\x1f]+) exited with status (-?\p{Nd}+)")?,
            error: Regex::new(&insensitive("(error|failed|fatal|abort)"))?,
            word: Regex::new(r"^[\p{L}\p{N}_]$")?,
            redactor: Redactor::new()?,
        })
    }
    fn error_hit(&self, line: &str) -> bool {
        self.error.find_iter(line).any(|m| {
            line.get(..m.start())
                .and_then(|s| s.chars().next_back())
                .is_none_or(|c| !self.word.is_match(c.encode_utf8(&mut [0; 4])))
        })
    }
    pub fn analyse_json(&self, text: &str, work: &Work<'_>) -> Result<Vec<u8>, Refusal> {
        work.check()?;
        if text.len() > LOG_CAP {
            return Err(Refusal::LogCap);
        }
        // Validate all lines before redaction or state allocation.
        let mut count = 0;
        for (line, _) in lines(text) {
            work.check()?;
            count += 1;
            if count > LINE_COUNT {
                return Err(Refusal::LineCount);
            }
            if line.len() > LINE_CAP {
                return Err(Refusal::LineCap);
            }
        }
        let whole = self.redact(text, work)?;
        let mut per_line = String::new();
        for (line, delimiter) in lines(text) {
            append(&mut per_line, &self.redact(line, work)?, REDACTED_CAP)?;
            append(&mut per_line, delimiter, REDACTED_CAP)?;
        }
        let suppress = whole != per_line;
        drop(whole);
        drop(per_line);
        let mut ports: BTreeMap<&str, Output<'_>> = BTreeMap::new();
        let mut latest: BTreeMap<&str, &str> = BTreeMap::new();
        let mut requested = BTreeSet::new();
        let mut names = BTreeSet::new();
        let mut failures: Vec<Failure<'_>> = Vec::new();
        let mut snapshot_bytes = 0usize;
        for (raw, _) in lines(text) {
            work.check()?;
            if let Some(c) = self.prefix.captures(raw) {
                let port = capture(&c, 1)?;
                admit_port(port, &ports)?;
                let line = capture(&c, 2)?;
                let out = ports.entry(port).or_default();
                if out.hits.is_empty() {
                    out.hits = self.rules.iter().map(|_| VecDeque::new()).collect();
                }
                for (rule, hits) in self.rules.iter().zip(&mut out.hits) {
                    if rule.patterns.iter().any(|p| p.is_match(line)) {
                        ring(hits, line)?;
                    }
                }
                if self.error_hit(line) {
                    ring(&mut out.errors, line)?;
                }
                ring(&mut out.tail, line)?;
                out.loaded |= line.contains("model loaded");
                continue;
            }
            if let Some(c) = self.spawn.captures(raw) {
                let name = capture(&c, 1)?;
                let port = capture(&c, 2)?;
                admit_name(name, &mut names, &mut snapshot_bytes)?;
                admit_port(port, &ports)?;
                latest.insert(name, port);
                ports.insert(port, Output::default());
                requested.remove(name);
                continue;
            }
            if let Some(c) = self.forced.captures(raw) {
                let name = c
                    .get(1)
                    .or_else(|| c.get(2))
                    .ok_or(Refusal::Internal)?
                    .as_str();
                admit_name(name, &mut names, &mut snapshot_bytes)?;
                if requested.len() >= ENTRY_CAP && !requested.contains(name) {
                    return Err(Refusal::EntryCap);
                }
                requested.insert(name);
                continue;
            }
            if let Some(c) = self.exit.captures(raw) {
                let name = capture(&c, 1)?;
                admit_name(name, &mut names, &mut snapshot_bytes)?;
                let raw_status = capture(&c, 2)?;
                if !raw_status.bytes().all(|b| b.is_ascii_digit() || b == b'-') {
                    return Err(Refusal::StatusDialect);
                }
                let status = raw_status
                    .parse::<i64>()
                    .map_err(|_| Refusal::StatusDialect)?;
                let port = latest.get(name).copied().unwrap_or("");
                if status == 0 || requested.contains(name) {
                    requested.remove(name);
                    continue;
                }
                let out = ports.get(port);
                let rule = out.and_then(|o| o.hits.iter().position(|hits| !hits.is_empty()));
                let selected = if let Some(out) = out {
                    if let Some(r) = rule {
                        Some(out.hits.get(r).ok_or(Refusal::Internal)?)
                    } else if !out.errors.is_empty() {
                        Some(&out.errors)
                    } else {
                        Some(&out.tail)
                    }
                } else {
                    None
                };
                // Charge snapshot bytes before allocating even borrowed evidence slots.
                // Overwriting releases that slot's previous logical charge.
                if let Some(old) = failures.iter().find(|f| f.model == name) {
                    snapshot_bytes = snapshot_bytes
                        .checked_sub(
                            old.model.len() + old.evidence.iter().map(|s| s.len()).sum::<usize>(),
                        )
                        .ok_or(Refusal::Internal)?;
                }
                charge(&mut snapshot_bytes, name.len())?;
                let mut evidence = Vec::new();
                if let Some(selected) = selected {
                    for line in selected {
                        charge(&mut snapshot_bytes, line.len())?;
                    }
                    evidence
                        .try_reserve(selected.len())
                        .map_err(|_| Refusal::Allocation)?;
                    evidence.extend(selected.iter().copied());
                }
                let f = Failure {
                    model: name,
                    port,
                    status,
                    rule,
                    evidence,
                };
                if let Some(old) = failures.iter_mut().find(|f| f.model == name) {
                    *old = f;
                } else {
                    failures.try_reserve(1).map_err(|_| Refusal::Allocation)?;
                    failures.push(f);
                }
            }
        }
        failures.retain(|f| {
            let port = latest.get(f.model).copied().unwrap_or("");
            !(!port.is_empty() && port != f.port && ports.get(port).is_some_and(|o| o.loaded))
        });
        let mut retained = 0usize;
        let mut writer = JsonWriter {
            bytes: Vec::new(),
            work,
        };
        writer
            .write_all(b"{\"failures\":[")
            .map_err(|_| writer.reason())?;
        for (i, f) in failures.iter().rev().enumerate() {
            work.check()?;
            if i > 0 {
                writer.write_all(b",").map_err(|_| writer.reason())?;
            }
            let (cause, title, advice) = if let Some(r) = f.rule {
                let r = self.rules.get(r).ok_or(Refusal::Internal)?;
                (r.id, r.title, r.advice)
            } else {
                (
                    "unknown",
                    "The model stopped for a reason noevia does not recognise",
                    "Read the log lines below; they are the last output before the model stopped.",
                )
            };
            let mut evidence = Vec::new();
            charge(&mut retained, f.model.len())?;
            if !suppress {
                for line in &f.evidence {
                    let safe = self.redact(line, work)?;
                    charge(&mut retained, safe.len())?;
                    evidence.try_reserve(1).map_err(|_| Refusal::Allocation)?;
                    evidence.push(safe);
                }
                // Selection must not create fresh contextual credential exposure.
                let mut block = String::new();
                for (j, line) in evidence.iter().enumerate() {
                    if j > 0 {
                        append(&mut block, "\n", REDACTED_CAP)?;
                    }
                    append(&mut block, line, REDACTED_CAP)?;
                }
                if self.redact(&block, work)? != block {
                    evidence.clear();
                }
            }
            // Value contains only bounded static fields, admitted model and scrubbed evidence.
            serde_json::to_writer(&mut writer,&serde_json::json!({"model":f.model,"status":f.status,"cause":cause,"title":title,"advice":advice,"evidence":evidence})).map_err(|_|writer.reason())?;
        }
        writer.write_all(b"]}").map_err(|_| writer.reason())?;
        work.check()?;
        Ok(writer.bytes)
    }
    // Consume the existing Redactor's exact compiled patterns and replacement policy.
    // No replace_all result is built before a bound check.
    fn redact(&self, input: &str, work: &Work<'_>) -> Result<String, Refusal> {
        if input.len() > LOG_CAP {
            return Err(Refusal::LogCap);
        }
        let mut text = String::new();
        append(&mut text, input, REDACTED_CAP)?;
        for (index, (pattern, replacement)) in self.redactor.patterns.iter().enumerate() {
            if index == 2 {
                text = replace(&text, &self.redactor.keys, None, work)?;
            }
            text = replace(&text, pattern, Some(replacement), work)?;
        }
        Ok(text)
    }
}
fn capture<'a>(c: &Captures<'a>, n: usize) -> Result<&'a str, Refusal> {
    c.get(n).map(|m| m.as_str()).ok_or(Refusal::Internal)
}
fn admit_name<'a>(
    name: &'a str,
    names: &mut BTreeSet<&'a str>,
    retained: &mut usize,
) -> Result<(), Refusal> {
    if name.len() > 1024 {
        return Err(Refusal::NameCap);
    }
    if !names.contains(name) && names.len() >= ENTRY_CAP {
        return Err(Refusal::EntryCap);
    }
    if !names.contains(name) {
        charge(retained, name.len())?;
        names.insert(name);
    }
    Ok(())
}
fn admit_port(port: &str, ports: &BTreeMap<&str, Output<'_>>) -> Result<(), Refusal> {
    if port.len() > 64 {
        return Err(Refusal::PortCap);
    }
    if !ports.contains_key(port) && ports.len() >= ENTRY_CAP {
        return Err(Refusal::EntryCap);
    }
    Ok(())
}
fn charge(total: &mut usize, n: usize) -> Result<(), Refusal> {
    *total = total
        .checked_add(n)
        .filter(|v| *v <= RETAINED_CAP)
        .ok_or(Refusal::RetainedCap)?;
    Ok(())
}
fn append(out: &mut String, s: &str, cap: usize) -> Result<(), Refusal> {
    if out.len().checked_add(s.len()).is_none_or(|n| n > cap) {
        return Err(Refusal::RedactedCap);
    }
    out.try_reserve(s.len()).map_err(|_| Refusal::Allocation)?;
    out.push_str(s);
    Ok(())
}
fn replace(
    text: &str,
    pattern: &Regex,
    replacement: Option<&&str>,
    work: &Work<'_>,
) -> Result<String, Refusal> {
    let mut out = String::new();
    let mut cursor = 0;
    for c in pattern.captures_iter(text) {
        work.check()?;
        let m = c.get(0).ok_or(Refusal::Internal)?;
        append(
            &mut out,
            text.get(cursor..m.start()).ok_or(Refusal::Internal)?,
            REDACTED_CAP,
        )?;
        if let Some(replacement) = replacement {
            match *replacement {
                "${1}[redacted]" => {
                    append(&mut out, capture(&c, 1)?, REDACTED_CAP)?;
                    append(&mut out, "[redacted]", REDACTED_CAP)?;
                }
                "${1}[redacted]@" => {
                    append(&mut out, capture(&c, 1)?, REDACTED_CAP)?;
                    append(&mut out, "[redacted]@", REDACTED_CAP)?;
                }
                "[redacted]" => append(&mut out, "[redacted]", REDACTED_CAP)?,
                _ => return Err(Refusal::Internal),
            }
        } else {
            let key = capture(&c, 2)?;
            let value = capture(&c, 3)?;
            // Compare Python/Rust lowercase policy without materializing an input-sized
            // lowercase temporary. The old Redactor's counter behavior is unchanged.
            let equals =
                |literal: &str| key.chars().flat_map(char::to_lowercase).eq(literal.chars());
            let counter = (key
                .get(..2)
                .is_some_and(|prefix| prefix.eq_ignore_ascii_case("n_"))
                && key.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'_'))
                || equals("tokens")
                || equals("token_count")
                || (equals("session") && value.chars().all(|c| c.is_ascii_digit() || c == '.'));
            if counter {
                append(&mut out, m.as_str(), REDACTED_CAP)?;
            } else {
                append(&mut out, capture(&c, 1)?, REDACTED_CAP)?;
                append(&mut out, "[redacted]", REDACTED_CAP)?;
            }
        }
        cursor = m.end();
    }
    append(
        &mut out,
        text.get(cursor..).ok_or(Refusal::Internal)?,
        REDACTED_CAP,
    )?;
    work.check()?;
    Ok(out)
}
fn separator(c: char) -> bool {
    matches!(
        c,
        '\n' | '\r'
            | '\u{b}'
            | '\u{c}'
            | '\u{1c}'
            | '\u{1d}'
            | '\u{1e}'
            | '\u{85}'
            | '\u{2028}'
            | '\u{2029}'
    )
}
/// Borrow complete Python splitlines lines with their original delimiters.
fn lines(text: &str) -> impl Iterator<Item = (&str, &str)> {
    let mut start = 0;
    std::iter::from_fn(move || {
        if start >= text.len() {
            return None;
        }
        let remaining = text.get(start..)?;
        for (offset, c) in remaining.char_indices() {
            if separator(c) {
                let end = start + offset;
                let mut next = end + c.len_utf8();
                if c == '\r' && text.get(next..).is_some_and(|s| s.starts_with('\n')) {
                    next += 1;
                }
                let pair = (text.get(start..end)?, text.get(end..next)?);
                start = next;
                return Some(pair);
            }
        }
        start = text.len();
        Some((remaining, ""))
    })
}
struct JsonWriter<'a, 'b> {
    bytes: Vec<u8>,
    work: &'a Work<'b>,
}
impl JsonWriter<'_, '_> {
    fn reason(&self) -> Refusal {
        self.work.check().err().unwrap_or(Refusal::JsonCap)
    }
}
impl Write for JsonWriter<'_, '_> {
    fn write(&mut self, b: &[u8]) -> io::Result<usize> {
        self.work
            .check()
            .map_err(|_| io::Error::other("diagnosis work stopped"))?;
        if self
            .bytes
            .len()
            .checked_add(b.len())
            .is_none_or(|n| n > JSON_CAP)
        {
            return Err(io::Error::other("diagnosis JSON cap"));
        }
        self.bytes.try_reserve(b.len()).map_err(io::Error::other)?;
        self.bytes.extend_from_slice(b);
        Ok(b.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
