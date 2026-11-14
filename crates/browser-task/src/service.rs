//! core browser-service.cjs (`createBrowserService`): Browser mode as a project feature. One task
//! is one durable job (kind `browser`) holding one executor session open for its life; actions are
//! taken one at a time, in the order they arrive; every consequential one stops at the approval
//! card; a restart never silently resumes an approval.
//!
//! Each live task has its own thread (core's `jobs.run` work): it opens the session, then takes
//! queued actions until a finish, a cancel or [`IDLE_TIMEOUT`] without one, and cleans up exactly
//! once (the executor closed within [`CLOSE_TIMEOUT`], the egress grant revoked and its network
//! report kept on the job). A cancel's own effects (the `tool.uncertain` of an action still running,
//! the answer it gives) are written before anything the cancel sets off, as in core, where they are
//! synchronous and the rest is not.

use crate::engine::{Launch, Proxy};
use crate::executor::{self, Executor, Flight, Held, Open};
use crate::signal::{Signal, Slot};
use crate::Fail;
use js_json::{wtf16, JValue};
use server_projects::task_journals::{browser_view, Job};
use server_projects::task_store::{NewJob, Options as StoreOptions, Refusal, Store};
use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::MutexGuard;
use std::sync::{Arc, Condvar, Mutex, PoisonError};
use std::time::{Duration, Instant};

/// `APPROVAL_TIMEOUT_MS`.
pub const APPROVAL_TIMEOUT: Duration = Duration::from_millis(5 * 60 * 1000);
/// `IDLE_TIMEOUT_MS`: no action (act or finish) for this long and the task is timed out.
pub const IDLE_TIMEOUT: Duration = Duration::from_millis(10 * 60 * 1000);
pub const TIMEOUT_REASON: &str = "This browser task made no progress and timed out.";
/// The executor's close is waited for this long before it counts as unconfirmed.
pub const CLOSE_TIMEOUT: Duration = Duration::from_millis(5000);
/// `MAX_RESULT_BYTES`: a caller's own summary, bounded as assistant output is.
pub const MAX_RESULT_BYTES: usize = 64 * 1024;
/// `grantable`.
pub const GRANTABLE: [&str; 1] = ["open_browser"];
const EXPIRED_MEMORY_MS: f64 = 60_000.0;
const RETAIN_MS: f64 = 7.0 * 86_400_000.0;
const MAX_JOBS: usize = 50;
const NEEDS_EGRESS: &str = "Browser tasks need the egress proxy (D15) configured on this server.";
const TERMINAL: [&str; 4] = ["completed", "failed", "cancelled", "interrupted"];

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

fn s(text: &str) -> JValue {
    JValue::Str(text.to_string())
}

fn refused(r: Refusal) -> Fail {
    Fail::plain(r.status, &r.message)
}

/// The egress proxy (core code-egress.cjs as browser-service.cjs uses it).
pub trait Egress: Send + Sync {
    /// `egress.endpoint` (`'egress'` when unset).
    fn endpoint(&self) -> Option<String>;
    /// `egress.available()`; an egress without one is available.
    fn available(&self) -> bool {
        true
    }
    /// `egress.grant({ taskId, domains })`: the grant (`{ token, … }`), or a falsy value.
    fn grant(&self, task_id: &str, domains: &[String]) -> Result<JValue, Fail>;
    /// `egress.revoke(grant)`: `{ confirmed, report }`.
    fn revoke(&self, grant: &JValue) -> Result<JValue, Fail>;
    /// `egress.forget(grant, snapshotId)`; `None` for an egress that has none.
    fn forget(&self, _grant: &JValue, _snapshot_id: &str) -> Option<Result<JValue, Fail>> {
        None
    }
}

/// `createBrowserService`'s dependencies.
pub struct Options {
    pub launch: Arc<dyn Launch>,
    pub egress: Option<Arc<dyn Egress>>,
    pub log: Arc<dyn Fn(JValue) + Send + Sync>,
    /// `Date.now()`, for the journal and the expired-card memory.
    pub now: Arc<dyn Fn() -> f64 + Send + Sync>,
    /// The job, session and approval ids (`crypto.randomUUID`).
    pub new_id: Arc<dyn Fn() -> String + Send + Sync>,
    pub timeout: Duration,
    pub idle_timeout: Duration,
    pub action_timeout: Duration,
    pub close_timeout: Duration,
    pub direct: bool,
}

impl Options {
    /// The defaults core runs with.
    pub fn new(
        launch: Arc<dyn Launch>,
        egress: Option<Arc<dyn Egress>>,
        log: Arc<dyn Fn(JValue) + Send + Sync>,
    ) -> Options {
        Options {
            launch,
            egress,
            log,
            now: Arc::new(server_projects::task_store::date_now),
            new_id: Arc::new(server_projects::source_jobs::new_id),
            timeout: APPROVAL_TIMEOUT,
            idle_timeout: IDLE_TIMEOUT,
            action_timeout: executor::ACTION_TIMEOUT,
            close_timeout: CLOSE_TIMEOUT,
            direct: false,
        }
    }
}

