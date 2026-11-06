//! One native task account owner per process/account. Recovery runs before first publication;
//! replacing an account directory never silently creates a fresh owner for its old executions.
use crate::{account::Account, context::Clock, facts::CodeConfig};
use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{Arc, Mutex},
};
#[derive(Clone, Copy)]
pub enum Ownership {
    Code,
    Both,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Unavailable;
pub struct Accounts {
    root: PathBuf,
    config: CodeConfig,
    epoch: String,
    ownership: Ownership,
    clock: Arc<dyn Clock>,
    accounts: Mutex<HashMap<String, Result<Arc<Account>, Unavailable>>>,
}
impl Accounts {
    pub fn new(
        root: PathBuf,
        config: CodeConfig,
        epoch: String,
        ownership: Ownership,
        clock: Arc<dyn Clock>,
    ) -> Self {
        Self {
            root,
            config,
            epoch,
            ownership,
            clock,
            accounts: Mutex::new(HashMap::new()),
        }
    }
    /// Blocking initialization only; callers use their blocking pool. The mutex crosses no await.
    pub fn get(&self, user: &str) -> Result<Arc<Account>, Unavailable> {
        let mut accounts = self.accounts.lock().map_err(|_| Unavailable)?;
        let account = accounts
            .entry(user.into())
            .or_insert_with(|| {
                let opened = match self.ownership {
                    Ownership::Code => Account::open_code(
                        &self.root,
                        user,
                        &self.config,
                        self.epoch.clone(),
                        self.clock.now(),
                    ),
                    Ownership::Both => Account::open(
                        &self.root,
                        user,
                        &self.config,
                        self.epoch.clone(),
                        self.clock.now(),
                    ),
                };
                opened.map(Arc::new).map_err(|_| Unavailable)
            })
            .clone()?;
        account.fence().map_err(|_| Unavailable)?;
        Ok(account)
    }
}
#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;
    use crate::{context::SystemClock, session::Context, Scope};
    use server_projects::task_journals::Surface;
    const USER: &str = "11111111-1111-4111-8111-111111111111";
    const TASK: &str = "22222222-2222-4222-8222-222222222222";
    #[test]
    fn concurrent_get_recovers_once_and_never_recovers_its_own_live_task() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("users").join(USER)).unwrap();
        let registry = Arc::new(Accounts::new(
            root.path().into(),
            CodeConfig::from_env(|_| None),
            "epoch".into(),
            Ownership::Both,
            Arc::new(SystemClock),
        ));
        let account = registry.get(USER).unwrap();
        let scope = Scope {
            user: USER.into(),
            project: "project".into(),
            surface: Surface::Code,
        };
        let guard = account.control.begin(scope, TASK.into()).unwrap();
        account
            .create(Surface::Code, TASK, "project", vec![], 1)
            .unwrap();
        let context = crate::context::JournalContext::new(
            account.clone(),
            guard,
            Arc::new(SystemClock),
            std::time::Duration::from_secs(1),
        )
        .unwrap();
        context
            .event("job.started", js_json::JValue::Obj(vec![]))
            .unwrap();
        let workers = (0..12)
            .map(|_| {
                let registry = registry.clone();
                std::thread::spawn(move || registry.get(USER).unwrap())
            })
            .collect::<Vec<_>>();
        for worker in workers {
            assert!(Arc::ptr_eq(&account, &worker.join().unwrap()));
        }
        assert!(
            matches!(account.owned(Surface::Code,"project",TASK).unwrap(),server_projects::task_journals::Found::Task(job)if job.status.as_str()==Some("running"))
        );
        std::fs::rename(
            root.path().join("users").join(USER),
            root.path().join("retired"),
        )
        .unwrap();
        std::fs::create_dir(root.path().join("users").join(USER)).unwrap();
        assert!(registry.get(USER).is_err());
        assert!(context.active().is_err());
    }
}
