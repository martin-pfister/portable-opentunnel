//! `init` and forwarding: prepare, provision, attach, announce, forward, clean up.
//!
//! Shape of one forwarding invocation:
//!
//! ```text
//! install signal handlers (main)
//! lock the state directory and commit wrapper metadata (route first)
//! provision or resume the identity
//! connect, wait for State::Connected
//! print the URL on stdout            <- readiness
//! forward until a signal or a terminal SDK error
//! close the tunnel, shut the forwarding runtime down
//! reusable: keep state    ephemeral: delete remotely, then locally
//! ```
//!
//! Provisioning and attachment share one startup deadline, so a slow
//! certificate issuance is never followed by a fresh full attach timeout.
//!
//! The forward runtime is a current-thread runtime created and consumed here,
//! so once `block_on` returns every SDK task is suspended and cannot race
//! local deletion. Remote deletion uses a separate fresh runtime.

use std::cell::Cell;
use std::future::{Future, poll_fn};
use std::path::PathBuf;
use std::task::Poll;
use std::time::{Duration, Instant};

use opentunnel::{Client, Identity, ProvisionStage, Route, Routes, State, Tunnel};

use crate::budget::{self, Budget};
use crate::cli::Target;
use crate::error::{AppError, AppResult};
use crate::output::{self, Log};
use crate::redact::Secrets;
use crate::sdk::{self, Saved};
use crate::signals::{Sig, Signals};
use crate::state::directory::Selection;
use crate::state::{self, Mode, Prepared, WrapperMeta, layout};

/// Everything an invocation needs beyond its action.
pub struct Run<'a> {
    pub state_dir: Selection<'a>,
    pub api_override: Option<&'a str>,
    pub ca_cert: Option<PathBuf>,
    pub startup_timeout: Duration,
    pub shutdown_timeout: Duration,
    pub log: Log,
}

impl Run<'_> {
    fn open(&self) -> AppResult<Prepared> {
        Prepared::open(self.state_dir, self.api_override, self.log)
    }

    fn client(&self, state: &Prepared) -> AppResult<Client> {
        sdk::client(&state.meta.api, &state.root, self.ca_cert.as_deref())
    }

    fn startup_deadline(&self) -> Instant {
        budget::deadline_after(self.startup_timeout)
    }
}

/// `init`: ensure the identity and route and return the URL, never forward.
pub fn init(run: &Run<'_>) -> AppResult<String> {
    let mut state = run.open()?;
    let budget = Budget::new(run.shutdown_timeout);
    let provisioned = block_on(async {
        let client = run.client(&state)?;
        let mut listener = listen()?;
        provision(&client, run, &mut listener, &budget, run.startup_deadline()).await
    });
    match provisioned {
        Ok(identity) => Ok(state::route_url(&state.meta.route, &identity.hostname)),
        Err(error) => Err(fail_with_cleanup(&mut state, run, &budget, error)),
    }
}

/// Forwards `target` until a signal or a terminal error. Returns warnings.
pub fn forward(run: &Run<'_>, target: &Target) -> AppResult<Vec<String>> {
    let mut state = run.open()?;
    let budget = Budget::new(run.shutdown_timeout);
    match block_on(provision_attach_forward(&state, run, target, &budget)) {
        Ok(Forwarded::Signalled(sig)) => {
            run.log.info(format!("{} received; stopping", sig.name()));
            Ok(finish(&mut state, run, &budget))
        }
        Ok(Forwarded::Terminal(error)) => {
            let error = AppError::fail(format!("the tunnel stopped: {error}"));
            Err(fail_with_cleanup(&mut state, run, &budget, error))
        }
        Err(error) => Err(fail_with_cleanup(&mut state, run, &budget, error)),
    }
}

/// Runs `future` on a fresh current-thread runtime and shuts it down, so no
/// SDK task outlives the call.
fn block_on<T>(future: impl Future<Output = AppResult<T>>) -> AppResult<T> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| AppError::fail(format!("creating the forwarding runtime: {error}")))?;
    let result = runtime.block_on(future);
    runtime.shutdown_background();
    result
}

fn listen() -> AppResult<Signals> {
    Signals::new().map_err(|error| AppError::fail(format!("installing signal handlers: {error}")))
}

