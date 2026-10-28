//! Private child accounting shared with the real namespace-init collector.
use std::collections::HashMap;
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Terminal {
    pub code: Option<i32>,
    pub signal: Option<i32>,
}
#[derive(Default)]
pub struct Children {
    pending: HashMap<i32, u64>,
    completed: HashMap<u64, Terminal>,
    next: u64,
    drained_epoch: Option<u64>,
}
impl Children {
    pub fn register(&mut self, pid: i32) -> Result<u64, &'static str> {
        if pid <= 1 || self.pending.contains_key(&pid) {
            return Err("invalid owned admission");
        }
        let ticket = self.next;
        self.next = self
            .next
            .checked_add(1)
            .ok_or("admission counter overflow")?;
        self.pending.insert(pid, ticket);
        self.drained_epoch = None;
        Ok(ticket)
    }
    pub fn terminal(&mut self, pid: i32, result: Terminal) {
        self.drained_epoch = None;
        // Adopted descendants are reaped without an unbounded per-PID history.
        if let Some(ticket) = self.pending.remove(&pid) {
            self.completed.insert(ticket, result);
        }
    }
    pub fn still_alive(&mut self) {
        self.drained_epoch = None;
    }
    /// Call only for the kernel's all-child __WALL ECHILD outcome, not an empty proc snapshot.
    pub fn all_child_echild(&mut self) -> Result<(), &'static str> {
        if !self.pending.is_empty() {
            return Err("missing tracked terminal evidence");
        }
        self.drained_epoch = Some(self.next);
        Ok(())
    }
    pub fn take(&mut self, ticket: u64) -> Option<Terminal> {
        self.completed.remove(&ticket)
    }
    pub fn receipt_ready(&self) -> bool {
        self.drained_epoch == Some(self.next)
            && self.pending.is_empty()
            && self.completed.is_empty()
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    const OK: Terminal = Terminal {
        code: Some(0),
        signal: None,
    };
    #[test]
    fn no_proc_or_poll_zero_can_establish_a_barrier() -> Result<(), &'static str> {
        let mut children = Children::default();
        assert!(!children.receipt_ready());
        children.still_alive();
        assert!(!children.receipt_ready());
        children.all_child_echild()?;
        assert!(children.receipt_ready());
        let ticket = children.register(123)?;
        assert!(!children.receipt_ready());
        assert!(children.all_child_echild().is_err());
        assert_eq!(children.take(ticket), None);
        Ok(())
    }
    #[test]
    fn all_recorded_statuses_are_claimed_before_receipt() -> Result<(), &'static str> {
        let mut children = Children::default();
        let ticket = children.register(123)?;
        children.terminal(123, OK);
        assert!(!children.receipt_ready());
        children.all_child_echild()?;
        assert!(!children.receipt_ready());
        assert_eq!(children.take(ticket), Some(OK));
        assert!(children.receipt_ready());
        children.still_alive();
        assert!(!children.receipt_ready());
        Ok(())
    }
    #[test]
    fn reused_pid_never_overwrites_an_earlier_unclaimed_measurement() -> Result<(), &'static str> {
        let mut children = Children::default();
        let first = children.register(123)?;
        children.terminal(123, OK);
        let second = children.register(123)?;
        let killed = Terminal {
            code: None,
            signal: Some(9),
        };
        assert_ne!(first, second);
        children.terminal(123, killed);
        children.all_child_echild()?;
        assert_eq!(children.take(first), Some(OK));
        assert!(!children.receipt_ready());
        assert_eq!(children.take(second), Some(killed));
        assert!(children.receipt_ready());
        Ok(())
    }
    #[test]
    fn helper_admission_invalidates_an_earlier_complete_drain() -> Result<(), &'static str> {
        let mut children = Children::default();
        children.all_child_echild()?;
        let helper = children.register(321)?;
        assert!(!children.receipt_ready());
        children.terminal(321, OK);
        assert_eq!(children.take(helper), Some(OK));
        assert!(!children.receipt_ready());
        children.all_child_echild()?;
        assert!(children.receipt_ready());
        Ok(())
    }
    #[test]
    fn adopted_exits_do_not_accumulate_unbounded_history() -> Result<(), &'static str> {
        let mut children = Children::default();
        for pid in 2..100_002 {
            children.terminal(pid, OK);
        }
        assert!(children.pending.is_empty());
        assert!(children.completed.is_empty());
        children.all_child_echild()?;
        assert!(children.receipt_ready());
        Ok(())
    }
    #[test]
    fn no_live_pid_or_ticket_reuse_is_admitted() -> Result<(), &'static str> {
        let mut children = Children::default();
        children.register(123)?;
        assert!(children.register(123).is_err());
        assert!(children.register(1).is_err());
        children.next = u64::MAX;
        assert!(children.register(456).is_err());
        Ok(())
    }
}
