//! noevia-core's `server/completeness-report.cjs` in Rust, exported from `dav-parse.wasm`
//! (COMPLETENESS_REPORT_IMPL). Given a derived job (jobs.cjs `derive()`, as `JSON.stringify`
//! writes it) and the caller's expected artifact names, [`build`] gives:
//!
//! - the five named checks of `buildCompletenessReport` (`tests-run`, `artifacts-present`,
//!   `plan-steps-closed`, `no-unresolved-uncertainty`, `checkpoint-head-recorded`), each with the
//!   JS's status, detail text and evidence, and the overall verdict (so `canEnterReviewing`);
//! - `reportHash`'s key-order-independent canonical JSON of that report and its sha256, or the
//!   reason the JS refuses to hash it (nested too deeply, too large; checked at the same points the
//!   JS checks them, so the first refusal is the same one).
//!
//! The journal, the lifecycle and the 409s stay in jobs.cjs. The host (noevia-core) always computes
//! the JS report and hands it out as verified only when this port returns the byte-identical
//! canonical text and hash; anything else marks the report unverified, which never lets a task
//! into `reviewing`.
//!
//! The JS reads properties of whatever `derive()` produced. On a JSON value that is: an object's
//! own member (no JSON key name used here exists on `Object.prototype`), `undefined` for every
//! other value. Truthiness, `${…}` and `JSON.stringify` follow ECMAScript (no Unicode tables are
//! involved anywhere: the SHA test is ASCII-only by the `/i` rules without `u`, the key sort is by
//! UTF-16 code units). Lone surrogates are kept and written as `JSON.stringify` writes them.
//!
//! # Refusals
//!
//! [`Refusal::Input`] where the JS throws (a job that is not an object; a `steps`/`artifacts`
//! that is truthy but not an array; a `null` step; an `expectedArtifacts` that is not iterable)
//! or the request is not `[job, expectedArtifacts]`; [`Refusal::TooLarge`] past
//! [`MAX_INPUT_BYTES`].
//!
//! # Stricter than the JS
//!
//! [`Refusal::Ambiguous`] (the host then marks the report unverified) where the JS answers but
//! the port does not model it:
//!
//! - a truthy `uncertain` that is not an array (the JS counts `.length` of a string or object);
//! - an `expectedArtifacts` string (the JS spreads it into code points) or one holding anything but
//!   strings (`Array#join` stringification of non-strings);
//! - a not-completed step whose `id` or `status` is an object or array (`${…}` of an object runs
//!   `toString`/`valueOf` lookups).
//!
//! Linear time (hashed name set), bounded input and output, no panics.

#![forbid(unsafe_code)]

use prompt_framing::json::{self, Value};
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use tool_exchange::js_number;

/// The largest request [`call`] accepts (the op byte and the JSON).
pub const MAX_INPUT_BYTES: usize = 8 * 1024 * 1024 + 1;
/// completeness-report.cjs MAX_HASH_CHARS: UTF-16 units of leaf text plus key lengths.
pub const MAX_HASH_CHARS: u64 = 4 * 1024 * 1024;
/// completeness-report.cjs MAX_HASH_DEPTH: containers open when another one is refused.
pub const MAX_HASH_DEPTH: usize = 64;
/// Parse depth of the request `[job, expected]`. A job container at depth j (the job is 1 here)
/// is serialised at least j deep, so one kept here at depth 65 is past MAX_HASH_DEPTH already;
/// anything deeper is never built.
const WIRE_DEPTH: usize = MAX_HASH_DEPTH + 2;

/// The check names, in report order (completeness-report.cjs CHECK_NAMES).
pub const CHECK_NAMES: [&str; 5] = [
    "tests-run",
    "artifacts-present",
    "plan-steps-closed",
    "no-unresolved-uncertainty",
    "checkpoint-head-recorded",
];
/// completeness-report.cjs TEST_STEP_IDS.
pub const TEST_STEP_IDS: [&str; 4] = ["tests", "test", "run-tests", "test-suite"];
/// completeness-report.cjs TEST_ARTIFACT_KIND.
pub const TEST_ARTIFACT_KIND: &str = "test-report";
/// completeness-report.cjs CHECKPOINT_SHA_KEYS, in lookup order.
pub const CHECKPOINT_SHA_KEYS: [&str; 6] = [
    "sha",
    "headSha",
    "head_sha",
    "commitSha",
    "commit_sha",
    "commit",
];

