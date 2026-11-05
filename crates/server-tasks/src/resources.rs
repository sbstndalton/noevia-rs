//! A claimed code workspace and its optional network grant. The actor keeps this handle through
//! teardown; dropping any claiming/releasing future transfers cleanup to a native worker.
use crate::{
    account::Account,
    context::JournalContext,
    egress::{self, Egress, Grant},
    pin,
    session::Context,
    start::Request,
    workspace,
};
use js_json::JValue;
use std::sync::{atomic::AtomicBool, Arc};
#[derive(Debug)]
pub enum Error {
    Workspace(workspace::Error),
    Grant(egress::Unavailable),
    Cancelled,
    Unavailable,
}
#[derive(Clone)]
pub struct Released {
    pub workspace: Option<JValue>,
    pub network: Option<JValue>,
    pub confirmed: bool,
}
pub struct Resources {
    pub workspace: JValue,
    context: Arc<JournalContext>,
    account: Arc<Account>,
    egress: Option<Arc<dyn Egress>>,
    grant: Option<Grant>,
    revoked: Option<egress::Cleanup>,
    network: Option<JValue>,
    reported: bool,
    acknowledged: bool,
    released: Option<Released>,
    armed: bool,
    runtime: tokio::runtime::Handle,
}
fn object(rows: Vec<(&str, JValue)>) -> JValue {
    JValue::Obj(rows.into_iter().map(|(k, v)| (k.into(), v)).collect())
}
fn text(s: &str) -> JValue {
    JValue::Str(s.into())
}
fn set(value: &mut JValue, key: &str, item: JValue) {
    if let JValue::Obj(rows) = value {
        if let Some(row) = rows.iter_mut().find(|(k, _)| k == key) {
            row.1 = item;
        } else {
            rows.push((key.into(), item));
        }
    }
}
impl Resources {
    pub async fn claim(
        context: Arc<JournalContext>,
        request: &Request,
        egress: Option<Arc<dyn Egress>>,
        resume: bool,
        cancelled: &AtomicBool,
    ) -> Result<Self, Error> {
        context.active().map_err(|_| Error::Cancelled)?;
        let account = context.account().clone();
        let runtime = tokio::runtime::Handle::try_current().map_err(|_| Error::Unavailable)?;
        let pinned = pin::cwd_pin_paths(&request.harness);
        let workspace = if resume {
            account
                .workspaces
                .reclaim(context.task(), &pinned, context.now(), cancelled)
        } else {
            account.workspaces.claim(
                context.task(),
                &request.repository.path,
                None,
                &request.capabilities,
                &request.domains,
                &pinned,
                context.now(),
                cancelled,
            )
        }
        .map_err(Error::Workspace)?;
        let mut handle = Self {
            workspace,
            context,
            account,
            egress,
            grant: None,
            revoked: None,
            network: None,
            reported: false,
            acknowledged: false,
            released: None,
            armed: true,
            runtime,
        };
        let wants_network = request
            .capabilities
            .iter()
            .any(|c| c == "network" || c == "install_dependency")
            && !request.domains.is_empty();
        if wants_network {
            if let Some(issuer) = handle.egress.clone() {
                let mut cancel = handle.context.cancellation();
                let already_cancelled = *cancel.borrow();
                let result = if already_cancelled {
                    Err(Error::Cancelled)
                } else {
                    tokio::select! {biased;_ = cancel.changed()=>Err(Error::Cancelled),result=issuer.grant(handle.context.task(),&request.domains)=>result.map_err(Error::Grant)}
                };
                match result {
                    Ok(grant) => handle.grant = Some(grant),
                    Err(error) => {
                        handle.release().await;
                        return Err(error);
                    }
                }
            }
        }
        if handle.context.active().is_err() {
            handle.release().await;
            return Err(Error::Cancelled);
        }
        Ok(handle)
    }
    pub fn grant(&self) -> Option<&Grant> {
        self.grant.as_ref()
    }
    pub async fn release(&mut self) -> Released {
        if let Some(released) = &self.released {
            return released.clone();
        }
        if self.revoked.is_none() {
            if let (Some(issuer), Some(grant)) = (&self.egress, &self.grant) {
                let cleanup = issuer
                    .revoke(grant)
                    .await
                    .unwrap_or_else(|_| egress::Cleanup {
                        confirmed: false,
                        report: object(vec![
                            ("available", JValue::Bool(false)),
                            ("state", text("uncertain")),
                            ("partial", JValue::Bool(true)),
                            ("final", JValue::Bool(false)),
                        ]),
                    });
                let mut network = cleanup.report.clone();
                set(
                    &mut network,
                    "cleanupConfirmed",
                    JValue::Bool(cleanup.confirmed),
                );
                if !cleanup.confirmed {
                    let _ = self.context.event(
                        "tool.uncertain",
                        object(vec![
                            ("tool", text("network")),
                            (
                                "reason",
                                text("Native egress cleanup could not be confirmed."),
                            ),
                        ]),
                    );
                }
                self.network = Some(network);
                self.revoked = Some(cleanup);
            }
        }
        if let Some(network) = &self.network {
            if !self.reported {
                self.reported = self
                    .context
                    .event(
                        "artifact.created",
                        object(vec![
                            ("name", text("network.cleanup")),
                            ("kind", text("native-network-report")),
                            ("network", network.clone()),
                        ]),
                    )
                    .is_ok()
                    && self.context.sync().is_ok();
            }
            if self.reported
                && !self.acknowledged
                && self.revoked.as_ref().is_some_and(|c| c.confirmed)
                && network.get("final") == &JValue::Bool(true)
            {
                if let (Some(issuer), Some(grant), Some(snapshot)) = (
                    &self.egress,
                    &self.grant,
                    network.get("snapshotId").as_str().filter(|s| !s.is_empty()),
                ) {
                    self.acknowledged = issuer.forget(grant, snapshot).await.unwrap_or(false);
                }
            }
        }
        let workspace = self
            .account
            .workspaces
            .release(self.context.task(), false, self.context.now())
            .ok()
            .flatten();
        let released = Released {
            workspace,
            network: self.network.clone(),
            confirmed: self.grant.is_none() || self.revoked.as_ref().is_some_and(|c| c.confirmed),
        };
        self.released = Some(released.clone());
        self.armed = false;
        released
    }
}
impl Drop for Resources {
    fn drop(&mut self) {
        if !self.armed || self.released.is_some() {
            return;
        }
        self.armed = false;
        let mut continuation = Self {
            workspace: self.workspace.clone(),
            context: self.context.clone(),
            account: self.account.clone(),
            egress: self.egress.clone(),
            grant: self.grant.take(),
            revoked: self.revoked.take(),
            network: self.network.take(),
            reported: self.reported,
            acknowledged: self.acknowledged,
            released: None,
            armed: false,
            runtime: self.runtime.clone(),
        };
        let runtime = self.runtime.clone();
        // A detached native cleanup worker retains the execution guard. No request future owns it.
        let _worker = self.runtime.spawn_blocking(move || {
            runtime.block_on(async move {
                let _ = continuation.release().await;
            })
        });
    }
}
#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;
    use crate::{
        context::SystemClock,
        facts::{CodeConfig, Repository},
        Scope,
    };
    use server_projects::task_journals::Surface;
    use std::{
        path::PathBuf,
        sync::{
            atomic::{AtomicUsize, Ordering},
            Mutex,
        },
        time::Duration,
    };
    use tokio::sync::Notify;
    const USER: &str = "11111111-1111-4111-8111-111111111111";
    const TASK: &str = "22222222-2222-4222-8222-222222222222";
    struct Issuer {
        path: PathBuf,
        pending_grant: bool,
        pending_revoke: bool,
        entered: Notify,
        proceed: Notify,
        issued: Arc<AtomicBool>,
        revokes: AtomicUsize,
        forgets: AtomicUsize,
        order: Mutex<Vec<&'static str>>,
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
        fn network(&self) -> Result<egress::Network, egress::Unavailable> {
            Ok(egress::Network::Ready)
        }
        fn endpoint(&self) -> &str {
            "egress.invalid:8080"
        }
        fn grant<'a>(&'a self, task: &'a str, _domains: &'a [String]) -> egress::Reply<'a, Grant> {
            Box::pin(async move {
                self.issued.store(true, Ordering::SeqCst);
                let mut issuing = Issuing(self.issued.clone(), true);
                if self.pending_grant {
                    self.entered.notify_one();
                    self.proceed.notified().await;
                }
                issuing.1 = false;
                Ok(Grant::new(
                    task.into(),
                    "handle".into(),
                    "synthetic-token".into(),
                ))
            })
        }
        fn revoke<'a>(&'a self, _grant: &'a Grant) -> egress::Reply<'a, egress::Cleanup> {
            Box::pin(async move {
                self.revokes.fetch_add(1, Ordering::SeqCst);
                self.order.lock().unwrap().push("revoke");
                if self.pending_revoke {
                    self.entered.notify_one();
                    self.proceed.notified().await;
                }
                self.issued.store(false, Ordering::SeqCst);
                Ok(egress::Cleanup {
                    confirmed: true,
                    report: object(vec![
                        ("available", JValue::Bool(true)),
                        ("final", JValue::Bool(true)),
                        ("partial", JValue::Bool(false)),
                        ("snapshotId", text("snapshot")),
                    ]),
                })
            })
        }
        fn forget<'a>(&'a self, _grant: &'a Grant, snapshot: &'a str) -> egress::Reply<'a, bool> {
            Box::pin(async move {
                assert_eq!(snapshot, "snapshot");
                let journal = std::fs::read_to_string(&self.path).unwrap();
                assert!(journal.contains("network.cleanup"));
                assert!(journal.contains("snapshot"));
                self.forgets.fetch_add(1, Ordering::SeqCst);
                self.order.lock().unwrap().push("forget");
                Ok(true)
            })
        }
    }
    fn setup(
        pending_grant: bool,
        pending_revoke: bool,
    ) -> (
        tempfile::TempDir,
        Arc<Account>,
        Arc<JournalContext>,
        Request,
        Arc<Issuer>,
    ) {
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
            let status = std::process::Command::new("git")
                .args(args)
                .current_dir(&repo)
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_CONFIG_SYSTEM", "/dev/null")
                .status()
                .unwrap();
            assert!(status.success());
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
        let scope = Scope {
            user: USER.into(),
            project: "project".into(),
            surface: Surface::Code,
        };
        let guard = account.control.begin(scope, TASK.into()).unwrap();
        account
            .create(Surface::Code, TASK, "project", vec![text("network")], 1)
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
        let request = Request {
            repository: Repository {
                id: "repo".into(),
                path: repo,
            },
            prompt: "synthetic".into(),
            capabilities: vec!["network".into()],
            domains: vec!["synthetic.invalid".into()],
            harness: "opencode".into(),
            preparation: "direct".into(),
            model: None,
            approve_plan: false,
        };
        let issuer = Arc::new(Issuer {
            path: root
                .path()
                .join("users")
                .join(USER)
                .join("jobs")
                .join(format!("{TASK}.jsonl")),
            pending_grant,
            pending_revoke,
            entered: Notify::new(),
            proceed: Notify::new(),
            issued: Arc::new(AtomicBool::new(false)),
            revokes: AtomicUsize::new(0),
            forgets: AtomicUsize::new(0),
            order: Mutex::new(vec![]),
        });
        (root, account, context, request, issuer)
    }
    async fn released(account: &Account) {
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if account
                    .workspaces
                    .read(TASK)
                    .unwrap()
                    .is_some_and(|r| r.get("status").as_str() == Some("released"))
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
    }
    #[tokio::test]
    async fn report_is_retained_before_forget_and_release_is_idempotent() {
        let (_root, account, context, request, issuer) = setup(false, false);
        let mut resources = Resources::claim(
            context,
            &request,
            Some(issuer.clone()),
            false,
            &AtomicBool::new(false),
        )
        .await
        .unwrap();
        let first = resources.release().await;
        assert!(first.confirmed);
        assert!(first.workspace.is_some());
        assert_eq!(
            first.network.unwrap().get("cleanupConfirmed"),
            &JValue::Bool(true)
        );
        assert!(resources.release().await.confirmed);
        released(&account).await;
        assert_eq!(issuer.revokes.load(Ordering::SeqCst), 1);
        assert_eq!(issuer.forgets.load(Ordering::SeqCst), 1);
        assert_eq!(*issuer.order.lock().unwrap(), vec!["revoke", "forget"]);
    }
    #[tokio::test]
    async fn dropped_claiming_future_transfers_workspace_cleanup_and_issuer_releases_unobserved_grant(
    ) {
        let (_root, account, context, request, issuer) = setup(true, false);
        let retained = issuer.clone();
        let flight = tokio::spawn(async move {
            Resources::claim(
                context,
                &request,
                Some(retained),
                false,
                &AtomicBool::new(false),
            )
            .await
        });
        tokio::time::timeout(Duration::from_secs(5), issuer.entered.notified())
            .await
            .unwrap();
        assert!(issuer.issued.load(Ordering::SeqCst));
        flight.abort();
        assert!(flight.await.is_err());
        released(&account).await;
        assert!(!issuer.issued.load(Ordering::SeqCst));
        assert_eq!(issuer.revokes.load(Ordering::SeqCst), 0);
    }
    #[tokio::test]
    async fn dropping_a_granted_handle_retains_the_reservation_through_real_cleanup_execution() {
        let (_root, account, context, request, issuer) = setup(false, true);
        let scope = context.scope().clone();
        let resources = Resources::claim(
            context,
            &request,
            Some(issuer.clone()),
            false,
            &AtomicBool::new(false),
        )
        .await
        .unwrap();
        drop(resources);
        tokio::time::timeout(Duration::from_secs(5), issuer.entered.notified())
            .await
            .unwrap();
        assert_eq!(issuer.revokes.load(Ordering::SeqCst), 1);
        assert!(account
            .control
            .begin(scope.clone(), "33333333-3333-4333-8333-333333333333".into())
            .is_err());
        issuer.proceed.notify_one();
        released(&account).await;
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if account
                    .control
                    .begin(scope.clone(), "33333333-3333-4333-8333-333333333333".into())
                    .is_ok()
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert_eq!(issuer.forgets.load(Ordering::SeqCst), 1);
        assert!(!issuer.issued.load(Ordering::SeqCst));
    }
}