/// `parseDomains`: lower-cased, trimmed, `^[a-z0-9.-]+\.[a-z]{2,}$` only, at most 20. An entry
/// `String()` throws on (an object with its own `toString`) is core's TypeError: a 500.
pub fn parse_domains(list: &JValue) -> Result<Vec<String>, Fail> {
    let JValue::Arr(items) = list else {
        return Ok(Vec::new());
    };
    let shaped = |d: &str| {
        let Some(dot) = d.rfind('.') else {
            return false;
        };
        let (head, tld) = (d.get(..dot).unwrap_or(""), d.get(dot + 1..).unwrap_or(""));
        !head.is_empty()
            && head
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'.' || b == b'-')
            && tld.len() >= 2
            && tld.bytes().all(|b| b.is_ascii_lowercase())
    };
    let mut out = Vec::new();
    for d in items {
        let raw = if d.truthy() {
            executor::coerce(d)?
        } else {
            String::new()
        };
        let d = raw.trim_matches(js_space).to_lowercase();
        if shaped(&d) {
            out.push(d);
        }
    }
    out.truncate(20);
    Ok(out)
}

fn js_space(c: char) -> bool {
    matches!(c, '\u{9}'..='\u{d}' | ' ' | '\u{a0}' | '\u{1680}' | '\u{2000}'..='\u{200a}' | '\u{2028}' | '\u{2029}' | '\u{202f}' | '\u{205f}' | '\u{3000}' | '\u{feff}')
}

struct Waiting {
    id: String,
    task_id: String,
    request: JValue,
    slot: Arc<Slot<String>>,
}

/// `pending` and `expired`: the cards waiting, in memory on purpose. Lock order: `pending`, then
/// `expired`.
struct Registry {
    pending: Mutex<Vec<Waiting>>,
    expired: Mutex<HashMap<String, f64>>,
    now: Arc<dyn Fn() -> f64 + Send + Sync>,
}

impl Registry {
    /// `finish(decision)`: the first answer settles the card. A timed-out card is remembered as
    /// expired before it stops waiting, so a late answer is never told it was simply gone.
    fn finish(&self, id: &str, decision: &str) {
        let taken = {
            let mut pending = lock(&self.pending);
            let at = pending.iter().position(|w| w.id == id);
            let taken = at.map(|i| pending.remove(i));
            if taken.is_some() && decision == "timeout" {
                let now = (self.now)();
                let mut expired = lock(&self.expired);
                expired.insert(id.to_string(), now + EXPIRED_MEMORY_MS);
                if expired.len() > 200 {
                    expired.retain(|_, until| *until > now);
                }
            }
            taken
        };
        if let Some(waiting) = taken {
            waiting.slot.settle(decision.to_string());
        }
    }

    fn abort_task(&self, task_id: &str) {
        let ids: Vec<String> = lock(&self.pending)
            .iter()
            .filter(|w| w.task_id == task_id)
            .map(|w| w.id.clone())
            .collect();
        for id in ids {
            self.finish(&id, "aborted");
        }
    }

    /// `pendingFor(taskId)` as the view shows it: `{ id, ...request }`.
    fn card_for(&self, task_id: &str) -> JValue {
        lock(&self.pending)
            .iter()
            .find(|w| w.task_id == task_id)
            .map_or(JValue::Null, |w| card_view(&w.id, &w.request))
    }
}

fn card_view(id: &str, request: &JValue) -> JValue {
    let mut items = vec![("id".to_string(), s(id))];
    if let JValue::Obj(fields) = request {
        for (k, v) in fields {
            wtf16::set(&mut items, k, v.clone());
        }
    }
    JValue::Obj(items)
}

type Reply = Arc<Slot<Result<JValue, Fail>>>;

enum Item {
    Act { action: JValue, reply: Reply },
    Finish { result: JValue },
}

/// The task's action queue, and whether it takes actions (core's `sessions` entry): one lock, so an
/// action is either queued while the task is open or refused, never queued after the cleanup.
#[derive(Default)]
struct Queue {
    items: VecDeque<Item>,
    open: bool,
}

fn cancelled() -> Fail {
    Fail::public(499, "Cancelled")
}

/// One live task (core's per-start closure state).
struct Run {
    id: String,
    store: Arc<Store>,
    registry: Arc<Registry>,
    egress: Option<Arc<dyn Egress>>,
    grant: Option<JValue>,
    signal: Arc<Signal>,
    /// A cancel's synchronous part runs holding this; every later journal write of the task takes it.
    gate: Mutex<()>,
    queue: Mutex<Queue>,
    wake: Condvar,
    executor: Mutex<Option<Arc<Executor>>>,
    opening: Held<Result<String, Fail>>,
    executor_closed: AtomicBool,
    cleanup: Held<JValue>,
    close_timeout: Duration,
    /// `activeAction`: the type of the action running, while it runs.
    active: Mutex<Option<String>>,
}