/// Ends the invocation: reusable state is kept, ephemeral state is deleted.
/// Returns warnings about anything left behind.
fn finish(state: &mut Prepared, run: &Run<'_>, budget: &Budget) -> Vec<String> {
    if state.meta.mode == Mode::Reusable {
        run.log.info(format!(
            "identity kept in {}; the URL stays the same next time",
            state.root.display()
        ));
        return Vec::new();
    }
    crate::delete::delete_identity(
        &mut state.lock,
        &state.meta,
        run.ca_cert.as_deref(),
        budget,
        run.log,
    )
    .unwrap_or_else(|problem| vec![problem])
}

/// Appends cleanup diagnostics to an earlier failure.
fn fail_with_cleanup(
    state: &mut Prepared,
    run: &Run<'_>,
    budget: &Budget,
    error: AppError,
) -> AppError {
    let mut messages = vec![error.message];
    messages.extend(finish(state, run, budget));
    AppError::new(error.code, messages.join("; "))
}

// ---------------------------------------------------------------------------
// Provisioning
// ---------------------------------------------------------------------------

/// The first event of a provisioning attempt.
enum ProvisioningEvent {
    Settled(AppResult<Identity>),
    TimedOut,
    Signalled(Sig),
}

/// Ensures a complete identity exists, bounded by the startup deadline and
/// interruptible by a signal.
///
/// On timeout or signal, an in-flight create keeps being polled within the
/// shutdown budget, so the SDK has a chance to save its pending credentials.
async fn provision(
    client: &Client,
    run: &Run<'_>,
    listener: &mut Signals,
    budget: &Budget,
    startup_deadline: Instant,
) -> AppResult<Identity> {
    let saved = sdk::inspect(client)?;
    run.log.info(match saved {
        Saved::Complete => "found a saved identity; checking it",
        Saved::Pending => "found an interrupted provision; resuming it",
        Saved::Absent => "no saved identity; creating one",
    });

    let create_without_credentials = Cell::new(false);
    let progress = |stage| {
        match stage {
            ProvisionStage::CreatingTunnel => create_without_credentials.set(true),
            // The SDK calls this only after persisting the pending identity.
            ProvisionStage::RequestingCertificate => create_without_credentials.set(false),
            _ => {}
        }
        run.log.info(sdk::describe_stage(stage));
    };

    let provisioning = async {
        match saved {
            Saved::Complete => client
                .ensure(layout::PROFILE)
                .await
                .map_err(|error| sdk::sdk_error("checking the saved identity", error)),
            Saved::Pending | Saved::Absent => client
                .create(layout::PROFILE, progress)
                .await
                .map_err(|error| sdk::sdk_error("provisioning the tunnel", error)),
        }
    };
    let mut provisioning = std::pin::pin!(provisioning);

    let first = tokio::select! {
        result = &mut provisioning => ProvisioningEvent::Settled(result),
        () = tokio::time::sleep_until(startup_deadline.into()) => ProvisioningEvent::TimedOut,
        sig = listener.wait() => ProvisioningEvent::Signalled(sig),
    };

    let mut error = match first {
        ProvisioningEvent::Settled(result) => return result,
        ProvisioningEvent::TimedOut => AppError::fail(format!(
            "provisioning did not finish within the {} second startup limit",
            run.startup_timeout.as_secs()
        )),
        ProvisioningEvent::Signalled(sig) => AppError::new(
            sig.exit_code(),
            format!("interrupted by {} during provisioning", sig.name()),
        ),
    };
    let deadline = budget.arm();
    if create_without_credentials.get() {
        // Check progress after each poll of the same future, rather than
        // waiting for certificate issuance or polling on a periodic timer.
        let grace = poll_fn(|cx| {
            if provisioning.as_mut().poll(cx).is_ready() || !create_without_credentials.get() {
                Poll::Ready(())
            } else {
                Poll::Pending
            }
        });
        let _ = tokio::time::timeout_at(deadline.into(), grace).await;
        if create_without_credentials.get() {
            error.message.push_str(
                "; a remote registration may exist, but no credentials were saved to delete it",
            );
        }
    }
    Err(error)
}

// ---------------------------------------------------------------------------
// Forwarding
// ---------------------------------------------------------------------------

enum Forwarded {
    Signalled(Sig),
    Terminal(String),
}

