//! Foreground reads of this process: the twin of core `rag.cjs` `foreground()` for the front's own
//! native project searches (track R4). While one runs, the outbox and media passes see the
//! foreground as busy, exactly as when Node's owner answer says `foregroundBusy: true`, so a pass
//! yields to a search in either process. Nothing enters it unless native retrieval is switched on.
use std::sync::atomic::{AtomicUsize, Ordering};

static ACTIVE: AtomicUsize = AtomicUsize::new(0);

/// Held for the length of one foreground read; dropping it leaves.
#[must_use = "the foreground ends when the guard is dropped"]
pub struct Guard(());

impl Drop for Guard {
    fn drop(&mut self) {
        ACTIVE.fetch_sub(1, Ordering::AcqRel);
    }
}

/// Enters the foreground (`foreground()`).
pub fn enter() -> Guard {
    ACTIVE.fetch_add(1, Ordering::AcqRel);
    Guard(())
}

/// Whether a foreground read of this process is running (`foregroundBusy()`).
pub fn busy() -> bool {
    ACTIVE.load(Ordering::Acquire) > 0
}

#[cfg(test)]
mod tests {
    #[test]
    fn nested_reads_keep_it_busy_until_the_last_leaves() {
        assert!(!super::busy());
        let a = super::enter();
        let b = super::enter();
        drop(a);
        assert!(super::busy());
        drop(b);
        assert!(!super::busy());
    }
}