/// Why the port will not answer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refusal {
    /// Not a request, or the JS throws on it.
    Input,
    TooLarge,
    /// The JS answers, but the port does not model that answer (see the crate docs).
    Ambiguous,
}

impl Refusal {
    pub fn json(self) -> &'static str {
        match self {
            Refusal::Input => r#"{"error":"input"}"#,
            Refusal::TooLarge => r#"{"error":"too_large"}"#,
            Refusal::Ambiguous => r#"{"error":"ambiguous"}"#,
        }
    }
}

/// One check's status.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    Pass,
    Fail,
    Unknown,
}

impl Status {
    pub fn as_str(self) -> &'static str {
        match self {
            Status::Pass => "pass",
            Status::Fail => "fail",
            Status::Unknown => "unknown",
        }
    }
}

/// Why `reportHash` throws (409) on the report.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Unhashable {
    /// "it is nested too deeply"
    Deep,
    /// "it is too large"
    Large,
}

impl Unhashable {
    pub fn as_str(self) -> &'static str {
        match self {
            Unhashable::Deep => "deep",
            Unhashable::Large => "large",
        }
    }
}

/// The report: statuses in [`CHECK_NAMES`] order, the overall verdict, and the canonical JSON
/// `reportHash` hashes (or why it refuses).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Report {
    pub statuses: [Status; 5],
    pub overall: Status,
    pub canonical: Result<String, Unhashable>,
}

impl Report {
    /// `canEnterReviewing(report)`: every check passed.
    pub fn can_enter_reviewing(&self) -> bool {
        self.statuses.iter().all(|s| *s == Status::Pass)
    }

    /// `reportHash(report)`: sha256 of the canonical JSON, lower-case hex.
    pub fn hash(&self) -> Result<String, Unhashable> {
        let text = self.canonical.as_ref().map_err(|e| *e)?;
        let digest = Sha256::digest(text.as_bytes());
        let mut out = String::with_capacity(64);
        for b in digest {
            out.push_str(&format!("{b:02x}"));
        }
        Ok(out)
    }
}

// ── the report tree ─────────────────────────────────────────────────────────

/// A value of the report: a borrowed piece of the job, or one the checks made.
enum Out<'a> {
    In(&'a Value),
    Null,
    Bool(bool),
    Count(usize),
    Text(Vec<u16>),
    Lit(&'static str),
    Arr(Vec<Out<'a>>),
    Obj(Vec<(&'static str, Out<'a>)>),
}

struct Check<'a> {
    name: &'static str,
    status: Status,
    detail: Vec<u16>,
    evidence: Out<'a>,
}

fn units(s: &str) -> Vec<u16> {
    s.encode_utf16().collect()
}

fn text(parts: &[&[u16]]) -> Vec<u16> {
    parts.concat()
}

/// `v[key]` for a JSON value: an object's own member, else `undefined`.
fn member<'a>(v: &'a Value, key: &str) -> Option<&'a Value> {
    v.get(key)
}

fn is_str(v: Option<&Value>, s: &str) -> bool {
    v.and_then(Value::as_str)
        .is_some_and(|u| u.iter().copied().eq(s.encode_utf16()))
}

/// ECMAScript ToBoolean (`None` is `undefined`).
fn truthy(v: Option<&Value>) -> bool {
    match v {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(Value::Num(n)) => *n != 0.0 && !n.is_nan(),
        Some(Value::Str(s)) => !s.is_empty(),
        Some(Value::Arr(_) | Value::Obj(_) | Value::Deep) => true,
    }
}

/// `x || []` where the JS then calls an array method: an array, `[]` when falsy, the JS throws
/// otherwise.
fn array_or_empty(v: Option<&Value>) -> Result<&[Value], Refusal> {
    match v {
        Some(Value::Arr(xs)) => Ok(xs),
        x if !truthy(x) => Ok(&[]),
        _ => Err(Refusal::Input),
    }
}

/// `x || null` as report evidence.
fn or_null(v: Option<&Value>) -> Out<'_> {
    match v {
        Some(x) if truthy(Some(x)) => Out::In(x),
        _ => Out::Null,
    }
}

