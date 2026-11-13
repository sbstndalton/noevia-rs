//! Retrieval trace data model (port of `diary/agent/retrieval_trace.py`, stages 1 and 2 only).
//!
//! A trace carries ids, scores, counts and budget figures only. Never entry text, entry headers,
//! embeddings, the query or credentials: `EntryRef` keeps the id, the day and the leading `HH:MM`
//! of the header, nothing else.
//!
//! Not ported here: the operator and per-request switch (`trace_requested`, an I/O wrapper), the
//! entity-focus stage (`set_focus`, stage 1b), and the `SafeTrace` wrapper. Rust returns errors
//! explicitly, so the `failed` flag is kept as a plain field and `attach` drops a failed trace the
//! same way the Python does.

use serde_json::{json, Map, Value};

/// Trace schema version written in every trace (`SCHEMA_VERSION` in Python).
pub const SCHEMA_VERSION: u64 = 1;

/// Entry reference copied into the trace: `id`, `day` and the leading clock time of the header.
#[derive(Debug, Clone, PartialEq)]
pub struct EntryRef {
    pub id: Value,
    pub day: Value,
    pub time: Option<String>,
}

impl EntryRef {
    /// Build the reference from an entry row (`_ref` in Python). Missing fields become `null`.
    ///
    /// The header is entry text: only its leading `HH:MM` is kept. A header that is not a string
    /// never matches the pattern in Python either (its `str()` form starts with a bracket or a
    /// letter), so it gives `None` here too.
    pub fn from_row(row: &Value) -> Self {
        let header = row.get("header").and_then(Value::as_str).unwrap_or("");
        Self {
            id: row.get("id").cloned().unwrap_or(Value::Null),
            day: row.get("day").cloned().unwrap_or(Value::Null),
            time: clock_time(header),
        }
    }

    /// The reference as the trace stores it: `id`, `day` and `time`.
    pub fn to_json(&self) -> Map<String, Value> {
        let mut out = Map::new();
        out.insert("id".into(), self.id.clone());
        out.insert("day".into(), self.day.clone());
        out.insert("time".into(), json!(self.time));
        out
    }
}

/// Match `^\s*(\d{1,2}:\d{2})\b` and return the matched `H:MM` / `HH:MM` text.
///
/// Digits are ASCII only; Python's `\d` also accepts other Unicode decimal digits. That is the one
/// known difference from the Python pattern, and it is recorded in the spec.
fn clock_time(header: &str) -> Option<String> {
    let s = header.trim_start();
    let hour_digits = s.bytes().take_while(u8::is_ascii_digit).count();
    if !(1..=2).contains(&hour_digits) {
        return None;
    }
    let rest = s.get(hour_digits..)?;
    let minutes = rest.strip_prefix(':')?.get(..2)?;
    if !minutes.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let end = hour_digits + 3;
    // `\b` after the two minute digits: the next character must not be a word character.
    if let Some(next) = s.get(end..).and_then(|tail| tail.chars().next()) {
        if next.is_alphanumeric() || next == '_' {
            return None;
        }
    }
    s.get(..end).map(str::to_owned)
}

#[derive(Debug, Clone, PartialEq)]
struct Candidate {
    entry: EntryRef,
    rank: u64,
    score: Value,
    vector_rank: u64,
    loaded: bool,
    dropped: Option<String>,
}

impl Candidate {
    fn to_json(&self) -> Value {
        let mut out = self.entry.to_json();
        out.insert("rank".into(), json!(self.rank));
        out.insert("score".into(), self.score.clone());
        out.insert("loaded".into(), json!(self.loaded));
        out.insert("dropped".into(), json!(self.dropped));
        out.insert("vector_rank".into(), json!(self.vector_rank));
        Value::Object(out)
    }
}

#[derive(Debug, Clone, PartialEq)]
struct Loaded {
    entry: EntryRef,
    score: Value,
    tokens: u64,
    chars: u64,
}

impl Loaded {
    fn to_json(&self) -> Value {
        let mut out = self.entry.to_json();
        out.insert("score".into(), self.score.clone());
        out.insert("tokens".into(), json!(self.tokens));
        out.insert("chars".into(), json!(self.chars));
        Value::Object(out)
    }
}

/// One exchange's retrieval stages. Created per request and never shared.
#[derive(Debug, Clone, PartialEq)]
pub struct RetrievalTrace {
    /// Set when a tracing step failed: `attach` then adds no trace to the response.
    failed: bool,
    status: String,
    params: Map<String, Value>,
    budget: Map<String, Value>,
    candidates: Vec<Candidate>,
    loaded: Vec<Loaded>,
    counts: Map<String, Value>,
    direct_fallback: Value,
    score_gap: Value,
}

impl Default for RetrievalTrace {
    fn default() -> Self {
        Self::new()
    }
}

impl RetrievalTrace {
    pub fn new() -> Self {
        Self {
            failed: false,
            status: "not_run".into(),
            params: Map::new(),
            budget: Map::new(),
            candidates: Vec::new(),
            loaded: Vec::new(),
            counts: zero_counts(),
            direct_fallback: json!({"used": false, "found": false, "tokens": 0}),
            score_gap: json!({"gap": null, "applied": false, "best": null, "removed": []}),
        }
    }

