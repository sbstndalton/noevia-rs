//! Per-account scheduling policy from diary-backup-worker.cjs. Durable backup progress stays in Diary.
use js_json::JValue;
use std::collections::HashMap;

#[derive(Default)]
pub struct Schedule {
    states: HashMap<String, (u64, u64)>,
}
impl Schedule {
    /// Checked before the storage latch: an ineligible tick must never consume a shared probe.
    pub fn due(&self, id: &str, now: u64) -> bool {
        self.states.get(id).is_none_or(|(_, at)| now >= *at)
    }
    /// Serial batches preserve Core's maximum of four, with no overlap between ticks.
    /// Dropping this future aborts and joins no detached user work (JoinSet aborts on drop).
    pub async fn tick<F, Fut>(&mut self, users: Vec<String>, now: u64, interval: u64, run: F)
    where
        F: Fn(String) -> Fut,
        Fut: std::future::Future<Output = Option<Option<JValue>>> + Send + 'static,
    {
        self.tick_until(users, now, interval, run, None).await;
    }
    pub async fn tick_until<F, Fut>(
        &mut self,
        users: Vec<String>,
        now: u64,
        interval: u64,
        run: F,
        mut stop: Option<tokio::sync::watch::Receiver<bool>>,
    ) where
        F: Fn(String) -> Fut,
        Fut: std::future::Future<Output = Option<Option<JValue>>> + Send + 'static,
    {
        self.retain(&users);
        for batch in users.chunks(4) {
            if stop.as_ref().is_some_and(|s| *s.borrow()) {
                return;
            }
            let mut tasks = tokio::task::JoinSet::new();
            for id in batch {
                if !self.due(id, now) {
                    continue;
                }
                let id = id.clone();
                let call = run(id.clone());
                tasks.spawn(async move { (id, call.await) });
            }
            loop {
                tokio::select! { biased;
                    _ = async { if let Some(s) = &mut stop { let _ = s.wait_for(|s| *s).await; } else { std::future::pending::<()>().await; } } => {
                        tasks.shutdown().await;
                        return;
                    }
                    result = tasks.join_next() => match result {
                        None => break,
                        Some(Ok((id, Some(reply)))) => self.finish(id, now, interval, reply.as_ref()),
                        _ => {}
                    }
                }
            }
        }
    }
    pub fn retain(&mut self, ids: &[String]) {
        self.states.retain(|id, _| ids.contains(id));
    }
    pub fn finish(&mut self, id: String, now: u64, interval: u64, reply: Option<&JValue>) {
        let idle = reply.is_none_or(|r| {
            !matches!(r, JValue::Obj(_) | JValue::Arr(_))
                || r.get("mode").as_str() == Some("legacy")
                || r.get("backup").as_str() == Some("not_configured")
        });
        if idle {
            let previous = self.states.get(&id).map_or(interval, |(delay, _)| *delay);
            let delay = previous.saturating_mul(2).min(600_000);
            self.states.insert(id, (delay, now.saturating_add(delay)));
        } else {
            self.states.insert(id, (interval, 0));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn concurrency_suspension_cancellation_and_serial_ticks() {
        use std::sync::{
            atomic::{AtomicUsize, Ordering},
            Arc,
        };
        let active = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));
        let calls = Arc::new(AtomicUsize::new(0));
        struct Active(Arc<AtomicUsize>);
        impl Drop for Active {
            fn drop(&mut self) {
                self.0.fetch_sub(1, Ordering::SeqCst);
            }
        }
        let run = |id: String| {
            let (active, peak, calls) = (active.clone(), peak.clone(), calls.clone());
            async move {
                if id == "suspended" {
                    return None;
                }
                calls.fetch_add(1, Ordering::SeqCst);
                let n = active.fetch_add(1, Ordering::SeqCst) + 1;
                let _guard = Active(active);
                peak.fetch_max(n, Ordering::SeqCst);
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                Some(None)
            }
        };
        let mut s = Schedule::default();
        let users = (0..9)
            .map(|n| n.to_string())
            .chain(["suspended".into()])
            .collect::<Vec<_>>();
        s.tick(users.clone(), 0, 30_000, &run).await;
        assert_eq!(peak.load(Ordering::SeqCst), 4);
        assert_eq!(calls.load(Ordering::SeqCst), 9);
        s.tick(users.clone(), 0, 30_000, &run).await;
        assert_eq!(calls.load(Ordering::SeqCst), 9);
        assert!(s.due("suspended", 0));
        let timed = tokio::time::timeout(
            std::time::Duration::from_millis(1),
            s.tick(users, 60_000, 30_000, &run),
        )
        .await;
        assert!(timed.is_err());
        tokio::task::yield_now().await;
        assert_eq!(active.load(Ordering::SeqCst), 0);
    }
    #[tokio::test]
    async fn stop_aborts_and_joins_the_current_batch_before_returning() {
        use std::sync::{
            atomic::{AtomicUsize, Ordering},
            Arc,
        };
        struct Active(Arc<AtomicUsize>);
        impl Drop for Active {
            fn drop(&mut self) {
                self.0.fetch_sub(1, Ordering::SeqCst);
            }
        }
        let active = Arc::new(AtomicUsize::new(0));
        let calls = Arc::new(AtomicUsize::new(0));
        let (tx, rx) = tokio::sync::watch::channel(false);
        let mut schedule = Schedule::default();
        schedule
            .tick_until(
                (0..9).map(|n| n.to_string()).collect(),
                0,
                30_000,
                |_| {
                    let (active, calls, tx) = (active.clone(), calls.clone(), tx.clone());
                    async move {
                        calls.fetch_add(1, Ordering::SeqCst);
                        let n = active.fetch_add(1, Ordering::SeqCst) + 1;
                        let _guard = Active(active);
                        if n == 4 {
                            let _ = tx.send(true);
                        }
                        std::future::pending::<()>().await;
                        Some(None)
                    }
                },
                Some(rx),
            )
            .await;
        assert_eq!(active.load(Ordering::SeqCst), 0);
        assert_eq!(calls.load(Ordering::SeqCst), 4);
    }
    #[test]
    fn backoff_idle_progress_restart_and_removed_accounts() {
        let mut s = Schedule::default();
        let id = "synthetic".to_owned();
        assert!(s.due(&id, 0));
        s.finish(id.clone(), 0, 30_000, None);
        assert!(!s.due(&id, 59_999));
        assert!(s.due(&id, 60_000));
        for _ in 0..20 {
            s.finish(id.clone(), 0, 30_000, None);
        }
        assert!(!s.due(&id, 599_999));
        assert!(s.due(&id, 600_000));
        s.finish(
            id.clone(),
            600_000,
            30_000,
            Some(&JValue::obj([("backup", JValue::Str("done".into()))])),
        );
        assert!(s.due(&id, 600_000));
        s.finish(
            id.clone(),
            600_000,
            30_000,
            Some(&JValue::obj([("mode", JValue::Str("legacy".into()))])),
        );
        assert!(!s.due(&id, 659_999));
        s.retain(&[]);
        assert!(s.due(&id, 0));
        assert!(Schedule::default().due(&id, 0));
    }
}