/// `${v}` of a scalar (`None` is `undefined`); objects and arrays are refused (crate docs).
fn js_string(v: Option<&Value>) -> Result<Vec<u16>, Refusal> {
    Ok(match v {
        None => units("undefined"),
        Some(Value::Null) => units("null"),
        Some(Value::Bool(b)) => units(if *b { "true" } else { "false" }),
        Some(Value::Num(n)) if n.is_finite() => units(&js_number(*n)),
        Some(Value::Num(n)) if n.is_nan() => units("NaN"),
        Some(Value::Num(n)) => units(if *n > 0.0 { "Infinity" } else { "-Infinity" }),
        Some(Value::Str(s)) => s.clone(),
        Some(Value::Arr(_) | Value::Obj(_) | Value::Deep) => return Err(Refusal::Ambiguous),
    })
}

fn count(n: usize) -> Vec<u16> {
    units(&n.to_string())
}

fn is_test_step(s: &Value) -> bool {
    TEST_STEP_IDS.iter().any(|id| is_str(member(s, "id"), id))
}

fn tests_run<'a>(steps: &'a [Value], artifacts: &'a [Value]) -> Check<'a> {
    let steps: Vec<&Value> = steps.iter().filter(|s| is_test_step(s)).collect();
    let artifacts: Vec<&Value> = artifacts
        .iter()
        .filter(|a| {
            is_str(member(a, "kind"), TEST_ARTIFACT_KIND)
                && matches!(member(a, "passed"), Some(Value::Bool(_)))
        })
        .collect();
    let (status, detail) = if steps.is_empty() && artifacts.is_empty() {
        (
            Status::Unknown,
            "No server-defined test step or server-measured test-report artifact was recorded.",
        )
    } else if steps.iter().any(|s| is_str(member(s, "status"), "failed"))
        || artifacts
            .iter()
            .any(|a| member(a, "passed") == Some(&Value::Bool(false)))
    {
        (
            Status::Fail,
            "A server-defined test step or test-report artifact recorded failure.",
        )
    } else if steps.iter().any(|s| is_str(member(s, "status"), "running")) {
        (
            Status::Unknown,
            "A server-defined test step has not completed yet.",
        )
    } else {
        (
            Status::Pass,
            "A server-defined test step or test-report artifact reports success.",
        )
    };
    Check {
        name: "tests-run",
        status,
        detail: units(detail),
        evidence: Out::Obj(vec![
            ("steps", Out::Arr(steps.into_iter().map(Out::In).collect())),
            (
                "artifacts",
                Out::Arr(artifacts.into_iter().map(Out::In).collect()),
            ),
        ]),
    }
}

fn artifacts_present<'a>(
    artifacts: &'a [Value],
    expected: &'a Value,
) -> Result<Check<'a>, Refusal> {
    let found: Vec<&[u16]> = artifacts
        .iter()
        .filter_map(|a| member(a, "name").and_then(Value::as_str))
        .collect();
    let found_out = || Out::Arr(found.iter().map(|n| Out::Text(n.to_vec())).collect());
    let names = match expected {
        Value::Null => {
            return Ok(Check {
                name: "artifacts-present",
                status: Status::Unknown,
                detail: units("No expected-artifact list was declared for this job."),
                evidence: Out::Obj(vec![("expected", Out::Null), ("found", found_out())]),
            })
        }
        Value::Arr(xs) => xs,
        Value::Str(_) => return Err(Refusal::Ambiguous),
        _ => return Err(Refusal::Input),
    };
    let mut expected_names: Vec<&[u16]> = Vec::with_capacity(names.len());
    for n in names {
        expected_names.push(n.as_str().ok_or(Refusal::Ambiguous)?);
    }
    let have: HashSet<&[u16]> = found.iter().copied().collect();
    let missing: Vec<&[u16]> = expected_names
        .iter()
        .copied()
        .filter(|n| !have.contains(n))
        .collect();
    let list = |xs: &[&[u16]]| Out::Arr(xs.iter().map(|n| Out::Text(n.to_vec())).collect());
    if missing.is_empty() {
        return Ok(Check {
            name: "artifacts-present",
            status: Status::Pass,
            detail: units("All expected artifacts are present."),
            evidence: Out::Obj(vec![
                ("expected", list(&expected_names)),
                ("found", found_out()),
            ]),
        });
    }
    let joined = missing.join(&units(", ")[..]);
    Ok(Check {
        name: "artifacts-present",
        status: Status::Fail,
        detail: text(&[
            &units("Missing expected artifact(s): "),
            &joined,
            &units("."),
        ]),
        evidence: Out::Obj(vec![
            ("expected", list(&expected_names)),
            ("found", found_out()),
            ("missing", list(&missing)),
        ]),
    })
}

