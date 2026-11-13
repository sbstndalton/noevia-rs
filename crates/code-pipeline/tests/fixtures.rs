//! Every row of tests/fixtures/code-pipeline.v1.json, recorded from noevia-core's
//! code-pipeline.cjs, planner-plan.cjs, code-review.cjs and auditor-report.cjs by
//! tools/gen-fixtures.cjs, replayed through the port and compared as JSON text in the fixture's
//! tagged form (so `undefined`, NaN and -0 count too). A TypeError is compared by name only.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use code_pipeline::planner::{self, plan_session_failure, plan_timeout_reason};
use code_pipeline::review::{self, review_pin_code, review_session_failure, review_timeout_reason};
use code_pipeline::*;
use js_json::{wtf16, JValue};
use std::collections::BTreeMap;

const TAG: &str = "$js";

fn fixture() -> JValue {
    wtf16::parse(include_str!("fixtures/code-pipeline.v1.json")).expect("fixture JSON")
}

fn s(t: &str) -> JValue {
    JValue::Str(t.into())
}

fn obj<const N: usize>(pairs: [(&str, JValue); N]) -> JValue {
    JValue::obj(pairs)
}

/// The fixture's tagged form back to JS values.
fn decode(v: &JValue) -> JValue {
    match v {
        JValue::Arr(items) => JValue::Arr(items.iter().map(decode).collect()),
        JValue::Obj(items) => {
            if let Some(tag) = v.get(TAG).as_str() {
                return match tag {
                    "undefined" => JValue::Undefined,
                    "NaN" => JValue::Num(f64::NAN),
                    "Infinity" => JValue::Num(f64::INFINITY),
                    "-Infinity" => JValue::Num(f64::NEG_INFINITY),
                    "-0" => JValue::Num(-0.0),
                    "repeat" => {
                        let n = v.get("n").as_num().unwrap() as usize;
                        JValue::Str(v.get("s").as_str().unwrap().repeat(n))
                    }
                    other => panic!("unknown tag {other}"),
                };
            }
            JValue::Obj(items.iter().map(|(k, x)| (k.clone(), decode(x))).collect())
        }
        other => other.clone(),
    }
}

/// JS values to the fixture's tagged form.
fn encode(v: &JValue) -> JValue {
    let tag = |t: &str| obj([(TAG, s(t))]);
    match v {
        JValue::Undefined => tag("undefined"),
        JValue::Num(n) if n.is_nan() => tag("NaN"),
        JValue::Num(n) if *n == f64::INFINITY => tag("Infinity"),
        JValue::Num(n) if *n == f64::NEG_INFINITY => tag("-Infinity"),
        JValue::Num(n) if *n == 0.0 && n.is_sign_negative() => tag("-0"),
        JValue::Arr(items) => JValue::Arr(items.iter().map(encode).collect()),
        JValue::Obj(items) => {
            JValue::Obj(items.iter().map(|(k, x)| (k.clone(), encode(x))).collect())
        }
        other => other.clone(),
    }
}

fn text(v: &JValue) -> String {
    wtf16::stringify(v).unwrap_or_else(|| "<undefined>".into())
}

fn arg(args: &JValue, i: usize) -> JValue {
    match args {
        JValue::Arr(items) => items.get(i).map(decode).unwrap_or(JValue::Undefined),
        _ => panic!("args"),
    }
}

fn strs(v: &[String]) -> JValue {
    JValue::Arr(v.iter().map(|x| s(x)).collect())
}

fn type_error() -> JValue {
    obj([("throws", obj([("name", s("TypeError"))]))])
}

fn out(v: JValue) -> JValue {
    obj([("out", encode(&v))])
}

fn ok_or_type(r: Result<JValue, TypeError>) -> JValue {
    r.map_or_else(|_| type_error(), out)
}

fn opt_str(v: &JValue) -> Option<String> {
    v.as_str().map(str::to_string)
}

fn u(v: &JValue) -> u32 {
    v.as_num().unwrap() as u32
}

fn failure(f: Failure) -> (String, String) {
    (f.code.into(), f.reason.into())
}

fn audit_input(
    i: &JValue,
) -> (
    JValue,
    u32,
    String,
    Option<String>,
    Option<String>,
    Option<String>,
) {
    (
        i.get("job").clone(),
        u(i.get("revision")),
        i.get("headSha").as_str().unwrap().to_string(),
        opt_str(i.get("baseSha")),
        opt_str(i.get("planHash")),
        opt_str(i.get("branchTip")),
    )
}