impl Run {
    fn gate(&self) -> MutexGuard<'_, ()> {
        lock(&self.gate)
    }

    /// One journal write, after any cancel that is being answered.
    fn append(&self, kind: &str, data: JValue) -> Result<(), Fail> {
        let _gate = self.gate();
        self.append_held(kind, data)
    }

    /// [`Run::append`] for a caller that holds the gate.
    fn append_held(&self, kind: &str, data: JValue) -> Result<(), Fail> {
        self.store
            .append(&self.id, kind, data)
            .map(|_| ())
            .map_err(refused)
    }

    fn uncertain(&self, data: JValue) {
        let _ = self.append("tool.uncertain", data);
    }

    /// The queue closed for actions; with `drain`, every queued action refused (a finish dropped).
    fn close_queue(&self, drain: bool) {
        let mut queue = lock(&self.queue);
        queue.open = false;
        if drain {
            while let Some(item) = queue.items.pop_front() {
                if let Item::Act { reply, .. } = item {
                    reply.settle(Err(Fail::public(
                        409,
                        "This task ended before this action ran.",
                    )));
                }
            }
        }
    }

    /// `cleanup()`: started once, on its own thread; the network report (`null` without a grant).
    fn cleanup(self: &Arc<Self>) -> Arc<Flight<JValue>> {
        let mut slot = lock(&self.cleanup);
        if let Some(f) = slot.as_ref() {
            return f.clone();
        }
        let run = self.clone();
        let f = Flight::spawn_or(JValue::Null, move || {
            // A cleanup that panicked (an egress or engine implementation) confirmed nothing.
            let r = run.clone();
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || r.clean()))
                .unwrap_or_else(|_| {
                    run.executor_closed.store(false, Ordering::SeqCst);
                    if run.grant.is_some() {
                        JValue::obj([
                            ("available", JValue::Bool(false)),
                            ("final", JValue::Bool(false)),
                            ("partial", JValue::Bool(true)),
                            ("state", s("uncertain")),
                            ("cleanupConfirmed", JValue::Bool(false)),
                        ])
                    } else {
                        JValue::Null
                    }
                })
        });
        *slot = Some(f.clone());
        f
    }

    fn cleaned(&self) -> bool {
        lock(&self.cleanup)
            .as_ref()
            .is_some_and(|f| f.peek().is_some())
    }

    fn clean(self: &Arc<Self>) -> JValue {
        drop(self.gate());
        self.close_queue(false);
        self.registry.abort_task(&self.id);
        self.close_queue(true);
        // The executor's close and the grant's revoke run side by side, as core's Promise.all; the
        // close is confirmed within `close_timeout` of its start or not at all.
        let executor = lock(&self.executor).clone();
        let closing = executor.map(|exec| {
            let (run, opening) = (self.clone(), lock(&self.opening).clone());
            std::thread::spawn(move || {
                let id = run.id.clone();
                let closer = Flight::spawn_or(false, move || {
                    exec.close_task(&id);
                    if let Some(o) = opening {
                        let _ = o.wait();
                    }
                    exec.shutdown()
                });
                if closer.wait_for(run.close_timeout) != Some(true) {
                    run.executor_closed.store(false, Ordering::SeqCst);
                    run.uncertain(JValue::obj([
                        ("tool", s("network")),
                        (
                            "reason",
                            s("Browser executor closure could not be confirmed."),
                        ),
                    ]));
                }
            })
        });
        let report = self.network();
        if let Some(t) = closing {
            if t.join().is_err() {
                self.executor_closed.store(false, Ordering::SeqCst);
            }
        }
        report
    }

    fn network(&self) -> JValue {
        let (Some(grant), Some(egress)) = (self.grant.as_ref(), self.egress.as_ref()) else {
            return JValue::Null;
        };
        let taken = egress.revoke(grant).ok();
        let base = taken
            .as_ref()
            .map(|t| t.get("report"))
            .filter(|r| r.truthy());
        let mut items: Vec<(String, JValue)> = match base {
            Some(JValue::Obj(items)) => items.clone(),
            Some(_) => Vec::new(),
            None => vec![
                ("available".into(), JValue::Bool(false)),
                ("final".into(), JValue::Bool(false)),
                ("partial".into(), JValue::Bool(true)),
                ("state".into(), s("uncertain")),
            ],
        };
        let confirmed = taken
            .as_ref()
            .is_some_and(|t| t.get("confirmed").as_bool() == Some(true));
        wtf16::set(&mut items, "cleanupConfirmed", JValue::Bool(confirmed));
        let report = JValue::Obj(items);
        if !confirmed {
            self.uncertain(JValue::obj([
                ("tool", s("network")),
                ("reason", s("Native egress cleanup could not be confirmed.")),
            ]));
        }
        let retained = self
            .append(
                "artifact.created",
                JValue::obj([
                    ("name", s("network.cleanup")),
                    ("kind", s("native-network-report")),
                    ("network", report.clone()),
                ]),
            )
            .is_ok();
        let snapshot = report.get("snapshotId");
        if retained && confirmed && report.get("final").truthy() && snapshot.truthy() {
            let _ = egress.forget(grant, &executor::js_string(snapshot));
        }
        report
    }
}

struct Tenant {
    store: Arc<Store>,
    /// From the one-task check to the job (core's is one synchronous step).
    starting: Mutex<()>,
}

/// One Browser service per process.
pub struct Service {
    options: Options,
    registry: Arc<Registry>,
    stores: Mutex<HashMap<PathBuf, Arc<Tenant>>>,
    /// `controllers`: the tasks running in this process, from `job.started` to their end.
    live: Mutex<HashMap<String, Arc<Run>>>,
}

fn is_terminal(job: &Job) -> bool {
    job.status.as_str().is_some_and(|st| TERMINAL.contains(&st))
}

impl Service {
    pub fn new(options: Options) -> Arc<Service> {
        let registry = Arc::new(Registry {
            pending: Mutex::new(Vec::new()),
            expired: Mutex::new(HashMap::new()),
            now: options.now.clone(),
        });
        Arc::new(Service {
            options,
            registry,
            stores: Mutex::new(HashMap::new()),
            live: Mutex::new(HashMap::new()),
        })
    }

    fn log(&self, entry: JValue) {
        (self.options.log)(entry);
    }

