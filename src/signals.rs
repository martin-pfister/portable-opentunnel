//! Process-wide termination handling.
//!
//! Two layers cooperate:
//!
//! * A raw `sigaction` handler records how many SIGINT/SIGTERM arrived and
//!   which kind came first. It stays active across runtime teardown and
//!   synchronous cleanup, where no async listener exists. A second signal
//!   forces `_exit`, so a stuck cleanup can always be escaped; a forced exit
//!   leaves recoverable state behind rather than pretending to have cleaned up.
//! * An async [`Signals`] listener, created on the current runtime, lets the
//!   supervisor notice the first signal and start bounded cleanup.
//!
//! Tokio installs its own handler for the process lifetime, so dropping a
//! runtime does not restore the default terminate action. The force-exit path
//! is what keeps a second signal effective after that transition.

use std::sync::atomic::{AtomicI32, AtomicUsize, Ordering};

use crate::error::{INTERRUPTED, TERMINATED};

static COUNT: AtomicUsize = AtomicUsize::new(0);
static FIRST: AtomicI32 = AtomicI32::new(0);

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

/// Records a signal and returns how many have arrived. `FIRST` is published
/// before `COUNT`, so a counted signal always has its kind available.
fn record(signal: libc::c_int) -> usize {
    let _ = FIRST.compare_exchange(0, signal, Ordering::SeqCst, Ordering::SeqCst);
    COUNT.fetch_add(1, Ordering::SeqCst) + 1
}

extern "C" fn handle(signal: libc::c_int) {
    if record(signal) >= 2 {
        let code = Sig::from_number(signal).map_or(TERMINATED, Sig::exit_code);
        // SAFETY: `_exit` is async-signal-safe and runs no destructors.
        unsafe { libc::_exit(code) };
    }
}

/// Installs the process-wide handlers. Must run before anything that a signal
/// should be able to interrupt.
pub fn install() {
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

/// The signal that started shutdown, if any.
pub fn first() -> Option<Sig> {
    Sig::from_number(FIRST.load(Ordering::SeqCst))
}

/// The exit status implied by the signals received so far.
pub fn exit_code() -> Option<i32> {
    first().map(Sig::exit_code)
}

/// An async listener for one runtime.
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

    /// Waits for the first SIGINT or SIGTERM.
    ///
    /// A signal that arrived before this listener was registered only reached
    /// the process-wide handler, so `recv()` would never report it and the
    /// caller would go on to provision or connect after shutdown was already
    /// requested. The recorded first signal covers that window.
    pub async fn wait(&mut self) -> Sig {
        if let Some(already) = first() {
            return already;
        }
        tokio::select! {
            _ = self.interrupt.recv() => Sig::Int,
            _ = self.terminate.recv() => Sig::Term,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::time::Duration;

    #[test]
    fn exit_codes_match_the_shell_convention() {
        assert_eq!(Sig::Int.exit_code(), 130);
        assert_eq!(Sig::Term.exit_code(), 143);
        assert_eq!(Sig::Int.name(), "SIGINT");
        assert_eq!(Sig::Term.name(), "SIGTERM");
    }

    /// The recorded state is process-global, and this is the only test that
    /// records a signal, so both properties are checked in one place.
    #[tokio::test]
    async fn the_first_recorded_signal_decides_and_is_seen_by_late_listeners() {
        let before = COUNT.load(Ordering::SeqCst);
        assert_eq!(record(libc::SIGTERM), before + 1);
        record(libc::SIGINT);
        assert_eq!(first(), Some(Sig::Term));
        assert_eq!(exit_code(), Some(TERMINATED));

        let mut signals = Signals::new().expect("registering signal handlers");
        let observed = tokio::time::timeout(Duration::from_millis(500), signals.wait())
            .await
            .expect("wait must not block when a signal was already recorded");
        assert_eq!(observed, Sig::Term);
    }
}
