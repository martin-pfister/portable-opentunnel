//! The single shutdown budget.
//!
//! Every phase of a shutdown shares one deadline: the provisioning grace, the
//! tunnel close, the runtime teardown, the remote deletion, and the local
//! cleanup. The deadline is armed exactly once, at the moment shutdown begins,
//! so a phase that finishes early does not hand its unused time to the next
//! one, and repeated `arm` calls can never extend it.

use std::cell::Cell;
use std::time::{Duration, Instant};

/// A deadline that starts on first use and never moves again.
#[derive(Debug)]
pub struct Budget {
    timeout: Duration,
    deadline: Cell<Option<Instant>>,
}

impl Budget {
    pub fn new(timeout: Duration) -> Self {
        Self {
            timeout,
            deadline: Cell::new(None),
        }
    }

    /// Starts the budget if it has not started yet and returns the time left.
    ///
    /// Idempotent: the deadline is fixed by the first caller, which is the
    /// first shutdown trigger.
    pub fn arm(&self) -> Duration {
        let deadline = match self.deadline.get() {
            Some(deadline) => deadline,
            None => {
                let armed = Instant::now()
                    .checked_add(self.timeout)
                    .unwrap_or_else(Instant::now);
                self.deadline.set(Some(armed));
                armed
            }
        };
        deadline
            .checked_duration_since(Instant::now())
            .unwrap_or(Duration::ZERO)
    }

    /// Time left, arming the budget if this is the first call.
    pub fn remaining(&self) -> Duration {
        self.arm()
    }

    pub fn expired(&self) -> bool {
        self.arm().is_zero()
    }

    /// The configured total, for messages.
    pub fn timeout(&self) -> Duration {
        self.timeout
    }

    /// Caps `other` by what is left of this budget.
    pub fn limit(&self, other: Duration) -> Duration {
        other.min(self.remaining())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_first_arm_sets_the_deadline() {
        let budget = Budget::new(Duration::from_secs(30));
        let first = budget.remaining();
        assert!(first <= Duration::from_secs(30));
        assert!(first > Duration::from_secs(25));
        assert!(!budget.expired());
    }

    #[test]
    fn arming_again_never_extends_it() {
        let budget = Budget::new(Duration::from_millis(50));
        let first = budget.arm();
        std::thread::sleep(Duration::from_millis(60));
        let second = budget.arm();
        assert!(first > Duration::ZERO);
        assert_eq!(second, Duration::ZERO);
        assert!(budget.expired());
    }

    #[test]
    fn a_zero_timeout_is_immediately_expired() {
        let budget = Budget::new(Duration::ZERO);
        assert!(budget.expired());
        assert_eq!(budget.limit(Duration::from_secs(5)), Duration::ZERO);
    }

    #[test]
    fn limit_caps_by_the_remaining_time() {
        let budget = Budget::new(Duration::from_secs(10));
        assert!(budget.limit(Duration::from_secs(1)) <= Duration::from_secs(1));
        assert!(budget.limit(Duration::from_secs(60)) <= Duration::from_secs(10));
    }
}
