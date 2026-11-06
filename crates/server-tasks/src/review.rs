//! Reviewed ordinary code tasks. The model verdict remains advice on a fresh human card; no
//! verdict, failure, timeout or standing tool permission can answer that card.
use crate::{
    code::{Failure, Run, Task},
    context::JournalContext,
    execution,
    resources::Resources,
    role_pin::Models,
    role_steps,
    session::Context,
    workspace, Decision,
};
use js_json::{wtf16, JValue};
use std::{sync::Arc, time::Duration};
pub struct Review {
    pub models: Option<Arc<dyn Models>>,
    pub deadline: Duration,
}
fn text(s: impl Into<String>) -> JValue {
    JValue::Str(s.into())
}
fn object(fields: Vec<(&str, JValue)>) -> JValue {
    JValue::Obj(fields.into_iter().map(|(k, v)| (k.into(), v)).collect())
}
fn set(value: &mut JValue, key: &str, item: JValue) {
    if let JValue::Obj(fields) = value {
        wtf16::set(fields, key, item);
    }
}
fn error(code: &str, reason: impl Into<String>) -> JValue {
    object(vec![
        ("ok", JValue::Bool(false)),
        ("code", text(code)),
        ("reason", text(reason)),
    ])
}
fn emit(context: &JournalContext, kind: &str, data: JValue) -> Result<(), Failure> {
    context
        .event(kind, data)
        .map_err(|_| Failure::unavailable())
}
fn files(change: &JValue) -> Vec<JValue> {
    match change.get("files") {
        JValue::Arr(files) => files.iter().map(|f| f.get("path").clone()).collect(),
        _ => vec![],
    }
}
fn summary(result: &JValue) -> String {
    let n = |key| {
        js_json::number_to_string(
            result
                .get(key)
                .as_num()
                .filter(|n| n.is_finite() && n.fract() == 0.0)
                .unwrap_or(0.0),
        )
    };
    format!(
        "{} tool calls; {} allowed, {} declined by the person, {} refused by noevia.",
        n("tools"),
        n("allowed"),
        n("refused"),
        n("denied")
    )
}
pub(crate) fn credentials(authority: &dyn crate::role_pin::Authority) -> JValue {
    JValue::Obj(
        authority
            .credentials()
            .iter()
            .enumerate()
            .map(|(i, s)| (format!("engine{i}"), text(s.as_str())))
            .collect(),
    )
}
impl execution::Review for Review {
    fn review<'a>(
        &'a self,
        context: Arc<JournalContext>,
        resources: &'a Resources,
        task: &'a Task,
        mut result: JValue,
    ) -> Run<'a> {
        Box::pin(async move {
            if *context.cancellation().borrow() {
                return Ok(result);
            }
            let base = resources.workspace.get("baseSha").clone();
            let head = resources
                .released()
                .and_then(|r| r.workspace.as_ref())
                .map_or(JValue::Null, |r| r.get("headSha").clone());
            let flag = context.blocking_cancellation();
            let (change, mut outcome) =
                match context
                    .account()
                    .workspaces
                    .change(context.task(), 200, flag.flag())
                {
                    Ok(change) => (change, None),
                    Err(error) => {
                        let (code, message) = match error {
                            workspace::Error::Coded { code, message } => (code, message),
                            _ => ("no_change", "The change could not be read.".into()),
                        };
                        (JValue::Null, Some(error_value(code, &message)))
                    }
                };
            drop(flag);
            let names = files(&change);
            emit(
                &context,
                "review.requested",
                object(vec![
                    ("baseSha", base.clone()),
                    ("headSha", head.clone()),
                    (
                        "files",
                        if change == JValue::Null {
                            JValue::Null
                        } else {
                            JValue::Num(names.len() as f64)
                        },
                    ),
                ]),
            )?;
            if outcome.is_none() {
                outcome = Some(
                    match self
                        .models
                        .as_ref()
                        .map(|models| models.bind(context.clone()))
                    {
                        None => error(
                            "unavailable",
                            "No reviewer model is configured on this server.",
                        ),
                        Some(Err(_)) => error(
                            "unavailable",
                            "The review context could not be verified, so it was not sent.",
                        ),
                        Some(Ok(authority)) => {
                            let job = context.job()?;
                            let mut state = object(vec![
                                ("taskId", text(context.task())),
                                ("tenantId", text(&context.scope().user)),
                                ("request", text(&task.request.prompt)),
                                (
                                    "capabilities",
                                    JValue::Arr(
                                        task.request.capabilities.iter().map(text).collect(),
                                    ),
                                ),
                                (
                                    "execution",
                                    object(vec![
                                        ("headSha", head.clone()),
                                        ("changedFiles", JValue::Arr(names.clone())),
                                        ("summary", text(summary(&result))),
                                    ]),
                                ),
                                ("change", change.clone()),
                                ("credentials", credentials(authority.as_ref())),
                            ]);
                            if let JValue::Arr(entries) = job.plan.get("subQuestions") {
                                if !entries.is_empty() {
                                    set(
                                        &mut state,
                                        "plan",
                                        object(vec![(
                                            "steps",
                                            JValue::Arr(
                                                entries
                                                    .iter()
                                                    .map(|entry| {
                                                        object(vec![("do", entry.clone())])
                                                    })
                                                    .collect(),
                                            ),
                                        )]),
                                    );
                                }
                            }
                            if let Some(grant) = resources.grant() {
                                set(&mut state, "tokens", JValue::Arr(vec![text(grant.token())]));
                            }
                            role_steps::review_start(
                                authority,
                                context.clone(),
                                &state,
                                self.deadline,
                            )
                            .await
                        }
                    },
                );
            }
            if *context.cancellation().borrow() {
                return Ok(result);
            }
            let outcome = outcome.ok_or_else(Failure::unavailable)?;
            let reviewed = outcome.get("ok") == &JValue::Bool(true);
            let mut record = object(vec![("baseSha", base.clone()), ("headSha", head.clone())]);
            if reviewed {
                if let JValue::Obj(fields) = outcome.get("verdict") {
                    for (k, v) in fields {
                        set(&mut record, k, v.clone());
                    }
                }
                set(
                    &mut record,
                    "corrected",
                    JValue::Bool(outcome.get("corrected") == &JValue::Bool(true)),
                );
                emit(&context, "review.completed", record)?;
            } else {
                set(&mut record, "code", outcome.get("code").clone());
                set(&mut record, "reason", outcome.get("reason").clone());
                emit(&context, "review.failed", record)?;
            }
            let recorded = context.job()?.review.unwrap_or(JValue::Null);
            let card = object(vec![
                ("taskId", text(context.task())),
                ("action", text("review_change")),
                ("title", text("Accept this change")),
                ("kind", text("review")),
                ("command", text("")),
                (
                    "paths",
                    JValue::Arr(names.iter().take(50).cloned().collect()),
                ),
                (
                    "reason",
                    text(if reviewed {
                        "The Planner’s verdict is advice. Accepting records this reviewed head as accepted; nothing is merged automatically.".into()
                    } else {
                        format!("Not reviewed: {} Review the change on its branch yourself before accepting it.",outcome.get("reason").as_str().unwrap_or(""))
                    }),
                ),
                (
                    "arguments",
                    object(vec![
                        ("branch", resources.workspace.get("branch").clone()),
                        ("baseSha", base),
                        ("headSha", head.clone()),
                        ("files", JValue::Arr(names)),
                    ]),
                ),
                ("diff", JValue::Null),
                ("review", recorded),
            ]);
            let answer = context
                .request_approval(card)
                .map_err(|_| Failure::unavailable())?
                .await;
            emit(
                &context,
                "approval.decided",
                object(vec![
                    ("decision", text(answer.as_str())),
                    ("action", text("review_change")),
                ]),
            )?;
            set(
                &mut result,
                "review",
                object(vec![
                    ("reviewed", JValue::Bool(reviewed)),
                    (
                        "verdict",
                        if reviewed {
                            outcome.get("verdict").get("verdict").clone()
                        } else {
                            JValue::Null
                        },
                    ),
                    (
                        "accepted",
                        JValue::Bool(matches!(answer, Decision::Approve | Decision::ApproveAll)),
                    ),
                    ("decision", text(answer.as_str())),
                    ("headSha", head),
                ]),
            );
            Ok(result)
        })
    }
}
fn error_value(code: &str, message: &str) -> JValue {
    error(code, wtf16::slice(message, 300))
}
#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;
    use crate::{
        code::Settings, execution::Review as _, facts::Repository, role_pin::Authority,
        start::Request,
    };
    use std::{
        path::Path,
        sync::atomic::{AtomicBool, AtomicUsize, Ordering},
    };
    struct ModelsFixture {
        authority: Arc<dyn Authority>,
        bound: AtomicUsize,
    }
    impl Models for ModelsFixture {
        fn bind(&self, context: Arc<JournalContext>) -> Result<Arc<dyn Authority>, Failure> {
            assert_eq!(context.scope().user, "11111111-1111-4111-8111-111111111111");
            self.bound.fetch_add(1, Ordering::SeqCst);
            Ok(self.authority.clone())
        }
    }
    fn normalize(value: &JValue, task: &str, base: &str, head: &str) -> JValue {
        let mut encoded = wtf16::stringify(value)
            .unwrap()
            .replace(task, "TASK")
            .replace(base, "BASE");
        if head != base {
            encoded = encoded.replace(head, "HEAD");
        }
        wtf16::parse(&encoded).unwrap()
    }
    #[tokio::test]
    async fn exact_node_review_gate_cards_results_and_decisions() {
        let fixtures =
            wtf16::parse(include_str!("../tests/fixtures/review-gate-node.json")).unwrap();
        let JValue::Arr(fixtures) = fixtures else {
            panic!()
        };
        for fixture in fixtures {
            let input = fixture.get("input");
            let limit = if input.get("decision").as_str() == Some("timeout") {
                Duration::from_millis(250)
            } else {
                Duration::from_secs(5)
            };
            let (root, context, base) = crate::verify::tests::context_with_timeout(limit);
            let request = Request {
                repository: Repository {
                    id: "repo".into(),
                    path: root.path().join("repo"),
                },
                prompt: "fix the median bug".into(),
                capabilities: vec!["read_repository".into(), "edit_file".into()],
                domains: vec![],
                harness: "opencode".into(),
                preparation: "direct".into(),
                model: None,
                approve_plan: false,
            };
            let mut resources = Resources::claim(
                context.clone(),
                &request,
                None,
                true,
                &AtomicBool::new(false),
            )
            .await
            .unwrap();
            let cwd = resources.workspace.get("path").as_str().unwrap();
            if input.get("unchanged") != &JValue::Bool(true) {
                std::fs::write(Path::new(cwd).join("median.js"), "synthetic change\n").unwrap();
                crate::verify::tests::git(Path::new(cwd), &["add", "median.js"]);
                crate::verify::tests::git(Path::new(cwd), &["commit", "-qm", "change"]);
            }
            resources.release().await;
            let head = resources
                .released()
                .unwrap()
                .workspace
                .as_ref()
                .unwrap()
                .get("headSha")
                .as_str()
                .unwrap()
                .to_owned();
            let models = Arc::new(ModelsFixture {
                authority: crate::role_steps::tests::authority(
                    r#"{"verdict":"approve","summary":"Fine","findings":[]}"#.into(),
                ),
                bound: AtomicUsize::new(0),
            });
            let reviewer = Review {
                models: if input.get("unavailable") == &JValue::Bool(true) {
                    None
                } else {
                    Some(models.clone())
                },
                deadline: Duration::from_secs(5),
            };
            let task = Task {
                request,
                context: String::new(),
                shared_context: false,
                settings: Settings {
                    reviewed: true,
                    ..Settings::default()
                },
            };
            let result = object(vec![
                ("tools", JValue::Num(0.0)),
                ("allowed", JValue::Num(0.0)),
                ("refused", JValue::Num(0.0)),
                ("denied", JValue::Num(0.0)),
            ]);
            let work = reviewer.review(context.clone(), &resources, &task, result);
            tokio::pin!(work);
            let card_id=tokio::time::timeout(Duration::from_secs(5),async{loop{tokio::select!{result=&mut work=>panic!("review ended before its card: {result:?}"),_=tokio::time::sleep(Duration::from_millis(2))=>{let cards=context.account().control.cards(context.scope()).unwrap();if let Some((id,_,_))=cards.first(){break id.clone();}}}}}).await.unwrap();
            match input.get("decision").as_str().unwrap() {
                "timeout" => {}
                "aborted" => context
                    .account()
                    .control
                    .end_approvals(context.scope(), context.task())
                    .unwrap(),
                decision => context
                    .account()
                    .control
                    .decide(context.scope(), context.task(), &card_id, decision)
                    .unwrap(),
            }
            let result = tokio::time::timeout(Duration::from_secs(5), work)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(
                normalize(result.get("review"), context.task(), &base, &head),
                *fixture.get("result")
            );
            let raw = std::fs::read_to_string(
                root.path()
                    .join("users")
                    .join(&context.scope().user)
                    .join("jobs")
                    .join(format!("{}.jsonl", context.task())),
            )
            .unwrap();
            let events = JValue::Arr(
                raw.lines()
                    .map(|line| wtf16::parse(line).unwrap())
                    .filter(|e| {
                        e.get("type")
                            .as_str()
                            .is_some_and(|s| s.starts_with("review.") || s.starts_with("approval."))
                    })
                    .map(|e| {
                        object(vec![
                            ("type", e.get("type").clone()),
                            ("data", e.get("data").clone()),
                        ])
                    })
                    .collect(),
            );
            assert_eq!(
                normalize(&events, context.task(), &base, &head),
                *fixture.get("events")
            );
            assert!(context.active().is_ok());
            if input.get("unchanged") == &JValue::Bool(true) {
                assert_eq!(models.bound.load(Ordering::SeqCst), 0);
            }
        }
    }
}
