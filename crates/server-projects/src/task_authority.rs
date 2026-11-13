//! core jobs.cjs `boundAuthorityEvent` and `revisionWindow`: what [`crate::task_store::Store::append_authority`]
//! checks and writes for a lifecycle authority event (`task.stage`, `task.revision`, #701). Holding
//! the authority proves who writes, not that the move is sound: a stale `from` or revision, an
//! illegal move, or a journal whose completeness report does not allow review is refused here,
//! before anything is written. The event's data is rebuilt from known fields only; the report a
//! move into `reviewing` records the hash of is built here, from the journal, never from caller data.

use crate::task_journals::{self as journals, Job};
use crate::task_store::{js_object, js_trim, Refusal};
use js_json::{number_to_string, wtf16, JValue};
use prompt_framing::json as report_json;
use task_lifecycle::{State, Throw, STATES};

/// jobs.cjs `MAX_STAGE_EVENTS`: `task.stage` events one journal takes.
pub const MAX_STAGE_EVENTS: usize = 64;
/// jobs.cjs `MAX_REVISIONS`: `task.revision` events one journal takes.
pub const MAX_REVISIONS: usize = 32;
/// jobs.cjs `MAX_STAGE_REASON`: UTF-16 units of a stage's reason.
pub const MAX_STAGE_REASON: usize = 300;
/// jobs.cjs `MAX_EXPECTED_ARTIFACTS`.
pub const MAX_EXPECTED_ARTIFACTS: usize = 32;
const MAX_ARTIFACT_NAME: usize = 200;
const INCONSISTENT: &str = "This task’s lifecycle journal is inconsistent; review required";
const UNVERIFIED: &str = "This task’s lifecycle could not be verified; review required";
/// The report's job is parsed as the report port parses it inside `[job, expected]`.
const REPORT_DEPTH: usize = completeness_report::MAX_HASH_DEPTH + 1;

static UNDEFINED: JValue = JValue::Undefined;

fn conflict(message: impl Into<String>) -> Refusal {
    Refusal::new(Some(409), message)
}

fn bad(message: &str) -> Refusal {
    Refusal::new(Some(400), message)
}

/// `raw[key]` as a JS object holds it (a repeated key's last value); `undefined` on a non-object,
/// which is `{}` in jobs.cjs (`data && typeof data === 'object' ? data : {}`; an array has none of
/// these members either).
fn field<'a>(raw: &'a JValue, key: &str) -> &'a JValue {
    match raw {
        JValue::Obj(items) => items
            .iter()
            .rev()
            .find(|(k, _)| k == key)
            .map_or(&UNDEFINED, |(_, v)| v),
        _ => &UNDEFINED,
    }
}

/// `JSON.stringify(v)` inside a template literal (`undefined` for undefined).
fn json_text(v: &JValue) -> String {
    let ordered = js_object(v.clone(), 0).unwrap_or_else(|_| v.clone());
    wtf16::stringify(&ordered).unwrap_or_else(|| "undefined".to_owned())
}

