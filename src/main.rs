use std::io::Write;

use clap::Parser;

use portable_opentunnel::cli::{self, Action, Invocation};
use portable_opentunnel::error::{self, AppError};
use portable_opentunnel::forward::{self, Completed, Run};
use portable_opentunnel::{directory, remove, signals, trust, url};

fn main() {
    // Process-wide handlers first: a signal during provisioning, attachment,
    // cleanup, or a hung exit must be noticed and, on the second one, forced.
    signals::install(true);

    let cli = cli::Cli::parse();
    let invocation = match cli.resolve() {
        Ok(invocation) => invocation,
        Err(error) => exit_with(error),
    };
    match execute(&invocation) {
        Ok(completed) => {
            for warning in &completed.warnings {
                eprintln!("portable-opentunnel: warning: {warning}");
            }
            // A signal always decides the status, even after a clean shutdown.
            std::process::exit(signals::exit_code().unwrap_or(error::SUCCESS));
        }
        Err(error) => exit_with(error),
    }
}

fn exit_with(error: AppError) -> ! {
    // stdout stays clean: diagnostics and recovery instructions go to stderr.
    let stderr = std::io::stderr();
    let mut handle = stderr.lock();
    let _ = writeln!(handle, "portable-opentunnel: {}", error.message);
    let _ = handle.flush();
    std::process::exit(signals::exit_code().unwrap_or(error.code));
}

/// Writes the announced URL to stdout.
///
/// `println!` panics on a closed pipe, which would turn a benign condition
/// (`| head -1`) into exit 101. This reports it as an ordinary operational
/// failure instead, so the caller's cleanup path still runs.
fn write_stdout(url: &str) -> Result<(), AppError> {
    let stdout = std::io::stdout();
    let mut handle = stdout.lock();
    writeln!(handle, "{url}")
        .and_then(|()| handle.flush())
        .map_err(AppError::io("writing the URL to stdout"))
}

fn execute(invocation: &Invocation) -> Result<Completed, AppError> {
    match &invocation.action {
        // `url` is local and offline: it must not fail because a CA setting
        // for *network* operations is unusable.
        Action::Url => {
            let path = directory::require_explicit(invocation.directory.as_deref())?;
            let found = url::run(path)?;
            write_stdout(&found)?;
            Ok(Completed {
                url: found,
                warnings: Vec::new(),
            })
        }
        Action::Delete => {
            let path = directory::require_explicit(invocation.directory.as_deref())?;
            let trust = trust::Trust::resolve(invocation.ca_cert.clone())?;
            remove::run(
                path,
                invocation.api.as_deref(),
                trust.ca_cert.as_deref(),
                invocation.shutdown_timeout,
                invocation.verbose,
            )
        }
        action @ (Action::Init | Action::Forward(_)) => {
            let trust = trust::Trust::resolve(invocation.ca_cert.clone())?;
            let run = Run {
                state_dir: if invocation.ephemeral {
                    directory::Selection::Ephemeral
                } else {
                    directory::Selection::Reusable(directory::require_explicit(
                        invocation.directory.as_deref(),
                    )?)
                },
                api_override: invocation.api.as_deref(),
                ca_cert: trust.ca_cert.as_deref(),
                startup_timeout: invocation.startup_timeout,
                shutdown_timeout: invocation.shutdown_timeout,
                verbose: invocation.verbose,
            };
            if let Action::Forward(target) = action {
                return forward::forward(&run, target);
            }
            // `init` prints the URL only after the identity exists; it never
            // claims forwarding is ready.
            let completed = forward::init(&run)?;
            write_stdout(&completed.url)?;
            Ok(completed)
        }
    }
}
