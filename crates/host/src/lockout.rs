//! Wrong-PIN throttling.
//!
//! Each attempt is one online guess. The first few failures only cost a
//! short delay; after that the host refuses every attempt for a window that
//! doubles with each further failure. A correct PIN resets the count.

use std::time::{Duration, Instant};

const FREE_FAILURES: u32 = 5;
const BASE_LOCK: Duration = Duration::from_secs(30);
const MAX_LOCK: Duration = Duration::from_secs(15 * 60);

#[derive(Debug, Default)]
pub struct Lockout {
    failures: u32,
    until: Option<Instant>,
}

impl Lockout {
    /// Time left before another attempt is accepted.
    pub fn remaining(&self, now: Instant) -> Option<Duration> {
        self.until
            .and_then(|until| until.checked_duration_since(now))
            .filter(|left| !left.is_zero())
    }

    pub fn fail(&mut self, now: Instant) -> Option<Duration> {
        self.failures = self.failures.saturating_add(1);
        if self.failures < FREE_FAILURES {
            return None;
        }
        let doublings = (self.failures - FREE_FAILURES).min(10);
        let lock = BASE_LOCK.saturating_mul(1 << doublings).min(MAX_LOCK);
        self.until = Some(now + lock);
        Some(lock)
    }

    pub fn succeed(&mut self) {
        self.failures = 0;
        self.until = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn locks_after_free_failures_and_doubles() {
        let now = Instant::now();
        let mut lockout = Lockout::default();
        for _ in 0..FREE_FAILURES - 1 {
            assert_eq!(lockout.fail(now), None);
        }
        assert_eq!(lockout.remaining(now), None);
        assert_eq!(lockout.fail(now), Some(BASE_LOCK));
        assert_eq!(lockout.remaining(now), Some(BASE_LOCK));
        assert_eq!(lockout.fail(now), Some(BASE_LOCK * 2));
        assert_eq!(lockout.remaining(now + MAX_LOCK), None);
        for _ in 0..20 {
            lockout.fail(now);
        }
        assert_eq!(lockout.remaining(now), Some(MAX_LOCK));
        lockout.succeed();
        assert_eq!(lockout.remaining(now), None);
    }
}
