//! Native Planner generation and review hooks. A role answer is validated advice; these hooks
//! never decide an approval, widen capabilities, write lifecycle stages, or merge a branch.
use crate::{
    context::{JournalContext, StageStop},
    planner_contract,
    role_pin::Session,
    role_prompt,
    role_stream::Call,
};
use js_json::{wtf16, JValue};
use std::{future::Future, sync::Arc, time::Duration};
pub const DEFAULT_DEADLINE: Duration = Duration::from_secs(180);
fn text(s: impl Into<String>) -> JValue {
    JValue::Str(s.into())
}
fn object(fields: Vec<(&str, JValue)>) -> JValue {
    JValue::Obj(fields.into_iter().map(|(k, v)| (k.into(), v)).collect())
}
fn fail(code: &str, reason: impl Into<String>) -> JValue {
    object(vec![
        ("ok", JValue::Bool(false)),
        ("code", text(code)),
        ("reason", text(reason)),
    ])
}
fn add(value: &mut JValue, key: &str, item: JValue) {
    if let JValue::Obj(fields) = value {
        wtf16::set(fields, key, item);
    }
}
async fn within<T>(stop: &StageStop, limit: Duration, work: impl Future<Output = T>) -> Option<T> {
    tokio::pin!(work);
    tokio::select! {biased;result=&mut work=>Some(result),_=tokio::time::sleep(limit)=>{stop.cancel();let _=work.await;None}}
}
fn seconds(limit: Duration) -> u128 {
    (limit.as_millis() + 500) / 1000
}
fn call<'a>(
    role: &'a str,
    state: &'a JValue,
    schema: &'a JValue,
    instructions: &'a str,
    name: &'a str,
    constrain: bool,
    budget: (u32, u32),
) -> Call<'a> {
    Call {
        role,
        state,
        roles: &[],
        model: "",
        thinking: false,
        instructions,
        schema,
        schema_name: name,
        constrain,
        schema_supported: false,
        reasoning_model: false,
        max_tokens: budget.0,
        max_bytes: budget.1,
    }
}
pub async fn plan(
    session: &Session,
    context: Arc<JournalContext>,
    state: &JValue,
    constrain: bool,
    limit: Duration,
) -> JValue {
    if *context.cancellation().borrow() {
        return fail("aborted", "The plan was cancelled.");
    }
    let Some(contracts) = planner_contract::contracts() else {
        return fail("error", "The Planner model could not be reached.");
    };
    let (staged, stop) = context.stage();
    let request = call(
        "planner",
        state,
        contracts.get("planSchema"),
        contracts.get("plannerInstructions").as_str().unwrap_or(""),
        "plan_artifact",
        constrain,
        (1400, 32768),
    );
    let Some(answer) = within(&stop, limit, session.call(request, staged.cancellation())).await
    else {
        return fail(
            "timeout",
            format!("The plan did not finish within {} seconds.", seconds(limit)),
        );
    };
    let answer = match answer {
        Ok(answer) => answer,
        Err(error) => {
            if error.code == "aborted" || *context.cancellation().borrow() {
                return fail("aborted", "The plan was cancelled.");
            }
            if error.code == "context_refused" {
                return fail("context_refused","The task was not sent to the Planner because its context would have carried private data.");
            }
            if error.code == "invalid" {
                let mut failed=fail("invalid","The Planner’s answer did not match the plan format, even after one correction.");
                add(
                    &mut failed,
                    "constraint",
                    error.constraint.unwrap_or_else(|| {
                        object(vec![
                            ("applied", JValue::Bool(false)),
                            ("mode", JValue::Null),
                            ("reason", JValue::Null),
                            ("fallback", JValue::Bool(false)),
                        ])
                    }),
                );
                return failed;
            }
            return fail(&error.code, error.reason);
        }
    };
    let Ok(plan) = wtf16::parse(&answer.text) else {
        return fail("invalid", "The Planner did not return a readable plan.");
    };
    let projection = match role_prompt::role("planner", state) {
        Ok(p) => p,
        Err(_) => return fail("error", "The Planner model could not be reached."),
    };
    let given = match projection.get("capabilities") {
        JValue::Arr(caps) => Some(
            caps.iter()
                .filter_map(|c| c.get("name").as_str().map(str::to_owned))
                .collect::<Vec<_>>(),
        ),
        _ => None,
    };
    let problems = planner_contract::validate(&plan, given.as_deref());
    if !problems.is_empty() {
        let mut failed = fail("invalid", "The Planner’s plan did not pass validation.");
        add(
            &mut failed,
            "problems",
            JValue::Arr(problems.into_iter().map(text).collect()),
        );
        add(&mut failed, "constraint", answer.constraint);
        return failed;
    }
    object(vec![
        ("ok", JValue::Bool(true)),
        ("plan", plan),
        ("constraint", answer.constraint),
        ("corrected", JValue::Bool(answer.corrected)),
    ])
}
pub async fn review(
    session: &Session,
    context: Arc<JournalContext>,
    state: &JValue,
    limit: Duration,
) -> JValue {
    if *context.cancellation().borrow() {
        return fail("aborted", "The task was cancelled before it was reviewed.");
    }
    let Some(contracts) = planner_contract::contracts() else {
        return fail("error", "The reviewer could not be reached.");
    };
    let (staged, stop) = context.stage();
    let request = call(
        "reviewer",
        state,
        contracts.get("reviewSchema"),
        contracts.get("reviewerInstructions").as_str().unwrap_or(""),
        "planner_review",
        true,
        (4096, 65536),
    );
    let Some(answer) = within(&stop, limit, session.call(request, staged.cancellation())).await
    else {
        return fail(
            "timeout",
            format!(
                "The review did not finish within {} seconds.",
                seconds(limit)
            ),
        );
    };
    if *context.cancellation().borrow() {
        return fail(
            "aborted",
            "The task was cancelled before the review finished.",
        );
    }
    let answer=match answer{Ok(answer)=>answer,Err(error)=>return match error.code.as_str(){"context_refused"=>fail("context_refused","The change was not sent for review because it would have carried private data."),"too_large"=>fail("too_large","The change is too large to review."),"invalid"=>fail("invalid","The reviewer’s answer did not match the verdict format, even after one correction."),"aborted"=>fail("aborted","The task was cancelled before the review finished."),"error"=>fail("error","The reviewer could not be reached."),_=>fail(&error.code,error.reason)}};
    let Ok(parsed) = wtf16::parse(&answer.text) else {
        return fail("invalid", "The reviewer did not return a readable verdict.");
    };
    match server_projects::task_journals::read_review_verdict(&parsed) {
        Ok(verdict) => object(vec![
            ("ok", JValue::Bool(true)),
            ("verdict", verdict),
            ("corrected", JValue::Bool(answer.corrected)),
        ]),
        Err(reason) => fail("invalid", reason),
    }
}
/// The Auditor's optional prose is returned separately from the deterministic measured report.
pub async fn audit(
    session: &Session,
    context: Arc<JournalContext>,
    state: &JValue,
    constrain: bool,
) -> Result<JValue, String> {
    let contracts = planner_contract::contracts().ok_or_else(|| "error".to_owned())?;
    let request = call(
        "auditor",
        state,
        contracts.get("auditSchema"),
        contracts.get("auditorInstructions").as_str().unwrap_or(""),
        "auditor_report",
        constrain,
        (2048, 32768),
    );
    let answer = session
        .call(request, context.cancellation())
        .await
        .map_err(|e| e.code)?;
    let parsed = wtf16::parse(&answer.text).map_err(|_| "invalid".to_owned())?;
    crate::audit::read_writeup(&parsed).map_err(|_| "invalid".to_owned())
}
#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;
    use crate::role_pin::{Authority, Endpoint, Fault};
    use crate::role_stream::{Body, Error, Response, Wait};
    use crate::session::Context;
    use std::{
        collections::VecDeque,
        sync::{
            atomic::{AtomicUsize, Ordering},
            Mutex,
        },
    };
    use tokio::sync::watch;
    struct Stream {
        chunks: VecDeque<Vec<u8>>,
    }
    impl Body for Stream {
        fn next(&mut self) -> Wait<'_, Result<Option<Vec<u8>>, Error>> {
            Box::pin(async { Ok(self.chunks.pop_front()) })
        }
    }
    struct Host {
        output: String,
        requests: Mutex<Vec<JValue>>,
        waiting: bool,
        entered: AtomicUsize,
        dropped: Arc<AtomicUsize>,
    }
    struct Pending(Arc<AtomicUsize>);
    impl Drop for Pending {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }
    impl Authority for Host {
        fn endpoint(&self) -> Result<Endpoint, Fault> {
            Ok(Endpoint {
                configured: true,
                external: false,
                model: Some("synthetic-model".into()),
                machine: None,
                schema_supported: false,
                reasoning_model: false,
            })
        }
        fn loaded(
            &self,
            _: &Endpoint,
            _: watch::Receiver<bool>,
        ) -> Wait<'_, Result<Vec<String>, Fault>> {
            Box::pin(async { Ok(vec![]) })
        }
        fn ready(&self, _: &Endpoint, _: watch::Receiver<bool>) -> Wait<'_, Result<(), Fault>> {
            Box::pin(async { Ok(()) })
        }
        fn budget(
            &self,
            _: &str,
            _: watch::Receiver<bool>,
        ) -> Wait<'_, Result<Option<String>, Fault>> {
            Box::pin(async { Ok(None) })
        }
        fn send(
            &self,
            _: &Endpoint,
            payload: JValue,
            _: watch::Receiver<bool>,
        ) -> Wait<'_, Result<Response, Error>> {
            Box::pin(async move {
                self.requests.lock().unwrap().push(payload);
                self.entered.fetch_add(1, Ordering::SeqCst);
                let _pending = Pending(self.dropped.clone());
                if self.waiting {
                    return std::future::pending().await;
                }
                let data = object(vec![(
                    "choices",
                    JValue::Arr(vec![object(vec![(
                        "delta",
                        object(vec![("content", text(&self.output))]),
                    )])]),
                )]);
                let raw = format!(
                    "data: {}\n\ndata: [DONE]\n\n",
                    wtf16::stringify(&data).unwrap()
                );
                Ok(Response {
                    status: 200,
                    body: Some(Box::new(Stream {
                        chunks: VecDeque::from([raw.into_bytes()]),
                    })),
                })
            })
        }
    }
    fn host(output: String, waiting: bool) -> Arc<Host> {
        Arc::new(Host {
            output,
            requests: Mutex::new(vec![]),
            waiting,
            entered: AtomicUsize::new(0),
            dropped: Arc::new(AtomicUsize::new(0)),
        })
    }
    #[tokio::test]
    async fn exact_node_planner_and_review_hooks_use_native_model_streams() {
        let fixtures =
            wtf16::parse(include_str!("../tests/fixtures/role-steps-node.json")).unwrap();
        let JValue::Arr(fixtures) = fixtures else {
            panic!()
        };
        for fixture in fixtures {
            let (_root, context, _head) = crate::verify::tests::context();
            let output = if fixture.get("output").as_str() == Some("bad") {
                String::from("bad")
            } else {
                wtf16::stringify(fixture.get("output")).unwrap()
            };
            let host = host(output, false);
            let session = crate::role_pin::pin(
                host.clone(),
                context.task(),
                None,
                false,
                role_prompt::ROLES.map(str::to_owned).into(),
                context.cancellation(),
            )
            .await
            .unwrap();
            let result = if fixture.get("kind").as_str() == Some("plan") {
                plan(
                    &session,
                    context.clone(),
                    fixture.get("state"),
                    false,
                    DEFAULT_DEADLINE,
                )
                .await
            } else {
                review(
                    &session,
                    context.clone(),
                    fixture.get("state"),
                    DEFAULT_DEADLINE,
                )
                .await
            };
            assert_eq!(result, *fixture.get("result"));
            assert!(context.active().is_ok());
            assert_eq!(
                host.requests.lock().unwrap().len(),
                if fixture.get("output").as_str() == Some("bad") {
                    2
                } else {
                    1
                }
            );
        }
    }
    #[tokio::test]
    async fn deadline_drops_actual_engine_wait_and_keeps_parent_task_active() {
        let (_root, context, _head) = crate::verify::tests::context();
        let host = host(String::new(), true);
        let session = crate::role_pin::pin(
            host.clone(),
            context.task(),
            None,
            false,
            role_prompt::ROLES.map(str::to_owned).into(),
            context.cancellation(),
        )
        .await
        .unwrap();
        let state = object(vec![
            ("taskId", text(context.task())),
            ("tenantId", text(&context.scope().user)),
            ("request", text("Change widget")),
        ]);
        let result = plan(
            &session,
            context.clone(),
            &state,
            false,
            Duration::from_millis(100),
        )
        .await;
        assert_eq!(result.get("code").as_str(), Some("timeout"));
        assert_eq!(
            result.get("reason").as_str(),
            Some("The plan did not finish within 0 seconds.")
        );
        assert_eq!(host.entered.load(Ordering::SeqCst), 1);
        assert_eq!(host.dropped.load(Ordering::SeqCst), 1);
        assert!(context.active().is_ok());
    }
}
