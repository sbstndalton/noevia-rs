//! Code-task lifecycle actor. HTTP futures only wait for admission; the native worker retains
//! its execution, inference and resource leases through cancellation and terminal publication.
use crate::{
    account::Account,
    context::{Clock, JournalContext},
    egress::Egress,
    resources::{self, Resources},
    session::Context,
    start::{Refusal, Request},
    Scope,
};
use js_json::{wtf16, JValue};
use server_projects::task_journals::{self, Found, Surface};
use std::{
    collections::HashMap,
    fmt,
    future::Future,
    pin::Pin,
    sync::{atomic::AtomicBool, Arc, Mutex, Weak},
    time::Duration,
};
use tokio::{
    sync::{oneshot, watch},
    task::JoinHandle,
};
pub const INFERENCE_PAUSED:&str="Coding tasks wait while noevia tunes or reconfigures models. Start this task when that finishes.";
pub struct Failure {
    pub status: Option<u16>,
    pub message: String,
    pub public_message: Option<String>,
    pub result: Option<JValue>,
}
impl fmt::Debug for Failure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Failure")
            .field("status", &self.status)
            .finish_non_exhaustive()
    }
}
impl Failure {
    pub fn public(status: u16, message: impl Into<String>) -> Self {
        Self {
            status: Some(status),
            message: message.into(),
            public_message: None,
            result: None,
        }
    }
    pub fn internal(message: impl Into<String>) -> Self {
        Self {
            status: None,
            message: message.into(),
            public_message: None,
            result: None,
        }
    }
    pub fn unavailable() -> Self {
        Self::public(
            503,
            "Task status is unavailable right now. Try again shortly.",
        )
    }
    pub fn visible(&self) -> &str {
        self.public_message
            .as_deref()
            .unwrap_or(if self.status.is_some() {
                &self.message
            } else {
                "Code request failed"
            })
    }
    pub(crate) fn recorded(&self) -> String {
        wtf16::slice(self.public_message.as_deref().unwrap_or(&self.message), 500)
    }
}
impl From<Refusal> for Failure {
    fn from(error: Refusal) -> Self {
        Self::public(error.status, error.message)
    }
}
pub(crate) fn resource_error(error: resources::Error) -> Failure {
    match error {
        resources::Error::Workspace(crate::workspace::Error::Refused { status, message }) => {
            Failure::public(status, message)
        }
        resources::Error::Workspace(error) => {
            Failure::internal(crate::workspace::error_text(&error))
        }
        resources::Error::Grant(error) => Failure::public(
            503,
            error
                .public_message
                .unwrap_or_else(|| "native egress is unavailable".into()),
        ),
        resources::Error::Cancelled => Failure::internal("cancelled"),
        resources::Error::Unavailable => Failure::unavailable(),
    }
}
pub use crate::context::Permit;
pub type Entering<'a> = Pin<Box<dyn Future<Output = Result<Arc<dyn Permit>, Failure>> + Send + 'a>>;
pub trait Admission: Send + Sync {
    fn paused(&self) -> Result<bool, Failure>;
    fn enter(&self) -> Result<Arc<dyn Permit>, Failure> {
        Err(Failure::unavailable())
    }
    fn enter_for<'a>(&'a self, _context: Arc<JournalContext>) -> Entering<'a> {
        Box::pin(async move { self.enter() })
    }
}
#[derive(Clone, Copy, Default)]
pub struct Settings {
    pub guarded: bool,
    pub reviewed: bool,
    pub merge: bool,
    pub constrain: bool,
}
pub struct Task {
    pub request: Request,
    pub context: String,
    pub shared_context: bool,
    pub settings: Settings,
}
pub type Run<'a> = Pin<Box<dyn Future<Output = Result<JValue, Failure>> + Send + 'a>>;
/// Native Rust programs only. Planner is a narrow boundary until its actor is installed; no
/// implementation of this interface may delegate task ownership to a legacy executor.
pub trait Program: Send + Sync {
    fn run<'a>(
        &'a self,
        context: Arc<JournalContext>,
        resources: &'a mut Resources,
        task: &'a Task,
    ) -> Run<'a>;
}
#[derive(Clone, Hash, PartialEq, Eq)]
struct Key {
    user: String,
    task: String,
}
struct Live {
    generation: u64,
    started: bool,
    context: Weak<JournalContext>,
    shutdown_reason: Option<&'static str>,
}
#[derive(Default)]
struct State {
    next: u64,
    live: HashMap<Key, Live>,
    workers: Vec<JoinHandle<()>>,
    draining: bool,
}
struct Running {
    state: Arc<Mutex<State>>,
    key: Key,
    generation: u64,
}
impl Running {
    fn current(&self) -> bool {
        self.state.lock().is_ok_and(|s| {
            s.live
                .get(&self.key)
                .is_some_and(|l| l.generation == self.generation)
        })
    }
}
impl Drop for Running {
    fn drop(&mut self) {
        if let Ok(mut state) = self.state.lock() {
            if state
                .live
                .get(&self.key)
                .is_some_and(|l| l.generation == self.generation)
            {
                state.live.remove(&self.key);
            }
        }
    }
}
pub struct Code {
    state: Arc<Mutex<State>>,
    direct: Arc<dyn Program>,
    planner: Option<Arc<dyn Program>>,
    admission: Arc<dyn Admission>,
    egress: Option<Arc<dyn Egress>>,
    clock: Arc<dyn Clock>,
    approval_timeout: Duration,
}
struct Prepared {
    context: Arc<JournalContext>,
    resources: Resources,
    program: Arc<dyn Program>,
    running: Running,
    response: JValue,
}
fn object(rows: Vec<(&str, JValue)>) -> JValue {
    JValue::Obj(rows.into_iter().map(|(k, v)| (k.into(), v)).collect())
}
fn text(s: &str) -> JValue {
    JValue::Str(s.into())
}
fn strings(values: &[String]) -> JValue {
    JValue::Arr(values.iter().map(|s| text(s)).collect())
}
impl Code {
    pub fn new(
        direct: Arc<dyn Program>,
        planner: Option<Arc<dyn Program>>,
        admission: Arc<dyn Admission>,
        egress: Option<Arc<dyn Egress>>,
        clock: Arc<dyn Clock>,
        approval_timeout: Duration,
    ) -> Self {
        Self {
            state: Arc::new(Mutex::new(State::default())),
            direct,
            planner,
            admission,
            egress,
            clock,
            approval_timeout,
        }
    }
    fn prepare(
        &self,
        account: Arc<Account>,
        project: &str,
        task: &Task,
    ) -> Result<Prepared, Failure> {
        let mut state = self.state.lock().map_err(|_| Failure::unavailable())?;
        if state.draining {
            return Err(Failure::unavailable());
        }
        if !account
            .list(Surface::Code, Some(project), true)
            .map_err(|_| Failure::unavailable())?
            .is_empty()
        {
            return Err(Failure::public(
                409,
                "This project already has a task running.",
            ));
        }
        if self
            .admission
            .paused()
            .map_err(|_| Failure::unavailable())?
        {
            return Err(Failure::public(503, INFERENCE_PAUSED));
        }
        let program = if task.request.preparation == "planner" {
            self.planner.clone().ok_or_else(|| {
                Failure::public(409, "The Planner pipeline is not set up on this server.")
            })?
        } else {
            self.direct.clone()
        };
        let generation = state.next.checked_add(1).ok_or_else(Failure::unavailable)?;
        state.next = generation;
        let id = crate::random_id().map_err(|_| Failure::unavailable())?;
        let scope = Scope {
            user: account.user().into(),
            project: project.into(),
            surface: Surface::Code,
        };
        let guard = account.control.begin(scope, id.clone()).map_err(|error| {
            if error == crate::Error::Busy {
                Failure::public(409, "This project already has a task running.")
            } else {
                Failure::unavailable()
            }
        })?;
        account
            .create(
                Surface::Code,
                &id,
                project,
                task.request.capabilities.iter().map(|s| text(s)).collect(),
                self.clock.now(),
            )
            .map_err(|_| Failure::unavailable())?;
        let context = Arc::new(
            JournalContext::new(
                account.clone(),
                guard,
                self.clock.clone(),
                self.approval_timeout,
            )
            .map_err(|_| Failure::unavailable())?,
        );
        let resources = match Resources::claim_workspace(
            context.clone(),
            &task.request,
            self.egress.clone(),
            false,
            &AtomicBool::new(false),
        ) {
            Ok(resources) => resources,
            Err(error) => {
                let error = resource_error(error);
                let _ = context.event(
                    "job.failed",
                    object(vec![("error", text(&error.recorded()))]),
                );
                return Err(error);
            }
        };
        let response = object(vec![
            ("taskId", text(&id)),
            ("branch", resources.workspace.get("branch").clone()),
            ("workspace", resources.workspace.get("path").clone()),
            ("repository", text(&task.request.repository.id)),
            ("capabilities", strings(&task.request.capabilities)),
            ("domains", strings(&task.request.domains)),
            ("harness", text(&task.request.harness)),
            ("promptPreparation", text(&task.request.preparation)),
            ("sharedContext", JValue::Bool(task.shared_context)),
        ]);
        let key = Key {
            user: account.user().into(),
            task: id,
        };
        state.live.insert(
            key.clone(),
            Live {
                generation,
                started: false,
                context: Arc::downgrade(&context),
                shutdown_reason: None,
            },
        );
        Ok(Prepared {
            context,
            resources,
            program,
            running: Running {
                state: self.state.clone(),
                key,
                generation,
            },
            response,
        })
    }
    pub async fn start(
        self: &Arc<Self>,
        account: Arc<Account>,
        project: String,
        task: Task,
    ) -> Result<JValue, Failure> {
        let runtime = tokio::runtime::Handle::try_current().map_err(|_| Failure::unavailable())?;
        let (answer, receiver) = oneshot::channel();
        let actor = self.clone();
        let worker_runtime = runtime.clone();
        let worker = runtime.spawn_blocking(move || {
            worker_runtime.block_on(actor.drive(account, project, task, answer))
        });
        {
            let mut state = self.state.lock().map_err(|_| Failure::unavailable())?;
            state.workers.retain(|w| !w.is_finished());
            state.workers.push(worker);
        }
        receiver
            .await
            .unwrap_or_else(|_| Err(Failure::unavailable()))
    }
    async fn drive(
        self: Arc<Self>,
        account: Arc<Account>,
        project: String,
        task: Task,
        answer: oneshot::Sender<Result<JValue, Failure>>,
    ) {
        let mut prepared = match self.prepare(account.clone(), &project, &task) {
            Ok(prepared) => prepared,
            Err(error) => {
                let _ = answer.send(Err(error));
                return;
            }
        };
        if let Err(error) = prepared.resources.issue_grant(&task.request).await {
            if matches!(error, resources::Error::Cancelled) {
                let _ = answer.send(Ok(prepared.response));
            } else {
                let error = resource_error(error);
                let _ = prepared.context.event(
                    "job.failed",
                    object(vec![("error", text(&error.recorded()))]),
                );
                let _ = answer.send(Err(error));
            }
            prepared.resources.release().await;
            if let Some(reason) = self.shutdown_reason(&prepared.running) {
                let _ = prepared
                    .context
                    .event("job.interrupted", object(vec![("reason", text(reason))]));
            }
            return;
        }
        let admitted = self.admission.enter_for(prepared.context.clone()).await;
        // An admission that arrives during cancellation still outlives resource settlement.
        let _retained = admitted.as_ref().ok().cloned();
        let permit = {
            let Ok(mut state) = self.state.lock() else {
                let _ = answer.send(Err(Failure::unavailable()));
                return;
            };
            if !prepared.running.current_unlocked(&state) || prepared.context.active().is_err() {
                None
            } else {
                match admitted {
                    Ok(permit) if permit.valid() => {
                        if prepared.context.bind_permit(permit.clone()).is_ok()
                            && prepared
                                .context
                                .event("job.started", object(vec![]))
                                .is_ok()
                        {
                            if let Some(live) = state.live.get_mut(&prepared.running.key) {
                                live.started = true;
                            }
                            Some(permit)
                        } else {
                            None
                        }
                    }
                    result => {
                        let error = match result {
                            Err(error) if error.status == Some(503) => {
                                Failure::public(503, INFERENCE_PAUSED)
                            }
                            Err(error) => error,
                            _ => Failure::unavailable(),
                        };
                        let _ = prepared.context.event(
                            "job.failed",
                            object(vec![("error", text(&error.recorded()))]),
                        );
                        None
                    }
                }
            }
        };
        let _ = answer.send(Ok(prepared.response));
        let Some(permit) = permit else {
            prepared.resources.release().await;
            if let Some(reason) = self.shutdown_reason(&prepared.running) {
                let _ = prepared
                    .context
                    .event("job.interrupted", object(vec![("reason", text(reason))]));
            }
            if matches!(account.owned(Surface::Code,&project,prepared.context.task()),Ok(Found::Task(job))if job.status.as_str()==Some("queued"))
            {
                let message = if task.request.preparation == "planner" {
                    "Code pipeline could not start."
                } else {
                    "Code task could not start."
                };
                let _ = prepared
                    .context
                    .event("job.failed", object(vec![("error", text(message))]));
            }
            return;
        };
        let (stop, mut stopped) = watch::channel(false);
        let watched = prepared.context.clone();
        let coverage = permit.clone();
        let monitor = tokio::spawn(async move {
            loop {
                tokio::select! {biased;_=stopped.changed()=>break,_=tokio::time::sleep(Duration::from_millis(200))=>{if watched.account().fence().is_err()||!coverage.valid(){let _=watched.account().control.cancel(watched.scope(),watched.task());break;}}}
            }
        });
        let outcome = if prepared.context.active().is_err() {
            Err(Failure::internal("cancelled"))
        } else {
            prepared
                .program
                .run(prepared.context.clone(), &mut prepared.resources, &task)
                .await
        };
        prepared.resources.release().await;
        let _ = prepared.context.active();
        stop.send_replace(true);
        let _ = monitor.await;
        if prepared.running.current() && account.fence().is_ok() {
            let cancelled = *prepared.context.cancellation().borrow();
            let (kind, data) = if let Some(reason) = self.shutdown_reason(&prepared.running) {
                ("job.interrupted", object(vec![("reason", text(reason))]))
            } else {
                match outcome {
                    Ok(result) => (
                        if cancelled {
                            "job.cancelled"
                        } else {
                            "job.completed"
                        },
                        object(vec![("result", result)]),
                    ),
                    Err(_) if cancelled => ("job.cancelled", object(vec![])),
                    Err(error) => (
                        "job.failed",
                        object(vec![
                            ("error", text(&error.recorded())),
                            ("result", error.result.unwrap_or(JValue::Null)),
                        ]),
                    ),
                }
            };
            let _ = prepared.context.event(kind, data);
        }
        drop(permit);
    }
    pub fn cancel(&self, account: &Account, project: &str, id: &str) -> Result<JValue, Failure> {
        let state = self.state.lock().map_err(|_| Failure::unavailable())?;
        let key = Key {
            user: account.user().into(),
            task: id.into(),
        };
        let started = state.live.get(&key).is_some_and(|l| l.started);
        owned(
            account
                .cancel_snapshot(Surface::Code, project, id, started, self.clock.now())
                .map_err(|_| Failure::unavailable())?,
        )
        .map(|job| task_journals::code_view(&job, &JValue::Null))
    }
    pub fn decide(
        &self,
        account: &Account,
        project: &str,
        id: &str,
        decision: &str,
        approval: &str,
    ) -> Result<JValue, Failure> {
        owned(
            account
                .owned(Surface::Code, project, id)
                .map_err(|_| Failure::unavailable())?,
        )?;
        let scope = Scope {
            user: account.user().into(),
            project: project.into(),
            surface: Surface::Code,
        };
        account
            .control
            .decide(&scope, id, approval, decision)
            .map_err(|error| match error {
                crate::Error::UnknownDecision => Failure::public(400, "Unknown decision"),
                crate::Error::MissingApproval => {
                    Failure::public(400, "Which approval is this answer for?")
                }
                crate::Error::Expired => {
                    Failure::public(409, "This approval expired before anyone answered it.")
                }
                crate::Error::Unavailable => Failure::unavailable(),
                _ => Failure::public(409, "That approval is no longer waiting."),
            })?;
        Ok(object(vec![("ok", JValue::Bool(true))]))
    }
    pub fn running(&self) -> Result<usize, Failure> {
        self.state
            .lock()
            .map(|state| state.live.len())
            .map_err(|_| Failure::unavailable())
    }
    fn shutdown_reason(&self, running: &Running) -> Option<&'static str> {
        self.state
            .lock()
            .ok()?
            .live
            .get(&running.key)
            .filter(|l| l.generation == running.generation)?
            .shutdown_reason
    }
    pub async fn drain(&self) {
        let (contexts, workers) = {
            let Ok(mut state) = self.state.lock() else {
                return;
            };
            state.draining = true;
            let contexts=state.live.values_mut().filter_map(|live|{let context=live.context.upgrade()?;let waiting=matches!(context.account().owned(context.scope().surface,&context.scope().project,context.task()),Ok(Found::Task(job))if job.status.as_str()==Some("waiting_approval"));live.shutdown_reason=Some(if waiting{"The server restarted while waiting for approval; start again to be asked again."}else{"The server restarted before this finished."});Some(context)}).collect::<Vec<_>>();
            (contexts, std::mem::take(&mut state.workers))
        };
        for context in contexts {
            let _ = context
                .account()
                .control
                .cancel(context.scope(), context.task());
        }
        for worker in workers {
            let _ = worker.await;
        }
    }
}
impl Running {
    fn current_unlocked(&self, state: &State) -> bool {
        state
            .live
            .get(&self.key)
            .is_some_and(|l| l.generation == self.generation)
    }
}
fn owned(found: Found) -> Result<Box<task_journals::Job>, Failure> {
    match found {
        Found::Task(job) => Ok(job),
        Found::Unreadable => Err(Failure::public(
            409,
            "Unreadable job journal; review required",
        )),
        Found::Missing => Err(Failure::public(404, "Task not found")),
    }
}
#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;
    use crate::{
        context::SystemClock,
        facts::{CodeConfig, Repository},
        Decision,
    };
    use std::{
        path::PathBuf,
        sync::atomic::{AtomicUsize, Ordering},
    };
    use tokio::sync::Notify;
    const USER: &str = "11111111-1111-4111-8111-111111111111";
    struct Lease {
        valid: Arc<AtomicBool>,
        count: Arc<AtomicUsize>,
    }
    impl Permit for Lease {
        fn valid(&self) -> bool {
            self.valid.load(Ordering::SeqCst)
        }
    }
    impl Drop for Lease {
        fn drop(&mut self) {
            self.count.fetch_sub(1, Ordering::SeqCst);
        }
    }
    struct Gate {
        paused: AtomicBool,
        refuse: AtomicBool,
        valid: Arc<AtomicBool>,
        count: Arc<AtomicUsize>,
    }
    impl Gate {
        fn new() -> Self {
            Self {
                paused: AtomicBool::new(false),
                refuse: AtomicBool::new(false),
                valid: Arc::new(AtomicBool::new(true)),
                count: Arc::new(AtomicUsize::new(0)),
            }
        }
    }
    impl Admission for Gate {
        fn paused(&self) -> Result<bool, Failure> {
            Ok(self.paused.load(Ordering::SeqCst))
        }
        fn enter(&self) -> Result<Arc<dyn Permit>, Failure> {
            if self.refuse.load(Ordering::SeqCst) {
                return Err(Failure::public(503, "synthetic refusal"));
            }
            self.count.fetch_add(1, Ordering::SeqCst);
            Ok(Arc::new(Lease {
                valid: self.valid.clone(),
                count: self.count.clone(),
            }))
        }
    }
    #[derive(Clone, Copy)]
    enum Mode {
        Complete,
        Fail,
        Wait,
        Approve,
    }
    struct Work {
        mode: Mode,
        entered: Notify,
        proceed: Notify,
        executions: AtomicUsize,
        active: AtomicUsize,
        peak: AtomicUsize,
    }
    impl Work {
        fn new(mode: Mode) -> Self {
            Self {
                mode,
                entered: Notify::new(),
                proceed: Notify::new(),
                executions: AtomicUsize::new(0),
                active: AtomicUsize::new(0),
                peak: AtomicUsize::new(0),
            }
        }
    }
    struct Active<'a>(&'a AtomicUsize);
    impl Drop for Active<'_> {
        fn drop(&mut self) {
            self.0.fetch_sub(1, Ordering::SeqCst);
        }
    }
    impl Program for Work {
        fn run<'a>(
            &'a self,
            context: Arc<JournalContext>,
            _resources: &'a mut Resources,
            _task: &'a Task,
        ) -> Run<'a> {
            Box::pin(async move {
                self.executions.fetch_add(1, Ordering::SeqCst);
                let active = self.active.fetch_add(1, Ordering::SeqCst) + 1;
                self.peak.fetch_max(active, Ordering::SeqCst);
                let _active = Active(&self.active);
                context
                    .event("progress", object(vec![("stage", text("running"))]))
                    .unwrap();
                self.entered.notify_one();
                match self.mode {
                    Mode::Complete => Ok(object(vec![("stopReason", text("end_turn"))])),
                    Mode::Fail => Err(Failure::internal("synthetic executor failure")),
                    Mode::Wait => {
                        let mut cancel = context.cancellation();
                        let already_cancelled = *cancel.borrow();
                        if already_cancelled {
                            return Err(Failure::internal("cancelled"));
                        }
                        tokio::select! {biased;_=cancel.changed()=>Err(Failure::internal("cancelled")),_=self.proceed.notified()=>Ok(object(vec![("stopReason",text("end_turn"))]))}
                    }
                    Mode::Approve => {
                        let card = object(vec![
                            ("taskId", text(context.task())),
                            ("action", text("edit_file")),
                            ("arguments", object(vec![("path", text("source.txt"))])),
                        ]);
                        let answer = context.request_approval(card).unwrap().await;
                        context
                            .event(
                                "approval.decided",
                                object(vec![
                                    ("decision", text(answer.as_str())),
                                    ("action", text("edit_file")),
                                ]),
                            )
                            .unwrap();
                        Ok(object(vec![(
                            "accepted",
                            JValue::Bool(matches!(
                                answer,
                                Decision::Approve | Decision::ApproveAll
                            )),
                        )]))
                    }
                }
            })
        }
    }
    fn setup() -> (tempfile::TempDir, Arc<Account>, PathBuf) {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("users").join(USER)).unwrap();
        let repo = root.path().join("repo");
        std::fs::create_dir(&repo).unwrap();
        for args in [
            vec!["init", "-q"],
            vec!["config", "user.email", "synthetic@example.invalid"],
            vec!["config", "user.name", "Synthetic"],
            vec![
                "-c",
                "commit.gpgsign=false",
                "commit",
                "--allow-empty",
                "-qm",
                "synthetic",
            ],
        ] {
            assert!(std::process::Command::new("git")
                .args(args)
                .current_dir(&repo)
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_CONFIG_SYSTEM", "/dev/null")
                .status()
                .unwrap()
                .success());
        }
        let account = Arc::new(
            Account::open(
                root.path(),
                USER,
                &CodeConfig::from_env(|_| None),
                "epoch".into(),
                1,
            )
            .unwrap(),
        );
        (root, account, repo)
    }
    fn task(repo: &std::path::Path) -> Task {
        Task {
            request: Request {
                repository: Repository {
                    id: "repo".into(),
                    path: repo.into(),
                },
                prompt: "synthetic".into(),
                capabilities: vec!["edit_file".into()],
                domains: vec![],
                harness: "opencode".into(),
                preparation: "direct".into(),
                model: None,
                approve_plan: false,
            },
            context: String::new(),
            shared_context: false,
            settings: Settings::default(),
        }
    }
    fn runtime(
        work: Arc<Work>,
        planner: Option<Arc<Work>>,
        gate: Arc<Gate>,
        timeout: Duration,
    ) -> Arc<Code> {
        Arc::new(Code::new(
            work,
            planner.map(|p| p as Arc<dyn Program>),
            gate,
            None,
            Arc::new(SystemClock),
            timeout,
        ))
    }
    async fn entered(work: &Work) {
        tokio::time::timeout(Duration::from_secs(5), work.entered.notified())
            .await
            .unwrap();
    }
    async fn finished(code: &Code) {
        tokio::time::timeout(Duration::from_secs(5), async {
            while code.running().unwrap() != 0 {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
    }
    fn job(account: &Account, project: &str, id: &str) -> Box<task_journals::Job> {
        owned(account.owned(Surface::Code, project, id).unwrap()).unwrap()
    }
    #[tokio::test]
    async fn start_cancel_single_project_and_cross_project_execution_are_native() {
        let (_root, account, repo) = setup();
        let work = Arc::new(Work::new(Mode::Wait));
        let gate = Arc::new(Gate::new());
        let code = runtime(work.clone(), None, gate.clone(), Duration::from_secs(30));
        let first = code
            .start(account.clone(), "a".into(), task(&repo))
            .await
            .unwrap();
        entered(&work).await;
        assert_eq!(first.get("repository").as_str(), Some("repo"));
        assert_eq!(first.get("promptPreparation").as_str(), Some("direct"));
        assert_eq!(first.get("sharedContext"), &JValue::Bool(false));
        let second = code
            .start(account.clone(), "a".into(), task(&repo))
            .await
            .unwrap_err();
        assert_eq!(second.status, Some(409));
        assert_eq!(work.executions.load(Ordering::SeqCst), 1);
        let other = code
            .start(account.clone(), "b".into(), task(&repo))
            .await
            .unwrap();
        entered(&work).await;
        assert_eq!(work.peak.load(Ordering::SeqCst), 2);
        let view = code
            .cancel(&account, "a", first.get("taskId").as_str().unwrap())
            .unwrap();
        assert_eq!(view.get("status").as_str(), Some("running"));
        assert_eq!(view.get("approval"), &JValue::Null);
        code.cancel(&account, "b", other.get("taskId").as_str().unwrap())
            .unwrap();
        finished(&code).await;
        assert_eq!(
            job(&account, "a", first.get("taskId").as_str().unwrap())
                .status
                .as_str(),
            Some("cancelled")
        );
        assert_eq!(gate.count.load(Ordering::SeqCst), 0);
    }
    #[tokio::test]
    async fn pause_refuses_before_claim_and_late_admission_failure_never_runs_the_program() {
        let (_root, account, repo) = setup();
        let work = Arc::new(Work::new(Mode::Complete));
        let gate = Arc::new(Gate::new());
        gate.paused.store(true, Ordering::SeqCst);
        let code = runtime(work.clone(), None, gate.clone(), Duration::from_secs(30));
        let refused = code
            .start(account.clone(), "a".into(), task(&repo))
            .await
            .unwrap_err();
        assert_eq!(refused.status, Some(503));
        assert_eq!(refused.message, INFERENCE_PAUSED);
        assert!(account
            .list(Surface::Code, Some("a"), false)
            .unwrap()
            .is_empty());
        gate.paused.store(false, Ordering::SeqCst);
        gate.refuse.store(true, Ordering::SeqCst);
        let started = code
            .start(account.clone(), "a".into(), task(&repo))
            .await
            .unwrap();
        finished(&code).await;
        let job = job(&account, "a", started.get("taskId").as_str().unwrap());
        assert_eq!(job.status.as_str(), Some("failed"));
        assert_eq!(job.error.as_str(), Some(INFERENCE_PAUSED));
        assert_eq!(work.executions.load(Ordering::SeqCst), 0);
    }
    #[tokio::test]
    async fn approvals_allow_deny_timeout_and_context_ownership_keep_existing_shapes() {
        for decision in [Some("approve"), Some("approve_all"), Some("deny"), None] {
            let (_root, account, repo) = setup();
            let work = Arc::new(Work::new(Mode::Approve));
            let code = runtime(
                work.clone(),
                None,
                Arc::new(Gate::new()),
                if decision.is_none() {
                    Duration::from_millis(10)
                } else {
                    Duration::from_secs(30)
                },
            );
            let start = code
                .start(account.clone(), "a".into(), task(&repo))
                .await
                .unwrap();
            let id = start.get("taskId").as_str().unwrap();
            entered(&work).await;
            if let Some(decision) = decision {
                let scope = Scope {
                    user: USER.into(),
                    project: "a".into(),
                    surface: Surface::Code,
                };
                let cards = tokio::time::timeout(Duration::from_secs(5), async {
                    loop {
                        let cards = account.control.cards(&scope).unwrap();
                        if !cards.is_empty() {
                            break cards;
                        }
                        tokio::task::yield_now().await;
                    }
                })
                .await
                .unwrap();
                assert_eq!(
                    code.decide(&account, "other", id, "unknown", &cards.first().unwrap().0)
                        .unwrap_err()
                        .status,
                    Some(404)
                );
                assert_eq!(
                    code.decide(&account, "a", id, "unknown", "")
                        .unwrap_err()
                        .message,
                    "Unknown decision"
                );
                assert_eq!(
                    code.decide(&account, "a", id, decision, "")
                        .unwrap_err()
                        .message,
                    "Which approval is this answer for?"
                );
                assert_eq!(
                    code.decide(&account, "a", id, decision, &cards.first().unwrap().0)
                        .unwrap()
                        .get("ok"),
                    &JValue::Bool(true)
                );
            }
            finished(&code).await;
            let job = job(&account, "a", id);
            assert_eq!(job.status.as_str(), Some("completed"));
            assert_eq!(
                job.result.get("accepted"),
                &JValue::Bool(matches!(decision, Some("approve" | "approve_all")))
            );
        }
    }
    #[tokio::test]
    async fn planner_program_is_selected_through_its_narrow_boundary() {
        let (_root, account, repo) = setup();
        let direct = Arc::new(Work::new(Mode::Fail));
        let planner = Arc::new(Work::new(Mode::Complete));
        let code = runtime(
            direct.clone(),
            Some(planner.clone()),
            Arc::new(Gate::new()),
            Duration::from_secs(30),
        );
        let mut request = task(&repo);
        request.request.preparation = "planner".into();
        let start = code
            .start(account.clone(), "a".into(), request)
            .await
            .unwrap();
        finished(&code).await;
        assert_eq!(start.get("promptPreparation").as_str(), Some("planner"));
        assert_eq!(direct.executions.load(Ordering::SeqCst), 0);
        assert_eq!(planner.executions.load(Ordering::SeqCst), 1);
        assert_eq!(
            job(&account, "a", start.get("taskId").as_str().unwrap())
                .status
                .as_str(),
            Some("completed")
        );
    }
    #[tokio::test]
    async fn lost_inference_coverage_stops_actual_execution_and_releases_its_lease() {
        let (_root, account, repo) = setup();
        let work = Arc::new(Work::new(Mode::Wait));
        let gate = Arc::new(Gate::new());
        let code = runtime(work.clone(), None, gate.clone(), Duration::from_secs(30));
        let start = code
            .start(account.clone(), "a".into(), task(&repo))
            .await
            .unwrap();
        entered(&work).await;
        assert_eq!(work.active.load(Ordering::SeqCst), 1);
        gate.valid.store(false, Ordering::SeqCst);
        finished(&code).await;
        assert_eq!(work.active.load(Ordering::SeqCst), 0);
        assert_eq!(gate.count.load(Ordering::SeqCst), 0);
        assert_eq!(
            job(&account, "a", start.get("taskId").as_str().unwrap())
                .status
                .as_str(),
            Some("cancelled")
        );
    }
    #[tokio::test]
    async fn graceful_runtime_restart_records_interruption_with_waiting_reason() {
        let (_root, account, repo) = setup();
        let work = Arc::new(Work::new(Mode::Approve));
        let gate = Arc::new(Gate::new());
        let code = runtime(work.clone(), None, gate.clone(), Duration::from_secs(30));
        let start = code
            .start(account.clone(), "a".into(), task(&repo))
            .await
            .unwrap();
        let id = start.get("taskId").as_str().unwrap();
        entered(&work).await;
        tokio::time::timeout(Duration::from_secs(5), async {
            while job(&account, "a", id).status.as_str() != Some("waiting_approval") {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        code.drain().await;
        let job = job(&account, "a", id);
        assert_eq!(job.status.as_str(), Some("interrupted"));
        assert_eq!(
            job.error.as_str(),
            Some("The server restarted while waiting for approval; start again to be asked again.")
        );
        assert_eq!(gate.count.load(Ordering::SeqCst), 0);
    }
    #[tokio::test]
    async fn another_account_or_project_cannot_cancel_or_decide_a_live_task() {
        let (root, account, repo) = setup();
        let other = "33333333-3333-4333-8333-333333333333";
        std::fs::create_dir_all(root.path().join("users").join(other)).unwrap();
        let other = Account::open(
            root.path(),
            other,
            &CodeConfig::from_env(|_| None),
            "other".into(),
            1,
        )
        .unwrap();
        let work = Arc::new(Work::new(Mode::Wait));
        let code = runtime(
            work.clone(),
            None,
            Arc::new(Gate::new()),
            Duration::from_secs(30),
        );
        let start = code
            .start(account.clone(), "a".into(), task(&repo))
            .await
            .unwrap();
        let id = start.get("taskId").as_str().unwrap();
        entered(&work).await;
        assert_eq!(code.cancel(&other, "a", id).unwrap_err().status, Some(404));
        assert_eq!(
            code.decide(&other, "a", id, "unknown", "")
                .unwrap_err()
                .status,
            Some(404)
        );
        assert_eq!(
            code.cancel(&account, "other", id).unwrap_err().status,
            Some(404)
        );
        assert_eq!(work.active.load(Ordering::SeqCst), 1);
        code.cancel(&account, "a", id).unwrap();
        finished(&code).await;
    }
    struct Issuer {
        entered: Notify,
        proceed: Notify,
        issued: Arc<AtomicBool>,
        revokes: AtomicUsize,
    }
    struct Issuing(Arc<AtomicBool>, bool);
    impl Drop for Issuing {
        fn drop(&mut self) {
            if self.1 {
                self.0.store(false, Ordering::SeqCst);
            }
        }
    }
    impl Egress for Issuer {
        fn network(&self) -> Result<crate::egress::Network, crate::egress::Unavailable> {
            Ok(crate::egress::Network::Ready)
        }
        fn endpoint(&self) -> &str {
            "egress.invalid:8080"
        }
        fn grant<'a>(
            &'a self,
            task: &'a str,
            _domains: &'a [String],
        ) -> crate::egress::Reply<'a, crate::egress::Grant> {
            Box::pin(async move {
                self.issued.store(true, Ordering::SeqCst);
                let mut issuing = Issuing(self.issued.clone(), true);
                self.entered.notify_one();
                self.proceed.notified().await;
                issuing.1 = false;
                Ok(crate::egress::Grant::new(
                    task.into(),
                    "handle".into(),
                    "synthetic-token".into(),
                ))
            })
        }
        fn revoke<'a>(
            &'a self,
            _grant: &'a crate::egress::Grant,
        ) -> crate::egress::Reply<'a, crate::egress::Cleanup> {
            Box::pin(async move {
                self.revokes.fetch_add(1, Ordering::SeqCst);
                self.issued.store(false, Ordering::SeqCst);
                Ok(crate::egress::Cleanup {
                    confirmed: true,
                    report: object(vec![
                        ("final", JValue::Bool(true)),
                        ("snapshotId", text("snapshot")),
                    ]),
                })
            })
        }
        fn forget<'a>(
            &'a self,
            _grant: &'a crate::egress::Grant,
            _snapshot: &'a str,
        ) -> crate::egress::Reply<'a, bool> {
            Box::pin(async { Ok(true) })
        }
    }
    #[tokio::test]
    async fn aborting_the_http_waiter_does_not_drop_the_native_task_or_its_claim() {
        let (_root, account, repo) = setup();
        let work = Arc::new(Work::new(Mode::Wait));
        let gate = Arc::new(Gate::new());
        let issuer = Arc::new(Issuer {
            entered: Notify::new(),
            proceed: Notify::new(),
            issued: Arc::new(AtomicBool::new(false)),
            revokes: AtomicUsize::new(0),
        });
        let code = Arc::new(Code::new(
            work.clone(),
            None,
            gate.clone(),
            Some(issuer.clone()),
            Arc::new(SystemClock),
            Duration::from_secs(30),
        ));
        let mut request = task(&repo);
        request.request.capabilities.push("network".into());
        request.request.domains.push("synthetic.invalid".into());
        let actor = code.clone();
        let owner = account.clone();
        let waiter = tokio::spawn(async move { actor.start(owner, "a".into(), request).await });
        tokio::time::timeout(Duration::from_secs(5), issuer.entered.notified())
            .await
            .unwrap();
        waiter.abort();
        assert!(waiter.await.is_err());
        assert!(issuer.issued.load(Ordering::SeqCst));
        assert_eq!(code.running().unwrap(), 1);
        issuer.proceed.notify_one();
        entered(&work).await;
        let jobs = account.list(Surface::Code, Some("a"), true).unwrap();
        let id = jobs.first().unwrap().id.as_str().unwrap();
        code.cancel(&account, "a", id).unwrap();
        finished(&code).await;
        assert_eq!(issuer.revokes.load(Ordering::SeqCst), 1);
        assert!(!issuer.issued.load(Ordering::SeqCst));
        assert_eq!(gate.count.load(Ordering::SeqCst), 0);
        assert_eq!(job(&account, "a", id).status.as_str(), Some("cancelled"));
    }

    #[tokio::test]
    async fn executor_failure_releases_the_workspace_and_inference_lease_before_completion() {
        let (_root, account, repo) = setup();
        let work = Arc::new(Work::new(Mode::Fail));
        let gate = Arc::new(Gate::new());
        let code = runtime(work.clone(), None, gate.clone(), Duration::from_secs(30));
        let start = code
            .start(account.clone(), "a".into(), task(&repo))
            .await
            .unwrap();
        let id = start.get("taskId").as_str().unwrap();
        finished(&code).await;
        let job = job(&account, "a", id);
        assert_eq!(job.status.as_str(), Some("failed"));
        assert_eq!(job.error.as_str(), Some("synthetic executor failure"));
        assert_eq!(work.executions.load(Ordering::SeqCst), 1);
        assert_eq!(gate.count.load(Ordering::SeqCst), 0);
        assert_eq!(
            account
                .workspaces
                .read(id)
                .unwrap()
                .unwrap()
                .get("status")
                .as_str(),
            Some("released")
        );
    }
}
