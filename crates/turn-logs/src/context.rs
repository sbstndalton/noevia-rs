//! core `server/context-log.cjs` ("measure first"): per-round context accounting, opt-in with
//! `CONTEXT_LOG=1`. A record holds estimated token counts and tool NAMES only (never message
//! text, tool arguments or results) and the chat id as a 16-hex sha256 prefix, in the tenant's
//! own directory, bounded in size. [`report`] ranks what consumes context.
use crate::{parse_line, prop_key};
use context_fit::tokens;
use js_json::{wtf16, JValue};
use sha2::{Digest, Sha256};
use std::fs::{self, DirBuilder, OpenOptions};
use std::io::Write as _;
use std::os::unix::fs::{DirBuilderExt as _, OpenOptionsExt as _};
use std::path::Path;

/// core `MAX_BYTES`: the current file rotates to `.1` once it is larger than this.
pub const MAX_BYTES: u64 = 2 * 1024 * 1024;
/// core `SUMMARY_PREFIX`: an assistant message that is the saved summary of older turns.
pub const SUMMARY_PREFIX: &str = "Earlier conversation summary";
/// The file in the tenant's directory.
pub const FILE_NAME: &str = "context-log.jsonl";

/// core `enabled(env)`: exactly `CONTEXT_LOG=1`.
pub fn enabled(get: impl Fn(&str) -> Option<String>) -> bool {
    get("CONTEXT_LOG").as_deref() == Some("1")
}

/// `call.function?.name || 'unknown'`.
fn call_name(call: &JValue) -> JValue {
    let name = call.get("function").opt("name");
    if name.truthy() {
        name.clone()
    } else {
        JValue::Str("unknown".into())
    }
}

/// `x || []` for a list core iterates with `for of` (a non-array truthy value throws there; it
/// counts as empty here).
fn list(v: &JValue) -> &[JValue] {
    match v {
        JValue::Arr(items) => items,
        _ => &[],
    }
}

/// JS `Map` key equality (SameValueZero) for the JSON values a tool call id can be: primitives by
/// value; two parsed objects or arrays are never the same key.
fn same_key(a: &JValue, b: &JValue) -> bool {
    match (a, b) {
        (JValue::Num(x), JValue::Num(y)) => x == y || (x.is_nan() && y.is_nan()),
        (JValue::Str(x), JValue::Str(y)) => x == y,
        (JValue::Bool(x), JValue::Bool(y)) => x == y,
        (JValue::Null, JValue::Null) | (JValue::Undefined, JValue::Undefined) => true,
        _ => false,
    }
}

/// core `breakdown(messages, tools)`.
#[derive(Debug, Clone, PartialEq)]
pub struct Breakdown {
    pub system: u64,
    pub history: u64,
    pub summary: u64,
    /// Tool name (as an object key) to its schema tokens, in JS property order.
    pub tool_schemas: Vec<(String, u64)>,
    /// `{ name, tokens }` per tool message, the name taken from the call that asked for it.
    pub tool_results: Vec<(JValue, u64)>,
    /// The names of the calls, in order.
    pub tool_sequence: Vec<JValue>,
}

impl Breakdown {
    fn schema_total(&self) -> u64 {
        self.tool_schemas.iter().map(|(_, n)| n).sum()
    }
    fn result_total(&self) -> u64 {
        self.tool_results.iter().map(|(_, n)| n).sum()
    }
}

