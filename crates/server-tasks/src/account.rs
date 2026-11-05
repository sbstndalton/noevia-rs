//! One account runtime per front incarnation. Keep this lease alive in every task actor, so a
//! replacement cannot recover a journal while its previous executor still owns the sandbox.
use crate::{facts::CodeConfig, workspace, Control};
use server_projects::{
    store,
    task_journals::{writer, Surface},
};
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
    root: PathBuf,
    lease: fs::File,
    identity: fs::Metadata,
    code: writer::Store,
    browser: writer::Store,
    pub control: Control,
    pub workspaces: workspace::Store,
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
        let browser =
            writer::Store::open(data_dir, user, Surface::Browser).map_err(Error::Journal)?;
        let workspaces =
            workspace::Store::open(data_dir, user, config, epoch).map_err(Error::Workspace)?;
        let account = Self {
            user: user.into(),
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
        account.browser.recover(now).map_err(Error::Journal)?;
        account.workspaces.recover(now).map_err(Error::Workspace)?;
        Ok(account)
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
            Surface::Browser => &self.browser,
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
        self.journal(surface)?
            .append(id, kind, data, now)
            .map_err(Error::Journal)
    }
    pub fn sync(&self, surface: Surface, id: &str) -> Result<(), Error> {
        self.journal(surface)?.sync(id).map_err(Error::Journal)
    }
}
#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;
    const USER: &str = "11111111-1111-4111-8111-111111111111";
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
        drop(account);
        let recovered = Account::open(root, USER, &config, "new".into(), 4).unwrap();
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
}
