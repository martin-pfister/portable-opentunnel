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
//! The forward runtime is a current-thread runtime created and consumed here,
//! so once `block_on` returns every SDK task is suspended and cannot race
//! local deletion. Remote deletion uses a separate fresh runtime.

use std::cell::Cell;
use std::future::poll_fn;
use std::path::{Path, PathBuf};
use std::task::Poll;
use std::time::{Duration, Instant};

use opentunnel::{Client, Identity, ProvisionStage, Route, Routes, State, Tunnel};

use crate::bridge::{self, Provisioned};
use crate::budget::Budget;
use crate::cleanup;
use crate::cli::Target;
use crate::directory;
use crate::error::{AppError, AppResult};
use crate::layout;
use crate::remote;
use crate::signals::{Sig, Signals};
use crate::state::{self, Classification, DEFAULT_API, Mode, StateLock, WrapperMeta};

/// Everything an invocation needs beyond its action.
pub struct Run<'a> {
    pub state_dir: directory::Selection<'a>,
    pub api_override: Option<&'a str>,
    pub ca_cert: Option<&'a Path>,
    pub startup_timeout: Duration,
    pub shutdown_timeout: Duration,
    pub verbose: u8,
}

impl Run<'_> {
    fn log(&self, message: impl AsRef<str>) {
        if self.verbose > 0 {
            eprintln!("portable-opentunnel: {}", message.as_ref());
        }
    }
}

/// What a completed invocation produced.
#[derive(Debug, Clone, Default)]
pub struct Completed {
    pub url: String,
    pub warnings: Vec<String>,
}

/// `init`: ensure the identity and route, print the URL, never forward.
pub fn init(run: &Run<'_>) -> AppResult<Completed> {
    let mut state = Prepared::open(run)?;
    let budget = Budget::new(run.shutdown_timeout);
    let result = (|| -> AppResult<Identity> {
        let runtime = forwarding_runtime()?;
        let outcome = runtime.block_on(provision_and_finish(&mut state, run, &budget));
        runtime.shutdown_background();
        outcome
    })();
    match result {
        Ok(identity) => Ok(Completed {
            url: state.url(&identity),
            warnings: Vec::new(),
        }),
        Err(error) => Err(state.fail_with_cleanup(run, &budget, error)),
    }
}

/// Forward `target` until a signal or a terminal error.
pub fn forward(run: &Run<'_>, target: &Target) -> AppResult<Completed> {
    let mut state = Prepared::open(run)?;
    let routes = routes_for(&state.meta, target);
    let budget = Budget::new(run.shutdown_timeout);

    let runtime = forwarding_runtime()?;
    let outcome = runtime.block_on(provision_attach_forward(&mut state, run, routes, &budget));
    // The tunnel and client are dropped inside `block_on`; the runtime's
    // remaining tasks are suspended from here on.
    runtime.shutdown_background();

    match outcome {
        Ok(Forwarded::Signalled(sig, url)) => {
            run.log(format!("{} received; stopping", sig.name()));
            let warnings = state.finish(run, &budget)?;
            Ok(Completed { url, warnings })
        }
        Ok(Forwarded::Terminal(error)) => {
            let error = AppError::fail(format!("the tunnel stopped: {error}"));
            Err(state.fail_with_cleanup(run, &budget, error))
        }
        Err(error) => Err(state.fail_with_cleanup(run, &budget, error)),
    }
}

fn routes_for(meta: &WrapperMeta, target: &Target) -> Routes {
    let mut routes = Routes::new();
    routes.insert(meta.route.clone(), Route::new(target.to_route_target()));
    routes
}

fn forwarding_runtime() -> AppResult<tokio::runtime::Runtime> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| AppError::fail(format!("creating the forwarding runtime: {error}")))
}

// ---------------------------------------------------------------------------
// Prepared state
// ---------------------------------------------------------------------------

/// A state directory opened for modification: created, validated, locked,
/// and carrying committed wrapper metadata.
#[derive(Debug)]
struct Prepared {
    root: PathBuf,
    meta: WrapperMeta,
    lock: StateLock,
}