fn hex_of(s: &str, lens: &[usize]) -> bool {
    lens.contains(&s.len())
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

fn count_of(rows: &[JValue], kind: &str) -> usize {
    rows.iter()
        .filter(|r| r.get("type").as_str() == Some(kind))
        .count()
}

/// `boundAuthorityEvent(id, type, data, current, known)`: the data to write, or the refusal.
/// `known` is the job folded from `current` (non-empty, a Code job, not finished).
pub(crate) fn bound(
    id: &str,
    kind: &str,
    raw: &JValue,
    current: &[JValue],
    known: &Job,
) -> Result<JValue, Refusal> {
    let state = match journals::fold_lifecycle(current, true) {
        Some(Ok(state)) => state,
        Some(Err(_)) => return Err(conflict(INCONSISTENT)),
        None => return Err(conflict(UNVERIFIED)),
    };
    // `known.revision?.n ?? 0`. A recorded `n` that is not a number is a journal this store did
    // not write: it reads as inconsistent (named difference 8 in task_store).
    let revision = match known.authoritative.as_ref().map(|a| a.revision.get("n")) {
        None => 0.0,
        Some(n) if n.is_nullish() => 0.0,
        Some(JValue::Num(n)) => *n,
        Some(_) => return Err(conflict(INCONSISTENT)),
    };
    if kind == "task.revision" {
        if count_of(current, kind) >= MAX_REVISIONS {
            return Err(conflict("Task revision limit reached"));
        }
        let n = field(raw, "n");
        if n.as_num() != Some(revision + 1.0) {
            return Err(conflict(format!(
                "Stale task revision: expected {}",
                number_to_string(revision + 1.0)
            )));
        }
        let head = field(raw, "headSha");
        if !head.as_str().is_some_and(|s| hex_of(s, &[40, 64])) {
            return Err(bad("A task revision needs the full head commit SHA"));
        }
        let plan = field(raw, "planHash");
        if !plan.is_nullish() && !plan.as_str().is_some_and(|s| hex_of(s, &[64])) {
            return Err(bad("A plan hash is a sha256 hex digest"));
        }
        return Ok(JValue::obj([
            ("n", n.clone()),
            ("headSha", head.clone()),
            (
                "planHash",
                if plan.is_nullish() {
                    JValue::Null
                } else {
                    plan.clone()
                },
            ),
        ]));
    }
    if count_of(current, kind) >= MAX_STAGE_EVENTS {
        return Err(conflict("Task stage limit reached"));
    }
    let to_value = field(raw, "to");
    let Some(to) = to_value
        .as_str()
        .and_then(|s| STATES.into_iter().find(|st| st.name() == s))
    else {
        return Err(bad(&format!("Unknown task stage: {}", json_text(to_value))));
    };
    if field(raw, "from").as_str() != Some(state.name()) {
        return Err(conflict(format!(
            "Stale task stage: the task is {}",
            state.name()
        )));
    }
    if field(raw, "revision").as_num() != Some(revision) {
        return Err(conflict(format!(
            "Stale task revision: the task is at revision {}",
            number_to_string(revision)
        )));
    }
    match task_lifecycle::assert_stage_move(state, to) {
        Ok(_) => {}
        Err(Throw::MergeFromReviewing) => {
            return Err(conflict(format!(
                "A task is merged only from reviewing, not {}",
                state.name()
            )))
        }
        Err(_) => {
            return Err(conflict(format!(
                "Illegal task-lifecycle transition: {} -> {}",
                state.name(),
                to.name()
            )))
        }
    }
    let mut out = vec![
        ("from", JValue::Str(state.name().to_owned())),
        ("to", JValue::Str(to.name().to_owned())),
        ("revision", JValue::Num(revision)),
        ("reason", reason(field(raw, "reason"))),
    ];
    if to == State::Reviewing && state != State::Reviewing {
        let expected = field(raw, "expectedArtifacts");
        let short_list = match expected {
            JValue::Arr(names) => {
                names.len() <= MAX_EXPECTED_ARTIFACTS
                    && names.iter().all(|n| {
                        n.as_str()
                            .is_some_and(|s| (1..=MAX_ARTIFACT_NAME).contains(&wtf16::len(s)))
                    })
            }
            v => v.is_nullish(),
        };
        if !short_list {
            return Err(bad("expectedArtifacts is a short list of artifact names"));
        }
        out.push((
            "reportHash",
            JValue::Str(report_hash(id, current, known, expected)?),
        ));
    }
    Ok(JValue::obj(out))
}

/// `typeof raw.reason === 'string' ? raw.reason.replace(/[\u0000-\u001f\u007f]+/g, ' ').trim()
/// .slice(0, MAX_STAGE_REASON) || null : null`.
fn reason(v: &JValue) -> JValue {
    let Some(text) = v.as_str() else {
        return JValue::Null;
    };
    let mut cleaned = String::with_capacity(text.len());
    let mut in_run = false;
    for c in text.chars() {
        if c <= '\u{1f}' || c == '\u{7f}' {
            if !in_run {
                cleaned.push(' ');
            }
            in_run = true;
        } else {
            cleaned.push(c);
            in_run = false;
        }
    }
    let cut = wtf16::slice(js_trim(&cleaned), MAX_STAGE_REASON);
    if cut.is_empty() {
        JValue::Null
    } else {
        JValue::Str(cut)
    }
}

/// Whether an approval is pending at the journal's end (derive's `pendingApproval`, which is
/// `e.data || {}` and so always truthy once set).
fn pending_approval(rows: &[JValue]) -> bool {
    let mut pending = false;
    for row in rows {
        match row.get("type").as_str() {
            Some("approval.requested") => pending = true,
            Some(
                "approval.decided" | "job.completed" | "job.failed" | "job.cancelled"
                | "job.interrupted",
            ) => pending = false,
            _ => {}
        }
    }
    pending
}

/// `reportHash(buildCompletenessReport({ job: { ...revisionWindow(current, known), id },
/// expectedArtifacts }))`, or the 409 when the report does not allow review.
fn report_hash(
    id: &str,
    current: &[JValue],
    known: &Job,
    expected: &JValue,
) -> Result<String, Refusal> {
    // revisionWindow: the steps and artifacts recorded since the task last entered
    // `implementing` (the job.created row and every row after that stage, folded on their own);
    // everything else is the whole job's.
    let since = current.iter().rposition(|row| {
        row.get("type").as_str() == Some("task.stage")
            && row.get("data").get("to").as_str() == Some("implementing")
    });
    let window = since.map(|i| {
        let rows: Vec<JValue> = current
            .first()
            .into_iter()
            .chain(current.get(i + 1..).unwrap_or(&[]))
            .cloned()
            .collect();
        journals::derive(&rows)
    });
    let (steps, artifacts) = match &window {
        Some(w) => (&w.steps, &w.artifacts),
        None => (&known.steps, &known.artifacts),
    };
    let job = JValue::obj([
        ("id", JValue::Str(id.to_owned())),
        ("steps", JValue::Arr(steps.clone())),
        ("artifacts", JValue::Arr(artifacts.clone())),
        ("plan", known.plan.clone()),
        ("checkpoint", known.checkpoint.clone()),
        ("uncertain", JValue::Arr(known.uncertain.clone())),
        ("pendingApproval", JValue::Bool(pending_approval(current))),
    ]);
    let expected = if expected.is_nullish() {
        &JValue::Null
    } else {
        expected
    };
    let parse = |v: &JValue| {
        wtf16::stringify(v).and_then(|t| report_json::parse_utf8(t.as_bytes(), REPORT_DEPTH))
    };
    // The report port refuses only shapes a journal this store wrote does not hold (named
    // difference 9 in task_store): refused as unverified, which never allows review.
    let unverified =
        || conflict("The completeness report does not allow review (unverified: impl_refused)");
    let (Some(job), Some(expected)) = (parse(&job), parse(expected)) else {
        return Err(unverified());
    };
    let report = completeness_report::build(&job, &expected).map_err(|_| unverified())?;
    if !report.can_enter_reviewing() {
        let open: Vec<String> = completeness_report::CHECK_NAMES
            .iter()
            .zip(report.statuses)
            .filter(|(_, s)| *s != completeness_report::Status::Pass)
            .map(|(name, s)| format!("{name}: {}", s.as_str()))
            .collect();
        return Err(conflict(format!(
            "The completeness report does not allow review ({})",
            open.join("; ")
        )));
    }
    report.hash().map_err(|e| {
        conflict(match e {
            completeness_report::Unhashable::Deep => {
                "The completeness report cannot be hashed: it is nested too deeply"
            }
            completeness_report::Unhashable::Large => {
                "The completeness report cannot be hashed: it is too large"
            }
        })
    })
}
