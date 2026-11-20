//! Remote deletion of a tunnel registration.
//!
//! Deletion runs on a fresh current-thread runtime with a new SDK `Client`
//! built inside it: the forwarding runtime may be shutting down, and reusing
//! its connection pool, TLS state, or runtime-bound I/O would be unsound.
//!
//! This module only touches the network. The SDK's `Client::remove` is not
//! used: it skips the server call for a pending-only identity, and it removes
//! the profile recursively, destroying entries this program does not own.
//! Local removal is [`super::local`]'s job.

use std::path::Path;
use std::time::Instant;

use opentunnel::Error as SdkError;

use crate::error::{AppError, AppResult};
use crate::sdk;
use crate::state::layout;

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
            Outcome::Uncertain => "the deletion request's outcome is unknown",
        }
    }
}

/// A remote deletion request settled within one deadline.
pub struct Request<'a> {
    pub api: &'a str,
    pub state_root: &'a Path,
    pub ca_cert: Option<&'a Path>,
    pub deadline: Instant,
}

/// Requests deletion of the saved registration, if any, and nothing else.
///
/// Blocks on a private runtime and shuts it down before returning, so no SDK
/// task that can write credentials outlives the call.
pub fn delete_saved_identity(request: Request<'_>) -> AppResult<Outcome> {
    let Request {
        api,
        state_root,
        ca_cert,
        deadline,
    } = request;
    if deadline <= Instant::now() {
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
    deadline: Instant,
) -> AppResult<Outcome> {
    let client = sdk::client(api, state_root, ca_cert)?;

    let saved = client
        .get(layout::PROFILE)
        .map_err(|error| sdk::sdk_error("reading the saved identity", error))?;
    let credentials = match saved {
        Some(identity) => Some((identity.token, identity.id)),
        None => client
            .pending(layout::PROFILE)
            .map_err(|error| sdk::sdk_error("reading the pending identity", error))?
            .map(|pending| (pending.token, pending.id)),
    };
    let Some((token, id)) = credentials else {
        return Ok(Outcome::Nothing);
    };

    // Client setup and synchronous credential reads also use up the budget.
    if deadline <= Instant::now() {
        return Ok(Outcome::Uncertain);
    }
    match tokio::time::timeout_at(deadline.into(), client.api().remove(&token, &id)).await {
        Ok(Ok(())) => Ok(Outcome::Deleted),
        Ok(Err(error)) if is_not_found(&error) => Ok(Outcome::AlreadyAbsent),
        Ok(Err(error)) => Err(sdk::sdk_error("deleting the tunnel registration", error)),
        Err(_) => Ok(Outcome::Uncertain),
    }
}

/// Only a confirmed 404 means the registration is gone. Everything else,
/// 401/403 included, stays an error: a rejected token does not prove the tunnel
/// was removed, so no local file may be deleted on that inference.
fn is_not_found(error: &SdkError) -> bool {
    matches!(error, SdkError::Api { status: 404, .. })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn a_zero_deadline_is_uncertain_without_contacting_the_api() {
        let temp = crate::test_support::TempDir::new();
        let outcome = delete_saved_identity(Request {
            api: "http://127.0.0.1:1",
            state_root: temp.path(),
            ca_cert: None,
            deadline: Instant::now(),
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
            deadline: Instant::now() + Duration::from_millis(50),
        })
        .unwrap();
        assert_eq!(outcome, Outcome::Nothing);
    }

    #[test]
    fn only_a_404_settles_a_failed_request() {
        let api = |status| SdkError::Api {
            status,
            tag: String::new(),
            message: String::new(),
        };
        assert!(is_not_found(&api(404)));
        assert!(!is_not_found(&api(401)));
        assert!(!is_not_found(&api(500)));
        assert!(!is_not_found(&SdkError::Invalid("x".into())));
    }

    #[test]
    fn local_files_are_never_removed() {
        let temp = crate::test_support::TempDir::new();
        let root = temp.path().join("state");
        crate::state::create_private_dir(&root).unwrap();
        let profile = layout::profile_dir(&root);
        std::fs::create_dir_all(&profile).unwrap();
        for name in [layout::TOKEN, layout::TUNNEL] {
            std::fs::write(profile.join(name), b"secret").unwrap();
        }
        let _ = delete_saved_identity(Request {
            api: "http://127.0.0.1:1",
            state_root: &root,
            ca_cert: None,
            deadline: Instant::now() + Duration::from_millis(20),
        });
        assert!(profile.join(layout::TOKEN).exists());
        assert!(profile.join(layout::TUNNEL).exists());
    }
}
