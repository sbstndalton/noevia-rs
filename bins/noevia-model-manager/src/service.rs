//! Mandatory native service lifecycle. There is no subprocess or private HTTP dependency.
use crate::StateData;
use std::{future::Future, sync::Arc, time::Duration};

/// Must fit within the deployment's ten-second stop window. No Rust destructor may release
/// a writer lease while another task/thread still runs: both clean and deadline exits use
/// process exit, which terminates all same-process workers before the kernel releases locks.
pub const SHUTDOWN_GRACE: Duration = Duration::from_secs(8);

/// Proof of installed native owners, created after schema/seed/INI bootstrap. Its state owns
/// the writer leases for the complete serving and shutdown lifetime.
pub struct Ready {
    state: Arc<StateData>,
    app: axum::Router,
}
impl Ready {
    pub fn new(state: Arc<StateData>) -> Result<Self, &'static str> {
        if !state
            .prompts
            .as_ref()
            .is_some_and(|store| store.schema_initialized())
        {
            return Err("native service schema and seed are not initialized");
        }
        let app = crate::routing::router(state.clone())?;
        Ok(Self { state, app })
    }
    pub async fn run(
        self,
        listener: tokio::net::TcpListener,
        stop: impl Future<Output = ()> + Send + 'static,
        stop_background: impl FnOnce(),
    ) -> Result<(), Box<dyn std::error::Error>> {
        let (signal, stopped) = tokio::sync::oneshot::channel::<()>();
        let serve = axum::serve(listener, self.app.clone()).with_graceful_shutdown(async {
            let _ = stopped.await;
        });
        let mut serving = Box::pin(async { serve.await });
        let result = tokio::select! {
            result = &mut serving => Some(result),
            _ = stop => None,
        };
        // No new connections after the graceful signal. Existing requests keep their
        // admission until their registered blocking work ends, including dropped clients.
        let deadline = tokio::time::Instant::now() + SHUTDOWN_GRACE;
        self.state.permits.close();
        let _ = signal.send(());
        stop_background();
        if let Some(embed) = self.state.embedding.as_ref() {
            embed.cancel_warm();
        }
        let drain = async {
            cancel_jobs(self.state.clone()).await;
            if result.is_none() {
                let _ = serving.await;
            }
            // A closed semaphore refuses new work; it is not proof that old work ended.
            // Existing admissions return permits only after their orphan joins finish.
            while self.state.permits.available_permits() < 4 {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            // A request already admitted at the boundary may have queued a job during its
            // drain. Sweep again after admission is empty, then await actual worker lifetimes.
            cancel_jobs(self.state.clone()).await;
            if let Some(diagnosis) = self.state.diagnosis.as_ref() {
                diagnosis.shutdown().await;
            }
            crate::work::drain(deadline).await
        };
        let drained = matches!(tokio::time::timeout_at(deadline, drain).await, Ok(Ok(())));
        if !drained {
            eprintln!("native shutdown grace expired; terminating with writer leases held");
        }
        // Keep self/state alive across exit. Dropping Tokio/Store could join a blocked thread
        // forever or release another lease early; process::exit does neither.
        std::process::exit(if result.is_some_and(|r| r.is_err()) {
            1
        } else {
            0
        });
    }
}