fn plan_steps_closed<'a>(
    plan: Option<&'a Value>,
    steps: &'a [Value],
) -> Result<Check<'a>, Refusal> {
    let plan_truthy = truthy(plan);
    let base = |extra: Option<Out<'a>>| {
        let mut ev = vec![
            ("plan", or_null(plan)),
            ("steps", Out::Arr(steps.iter().map(Out::In).collect())),
        ];
        if let Some(open) = extra {
            ev.push(("open", open));
        }
        Out::Obj(ev)
    };
    let unknown = |detail: &str| Check {
        name: "plan-steps-closed",
        status: Status::Unknown,
        detail: units(detail),
        evidence: base(None),
    };
    if plan_truthy && plan.is_some_and(|p| is_str(member(p, "status"), "skipped")) {
        return Ok(unknown(
            "The plan was explicitly skipped; there are no steps to close.",
        ));
    }
    if !plan_truthy && steps.is_empty() {
        return Ok(unknown("No plan was proposed and no steps were recorded."));
    }
    let open: Vec<&Value> = steps
        .iter()
        .filter(|s| !is_str(member(s, "status"), "completed"))
        .collect();
    if !open.is_empty() {
        let mut list: Vec<u16> = Vec::new();
        for (i, s) in open.iter().enumerate() {
            if i > 0 {
                list.extend(units(", "));
            }
            list.extend(js_string(member(s, "id"))?);
            list.push(u16::from(b':'));
            list.extend(js_string(member(s, "status"))?);
        }
        let detail = text(&[
            &count(open.len()),
            &units(" step(s) are not completed (running or failed): "),
            &list,
            &units("."),
        ]);
        return Ok(Check {
            name: "plan-steps-closed",
            status: Status::Fail,
            detail,
            evidence: base(Some(Out::Arr(open.into_iter().map(Out::In).collect()))),
        });
    }
    if steps.is_empty() {
        return Ok(unknown(
            "A plan was proposed but no step events were recorded to verify closure.",
        ));
    }
    Ok(Check {
        name: "plan-steps-closed",
        status: Status::Pass,
        detail: text(&[
            &units("All "),
            &count(steps.len()),
            &units(" recorded step(s) are completed."),
        ]),
        evidence: base(None),
    })
}

fn unresolved<'a>(job: &'a Value) -> Result<Check<'a>, Refusal> {
    let uncertain = match member(job, "uncertain") {
        Some(Value::Arr(xs)) => xs.len(),
        x if !truthy(x) => 0,
        _ => return Err(Refusal::Ambiguous),
    };
    let pending = truthy(member(job, "pendingApproval"));
    let evidence = Out::Obj(vec![
        ("uncertainCount", Out::Count(uncertain)),
        ("pendingApproval", Out::Bool(pending)),
    ]);
    if uncertain > 0 || pending {
        let mut parts: Vec<Vec<u16>> = Vec::new();
        if uncertain > 0 {
            parts.push(text(&[
                &count(uncertain),
                &units(" unresolved tool.uncertain event(s)"),
            ]));
        }
        if pending {
            parts.push(units("a pending approval"));
        }
        let joined = parts.join(&units(" and ")[..]);
        return Ok(Check {
            name: "no-unresolved-uncertainty",
            status: Status::Fail,
            detail: text(&[&joined, &units(" remain.")]),
            evidence,
        });
    }
    Ok(Check {
        name: "no-unresolved-uncertainty",
        status: Status::Pass,
        detail: units("No unresolved uncertainty or pending approval."),
        evidence,
    })
}

/// `/^[0-9a-f]{7,40}$/i`.
fn sha_shaped(s: &[u16]) -> bool {
    (7..=40).contains(&s.len())
        && s.iter()
            .all(|&c| u8::try_from(c).is_ok_and(|b| b.is_ascii_hexdigit()))
}

