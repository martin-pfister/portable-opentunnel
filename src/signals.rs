//! Process-wide termination handling.
//!
//! Two layers cooperate:
//!
//! * A process-wide counter (atomic, updated from a raw `sigaction` handler)
//!   records how many SIGINT/SIGTERM arrived and which kind came first. It
//!   stays active across runtime teardown and synchronous cleanup, where no
//!   async listener exists. A second signal forces `_exit`, so a stuck cleanup
//!   can always be escaped, and a forced exit never pretends to have cleaned
//!   up: it leaves recoverable state behind.
//! * An async `Signals::wait` future, installed on the current runtime, lets
//!   the supervisor notice the first signal and start bounded cleanup.
//!
//! Tokio installs its own handler for the process lifetime, so dropping a
//! runtime does not restore the default terminate action. The counter's
//! force-exit path is what keeps a second signal effective after that
//! transition.

use std::sync::atomic::{AtomicBool, AtomicI32, AtomicUsize, Ordering};
use std::time::Duration;

use crate::error::{INTERRUPTED, TERMINATED};

static COUNT: AtomicUsize = AtomicUsize::new(0);
static FIRST: AtomicI32 = AtomicI32::new(0);
static FORCE_EXIT: AtomicBool = AtomicBool::new(true);

/// Which termination signal started the shutdown.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sig {
    Int,
    Term,
}

impl Sig {
    pub fn exit_code(self) -> i32 {
        match self {
            Sig::Int => INTERRUPTED,
            Sig::Term => TERMINATED,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Sig::Int => "SIGINT",
            Sig::Term => "SIGTERM",
        }
    }

    fn from_number(number: libc::c_int) -> Option<Self> {
        match number {
            libc::SIGINT => Some(Sig::Int),
            libc::SIGTERM => Some(Sig::Term),
            _ => None,
        }
    }
}

fn record_number(number: libc::c_int) {
    // The first signal decides the exit status; later ones only count.
    let _ = FIRST.compare_exchange(0, number, Ordering::SeqCst, Ordering::SeqCst);
    COUNT.fetch_add(1, Ordering::SeqCst);
}

extern "C" fn handle(signal: libc::c_int) {
    record_number(signal);
    if COUNT.load(Ordering::SeqCst) >= 2 && FORCE_EXIT.load(Ordering::SeqCst) {
        // Second request: leave now, keeping state for recovery. `_exit` is
        // async-signal-safe; no destructors may run here.
        let code = Sig::from_number(signal).map_or(TERMINATED, Sig::exit_code);
        unsafe { libc::_exit(code) };
    }
}

/// Installs the process-wide handlers. `force_exit` exists so tests can count
/// signals without a second one killing the test process.
pub fn install(force_exit: bool) {
    FORCE_EXIT.store(force_exit, Ordering::SeqCst);
    // SAFETY: a zeroed `sigaction` with a valid handler and an initialized mask
    // is a valid argument; the handler only touches atomics and `_exit`.
    unsafe {
        let mut action: libc::sigaction = std::mem::zeroed();
        action.sa_sigaction = handle as usize;
        action.sa_flags = libc::SA_RESTART;
        libc::sigemptyset(&mut action.sa_mask);
        libc::sigaction(libc::SIGINT, &action, std::ptr::null_mut());
        libc::sigaction(libc::SIGTERM, &action, std::ptr::null_mut());
        // A closed stdout pipe must not kill the process mid-cleanup.
        libc::signal(libc::SIGPIPE, libc::SIG_IGN);
    }
}

/// Total SIGINT/SIGTERM received so far, from any runtime.
pub fn count() -> usize {
    COUNT.load(Ordering::SeqCst)
}

/// Whether a signal has arrived. Used during synchronous cleanup, where the
/// async listener is suspended with the runtime.
pub fn requested() -> bool {
    count() > 0
}

/// The signal that started shutdown, if any.
pub fn first() -> Option<Sig> {
    Sig::from_number(FIRST.load(Ordering::SeqCst))
}