    /// `storeFor(workspace)`: opened once per directory, recovering what a previous process left
    /// running (interrupted, never resumed).
    fn tenant(&self, dir: &Path) -> Result<Arc<Tenant>, Fail> {
        let mut stores = lock(&self.stores);
        if let Some(t) = stores.get(dir) {
            return Ok(t.clone());
        }
        let (now, new_id) = (self.options.now.clone(), self.options.new_id.clone());
        let store = Arc::new(Store::with(
            dir,
            StoreOptions {
                kinds: Some(vec!["browser".into()]),
                durable: false,
                retain_ms: RETAIN_MS,
                max_jobs: MAX_JOBS,
            },
            Box::new(move || now()),
            Box::new(move || new_id()),
        ));
        let live: Vec<String> = lock(&self.live).keys().cloned().collect();
        store
            .recover(&|id| live.iter().any(|l| l == id))
            .map_err(refused)?;
        let tenant = Arc::new(Tenant {
            store,
            starting: Mutex::new(()),
        });
        stores.insert(dir.to_path_buf(), tenant.clone());
        Ok(tenant)
    }

    /// `owned(workspace, project, taskId)`.
    fn owned(&self, tenant: &Tenant, project_id: &str, task_id: &str) -> Result<Job, Fail> {
        let job = tenant.store.get(task_id).map_err(refused)?;
        match job {
            Some(job)
                if job.kind.as_str() == Some("browser")
                    && job.project_id.as_str() == Some(project_id) =>
            {
                Ok(job)
            }
            _ => Err(Fail::public(404, "Task not found")),
        }
    }

    pub fn network(&self) -> bool {
        self.options.egress.as_ref().is_some_and(|e| e.available())
    }

