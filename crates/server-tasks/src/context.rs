//! The native journal/approval context retained by an executor actor and all its callbacks.
//! Its execution guard outlives an HTTP future, cancellation, and pending sidecar teardown.
use crate::{
    account::Account,
    acp::RpcError,
    session::{Answer, Context},
    Decision, ExecutionGuard, Scope,
};
use js_json::JValue;
use std::{
    sync::{Arc, Mutex},
    time::{Duration, SystemTime, UNIX_EPOCH},
};
pub trait Permit: Send + Sync {
    fn valid(&self) -> bool;
}
pub trait Clock: Send + Sync {
    fn now(&self) -> i64;
}
pub struct SystemClock;
impl Clock for SystemClock {
    fn now(&self) -> i64 {
        match SystemTime::now().duration_since(UNIX_EPOCH) {
            Ok(d) => d.as_millis().min(i64::MAX as u128) as i64,
            Err(e) => -(e.duration().as_millis().min(i64::MAX as u128) as i64),
        }
    }
}
pub struct JournalContext {
    account: Arc<Account>,
    guard: Arc<ExecutionGuard>,
    clock: Arc<dyn Clock>,
    timeout: Duration,
    permit: Arc<Mutex<Option<Arc<dyn Permit>>>>,
    stage_cancel: Option<tokio::sync::watch::Receiver<bool>>,
}
/// Dropping the stage timer stops its operation; cleanup still owns the scoped context.
pub struct StageStop(tokio::sync::watch::Sender<bool>);
impl StageStop {
    pub fn cancel(&self) {
        self.0.send_replace(true);
    }
}
impl Drop for StageStop {
    fn drop(&mut self) {
        self.cancel();
    }
}
/// Cancellation bridge for bounded synchronous Git/workspace work. The task lease stays owned
/// by the bridge until its receiver settles; the synchronous caller retains the flag through Git reap.
pub struct BlockingCancellation {
    flag: Arc<std::sync::atomic::AtomicBool>,
    stop: Option<tokio::sync::oneshot::Sender<()>>,
}
impl BlockingCancellation {
    pub fn flag(&self) -> &std::sync::atomic::AtomicBool {
        &self.flag
    }
}
impl Drop for BlockingCancellation {
    fn drop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
    }
}
fn unavailable() -> RpcError {
    RpcError {
        code: -32603,
        message: "Task status is unavailable right now. Try again shortly.".into(),
        data: None,
    }
}
impl JournalContext {
    pub fn new(
        account: Arc<Account>,
        guard: ExecutionGuard,
        clock: Arc<dyn Clock>,
        timeout: Duration,
    ) -> Result<Self, RpcError> {
        if account.user() != guard.execution.scope.user
            || !Arc::ptr_eq(&account.control.state, &guard.control.state)
        {
            return Err(unavailable());
        }
        account.fence().map_err(|_| unavailable())?;
        if !matches!(
            account.owned(
                guard.execution.scope.surface,
                &guard.execution.scope.project,
                &guard.task
            ),
            Ok(server_projects::task_journals::Found::Task(_))
        ) {
            return Err(unavailable());
        }
        Ok(Self {
            account,
            guard: Arc::new(guard),
            clock,
            timeout,
            permit: Arc::new(Mutex::new(None)),
            stage_cancel: None,
        })
    }
    pub(crate) fn bind_permit(&self, permit: Arc<dyn Permit>) -> Result<(), RpcError> {
        let mut held = self.permit.lock().map_err(|_| unavailable())?;
        if held.is_some() || !permit.valid() {
            return Err(unavailable());
        }
        *held = Some(permit);
        Ok(())
    }
    pub(crate) fn account(&self) -> &Arc<Account> {
        &self.account
    }
    pub fn scope(&self) -> &Scope {
        &self.guard.execution.scope
    }
    pub fn task(&self) -> &str {
        &self.guard.task
    }
    pub fn cancellation(&self) -> tokio::sync::watch::Receiver<bool> {
        self.stage_cancel
            .clone()
            .unwrap_or_else(|| self.guard.cancellation())
    }
    /// A stage deadline cancels the current engine operation, while retaining the account,
    /// inference permit and execution reservation until its callbacks and cleanup settle.
    pub fn stage(self: &Arc<Self>) -> (Arc<Self>, StageStop) {
        let (stop, cancel) = tokio::sync::watch::channel(false);
        let child = Arc::new(Self {
            account: self.account.clone(),
            guard: self.guard.clone(),
            clock: self.clock.clone(),
            timeout: self.timeout,
            permit: self.permit.clone(),
            stage_cancel: Some(cancel.clone()),
        });
        let parent = self.clone();
        let forward = stop.clone();
        let mut parent_cancel = self.cancellation();
        let mut child_cancel = cancel;
        tokio::spawn(async move {
            if *parent_cancel.borrow() {
                forward.send_replace(true);
                return;
            }
            tokio::select! {biased;_=parent_cancel.changed()=>{forward.send_replace(true);},_=child_cancel.changed()=>{}}
            drop(parent);
        });
        (child, StageStop(stop))
    }
    pub fn blocking_cancellation(self: &Arc<Self>) -> BlockingCancellation {
        let flag = Arc::new(std::sync::atomic::AtomicBool::new(self.active().is_err()));
        let watched = flag.clone();
        let context = self.clone();
        let mut cancel = self.cancellation();
        let (stop, stopped) = tokio::sync::oneshot::channel();
        match tokio::runtime::Handle::try_current() {
            Ok(runtime) => {
                runtime.spawn(async move{if *cancel.borrow(){watched.store(true,std::sync::atomic::Ordering::Release);return;}tokio::select!{biased;_=cancel.changed()=>watched.store(true,std::sync::atomic::Ordering::Release),_=stopped=>{}}drop(context);});
            }
            Err(_) => flag.store(true, std::sync::atomic::Ordering::Release),
        }
        BlockingCancellation {
            flag,
            stop: Some(stop),
        }
    }
    pub(crate) fn job(&self) -> Result<server_projects::task_journals::Job, crate::code::Failure> {
        match self
            .account
            .owned(self.scope().surface, &self.scope().project, self.task())
        {
            Ok(server_projects::task_journals::Found::Task(job)) => Ok(*job),
            _ => Err(crate::code::Failure::unavailable()),
        }
    }
    pub(crate) fn authority(&self, kind: &str, data: JValue) -> Result<(), crate::code::Failure> {
        if self.scope().surface != server_projects::task_journals::Surface::Code {
            return Err(crate::code::Failure::unavailable());
        }
        match self
            .account
            .authority(self.task(), kind, data, self.clock.now())
        {
            Ok(_) => Ok(()),
            Err(crate::account::Error::Journal(
                server_projects::task_journals::writer::Error::Authority { status, message },
            )) => Err(crate::code::Failure::public(status, message)),
            Err(_) => Err(crate::code::Failure::unavailable()),
        }
    }
    pub fn sync(&self) -> Result<(), RpcError> {
        self.account
            .sync(self.scope().surface, self.task())
            .map_err(|_| unavailable())
    }
}
impl Context for JournalContext {
    fn active(&self) -> Result<(), RpcError> {
        self.account.fence().map_err(|_| unavailable())?;
        if self
            .permit
            .lock()
            .map_err(|_| unavailable())?
            .as_ref()
            .is_some_and(|permit| !permit.valid())
        {
            let _ = self.account.control.cancel(self.scope(), self.task());
            return Err(unavailable());
        }
        if *self.guard.execution.cancel.borrow()
            || self
                .stage_cancel
                .as_ref()
                .is_some_and(|cancel| *cancel.borrow())
        {
            return Err(RpcError {
                code: -32603,
                message: "The task was cancelled.".into(),
                data: None,
            });
        }
        Ok(())
    }
    fn event(&self, kind: &str, data: JValue) -> Result<(), RpcError> {
        self.account
            .append(
                self.scope().surface,
                self.task(),
                kind,
                data,
                self.clock.now(),
            )
            .map(|_| ())
            .map_err(|_| unavailable())
    }
    fn approval(&self, request: JValue) -> Answer {
        if self.account.fence().is_err() {
            return Box::pin(async { Decision::Aborted });
        }
        let wait = self
            .account
            .control
            .ask(self.scope(), self.task(), request, self.timeout);
        let account = self.account.clone();
        let mut cancelled = self.cancellation();
        Box::pin(async move {
            let answer = match wait {
                Ok(wait) => {
                    if *cancelled.borrow() {
                        Decision::Aborted
                    } else {
                        tokio::select! {biased;_=cancelled.changed()=>Decision::Aborted,answer=wait.wait()=>answer}
                    }
                }
                Err(_) => Decision::Aborted,
            };
            if account.fence().is_err() {
                Decision::Aborted
            } else {
                answer
            }
        })
    }
    fn request_approval(&self, request: JValue) -> Result<Answer, RpcError> {
        let _held = self.account.mutation().map_err(|_| unavailable())?;
        self.account
            .append_unlocked(
                self.scope().surface,
                self.task(),
                "approval.requested",
                request.clone(),
                self.clock.now(),
            )
            .map_err(|_| unavailable())?;
        Ok(self.approval(request))
    }
    fn now(&self) -> i64 {
        self.clock.now()
    }
    fn end_approvals(&self) {
        let Ok(_held) = self.account.mutation() else {
            return;
        };
        let _ = self
            .account
            .control
            .end_approvals(self.scope(), self.task());
    }
}
#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;
    use crate::{acp::Handlers, facts::CodeConfig, session::Session};
    use server_projects::task_journals::Surface;
    const USER: &str = "11111111-1111-4111-8111-111111111111";
    const TASK: &str = "22222222-2222-4222-8222-222222222222";
    fn text(s: &str) -> JValue {
        JValue::Str(s.into())
    }
    #[tokio::test]
    async fn stage_timeout_stops_approval_but_retains_reservation_and_parent() {
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
        let scope = Scope {
            user: USER.into(),
            project: "project".into(),
            surface: Surface::Code,
        };
        let guard = account.control.begin(scope.clone(), TASK.into()).unwrap();
        account
            .create(Surface::Code, TASK, "project", vec![], 1)
            .unwrap();
        let parent = Arc::new(
            JournalContext::new(
                account.clone(),
                guard,
                Arc::new(SystemClock),
                Duration::from_secs(60),
            )
            .unwrap(),
        );
        let (stage, stop) = parent.stage();
        let answer = stage
            .request_approval(JValue::Obj(vec![("action".into(), text("review_code"))]))
            .unwrap();
        drop(stop);
        assert_eq!(answer.await, Decision::Aborted);
        assert!(stage.active().is_err());
        assert!(parent.active().is_ok());
        drop(parent);
        assert!(account
            .control
            .begin(scope.clone(), "successor".into())
            .is_err());
        let (nested, nested_stop) = stage.stage();
        tokio::time::timeout(Duration::from_secs(2), async {
            let mut cancel = nested.cancellation();
            while !*cancel.borrow() {
                cancel.changed().await.unwrap();
            }
        })
        .await
        .unwrap();
        drop(nested_stop);
        drop(nested);
        drop(stage);
        // The cancellation-forwarding tasks also own the reservation until they settle.
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if let Ok(guard) = account.control.begin(scope.clone(), "successor".into()) {
                    drop(guard);
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
    }
    #[tokio::test]
    async fn pending_callback_drop_and_cancel_keep_the_execution_reserved_until_teardown() {
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
        let scope = Scope {
            user: USER.into(),
            project: "project".into(),
            surface: Surface::Code,
        };
        let guard = account.control.begin(scope.clone(), TASK.into()).unwrap();
        account
            .create(Surface::Code, TASK, "project", vec![text("edit_file")], 1)
            .unwrap();
        let context = Arc::new(
            JournalContext::new(
                account.clone(),
                guard,
                Arc::new(SystemClock),
                Duration::from_secs(30),
            )
            .unwrap(),
        );
        context.event("job.started", JValue::Obj(vec![])).unwrap();
        let session = Arc::new(Session::new(
            TASK.into(),
            root.path().into(),
            vec!["edit_file".into()],
            vec![],
            context.clone(),
        ));
        let params=js_json::wtf16::parse(r#"{"toolCall":{"kind":"edit","rawInput":{"path":"source.txt"},"locations":[{"path":"source.txt"}]},"options":[{"optionId":"allow","kind":"allow_once"},{"optionId":"reject","kind":"reject_once"}]}"#).unwrap();
        let dropped = session
            .clone()
            .request("session/request_permission", params.clone());
        assert_eq!(account.control.cards(&scope).unwrap().len(), 1);
        drop(dropped);
        assert!(account.control.cards(&scope).unwrap().is_empty());
        let pending = session
            .clone()
            .request("session/request_permission", params);
        assert_eq!(account.control.cards(&scope).unwrap().len(), 1);
        let server_projects::task_journals::Found::Task(snapshot) = account
            .cancel_snapshot(Surface::Code, "project", TASK, true, 2)
            .unwrap()
        else {
            panic!("task missing")
        };
        assert_eq!(snapshot.status.as_str(), Some("waiting_approval"));
        assert!(account.control.cards(&scope).unwrap().is_empty());
        session.end();
        let outcome = pending.await.unwrap();
        assert_eq!(outcome.get("optionId").as_str(), Some("reject"));
        assert!(account.control.cards(&scope).unwrap().is_empty());
        assert!(context.active().is_err());
        assert!(account
            .control
            .begin(scope.clone(), "33333333-3333-4333-8333-333333333333".into())
            .is_err());
        drop(session);
        drop(context);
        assert!(account
            .control
            .begin(scope, "33333333-3333-4333-8333-333333333333".into())
            .is_ok());
    }
    #[tokio::test]
    async fn engine_stop_then_checkpoint_then_approval_drain_precedes_the_terminal_event() {
        use tokio::{
            io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
            net::{
                tcp::{OwnedReadHalf, OwnedWriteHalf},
                TcpListener,
            },
        };
        async fn read(reader: &mut BufReader<OwnedReadHalf>) -> JValue {
            let mut line = String::new();
            assert!(reader.read_line(&mut line).await.unwrap() > 0);
            js_json::wtf16::parse(&line).unwrap()
        }
        async fn send(writer: &mut OwnedWriteHalf, value: JValue) {
            writer
                .write_all((js_json::wtf16::stringify(&value).unwrap() + "\n").as_bytes())
                .await
                .unwrap();
        }
        fn object(rows: Vec<(&str, JValue)>) -> JValue {
            JValue::Obj(rows.into_iter().map(|(k, v)| (k.into(), v)).collect())
        }
        fn reply(request: &JValue, result: JValue) -> JValue {
            object(vec![
                ("jsonrpc", text("2.0")),
                ("id", request.get("id").clone()),
                ("result", result),
            ])
        }
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
        let scope = Scope {
            user: USER.into(),
            project: "project".into(),
            surface: Surface::Code,
        };
        let guard = account.control.begin(scope.clone(), TASK.into()).unwrap();
        account
            .create(Surface::Code, TASK, "project", vec![text("edit_file")], 1)
            .unwrap();
        let context = Arc::new(
            JournalContext::new(
                account.clone(),
                guard,
                Arc::new(SystemClock),
                Duration::from_secs(30),
            )
            .unwrap(),
        );
        context.event("job.started", object(vec![])).unwrap();
        let session = Arc::new(Session::new(
            TASK.into(),
            root.path().into(),
            vec!["edit_file".into()],
            vec![],
            context.clone(),
        ));
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = listener.local_addr().unwrap().to_string();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let (read_half, mut writer) = stream.into_split();
            let mut reader = BufReader::new(read_half);
            assert_eq!(
                read(&mut reader).await.get("noevia").as_str(),
                Some("start")
            );
            let init = read(&mut reader).await;
            send(
                &mut writer,
                reply(&init, object(vec![("protocolVersion", JValue::Num(1.0))])),
            )
            .await;
            let new = read(&mut reader).await;
            send(
                &mut writer,
                reply(&new, object(vec![("sessionId", text("session"))])),
            )
            .await;
            let prompt = read(&mut reader).await;
            let permission=js_json::wtf16::parse(r#"{"jsonrpc":"2.0","id":99,"method":"session/request_permission","params":{"toolCall":{"kind":"edit","rawInput":{"path":"source.txt"},"locations":[{"path":"source.txt"}]},"options":[{"optionId":"allow","kind":"allow_once"},{"optionId":"reject","kind":"reject_once"}]}}"#).unwrap();
            send(&mut writer, permission).await;
            send(
                &mut writer,
                reply(&prompt, object(vec![("stopReason", text("end_turn"))])),
            )
            .await;
            let mut line = String::new();
            assert_eq!(
                tokio::time::timeout(Duration::from_secs(5), reader.read_line(&mut line))
                    .await
                    .unwrap()
                    .unwrap(),
                0
            );
        });
        let mut client = crate::acp::Client::connect(
            crate::acp::Options {
                endpoint,
                cwd: root.path().to_string_lossy().into_owned(),
                env: object(vec![]),
                session_meta: None,
                model: None,
                handshake_timeout: Duration::from_secs(5),
            },
            session.clone(),
            context.cancellation(),
        )
        .await
        .unwrap();
        client.turn("synthetic").await.unwrap();
        assert_eq!(account.control.cards(&scope).unwrap().len(), 1);
        context
            .event(
                "checkpoint.created",
                object(vec![("task", text("synthetic"))]),
            )
            .unwrap();
        client.close().await;
        assert!(account.control.cards(&scope).unwrap().is_empty());
        context
            .event("job.completed", object(vec![("result", JValue::Null)]))
            .unwrap();
        let rows = std::fs::read_to_string(
            root.path()
                .join("users")
                .join(USER)
                .join("jobs")
                .join(format!("{TASK}.jsonl")),
        )
        .unwrap()
        .lines()
        .map(|line| js_json::wtf16::parse(line).unwrap())
        .collect::<Vec<_>>();
        let types = rows
            .iter()
            .map(|row| row.get("type").as_str().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(
            types,
            vec![
                "job.created",
                "job.started",
                "approval.requested",
                "checkpoint.created",
                "approval.decided",
                "job.completed"
            ]
        );
        assert_eq!(
            rows.iter()
                .find(|row| row.get("type").as_str() == Some("approval.decided"))
                .unwrap()
                .get("data")
                .get("decision")
                .as_str(),
            Some("aborted")
        );
        assert_eq!(
            session.summary().unwrap().get("refused").as_num(),
            Some(1.0)
        );
        server.await.unwrap();
    }
}
