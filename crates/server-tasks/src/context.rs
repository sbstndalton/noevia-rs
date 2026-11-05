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
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
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
    guard: ExecutionGuard,
    clock: Arc<dyn Clock>,
    timeout: Duration,
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
        Ok(Self {
            account,
            guard,
            clock,
            timeout,
        })
    }
    pub fn scope(&self) -> &Scope {
        &self.guard.execution.scope
    }
    pub fn task(&self) -> &str {
        &self.guard.task
    }
    pub fn cancellation(&self) -> tokio::sync::watch::Receiver<bool> {
        self.guard.cancellation()
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
        if *self.guard.execution.cancel.borrow() {
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
        Box::pin(async move {
            let answer = match wait {
                Ok(wait) => wait.wait().await,
                Err(_) => Decision::Aborted,
            };
            if account.fence().is_err() {
                Decision::Aborted
            } else {
                answer
            }
        })
    }
    fn now(&self) -> i64 {
        self.clock.now()
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
        account.control.cancel(&scope, TASK).unwrap();
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
}