fn checkpoint_head(checkpoint: Option<&Value>) -> Check<'_> {
    let Some(cp) = checkpoint.filter(|c| truthy(Some(c))) else {
        return Check {
            name: "checkpoint-head-recorded",
            status: Status::Unknown,
            detail: units("No checkpoint was recorded for this job."),
            evidence: Out::Obj(vec![("checkpoint", Out::Null)]),
        };
    };
    let found = CHECKPOINT_SHA_KEYS.iter().find_map(|k| {
        member(cp, k)
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(|s| (*k, s))
    });
    let Some((key, sha)) = found else {
        return Check {
            name: "checkpoint-head-recorded",
            status: Status::Unknown,
            detail: units(
                "A checkpoint was recorded but carries no head SHA (expected until #513 lands).",
            ),
            evidence: Out::Obj(vec![("checkpoint", Out::In(cp))]),
        };
    };
    let evidence = Out::Obj(vec![
        ("checkpoint", Out::In(cp)),
        ("field", Out::Lit(key)),
        ("sha", Out::Text(sha.to_vec())),
    ]);
    if !sha_shaped(sha) {
        let mut quoted = Vec::new();
        quote(&mut quoted, sha);
        return Check {
            name: "checkpoint-head-recorded",
            status: Status::Fail,
            detail: text(&[
                &units("Checkpoint head SHA field \""),
                &units(key),
                &units("\" does not look like a commit SHA: "),
                &quoted,
                &units("."),
            ]),
            evidence,
        };
    }
    Check {
        name: "checkpoint-head-recorded",
        status: Status::Pass,
        detail: text(&[
            &units("Checkpoint head SHA recorded ("),
            &units(key),
            &units(")."),
        ]),
        evidence,
    }
}

/// `buildCompletenessReport({ job, expectedArtifacts })` and what `reportHash` makes of it.
/// `expected` is `expectedArtifacts ?? null` (`Value::Null` for none declared).
pub fn build(job: &Value, expected: &Value) -> Result<Report, Refusal> {
    if !matches!(job, Value::Obj(_) | Value::Arr(_)) {
        return Err(Refusal::Input);
    }
    let steps = array_or_empty(member(job, "steps"))?;
    let artifacts = array_or_empty(member(job, "artifacts"))?;
    // Every step is read as `s.id` / `s.status`: the JS throws on a null one.
    if steps.iter().any(|s| matches!(s, Value::Null)) {
        return Err(Refusal::Input);
    }
    let checks = [
        tests_run(steps, artifacts),
        artifacts_present(artifacts, expected)?,
        plan_steps_closed(member(job, "plan"), steps)?,
        unresolved(job)?,
        checkpoint_head(member(job, "checkpoint")),
    ];
    let statuses = checks.each_ref().map(|c| c.status);
    let overall = if statuses.contains(&Status::Fail) {
        Status::Fail
    } else if statuses.contains(&Status::Unknown) {
        Status::Unknown
    } else {
        Status::Pass
    };
    let job_id = match member(job, "id") {
        None | Some(Value::Null) => Out::Null,
        Some(v) => Out::In(v),
    };
    let tree = Out::Obj(vec![
        ("jobId", job_id),
        (
            "checks",
            Out::Arr(
                checks
                    .into_iter()
                    .map(|c| {
                        Out::Obj(vec![
                            ("name", Out::Lit(c.name)),
                            ("status", Out::Lit(c.status.as_str())),
                            ("detail", Out::Text(c.detail)),
                            ("evidence", c.evidence),
                        ])
                    })
                    .collect(),
            ),
        ),
        ("overall", Out::Lit(overall.as_str())),
    ]);
    let mut w = Writer {
        out: Vec::new(),
        chars: 0,
        depth: 0,
    };
    let canonical = match w.out(&tree) {
        Ok(()) => Ok(String::from_utf16(&w.out).map_err(|_| Refusal::Input)?),
        Err(e) => Err(e),
    };
    Ok(Report {
        statuses,
        overall,
        canonical,
    })
}

// ── canonical JSON (completeness-report.cjs canonical) ─────────────────────

/// `JSON.stringify(string)` as UTF-16 units: lone surrogates as lower-case `\udxxx`.
fn quote(out: &mut Vec<u16>, s: &[u16]) {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let esc = |out: &mut Vec<u16>, u: u16| {
        out.extend(units("\\u"));
        for shift in [12u16, 8, 4, 0] {
            let d = HEX
                .get(usize::from((u >> shift) & 0xf))
                .copied()
                .unwrap_or(b'0');
            out.push(u16::from(d));
        }
    };
    out.push(0x22);
    let mut i = 0;
    while let Some(&c) = s.get(i) {
        match c {
            0x22 => out.extend(units("\\\"")),
            0x5c => out.extend(units("\\\\")),
            0x08 => out.extend(units("\\b")),
            0x0c => out.extend(units("\\f")),
            0x0a => out.extend(units("\\n")),
            0x0d => out.extend(units("\\r")),
            0x09 => out.extend(units("\\t")),
            0..=0x1f => esc(out, c),
            0xd800..=0xdbff => match s.get(i + 1) {
                Some(&d) if (0xdc00..=0xdfff).contains(&d) => {
                    out.push(c);
                    out.push(d);
                    i += 1;
                }
                _ => esc(out, c),
            },
            0xdc00..=0xdfff => esc(out, c),
            _ => out.push(c),
        }
        i += 1;
    }
    out.push(0x22);
}