fn audit(i: &JValue) -> Result<AuditReport, AuditReportError> {
    let (job, revision, head, base, plan, tip) = audit_input(i);
    build_audit_report(
        &job,
        &AuditInput {
            revision,
            head_sha: &head,
            base_sha: base.as_deref(),
            plan_hash: plan.as_deref(),
            branch_tip: tip.as_deref(),
        },
    )
}

fn offer_value(into: &Option<String>, withheld: &Option<Withheld>) -> JValue {
    obj([
        ("into", into.as_deref().map_or(JValue::Null, s)),
        (
            "withheld",
            withheld.as_ref().map_or(JValue::Null, Withheld::to_value),
        ),
    ])
}

fn accept_row(sc: &JValue) -> JValue {
    let report = audit(sc.get("audit")).unwrap();
    let write_up = match sc.get("rawWriteUp") {
        JValue::Null => None,
        raw => Some(read_audit(raw).unwrap()),
    };
    let verdict = read_verdict(sc.get("rawVerdict")).unwrap();
    let pre = sc.get("pre").clone();
    let (into, withheld) = merge_offer(sc.get("merge") == &JValue::Bool(true), &report, || {
        if pre.get("ok") == &JValue::Bool(true) {
            Ok(pre.get("baseBranch").as_str().unwrap().to_string())
        } else {
            Err((
                pre.get("code").as_str().unwrap().into(),
                pre.get("reason").as_str().unwrap().into(),
            ))
        }
    });
    let files: Vec<String> = match sc.get("files") {
        JValue::Arr(xs) => xs.iter().map(|x| x.as_str().unwrap().to_string()).collect(),
        _ => panic!("files"),
    };
    let network = match sc.get("network") {
        JValue::Arr(xs) => xs.clone(),
        _ => panic!("network"),
    };
    let head = sc.get("headSha").as_str().unwrap();
    let a = Accept {
        task_id: sc.get("taskId").as_str().unwrap(),
        revision: u(sc.get("n")),
        branch: sc.get("branch").as_str().unwrap(),
        base_sha: sc.get("baseSha").as_str().unwrap(),
        head_sha: head,
        files: &files,
        job_review: sc.get("jobReview"),
        verdict: &verdict,
        report: &report,
        write_up: write_up.as_ref(),
        plan_hash: sc.get("planHash").as_str().unwrap(),
        into: into.as_deref(),
        withheld: withheld.as_ref(),
        decision: sc.get("answer"),
        loops: u(sc.get("loops")),
        network: &network,
    };
    let result = accept_result(&a);
    let mut merged = Vec::new();
    for record_failed in [false, true] {
        for stage_throws in [false, true] {
            let branch = into.as_deref().filter(|i| !i.is_empty()).unwrap_or("main");
            merged.push(obj([
                ("recordFailed", JValue::Bool(record_failed)),
                ("stageThrows", JValue::Bool(stage_throws)),
                ("stageReason", s(&merged_stage_reason(branch))),
                (
                    "out",
                    merged_result(&result, branch, !record_failed && !stage_throws),
                ),
            ]));
        }
    }
    let (message, refused) = merge_refused(&result, "base_moved", "QA main moved.");
    let refused_blocked = blocked_result(a.revision, Some(head), a.loops, &network, Some(&refused));
    obj([
        ("into", into.as_deref().map_or(JValue::Null, s)),
        (
            "withheld",
            withheld.as_ref().map_or(JValue::Null, Withheld::to_value),
        ),
        ("request", accept_card(&a)),
        ("result", result.clone()),
        ("mergeOff", merge_turned_off_result(&result)),
        ("merged", JValue::Arr(merged)),
        (
            "refused",
            obj([("message", s(&message)), ("result", refused)]),
        ),
        ("refusedBlocked", refused_blocked),
        ("shouldMerge", JValue::Bool(should_merge(&a))),
    ])
}