fn resolve_api(recorded: Option<&str>, requested: Option<&str>) -> AppResult<String> {
    match (recorded, requested) {
        (Some(recorded), Some(requested)) if recorded != requested => Err(AppError::fail(format!(
            "the state directory was created for {recorded}, but --api {requested} was given; \
             a saved identity keeps the endpoint it was created with"
        ))),
        (_, Some(requested)) => Ok(requested.to_owned()),
        (Some(recorded), None) => Ok(recorded.to_owned()),
        (None, None) => Ok(DEFAULT_API.to_owned()),
    }
}

pub fn random_route() -> String {
    // 12 characters from a lowercase-alphanumeric alphabet is a valid route
    // label, and the same generator the hosted server's ids resemble.
    crate::random::alphanumeric(12)
}

impl Prepared {
    fn open(run: &Run<'_>) -> AppResult<Self> {
        let mode = run.state_dir.mode();
        let root = directory::prepare(run.state_dir)?;
        state::check_private_dir(&root)?;
        if mode == Mode::Ephemeral {
            run.log(format!(
                "using temporary state directory {}",
                root.display()
            ));
        }

        // Classify after the lock, so a concurrent process cannot change the
        // outcome between the decision and the first write.
        let lock = StateLock::acquire(&root, true)?;
        lock.validate()?;
        let classification = state::classify(&root)?;

        let (meta, residue) = match classification {
            Classification::FirstRun { residue } => {
                let api = resolve_api(None, run.api_override)?;
                let meta = WrapperMeta::new(mode, api, random_route());
                lock.validate()?;
                // The route is committed before any SDK write, so an
                // interruption can never leave an identity without its route.
                state::write_wrapper(&root, &meta)?;
                (meta, residue)
            }
            Classification::Resumable { meta } => {
                let meta = adopt_meta(meta, mode, run.api_override)?;
                lock.validate()?;
                state::write_wrapper(&root, &meta)?;
                (meta, Vec::new())
            }
            Classification::Initialized { meta, .. } => {
                let meta = adopt_meta(meta, mode, run.api_override)?;
                if meta.api != DEFAULT_API || run.api_override.is_some() {
                    lock.validate()?;
                    state::write_wrapper(&root, &meta)?;
                }
                (meta, Vec::new())
            }
            // Never reprovision over a half-deleted directory: its remote
            // registration is already gone, so a new one would be a different
            // identity behind the same route. Finish the deletion instead.
            Classification::Deleting { .. } => {
                return Err(AppError::fail(format!(
                    "{} holds an identity whose remote registration was already deleted, but \
                     local cleanup did not finish. Finish it with: {}",
                    root.display(),
                    crate::cleanup::recovery_command(&root)
                )));
            }
            Classification::Corrupt { reason } => {
                return Err(AppError::fail(format!(
                    "{reason}. The credentials were preserved for manual recovery"
                )));
            }
            Classification::Unrelated { reason } => {
                return Err(AppError::fail(format!(
                    "{reason}. Choose another --dir, or remove that content yourself"
                )));
            }
        };

        // Recognized owned residue is discarded only now: holding the lock,
        // with the final metadata file in place.
        for path in residue {
            if state::safe_to_remove(&path) {
                run.log(format!("discarding incomplete file {}", path.display()));
                let _ = std::fs::remove_file(&path);
            }
        }

        Ok(Self { root, meta, lock })
    }

    fn sdk_client(&self, ca_cert: Option<&Path>) -> AppResult<Client> {
        bridge::client(&self.meta.api, &self.root, ca_cert)
    }

    fn url(&self, identity: &Identity) -> String {
        state::route_url(&self.meta.route, &identity.hostname)
    }

    /// Stops the invocation and, in ephemeral mode, deletes the identity.
    fn finish(&mut self, run: &Run<'_>, budget: &Budget) -> AppResult<Vec<String>> {
        if self.meta.mode == Mode::Reusable {
            run.log(format!(
                "identity kept in {}; the URL stays the same next time",
                self.root.display()
            ));
            return Ok(Vec::new());
        }
        // The budget was armed when shutdown began, so this shares whatever is
        // left with the earlier grace and tunnel close.
        if budget.expired() {
            return Ok(warnings_with_recovery(
                &format!(
                    "the {} second shutdown budget was exhausted before the registration was deleted",
                    budget.timeout().as_secs()
                ),
                &self.root,
            ));
        }

        self.lock.validate()?;
        let outcome = remote::delete_saved_identity(remote::Request {
            api: &self.meta.api,
            state_root: &self.root,
            ca_cert: run.ca_cert,
            deadline: budget.arm(),
        });
        match outcome {
            Ok(outcome) if outcome.may_remove_local() => {
                run.log(outcome.describe());
            }
            Ok(outcome) => {
                return Ok(warnings_with_recovery(outcome.describe(), &self.root));
            }
            Err(error) => {
                return Ok(warnings_with_recovery(&error.message, &self.root));
            }
        }

        self.finish_local(budget)
    }