/// core `breakdown`: system, saved summary, other history, schema tokens per tool name, result
/// tokens per tool message (named by the call that asked for it, else `unknown`), and the call
/// sequence. A message that is not an object is skipped (core throws on `null`).
pub fn breakdown(messages: &[JValue], tools: &[JValue]) -> Breakdown {
    let mut names_by_id: Vec<(JValue, JValue)> = Vec::new();
    for m in messages {
        for call in list(m.get("tool_calls")) {
            let id = call.get("id").clone();
            let name = call_name(call);
            match names_by_id.iter_mut().find(|(k, _)| same_key(k, &id)) {
                Some(slot) => slot.1 = name,
                None => names_by_id.push((id, name)),
            }
        }
    }
    let mut out = Breakdown {
        system: 0,
        history: 0,
        summary: 0,
        tool_schemas: Vec::new(),
        tool_results: Vec::new(),
        tool_sequence: Vec::new(),
    };
    for m in messages {
        if m.is_nullish() {
            continue;
        }
        let size = tokens(m);
        let role = m.get("role").as_str();
        if role == Some("system") {
            out.system += size;
        } else if role == Some("tool") {
            let id = m.get("tool_call_id");
            let name = names_by_id
                .iter()
                .find(|(k, _)| same_key(k, id))
                .map(|(_, n)| n.clone())
                .unwrap_or_else(|| JValue::Str("unknown".into()));
            out.tool_results.push((name, size));
        } else if role == Some("assistant")
            && m.get("content").as_str().is_some_and(|c| c.starts_with(SUMMARY_PREFIX))
        {
            out.summary += size;
        } else {
            out.history += size;
        }
        for call in list(m.get("tool_calls")) {
            out.tool_sequence.push(call_name(call));
        }
    }
    for tool in tools {
        let Some(name) = prop_key(&call_name(tool)) else { continue };
        let size = tokens(tool);
        let mut map: Vec<(String, JValue)> = out
            .tool_schemas
            .iter()
            .map(|(k, n)| (k.clone(), JValue::Num(*n as f64)))
            .collect();
        let prev = out.tool_schemas.iter().find(|(k, _)| *k == name).map_or(0, |(_, n)| *n);
        wtf16::set(&mut map, &name, JValue::Num((prev + size) as f64));
        out.tool_schemas = map
            .into_iter()
            .map(|(k, v)| (k, v.as_num().map_or(0, |n| n as u64)))
            .collect();
    }
    out
}

/// What [`record`] is given (core `record({ chatId, model, limit, round, compacted, messages,
/// tools, now })`); `model`, `limit` and `round` are copied as they are (`undefined` drops out).
pub struct Round<'a> {
    pub chat_id: &'a JValue,
    pub model: &'a JValue,
    pub limit: &'a JValue,
    pub round: &'a JValue,
    pub compacted: bool,
    pub messages: &'a [JValue],
    pub tools: &'a [JValue],
    /// `Date.now()` in milliseconds.
    pub now: f64,
}

/// `sha256(String(chatId))` hex, first 16 characters.
pub fn chat_hash(chat_id: &JValue) -> String {
    let text = js_json::to_js_string(chat_id).unwrap_or_else(|_| "[object Object]".into());
    let mut out = String::with_capacity(64);
    for b in Sha256::digest(text.as_bytes()) {
        out.push_str(&format!("{b:02x}"));
    }
    out.chars().take(16).collect()
}

/// core `record(...)`: the entry, in core's key order: at, chat, model, limit, round, compacted,
/// total, system, history, summary, toolSchemas, toolResults, toolSequence.
pub fn record(r: &Round<'_>) -> JValue {
    let parts = breakdown(r.messages, r.tools);
    let total = parts.system + parts.history + parts.summary + parts.schema_total() + parts.result_total();
    let num = |n: u64| JValue::Num(n as f64);
    JValue::obj([
        ("at", JValue::Num(r.now)),
        ("chat", JValue::Str(chat_hash(r.chat_id))),
        ("model", r.model.clone()),
        ("limit", r.limit.clone()),
        ("round", r.round.clone()),
        ("compacted", JValue::Bool(r.compacted)),
        ("total", num(total)),
        ("system", num(parts.system)),
        ("history", num(parts.history)),
        ("summary", num(parts.summary)),
        (
            "toolSchemas",
            JValue::Obj(parts.tool_schemas.iter().map(|(k, n)| (k.clone(), num(*n))).collect()),
        ),
        (
            "toolResults",
            JValue::Arr(
                parts
                    .tool_results
                    .iter()
                    .map(|(name, n)| JValue::obj([("name", name.clone()), ("tokens", num(*n))]))
                    .collect(),
            ),
        ),
        ("toolSequence", JValue::Arr(parts.tool_sequence.clone())),
    ])
}

/// core `append(dir, entry)`: the directory is made private (0700) if missing, a file already over
/// [`MAX_BYTES`] is rotated to `.1` first, and the line is appended to a 0600 file. Errors are the
/// caller's (core's `logRound` warns and goes on).
pub fn append(dir: &Path, entry: &JValue) -> std::io::Result<()> {
    let file = dir.join(FILE_NAME);
    DirBuilder::new().recursive(true).mode(0o700).create(dir)?;
    if let Ok(meta) = fs::metadata(&file) {
        if meta.len() > MAX_BYTES {
            fs::rename(&file, crate::decision::rotated(&file))?;
        }
    }
    let mut line = wtf16::stringify(entry).unwrap_or_else(|| "undefined".into());
    line.push('\n');
    let mut f = OpenOptions::new().append(true).create(true).mode(0o600).open(&file)?;
    f.write_all(line.as_bytes())
}

