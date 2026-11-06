//! One account runtime per front incarnation. Keep this lease alive in every task actor, so a
//! replacement cannot recover a journal while its previous executor still owns the sandbox.
use crate::{facts::CodeConfig, workspace, Control};
use server_projects::{
    store,
    task_journals::{writer, Surface},
};
use std::sync::{Mutex, MutexGuard};
use std::{
    fs,
    path::{Path, PathBuf},
};
#[derive(Debug)]
pub enum Error {
    Unavailable,
    Busy,
    Journal(writer::Error),
    Workspace(workspace::Error),
    Io(std::io::Error),
}
impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}
pub struct Account {
    user: String,
    data_dir: PathBuf,
    mutation: Mutex<()>,
    root: PathBuf,
    lease: fs::File,
    identity: fs::Metadata,
    code: writer::Store,
    browser: Option<writer::Store>,
    pub control: Control,
    pub workspaces: workspace::Store,
}
impl Drop for Account {
    fn drop(&mut self) {
        self.workspaces.retire();
        let _ = self.lease.unlock();
    }
}
impl Account {
    /// Run in the account registry's blocking initialization, once, before publishing the Arc.
    pub fn open(
        data_dir: &Path,
        user: &str,
        config: &CodeConfig,
        epoch: String,
        now: i64,
    ) -> Result<Self, Error> {
        Self::open_owned(data_dir, user, config, epoch, now, true)
    }
    /// Staged ownership: a code-only front must neither recover nor publish browser journals
    /// while the legacy browser actor is still live. Ownership broadens only with a new runtime.
    pub fn open_code(
        data_dir: &Path,
        user: &str,
        config: &CodeConfig,
        epoch: String,
        now: i64,
    ) -> Result<Self, Error> {
        Self::open_owned(data_dir, user, config, epoch, now, false)
    }
    fn open_owned(
        data_dir: &Path,
        user: &str,
        config: &CodeConfig,
        epoch: String,
        now: i64,
        browser_owned: bool,
    ) -> Result<Self, Error> {
        let root = store::user_dir(data_dir, user).map_err(|_| Error::Unavailable)?;
        for dir in [data_dir.join("users"), root.clone()] {
            if !fs::symlink_metadata(dir)?.file_type().is_dir() {
                return Err(Error::Unavailable);
            }
        }
        let identity = fs::metadata(&root)?;
        let mut options = fs::OpenOptions::new();
        options.read(true).write(true).create(true).truncate(false);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options
                .mode(0o600)
                .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
        }
        let lease = options.open(root.join("native-task-runtime.lock"))?;
        if !lease.metadata()?.is_file() {
            return Err(Error::Unavailable);
        }
        lease.try_lock().map_err(|_| Error::Busy)?;
        let code = writer::Store::open(data_dir, user, Surface::Code).map_err(Error::Journal)?;
        let browser = if browser_owned {
            Some(writer::Store::open(data_dir, user, Surface::Browser).map_err(Error::Journal)?)
        } else {
            None
        };
        let workspaces =
            workspace::Store::open(data_dir, user, config, epoch).map_err(Error::Workspace)?;
        let account = Self {
            user: user.into(),
            data_dir: data_dir.to_path_buf(),
            mutation: Mutex::new(()),
            root,
            lease,
            identity,
            code,
            browser,
            control: Control::default(),
            workspaces,
        };
        account.fence()?;
        account.code.recover(now).map_err(Error::Journal)?;
        if let Some(browser) = &account.browser {
            browser.recover(now).map_err(Error::Journal)?;
        }
        account.workspaces.recover(now).map_err(Error::Workspace)?;
        Ok(account)
    }
    pub(crate) fn mutation(&self) -> Result<MutexGuard<'_, ()>, Error> {
        self.mutation.lock().map_err(|_| Error::Unavailable)
    }
    fn owned_unlocked(
        &self,
        surface: Surface,
        project: &str,
        id: &str,
    ) -> Result<server_projects::task_journals::Found, Error> {
        self.journal(surface)?;
        server_projects::task_journals::get(&self.data_dir, &self.user, surface, project, id)
            .map_err(|_| Error::Unavailable)
    }
    pub fn owned(
        &self,
        surface: Surface,
        project: &str,
        id: &str,
    ) -> Result<server_projects::task_journals::Found, Error> {
        let _held = self.mutation()?;
        self.owned_unlocked(surface, project, id)
    }
    pub fn list(
        &self,
        surface: Surface,
        project: Option<&str>,
        active: bool,
    ) -> Result<Vec<server_projects::task_journals::Job>, Error> {
        let _held = self.mutation()?;
        self.journal(surface)?;
        server_projects::task_journals::list(&self.data_dir, &self.user, surface, project, active)
            .map_err(|_| Error::Unavailable)
    }
    pub fn cancel_snapshot(
        &self,
        surface: Surface,
        project: &str,
        id: &str,
        started: bool,
        now: i64,
    ) -> Result<server_projects::task_journals::Found, Error> {
        let _held = self.mutation()?;
        let found = self.owned_unlocked(surface, project, id)?;
        if let server_projects::task_journals::Found::Task(job) = &found {
            let scope = crate::Scope {
                user: self.user.clone(),
                project: project.into(),
                surface,
            };
            self.control
                .end_approvals(&scope, id)
                .map_err(|_| Error::Unavailable)?;
            if matches!(
                job.status.as_str(),
                Some("completed" | "failed" | "cancelled" | "interrupted")
            ) {
                return Ok(found);
            }
            let cancelled = self.control.cancel(&scope, id);
            if started && cancelled.is_err() {
                return Err(Error::Unavailable);
            }
            if !started {
                self.journal(surface)?
                    .append(id, "job.cancelled", js_json::JValue::Obj(vec![]), now)
                    .map_err(Error::Journal)?;
            }
            return self.owned_unlocked(surface, project, id);
        }
        Ok(found)
    }
    pub fn user(&self) -> &str {
        &self.user
    }
    /// Check before journal mutation and before publishing a decision to an engine. A deleted
    /// account or replaced lease is never silently reacquired by an old actor.
    pub fn fence(&self) -> Result<(), Error> {
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            let current = fs::symlink_metadata(&self.root)?;
            if !current.file_type().is_dir()
                || current.dev() != self.identity.dev()
                || current.ino() != self.identity.ino()
            {
                return Err(Error::Unavailable);
            }
            let path = fs::symlink_metadata(self.root.join("native-task-runtime.lock"))?;
            let held = self.lease.metadata()?;
            if !path.is_file()
                || path.dev() != held.dev()
                || path.ino() != held.ino()
                || held.nlink() != 1
            {
                return Err(Error::Unavailable);
            }
        }
        Ok(())
    }
    fn journal(&self, surface: Surface) -> Result<&writer::Store, Error> {
        self.fence()?;
        Ok(match surface {
            Surface::Code => &self.code,
            Surface::Browser => self.browser.as_ref().ok_or(Error::Unavailable)?,
        })
    }
    pub fn create(
        &self,
        surface: Surface,
        id: &str,
        project: &str,
        capabilities: Vec<js_json::JValue>,
        now: i64,
    ) -> Result<server_projects::task_journals::Job, Error> {
        let _held = self.mutation()?;
        self.journal(surface)?
            .create(id, project, capabilities, now)
            .map_err(Error::Journal)
    }
    pub fn append(
        &self,
        surface: Surface,
        id: &str,
        kind: &str,
        data: js_json::JValue,
        now: i64,
    ) -> Result<server_projects::task_journals::Job, Error> {
        let _held = self.mutation()?;
        self.append_unlocked(surface, id, kind, data, now)
    }
    pub(crate) fn append_unlocked(
        &self,
        surface: Surface,
        id: &str,
        kind: &str,
        data: js_json::JValue,
        now: i64,
    ) -> Result<server_projects::task_journals::Job, Error> {
        self.journal(surface)?
            .append(id, kind, data, now)
            .map_err(Error::Journal)
    }
    pub(crate) fn authority(
        &self,
        id: &str,
        kind: &str,
        data: js_json::JValue,
        now: i64,
    ) -> Result<server_projects::task_journals::Job, Error> {
        let _held = self.mutation()?;
        self.journal(Surface::Code)?
            .authority(id, kind, data, now)
            .map_err(Error::Journal)
    }
    pub fn sync(&self, surface: Surface, id: &str) -> Result<(), Error> {
        let _held = self.mutation()?;
        self.journal(surface)?.sync(id).map_err(Error::Journal)
    }
}
#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;
    const USER: &str = "11111111-1111-4111-8111-111111111111";
    #[test]
    fn code_only_ownership_never_recovers_or_mutates_legacy_browser_tasks() {
        use js_json::JValue;
        use server_projects::task_journals::{self, Found};
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("users").join(USER)).unwrap();
        let browser = "22222222-2222-4222-8222-222222222222";
        let code = "33333333-3333-4333-8333-333333333333";
        let legacy = writer::Store::open(root.path(), USER, Surface::Browser).unwrap();
        legacy.create(browser, "project", vec![], 1).unwrap();
        legacy
            .append(browser, "job.started", JValue::Obj(vec![]), 2)
            .unwrap();
        let old_code = writer::Store::open(root.path(), USER, Surface::Code).unwrap();
        old_code.create(code, "project", vec![], 1).unwrap();
        old_code
            .append(code, "job.started", JValue::Obj(vec![]), 2)
            .unwrap();
        let path = root
            .path()
            .join("users")
            .join(USER)
            .join("jobs")
            .join(format!("{browser}.jsonl"));
        let before = std::fs::read(&path).unwrap();
        let config = CodeConfig::from_env(|_| None);
        let account =
            Account::open_code(root.path(), USER, &config, "code-incarnation".into(), 3).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), before);
        assert!(
            matches!(account.owned(Surface::Code,"project",code).unwrap(),Found::Task(job) if job.status.as_str()==Some("interrupted"))
        );
        assert!(account.owned(Surface::Browser, "project", browser).is_err());
        assert!(account.list(Surface::Browser, None, true).is_err());
        assert!(account
            .append(
                Surface::Browser,
                browser,
                "job.cancelled",
                JValue::Obj(vec![]),
                4
            )
            .is_err());
        // The legacy owner continues its live job; a native Code account never steals it.
        legacy
            .append(
                browser,
                "progress",
                JValue::Obj(vec![("stage".into(), JValue::Str("running".into()))]),
                4,
            )
            .unwrap();
        drop(account);
        let both =
            Account::open(root.path(), USER, &config, "unified-incarnation".into(), 5).unwrap();
        assert!(
            matches!(both.owned(Surface::Browser,"project",browser).unwrap(),Found::Task(job) if job.status.as_str()==Some("interrupted"))
        );
        assert!(
            matches!(task_journals::get(root.path(),USER,Surface::Browser,"project",browser).unwrap(),Found::Task(job) if job.error.as_str()==Some("The server restarted before this finished."))
        );
    }
    #[test]
    fn recovery_waits_for_old_actor_lease_and_replaced_account_is_fenced() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let user = root.join("users").join(USER);
        fs::create_dir_all(&user).unwrap();
        let config = CodeConfig::from_env(|_| None);
        let account = Account::open(root, USER, &config, "old".into(), 1).unwrap();
        let task = "22222222-2222-4222-8222-222222222222";
        account
            .create(Surface::Code, task, "project", vec![], 2)
            .unwrap();
        assert!(matches!(
            Account::open(root, USER, &config, "new".into(), 3),
            Err(Error::Busy)
        ));
        let inherited = account.lease.try_clone().unwrap();
        drop(account);
        let recovered = Account::open(root, USER, &config, "new".into(), 4).unwrap();
        drop(inherited);
        let view = server_projects::task_journals::get(root, USER, Surface::Code, "project", task)
            .unwrap();
        let server_projects::task_journals::Found::Task(view) = view else {
            panic!("missing task")
        };
        assert_eq!(view.status.as_str(), Some("interrupted"));
        let lock = user.join("native-task-runtime.lock");
        fs::rename(&lock, user.join("old-lock")).unwrap();
        fs::write(lock, "").unwrap();
        assert!(recovered.fence().is_err());
        assert!(recovered.journal(Surface::Code).is_err());
    }
    #[test]
    fn simultaneous_task_events_are_serialized_without_busy_or_lost_writes() {
        use std::sync::{Arc, Barrier};
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
        let task = "22222222-2222-4222-8222-222222222222";
        account
            .create(Surface::Code, task, "project", vec![], 1)
            .unwrap();
        let barrier = Arc::new(Barrier::new(12));
        let mut workers = vec![];
        for worker in 0..12 {
            let account = account.clone();
            let barrier = barrier.clone();
            workers.push(std::thread::spawn(move || {
                barrier.wait();
                for n in 0..16 {
                    account
                        .append(
                            Surface::Code,
                            task,
                            "progress",
                            js_json::JValue::Obj(vec![(
                                "stage".into(),
                                js_json::JValue::Str(format!("{worker}:{n}")),
                            )]),
                            2,
                        )
                        .unwrap();
                }
            }));
        }
        for worker in workers {
            worker.join().unwrap();
        }
        let rows = std::fs::read_to_string(
            root.path()
                .join("users")
                .join(USER)
                .join("jobs")
                .join(format!("{task}.jsonl")),
        )
        .unwrap()
        .lines()
        .map(|line| js_json::wtf16::parse(line).unwrap())
        .collect::<Vec<_>>();
        assert_eq!(rows.len(), 193);
        let observed = rows
            .iter()
            .skip(1)
            .map(|row| row.get("data").get("stage").as_str().unwrap().to_owned())
            .collect::<std::collections::HashSet<_>>();
        assert_eq!(observed.len(), 192);
        for (index, row) in rows.iter().enumerate() {
            assert_eq!(row.get("seq").as_num(), Some((index + 1) as f64));
        }
    }
}
