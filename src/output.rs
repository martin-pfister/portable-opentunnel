//! The two output channels: stdout carries only the URL, and every diagnostic
//! goes to stderr.

use std::io::Write;

use crate::error::{AppError, AppResult};

/// Writes the URL line to stdout.
///
/// `println!` panics on a closed pipe, which would turn a benign condition
/// (`| head -1`) into exit 101. This reports it as an ordinary operational
/// failure instead, so the caller's cleanup path still runs.
pub fn print_url(url: &str) -> AppResult<()> {
    let mut stdout = std::io::stdout().lock();
    writeln!(stdout, "{url}")
        .and_then(|()| stdout.flush())
        .map_err(AppError::io("writing the URL to stdout"))
}

/// Writes one diagnostic line to stderr. A failing stderr is ignored: there is
/// nowhere left to report it.
pub fn diagnostic(message: impl AsRef<str>) {
    let _ = writeln!(
        std::io::stderr().lock(),
        "portable-opentunnel: {}",
        message.as_ref()
    );
}

/// Progress messages shown only with `--verbose`.
#[derive(Debug, Clone, Copy)]
pub struct Log {
    pub verbose: bool,
}

impl Log {
    pub fn info(self, message: impl AsRef<str>) {
        if self.verbose {
            diagnostic(message);
        }
    }
}
