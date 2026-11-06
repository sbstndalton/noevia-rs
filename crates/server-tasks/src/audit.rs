//! Deterministic revision evidence and strict optional Auditor write-up. Model prose cannot
//! change a measured check or accept/merge a change.
use js_json::{wtf16, JValue};
fn text(s: impl Into<String>) -> JValue {
    JValue::Str(s.into())
}
fn object(fields: Vec<(&str, JValue)>) -> JValue {
    JValue::Obj(fields.into_iter().map(|(k, v)| (k.into(), v)).collect())
}
fn clean(value: &JValue, max: usize) -> String {
    let s=value.as_str().unwrap_or("").chars().filter(|c|!matches!(c,'\u{0}'..='\u{8}'|'\u{b}'..='\u{1f}'|'\u{7f}'|'\u{202a}'..='\u{202e}'|'\u{2066}'..='\u{2069}')).collect::<String>();
    server_account::util::js_trim(&s)
        .chars()
        .take(max)
        .collect()
}
fn only(value: &JValue, keys: &[&str]) -> bool {
    matches!(value,JValue::Obj(fields) if fields.iter().all(|(k,_)|keys.contains(&k.as_str())))
}
pub fn read_writeup(raw: &JValue) -> Result<JValue, &'static str> {
    if !only(raw, &["completeness", "summary", "evidence", "gaps"]) {
        return Err("The write-up had fields an audit cannot have.");
    }
    if !matches!(
        raw.get("completeness").as_str(),
        Some("complete" | "incomplete")
    ) {
        return Err("The write-up did not say complete or incomplete.");
    }
    let (JValue::Arr(evidence), JValue::Arr(gaps)) = (raw.get("evidence"), raw.get("gaps")) else {
        return Err("The write-up was missing its evidence or gaps.");
    };
    if evidence.len() > 12 || gaps.len() > 12 {
        return Err("The write-up listed too many items.");
    }
    let mut kept = Vec::new();
    for e in evidence {
        if !only(e, &["source", "note"])
            || !matches!(
                e.get("source").as_str(),
                Some("plan" | "tests" | "review" | "diff" | "journal")
            )
        {
            return Err("An evidence item was malformed.");
        }
        let note = clean(e.get("note"), 300);
        if note.is_empty() {
            return Err("An evidence item had no note.");
        }
        kept.push(object(vec![
            ("source", e.get("source").clone()),
            ("note", text(note)),
        ]));
    }
    let gaps = gaps
        .iter()
        .map(|g| clean(g, 300))
        .filter(|g| !g.is_empty())
        .map(text)
        .collect();
    let summary = clean(raw.get("summary"), 600);
    if summary.is_empty() {
        return Err("The write-up had no summary.");
    }
    Ok(object(vec![
        ("completeness", raw.get("completeness").clone()),
        ("summary", text(summary)),
        ("evidence", JValue::Arr(kept)),
        ("gaps", JValue::Arr(gaps)),
    ]))
}
fn artifact<'a>(job: &'a JValue, kind: &str, revision: f64) -> Option<&'a JValue> {
    let JValue::Arr(artifacts) = job.get("artifacts") else {
        return None;
    };
    artifacts.iter().rev().find(|a| {
        a.get("kind").as_str() == Some(kind) && a.get("revision").as_num() == Some(revision)
    })
}
fn check(name: &str, status: &str, detail: impl Into<String>) -> JValue {
    object(vec![
        ("name", text(name)),
        ("status", text(status)),
        ("detail", text(detail)),
    ])
}
fn nullish(v: &JValue) -> JValue {
    if v.is_nullish() {
        JValue::Null
    } else {
        v.clone()
    }
}
pub fn report(
    job: &JValue,
    revision: f64,
    head: &str,
    base: Option<&str>,
    plan_hash: Option<&str>,
    branch_tip: Option<&str>,
) -> JValue {
    let bound = job.get("revision").get("n").as_num() == Some(revision)
        && job.get("revision").get("headSha").as_str() == Some(head);
    let mut checks = vec![if bound {
        check(
            "revision-bound",
            "pass",
            format!(
                "Revision {revision} is bound to {}.",
                wtf16::slice(head, 12)
            ),
        )
    } else {
        check(
            "revision-bound",
            "fail",
            "The job’s current revision is not the one being audited.",
        )
    }];
    let plan = artifact(job, "plan", revision);
    checks.push(match plan {
        None => check(
            "plan-recorded",
            "fail",
            "No plan was recorded for this revision.",
        ),
        Some(p)
            if plan_hash.is_some_and(|hash| {
                !hash.is_empty() && p.get("planHash").as_str() != Some(hash)
            }) =>
        {
            check(
                "plan-recorded",
                "fail",
                "The recorded plan is not the plan this revision was bound to.",
            )
        }
        Some(p) => check(
            "plan-recorded",
            "pass",
            format!(
                "The plan for this revision is recorded ({}).",
                wtf16::slice(p.get("planHash").as_str().unwrap_or(""), 12)
            ),
        ),
    });
    let tests = artifact(job, "test-report", revision);
    checks.push(match tests {
        None => check(
            "tests-measured",
            "fail",
            "No server-measured test report was recorded for this revision.",
        ),
        Some(t) if t.get("headSha").as_str() != Some(head) => check(
            "tests-measured",
            "fail",
            "The test report is for a different commit.",
        ),
        Some(t) if t.get("passed") != &JValue::Bool(true) => check(
            "tests-measured",
            "fail",
            "The operator’s tests did not pass at this head.",
        ),
        Some(t) => check(
            "tests-measured",
            "pass",
            format!(
                "The operator’s tests passed at this head{}.",
                t.get("exitCode")
                    .as_num()
                    .filter(|n| n.is_finite() && n.fract() == 0.0)
                    .map_or(String::new(), |n| format!(" (exit {n})"))
            ),
        ),
    });
    let gate = match job.get("stages") {
        JValue::Arr(stages) => stages.iter().rev().find(|s| {
            s.get("to").as_str() == Some("reviewing")
                && s.get("revision").as_num() == Some(revision)
                && s.get("reportHash").truthy()
        }),
        _ => None,
    };
    checks.push(if gate.is_some() {
        check(
            "completeness-gate",
            "pass",
            "The completeness report allowed review of this revision.",
        )
    } else {
        check(
            "completeness-gate",
            "fail",
            "This revision never passed the completeness gate into review.",
        )
    });
    let review = artifact(job, "review-verdict", revision);
    checks.push(match review {
        None => check(
            "review-bound",
            "fail",
            "No Planner verdict was recorded for this revision.",
        ),
        Some(r) if r.get("headSha").as_str() != Some(head) => check(
            "review-bound",
            "fail",
            "The Planner’s verdict is for a different commit.",
        ),
        Some(r) if r.get("verdict").as_str() != Some("approve") => check(
            "review-bound",
            "fail",
            "The Planner did not approve this revision.",
        ),
        Some(_) => check("review-bound", "pass", "The Planner approved this head."),
    });
    checks.push(match branch_tip {
        None => check(
            "head-unchanged",
            "unknown",
            "The task branch’s tip could not be read.",
        ),
        Some(tip) if tip == head => check(
            "head-unchanged",
            "pass",
            "The task branch is still at the audited head.",
        ),
        _ => check(
            "head-unchanged",
            "fail",
            "The task branch moved after this revision was recorded.",
        ),
    });
    let open = matches!(job.get("uncertain"),JValue::Arr(v)if !v.is_empty())
        || job.get("pendingApproval").truthy();
    checks.push(if open {
        check(
            "nothing-unresolved",
            "fail",
            "An uncertain tool result or an open approval remains.",
        )
    } else {
        check(
            "nothing-unresolved",
            "pass",
            "No uncertain tool result or open approval remains.",
        )
    });
    let complete = checks
        .iter()
        .all(|c| c.get("status").as_str() == Some("pass"));
    let evidence = object(vec![
        (
            "plan",
            plan.map_or(JValue::Null, |p| {
                object(vec![("planHash", nullish(p.get("planHash")))])
            }),
        ),
        (
            "tests",
            tests.map_or(JValue::Null, |t| {
                object(vec![
                    (
                        "passed",
                        JValue::Bool(t.get("passed") == &JValue::Bool(true)),
                    ),
                    ("exitCode", nullish(t.get("exitCode"))),
                    (
                        "timedOut",
                        JValue::Bool(t.get("timedOut") == &JValue::Bool(true)),
                    ),
                    (
                        "durationMs",
                        t.get("durationMs")
                            .as_num()
                            .filter(|n| n.is_finite())
                            .map_or(JValue::Null, JValue::Num),
                    ),
                    ("headSha", nullish(t.get("headSha"))),
                ])
            }),
        ),
        (
            "review",
            review.map_or(JValue::Null, |r| {
                object(vec![
                    ("verdict", r.get("verdict").clone()),
                    (
                        "findings",
                        JValue::Num(match r.get("findings") {
                            JValue::Arr(f) => f.len() as f64,
                            _ => 0.0,
                        }),
                    ),
                    ("headSha", nullish(r.get("headSha"))),
                ])
            }),
        ),
        (
            "completeness",
            gate.map_or(JValue::Null, |g| {
                object(vec![("reportHash", g.get("reportHash").clone())])
            }),
        ),
    ]);
    object(vec![
        ("revision", JValue::Num(revision)),
        ("headSha", text(head)),
        ("baseSha", base.map_or(JValue::Null, text)),
        ("planHash", plan_hash.map_or(JValue::Null, text)),
        (
            "overall",
            text(if complete { "complete" } else { "incomplete" }),
        ),
        ("checks", JValue::Arr(checks)),
        ("evidence", evidence),
    ])
}
#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;
    #[test]
    fn exact_node_audit_and_writeup_vectors() {
        let f = wtf16::parse(include_str!(
            "../tests/fixtures/planner-contracts-node.json"
        ))
        .unwrap();
        let JValue::Arr(audits) = f.get("audits") else {
            panic!()
        };
        for a in audits {
            let i = a.get("input");
            assert_eq!(
                report(
                    i.get("job"),
                    i.get("revision").as_num().unwrap(),
                    i.get("headSha").as_str().unwrap(),
                    i.get("baseSha").as_str(),
                    i.get("planHash").as_str(),
                    i.get("branchTip").as_str()
                ),
                *a.get("report")
            );
        }
        let JValue::Arr(writeups) = f.get("writeups") else {
            panic!()
        };
        for w in writeups {
            match read_writeup(w.get("input")) {
                Ok(v) => assert_eq!(v, *w.get("value")),
                Err(e) => assert_eq!(Some(e), w.get("error").as_str()),
            }
        }
    }
}