struct Writer {
    out: Vec<u16>,
    chars: u64,
    depth: usize,
}

impl Writer {
    /// A leaf: its JSON text counts toward the budget, checked right after (as the JS does).
    fn leaf(&mut self, f: impl FnOnce(&mut Vec<u16>)) -> Result<(), Unhashable> {
        let start = self.out.len();
        f(&mut self.out);
        self.chars = self
            .chars
            .saturating_add(self.out.len().saturating_sub(start) as u64);
        if self.chars > MAX_HASH_CHARS {
            return Err(Unhashable::Large);
        }
        Ok(())
    }

    fn enter(&mut self) -> Result<(), Unhashable> {
        if self.depth >= MAX_HASH_DEPTH {
            return Err(Unhashable::Deep);
        }
        self.depth += 1;
        Ok(())
    }

    fn key(&mut self, first: bool, k: &[u16]) {
        if !first {
            self.out.push(u16::from(b','));
        }
        self.chars = self.chars.saturating_add(k.len() as u64);
        quote(&mut self.out, k);
        self.out.push(u16::from(b':'));
    }

    fn value(&mut self, v: &Value) -> Result<(), Unhashable> {
        match v {
            Value::Null => self.leaf(|o| o.extend(units("null"))),
            Value::Bool(b) => self.leaf(|o| o.extend(units(if *b { "true" } else { "false" }))),
            Value::Num(n) if n.is_finite() => self.leaf(|o| o.extend(units(&js_number(*n)))),
            Value::Num(_) => self.leaf(|o| o.extend(units("null"))),
            Value::Str(s) => self.leaf(|o| quote(o, s)),
            Value::Arr(xs) => {
                self.enter()?;
                self.out.push(u16::from(b'['));
                for (i, x) in xs.iter().enumerate() {
                    if i > 0 {
                        self.out.push(u16::from(b','));
                    }
                    self.value(x)?;
                }
                self.out.push(u16::from(b']'));
                self.depth -= 1;
                Ok(())
            }
            Value::Obj(m) => {
                self.enter()?;
                let mut members: Vec<&(Vec<u16>, Value)> = m.iter().collect();
                members.sort_by(|a, b| a.0.cmp(&b.0));
                self.out.push(u16::from(b'{'));
                for (i, (k, x)) in members.into_iter().enumerate() {
                    self.key(i == 0, k);
                    self.value(x)?;
                }
                self.out.push(u16::from(b'}'));
                self.depth -= 1;
                Ok(())
            }
            // Only containers past the parse depth are Deep; their parents are already too deep.
            Value::Deep => Err(Unhashable::Deep),
        }
    }

