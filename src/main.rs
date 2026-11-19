use std::io::Write;
use std::time::Duration;

use clap::Parser;

use portable_opentunnel::cli::{Cli, Command};
use portable_opentunnel::directory::Selection;
use portable_opentunnel::error::{self, AppError};
use portable_opentunnel::forward::{self, Completed, Run};
use portable_opentunnel::{remove, signals, trust, url};

fn main() {
    // Process-wide handlers first: a signal during provisioning, attachment,
    // cleanup, or a hung exit must be noticed and, on the second one, forced.
    signals::install(true);

    match execute(&Cli::parse().command) {
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

fn execute(command: &Command) -> Result<Completed, AppError> {
    let (state_dir, args, target) = match command {
        Command::Url(state) => {
            let found = url::run(&state.directory)?;
            write_stdout(&found)?;
            return Ok(Completed {
                url: found,
                warnings: Vec::new(),
            });
        }
        Command::Delete {
            state,
            network,
            shutdown,
        } => {
            let trust = trust::Trust::resolve(network.ca_cert.clone())?;
            return remove::run(
                &state.directory,
                network.api.as_deref(),
                trust.ca_cert.as_deref(),
                Duration::from_secs(shutdown.shutdown_timeout),
                network.verbose,
            );
        }
        Command::Up { state, forward } => (
            Selection::Reusable(&state.directory),
            &forward.run,
            Some(&forward.target),
        ),
        Command::Ephemeral(forward) => (Selection::Ephemeral, &forward.run, Some(&forward.target)),
        Command::Init { state, run } => (Selection::Reusable(&state.directory), run, None),
    };

    let trust = trust::Trust::resolve(args.network.ca_cert.clone())?;
    let run = Run {
        state_dir,
        api_override: args.network.api.as_deref(),
        ca_cert: trust.ca_cert.as_deref(),
        startup_timeout: Duration::from_secs(args.startup_timeout),
        shutdown_timeout: Duration::from_secs(args.shutdown.shutdown_timeout),
        verbose: args.network.verbose,
    };
    if let Some(target) = target {
        return forward::forward(&run, target);
    }
    let completed = forward::init(&run)?;
    write_stdout(&completed.url)?;
    Ok(completed)
}
