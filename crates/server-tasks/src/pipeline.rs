//! Native Planner → Executor → measured verification → review → audit actor. One task owns the
//! branch and all grants through every round; only a fresh human answer can accept or merge.
use crate::{
    audit,
    code::{Failure, Program, Run, Task},
    context::JournalContext,
    egress::Egress,
    execution::{Executor, Invocation},
    planner_contract,
    resources::Resources,
    review,
    role_pin::{self, Models},
    role_prompt, role_steps,
    session::Context,
    verify::Verify,
    workspace, Decision,
};
use js_json::{wtf16, JValue};
use sha2::{Digest, Sha256};
use std::{future::Future, sync::Arc, time::Duration};
use zeroize::Zeroizing;
pub trait Flag: Send + Sync {
    fn enabled(&self) -> bool;
}
impl<F: Fn() -> bool + Send + Sync> Flag for F {
    fn enabled(&self) -> bool {
        self()
    }
}
pub struct Limits {
    pub plan: Duration,
    pub implement: Duration,
    pub verify: Duration,
    pub review: Duration,
    pub audit: Duration,
}
impl Default for Limits {
    fn default() -> Self {
        Self {
            plan: Duration::from_secs(300),
            implement: Duration::from_secs(2700),
            verify: Duration::from_secs(900),
            review: Duration::from_secs(300),
            audit: Duration::from_secs(180),
        }
    }
}
pub struct Pipeline {
    pub executor: Arc<Executor>,
    pub models: Option<Arc<dyn Models>>,
    pub verify: Arc<Verify>,
    pub egress: Option<Arc<dyn Egress>>,
    pub merge: Arc<dyn Flag>,
    pub constrain: Arc<dyn Flag>,
    pub audit_writeup: bool,
    pub max_loops: u32,
    pub limits: Limits,
}
struct State {
    lifecycle: String,
    revision: u32,
    head: Option<String>,
    loops: u32,
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
fn yes(value: &JValue) -> bool {
    value == &JValue::Bool(true)
}
fn emit(ctx: &JournalContext, kind: &str, data: JValue) -> Result<(), Failure> {
    ctx.event(kind, data).map_err(|_| Failure::unavailable())
}
fn block(reason: impl Into<String>) -> Failure {
    let reason = reason.into();
    let mut error = Failure::internal(reason.clone());
    error.public_message = Some(reason);
    error
}
fn live(ctx: &JournalContext) -> Result<(), Failure> {
    if *ctx.cancellation().borrow() {
        let mut e = Failure::internal("cancelled");
        e.public_message = Some("Cancelled".into());
        return Err(e);
    }
    ctx.active().map_err(|_| Failure::unavailable())
}
fn message(error: &Failure) -> &str {
    error.public_message.as_deref().unwrap_or(&error.message)
}
fn reason(value: &JValue) -> &str {
    value.get("reason").as_str().unwrap_or("")
}
fn stage(
    ctx: &JournalContext,
    state: &mut State,
    to: &str,
    reason: &str,
    extra: Vec<(&str, JValue)>,
) -> Result<(), Failure> {
    let mut data = object(vec![
        ("from", text(&state.lifecycle)),
        ("to", text(to)),
        ("revision", JValue::Num(f64::from(state.revision))),
        ("reason", text(reason)),
    ]);
    for (k, v) in extra {
        set(&mut data, k, v);
    }
    ctx.authority("task.stage", data)?;
    state.lifecycle = to.into();
    Ok(())
}
async fn timed<T: Send, F: Future<Output = Result<T, Failure>> + Send>(
    ctx: Arc<JournalContext>,
    name: &'static str,
    limit: Duration,
    work: impl FnOnce(Arc<JournalContext>) -> F + Send,
) -> Result<T, Failure> {
    let (scoped, stop) = ctx.stage();
    let work = work(scoped);
    tokio::pin!(work);
    let (result, expired) = tokio::select! {biased;result=&mut work=>(result,false),_=tokio::time::sleep(limit)=>{stop.cancel();(work.await,true)}};
    live(&ctx)?;
    if expired {
        let minutes = ((limit.as_millis() + 30000) / 60000).max(1);
        return Err(block(format!(
            "The {name} stage did not finish within {minutes} minute{}.",
            if minutes == 1 { "" } else { "s" }
        )));
    }
    result
}
fn canonical(value: &JValue) -> JValue {
    match value {
        JValue::Obj(fields) => {
            let mut fields = fields
                .iter()
                .map(|(k, v)| (k.clone(), canonical(v)))
                .collect::<Vec<_>>();
            fields.sort_by(|a, b| a.0.encode_utf16().cmp(b.0.encode_utf16()));
            JValue::Obj(fields)
        }
        JValue::Arr(values) => JValue::Arr(values.iter().map(canonical).collect()),
        _ => value.clone(),
    }
}
fn hash(plan: &JValue) -> Result<String, Failure> {
    let encoded = wtf16::stringify(&canonical(plan)).ok_or_else(Failure::unavailable)?;
    Ok(Sha256::digest(encoded.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect())
}
fn caps(task: &Task) -> JValue {
    JValue::Arr(
        task.request
            .capabilities
            .iter()
            .map(|name| {
                let description = match name.as_str() {
                    "read_repository" => Some("Read files in the task workspace."),
                    "edit_file" => Some("Create and edit files in the task workspace."),
                    "execute_command" => Some("Run commands in the sandbox."),
                    "install_dependency" => Some("Install dependencies through the egress proxy."),
                    "network" => Some("Reach the granted domains through the egress proxy."),
                    "delete" => Some("Delete files in the task workspace."),
                    "git_push" => Some("Push the task branch."),
                    _ => None,
                };
                object(vec![
                    ("name", text(name)),
                    ("description", description.map_or(JValue::Undefined, text)),
                ])
            })
            .collect(),
    )
}
fn snapshot(job: &server_projects::task_journals::Job) -> JValue {
    object(vec![
        (
            "revision",
            job.authoritative
                .as_ref()
                .map_or(JValue::Null, |a| a.revision.clone()),
        ),
        (
            "stages",
            JValue::Arr(
                job.authoritative
                    .as_ref()
                    .map_or_else(Vec::new, |a| a.stages.clone()),
            ),
        ),
        ("artifacts", JValue::Arr(job.artifacts.clone())),
        ("uncertain", JValue::Arr(job.uncertain.clone())),
        (
            "pendingApproval",
            JValue::Bool(job.status.as_str() == Some("waiting_approval")),
        ),
    ])
}
fn tip(ctx: &Arc<JournalContext>) -> Option<String> {
    let flag = ctx.blocking_cancellation();
    let record = ctx.account().workspaces.read(ctx.task()).ok().flatten()?;
    ctx.account().workspaces.branch_head(&record, flag.flag())
}
fn workspace_error(error: workspace::Error) -> String {
    match error {
        workspace::Error::Coded { message, .. } | workspace::Error::Refused { message, .. } => {
            message
        }
        _ => "The change could not be read.".into(),
    }
}
fn names(change: &JValue) -> Vec<JValue> {
    match change.get("files") {
        JValue::Arr(files) => files.iter().map(|f| f.get("path").clone()).collect(),
        _ => vec![],
    }
}
async fn ask(ctx: &JournalContext, card: JValue, action: &str) -> Result<Decision, Failure> {
    let answer = ctx
        .request_approval(card)
        .map_err(|_| Failure::unavailable())?
        .await;
    emit(
        ctx,
        "approval.decided",
        object(vec![
            ("decision", text(answer.as_str())),
            ("action", text(action)),
        ]),
    )?;
    live(ctx)?;
    Ok(answer)
}
fn network(resources: &Resources, revision: u32, rows: &mut Vec<JValue>) {
    if let Some(network) = resources.released().and_then(|r| r.network.as_ref()) {
        if rows.len() >= 32 {
            rows.remove(0);
        }
        let mut row = object(vec![("revision", JValue::Num(f64::from(revision)))]);
        if let JValue::Obj(fields) = network {
            for (k, v) in fields {
                set(&mut row, k, v.clone());
            }
        }
        rows.push(row);
    }
}
impl Program for Pipeline {
    fn run<'a>(
        &'a self,
        ctx: Arc<JournalContext>,
        resources: &'a mut Resources,
        task: &'a Task,
    ) -> Run<'a> {
        Box::pin(async move {
            let mut state = State {
                lifecycle: "planned".into(),
                revision: 0,
                head: None,
                loops: 0,
            };
            let mut network = Vec::new();
            let result = self
                .drive(ctx.clone(), resources, task, &mut state, &mut network)
                .await;
            let result = match result {
                Ok(value) => Ok(value),
                Err(error) => {
                    if *ctx.cancellation().borrow() || error.message == "cancelled" {
                        if !matches!(state.lifecycle.as_str(), "blocked" | "merged") {
                            let _ = stage(&ctx, &mut state, "blocked", "Cancelled", vec![]);
                        }
                        Err(error)
                    } else {
                        let mut error = if error.public_message.is_some() {
                            error
                        } else {
                            block(format!(
                                "The pipeline stopped: {}",
                                wtf16::slice(&error.message, 300)
                            ))
                        };
                        if !matches!(state.lifecycle.as_str(), "blocked" | "merged") {
                            let why = message(&error).to_owned();
                            let _ = stage(&ctx, &mut state, "blocked", &why, vec![]);
                        }
                        let mut result = object(vec![
                            ("pipeline", JValue::Bool(true)),
                            ("blocked", JValue::Bool(true)),
                            ("revision", JValue::Num(f64::from(state.revision))),
                            ("headSha", state.head.as_ref().map_or(JValue::Null, text)),
                            ("loops", JValue::Num(f64::from(state.loops))),
                        ]);
                        if !network.is_empty() {
                            set(&mut result, "network", JValue::Arr(network));
                        }
                        if let Some(JValue::Obj(fields)) = error.result.take() {
                            for (k, v) in fields {
                                set(&mut result, &k, v);
                            }
                        }
                        error.result = Some(result);
                        Err(error)
                    }
                }
            };
            resources.release().await;
            result
        })
    }
}
impl Pipeline {
    fn next_round(&self, state: &mut State, why: &str) -> Result<String, Failure> {
        state.loops = state.loops.saturating_add(1);
        if state.loops > self.max_loops {
            return Err(block(format!(
                "{why} after {} rounds of changes; the task needs a person.",
                self.max_loops
            )));
        }
        Ok(format!(
            "{why} (round {} of {})",
            state.loops, self.max_loops
        ))
    }
    async fn drive(
        &self,
        ctx: Arc<JournalContext>,
        resources: &mut Resources,
        task: &Task,
        state: &mut State,
        network_rows: &mut Vec<JValue>,
    ) -> Result<JValue, Failure> {
        let branch = resources.workspace.get("branch").clone();
        let base = resources
            .workspace
            .get("baseSha")
            .as_str()
            .filter(|s| workspace::full_sha(s))
            .map(str::to_owned)
            .ok_or_else(|| block("The repository has no commit to start from."))?;
        stage(&ctx, state, "planned", "Planning", vec![])?;
        emit(
            &ctx,
            "checkpoint.created",
            object(vec![
                ("branch", branch.clone()),
                ("task", text(wtf16::slice(&task.request.prompt, 120))),
                ("baseSha", text(&base)),
            ]),
        )?;
        emit(
            &ctx,
            "artifact.created",
            object(vec![
                ("name", text("pipeline")),
                ("kind", text("pipeline")),
                ("version", JValue::Num(1.0)),
                ("maxLoops", JValue::Num(f64::from(self.max_loops))),
                ("merge", JValue::Bool(task.settings.merge)),
            ]),
        )?;
        let models = self
            .models
            .as_ref()
            .ok_or_else(|| block("The Planner is not set up on this server."))?;
        let authority = models.bind(ctx.clone()).map_err(|e| {
            block(format!(
                "No model was pinned for this task: {}",
                message(&e)
            ))
        })?;
        let session = timed(ctx.clone(), "plan", self.limits.plan, |scoped| {
            let authority = authority.clone();
            let model = task.request.model.as_deref();
            async move {
                role_pin::pin(
                    authority,
                    scoped.task(),
                    model,
                    false,
                    role_prompt::ROLES.map(str::to_owned).into(),
                    scoped.cancellation(),
                )
                .await
                .map_err(|e| block(format!("No model was pinned for this task: {}", e.reason)))
            }
        })
        .await?;
        let flag = ctx.blocking_cancellation();
        let project = ctx
            .account()
            .workspaces
            .project_snapshot(ctx.task(), 400, 8192, flag.flag())
            .map_err(|_| Failure::unavailable())?;
        drop(flag);
        let mut snippets = Vec::new();
        if let JValue::Arr(files) = project.get("files") {
            if !files.is_empty() {
                let mut listed = files
                    .iter()
                    .filter_map(JValue::as_str)
                    .collect::<Vec<_>>()
                    .join("\n");
                if yes(project.get("truncated")) {
                    listed.push_str("\n…");
                }
                snippets.push(object(vec![
                    ("source", text("project")),
                    ("tenantId", text(&ctx.scope().user)),
                    ("label", text("git ls-files")),
                    ("text", text(listed)),
                ]));
            }
        }
        if project.get("readme").truthy() {
            snippets.push(object(vec![
                ("source", text("project")),
                ("tenantId", text(&ctx.scope().user)),
                ("label", project.get("readme").get("path").clone()),
                ("text", project.get("readme").get("text").clone()),
            ]));
        }
        let capabilities = caps(task);
        let base_state = object(vec![
            ("taskId", text(ctx.task())),
            ("tenantId", text(&ctx.scope().user)),
            ("request", text(&task.request.prompt)),
            ("credentials", review::credentials(authority.as_ref())),
        ]);
        let mut planning = base_state.clone();
        set(&mut planning, "revision", JValue::Num(0.0));
        set(
            &mut planning,
            "projectInstructions",
            if task.context.is_empty() {
                JValue::Undefined
            } else {
                text(&task.context)
            },
        );
        set(&mut planning, "snippets", JValue::Arr(snippets));
        set(&mut planning, "capabilities", capabilities.clone());
        set(
            &mut planning,
            "constraints",
            JValue::Arr(vec![
                text("Change only this repository, on the task branch."),
                text("Every write, command or network use may need a person’s approval."),
            ]),
        );
        emit(
            &ctx,
            "step.started",
            object(vec![
                ("id", text("pipeline.plan")),
                ("title", text("The Planner writes the plan")),
            ]),
        )?;
        let planned = timed(ctx.clone(), "plan", self.limits.plan, |scoped| async {
            Ok(role_steps::plan(
                &session,
                scoped,
                &planning,
                self.constrain.enabled(),
                role_steps::DEFAULT_DEADLINE,
            )
            .await)
        })
        .await?;
        if !yes(planned.get("ok")) {
            emit(
                &ctx,
                "step.completed",
                object(vec![
                    ("id", text("pipeline.plan")),
                    ("failed", JValue::Bool(true)),
                    ("reason", text(wtf16::slice(reason(&planned), 300))),
                ]),
            )?;
            return Err(block(format!(
                "The Planner could not plan this task: {}",
                reason(&planned)
            )));
        }
        emit(
            &ctx,
            "step.completed",
            object(vec![("id", text("pipeline.plan"))]),
        )?;
        let plan = planned.get("plan").clone();
        let plan_hash = hash(&plan)?;
        if task.request.approve_plan {
            let steps = match plan.get("steps") {
                JValue::Arr(steps) => steps.iter().map(|s| s.get("do").clone()).collect(),
                _ => vec![],
            };
            let card=object(vec![("taskId",text(ctx.task())),("action",text("approve_plan")),("title",text("Approve the plan")),("kind",text("plan")),("command",text("")),("paths",JValue::Arr(vec![])),("reason",text("The Planner’s plan for this task. Approving lets the coding agent start on it; every write it makes still asks.")),("arguments",object(vec![("planHash",text(&plan_hash)),("goal",plan.get("goal").clone()),("steps",JValue::Arr(steps))])),("diff",JValue::Null),("plan",plan.clone())]);
            if !matches!(
                ask(&ctx, card, "approve_plan").await?,
                Decision::Approve | Decision::ApproveAll
            ) {
                return Err(block("The plan was not approved."));
            }
        }
        let mut feedback = Vec::new();
        let mut why = "Plan ready".to_owned();
        let mut tokens: Vec<Zeroizing<String>> = Vec::new();
        let contracts = planner_contract::contracts().ok_or_else(Failure::unavailable)?;
        loop {
            live(&ctx)?;
            stage(&ctx, state, "implementing", &why, vec![])?;
            let n = state.revision.saturating_add(1);
            emit(
                &ctx,
                "artifact.created",
                object(vec![
                    ("name", text("plan")),
                    ("kind", text("plan")),
                    ("revision", JValue::Num(f64::from(n))),
                    ("planHash", text(&plan_hash)),
                    ("plan", plan.clone()),
                ]),
            )?;
            if n > 1 {
                if resources.released().is_some_and(|r| !r.confirmed) {
                    return Err(block("Native network cleanup could not be confirmed; the task cannot take another grant."));
                }
                let flag = ctx.blocking_cancellation();
                *resources = Resources::claim(
                    ctx.clone(),
                    &task.request,
                    self.egress.clone(),
                    true,
                    flag.flag(),
                )
                .await
                .map_err(|e| {
                    block(format!(
                        "The task’s branch could not be taken up again: {}",
                        wtf16::slice(&crate::code::resource_error(e).message, 200)
                    ))
                })?;
            }
            if let Some(grant) = resources.grant() {
                tokens.push(Zeroizing::new(grant.token().into()));
            }
            let mut executing = object(vec![
                ("taskId", text(ctx.task())),
                ("tenantId", text(&ctx.scope().user)),
                ("revision", JValue::Num(f64::from(n))),
                ("request", text(&task.request.prompt)),
                (
                    "projectInstructions",
                    if task.context.is_empty() {
                        JValue::Undefined
                    } else {
                        text(&task.context)
                    },
                ),
                ("plan", plan.clone()),
                ("capabilities", capabilities.clone()),
                (
                    "feedback",
                    if feedback.is_empty() {
                        JValue::Undefined
                    } else {
                        JValue::Arr(feedback.clone())
                    },
                ),
                ("credentials", review::credentials(authority.as_ref())),
                (
                    "tokens",
                    if tokens.is_empty() {
                        JValue::Undefined
                    } else {
                        JValue::Arr(tokens.iter().map(|s| text(s.as_str())).collect())
                    },
                ),
                (
                    "roleSystemPrompts",
                    object(vec![
                        ("executor", contracts.get("executorInstructions").clone()),
                        ("planner", contracts.get("plannerInstructions").clone()),
                        ("reviewer", contracts.get("reviewerInstructions").clone()),
                        ("auditor", contracts.get("auditorInstructions").clone()),
                    ]),
                ),
                (
                    "orchestrator",
                    object(vec![
                        ("metaPrompt", contracts.get("pipelineMeta").clone()),
                        ("systemPrompt", text(role_prompt::SHARED_FRAME)),
                    ]),
                ),
            ]);
            let brief=match role_prompt::role("executor",&executing){Ok(projection)=>planner_contract::executor_brief(&projection),Err(role_prompt::Refusal::ContextRefused)=>return Err(block("The coding agent’s brief would have carried text it must not see, so it was not sent.")),Err(_)=>return Err(block("The coding agent’s brief could not be prepared."))};
            set(&mut executing, "credentials", JValue::Null);
            let invocation = Invocation {
                prompt: brief,
                label: task.request.prompt.clone(),
                model: Some(session.model.clone()),
                machine: Some(session.machine.as_ref().map_or(JValue::Null, text)),
                context: String::new(),
                preparation: "planner".into(),
                guarded: task.settings.guarded,
                harness: task.request.harness.clone(),
                capabilities: task.request.capabilities.clone(),
                domains: task.request.domains.clone(),
            };
            let executed = timed(ctx.clone(), "implement", self.limits.implement, |scoped| {
                self.executor.execute(scoped, resources, invocation)
            })
            .await;
            network(resources, n, network_rows);
            if let Err(error) = executed {
                if *ctx.cancellation().borrow()
                    || message(&error).starts_with("The implement stage did not finish")
                {
                    return Err(error);
                }
                return Err(block(format!(
                    "The coding agent stopped: {}",
                    wtf16::slice(message(&error), 300)
                )));
            }
            let released = resources
                .released()
                .ok_or_else(|| block("The workspace was not released cleanly."))?;
            if !released.confirmed {
                return Err(block(
                    "Native network cleanup could not be confirmed; this revision cannot continue.",
                ));
            }
            let released = released
                .workspace
                .as_ref()
                .ok_or_else(|| block("The workspace was not released cleanly."))?;
            if released.get("status").as_str() != Some("released") {
                return Err(block(if let Some(error) = released.get("error").as_str() {
                    format!(
                        "The workspace was not released cleanly: {}",
                        wtf16::slice(error, 200)
                    )
                } else {
                    "The workspace was not released cleanly.".into()
                }));
            }
            let head = released
                .get("headSha")
                .as_str()
                .filter(|s| workspace::full_sha(s))
                .filter(|s| *s != base)
                .ok_or_else(|| block("The coding agent made no change."))?
                .to_owned();
            if state.head.as_ref() == Some(&head) {
                return Err(block(
                    "The coding agent made no further change in this round.",
                ));
            }
            ctx.authority(
                "task.revision",
                object(vec![
                    ("n", JValue::Num(f64::from(n))),
                    ("headSha", text(&head)),
                    ("planHash", text(&plan_hash)),
                ]),
            )?;
            state.revision = n;
            state.head = Some(head.clone());
            emit(
                &ctx,
                "artifact.created",
                object(vec![
                    ("name", text("revision")),
                    ("kind", text("revision")),
                    ("revision", JValue::Num(f64::from(n))),
                    ("headSha", text(&head)),
                    ("baseSha", text(&base)),
                ]),
            )?;
            stage(&ctx, state, "verifying", "Change committed", vec![])?;
            if !self.verify.available() {
                return Err(block("Verification is not available on this server, so the change cannot be verified."));
            }
            let verified = timed(ctx.clone(), "verify", self.limits.verify, |scoped| {
                self.verify
                    .run_pipeline(scoped, &task.request.repository.id, &head, f64::from(n))
            })
            .await?;
            if verified.status == "unavailable" {
                return Err(block(format!(
                    "Verification is unavailable: {}",
                    verified
                        .error
                        .as_ref()
                        .and_then(|e| e.get("message").as_str())
                        .unwrap_or("no verifier")
                )));
            }
            if !matches!(verified.status, "passed" | "failed") {
                return Err(block(format!(
                    "Verification could not run: {}",
                    verified
                        .error
                        .as_ref()
                        .and_then(|e| e.get("message").as_str())
                        .unwrap_or("unknown error")
                )));
            }
            let measured=verified.report.as_ref().filter(|r|r.matches(ctx.task(),&head,f64::from(n))).ok_or_else(||block("Verification could not run: The test report was not measured by the verifier for this task, commit and revision."))?;
            let measured = measured.report();
            if verified.status == "failed" {
                let exit = measured
                    .get("exitCode")
                    .as_num()
                    .filter(|n| n.is_finite() && n.fract() == 0.0)
                    .map_or(String::new(), |exit| format!(" (exit {exit})"));
                feedback = vec![text(format!(
                    "The operator's tests failed at revision {n}{exit}{}.",
                    if yes(measured.get("timedOut")) {
                        ", timed out"
                    } else {
                        ""
                    }
                ))];
                if let Some(tail) = measured
                    .get("tail")
                    .as_str()
                    .filter(|s| !server_account::util::js_trim(s).is_empty())
                {
                    let tail = server_account::util::js_trim(tail)
                        .chars()
                        .rev()
                        .take(500)
                        .collect::<Vec<_>>()
                        .into_iter()
                        .rev()
                        .collect::<String>();
                    feedback.push(text(format!("End of the test output: {tail}")));
                }
                stage(&ctx, state, "changes_requested", "Tests failed", vec![])?;
                why = self.next_round(state, "Tests failed")?;
                continue;
            }
            stage(
                &ctx,
                state,
                "reviewing",
                "Tests passed",
                vec![(
                    "expectedArtifacts",
                    JValue::Arr(vec![text("plan"), text("test-report")]),
                )],
            )
            .map_err(|e| block(wtf16::slice(&e.message, 300)))?;
            let flag = ctx.blocking_cancellation();
            let change = ctx
                .account()
                .workspaces
                .change(ctx.task(), 200, flag.flag())
                .map_err(|e| {
                    block(format!(
                        "The change could not be read for review: {}",
                        wtf16::slice(&workspace_error(e), 200)
                    ))
                })?;
            drop(flag);
            let files = names(&change);
            emit(
                &ctx,
                "review.requested",
                object(vec![
                    ("baseSha", text(&base)),
                    ("headSha", text(&head)),
                    ("files", JValue::Num(files.len() as f64)),
                ]),
            )?;
            let execution = object(vec![
                ("headSha", text(&head)),
                ("changedFiles", JValue::Arr(files.clone())),
                (
                    "testResults",
                    JValue::Arr(vec![object(vec![
                        ("name", text("tests")),
                        ("passed", JValue::Bool(true)),
                    ])]),
                ),
                (
                    "summary",
                    text(format!(
                        "Revision {n}; the operator's tests passed at this head."
                    )),
                ),
            ]);
            let mut reviewing = base_state.clone();
            for (k, v) in [
                ("revision", JValue::Num(f64::from(n))),
                ("capabilities", capabilities.clone()),
                ("plan", plan.clone()),
                ("execution", execution.clone()),
                ("change", change.clone()),
                (
                    "tokens",
                    if tokens.is_empty() {
                        JValue::Undefined
                    } else {
                        JValue::Arr(tokens.iter().map(|s| text(s.as_str())).collect())
                    },
                ),
            ] {
                set(&mut reviewing, k, v);
            }
            let judged = timed(ctx.clone(), "review", self.limits.review, |scoped| async {
                Ok(
                    role_steps::review(&session, scoped, &reviewing, role_steps::DEFAULT_DEADLINE)
                        .await,
                )
            })
            .await?;
            if !yes(judged.get("ok")) {
                emit(
                    &ctx,
                    "review.failed",
                    object(vec![
                        ("baseSha", text(&base)),
                        ("headSha", text(&head)),
                        ("code", judged.get("code").clone()),
                        ("reason", judged.get("reason").clone()),
                    ]),
                )?;
                return Err(block(format!(
                    "The Planner’s review did not finish: {}",
                    reason(&judged)
                )));
            }
            let current = ctx.job()?;
            if current.authoritative.as_ref().is_none_or(|a| {
                a.revision.get("n").as_num() != Some(f64::from(n))
                    || a.revision.get("headSha").as_str() != Some(head.as_str())
            }) || tip(&ctx).as_deref() != Some(head.as_str())
            {
                emit(
                    &ctx,
                    "review.failed",
                    object(vec![
                        ("baseSha", text(&base)),
                        ("headSha", text(&head)),
                        ("code", text("stale")),
                        ("reason", text("The verdict was for an earlier revision.")),
                    ]),
                )?;
                return Err(block("The Planner’s verdict was for an earlier revision of the change, so it was not used."));
            }
            let verdict = judged.get("verdict");
            let mut completed = object(vec![("baseSha", text(&base)), ("headSha", text(&head))]);
            let mut artifact = object(vec![
                ("name", text("review")),
                ("kind", text("review-verdict")),
                ("revision", JValue::Num(f64::from(n))),
                ("headSha", text(&head)),
            ]);
            if let JValue::Obj(fields) = verdict {
                for (k, v) in fields {
                    set(&mut completed, k, v.clone());
                    set(&mut artifact, k, v.clone());
                }
            }
            set(
                &mut completed,
                "corrected",
                JValue::Bool(yes(judged.get("corrected"))),
            );
            emit(&ctx, "review.completed", completed)?;
            emit(&ctx, "artifact.created", artifact)?;
            if verdict.get("verdict").as_str() == Some("request_changes") {
                feedback = match verdict.get("findings") {
                    JValue::Arr(findings) => findings
                        .iter()
                        .map(|f| {
                            text(format!(
                                "{}{}: {}",
                                f.get("severity").as_str().unwrap_or(""),
                                if f.get("file").truthy() {
                                    format!(" in {}", f.get("file").as_str().unwrap_or(""))
                                } else {
                                    String::new()
                                },
                                f.get("message").as_str().unwrap_or("")
                            ))
                        })
                        .collect(),
                    _ => vec![],
                };
                stage(
                    &ctx,
                    state,
                    "changes_requested",
                    "The Planner requested changes",
                    vec![],
                )?;
                why = self.next_round(state, "Changes requested")?;
                continue;
            }
            let report = audit::report(
                &snapshot(&ctx.job()?),
                f64::from(n),
                &head,
                Some(&base),
                Some(&plan_hash),
                tip(&ctx).as_deref(),
            );
            let mut writeup = JValue::Null;
            let mut writeup_error = None;
            if self.audit_writeup {
                let mut auditing = base_state.clone();
                for (k, v) in [
                    ("revision", JValue::Num(f64::from(n))),
                    ("plan", plan.clone()),
                    ("lifecycleState", text("reviewing")),
                    ("execution", execution),
                ] {
                    set(&mut auditing, k, v);
                }
                match timed(ctx.clone(), "audit", self.limits.audit, |scoped| async {
                    Ok(
                        role_steps::audit(&session, scoped, &auditing, self.constrain.enabled())
                            .await,
                    )
                })
                .await?
                {
                    Ok(value) => writeup = value,
                    Err(code) => writeup_error = Some(code),
                }
            }
            let mut audit_artifact = object(vec![
                ("name", text("audit")),
                ("kind", text("audit-report")),
            ]);
            if let JValue::Obj(fields) = &report {
                for (k, v) in fields {
                    set(&mut audit_artifact, k, v.clone());
                }
            }
            set(&mut audit_artifact, "writeUp", writeup.clone());
            if let Some(code) = writeup_error {
                set(&mut audit_artifact, "writeUpError", text(code));
            }
            emit(&ctx, "artifact.created", audit_artifact)?;
            let mut into = None;
            let mut withheld = JValue::Null;
            if task.settings.merge {
                if report.get("overall").as_str() != Some("complete") {
                    let open = match report.get("checks") {
                        JValue::Arr(checks) => checks
                            .iter()
                            .filter(|c| c.get("status").as_str() != Some("pass"))
                            .map(|c| {
                                format!(
                                    "{}: {}",
                                    c.get("name").as_str().unwrap_or(""),
                                    c.get("detail").as_str().unwrap_or("")
                                )
                            })
                            .collect::<Vec<_>>()
                            .join(" "),
                        _ => String::new(),
                    };
                    withheld = object(vec![
                        ("code", text("audit_incomplete")),
                        (
                            "reason",
                            text(format!(
                                "Merging is not offered because the audit is incomplete ({open})"
                            )),
                        ),
                    ]);
                } else {
                    let pre = ctx.account().workspaces.merge_preflight(ctx.task(), &head);
                    if yes(pre.get("ok")) {
                        into = pre.get("baseBranch").as_str().map(str::to_owned);
                    } else {
                        withheld = object(vec![
                            ("code", pre.get("code").clone()),
                            (
                                "reason",
                                text(format!("Merging is not offered: {}", reason(&pre))),
                            ),
                        ]);
                    }
                }
            }
            let card_reason = if let Some(into) = &into {
                format!("The Planner approved revision {n} and the operator’s tests passed at it. Accepting fast-forwards {into} to this exact commit, only if {into} has not moved.")
            } else {
                format!("The Planner approved revision {n} and the operator’s tests passed at it. Accepting records this reviewed head as accepted; nothing is merged.{}",if withheld==JValue::Null{String::new()}else{format!(" {}",reason(&withheld))})
            };
            let card = object(vec![
                ("taskId", text(ctx.task())),
                ("action", text("review_change")),
                ("title", text("Accept this change")),
                ("kind", text("review")),
                ("command", text("")),
                (
                    "paths",
                    JValue::Arr(files.iter().take(50).cloned().collect()),
                ),
                ("reason", text(card_reason)),
                (
                    "arguments",
                    object(vec![
                        ("branch", branch.clone()),
                        ("baseSha", text(&base)),
                        ("headSha", text(&head)),
                        ("files", JValue::Arr(files)),
                        ("revision", JValue::Num(f64::from(n))),
                        ("mergeInto", into.as_ref().map_or(JValue::Null, text)),
                    ]),
                ),
                ("diff", JValue::Null),
                ("review", ctx.job()?.review.unwrap_or(JValue::Null)),
                (
                    "verdict",
                    object(vec![
                        ("verdict", verdict.get("verdict").clone()),
                        ("summary", verdict.get("summary").clone()),
                        (
                            "findings",
                            JValue::Num(match verdict.get("findings") {
                                JValue::Arr(f) => f.len() as f64,
                                _ => 0.0,
                            }),
                        ),
                    ]),
                ),
                (
                    "audit",
                    object(vec![
                        ("overall", report.get("overall").clone()),
                        (
                            "checks",
                            JValue::Arr(match report.get("checks") {
                                JValue::Arr(checks) => checks
                                    .iter()
                                    .map(|c| {
                                        object(vec![
                                            ("name", c.get("name").clone()),
                                            ("status", c.get("status").clone()),
                                        ])
                                    })
                                    .collect(),
                                _ => vec![],
                            }),
                        ),
                        (
                            "writeUp",
                            if writeup == JValue::Null {
                                JValue::Null
                            } else {
                                object(vec![
                                    ("completeness", writeup.get("completeness").clone()),
                                    ("summary", writeup.get("summary").clone()),
                                ])
                            },
                        ),
                    ]),
                ),
                (
                    "evidence",
                    object(vec![
                        ("revision", JValue::Num(f64::from(n))),
                        ("headSha", text(&head)),
                        ("planHash", text(&plan_hash)),
                        ("tests", report.get("evidence").get("tests").clone()),
                        (
                            "completeness",
                            report.get("evidence").get("completeness").clone(),
                        ),
                    ]),
                ),
                (
                    "merge",
                    into.as_ref().map_or(JValue::Null, |into| {
                        object(vec![
                            ("into", text(into)),
                            ("from", text(&base)),
                            ("to", text(&head)),
                        ])
                    }),
                ),
                ("mergeWithheld", withheld.clone()),
            ]);
            let answer = ask(&ctx, card, "review_change").await?;
            let accepted = matches!(answer, Decision::Approve | Decision::ApproveAll);
            let mut result = object(vec![
                ("pipeline", JValue::Bool(true)),
                ("revision", JValue::Num(f64::from(n))),
                ("branch", branch.clone()),
                ("baseSha", text(&base)),
                ("headSha", text(&head)),
                ("verdict", verdict.get("verdict").clone()),
                ("audit", report.get("overall").clone()),
                ("accepted", JValue::Bool(accepted)),
                ("decision", text(answer.as_str())),
                ("merged", JValue::Bool(false)),
                ("mergedInto", JValue::Null),
                ("loops", JValue::Num(f64::from(state.loops))),
            ]);
            if !network_rows.is_empty() {
                set(&mut result, "network", JValue::Arr(network_rows.clone()));
            }
            if withheld != JValue::Null {
                set(&mut result, "mergeWithheld", withheld.get("code").clone());
            }
            let Some(into) = into.filter(|_| accepted) else {
                return Ok(result);
            };
            if !self.merge.enabled() {
                set(&mut result, "note", text("merge-turned-off"));
                set(&mut result,"noteText",text("Merging was turned off before this was accepted, so the change is recorded as accepted and not merged."));
                return Ok(result);
            }
            live(&ctx)?;
            // No future is abandoned across the Git compare-and-swap. The task/account/inference holds
            // remain owned until Git settles and the authoritative source ref has been read back.
            let merged = ctx
                .account()
                .workspaces
                .merge_verified(ctx.task(), &head, ctx.now());
            if !yes(merged.get("ok")) {
                set(&mut result, "mergeRefused", merged.get("code").clone());
                let mut error = block(format!("Accepted, but not merged: {}", reason(&merged)));
                error.result = Some(result);
                return Err(error);
            }
            let mut recorded = !yes(merged.get("recordFailed"));
            if stage(
                &ctx,
                state,
                "merged",
                &format!("Fast-forwarded {into} to the reviewed head"),
                vec![],
            )
            .is_err()
            {
                recorded = false;
            }
            set(&mut result, "merged", JValue::Bool(true));
            set(&mut result, "mergedInto", text(&into));
            if !recorded {
                set(&mut result, "note", text("record-failed"));
                set(&mut result,"noteText",text(format!("{into} was fast-forwarded to the reviewed commit, but noevia could not record it.")));
            }
            return Ok(result);
        }
    }
}

#[cfg(test)]
#[path = "pipeline_tests.rs"]
mod tests;
