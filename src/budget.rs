//! The single shutdown budget.
//!
//! Every phase of a shutdown shares one deadline: the provisioning grace, the
//! tunnel close, the runtime teardown, the remote deletion, and the local
//! cleanup. The deadline is armed exactly once, at the moment shutdown begins,
//! so each phase uses whatever time is left, and repeated `arm` calls can
//! never extend it.

use std::cell::Cell;
use std::time::{Duration, Instant};

/// Stands in for a timeout too large to add to the current time; it is also
/// what tokio itself uses as "never".
const FAR_FUTURE: Duration = Duration::from_secs(30 * 365 * 24 * 60 * 60);

/// The instant `timeout` from now. A timeout too large to represent means
/// "effectively never", not "already expired".
pub fn deadline_after(timeout: Duration) -> Instant {
    let now = Instant::now();
    now.checked_add(timeout)
        .or_else(|| now.checked_add(FAR_FUTURE))
        .unwrap_or(now)
}

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

    /// Starts the budget if it has not started yet and returns its deadline.
    ///
    /// Idempotent: the deadline is fixed by the first caller, which is the
    /// first shutdown trigger.
    pub fn arm(&self) -> Instant {
        match self.deadline.get() {
            Some(deadline) => deadline,
            None => {
                let armed = deadline_after(self.timeout);
                self.deadline.set(Some(armed));
                armed
            }
        }
    }

    /// Time left, arming the budget if this is the first call.
    pub fn remaining(&self) -> Duration {
        self.arm().saturating_duration_since(Instant::now())
    }

    pub fn expired(&self) -> bool {
        self.remaining().is_zero()
    }

    /// The configured total, for messages.
    pub fn timeout(&self) -> Duration {
        self.timeout
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
        assert_eq!(first, second);
        assert!(budget.expired());
    }

    #[test]
    fn a_zero_timeout_is_immediately_expired() {
        let budget = Budget::new(Duration::ZERO);
        assert!(budget.expired());
    }

    #[test]
    fn an_unrepresentable_timeout_never_expires() {
        let budget = Budget::new(Duration::from_secs(u64::MAX));
        assert!(budget.remaining() > Duration::from_secs(365 * 24 * 60 * 60));
        assert!(!budget.expired());
    }
}