    /// `networkState()`: `unconfigured`, `ready` or `unavailable` (#2242).
    pub fn network_state(&self) -> &'static str {
        match &self.options.egress {
            None => "unconfigured",
            Some(e) if e.available() => "ready",
            Some(_) => "unavailable",
        }
    }

    /// `facts(workspace)`: what the task reads cannot read from disk.
    pub fn facts(&self, dir: &Path) -> Result<JValue, Fail> {
        let tenant = self.tenant(dir)?;
        let mut approvals: Vec<(String, JValue)> = Vec::new();
        let pending: Vec<(String, String, JValue)> = lock(&self.registry.pending)
            .iter()
            .map(|w| (w.task_id.clone(), w.id.clone(), w.request.clone()))
            .collect();
        for (task, id, request) in pending {
            if approvals.iter().any(|(k, _)| *k == task) {
                continue;
            }
            let own = tenant
                .store
                .get(&task)
                .ok()
                .flatten()
                .is_some_and(|j| j.kind.as_str() == Some("browser"));
            if own {
                approvals.push((task, card_view(&id, &request)));
            }
        }
        Ok(JValue::obj([
            (
                "capabilities",
                JValue::Arr(GRANTABLE.iter().map(|c| s(c)).collect()),
            ),
            ("network", JValue::Bool(self.network())),
            ("networkState", s(self.network_state())),
            ("approvals", JValue::Obj(approvals)),
        ]))
    }

    /// The card waiting on a task, as a task view shows it (`null`: none).
    pub fn approval(&self, task_id: &str) -> JValue {
        self.registry.card_for(task_id)
    }

    /// `get(workspace, project, taskId)`.
    pub fn get(&self, dir: &Path, project_id: &str, task_id: &str) -> Result<JValue, Fail> {
        let tenant = self.tenant(dir)?;
        let job = self.owned(&tenant, project_id, task_id)?;
        Ok(browser_view(&job, &self.approval(task_id)))
    }

    /// `jobs.list({ projectId, kind: 'browser', active: true }).length`: an unreadable journal of any
    /// kind counts (core's non-strict list adds it whatever the filter).
    fn active(&self, tenant: &Tenant, project_id: &str) -> bool {
        tenant
            .store
            .ids()
            .iter()
            .any(|id| match tenant.store.get(id) {
                Ok(Some(j)) => {
                    j.kind.as_str() == Some("browser")
                        && j.project_id.as_str() == Some(project_id)
                        && !is_terminal(&j)
                }
                Ok(None) => false,
                Err(e) => e.is_unreadable(),
            })
    }

    /// `start(workspace, project, body)`: `{ taskId, domains }` (202) once the session is open.
    pub fn start(
        self: &Arc<Self>,
        dir: &Path,
        project_id: &str,
        body: &JValue,
    ) -> Result<JValue, Fail> {
        if matches!(body, JValue::Null) {
            return Err(Fail::plain(
                None,
                "Cannot read properties of null (reading 'domains')",
            ));
        }
        let domains = parse_domains(body.get("domains"))?;
        if domains.is_empty() {
            return Err(Fail::public(
                400,
                "A browser task needs at least one domain it may reach.",
            ));
        }
        let tenant = self.tenant(dir)?;
        let id = {
            let _one = lock(&tenant.starting);
            if self.active(&tenant, project_id) {
                return Err(Fail::public(
                    409,
                    "This project already has a browser task running.",
                ));
            }
            // Refused up front, without a job: a job created and refused at once would sit queued.
            if self.options.egress.is_none() && !self.options.direct {
                return Err(Fail::public(503, NEEDS_EGRESS));
            }
            let capabilities = vec!["open_browser".to_string()];
            tenant
                .store
                .create(NewJob {
                    kind: "browser",
                    project_id: Some(project_id),
                    parent_id: None,
                    capabilities: &capabilities,
                })
                .map_err(refused)?
        };
        let store = tenant.store.clone();
        let downloads_dir = dir.join("browser-downloads").join(&id);
        let grant = match &self.options.egress {
            Some(egress) => match egress.grant(&id, &domains) {
                Ok(g) => Some(g).filter(|g| g.truthy()),
                Err(e) => {
                    let _ = store.cancel(&id, &|_| false);
                    let text = e
                        .public
                        .filter(|p| !p.is_empty())
                        .unwrap_or_else(|| "The egress proxy refused this task.".into());
                    return Err(Fail::public(503, &text));
                }
            },
            None => None,
        };
        let run = Arc::new(Run {
            id: id.clone(),
            store: store.clone(),
            registry: self.registry.clone(),
            egress: self.options.egress.clone(),
            grant: grant.clone(),
            signal: Arc::new(Signal::new()),
            gate: Mutex::new(()),
            queue: Mutex::new(Queue::default()),
            wake: Condvar::new(),
            executor: Mutex::new(None),
            opening: Mutex::new(None),
            executor_closed: AtomicBool::new(true),
            cleanup: Mutex::new(None),
            close_timeout: self.options.close_timeout,
            active: Mutex::new(None),
        });
        let proxy = grant.as_ref().map(|g| Proxy {
            server: format!(
                "http://{}",
                self.options
                    .egress
                    .as_ref()
                    .and_then(|e| e.endpoint())
                    .filter(|e| !e.is_empty())
                    .unwrap_or_else(|| "egress".into())
            ),
            username: "task".into(),
            password: executor::js_string(g.get("token")),
        });
        if proxy.is_none() && !self.options.direct {
            run.cleanup().wait();
            let _ = store.cancel(&id, &|_| false);
            return Err(Fail::public(503, NEEDS_EGRESS));
        }
        let ready: Arc<Slot<Result<(), Fail>>> = Arc::new(Slot::new());
        // jobs.cjs `run`: `job.started` before the work is live.
        if let Err(error) = run.append("job.started", JValue::Undefined) {
            run.cleanup().wait();
            if store
                .get(&id)
                .ok()
                .flatten()
                .is_some_and(|j| j.status.as_str() == Some("queued"))
            {
                let _ = store.append(
                    &id,
                    "job.failed",
                    JValue::obj([("error", s("The browser could not start."))]),
                );
            }
            ready.settle(Err(error.clone()));
            self.log(JValue::obj([
                ("event", s("browser.job.error")),
                ("jobId", s(&id)),
                ("error", s(&error.message)),
            ]));
        } else {
            lock(&self.live).insert(id.clone(), run.clone());
            let (me, run2, ready2) = (self.clone(), run.clone(), ready.clone());
            let (domains2, project) = (domains.clone(), project_id.to_string());
            std::thread::spawn(move || {
                let work = std::panic::AssertUnwindSafe(|| {
                    me.work(&run2, &ready2, &domains2, &project, downloads_dir, proxy)
                });
                let outcome = std::panic::catch_unwind(work).unwrap_or_else(|_| {
                    // Nobody waits forever on a task whose own thread stopped, and its session and
                    // grant are still taken away.
                    run2.close_queue(true);
                    run2.cleanup().wait();
                    Err(Fail::plain(None, "The browser task stopped unexpectedly."))
                });
                if let Err(e) = &outcome {
                    ready2.settle(Err(e.clone()));
                }
                me.end(&run2, outcome);
            });
        }
        match ready.wait() {
            Ok(()) => Ok(JValue::obj([
                ("taskId", s(&id)),
                (
                    "domains",
                    JValue::Arr(domains.iter().map(|d| s(d)).collect()),
                ),
            ])),
            Err(error) => {
                let status = error.status.filter(|st| *st != 0).unwrap_or(502);
                let text = error
                    .public
                    .filter(|p| !p.is_empty())
                    .unwrap_or_else(|| "The browser could not open.".into());
                Err(Fail::public(status, &text))
            }
        }
    }

    /// The end of jobs.cjs `run`: completed, failed or cancelled, exactly once.
    fn end(&self, run: &Arc<Run>, outcome: Result<JValue, Fail>) {
        {
            let _gate = run.gate();
            let aborted = run.signal.is_aborted();
            let (kind, data) = match outcome {
                Ok(result) if aborted => (
                    "job.cancelled",
                    if matches!(result, JValue::Undefined) {
                        JValue::obj::<&str>([])
                    } else {
                        JValue::obj([("result", result)])
                    },
                ),
                Ok(result) => ("job.completed", JValue::obj([("result", result)])),
                Err(_) if aborted => ("job.cancelled", JValue::Undefined),
                Err(e) => {
                    let text = e
                        .public
                        .clone()
                        .filter(|p| !p.is_empty())
                        .or_else(|| Some(e.message.clone()).filter(|m| !m.is_empty()))
                        .unwrap_or_else(|| "failed".into());
                    (
                        "job.failed",
                        JValue::obj([
                            ("error", s(&wtf16::slice(&text, 500))),
                            ("result", e.result.clone().unwrap_or(JValue::Null)),
                        ]),
                    )
                }
            };
            let _ = run.append_held(kind, data);
        }
        lock(&self.live).remove(&run.id);
    }

    /// The task's work: open the session, take actions, clean up, and say what the task did.
    fn work(
        self: &Arc<Self>,
        run: &Arc<Run>,
        ready: &Arc<Slot<Result<(), Fail>>>,
        domains: &[String],
        project_id: &str,
        downloads_dir: PathBuf,
        proxy: Option<Proxy>,
    ) -> Result<JValue, Fail> {
        // A cancel takes the session away at once (and says an action it interrupted may have run).
        // It runs on the cancelling thread with the task's gate held: only signals and spawns.
        let on_cancel = {
            let r = run.clone();
            run.signal.on_abort(move || {
                let _ = r.cleanup();
                if let Some(kind) = lock(&r.active).clone() {
                    let _ = r.append_held(
                        "tool.uncertain",
                        JValue::obj([
                            ("tool", s("browser")),
                            ("action", s(&kind)),
                            (
                                "reason",
                                s("The action was still pending when cancellation was accepted."),
                            ),
                        ]),
                    );
                }
            })
        };
        let waker = {
            let r = run.clone();
            run.signal.on_abort(move || {
                let _queue = lock(&r.queue);
                r.wake.notify_all();
            })
        };
        if run.signal.is_aborted() {
            let _ = run.cleanup();
        }
        let outcome = self.work_inner(run, ready, domains, project_id, downloads_dir, proxy);
        let outcome = match outcome {
            Ok(v) => Ok(v),
            Err(e) => {
                run.cleanup().wait();
                ready.settle(Err(e.clone()));
                Err(e)
            }
        };
        run.signal.off(on_cancel);
        run.signal.off(waker);
        run.cleanup().wait();
        outcome
    }

    /// `executor.open`, raced against a cancel as core races it: whichever comes first.
    fn open(&self, run: &Arc<Run>, exec: &Arc<Executor>, task: Open) -> Result<String, Fail> {
        if run.signal.is_aborted() {
            return Err(cancelled());
        }
        let first: Arc<Slot<Option<Result<String, Fail>>>> = Arc::new(Slot::new());
        let (e2, f2) = (exec.clone(), first.clone());
        let opening = Flight::spawn_or(
            Err(Fail::plain(None, "The browser could not open.")),
            move || {
                let r = e2.open(task);
                f2.settle(Some(r.clone()));
                r
            },
        );
        *lock(&run.opening) = Some(opening.clone());
        let f3 = first.clone();
        let hook = run.signal.on_abort(move || {
            f3.settle(None);
        });
        if hook.is_none() {
            first.settle(None);
        }
        let outcome = loop {
            if let Some(o) = first.wait_for(Duration::from_millis(50)) {
                break o;
            }
            // A panicking open settles its flight, not `first`.
            if let Some(r) = opening.peek() {
                first.settle(Some(r));
            }
        };
        run.signal.off(hook);
        outcome.unwrap_or_else(|| Err(cancelled()))
    }

    fn work_inner(
        self: &Arc<Self>,
        run: &Arc<Run>,
        ready: &Arc<Slot<Result<(), Fail>>>,
        domains: &[String],
        project_id: &str,
        downloads_dir: PathBuf,
        proxy: Option<Proxy>,
    ) -> Result<JValue, Fail> {
        let id = run.id.clone();
        run.append(
            "checkpoint.created",
            JValue::obj([(
                "domains",
                JValue::Arr(domains.iter().map(|d| s(d)).collect()),
            )]),
        )?;
        let ask: executor::Ask = {
            let (r, registry, timeout, new_id) = (
                run.clone(),
                self.registry.clone(),
                self.options.timeout,
                self.options.new_id.clone(),
            );
            Arc::new(move |card: &JValue| -> Result<String, Fail> {
                let mut card = card.clone();
                if let JValue::Obj(items) = &mut card {
                    wtf16::set(items, "jobId", s(&r.id));
                }
                r.append("approval.requested", card.clone())?;
                let answer = ask_approval(&registry, &new_id, timeout, &card, &r.signal);
                r.append(
                    "approval.decided",
                    JValue::obj([
                        ("decision", s(&answer)),
                        ("action", card.get("action").clone()),
                    ]),
                )?;
                Ok(answer)
            })
        };
        let log = {
            let (log, project) = (self.options.log.clone(), project_id.to_string());
            Arc::new(move |entry: JValue| {
                let mut items = match entry {
                    JValue::Obj(items) => items,
                    _ => Vec::new(),
                };
                wtf16::set(&mut items, "projectId", s(&project));
                log(JValue::Obj(items));
            })
        };
        let exec = Executor::new(executor::Deps {
            launch: self.options.launch.clone(),
            ask,
            secrets: Vec::new(),
            log,
            timeout: self.options.action_timeout,
            direct: self.options.direct,
            new_id: self.options.new_id.clone(),
        });
        *lock(&run.executor) = Some(exec.clone());
        let task = Open {
            job_id: id.clone(),
            allowed_domains: domains.to_vec(),
            downloads_dir,
            upload_files: Vec::new(),
            signal: Some(run.signal.clone()),
            proxy,
        };
        let session_id = match self.open(run, &exec, task) {
            Ok(sid) => sid,
            Err(e) => {
                run.cleanup().wait();
                ready.settle(Err(e.clone()));
                return Err(e);
            }
        };
        {
            let mut queue = lock(&run.queue);
            if !run.signal.is_aborted() {
                queue.open = true;
            }
        }
        ready.settle(Ok(()));
        let mut output = JValue::Null;
        loop {
            if run.signal.is_aborted() {
                break;
            }
            let item = {
                let mut queue = lock(&run.queue);
                // A limit past what the clock can hold is no limit.
                let deadline = Instant::now().checked_add(self.options.idle_timeout);
                loop {
                    if run.signal.is_aborted() {
                        break None;
                    }
                    if let Some(item) = queue.items.pop_front() {
                        break Some(item);
                    }
                    match deadline {
                        Some(d) => {
                            let left = d.saturating_duration_since(Instant::now());
                            if left.is_zero() {
                                drop(queue);
                                return Err(Fail {
                                    status: None,
                                    message: TIMEOUT_REASON.into(),
                                    public: Some(TIMEOUT_REASON.into()),
                                    result: None,
                                });
                            }
                            queue = run
                                .wake
                                .wait_timeout(queue, left)
                                .unwrap_or_else(PoisonError::into_inner)
                                .0;
                        }
                        None => {
                            queue = run.wake.wait(queue).unwrap_or_else(PoisonError::into_inner);
                        }
                    }
                }
            };
            let Some(item) = item else { break };
            match item {
                Item::Finish { result } => {
                    output = result;
                    break;
                }
                Item::Act { action, reply } => {
                    self.act_one(run, &exec, &session_id, action, &reply)
                }
            }
        }
        let network = run.cleanup().wait();
        let result = if network.is_nullish() {
            output
        } else {
            let mut items = match &output {
                JValue::Obj(items) => items.clone(),
                other => vec![("value".to_string(), other.clone())],
            };
            wtf16::set(&mut items, "network", network.clone());
            JValue::Obj(items)
        };
        let unconfirmed =
            !network.is_nullish() && network.get("cleanupConfirmed").as_bool() != Some(true);
        if unconfirmed || !run.executor_closed.load(Ordering::SeqCst) {
            let mut fail = Fail::public(503, "Browser cleanup could not be confirmed.");
            fail.result = Some(result);
            return Err(fail);
        }
        Ok(result)
    }

    /// One queued action, raced against a cancel (`whileActive`): the action's own answer if it comes
    /// first; otherwise 499 once the cleanup has taken the session away, and the task goes on to its
    /// end without waiting for an engine call that may never return.
    fn act_one(
        &self,
        run: &Arc<Run>,
        exec: &Arc<Executor>,
        session_id: &str,
        action: JValue,
        reply: &Reply,
    ) {
        if run.signal.is_aborted() {
            reply.settle(Err(cancelled()));
            return;
        }
        let late = |r: &Arc<Run>, rep: &Reply| {
            let (r, rep) = (r.clone(), rep.clone());
            std::thread::spawn(move || {
                r.cleanup().wait();
                rep.settle(Err(cancelled()));
            });
        };
        let hook = {
            let (r, rep) = (run.clone(), reply.clone());
            run.signal.on_abort(move || late(&r, &rep))
        };
        if hook.is_none() {
            // Cancelled since the check above: the action is never sent.
            late(run, reply);
            return;
        }
        let kind = executor::js_string(action.get("type"));
        *lock(&run.active) = Some(kind.clone());
        let (e2, sid) = (exec.clone(), session_id.to_string());
        let acting = Flight::spawn_or(
            Err(Fail::plain(
                None,
                "The browser action stopped unexpectedly.",
            )),
            move || e2.act(&sid, &action),
        );
        let outcome = loop {
            if let Some(r) = acting.wait_for(Duration::from_millis(20)) {
                break Some(r);
            }
            if run.signal.is_aborted() && run.cleaned() {
                break None;
            }
        };
        *lock(&run.active) = None;
        run.signal.off(hook);
        match outcome {
            None => {
                reply.settle(Err(cancelled()));
            }
            Some(Ok(result)) => {
                if !run.signal.is_aborted() {
                    let stage = format!("{}: {}", kind, executor::js_string(result.get("status")));
                    let _ = run.append("progress", JValue::obj([("stage", s(&stage))]));
                }
                reply.settle(Ok(result));
            }
            Some(Err(e)) => {
                reply.settle(Err(e));
            }
        }
    }

    fn running(job: &Job) -> bool {
        matches!(job.status.as_str(), Some("running" | "waiting_approval"))
    }

    /// The live task's queue, open for actions; core's 409 otherwise.
    fn enqueue(&self, task_id: &str, item: Item) -> Result<(), Fail> {
        let run = lock(&self.live).get(task_id).cloned();
        let not_open = || Fail::public(409, "This task is not open for actions.");
        let run = run.ok_or_else(not_open)?;
        let mut queue = lock(&run.queue);
        if !queue.open {
            return Err(not_open());
        }
        queue.items.push_back(item);
        run.wake.notify_all();
        Ok(())
    }

    /// `act(workspace, project, taskId, action)`: runs one action and waits for its outcome.
    pub fn act(
        &self,
        dir: &Path,
        project_id: &str,
        task_id: &str,
        action: &JValue,
    ) -> Result<JValue, Fail> {
        let tenant = self.tenant(dir)?;
        let job = self.owned(&tenant, project_id, task_id)?;
        if !Service::running(&job) {
            return Err(Fail::public(409, "This task is not running."));
        }
        let kind = action.get("type");
        if !action.truthy() || kind.as_str().is_none_or(str::is_empty) {
            return Err(Fail::public(400, "Name an action to run."));
        }
        let reply: Reply = Arc::new(Slot::new());
        self.enqueue(
            task_id,
            Item::Act {
                action: action.clone(),
                reply: reply.clone(),
            },
        )?;
        reply.wait()
    }

    /// `decide(workspace, project, taskId, decision, approvalId)`.
    pub fn decide(
        &self,
        dir: &Path,
        project_id: &str,
        task_id: &str,
        decision: &str,
        approval_id: &str,
    ) -> Result<JValue, Fail> {
        let tenant = self.tenant(dir)?;
        self.owned(&tenant, project_id, task_id)?;
        if !matches!(decision, "approve" | "approve_all" | "deny") {
            return Err(Fail::public(400, "Unknown decision"));
        }
        if approval_id.is_empty() {
            return Err(Fail::public(400, "Which approval is this answer for?"));
        }
        let run = lock(&self.live).get(task_id).cloned();
        // Found and answered under one lock: a card cannot time out in between, and a cancelled task's
        // card is never answered (core refuses its cards before it aborts the task).
        let waiting = {
            let mut pending = lock(&self.registry.pending);
            let cancelled = run.as_ref().is_some_and(|r| r.signal.is_aborted());
            match pending
                .iter()
                .position(|w| w.id == approval_id && w.task_id == task_id && !cancelled)
            {
                Some(i) => Ok(pending.remove(i)),
                None => {
                    let now = (self.options.now)();
                    Err(lock(&self.registry.expired)
                        .get(approval_id)
                        .is_some_and(|until| *until > now))
                }
            }
        };
        match waiting {
            Ok(w) => {
                w.slot.settle(
                    if decision == "deny" {
                        "deny"
                    } else {
                        "approve"
                    }
                    .into(),
                );
                Ok(JValue::obj([("ok", JValue::Bool(true))]))
            }
            Err(expired) => Err(Fail::public(
                409,
                if expired {
                    "This approval expired before anyone answered it."
                } else {
                    "That approval is no longer waiting."
                },
            )),
        }
    }

    /// `finish(workspace, project, taskId, result)`: ends the task cleanly with the caller's result.
    pub fn finish(
        &self,
        dir: &Path,
        project_id: &str,
        task_id: &str,
        result: &JValue,
    ) -> Result<JValue, Fail> {
        let tenant = self.tenant(dir)?;
        let job = self.owned(&tenant, project_id, task_id)?;
        if !Service::running(&job) {
            return Err(Fail::public(409, "This task is not running."));
        }
        let result = if matches!(result, JValue::Undefined) {
            JValue::Null
        } else {
            result.clone()
        };
        if !result.is_nullish()
            && wtf16::stringify(&result).is_some_and(|t| t.len() > MAX_RESULT_BYTES)
        {
            return Err(Fail::public(413, "That result is too large to record."));
        }
        self.enqueue(task_id, Item::Finish { result })?;
        Ok(JValue::obj([("ok", JValue::Bool(true))]))
    }

    /// `cancel(workspace, project, taskId)`: the task as the cancel leaves it, before the cleanup it
    /// sets off has run.
    pub fn cancel(&self, dir: &Path, project_id: &str, task_id: &str) -> Result<JValue, Fail> {
        let tenant = self.tenant(dir)?;
        let job = self.owned(&tenant, project_id, task_id)?;
        if is_terminal(&job) {
            self.registry.abort_task(task_id);
            return Ok(browser_view(&job, &JValue::Null));
        }
        let run = lock(&self.live).get(task_id).cloned();
        let job = match run {
            Some(run) => {
                let _gate = run.gate();
                run.signal.abort();
                // core's cleanup starts right after the cancel: the task takes no more actions.
                run.close_queue(false);
                // Refuse anything still waiting, so a cancelled task never leaves a card that could
                // later be answered into an action.
                self.registry.abort_task(task_id);
                tenant.store.get(task_id).map_err(refused)?
            }
            None => {
                self.registry.abort_task(task_id);
                tenant.store.cancel(task_id, &|_| false).map_err(refused)?
            }
        };
        Ok(job.map_or(JValue::Null, |j| browser_view(&j, &JValue::Null)))
    }
}

