//! Remote deletion of a tunnel registration.
//!
//! Deletion always runs on a **fresh** current-thread runtime with a **new**
//! SDK `Client`, constructed inside that runtime: the forwarding runtime may be
//! shutting down, and reusing its connection pool, TLS state, or runtime-bound
//! I/O would be unsound. That is also why local state is removed only after the
//! remote call is settled.
//!
//! This module only ever touches the network. It deliberately does **not** call
//! the SDK's `Storage::remove`, which recursively deletes the profile directory:
//! a profile may contain entries this program does not own (the state
//! classifier tolerates unknown files alongside a valid wrapper), and a
//! recursive delete would destroy them. Local removal stays in
//! [`crate::cleanup`], which deletes recognized entries one by one and retains
//! everything else.
//!
//! The pinned SDK's `Client::remove` skips the server call for a pending-only
//! identity and then removes the profile directory, so pending identities are
//! deleted explicitly through `client.api().remove` as well.

use std::path::Path;
use std::time::Duration;

use opentunnel::{Error as SdkError, Identity, PendingIdentity};

use crate::bridge::{self, client as build_client};
use crate::error::{AppError, AppResult};
use crate::layout;

/// What the remote deletion achieved.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// The server confirmed deletion.
    Deleted,
    /// The server answered 404: already gone, treated as success.
    AlreadyAbsent,
    /// Nothing to delete (no complete or pending identity).
    Nothing,
    /// The request may or may not have arrived; retry explicitly later.
    Uncertain,
}

impl Outcome {
    /// Whether the registration is known to be gone, so local cleanup may run.
    pub fn may_remove_local(self) -> bool {
        matches!(
            self,
            Outcome::Deleted | Outcome::AlreadyAbsent | Outcome::Nothing
        )
    }

    pub fn describe(self) -> &'static str {
        match self {
            Outcome::Deleted => "the tunnel registration was deleted",
            Outcome::AlreadyAbsent => "the tunnel registration was already absent",
            Outcome::Nothing => "no saved identity needed deletion",
            Outcome::Uncertain => {
                "the deletion request's outcome is unknown; credentials were kept so another \
                 attempt can confirm it"
            }
        }
    }
}

/// A remote deletion request settled within one deadline.
pub struct Request<'a> {
    pub api: &'a str,
    pub state_root: &'a Path,
    pub ca_cert: Option<&'a Path>,
    pub deadline: Duration,
}

/// Requests deletion of the saved registration, if any, and nothing else.
///
/// Runs blocking: it owns a private runtime and shuts it down before returning,
/// so no SDK task that can write credentials outlives the call. Unrelated
/// blocking work, such as a hostname lookup, may still outlive a background
/// shutdown; that is why local files are only ever deleted by
/// [`crate::cleanup`] after this has returned.
pub fn delete_saved_identity(request: Request<'_>) -> AppResult<Outcome> {
    let Request {
        api,
        state_root,
        ca_cert,
        deadline,
    } = request;
    if deadline.is_zero() {
        return Ok(Outcome::Uncertain);
    }

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| AppError::fail(format!("creating the cleanup runtime: {error}")))?;
    let result = runtime.block_on(remote_only(api, state_root, ca_cert, deadline));
    runtime.shutdown_background();
    result
}

/// The client is created here, inside the cleanup runtime that will drive it.
async fn remote_only(
    api: &str,
    state_root: &Path,
    ca_cert: Option<&Path>,
    deadline: Duration,
) -> AppResult<Outcome> {
    let client = build_client(api, state_root, ca_cert)?;

    // A complete identity is the normal case.
    let identity: Option<Identity> = client
        .get(layout::PROFILE)
        .map_err(|error| bridge::sdk_error("reading the saved identity", error))?;
    let pending: Option<PendingIdentity> = client
        .pending(layout::PROFILE)
        .map_err(|error| bridge::sdk_error("reading the pending identity", error))?;

    let Some((token, id)) = identity
        .as_ref()
        .map(|identity| (identity.token.clone(), identity.id.clone()))
        .or_else(|| {
            pending
                .as_ref()
                .map(|pending| (pending.token.clone(), pending.id.clone()))
        })
    else {
        return Ok(Outcome::Nothing);
    };

    match tokio::time::timeout(deadline, client.api().remove(&token, &id)).await {
        Ok(Ok(())) => Ok(Outcome::Deleted),
        // A confirmed 404 means the registration is gone already.
        Ok(Err(error)) => Ok(match outcome_for_error(status_of(&error)) {
            Some(outcome) => outcome,
            None => return Err(bridge::sdk_error("deleting the tunnel registration", error)),
        }),
        // A timeout is an uncertain outcome: keep credentials.
        Err(_) => Ok(Outcome::Uncertain),
    }
}

/// The HTTP status of an API error, if the failure was one.
fn status_of(error: &SdkError) -> u16 {
    match error {
        SdkError::Api { status, .. } => *status,
        _ => 0,
    }
}

/// How a non-success API status maps onto a deletion outcome.
///
/// Only a *confirmed* 404 means the registration is gone. Everything else —
/// 401/403 included, since a rejected token does not prove the tunnel was
/// removed — stays an error, so no local file is deleted on an inference.
pub fn outcome_for_error(status: u16) -> Option<Outcome> {
    (status == 404).then_some(Outcome::AlreadyAbsent)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn outcomes_gate_local_removal() {
        assert!(Outcome::Deleted.may_remove_local());
        assert!(Outcome::AlreadyAbsent.may_remove_local());
        assert!(Outcome::Nothing.may_remove_local());
        assert!(!Outcome::Uncertain.may_remove_local());
    }

    #[test]
    fn a_zero_deadline_is_uncertain_without_contacting_the_api() {
        let temp = crate::test_support::TempDir::new();
        let outcome = delete_saved_identity(Request {
            api: "http://127.0.0.1:1",
            state_root: temp.path(),
            ca_cert: None,
            deadline: Duration::ZERO,
        })
        .unwrap();
        assert_eq!(outcome, Outcome::Uncertain);
    }

    #[test]
    fn missing_identity_needs_no_request() {
        let temp = crate::test_support::TempDir::new();
        let root = temp.path().join("state");
        crate::state::create_private_dir(&root).unwrap();
        let outcome = delete_saved_identity(Request {
            api: "http://127.0.0.1:1",
            state_root: &root,
            ca_cert: None,
            deadline: Duration::from_millis(50),
        })
        .unwrap();
        assert_eq!(outcome, Outcome::Nothing);
    }

    #[test]
    fn a_successful_remote_delete_never_removes_local_files() {
        let temp = crate::test_support::TempDir::new();
        let root = temp.path().join("state");
        crate::state::create_private_dir(&root).unwrap();
        let profile = layout::profile_dir(&root);
        std::fs::create_dir_all(&profile).unwrap();
        for name in [layout::TOKEN, layout::TUNNEL] {
            std::fs::write(profile.join(name), b"secret").unwrap();
        }
        // No complete identity can be read, so this returns `Nothing`; the point
        // of the assertion is that no path in this module deletes files.
        let _ = delete_saved_identity(Request {
            api: "http://127.0.0.1:1",
            state_root: &root,
            ca_cert: None,
            deadline: Duration::from_millis(20),
        });
        assert!(profile.join(layout::TOKEN).exists());
        assert!(profile.join(layout::TUNNEL).exists());
    }
}