fn plan_failure(sc: &JValue) -> JValue {
    let row = |code: &str, reason: &str, problems: JValue| {
        obj([
            ("ok", JValue::Bool(false)),
            ("code", s(code)),
            ("reason", s(reason)),
            ("problems", problems),
        ])
    };
    let simple = |(c, r): (String, String)| row(&c, &r, JValue::Undefined);
    if sc.get("aborted").truthy() {
        return simple(failure(planner::PLAN_CANCELLED));
    }
    if sc.get("throws").truthy() {
        return simple(failure(planner::PLAN_UNREACHABLE));
    }
    if let Some(ms) = sc.get("timeout").as_num() {
        return simple((
            planner::PLAN_TIMEOUT_CODE.into(),
            plan_timeout_reason(ms as u64),
        ));
    }
    if let Some(t) = sc.get("text").as_str() {
        let Ok(plan) = wtf16::parse(t) else {
            return simple(failure(planner::PLAN_UNREADABLE));
        };
        let problems = validate_plan_artifact(&plan, None);
        if problems.is_empty() {
            return obj([
                ("ok", JValue::Bool(true)),
                ("code", JValue::Undefined),
                ("reason", JValue::Undefined),
                ("problems", JValue::Undefined),
            ]);
        }
        let f = planner::PLAN_NOT_VALID;
        return row(f.code, f.reason, strs(&problems));
    }
    let code = sc.get("code").as_str().unwrap();
    simple(plan_session_failure(code).map_or_else(
        || (code.into(), sc.get("reason").as_str().unwrap().into()),
        failure,
    ))
}

fn review_failure(sc: &JValue) -> JValue {
    let simple = |(c, r): (String, String)| {
        obj([
            ("ok", JValue::Bool(false)),
            ("code", s(&c)),
            ("reason", s(&r)),
        ])
    };
    if sc.get("aborted").truthy() {
        return simple(failure(review::REVIEW_CANCELLED_BEFORE));
    }
    if sc.get("throws").truthy() {
        return simple(failure(review::REVIEW_UNREACHABLE));
    }
    if let Some(pin) = sc.get("pin").as_str() {
        return simple((review_pin_code(pin).into(), format!("QA pin {pin}")));
    }
    if let Some(ms) = sc.get("timeout").as_num() {
        return simple((
            review::REVIEW_TIMEOUT_CODE.into(),
            review_timeout_reason(ms as u64),
        ));
    }
    if let Some(t) = sc.get("text").as_str() {
        let Ok(raw) = wtf16::parse(t) else {
            return simple(failure(review::REVIEW_UNREADABLE));
        };
        return match read_verdict(&raw) {
            Ok(_) => obj([
                ("ok", JValue::Bool(true)),
                ("code", JValue::Undefined),
                ("reason", JValue::Undefined),
            ]),
            Err(e) => simple(("invalid".into(), e.message().into())),
        };
    }
    let code = sc.get("code").as_str().unwrap();
    simple(review_session_failure(code).map_or_else(
        || (code.into(), sc.get("reason").as_str().unwrap().into()),
        failure,
    ))
}

