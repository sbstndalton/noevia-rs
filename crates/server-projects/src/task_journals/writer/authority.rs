//! Server-only lifecycle authority: no caller-provided completeness report is trusted.
use super::{object, Error, Job};
use crate::task_journals::{derive, lifecycle_state};
use js_json::{wtf16, JValue};
use prompt_framing::json;
use task_lifecycle::State;
fn refusal(status: u16, message: impl Into<String>) -> Error {
    Error::Authority {
        status,
        message: message.into(),
    }
}
fn conflict(message: impl Into<String>) -> Error {
    refusal(409, message)
}
fn text(s: impl Into<String>) -> JValue {
    JValue::Str(s.into())
}
fn hex(value: &JValue, lengths: &[usize]) -> bool {
    value.as_str().is_some_and(|s| {
        lengths.contains(&s.len())
            && s.bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    })
}
fn parse(value: &JValue) -> Result<json::Value, Error> {
    let s = wtf16::stringify(value).ok_or(Error::InvalidEvent)?;
    json::parse_utf8(s.as_bytes(), 64).ok_or(Error::InvalidEvent)
}
pub(super) fn bound(
    id: &str,
    kind: &str,
    data: &JValue,
    rows: &[JValue],
    known: &Job,
) -> Result<JValue, Error> {
    let state = lifecycle_state(rows, true);
    let state = state
        .as_str()
        .and_then(|s| State::from_units(&s.encode_utf16().collect::<Vec<_>>()))
        .ok_or_else(|| {
            conflict("This task’s lifecycle journal is inconsistent; review required")
        })?;
    let revision = known
        .authoritative
        .as_ref()
        .map(|a| a.revision.get("n"))
        .and_then(JValue::as_num)
        .unwrap_or(0.0);
    let count = rows
        .iter()
        .filter(|r| r.get("type").as_str() == Some(kind))
        .count();
    if kind == "task.revision" {
        if count >= 32 {
            return Err(conflict("Task revision limit reached"));
        }
        if data.get("n").as_num() != Some(revision + 1.0) {
            return Err(conflict(format!(
                "Stale task revision: expected {}",
                revision + 1.0
            )));
        }
        if !hex(data.get("headSha"), &[40, 64]) {
            return Err(refusal(
                400,
                "A task revision needs the full head commit SHA",
            ));
        }
        if !data.get("planHash").is_nullish() && !hex(data.get("planHash"), &[64]) {
            return Err(refusal(400, "A plan hash is a sha256 hex digest"));
        }
        return Ok(object(vec![
            ("n", data.get("n").clone()),
            ("headSha", data.get("headSha").clone()),
            (
                "planHash",
                if data.get("planHash").is_nullish() {
                    JValue::Null
                } else {
                    data.get("planHash").clone()
                },
            ),
        ]));
    }
    if count >= 64 {
        return Err(conflict("Task stage limit reached"));
    }
    let target = data
        .get("to")
        .as_str()
        .and_then(|s| State::from_units(&s.encode_utf16().collect::<Vec<_>>()))
        .ok_or_else(|| {
            refusal(
                400,
                format!(
                    "Unknown task stage: {}",
                    wtf16::stringify(data.get("to")).unwrap_or_else(|| "undefined".into())
                ),
            )
        })?;
    if data.get("from").as_str() != Some(state.name()) {
        return Err(conflict(format!(
            "Stale task stage: the task is {}",
            state.name()
        )));
    }
    if data.get("revision").as_num() != Some(revision) {
        return Err(conflict(format!(
            "Stale task revision: the task is at revision {revision}"
        )));
    }
    task_lifecycle::assert_stage_move(state, target).map_err(|e| {
        conflict(if e == task_lifecycle::Throw::MergeFromReviewing {
            format!("A task is merged only from reviewing, not {}", state.name())
        } else {
            format!(
                "Illegal task-lifecycle transition: {} -> {}",
                state.name(),
                target.name()
            )
        })
    })?;
    let reason = data
        .get("reason")
        .as_str()
        .map(|s| {
            let mut out = String::new();
            let mut control = false;
            for c in s.chars() {
                if c <= '\u{1f}' || c == '\u{7f}' {
                    if !control {
                        out.push(' ');
                    }
                    control = true;
                } else {
                    out.push(c);
                    control = false;
                }
            }
            wtf16::slice(out.trim_matches(crate::js::is_space), 300)
        })
        .filter(|s| !s.is_empty())
        .map_or(JValue::Null, text);
    let mut out = vec![
        ("from", text(state.name())),
        ("to", text(target.name())),
        ("revision", JValue::Num(revision)),
        ("reason", reason),
    ];
    if target == State::Reviewing && state != target {
        let expected = data.get("expectedArtifacts");
        if !expected.is_nullish()
            && !matches!(expected,JValue::Arr(names) if names.len()<=32 && names.iter().all(|n|n.as_str().is_some_and(|s|!s.is_empty() && s.encode_utf16().count()<=200)))
        {
            return Err(refusal(
                400,
                "expectedArtifacts is a short list of artifact names",
            ));
        }
        let window = rows
            .iter()
            .rposition(|r| {
                r.get("type").as_str() == Some("task.stage")
                    && r.get("data").get("to").as_str() == Some("implementing")
            })
            .map(|since| {
                let mut window = rows.first().cloned().into_iter().collect::<Vec<_>>();
                window.extend(rows.iter().skip(since + 1).cloned());
                derive(&window)
            });
        let evidence = window.as_ref().unwrap_or(known);
        let job = object(vec![
            ("id", text(id)),
            ("steps", JValue::Arr(evidence.steps.clone())),
            ("artifacts", JValue::Arr(evidence.artifacts.clone())),
            ("plan", known.plan.clone()),
            ("checkpoint", known.checkpoint.clone()),
            ("uncertain", JValue::Arr(known.uncertain.clone())),
            (
                "pendingApproval",
                JValue::Bool(known.status.as_str() == Some("waiting_approval")),
            ),
        ]);
        let expected = if expected.is_nullish() {
            JValue::Null
        } else {
            expected.clone()
        };
        let report = completeness_report::build(&parse(&job)?, &parse(&expected)?)
            .map_err(|_| Error::InvalidEvent)?;
        if !report.can_enter_reviewing() {
            let open = completeness_report::CHECK_NAMES
                .iter()
                .zip(report.statuses)
                .filter(|(_, s)| *s != completeness_report::Status::Pass)
                .map(|(name, status)| format!("{name}: {}", status.as_str()))
                .collect::<Vec<_>>()
                .join("; ");
            return Err(conflict(format!(
                "The completeness report does not allow review ({open})"
            )));
        }
        out.push((
            "reportHash",
            text(report.hash().map_err(|_| Error::InvalidEvent)?),
        ));
    }
    Ok(object(out))
}