    /// Records a settled remote result even if no time remains to unlink files.
    fn finish_local(&mut self, budget: &Budget) -> AppResult<Vec<String>> {
        self.lock.validate()?;
        if let Err(error) = state::mark_remote_deleted(&self.root, &self.meta) {
            return Ok(warnings_with_recovery(
                &format!(
                    "{}; the remote registration is gone but the deletion could not be \
                     recorded, so no local file was removed",
                    error.message
                ),
                &self.root,
            ));
        }

        if budget.expired() {
            return Ok(warnings_with_recovery(
                &format!(
                    "the {} second shutdown budget ran out before local cleanup; the remote \
                     registration is gone and the directory is marked for resumption",
                    budget.timeout().as_secs()
                ),
                &self.root,
            ));
        }

        let removal = cleanup::remove_local(&mut self.lock)?;
        if !removal.failures.is_empty() {
            return Ok(vec![format!(
                "{}; retry with: {}",
                removal.describe(),
                cleanup::recovery_command(&self.root)
            )]);
        }
        Ok(if removal.root_removed {
            Vec::new()
        } else {
            vec![removal.describe()]
        })
    }

    /// Combines an earlier failure with cleanup diagnostics.
    fn fail_with_cleanup(&mut self, run: &Run<'_>, budget: &Budget, error: AppError) -> AppError {
        let mut messages = vec![error.message];
        match self.finish(run, budget) {
            Ok(warnings) => messages.extend(warnings),
            Err(cleanup_error) => messages.push(format!("cleanup also failed: {cleanup_error}")),
        }
        AppError {
            code: error.code,
            message: messages.join("; "),
        }
    }
}

/// Adds the recovery command to a cleanup problem.
fn warnings_with_recovery(problem: &str, root: &Path) -> Vec<String> {
    vec![format!(
        "{problem}. Credentials were kept at {}; retry with: {}",
        root.display(),
        cleanup::recovery_command(root)
    )]
}

/// Applies the mode and endpoint rules to metadata that already exists.
fn adopt_meta(
    mut meta: WrapperMeta,
    mode: Mode,
    api_override: Option<&str>,
) -> AppResult<WrapperMeta> {
    if meta.mode != mode {
        return Err(AppError::fail(format!(
            "the state directory holds a {} identity; refusing to reuse it as {}",
            meta.mode.as_str(),
            mode.as_str()
        )));
    }
    meta.api = resolve_api(Some(&meta.api), api_override)?;
    Ok(meta)
}

// ---------------------------------------------------------------------------
// Provisioning
// ---------------------------------------------------------------------------

/// Ensures a complete identity exists, bounded by the startup deadline and
/// interruptible by signals.
async fn provision_and_finish(
    state: &mut Prepared,
    run: &Run<'_>,
    budget: &Budget,
) -> AppResult<Identity> {
    let client = state.sdk_client(run.ca_cert)?;
    let mut listener = Signals::new()
        .map_err(|error| AppError::fail(format!("installing signal handlers: {error}")))?;
    provision_bounded(&client, run, &mut listener, budget, startup_deadline(run)).await
}

/// The one deadline that bounds provisioning *and* attachment.
///
/// Created once per invocation, before either phase starts, so a slow
/// certificate issuance cannot be followed by a fresh full attach timeout.
fn startup_deadline(run: &Run<'_>) -> Instant {
    Instant::now()
        .checked_add(run.startup_timeout)
        .unwrap_or_else(Instant::now)
}

/// How much of the startup deadline is left, or zero if it has passed.
fn remaining_startup(deadline: Instant) -> Duration {
    deadline
        .checked_duration_since(Instant::now())
        .unwrap_or(Duration::ZERO)
}

/// The first event of a provisioning attempt.
enum ProvisioningEvent {
    Settled(AppResult<Identity>),
    TimedOut,
    Signalled(Sig),
}