/// `askApproval(card, { signal })`: a card with a fresh id, refused as `timeout` after the
/// service's timeout and as `aborted` once the task's signal fires; `approve` or `deny`. The card is
/// never shown on a cancelled task: the abort hook is in place before the card, and the card is
/// added under the registry's lock only while the task is not cancelled.
fn ask_approval(
    registry: &Arc<Registry>,
    new_id: &Arc<dyn Fn() -> String + Send + Sync>,
    timeout: Duration,
    card: &JValue,
    signal: &Arc<Signal>,
) -> String {
    let id = new_id();
    let slot = Arc::new(Slot::new());
    let mut request = card.clone();
    if let JValue::Obj(items) = &mut request {
        wtf16::set(items, "id", s(&id));
    }
    let hook = {
        let (registry, id) = (registry.clone(), id.clone());
        signal.on_abort(move || registry.finish(&id, "aborted"))
    };
    if hook.is_none() {
        return "deny".into();
    }
    {
        let mut pending = lock(&registry.pending);
        if signal.is_aborted() {
            drop(pending);
            signal.off(hook);
            return "deny".into();
        }
        pending.push(Waiting {
            id: id.clone(),
            task_id: card.get("jobId").as_str().unwrap_or("").to_string(),
            request,
            slot: slot.clone(),
        });
    }
    let decision = match slot.wait_for(timeout) {
        Some(d) => d,
        None => {
            registry.finish(&id, "timeout");
            slot.wait()
        }
    };
    signal.off(hook);
    if decision == "approve" {
        "approve".into()
    } else {
        "deny".into()
    }
}