/// The port's answer to one row, in the row's `{out}`/`{throws}` form.
fn port(f: &str, a: &JValue) -> JValue {
    match f {
        "validatePlanArtifact" => {
            let caps: Option<Vec<String>> = match arg(a, 1).get("capabilities") {
                JValue::Arr(xs) => Some(
                    xs.iter()
                        .filter_map(|x| x.as_str().map(str::to_string))
                        .collect(),
                ),
                _ => None,
            };
            out(strs(&validate_plan_artifact(&arg(a, 0), caps.as_deref())))
        }
        "renderExecutorBrief" => ok_or_type(render_executor_brief(&arg(a, 0)).map(JValue::Str)),
        "planHash" => {
            let v = arg(a, 0);
            match plan_hash(&v) {
                Some(h) => out(obj([
                    ("serialized", s(&serialize_projection(&v).unwrap())),
                    ("hash", s(&h)),
                ])),
                None => type_error(),
            }
        }
        "minutes" => out(s(&minutes(arg(a, 0).as_num().unwrap() as u64))),
        "stageDeadline" => out(s(&stage_deadline_message(
            arg(a, 0).as_str().unwrap(),
            arg(a, 1).as_num().unwrap() as u64,
        ))),
        "testsFeedback" => out(strs(&tests_failed_feedback(u(&arg(a, 0)), &arg(a, 1)))),
        "reviewFeedback" => out(strs(&review_feedback(&read_verdict(&arg(a, 0)).unwrap()))),
        "loop" => {
            let r = next_round(arg(a, 0).as_str().unwrap(), u(&arg(a, 1)), u(&arg(a, 2)));
            out(match r.next {
                Ok(reason) => obj([("loops", JValue::from(r.loops)), ("reason", s(&reason))]),
                Err(blocked) => obj([("loops", JValue::from(r.loops)), ("blocked", s(&blocked))]),
            })
        }
        "planCard" => ok_or_type(plan_card(
            arg(a, 0).as_str().unwrap(),
            &arg(a, 1),
            arg(a, 2).as_str().unwrap(),
        )),
        "readAudit" => match read_audit(&arg(a, 0)) {
            Ok(w) => out(w.to_value()),
            Err(e) => obj([(
                "throws",
                obj([
                    ("name", s("AuditWriteUpError")),
                    ("message", s(e.message())),
                ]),
            )]),
        },
        "buildAuditReport" => match audit(&arg(a, 0)) {
            Ok(r) => out(r.to_value()),
            Err(AuditReportError::NotAJob) => obj([(
                "throws",
                obj([
                    ("name", s("Error")),
                    ("message", s(AuditReportError::NOT_A_JOB)),
                    (
                        "status",
                        JValue::Num(f64::from(AuditReportError::NOT_A_JOB_STATUS)),
                    ),
                ]),
            )]),
            Err(AuditReportError::Type) => type_error(),
        },
        "accept" => out(accept_row(&arg(a, 0))),
        "blockedResult" => {
            let t = arg(a, 0);
            let network = match arg(a, 1) {
                JValue::Arr(xs) => xs,
                _ => panic!("network"),
            };
            let error = arg(a, 2);
            let extra = error.get("result");
            let head = opt_str(t.get("headSha"));
            out(blocked_result(
                u(t.get("revision")),
                head.as_deref(),
                u(t.get("loops")),
                &network,
                Some(extra),
            ))
        }
        "auditWithheld" => {
            let report = audit(&arg(a, 0)).unwrap();
            let (into, withheld) = merge_offer(true, &report, || Ok("main".into()));
            out(offer_value(&into, &withheld))
        }
        "fullSha" => out(JValue::Bool(is_full_sha(arg(a, 0).as_str().unwrap()))),
        "planFailure" => out(plan_failure(&arg(a, 0))),
        "reviewFailure" => out(review_failure(&arg(a, 0))),
        other => panic!("no port for {other}"),
    }
}

fn node(row: &JValue) -> JValue {
    match (row.get("out"), row.get("throws")) {
        (JValue::Undefined, t) => obj([("throws", t.clone())]),
        (o, _) => obj([("out", o.clone())]),
    }
}

#[test]
fn every_row_matches_node() {
    let f = fixture();
    let JValue::Arr(rows) = f.get("rows") else {
        panic!("rows")
    };
    let mut counts: BTreeMap<String, usize> = BTreeMap::new();
    let mut failures = Vec::new();
    for (i, r) in rows.iter().enumerate() {
        let name = r.get("fn").as_str().unwrap();
        *counts.entry(name.into()).or_default() += 1;
        let want = text(&node(r));
        let got = text(&port(name, r.get("args")));
        if want != got {
            failures.push(format!(
                "row {i} {name} {}\n  node: {want}\n  port: {got}",
                text(r.get("args"))
            ));
        }
    }
    let JValue::Obj(recorded) = f.get("counts") else {
        panic!("counts")
    };
    for (k, n) in recorded {
        assert_eq!(
            counts.get(k).copied(),
            n.as_num().map(|x| x as usize),
            "{k}"
        );
    }
    assert!(
        failures.is_empty(),
        "{} of {} rows differ:\n{}",
        failures.len(),
        rows.len(),
        failures
            .iter()
            .take(20)
            .cloned()
            .collect::<Vec<_>>()
            .join("\n")
    );
}

#[test]
fn sources_are_the_ones_ported() {
    let f = fixture();
    let src = f.get("source");
    for (file, sha) in [
        ("code-pipeline.cjs", CODE_PIPELINE_SHA256),
        ("planner-plan.cjs", PLANNER_PLAN_SHA256),
        ("code-review.cjs", CODE_REVIEW_SHA256),
        ("auditor-report.cjs", AUDITOR_REPORT_SHA256),
        ("role-context.cjs", ROLE_CONTEXT_SHA256),
        ("code-actions.cjs", CODE_ACTIONS_SHA256),
        ("code-review-verdict.cjs", CODE_REVIEW_VERDICT_SHA256),
    ] {
        assert_eq!(src.get(file).as_str(), Some(sha), "{file}");
    }
}