/// Provisions the identity, bounded by the startup deadline and interruptible
/// by a signal.
///
/// On timeout or signal, keep polling an in-flight create within the
/// shutdown budget so the SDK has a chance to save its pending credentials.
async fn provision_bounded(
    client: &Client,
    run: &Run<'_>,
    listener: &mut Signals,
    budget: &Budget,
    startup_deadline: Instant,
) -> AppResult<Identity> {
    let inspected = bridge::inspect(client)?;
    run.log(match inspected {
        Provisioned::Complete(_) => "found a saved identity; checking it",
        Provisioned::Pending(_) => "found an interrupted provision; resuming it",
        Provisioned::Absent => "no saved identity; creating one",
    });

    let create_without_credentials = Cell::new(false);
    let progress = |stage| {
        match stage {
            ProvisionStage::CreatingTunnel => create_without_credentials.set(true),
            // The SDK calls this only after persisting the pending identity.
            ProvisionStage::RequestingCertificate => create_without_credentials.set(false),
            _ => {}
        }
        run.log(bridge::describe(stage));
    };

    let provisioning = async {
        match inspected {
            Provisioned::Complete(_) => client
                .ensure(layout::PROFILE)
                .await
                .map_err(|error| bridge::sdk_error("checking the saved identity", error)),
            Provisioned::Pending(_) | Provisioned::Absent => client
                .create(layout::PROFILE, progress)
                .await
                .map_err(|error| bridge::sdk_error("provisioning the tunnel", error)),
        }
    };
    let mut provisioning = std::pin::pin!(provisioning);
    // One deadline for the whole startup: provisioning *and* attachment. Each
    // phase must not get its own full timeout, or a slow provision plus a slow
    // attach could take twice the documented limit.
    let startup = tokio::time::sleep_until(tokio::time::Instant::from_std(startup_deadline));
    tokio::pin!(startup);

    let first = tokio::select! {
        result = &mut provisioning => ProvisioningEvent::Settled(result),
        _ = &mut startup => ProvisioningEvent::TimedOut,
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
    Signalled(Sig, String),
    Terminal(String),
}

async fn provision_attach_forward(
    state: &mut Prepared,
    run: &Run<'_>,
    routes: Routes,
    budget: &Budget,
) -> AppResult<Forwarded> {
    let client = state.sdk_client(run.ca_cert)?;
    let mut listener = Signals::new()
        .map_err(|error| AppError::fail(format!("installing signal handlers: {error}")))?;
    // One deadline for provisioning *and* attachment.
    let startup_deadline = startup_deadline(run);
    let identity = provision_bounded(&client, run, &mut listener, budget, startup_deadline).await?;
    // From here on, SDK-supplied text may be echoed to stderr, so the known
    // credentials are registered for redaction.
    let secrets = crate::redact::Secrets::from_identity(&identity);

    let tunnel = client
        .connect(layout::PROFILE, routes)
        .map_err(|error| bridge::sdk_error("starting the bridge", error))?;

    // Readiness: attachment, not merely a constructed tunnel. This continues
    // the startup deadline rather than starting a new one.
    let leftover = remaining_startup(startup_deadline);
    let startup = tokio::time::sleep(leftover);
    tokio::pin!(startup);
    let attached = tokio::select! {
        result = wait_attached(&tunnel, &secrets) => result,
        _ = &mut startup => Err(AppError::fail(if leftover.is_zero() {
            format!(
                "the {} second startup limit was reached before the bridge could attach",
                run.startup_timeout.as_secs()
            )
        } else {
            format!(
                "the bridge did not attach within the {} second startup limit",
                run.startup_timeout.as_secs()
            )
        })),
        sig = listener.wait() => Err(AppError::new(
            sig.exit_code(),
            format!("interrupted by {} while attaching", sig.name()),
        )),
    };
    let url = state.url(&identity);
    match attached {
        Ok(()) => {}
        Err(error) => {
            // Shutdown begins here; the budget covers close and cleanup.
            budget.arm();
            let _ = close_tunnel(tunnel, budget.remaining()).await;
            drop(client);
            return Err(error);
        }
    }

    run.log(format!("forwarding to the tunnel; {url} is ready"));
    write_url(&url)?;

    let end = supervise(&tunnel, &mut listener).await;
    // Shutdown begins now: one deadline covers close, runtime teardown, remote
    // deletion, and local cleanup.
    budget.arm();
    let close_error = close_tunnel(tunnel, budget.remaining()).await;
    drop(client);
    match end {
        Supervised::Signal(sig) => Ok(Forwarded::Signalled(sig, url)),
        Supervised::Stopped => match close_error {
            Some(error) => Ok(Forwarded::Terminal(secrets.scrub(&error))),
            None => Ok(Forwarded::Terminal("the tunnel task ended".to_owned())),
        },
    }
}

/// Waits for the first attached state, or a terminal stop.
async fn wait_attached(tunnel: &Tunnel, secrets: &crate::redact::Secrets) -> AppResult<()> {
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

fn write_url(url: &str) -> AppResult<()> {
    use std::io::Write;
    let stdout = std::io::stdout();
    let mut handle = stdout.lock();
    writeln!(handle, "{url}")
        .and_then(|()| handle.flush())
        .map_err(AppError::io("writing the URL to stdout"))
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::test_support::TempDir;

    fn run_for<'a>(dir: &'a Path, api: &'a str) -> Run<'a> {
        Run {
            state_dir: directory::Selection::Reusable(dir),
            api_override: Some(api),
            ca_cert: None,
            startup_timeout: Duration::from_secs(1),
            shutdown_timeout: Duration::from_secs(1),
            verbose: 0,
        }
    }

    fn save_ephemeral_meta(root: &Path) -> WrapperMeta {
        state::create_private_dir(root).unwrap();
        let meta = WrapperMeta::new(Mode::Ephemeral, "http://127.0.0.1:1".into(), "abc".into());
        state::write_wrapper(root, &meta).unwrap();
        meta
    }

    #[test]
    fn api_resolution_rejects_conflicts() {
        assert_eq!(resolve_api(None, None).unwrap(), DEFAULT_API);
        assert_eq!(resolve_api(None, Some("http://a")).unwrap(), "http://a");
        assert_eq!(resolve_api(Some("http://a"), None).unwrap(), "http://a");
        assert!(resolve_api(Some("http://a"), Some("http://b")).is_err());
    }

    #[test]
    fn random_routes_are_valid_and_distinct() {
        let first = random_route();
        let second = random_route();
        assert!(opentunnel::protocol::names::is_valid_route(&first));
        assert!(opentunnel::protocol::names::is_valid_route(&second));
        assert_ne!(first, second);
    }

    /// Provisioning and attachment must share one startup deadline. Each phase
    /// taking a fresh `startup_timeout` would allow twice the documented limit.
    #[test]
    fn the_startup_deadline_is_shared_and_never_extended() {
        let temp = TempDir::new();
        let dir = temp.path().join("state");
        let run = run_for(&dir, "http://127.0.0.1:1");
        let deadline = startup_deadline(&run);
        let first = remaining_startup(deadline);
        assert!(first <= run.startup_timeout);
        assert!(first > Duration::ZERO);

        // Simulate provisioning consuming most of the budget.
        std::thread::sleep(Duration::from_millis(120));
        let after_provisioning = remaining_startup(deadline);
        assert!(
            after_provisioning < first,
            "the attach phase must inherit the reduced budget: {after_provisioning:?} vs {first:?}"
        );
    }

    #[test]
    fn an_exhausted_startup_deadline_reports_zero() {
        let past = Instant::now() - Duration::from_secs(1);
        assert_eq!(remaining_startup(past), Duration::ZERO);
    }

    #[test]
    fn a_marked_deletion_is_refused_instead_of_reprovisioned() {
        let temp = TempDir::new();
        let dir = temp.path().join("state");
        let run = run_for(&dir, "http://127.0.0.1:1");
        {
            let prepared = Prepared::open(&run).unwrap();
            crate::state::mark_remote_deleted(&prepared.root, &prepared.meta).unwrap();
        }
        let error = Prepared::open(&run).unwrap_err();
        assert!(
            error.message.contains("already deleted"),
            "{}",
            error.message
        );
        assert!(
            error.message.contains("delete --dir"),
            "the message must name the recovery command: {}",
            error.message
        );
    }

    #[test]
    fn confirmed_deletion_is_recorded_even_after_the_shutdown_budget_expires() {
        let temp = TempDir::new();
        let root = temp.path().join("state");
        let mut prepared = Prepared {
            meta: save_ephemeral_meta(&root),
            root: root.clone(),
            lock: StateLock::acquire(&root, true).unwrap(),
        };
        let profile = layout::profile_dir(&root);
        std::fs::create_dir_all(&profile).unwrap();
        std::fs::write(
            profile.join(layout::TUNNEL),
            br#"{"hostname":"abc.opentunnel.xyz"}"#,
        )
        .unwrap();
        assert!(crate::url::run(&root).is_ok());

        let warnings = prepared.finish_local(&Budget::new(Duration::ZERO)).unwrap();
        assert!(warnings[0].contains("marked for resumption"));
        assert!(profile.join(layout::TUNNEL).exists());
        assert_eq!(
            crate::url::run(&root).unwrap_err().code,
            crate::error::NO_TUNNEL
        );

        drop(prepared);
        crate::remove::run(&root, None, None, Duration::ZERO, 0).unwrap();
        assert!(
            !root.exists(),
            "a marked deletion can finish without network"
        );
    }

    #[test]
    fn route_targets_are_scoped_to_the_saved_route() {
        let meta = WrapperMeta::new(Mode::Reusable, DEFAULT_API.into(), "abc".into());
        let target = Target {
            host: "127.0.0.1".into(),
            port: 3000,
        };
        let routes = routes_for(&meta, &target);
        assert_eq!(routes.len(), 1);
        let route = routes.get("abc").expect("the saved route is used");
        assert_eq!(route.target, "127.0.0.1:3000");
    }

    #[test]
    fn prepared_state_commits_metadata_before_provisioning() {
        let temp = TempDir::new();
        let dir = temp.path().join("state");
        let run = run_for(&dir, "http://127.0.0.1:1");
        let prepared = Prepared::open(&run).unwrap();
        assert!(prepared.root.join(layout::WRAPPER_FILE).exists());
        assert!(opentunnel::protocol::names::is_valid_route(
            &prepared.meta.route
        ));
        // The second run reuses the same route.
        let route = prepared.meta.route.clone();
        drop(prepared);
        let again = Prepared::open(&run).unwrap();
        assert_eq!(again.meta.route, route);
    }

    #[test]
    fn a_saved_route_survives_a_conflicting_endpoint() {
        let temp = TempDir::new();
        let dir = temp.path().join("state");
        {
            let run = run_for(&dir, "http://127.0.0.1:1");
            Prepared::open(&run).unwrap();
        }
        let other = run_for(&dir, "http://127.0.0.1:2");
        let error = Prepared::open(&other).unwrap_err();
        assert!(error.message.contains("--api"), "{}", error.message);
    }

    #[test]
    fn mode_cannot_change_for_an_existing_identity() {
        let temp = TempDir::new();
        let ephemeral = temp.path().join("ephemeral");
        save_ephemeral_meta(&ephemeral);
        let reusable = run_for(&ephemeral, "http://127.0.0.1:1");
        let error = Prepared::open(&reusable).unwrap_err();
        assert!(
            error.message.contains("refusing to reuse"),
            "{}",
            error.message
        );
    }

    #[test]
    fn unrelated_directory_is_refused() {
        let temp = TempDir::new();
        let dir = temp.path().join("state");
        std::fs::create_dir(&dir).unwrap();
        std::fs::set_permissions(
            &dir,
            <std::fs::Permissions as std::os::unix::fs::PermissionsExt>::from_mode(0o700),
        )
        .unwrap();
        std::fs::write(dir.join("notes.txt"), b"mine").unwrap();
        let run = run_for(&dir, "http://127.0.0.1:1");
        let error = Prepared::open(&run).unwrap_err();
        assert!(error.message.contains("not empty"), "{}", error.message);
    }

    #[test]
    fn a_concurrent_holder_blocks_the_second_instance() {
        let temp = TempDir::new();
        let dir = temp.path().join("state");
        let run = run_for(&dir, "http://127.0.0.1:1");
        let held = Prepared::open(&run).unwrap();
        let error = Prepared::open(&run).unwrap_err();
        assert!(
            error.message.contains("holds the lock"),
            "{}",
            error.message
        );
        drop(held);
    }
}
