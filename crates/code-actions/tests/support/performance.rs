//! Wall-clock qualification belongs to the optimized, single-worker CI phase.
//! Debug tests retain every input and semantic assertion without claiming a
//! debug/dependency-profile mix represents the deployed optimized build.
use std::time::Duration;
pub fn under(took: Duration, budget: Duration, context: &str) {
    if !cfg!(debug_assertions) {
        assert!(took < budget, "{context}: {took:?} >= {budget:?}");
    }
}
