//! Private production admission/completion policy; no socket or portable-runner substitute.
use super::lifecycle::Terminal;
use std::time::{Duration, Instant};
#[derive(Clone, Copy)]
pub(super) struct Admission {
    deadline: Instant,
}
impl Admission {
    pub(super) fn deadline(self) -> Instant {
        self.deadline
    }
}
pub(super) struct Budget {
    deadline: Instant,
    expired: bool,
}
pub(super) struct Completion {
    pub terminal: Option<Terminal>,
    pub timed_out: bool,
}
impl Budget {
    pub(super) fn new(deadline: Instant) -> Self {
        Self {
            deadline,
            expired: false,
        }
    }
    pub(super) fn expired(&mut self, now: Instant) -> bool {
        self.expired |= now >= self.deadline;
        self.expired
    }
    pub(super) fn admit(&mut self, now: Instant) -> Result<Admission, &'static str> {
        if self.expired(now) {
            return Err("execution budget expired before admission");
        }
        Ok(Admission {
            deadline: self.deadline,
        })
    }
    pub(super) fn observe(
        &mut self,
        now: Instant,
        terminal: Option<Terminal>,
    ) -> Option<Completion> {
        let expired = self.expired(now);
        if terminal.is_some() || expired {
            Some(Completion {
                terminal,
                timed_out: expired,
            })
        } else {
            None
        }
    }
}
/// Parent samples the raw monotonic clock BEFORE computing remaining Instant budget.
/// Therefore bootstrap delay cannot renew a budget or extend it through sampling order.
pub(super) fn bootstrap_deadline(
    before_remaining_ns: u64,
    remaining: Duration,
) -> Result<u64, &'static str> {
    let ns = u64::try_from(remaining.as_nanos()).map_err(|_| "bootstrap budget overflow")?;
    if ns == 0 {
        return Err("bootstrap budget expired");
    }
    before_remaining_ns
        .checked_add(ns)
        .ok_or("bootstrap clock overflow")
}
pub(super) fn bootstrap_live(now_ns: u64, deadline_ns: u64) -> bool {
    now_ns < deadline_ns
}
#[cfg(test)]
mod tests {
    use super::*;
    const OK: Terminal = Terminal {
        code: Some(0),
        signal: None,
    };
    #[test]
    fn expired_admission_cannot_produce_a_worker_token() -> Result<(), &'static str> {
        let now = Instant::now();
        let end = now + Duration::from_secs(1);
        let mut budget = Budget::new(end);
        assert_eq!(budget.admit(now)?.deadline(), end);
        assert!(budget.admit(end).is_err());
        assert!(budget.admit(now).is_err());
        Ok(())
    }
    #[test]
    fn a_newly_observed_exit_zero_after_expiry_is_not_a_timely_pass() -> Result<(), &'static str> {
        let now = Instant::now();
        let end = now + Duration::from_secs(1);
        let mut budget = Budget::new(end);
        budget.admit(now)?;
        let completion = budget
            .observe(end + Duration::from_millis(1), Some(OK))
            .ok_or("completion")?;
        assert!(completion.timed_out);
        assert_eq!(completion.terminal, Some(OK));
        assert!(budget.admit(end).is_err());
        Ok(())
    }
    #[test]
    fn live_worker_timeout_still_requires_its_terminal_after_drain() -> Result<(), &'static str> {
        let now = Instant::now();
        let end = now + Duration::from_secs(1);
        let mut budget = Budget::new(end);
        assert!(budget.observe(now, None).is_none());
        let completion = budget.observe(end, None).ok_or("timeout")?;
        assert!(completion.timed_out);
        assert_eq!(completion.terminal, None);
        Ok(())
    }
    #[test]
    fn completion_before_expiry_does_not_hide_late_drain() -> Result<(), &'static str> {
        let now = Instant::now();
        let end = now + Duration::from_secs(1);
        let mut budget = Budget::new(end);
        assert!(!budget.observe(now, Some(OK)).ok_or("completion")?.timed_out);
        assert!(budget.expired(end));
        Ok(())
    }
    #[test]
    fn helper_bootstrap_does_not_refresh_remaining_time() -> Result<(), &'static str> {
        let deadline = bootstrap_deadline(100, Duration::from_nanos(50))?;
        assert_eq!(deadline, 150);
        assert!(bootstrap_live(149, deadline));
        assert!(!bootstrap_live(150, deadline));
        assert!(!bootstrap_live(151, deadline));
        assert!(bootstrap_deadline(u64::MAX, Duration::from_nanos(1)).is_err());
        assert!(bootstrap_deadline(10, Duration::ZERO).is_err());
        Ok(())
    }
}