async fn provision_attach_forward(
    state: &Prepared,
    run: &Run<'_>,
    target: &Target,
    budget: &Budget,
) -> AppResult<Forwarded> {
    let client = run.client(state)?;
    let mut listener = listen()?;
    let startup_deadline = run.startup_deadline();
    let identity = provision(&client, run, &mut listener, budget, startup_deadline).await?;
    // From here on, SDK-supplied text may be echoed to stderr.
    let secrets = Secrets::from_identity(&identity);

    let tunnel = client
        .connect(layout::PROFILE, routes_for(&state.meta, target))
        .map_err(|error| sdk::sdk_error("starting the bridge", error))?;

    // Readiness means attachment, not merely a constructed tunnel.
    let attached = tokio::select! {
        result = wait_attached(&tunnel, &secrets) => result,
        () = tokio::time::sleep_until(startup_deadline.into()) => Err(AppError::fail(format!(
            "the bridge did not attach within the {} second startup limit",
            run.startup_timeout.as_secs()
        ))),
        sig = listener.wait() => Err(AppError::new(
            sig.exit_code(),
            format!("interrupted by {} while attaching", sig.name()),
        )),
    };
    if let Err(error) = attached {
        let _ = close_tunnel(tunnel, budget.remaining()).await;
        return Err(error);
    }

    let url = state::route_url(&state.meta.route, &identity.hostname);
    run.log
        .info(format!("forwarding to the tunnel; {url} is ready"));
    output::print_url(&url)?;

    let end = supervise(&tunnel, &mut listener).await;
    // Shutdown begins now: one budget covers close, runtime teardown, remote
    // deletion, and local cleanup.
    let close_error = close_tunnel(tunnel, budget.remaining()).await;
    match end {
        Supervised::Signal(sig) => Ok(Forwarded::Signalled(sig)),
        Supervised::Stopped => Ok(Forwarded::Terminal(close_error.map_or_else(
            || "the tunnel task ended".to_owned(),
            |error| secrets.scrub(&error),
        ))),
    }
}

fn routes_for(meta: &WrapperMeta, target: &Target) -> Routes {
    let mut routes = Routes::new();
    routes.insert(meta.route.clone(), Route::new(target.to_route_target()));
    routes
}

/// Waits for the first attached state, or a terminal stop.
async fn wait_attached(tunnel: &Tunnel, secrets: &Secrets) -> AppResult<()> {
    let mut status = tunnel.status_watch();
    loop {
        let current = status.borrow_and_update().clone();
        match current.state {
            State::Connected => return Ok(()),
            State::Stopped => {
                return Err(AppError::fail(format!(
                    "the tunnel stopped before attaching{}",
                    current
                        .last_error
                        .map(|error| format!(": {}", secrets.scrub(&error)))
                        .unwrap_or_default()
                )));
            }
            State::WaitingRoutes | State::Connecting | State::Reconnecting => {}
        }
        if status.changed().await.is_err() {
            return Err(AppError::fail(
                "the tunnel task ended before attaching (the saved identity may be invalid)",
            ));
        }
    }
}

enum Supervised {
    Signal(Sig),
    /// The tunnel reached `State::Stopped` on its own.
    Stopped,
}

/// Watches status until a signal arrives or the tunnel stops by itself.
///
/// `Tunnel::wait()` is never raced against signals: it takes the task handle,
/// and cancelling it would detach the task. `status_watch()` is a plain watch
/// channel, safe to observe.
///
/// The current value is read *before* awaiting `changed()`: a freshly cloned
/// receiver marks the current value as seen, so `changed()` would never fire
/// for a tunnel that had already stopped. See `docs/sdk-upgrades.md`.
async fn supervise(tunnel: &Tunnel, listener: &mut Signals) -> Supervised {
    let mut status = tunnel.status_watch();
    loop {
        if status.borrow_and_update().state == State::Stopped {
            return Supervised::Stopped;
        }
        tokio::select! {
            changed = status.changed() => {
                if changed.is_err() {
                    return Supervised::Stopped;
                }
            }
            sig = listener.wait() => return Supervised::Signal(sig),
        }
    }
}

/// Closes the tunnel within the shutdown budget, returning any terminal error.
async fn close_tunnel(tunnel: Tunnel, budget: Duration) -> Option<String> {
    match tokio::time::timeout(budget, tunnel.close()).await {
        Ok(Ok(())) => None,
        Ok(Err(error)) => Some(error.to_string()),
        Err(_) => Some("closing the tunnel timed out".to_owned()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn route_targets_are_scoped_to_the_saved_route() {
        let meta = WrapperMeta::new(Mode::Reusable, state::DEFAULT_API.into(), "abc".into());
        let target = Target {
            host: "127.0.0.1".into(),
            port: 3000,
        };
        let routes = routes_for(&meta, &target);
        assert_eq!(routes.len(), 1);
        let route = routes.get("abc").expect("the saved route is used");
        assert_eq!(route.target, "127.0.0.1:3000");
    }
}
