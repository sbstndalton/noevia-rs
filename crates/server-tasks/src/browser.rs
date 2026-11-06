//! Native browser-task lifecycle. HTTP waiters never own execution or cleanup; the native
//! worker retains the reservation, journal and network authority through engine settlement.
use crate::{
    account::Account,
    browser_action::{Actions, BrowserEngine, State as PageState},
    code::Failure,
    context::{Clock, JournalContext},
    egress::{Egress, Grant, Network},
    session::Context,
    Scope,
};
use js_json::{wtf16, JValue};
use server_projects::task_journals::{self, Found, Surface};
use std::{
    collections::HashMap,
    future::Future,
    pin::Pin,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::sync::{mpsc, oneshot};
pub struct Proxy {
    pub server: String,
    pub username: String,
    pub password: zeroize::Zeroizing<String>,
}
pub type Open =
    Pin<Box<dyn Future<Output = Result<Arc<dyn BrowserEngine>, Failure>> + Send + 'static>>;
/// Account-bound primitive engine factory. On a dropped open waiter, ownership must move to
/// a settle task that closes any created browser; no task execution or policy lives here.
pub trait Engines: Send + Sync {
    fn open(
        &self,
        context: Arc<JournalContext>,
        state: Arc<PageState>,
        proxy: Option<Proxy>,
    ) -> Open;
}
enum Item {
    Act(JValue, oneshot::Sender<Result<JValue, Failure>>),
    Finish(JValue),
}
struct Coverage(tokio::sync::watch::Sender<bool>);
impl Drop for Coverage {
    fn drop(&mut self) {
        self.0.send_replace(true);
    }
}
fn watch_context(context: Arc<JournalContext>) -> Coverage {
    let (stop, mut stopped) = tokio::sync::watch::channel(false);
    tokio::spawn(async move {
        loop {
            tokio::select! {biased;_=stopped.changed()=>break,_=tokio::time::sleep(Duration::from_millis(200))=>{if context.account().fence().is_err(){let _=context.account().control.cancel(context.scope(),context.task());break;}}}
        }
    });
    Coverage(stop)
}
struct Live {
    context: Arc<JournalContext>,
    sender: Option<mpsc::UnboundedSender<Item>>,
    started: bool,
    restart: Option<&'static str>,
}
#[derive(Default)]
struct State {
    live: HashMap<(String, String), Live>,
    workers: Vec<tokio::task::JoinHandle<()>>,
    draining: bool,
}
pub struct Browser {
    state: Mutex<State>,
    engines: Arc<dyn Engines>,
    egress: Option<Arc<dyn Egress>>,
    clock: Arc<dyn Clock>,
    approval_timeout: Duration,
    idle_timeout: Duration,
    close_timeout: Duration,
    direct: bool,
}
fn object(rows: Vec<(&str, JValue)>) -> JValue {
    JValue::Obj(rows.into_iter().map(|(k, v)| (k.into(), v)).collect())
}
fn string(value: &str) -> JValue {
    JValue::Str(value.into())
}
async fn wait_cancel(cancel: &mut tokio::sync::watch::Receiver<bool>) {
    if !*cancel.borrow() {
        let _ = cancel.changed().await;
    }
}
fn okay() -> JValue {
    object(vec![("ok", JValue::Bool(true))])
}
fn owned(found: Found) -> Result<Box<task_journals::Job>, Failure> {
    match found {
        Found::Task(job) => Ok(job),
        Found::Missing => Err(Failure::public(404, "Task not found")),
        Found::Unreadable => Err(Failure::public(
            409,
            "Unreadable job journal; review required",
        )),
    }
}
fn domains(body: &JValue) -> Result<Vec<String>, Failure> {
    if body == &JValue::Null {
        return Err(Failure::public(500, "Browser request failed"));
    }
    let JValue::Arr(rows) = body.get("domains") else {
        return Ok(vec![]);
    };
    let mut out = vec![];
    for value in rows {
        let value = if value.truthy() {
            js_json::to_js_string(value)
                .map_err(|_| Failure::public(500, "Browser request failed"))?
        } else {
            String::new()
        };
        let value = server_account::util::js_trim(&value).to_lowercase();
        if let Some((head, tail)) = value.rsplit_once('.') {
            if !head.is_empty()
                && head
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b".-".contains(&b))
                && tail.len() >= 2
                && tail.bytes().all(|b| b.is_ascii_lowercase())
            {
                out.push(value);
                if out.len() == 20 {
                    break;
                }
            }
        }
    }
    Ok(out)
}
impl Browser {
    pub fn new(
        engines: Arc<dyn Engines>,
        egress: Option<Arc<dyn Egress>>,
        clock: Arc<dyn Clock>,
        approval_timeout: Duration,
        idle_timeout: Duration,
        direct: bool,
    ) -> Self {
        Self {
            state: Mutex::new(State::default()),
            engines,
            egress,
            clock,
            approval_timeout,
            idle_timeout,
            close_timeout: Duration::from_secs(5),
            direct,
        }
    }
    pub fn facts(&self, account: &Account) -> Result<JValue, Failure> {
        account.fence().map_err(|_| Failure::unavailable())?;
        let network = self
            .egress
            .as_ref()
            .map(|e| e.network())
            .transpose()
            .map_err(|_| Failure::unavailable())?
            .unwrap_or(Network::Unconfigured);
        let state = self.state.lock().map_err(|_| Failure::unavailable())?;
        let mut approvals = vec![];
        for ((user, id), live) in &state.live {
            if user != account.user() {
                continue;
            }
            let cards = account
                .control
                .cards(live.context.scope())
                .map_err(|_| Failure::unavailable())?;
            if let Some((approval, _, card)) = cards.into_iter().find(|(_, task, _)| task == id) {
                let mut card = card;
                if let JValue::Obj(rows) = &mut card {
                    rows.insert(0, ("id".into(), string(&approval)));
                }
                approvals.push((id.clone(), card));
            }
        }
        Ok(object(vec![
            ("capabilities", JValue::Arr(vec![string("open_browser")])),
            ("network", JValue::Bool(network == Network::Ready)),
            ("networkState", string(network.as_str())),
            ("approvals", JValue::Obj(approvals)),
        ]))
    }
    pub async fn start(
        self: &Arc<Self>,
        account: Arc<Account>,
        project: String,
        body: JValue,
    ) -> Result<JValue, Failure> {
        let domains = domains(&body)?;
        if domains.is_empty() {
            return Err(Failure::public(
                400,
                "A browser task needs at least one domain it may reach.",
            ));
        }
        let (answer, wait) = oneshot::channel();
        let actor = self.clone();
        let worker = tokio::spawn(async move {
            actor.drive(account, project, domains, answer).await;
        });
        self.state
            .lock()
            .map_err(|_| Failure::unavailable())?
            .workers
            .push(worker);
        wait.await.unwrap_or_else(|_| Err(Failure::unavailable()))
    }
    fn prepare(
        &self,
        account: Arc<Account>,
        project: &str,
    ) -> Result<Arc<JournalContext>, Failure> {
        let mut state = self.state.lock().map_err(|_| Failure::unavailable())?;
        if state.draining {
            return Err(Failure::unavailable());
        }
        if !account
            .list(Surface::Browser, Some(project), true)
            .map_err(|_| Failure::unavailable())?
            .is_empty()
        {
            return Err(Failure::public(
                409,
                "This project already has a browser task running.",
            ));
        }
        if self.egress.is_none() && !self.direct {
            return Err(Failure::public(
                503,
                "Browser tasks need the egress proxy (D15) configured on this server.",
            ));
        }
        let id = crate::random_id().map_err(|_| Failure::unavailable())?;
        let scope = Scope {
            user: account.user().into(),
            project: project.into(),
            surface: Surface::Browser,
        };
        let guard = account.control.begin(scope, id.clone()).map_err(|error| {
            if error == crate::Error::Busy {
                Failure::public(409, "This project already has a browser task running.")
            } else {
                Failure::unavailable()
            }
        })?;
        account
            .create(
                Surface::Browser,
                &id,
                project,
                vec![string("open_browser")],
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
        state.live.insert(
            (account.user().into(), id),
            Live {
                context: context.clone(),
                sender: None,
                started: false,
                restart: None,
            },
        );
        Ok(context)
    }
    async fn drive(
        self: Arc<Self>,
        account: Arc<Account>,
        project: String,
        domains: Vec<String>,
        answer: oneshot::Sender<Result<JValue, Failure>>,
    ) {
        let context = match self.prepare(account.clone(), &project) {
            Ok(context) => context,
            Err(error) => {
                let _ = answer.send(Err(error));
                return;
            }
        };
        let _coverage = watch_context(context.clone());
        let key = (account.user().to_owned(), context.task().to_owned());
        let mut grant = None;
        if let Some(egress) = &self.egress {
            match egress.grant(context.task(), &domains).await {
                Ok(value) => grant = Some(value),
                Err(error) => {
                    let _ = context.event("job.cancelled", object(vec![]));
                    let _ = answer.send(Err(Failure::public(
                        503,
                        error
                            .public_message
                            .unwrap_or_else(|| "The egress proxy refused this task.".into()),
                    )));
                    self.remove(&key);
                    return;
                }
            }
        }
        if context.active().is_err() {
            let _ = answer.send(Err(Failure::public(499, "Cancelled")));
            self.cleanup(context.clone(), None, grant).await;
            self.remove(&key);
            return;
        }
        if context.event("job.started", object(vec![])).is_err() {
            let _ = answer.send(Err(Failure::unavailable()));
            self.cleanup(context.clone(), None, grant).await;
            self.remove(&key);
            return;
        }
        if context
            .event(
                "checkpoint.created",
                object(vec![(
                    "domains",
                    JValue::Arr(domains.iter().map(|d| string(d)).collect()),
                )]),
            )
            .is_err()
        {
            let _ = answer.send(Err(Failure::unavailable()));
            self.cleanup(context.clone(), None, grant).await;
            self.remove(&key);
            return;
        }
        if let Ok(mut state) = self.state.lock() {
            if let Some(live) = state.live.get_mut(&key) {
                live.started = true;
            }
        }
        let proxy = grant.as_ref().map(|g| Proxy {
            server: format!(
                "http://{}",
                self.egress.as_ref().map_or("egress", |e| e.endpoint())
            ),
            username: "task".into(),
            password: zeroize::Zeroizing::new(g.token().into()),
        });
        let page_state = Arc::new(PageState::new(domains.clone(), Arc::new(vec![])));
        let opening = self
            .engines
            .open(context.clone(), page_state.clone(), proxy);
        tokio::pin!(opening);
        let mut cancel = context.cancellation();
        let engine = tokio::select! {biased;
            _=wait_cancel(&mut cancel)=>{
                let _=answer.send(Err(Failure::public(499,"The browser could not open.")));
                let revoke=self.cleanup(context.clone(),None,grant);
                let close=async {if let Ok(engine)=opening.await{let _=engine.close().await;}};
                tokio::join!(revoke,close);
                let _=context.event("job.cancelled",object(vec![]));self.remove(&key);return;
            },
            result=&mut opening=>match result {Ok(engine)=>engine,Err(error)=>{
                let status=error.status.unwrap_or(502);let message=error.public_message.unwrap_or_else(||"The browser could not open.".into());
                let _=answer.send(Err(Failure::public(status,message)));
                self.cleanup(context.clone(),None,grant).await;
                let _=context.event("job.failed",object(vec![("error",string("The browser could not open."))]));self.remove(&key);return;
            }}
        };
        let (sender, mut queue) = mpsc::unbounded_channel();
        if let Ok(mut state) = self.state.lock() {
            if let Some(live) = state.live.get_mut(&key) {
                live.sender = Some(sender);
            }
        }
        let _ = answer.send(Ok(object(vec![
            ("taskId", string(context.task())),
            (
                "domains",
                JValue::Arr(domains.iter().map(|d| string(d)).collect()),
            ),
        ])));
        let mut actions = Actions::new(
            engine.clone(),
            context.clone(),
            page_state,
            context.task().into(),
        );
        let mut output = JValue::Null;
        let mut error = None;
        let mut pending = None;
        loop {
            if context.active().is_err() {
                break;
            }
            let item = tokio::select! {biased;_=wait_cancel(&mut cancel)=>break,item=queue.recv()=>item,_=tokio::time::sleep(self.idle_timeout)=>{error=Some("This browser task made no progress and timed out.");break;}};
            let Some(item) = item else {
                break;
            };
            match item {
                Item::Finish(result) => {
                    output = result;
                    break;
                }
                Item::Act(action, reply) => {
                    let kind = action.get("type").as_str().unwrap_or("").to_owned();
                    let mut running = tokio::spawn(async move {
                        let result = actions.act(action).await;
                        (actions, result)
                    });
                    tokio::select! {biased;result=&mut running=>match result {Ok((returned,result))=>{actions=returned;if context.active().is_ok(){let _=context.event("progress",object(vec![("stage",string(&format!("{kind}: {}",result.get("status").as_str().unwrap_or(""))))]));}let _=reply.send(Ok(result));},Err(_)=>{let _=reply.send(Err(Failure::unavailable()));error=Some("The browser could not complete this action.");break;}},_=wait_cancel(&mut cancel)=>{let _=context.event("tool.uncertain",object(vec![("tool",string("browser")),("action",string(&kind)),("reason",string("The action was still pending when cancellation was accepted."))]));pending=Some((running,reply));break;}}
                }
            }
        }
        if let Ok(mut state) = self.state.lock() {
            if let Some(live) = state.live.get_mut(&key) {
                live.sender = None;
            }
        }
        context.end_approvals();
        queue.close();
        while let Ok(item) = queue.try_recv() {
            if let Item::Act(_, reply) = item {
                let _ = reply.send(Err(Failure::public(
                    409,
                    "This task ended before this action ran.",
                )));
            }
        }
        let actor = self.clone();
        let cleaning = context.clone();
        let mut cleanup =
            tokio::spawn(async move { actor.cleanup(cleaning, Some(engine), grant).await });
        let completed = if pending.is_some() {
            tokio::select! {result=&mut cleanup=>Some(result),_=tokio::time::sleep(self.close_timeout)=>None}
        } else {
            None
        };
        if let Some((running, reply)) = pending {
            let _ = reply.send(Err(Failure::public(499, "Cancelled")));
            let _ = running.await;
        }
        // The settle task continues polling close/revoke while the action itself unwinds.
        let (network, confirmed) = match completed {
            Some(result) => result.unwrap_or((None, false)),
            None => cleanup.await.unwrap_or((None, false)),
        };
        if !confirmed {
            error = Some("Browser cleanup could not be confirmed.");
        }
        if let Some(network) = network {
            let mut rows = match output {
                JValue::Obj(rows) => rows,
                _ => vec![("value".into(), output)],
            };
            rows.push(("network".into(), network));
            output = JValue::Obj(rows);
        }
        let restart = self
            .state
            .lock()
            .ok()
            .and_then(|s| s.live.get(&key).and_then(|l| l.restart));
        if let Some(reason) = restart {
            let _ = context.event("job.interrupted", object(vec![("reason", string(reason))]));
        } else if *context.cancellation().borrow() {
            let _ = context.event("job.cancelled", object(vec![("result", output)]));
        } else if let Some(reason) = error {
            let _ = context.event(
                "job.failed",
                object(vec![("error", string(reason)), ("result", output)]),
            );
        } else {
            let _ = context.event("job.completed", object(vec![("result", output)]));
        }
        self.remove(&key);
    }
    fn remove(&self, key: &(String, String)) {
        if let Ok(mut state) = self.state.lock() {
            state.live.remove(key);
        }
    }
    async fn cleanup(
        &self,
        context: Arc<JournalContext>,
        engine: Option<Arc<dyn BrowserEngine>>,
        grant: Option<Grant>,
    ) -> (Option<JValue>, bool) {
        let closed = async {
            if let Some(engine) = engine {
                match engine.close().await {
                    Ok(answer) if answer.get("confirmed") == &JValue::Bool(true) => true,
                    _ => {
                        let _ = context.event(
                            "tool.uncertain",
                            object(vec![
                                ("tool", string("network")),
                                (
                                    "reason",
                                    string("Browser executor closure could not be confirmed."),
                                ),
                            ]),
                        );
                        false
                    }
                }
            } else {
                true
            }
        };
        let revoked = async {
            let (Some(egress), Some(grant)) = (&self.egress, grant) else {
                return None;
            };
            let cleanup = egress.revoke(&grant).await;
            let (mut report, confirmed) = match cleanup {
                Ok(cleanup) => (cleanup.report, cleanup.confirmed),
                Err(_) => (
                    object(vec![
                        ("available", JValue::Bool(false)),
                        ("final", JValue::Bool(false)),
                        ("partial", JValue::Bool(true)),
                        ("state", string("uncertain")),
                    ]),
                    false,
                ),
            };
            if let JValue::Obj(rows) = &mut report {
                rows.retain(|(k, _)| k != "cleanupConfirmed");
                rows.push(("cleanupConfirmed".into(), JValue::Bool(confirmed)));
            }
            if !confirmed {
                let _ = context.event(
                    "tool.uncertain",
                    object(vec![
                        ("tool", string("network")),
                        (
                            "reason",
                            string("Native egress cleanup could not be confirmed."),
                        ),
                    ]),
                );
            }
            let retained = context
                .event(
                    "artifact.created",
                    object(vec![
                        ("name", string("network.cleanup")),
                        ("kind", string("native-network-report")),
                        ("network", report.clone()),
                    ]),
                )
                .is_ok()
                && context.sync().is_ok();
            if retained && confirmed && report.get("final") == &JValue::Bool(true) {
                if let Some(snapshot) = report.get("snapshotId").as_str() {
                    let _ = egress.forget(&grant, snapshot).await;
                }
            }
            Some(report)
        };
        let (network, confirmed) = tokio::join!(revoked, closed);
        let confirmed = confirmed
            && network
                .as_ref()
                .is_none_or(|n| n.get("cleanupConfirmed") == &JValue::Bool(true));
        (network, confirmed)
    }
    pub fn act(
        &self,
        account: &Account,
        project: &str,
        id: &str,
        action: JValue,
    ) -> Result<oneshot::Receiver<Result<JValue, Failure>>, Failure> {
        self.running(account, project, id)?;
        if action.get("type").as_str().is_none_or(|s| s.is_empty()) {
            return Err(Failure::public(400, "Name an action to run."));
        }
        let state = self.state.lock().map_err(|_| Failure::unavailable())?;
        let live = state
            .live
            .get(&(account.user().into(), id.into()))
            .and_then(|l| l.sender.as_ref())
            .ok_or_else(|| Failure::public(409, "This task is not open for actions."))?;
        let (reply, wait) = oneshot::channel();
        live.send(Item::Act(action, reply))
            .map_err(|_| Failure::public(409, "This task is not open for actions."))?;
        Ok(wait)
    }
    fn running(&self, account: &Account, project: &str, id: &str) -> Result<(), Failure> {
        let job = owned(
            account
                .owned(Surface::Browser, project, id)
                .map_err(|_| Failure::unavailable())?,
        )?;
        if !matches!(job.status.as_str(), Some("running" | "waiting_approval")) {
            return Err(Failure::public(409, "This task is not running."));
        }
        Ok(())
    }
    pub fn finish(
        &self,
        account: &Account,
        project: &str,
        id: &str,
        result: JValue,
    ) -> Result<JValue, Failure> {
        self.running(account, project, id)?;
        if !result.is_nullish() && wtf16::stringify(&result).is_some_and(|s| s.len() > 64 * 1024) {
            return Err(Failure::public(413, "That result is too large to record."));
        }
        let state = self.state.lock().map_err(|_| Failure::unavailable())?;
        let live = state
            .live
            .get(&(account.user().into(), id.into()))
            .and_then(|l| l.sender.as_ref())
            .ok_or_else(|| Failure::public(409, "This task is not open for actions."))?;
        live.send(Item::Finish(result))
            .map_err(|_| Failure::public(409, "This task is not open for actions."))?;
        Ok(okay())
    }
    pub fn cancel(&self, account: &Account, project: &str, id: &str) -> Result<JValue, Failure> {
        let state = self.state.lock().map_err(|_| Failure::unavailable())?;
        let started = state
            .live
            .get(&(account.user().into(), id.into()))
            .is_some_and(|l| l.started);
        let job = owned(
            account
                .cancel_snapshot(Surface::Browser, project, id, started, self.clock.now())
                .map_err(|_| Failure::unavailable())?,
        )?;
        Ok(task_journals::browser_view(&job, &JValue::Null))
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
                .owned(Surface::Browser, project, id)
                .map_err(|_| Failure::unavailable())?,
        )?;
        account
            .control
            .decide(
                &Scope {
                    user: account.user().into(),
                    project: project.into(),
                    surface: Surface::Browser,
                },
                id,
                approval,
                decision,
            )
            .map_err(|e| match e {
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
        Ok(okay())
    }
    pub async fn drain(&self) {
        let (contexts, workers) = {
            let Ok(mut state) = self.state.lock() else {
                return;
            };
            state.draining = true;
            let contexts=state.live.values_mut().map(|l|{l.restart=Some(if matches!(l.context.job(),Ok(job)if job.status.as_str()==Some("waiting_approval")){"The server restarted while waiting for approval; start again to be asked again."}else{"The server restarted before this finished."});l.context.clone()}).collect::<Vec<_>>();
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

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;
    use crate::{
        browser_action::{Primitive, Reply},
        context::SystemClock,
        facts::CodeConfig,
    };
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use tokio::sync::{Notify, Semaphore};
    const USER: &str = "11111111-1111-4111-8111-111111111111";

    struct Shared {
        opens: AtomicUsize,
        effects: AtomicUsize,
        hang: AtomicBool,
        block_close: AtomicBool,
        open_entered: Notify,
        close_entered: Notify,
        dispatch_entered: Notify,
        open_gate: Semaphore,
        close_gate: Semaphore,
        dispatch_gate: Semaphore,
    }
    impl Shared {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                opens: AtomicUsize::new(0),
                effects: AtomicUsize::new(0),
                hang: AtomicBool::new(false),
                block_close: AtomicBool::new(false),
                open_entered: Notify::new(),
                close_entered: Notify::new(),
                dispatch_entered: Notify::new(),
                open_gate: Semaphore::new(16),
                close_gate: Semaphore::new(0),
                dispatch_gate: Semaphore::new(0),
            })
        }
    }
    struct Factory(Arc<Shared>);
    struct Engine {
        shared: Arc<Shared>,
        context: Arc<JournalContext>,
    }
    impl Engines for Factory {
        fn open(&self, context: Arc<JournalContext>, _: Arc<PageState>, _: Option<Proxy>) -> Open {
            let shared = self.0.clone();
            Box::pin(async move {
                shared.opens.fetch_add(1, Ordering::SeqCst);
                shared.open_entered.notify_one();
                let _permit = shared.open_gate.acquire().await.unwrap();
                Ok(Arc::new(Engine {
                    shared: shared.clone(),
                    context,
                }) as Arc<dyn BrowserEngine>)
            })
        }
    }
    fn element() -> JValue {
        object(vec![
            ("tag", string("button")),
            ("type", string("submit")),
            ("name", string("Send")),
            ("text", string("Send")),
            ("inForm", JValue::Bool(true)),
        ])
    }
    impl BrowserEngine for Engine {
        fn execute(&self, command: Primitive) -> Reply {
            let shared = self.shared.clone();
            let context = self.context.clone();
            Box::pin(async move {
                let reply = match command {
                    Primitive::Current => object(vec![
                        ("page", string("page")),
                        ("origin", string("https://synthetic.invalid")),
                        ("url", string("https://synthetic.invalid/")),
                    ]),
                    Primitive::Inspect { .. } => {
                        object(vec![("element", string("button")), ("facts", element())])
                    }
                    Primitive::Recheck { .. } => object(vec![
                        ("samePage", JValue::Bool(true)),
                        ("origin", string("https://synthetic.invalid")),
                        ("connected", JValue::Bool(true)),
                        ("focused", JValue::Bool(true)),
                        ("facts", element()),
                    ]),
                    Primitive::Screenshot { .. } => object(vec![("png", string("cG5n"))]),
                    Primitive::Extract { .. } => object(vec![("text", string("body text"))]),
                    Primitive::Dispatch { .. } => {
                        shared.dispatch_entered.notify_one();
                        if shared.hang.load(Ordering::SeqCst) {
                            let _permit = shared.dispatch_gate.acquire().await.unwrap();
                        }
                        if context.active().is_ok() {
                            shared.effects.fetch_add(1, Ordering::SeqCst);
                        }
                        object(vec![])
                    }
                    _ => object(vec![]),
                };
                Ok(reply)
            })
        }
        fn close(&self) -> Reply {
            let shared = self.shared.clone();
            let context = self.context.clone();
            Box::pin(async move {
                shared.close_entered.notify_one();
                if shared.block_close.load(Ordering::SeqCst) {
                    let _permit = shared.close_gate.acquire().await.unwrap();
                }
                shared.dispatch_gate.add_permits(1);
                drop(context);
                Ok(object(vec![("confirmed", JValue::Bool(true))]))
            })
        }
    }
    struct Issuer {
        revoked: Notify,
        revokes: AtomicUsize,
        forgot: AtomicUsize,
    }
    impl Egress for Issuer {
        fn network(&self) -> Result<Network, crate::egress::Unavailable> {
            Ok(Network::Ready)
        }
        fn endpoint(&self) -> &str {
            "synthetic.invalid:8040"
        }
        fn grant<'a>(&'a self, id: &'a str, _: &'a [String]) -> crate::egress::Reply<'a, Grant> {
            Box::pin(async move {
                Ok(Grant::new(
                    id.into(),
                    "handle".into(),
                    "synthetic-token".into(),
                ))
            })
        }
        fn revoke<'a>(&'a self, _: &'a Grant) -> crate::egress::Reply<'a, crate::egress::Cleanup> {
            Box::pin(async move {
                self.revokes.fetch_add(1, Ordering::SeqCst);
                self.revoked.notify_one();
                Ok(crate::egress::Cleanup {
                    confirmed: true,
                    report: object(vec![
                        ("available", JValue::Bool(true)),
                        ("final", JValue::Bool(true)),
                        ("snapshotId", string("snapshot")),
                    ]),
                })
            })
        }
        fn forget<'a>(&'a self, _: &'a Grant, _: &'a str) -> crate::egress::Reply<'a, bool> {
            Box::pin(async move {
                self.forgot.fetch_add(1, Ordering::SeqCst);
                Ok(true)
            })
        }
    }
    fn setup() -> (
        tempfile::TempDir,
        Arc<Account>,
        Arc<Browser>,
        Arc<Shared>,
        Arc<Issuer>,
    ) {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("users").join(USER)).unwrap();
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
        let shared = Shared::new();
        let issuer = Arc::new(Issuer {
            revoked: Notify::new(),
            revokes: AtomicUsize::new(0),
            forgot: AtomicUsize::new(0),
        });
        let mut actor = Browser::new(
            Arc::new(Factory(shared.clone())),
            Some(issuer.clone()),
            Arc::new(SystemClock),
            Duration::from_millis(80),
            Duration::from_secs(30),
            false,
        );
        actor.close_timeout = Duration::from_millis(20);
        (root, account, Arc::new(actor), shared, issuer)
    }
    fn body() -> JValue {
        object(vec![(
            "domains",
            JValue::Arr(vec![string("synthetic.invalid")]),
        )])
    }
    fn job(account: &Account, id: &str) -> Box<task_journals::Job> {
        owned(account.owned(Surface::Browser, "project", id).unwrap()).unwrap()
    }
    async fn terminal(account: &Account, id: &str) {
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if matches!(
                    job(account, id).status.as_str(),
                    Some("completed" | "cancelled" | "failed" | "interrupted")
                ) {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
    }
    #[tokio::test]
    async fn sequential_finish_recovery_and_tenant_ownership() {
        let (_root, account, actor, shared, issuer) = setup();
        let started = actor
            .start(account.clone(), "project".into(), body())
            .await
            .unwrap();
        let id = started.get("taskId").as_str().unwrap();
        assert_eq!(job(&account, id).status.as_str(), Some("running"));
        assert_eq!(
            actor
                .start(account.clone(), "project".into(), body())
                .await
                .err()
                .unwrap()
                .status,
            Some(409)
        );
        let read = actor
            .act(
                &account,
                "project",
                id,
                object(vec![("type", string("extract"))]),
            )
            .unwrap()
            .await
            .unwrap()
            .unwrap();
        assert_eq!(read.get("status").as_str(), Some("done"));
        assert_eq!(
            actor
                .finish(
                    &account,
                    "project",
                    id,
                    object(vec![("pages", JValue::Num(1.0))])
                )
                .unwrap(),
            okay()
        );
        terminal(&account, id).await;
        assert_eq!(job(&account, id).status.as_str(), Some("completed"));
        assert_eq!(job(&account, id).result.get("pages"), &JValue::Num(1.0));
        assert_eq!(issuer.revokes.load(Ordering::SeqCst), 1);
        assert_eq!(issuer.forgot.load(Ordering::SeqCst), 1);
        shared.block_close.store(false, Ordering::SeqCst);
        actor.drain().await;
    }
    #[tokio::test]
    async fn approve_all_is_one_browser_action_and_timeout_is_a_recorded_deny() {
        let (_root, account, actor, shared, _) = setup();
        let started = actor
            .start(account.clone(), "project".into(), body())
            .await
            .unwrap();
        let id = started.get("taskId").as_str().unwrap();
        let action = object(vec![("type", string("click")), ("selector", string("#go"))]);
        for decision in [Some("approve_all"), Some("deny"), None] {
            let waiting = actor.act(&account, "project", id, action.clone()).unwrap();
            let card = tokio::time::timeout(Duration::from_secs(1), async {
                loop {
                    let facts = actor.facts(&account).unwrap();
                    if let Some(id) = facts.get("approvals").get(id).get("id").as_str() {
                        break id.to_owned();
                    }
                    tokio::task::yield_now().await;
                }
            })
            .await
            .unwrap();
            if let Some(decision) = decision {
                actor
                    .decide(&account, "project", id, decision, &card)
                    .unwrap();
            }
            let result = waiting.await.unwrap().unwrap();
            assert_eq!(
                result.get("status").as_str(),
                Some(if decision == Some("approve_all") {
                    "done"
                } else {
                    "blocked"
                })
            );
            if decision.is_none() {
                assert_eq!(
                    actor
                        .decide(&account, "project", id, "approve", &card)
                        .err()
                        .unwrap()
                        .status,
                    Some(409)
                );
            }
        }
        assert_eq!(shared.effects.load(Ordering::SeqCst), 1);
        actor.finish(&account, "project", id, JValue::Null).unwrap();
        terminal(&account, id).await;
        actor.drain().await;
    }
    #[tokio::test]
    async fn cancellation_revokes_independently_and_keeps_reservation_through_real_close() {
        let (_root, account, actor, shared, issuer) = setup();
        shared.hang.store(true, Ordering::SeqCst);
        shared.block_close.store(true, Ordering::SeqCst);
        let started = actor
            .start(account.clone(), "project".into(), body())
            .await
            .unwrap();
        let id = started.get("taskId").as_str().unwrap();
        let waiting = actor
            .act(
                &account,
                "project",
                id,
                object(vec![("type", string("wait"))]),
            )
            .unwrap();
        shared.dispatch_entered.notified().await;
        let queued = actor
            .act(
                &account,
                "project",
                id,
                object(vec![("type", string("extract"))]),
            )
            .unwrap();
        actor.cancel(&account, "project", id).unwrap();
        shared.close_entered.notified().await;
        issuer.revoked.notified().await;
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(1), waiting)
                .await
                .unwrap()
                .unwrap()
                .err()
                .unwrap()
                .status,
            Some(499)
        );
        assert_eq!(queued.await.unwrap().err().unwrap().status, Some(409));
        assert_eq!(issuer.revokes.load(Ordering::SeqCst), 1);
        assert_eq!(
            actor
                .start(account.clone(), "project".into(), body())
                .await
                .err()
                .unwrap()
                .status,
            Some(409)
        );
        assert_eq!(shared.effects.load(Ordering::SeqCst), 0);
        shared.close_gate.add_permits(1);
        terminal(&account, id).await;
        actor.drain().await;
        assert_eq!(job(&account, id).status.as_str(), Some("cancelled"));
        assert_eq!(shared.effects.load(Ordering::SeqCst), 0);
    }
}