#[test]
fn constants_match_node() {
    let f = fixture();
    let c = decode(f.get("constants"));
    let n = |x: u64| JValue::Num(x as f64);
    let list = |xs: &[&str]| JValue::Arr(xs.iter().map(|x| s(x)).collect());
    let mine = [
        ("EXECUTOR_INSTRUCTIONS", s(EXECUTOR_INSTRUCTIONS)),
        ("PIPELINE_META", s(PIPELINE_META)),
        ("PLAN_ACTION", s(PLAN_ACTION)),
        ("EXPECTED_ARTIFACTS", list(&EXPECTED_ARTIFACTS)),
        (
            "DEADLINES",
            obj([
                ("plan", n(DEADLINES.plan)),
                ("implement", n(DEADLINES.implement)),
                ("verify", n(DEADLINES.verify)),
                ("review", n(DEADLINES.review)),
                ("audit", n(DEADLINES.audit)),
            ]),
        ),
        ("MAX_LOOPS", n(u64::from(MAX_LOOPS))),
        (
            "CAPABILITY_TEXT",
            JValue::obj(CAPABILITY_TEXT.iter().map(|(k, v)| (*k, s(v)))),
        ),
        ("BLOCKED", list(&blocked::ALL)),
        ("PLANNER_INSTRUCTIONS", s(planner::INSTRUCTIONS)),
        ("MAX_PLAN_TOKENS", n(planner::MAX_PLAN_TOKENS as u64)),
        (
            "PLANNER_DEFAULT_DEADLINE_MS",
            n(planner::DEFAULT_DEADLINE_MS),
        ),
        ("MAX_PLAN_BYTES", n(planner::MAX_PLAN_BYTES as u64)),
        ("MAX_REPLY_TOKENS", n(planner::MAX_REPLY_TOKENS as u64)),
        ("REVIEW_ACTION", s(REVIEW_ACTION)),
        ("REVIEW_INSTRUCTIONS", s(review::INSTRUCTIONS)),
        ("REVIEW_DEFAULT_DEADLINE_MS", n(review::DEFAULT_DEADLINE_MS)),
        ("MAX_VERDICT_BYTES", n(review::MAX_VERDICT_BYTES as u64)),
        ("AUDIT_SCHEMA", audit_schema()),
        ("AUDIT_INSTRUCTIONS", s(AUDIT_INSTRUCTIONS)),
        ("AUDIT_SOURCES", list(&AUDIT_SOURCES)),
        ("FULL_SHA", s("/^(?:[0-9a-f]{40}|[0-9a-f]{64})$/")),
    ];
    for (k, v) in mine {
        assert_eq!(text(c.get(k)), text(&v), "{k}");
    }
    // Each module's exports as the port was written against them: one added or removed in Node
    // fails here until the port follows.
    let exports = obj([
        (
            "pipeline",
            list(&[
                "createCodePipeline",
                "renderExecutorBrief",
                "EXECUTOR_INSTRUCTIONS",
                "PIPELINE_META",
                "PLAN_ACTION",
                "EXPECTED_ARTIFACTS",
                "DEADLINES",
                "MAX_LOOPS",
            ]),
        ),
        (
            "planner",
            list(&[
                "createPlannerPlan",
                "validatePlanArtifact",
                "INSTRUCTIONS",
                "MAX_PLAN_TOKENS",
                "DEFAULT_DEADLINE_MS",
            ]),
        ),
        (
            "review",
            list(&[
                "createPlannerReview",
                "createEngineReviewer",
                "REVIEW_ACTION",
                "INSTRUCTIONS",
                "DEFAULT_DEADLINE_MS",
            ]),
        ),
        (
            "auditor",
            list(&[
                "AUDIT_SCHEMA",
                "AUDIT_INSTRUCTIONS",
                "AUDIT_SOURCES",
                "AuditWriteUpError",
                "readAudit",
                "buildAuditReport",
                "pipelineView",
                "FULL_SHA",
            ]),
        ),
    ]);
    assert_eq!(text(c.get("exports")), text(&exports), "exports");
    let JValue::Obj(keys) = &c else {
        panic!("constants")
    };
    assert_eq!(keys.len(), 22, "every recorded constant is compared");
    for (name, t) in CAPABILITY_TEXT {
        assert_eq!(capability_text(name), Some(t));
    }
    assert_eq!(capability_text("open_browser"), None);
}