/// `x || 0` for a number field of a parsed line (a non-number counts as 0).
fn num_or_zero(v: &JValue) -> f64 {
    match v.as_num() {
        Some(n) if !n.is_nan() => n,
        _ => 0.0,
    }
}

struct ToolRow {
    name: JValue,
    schema: f64,
    result: f64,
    results: f64,
}

struct SeqRow {
    sequence: String,
    count: f64,
    result: f64,
}

fn row<'a>(tools: &'a mut Vec<ToolRow>, name: &JValue) -> Option<&'a mut ToolRow> {
    if !tools.iter().any(|t| same_key(&t.name, name)) {
        tools.push(ToolRow { name: name.clone(), schema: 0.0, result: 0.0, results: 0.0 });
    }
    tools.iter_mut().find(|t| same_key(&t.name, name))
}

/// `Array.prototype.join(' → ')`: `null`/`undefined` as empty text, the rest `String(v)`.
fn join_sequence(items: &[JValue]) -> String {
    items
        .iter()
        .map(|v| {
            if v.is_nullish() {
                String::new()
            } else {
                js_json::to_js_string(v).unwrap_or_else(|_| "[object Object]".into())
            }
        })
        .collect::<Vec<_>>()
        .join(" → ")
}

/// core `report(lines)`: rounds, compactions, the median total, tools ranked by result + schema
/// tokens, and recurring tool sequences (per round) ranked by the result tokens they produced, then
/// by count. Lines that are not JSON are skipped (and `null`, where core throws).
pub fn report<'a>(lines: impl IntoIterator<Item = &'a str>) -> JValue {
    let mut tools: Vec<ToolRow> = Vec::new();
    let mut seqs: Vec<SeqRow> = Vec::new();
    let (mut rounds, mut compactions) = (0.0, 0.0);
    let mut totals: Vec<f64> = Vec::new();
    for line in lines {
        let Some(entry) = parse_line(line) else { continue };
        rounds += 1.0;
        if entry.get("compacted").truthy() {
            compactions += 1.0;
        }
        totals.push(num_or_zero(entry.get("total")));
        if let JValue::Obj(schemas) = entry.get("toolSchemas") {
            for (name, value) in schemas {
                if let Some(r) = row(&mut tools, &JValue::Str(name.clone())) {
                    r.schema += num_or_zero(value);
                }
            }
        }
        let results = list(entry.get("toolResults"));
        for result in results {
            if result.is_nullish() {
                continue;
            }
            if let Some(r) = row(&mut tools, result.get("name")) {
                r.result += num_or_zero(result.get("tokens"));
                r.results += 1.0;
            }
        }
        let sequence = list(entry.get("toolSequence"));
        if !sequence.is_empty() {
            let key = join_sequence(sequence);
            let produced: f64 = results.iter().filter(|r| !r.is_nullish()).map(|r| num_or_zero(r.get("tokens"))).sum();
            match seqs.iter_mut().find(|s| s.sequence == key) {
                Some(s) => {
                    s.count += 1.0;
                    s.result += produced;
                }
                None => seqs.push(SeqRow { sequence: key, count: 1.0, result: produced }),
            }
        }
    }
    totals.sort_by(f64::total_cmp);
    let median = totals.get(totals.len() / 2).copied().unwrap_or(0.0);
    tools.sort_by(|a, b| (b.result + b.schema).total_cmp(&(a.result + a.schema)));
    seqs.sort_by(|a, b| b.result.total_cmp(&a.result).then(b.count.total_cmp(&a.count)));
    JValue::obj([
        ("rounds", JValue::Num(rounds)),
        ("compactions", JValue::Num(compactions)),
        ("medianTotal", JValue::Num(median)),
        (
            "tools",
            JValue::Arr(
                tools
                    .into_iter()
                    .map(|t| {
                        JValue::obj([
                            ("name", t.name),
                            ("schemaTokens", JValue::Num(t.schema)),
                            ("resultTokens", JValue::Num(t.result)),
                            ("results", JValue::Num(t.results)),
                        ])
                    })
                    .collect(),
            ),
        ),
        (
            "sequences",
            JValue::Arr(
                seqs.into_iter()
                    .map(|s| {
                        JValue::obj([
                            ("sequence", JValue::Str(s.sequence)),
                            ("count", JValue::Num(s.count)),
                            ("resultTokens", JValue::Num(s.result)),
                        ])
                    })
                    .collect(),
            ),
        ),
    ])
}