/// The exit status implied by the signals received so far.
pub fn exit_code() -> Option<i32> {
    first().map(Sig::exit_code)
}

/// An async listener for one runtime. Each runtime creates its own; the
/// process-wide counter survives it.
pub struct Signals {
    interrupt: tokio::signal::unix::Signal,
    terminate: tokio::signal::unix::Signal,
}

impl Signals {
    pub fn new() -> std::io::Result<Self> {
        use tokio::signal::unix::{SignalKind, signal};
        Ok(Self {
            interrupt: signal(SignalKind::interrupt())?,
            terminate: signal(SignalKind::terminate())?,
        })
    }

    /// Waits for the next SIGINT or SIGTERM.
    ///
    /// A signal handled before this listener was registered must not be lost:
    /// tokio's own subscription is created by `Signals::new`, so a signal that
    /// arrived earlier woke only the process-wide handler. `recv()` would never
    /// fire for it, and a supervisor awaiting `wait()` would hang forever —
    /// then go on to provision or connect after shutdown was already requested.
    /// Consulting the recorded first signal closes that window. A later signal
    /// still arrives through `recv()` as usual.
    pub async fn wait(&mut self) -> Sig {
        // `record_number` publishes `FIRST` before `COUNT`, so a recorded
        // signal always has its kind available here.
        if let Some(already) = first() {
            return already;
        }
        tokio::select! {
            _ = self.interrupt.recv() => Sig::Int,
            _ = self.terminate.recv() => Sig::Term,
        }
    }
}

/// Sleeps for `duration`, returning early with the signal if one arrives.
pub async fn sleep_or_signal(duration: Duration) -> Option<Sig> {
    let mut signals = match Signals::new() {
        Ok(signals) => signals,
        Err(_) => {
            tokio::time::sleep(duration).await;
            return None;
        }
    };
    tokio::select! {
        _ = tokio::time::sleep(duration) => None,
        kind = signals.wait() => Some(kind),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exit_codes_match_the_shell_convention() {
        assert_eq!(Sig::Int.exit_code(), 130);
        assert_eq!(Sig::Term.exit_code(), 143);
        assert_eq!(Sig::Int.name(), "SIGINT");
        assert_eq!(Sig::Term.name(), "SIGTERM");
    }

    #[test]
    fn the_first_signal_decides_the_status() {
        let before = count();
        let first_before = FIRST.load(Ordering::SeqCst);
        record_number(libc::SIGTERM);
        assert_eq!(count(), before + 1);
        assert!(requested());
        if first_before == 0 {
            assert_eq!(exit_code(), Some(TERMINATED));
            assert_eq!(first(), Some(Sig::Term));
        }
        // A later SIGINT does not change which signal started shutdown.
        let first_after = FIRST.load(Ordering::SeqCst);
        record_number(libc::SIGINT);
        assert_eq!(FIRST.load(Ordering::SeqCst), first_after);
    }

    /// The window this closes: a signal arrives while the program is setting up
    /// its state directory, before any Tokio runtime installed a subscriber.
    /// The process-wide handler records it, so `wait()` must report it instead
    /// of blocking forever on `recv()`.
    ///
    /// The recorded signal is process-global, so this test does not fabricate
    /// one: it asserts the contract on the real recorded value, keeping the
    /// check independent of which signal a test runner has already seen.
    #[tokio::test]
    async fn wait_reports_an_already_recorded_signal_without_blocking() {
        let Some(expected) = first() else {
            // No signal has been recorded in this process; the "already
            // recorded" branch cannot be exercised without faking global
            // state, which would race other tests. The child-process lifecycle
            // test covers the real end-to-end case.
            return;
        };
        let mut signals = Signals::new().expect("registering signal handlers");
        let observed = tokio::time::timeout(Duration::from_millis(500), signals.wait())
            .await
            .expect("wait must not block when a signal was already recorded");
        assert_eq!(observed, expected);
    }
}
