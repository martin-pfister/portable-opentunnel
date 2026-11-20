mod budget;
mod cli;
mod delete;
mod error;
mod forward;
mod output;
mod random;
mod redact;
mod sdk;
mod signals;
mod state;
#[cfg(test)]
mod test_support;
mod trust;

use std::time::Duration;

use clap::Parser;

use crate::cli::{Cli, Command, RunArgs};
use crate::error::AppResult;
use crate::forward::Run;
use crate::state::directory::Selection;

fn main() {
    // Process-wide handlers first: a signal during provisioning, attachment,
    // cleanup, or a hung exit must be noticed and, on the second one, forced.
    signals::install();

    let code = match execute(Cli::parse().command) {
        Ok(warnings) => {
            for warning in warnings {
                output::diagnostic(format!("warning: {warning}"));
            }
            error::SUCCESS
        }
        Err(error) => {
            output::diagnostic(&error.message);
            error.code
        }
    };
    // A signal always decides the status, even after a clean shutdown.
    std::process::exit(signals::exit_code().unwrap_or(code));
}

/// Runs one command and returns warnings for stderr.
fn execute(command: Command) -> AppResult<Vec<String>> {
    match command {
        Command::Url(state) => {
            output::print_url(&state::saved_url(&state.directory)?)?;
            Ok(Vec::new())
        }
        Command::Init { state, run } => {
            let url = forward::init(&run_with(Selection::Reusable(&state.directory), &run)?)?;
            output::print_url(&url)?;
            Ok(Vec::new())
        }
        Command::Up {
            state,
            forward: args,
        } => forward::forward(
            &run_with(Selection::Reusable(&state.directory), &args.run)?,
            &args.target,
        ),
        Command::Ephemeral(args) => {
            forward::forward(&run_with(Selection::Ephemeral, &args.run)?, &args.target)
        }
        Command::Delete {
            state,
            network,
            shutdown,
        } => delete::run(
            &state.directory,
            network.api.as_deref(),
            trust::resolve_ca_cert(network.ca_cert.clone())?.as_deref(),
            Duration::from_secs(shutdown.shutdown_timeout),
            network.log(),
        ),
    }
}

fn run_with<'a>(state_dir: Selection<'a>, args: &'a RunArgs) -> AppResult<Run<'a>> {
    Ok(Run {
        state_dir,
        api_override: args.network.api.as_deref(),
        ca_cert: trust::resolve_ca_cert(args.network.ca_cert.clone())?,
        startup_timeout: Duration::from_secs(args.startup_timeout),
        shutdown_timeout: Duration::from_secs(args.shutdown.shutdown_timeout),
        log: args.network.log(),
    })
}