async fn cancel_jobs(state: Arc<StateData>) {
    // Cancellation takes job mutexes: account for their actual worker lifetime and include any
    // lock contention in the same deadline, rather than blocking before the grace timer starts.
    let _ = crate::work::spawn_blocking(move || {
        if let Some(runner) = state.bench_runner.as_ref() {
            runner.cancel();
        }
        if let Some(table) = state.downloads.as_ref() {
            table.cancel_all();
        }
    })
    .await;
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn readiness_requires_actual_schema_seed_before_owner_validation() {
        let mut state = crate::test_support::test_state("http://127.0.0.1:9");
        assert_eq!(
            Ready::new(Arc::new(state)).err(),
            Some("native service schema and seed are not initialized")
        );
        let dir = tempfile::Builder::new()
            .prefix("noevia-native-ready-")
            .tempdir_in("/tmp")
            .unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        state = crate::test_support::test_state("http://127.0.0.1:9");
        state.prompts = Some(Arc::new(
            crate::prompts::Prompts::open(dir.path()).await.unwrap(),
        ));
        assert_eq!(
            Ready::new(Arc::new(state)).err(),
            Some("native service schema and seed are not initialized")
        );
        state = crate::test_support::test_state("http://127.0.0.1:9");
        state.prompts = Some(Arc::new(
            crate::prompts::Prompts::open_with_schema(dir.path(), true)
                .await
                .unwrap(),
        ));
        let refused = Ready::new(Arc::new(state)).err().unwrap();
        assert!(refused.contains("native routing requires"), "{refused}");
    }
    // Invoked only by the owned subprocess regression below. It uses the same supervisor,
    // synthetic SQLite transaction and actual lifetime writer lease; no production test hook.
    #[tokio::test]
    async fn blocked_background_process_fixture() {
        let Some(root) = std::env::var_os("NOEVIA_QA_BACKGROUND_ROOT") else {
            return;
        };
        let root = std::path::PathBuf::from(root);
        let mut signal =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).unwrap();
        let mut state = crate::test_support::test_state("http://127.0.0.1:9");
        state.prompts = Some(Arc::new(
            crate::prompts::Prompts::open_with_schema(&root, true)
                .await
                .unwrap(),
        ));
        let (entered, ready) = tokio::sync::oneshot::channel();
        let dir = root.clone();
        let _worker = crate::work::spawn_blocking(move || {
            let db = rusqlite::Connection::open(dir.join("model_loader.db")).unwrap();
            db.execute_batch("BEGIN IMMEDIATE; INSERT INTO prompts(name,body,created_at) VALUES('qa-uncommitted','qa',1)").unwrap();
            std::fs::write(dir.join("worker-entered"), b"ready").unwrap();
            let _ = entered.send(());
            let (_hold, wait) = std::sync::mpsc::channel::<()>();
            let _ = wait.recv_timeout(Duration::from_secs(30));
            db.execute_batch("ROLLBACK").unwrap();
        });
        ready.await.unwrap();
        let state = Arc::new(state);
        let ready = Ready {
            state,
            app: axum::Router::new(),
        };
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        ready
            .run(
                listener,
                async move {
                    signal.recv().await;
                },
                || {},
            )
            .await
            .unwrap();
    }

    #[test]
    fn independent_blocking_worker_is_drained_or_keeps_lease_until_deadline_exit() {
        use std::os::unix::fs::PermissionsExt;
        use std::{
            fs,
            process::{Command, Stdio},
            time::Instant,
        };
        let dir = tempfile::Builder::new()
            .prefix("noevia-native-worker-")
            .tempdir_in("/tmp")
            .unwrap();
        fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let stderr = fs::File::create(dir.path().join("stderr")).unwrap();
        let mut child = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "service::tests::blocked_background_process_fixture",
                "--nocapture",
            ])
            .env("NOEVIA_QA_BACKGROUND_ROOT", dir.path())
            .stdout(Stdio::null())
            .stderr(stderr)
            .spawn()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while !dir.path().join("worker-entered").exists() {
            if Instant::now() >= deadline || child.try_wait().unwrap().is_some() {
                let _ = child.kill();
                let _ = child.wait();
                panic!("owned background fixture failed to reach worker");
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        let lock = fs::File::options()
            .read(true)
            .write(true)
            .open(dir.path().join("model_loader.db.prompts.lock"))
            .unwrap();
        let started = Instant::now();
        let pid = rustix::process::Pid::from_raw(i32::try_from(child.id()).unwrap()).unwrap();
        rustix::process::kill_process(pid, rustix::process::Signal::TERM).unwrap();
        std::thread::sleep(Duration::from_millis(100));
        assert!(
            child.try_wait().unwrap().is_none(),
            "background worker falsely counted as drained"
        );
        assert!(
            lock.try_lock().is_err(),
            "writer lease released before background worker death"
        );
        let deadline = started + Duration::from_secs(11);
        let status = loop {
            if let Some(status) = child.try_wait().unwrap() {
                break status;
            }
            if Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                panic!("shutdown unbounded");
            }
            std::thread::sleep(Duration::from_millis(10));
        };
        assert!(status.success());
        assert!(
            started.elapsed() >= Duration::from_secs(7),
            "background drain skipped"
        );
        assert!(fs::read_to_string(dir.path().join("stderr"))
            .unwrap()
            .contains("native shutdown grace expired"));
        lock.try_lock().unwrap();
        let db = rusqlite::Connection::open(dir.path().join("model_loader.db")).unwrap();
        assert_eq!(
            db.query_row("PRAGMA integrity_check", [], |r| r.get::<_, String>(0))
                .unwrap(),
            "ok"
        );
        assert_eq!(
            db.query_row(
                "SELECT count(*) FROM prompts WHERE name='qa-uncommitted'",
                [],
                |r| r.get::<_, i64>(0)
            )
            .unwrap(),
            0
        );
    }
}
