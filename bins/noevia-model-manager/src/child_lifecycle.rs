//! Keep the parent's stores alive until the killed child is confirmed reaped (#1369).
use std::{future::Future, io, process::ExitStatus, sync::Arc, time::Duration};
trait Reaper {
    fn request_kill(&mut self) -> io::Result<()>;
    fn wait(&mut self) -> impl Future<Output = io::Result<ExitStatus>>;
}
impl Reaper for tokio::process::Child {
    fn request_kill(&mut self) -> io::Result<()> {
        self.start_kill()
    }
    fn wait(&mut self) -> impl Future<Output = io::Result<ExitStatus>> {
        self.wait()
    }
}
pub(super) async fn reap<T>(child: &mut tokio::process::Child, lease: Arc<T>) -> io::Result<()> {
    reap_with(child, lease).await
}
async fn reap_with<T>(child: &mut impl Reaper, lease: Arc<T>) -> io::Result<()> {
    let mut lease = Some(lease);
    let mut failure = child.request_kill().err();
    let mut warned = false;
    loop {
        match tokio::time::timeout(Duration::from_secs(5), child.wait()).await {
            Ok(Ok(_)) => {
                drop(lease.take());
                return failure.map_or(Ok(()), Err);
            }
            Ok(Err(error)) => {
                if failure.is_none() {
                    failure = Some(error);
                }
            }
            Err(_) => {}
        }
        if !warned {
            eprintln!(
                "native model manager retains writer lease while waiting for legacy process reap"
            );
            warned = true;
        }
        // A deadline cannot prove OS process death. Retain state and retry, without spinning.
        tokio::time::sleep(Duration::from_millis(250)).await;
        if let Err(error) = child.request_kill() {
            if failure.is_none() {
                failure = Some(error);
            }
        }
    }
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;
    use std::{
        collections::VecDeque,
        os::unix::process::ExitStatusExt,
        sync::{
            atomic::{AtomicUsize, Ordering},
            Arc, Weak,
        },
    };
    struct Lease(Arc<AtomicUsize>);
    impl Drop for Lease {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }
    enum Wait {
        Timeout,
        Error,
        Reaped,
    }
    struct Fake {
        lease: Weak<Lease>,
        kill_errors: usize,
        kills: usize,
        waits: VecDeque<Wait>,
    }
    impl Reaper for Fake {
        fn request_kill(&mut self) -> io::Result<()> {
            assert!(self.lease.upgrade().is_some());
            self.kills += 1;
            if self.kill_errors > 0 {
                self.kill_errors -= 1;
                Err(io::Error::other("synthetic kill failure"))
            } else {
                Ok(())
            }
        }
        async fn wait(&mut self) -> io::Result<ExitStatus> {
            assert!(self.lease.upgrade().is_some());
            match self.waits.pop_front().expect("bounded synthetic sequence") {
                Wait::Timeout => {
                    tokio::time::sleep(Duration::from_secs(6)).await;
                    unreachable!("timeout cancels synthetic wait")
                }
                Wait::Error => Err(io::Error::other("synthetic wait failure")),
                Wait::Reaped => Ok(ExitStatus::from_raw(0)),
            }
        }
    }
    #[tokio::test(start_paused = true)]
    async fn kill_failure_wait_error_and_timeout_cannot_release_lease_before_reap() {
        for waits in [
            vec![Wait::Error, Wait::Reaped],
            vec![Wait::Timeout, Wait::Reaped],
            vec![Wait::Error, Wait::Timeout, Wait::Reaped],
        ] {
            let drops = Arc::new(AtomicUsize::new(0));
            let lease = Arc::new(Lease(drops.clone()));
            let mut child = Fake {
                lease: Arc::downgrade(&lease),
                kill_errors: 1,
                kills: 0,
                waits: waits.into(),
            };
            assert!(reap_with(&mut child, lease).await.is_err());
            assert!(child.waits.is_empty());
            assert!(child.kills >= 2);
            assert_eq!(drops.load(Ordering::SeqCst), 1);
        }
    }
    #[tokio::test(start_paused = true)]
    async fn successful_reap_needs_one_kill_and_preserves_success() {
        let drops = Arc::new(AtomicUsize::new(0));
        let lease = Arc::new(Lease(drops.clone()));
        let mut child = Fake {
            lease: Arc::downgrade(&lease),
            kill_errors: 0,
            kills: 0,
            waits: vec![Wait::Reaped].into(),
        };
        assert!(reap_with(&mut child, lease).await.is_ok());
        assert_eq!(child.kills, 1);
        assert_eq!(drops.load(Ordering::SeqCst), 1);
    }
}