    fn out(&mut self, v: &Out<'_>) -> Result<(), Unhashable> {
        match v {
            Out::In(x) => self.value(x),
            Out::Null => self.leaf(|o| o.extend(units("null"))),
            Out::Bool(b) => self.leaf(|o| o.extend(units(if *b { "true" } else { "false" }))),
            Out::Count(n) => self.leaf(|o| o.extend(count(*n))),
            Out::Text(s) => self.leaf(|o| quote(o, s)),
            Out::Lit(s) => self.leaf(|o| quote(o, &units(s))),
            Out::Arr(xs) => {
                self.enter()?;
                self.out.push(u16::from(b'['));
                for (i, x) in xs.iter().enumerate() {
                    if i > 0 {
                        self.out.push(u16::from(b','));
                    }
                    self.out(x)?;
                }
                self.out.push(u16::from(b']'));
                self.depth -= 1;
                Ok(())
            }
            Out::Obj(m) => {
                self.enter()?;
                let mut members: Vec<&(&str, Out<'_>)> = m.iter().collect();
                // ASCII keys: byte order is code-unit order.
                members.sort_by(|a, b| a.0.cmp(b.0));
                self.out.push(u16::from(b'{'));
                for (i, (k, x)) in members.into_iter().enumerate() {
                    self.key(i == 0, &units(k));
                    self.out(x)?;
                }
                self.out.push(u16::from(b'}'));
                self.depth -= 1;
                Ok(())
            }
        }
    }
}

// ── the wasm call ───────────────────────────────────────────────────────────

/// The reply for a report: `{"hash":"…","report":<canonical JSON>}`, or when `reportHash` would
/// throw `{"unhashable":"deep"|"large","overall":"…","statuses":["…",…]}`.
pub fn reply(r: &Report) -> String {
    match (r.canonical.as_ref(), r.hash()) {
        (Ok(text), Ok(hash)) => format!("{{\"hash\":\"{hash}\",\"report\":{text}}}"),
        (Err(&e), _) | (_, Err(e)) => {
            let statuses: Vec<String> = r
                .statuses
                .iter()
                .map(|s| format!("\"{}\"", s.as_str()))
                .collect();
            format!(
                "{{\"unhashable\":\"{}\",\"overall\":\"{}\",\"statuses\":[{}]}}",
                e.as_str(),
                r.overall.as_str(),
                statuses.join(",")
            )
        }
    }
}

/// The `completeness_report` export: `u8(1)` and the UTF-8 JSON `[job, expectedArtifacts]`
/// (`JSON.stringify`, `expectedArtifacts ?? null`). Status 0 with [`reply`], or status 1 with a
/// [`Refusal`].
pub fn call(input: &[u8]) -> (u32, String) {
    match call_inner(input) {
        Ok(r) => (0, reply(&r)),
        Err(e) => (1, e.json().to_owned()),
    }
}

fn call_inner(input: &[u8]) -> Result<Report, Refusal> {
    if input.len() > MAX_INPUT_BYTES {
        return Err(Refusal::TooLarge);
    }
    let Some((&1, body)) = input.split_first() else {
        return Err(Refusal::Input);
    };
    let v = json::parse_utf8(body, WIRE_DEPTH).ok_or(Refusal::Input)?;
    let Value::Arr(parts) = v else {
        return Err(Refusal::Input);
    };
    let [job, expected] = parts.as_slice() else {
        return Err(Refusal::Input);
    };
    build(job, expected)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
mod tests {
    use super::*;

    fn run(wire: &str) -> (u32, String) {
        let mut input = vec![1u8];
        input.extend(wire.as_bytes());
        call(&input)
    }

    #[test]
    fn empty_job() {
        let (s, r) = run(r#"[{},null]"#);
        assert_eq!(s, 0);
        assert!(r.contains(r#""overall":"unknown""#), "{r}");
        assert!(r.starts_with(r#"{"hash":""#));
    }

    #[test]
    fn refusals() {
        assert_eq!(run("[1,null]").1, Refusal::Input.json());
        assert_eq!(run("[{}]").1, Refusal::Input.json());
        assert_eq!(run(r#"[{"steps":"x"},null]"#).1, Refusal::Input.json());
        assert_eq!(run(r#"[{"steps":[null]},null]"#).1, Refusal::Input.json());
        assert_eq!(run(r#"[{},"ab"]"#).1, Refusal::Ambiguous.json());
        assert_eq!(run(r#"[{},[1]]"#).1, Refusal::Ambiguous.json());
        assert_eq!(
            run(r#"[{"uncertain":"x"},null]"#).1,
            Refusal::Ambiguous.json()
        );
        assert_eq!(call(&[2, b'[', b']']).1, Refusal::Input.json());
        assert_eq!(call(&[]).1, Refusal::Input.json());
    }

    #[test]
    fn quotes_like_stringify() {
        let mut o = Vec::new();
        quote(&mut o, &[0x41, 0xd800, 0x0a, 0x01, 0xd83d, 0xde00, 0xdc00]);
        assert_eq!(
            String::from_utf16(&o).unwrap(),
            "\"A\\ud800\\n\\u0001\u{1f600}\\udc00\""
        );
    }

    #[test]
    fn sha_shape() {
        assert!(sha_shaped(&units("abcdef0")));
        assert!(sha_shaped(&units("ABCDEF0")));
        assert!(!sha_shaped(&units("abcdef")));
        assert!(!sha_shaped(&units("abcdefg")));
        assert!(!sha_shaped(&units(&"a".repeat(41))));
    }
}