    pub fn set_status(&mut self, status: &str) {
        self.status = status.to_owned();
    }

    /// Replace the parameters block (Python `set_params(**params)`).
    pub fn set_params(&mut self, params: Map<String, Value>) {
        self.params = params;
    }

    /// Replace the budget block (Python `set_budget(**budget)`).
    pub fn set_budget(&mut self, budget: Map<String, Value>) {
        self.budget = budget;
    }

    /// Record a stage 1 candidate and return its index for `set_candidate_outcome`.
    /// `vector_rank` defaults to `rank` when `None`, as in Python.
    pub fn add_candidate(&mut self, rank: u64, row: &Value, vector_rank: Option<u64>) -> usize {
        self.candidates.push(Candidate {
            entry: EntryRef::from_row(row),
            rank,
            score: row.get("score").cloned().unwrap_or(Value::Null),
            vector_rank: vector_rank.unwrap_or(rank),
            loaded: false,
            dropped: None,
        });
        self.candidates.len() - 1
    }

    /// Set what happened to a candidate: loaded into the context, or dropped with a reason
    /// (`"budget"` or `"top_k"`). An unknown index is ignored.
    pub fn set_candidate_outcome(&mut self, index: usize, loaded: bool, dropped: Option<&str>) {
        if let Some(candidate) = self.candidates.get_mut(index) {
            candidate.loaded = loaded;
            candidate.dropped = dropped.map(str::to_owned);
        }
    }

    /// Stage 1c (score gap). Entry refs and scores only; `diagnostics` is merged in like Python's
    /// `dict.update`.
    pub fn set_score_gap(
        &mut self,
        gap: Option<f64>,
        applied: bool,
        best: Value,
        removed: &[(u64, Value)],
        diagnostics: Option<Map<String, Value>>,
    ) {
        let removed: Vec<Value> = removed
            .iter()
            .map(|(rank, row)| {
                let mut out = EntryRef::from_row(row).to_json();
                out.insert("vector_rank".into(), json!(rank));
                out.insert(
                    "score".into(),
                    row.get("score").cloned().unwrap_or(Value::Null),
                );
                Value::Object(out)
            })
            .collect();
        let mut block = Map::new();
        block.insert("gap".into(), json!(gap));
        block.insert("applied".into(), json!(applied));
        block.insert("best".into(), best);
        block.insert("removed".into(), json!(removed));
        if let Some(extra) = diagnostics {
            block.extend(extra);
        }
        self.score_gap = Value::Object(block);
    }

    /// Record a chunk that went into the context.
    pub fn add_loaded(&mut self, row: &Value, tokens: u64, chars: u64) {
        self.loaded.push(Loaded {
            entry: EntryRef::from_row(row),
            score: row.get("score").cloned().unwrap_or(Value::Null),
            tokens,
            chars,
        });
    }

    /// Fill in the budget totals and the counts, once the context is built.
    pub fn finish(&mut self, used_chars: u64) {
        let used_tokens: u64 = self.loaded.iter().map(|e| e.tokens).sum();
        self.budget.insert("used_chars".into(), json!(used_chars));
        self.budget.insert("used_tokens".into(), json!(used_tokens));
        self.counts = Map::new();
        self.counts
            .insert("candidates".into(), json!(self.candidates.len()));
        self.counts
            .insert("loaded".into(), json!(self.loaded.len()));
        self.counts
            .insert("dropped_budget".into(), json!(self.dropped_count("budget")));
        self.counts
            .insert("dropped_top_k".into(), json!(self.dropped_count("top_k")));
    }

    /// The direct-file fallback ran. `found` says whether it produced any text.
    pub fn set_direct_fallback(&mut self, found: bool, tokens: u64) {
        self.direct_fallback = json!({"used": true, "found": found, "tokens": tokens});
    }

    /// Mark the trace as failed. `attach` then leaves the response without a trace.
    pub fn fail(&mut self) {
        self.failed = true;
    }

    /// The full trace object, the same keys as the Python `self.data`.
    pub fn to_json(&self) -> Value {
        json!({
            "version": SCHEMA_VERSION,
            "status": self.status,
            "params": self.params,
            "rerank": {"present": false},
            "candidates": self.candidates.iter().map(Candidate::to_json).collect::<Vec<_>>(),
            "loaded": self.loaded.iter().map(Loaded::to_json).collect::<Vec<_>>(),
            "budget": self.budget,
            "counts": self.counts,
            "direct_fallback": self.direct_fallback,
            "focus": {"checked": false},
            "score_gap": self.score_gap,
        })
    }

    fn dropped_count(&self, reason: &str) -> usize {
        self.candidates
            .iter()
            .filter(|c| c.dropped.as_deref() == Some(reason))
            .count()
    }
}

fn zero_counts() -> Map<String, Value> {
    let mut counts = Map::new();
    for key in ["candidates", "loaded", "dropped_budget", "dropped_top_k"] {
        counts.insert(key.into(), json!(0));
    }
    counts
}

/// Add the trace to a response, unless there is none or it failed (`attach` in Python).
pub fn attach(result: &mut Map<String, Value>, trace: Option<&RetrievalTrace>) {
    if let Some(trace) = trace.filter(|t| !t.failed) {
        result.insert("retrieval_trace".into(), trace.to_json());
    }
}
