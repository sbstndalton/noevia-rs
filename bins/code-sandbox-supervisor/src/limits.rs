//! Deadline validation shared by environment and direct library configuration.
use std::time::{Duration, Instant};

pub(crate) fn validate(now: Instant, wall: Duration, grace: Duration) -> Result<(), &'static str> {
    if wall.is_zero() {
        return Err("CODE_SANDBOX_MAX_WALL_MS must be greater than zero");
    }
    if now.checked_add(wall).is_none() {
        return Err("sandbox wall deadline is outside the supported range");
    }
    if now.checked_add(grace).is_none() {
        return Err("sandbox grace deadline is outside the supported range");
    }
    Ok(())
}

// Validation happens before serving. If a once-representable deadline ceases
// to fit as time advances, fail closed into immediate teardown, never panic
// after a child has started.
pub(crate) fn deadline_or_now(now: Instant, duration: Duration) -> Instant {
    now.checked_add(duration).unwrap_or(now)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn positive_wall_and_zero_grace_are_supported() {
        let now = Instant::now();
        assert!(validate(now, Duration::from_secs(7200), Duration::ZERO).is_ok());
        assert_eq!(deadline_or_now(now, Duration::ZERO), now);
    }

    #[test]
    fn invalid_deadlines_refuse_before_any_child_is_needed() {
        let now = Instant::now();
        assert!(validate(now, Duration::ZERO, Duration::from_secs(5)).is_err());
        assert!(validate(now, Duration::MAX, Duration::from_secs(5)).is_err());
        assert!(validate(now, Duration::from_secs(5), Duration::MAX).is_err());
    }

    #[test]
    fn runtime_overflow_selects_immediate_teardown() {
        let now = Instant::now();
        assert_eq!(deadline_or_now(now, Duration::MAX), now);
        assert_eq!(
            deadline_or_now(now, Duration::from_secs(1)).duration_since(now),
            Duration::from_secs(1)
        );
    }
}
