//! Bounds optional native inventory work while a child can still supply the answer.
//! Only timeouts are cached, never inventory values or other failures. A monotonic deadline
//! avoids wall-clock changes; one deadline per sampler bounds memory regardless of requests.
use std::{collections::BTreeMap, future::Future, sync::Mutex, time::Duration};
use tokio::time::Instant;

pub(crate) const TIMEOUT: Duration = Duration::from_secs(10);
const COOLDOWN: Duration = Duration::from_secs(10);

#[derive(Default)]
pub(crate) struct Reads {
    retry_after: Mutex<Option<Instant>>,
}

impl Reads {
    pub(crate) async fn run<T>(&self, work: impl Future<Output = T>) -> Result<T, ()> {
        let deadline = *self
            .retry_after
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if deadline.is_some_and(|deadline| Instant::now() < deadline) {
            return Err(());
        }
        match tokio::time::timeout(TIMEOUT, work).await {
            Ok(value) => Ok(value),
            Err(_) => {
                *self
                    .retry_after
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) =
                    Some(Instant::now() + COOLDOWN);
                Err(())
            }
        }
    }
}

/// Bounded, input-free counted warnings. Callers use fixed reason classes.
#[derive(Default)]
pub(crate) struct Log(Mutex<BTreeMap<&'static str, u64>>);

impl Log {
    pub(crate) fn record(&self, prefix: &str, class: &'static str) -> Option<String> {
        let mut counts = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !counts.contains_key(class) && counts.len() >= 32 {
            return None;
        }
        let count = counts.entry(class).or_default();
        *count = count.saturating_add(1);
        if *count == 1
            || count
                .checked_ilog10()
                .is_some_and(|power| *count == 10_u64.pow(power))
        {
            Some(format!("{prefix} ({class}); fallback count {count}"))
        } else {
            None
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[tokio::test(start_paused = true)]
    async fn timeout_has_a_fixed_monotonic_cooldown_and_recovery_retries() {
        let reads = Reads::default();
        let called = AtomicUsize::new(0);
        let hung = || async {
            called.fetch_add(1, Ordering::Relaxed);
            std::future::pending::<()>().await;
        };
        let before = Instant::now();
        assert!(reads.run(hung()).await.is_err());
        assert_eq!(Instant::now() - before, TIMEOUT);
        for _ in 0..100 {
            assert!(reads.run(hung()).await.is_err());
        }
        assert_eq!(called.load(Ordering::Relaxed), 1);
        tokio::time::advance(COOLDOWN - Duration::from_millis(1)).await;
        assert!(reads.run(hung()).await.is_err());
        tokio::time::advance(Duration::from_millis(1)).await;
        assert_eq!(reads.run(async { 7 }).await, Ok(7));
        // Successful results are not cached.
        assert_eq!(reads.run(async { 8 }).await, Ok(8));
        assert!(reads.run(hung()).await.is_err());
        assert_eq!(called.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn warning_counts_repeat_at_powers_of_ten_and_reason_storage_is_bounded() {
        let log = Log::default();
        let lines: Vec<_> = (0..150)
            .filter_map(|_| log.record("qa fallback", "Docker did not answer in time"))
            .collect();
        assert_eq!(
            lines,
            vec![
                "qa fallback (Docker did not answer in time); fallback count 1",
                "qa fallback (Docker did not answer in time); fallback count 10",
                "qa fallback (Docker did not answer in time); fallback count 100",
            ]
        );
        // Each table has a fixed cap; production callers use far fewer fixed classes.
        let mut counts = log.0.lock().unwrap();
        for key in [
            "a", "b", "c", "d", "e", "f", "g", "h", "i", "j", "k", "l", "m", "n", "o", "p", "q",
            "r", "s", "t", "u", "v", "w", "x", "y", "z", "aa", "ab", "ac", "ad", "ae",
        ] {
            counts.insert(key, 1);
        }
        drop(counts);
        assert!(log.record("qa", "overflow").is_none());
        assert_eq!(log.0.lock().unwrap().len(), 32);
    }
    #[test]
    #[cfg(all(feature = "native-search", feature = "native-telemetry"))]
    fn hung_socket_fallbacks_emit_the_expected_warning_lines() {
        use std::io::Read;
        use std::process::{Command, Stdio};
        use std::time::Instant;
        for (test, expected) in [
            ("section_autoconfig::inventory::tests::a_hung_docker_socket_keeps_the_child_inventory_and_repeated_reads_return_promptly",
             "native-section-autoconfig: the backend inventory was taken from the python child (Docker did not answer in time); fallback count 1"),
            ("search::estimates::tests::an_unreadable_summary_skips_docker_and_a_timeout_leaves_estimates_with_the_child",
             "native-search: the repository estimates were asked of the python child (Docker did not answer in time); fallback count 1"),
        ] {
            let mut child = Command::new(std::env::current_exe().unwrap())
                .args(["--exact", test, "--nocapture"])
                .stdout(Stdio::null()).stderr(Stdio::piped()).spawn().unwrap();
            let deadline = Instant::now() + Duration::from_secs(30);
            let status = loop {
                if let Some(status) = child.try_wait().unwrap() { break status; }
                if Instant::now() >= deadline {
                    child.kill().unwrap();
                    child.wait().unwrap();
                    panic!("hung socket test did not finish before its deadline");
                }
                std::thread::sleep(Duration::from_millis(20));
            };
            let mut output = String::new();
            child.stderr.take().unwrap().read_to_string(&mut output).unwrap();
            assert!(status.success(), "{output}");
            assert!(output.lines().any(|line| line == expected), "{output}");
        }
    }
}
