//! The explicit `delete` command.
//!
//! `delete` is the only way to remove a saved identity besides an ephemeral
//! run's own exit. It refuses live state, unowned state, and unsafe
//! directories. Remote deletion happens first; local files are removed only
//! after the remote outcome is confirmed or confirmed absent.

use std::io;
use std::path::Path;
use std::time::Duration;

use crate::budget::Budget;
use crate::cleanup;
use crate::directory;
use crate::error::{AppError, AppResult};
use crate::forward::Completed;
use crate::layout;
use crate::remote::{self, Outcome};
use crate::state::{self, Classification, StateLock, WrapperMeta};

pub fn run(
    directory: &Path,
    api_override: Option<&str>,
    ca_cert: Option<&Path>,
    deadline: Duration,
    verbose: u8,
) -> AppResult<Completed> {
    let root = directory::absolutize(directory);
    let mut warnings = Vec::new();
    match std::fs::symlink_metadata(&root) {
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            warnings.push(format!(
                "{} does not exist; nothing to delete",
                root.display()
            ));
            return Ok(Completed {
                url: String::new(),
                warnings,
            });
        }
        Err(error) => {
            return Err(AppError::io(format!("inspecting {}", root.display()))(
                error,
            ));
        }
    }

    state::check_private_dir(&root)?;
    let lock_path = root.join(layout::LOCK_FILE);
    let create_lock = match std::fs::symlink_metadata(&lock_path) {
        Ok(_) => false,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            // Never write a lock to an unrecognized directory. Recheck the
            // classification after locking before deleting anything.
            delete_plan(&root)?;
            true
        }
        Err(error) => {
            return Err(AppError::io(format!("inspecting {}", lock_path.display()))(
                error,
            ));
        }
    };
    let mut lock = StateLock::acquire(&root, create_lock)?;
    state::check_private_dir(&root)?;
    let Some((meta, mode_note)) = delete_plan(&root)? else {
        // Recognized residue, but no identity to delete remotely.
        let removal = cleanup::remove_local(&mut lock)?;
        if !removal.failures.is_empty() {
            return Err(AppError::fail(format!(
                "local cleanup failed: {}",
                removal.describe()
            )));
        }
        warnings.push(removal.describe());
        return Ok(Completed {
            url: String::new(),
            warnings,
        });
    };
    if let Some(note) = mode_note {
        warnings.push(note.to_owned());
    }

    lock.validate()?;
    // A marked directory means a previous deletion already removed the remote
    // registration; retrying must not contact the API (it could not: the
    // credentials may be partly gone).
    let outcome = if meta.remote_deleted {
        Outcome::AlreadyAbsent
    } else {
        let api = match api_override {
            Some(requested) if requested != meta.api => {
                return Err(AppError::fail(format!(
                    "the state was created for {}, but --api {requested} was given; delete with \
                     the recorded endpoint or omit --api",
                    meta.api
                )));
            }
            Some(requested) => requested.to_owned(),
            None => meta.api.clone(),
        };
        // Every failure path here is recoverable: the remote call either
        // settled (and only then may local files go) or it did not, in which
        // case the credentials are untouched and must be *reported* as such.
        // A bare `?` would drop the diagnostic for transport/HTTP failures
        // (e.g. a 500), leaving the user with an error that never names the
        // preserved state directory or the retry command.
        let outcome = match remote::delete_saved_identity(remote::Request {
            api: &api,
            state_root: &root,
            ca_cert,
            deadline: Budget::new(deadline).arm(),
        }) {
            Ok(outcome) => outcome,
            Err(error) => return Err(kept_credentials(&root, &error.message)),
        };
        if !outcome.may_remove_local() {
            return Err(kept_credentials(&root, outcome.describe()));
        }
        // Durably record the remote result *before* any credential is
        // unlinked, so an interruption here is still resumable. A failure here
        // leaves every credential in place.
        state::mark_remote_deleted(&root, &meta).map_err(|error| {
            kept_credentials(
                &root,
                &format!(
                    "the tunnel registration was deleted but the deletion could not be recorded \
                     ({})",
                    error.message
                ),
            )
        })?;
        outcome
    };
    if verbose > 0 {
        eprintln!("portable-opentunnel: {}", outcome.describe());
    }

    let removal = cleanup::remove_local(&mut lock).map_err(|error| {
        AppError::fail(format!(
            "the tunnel registration is gone but local cleanup failed: {error}"
        ))
    })?;
    if !removal.failures.is_empty() {
        return Err(AppError::fail(format!(
            "the tunnel registration was deleted but local cleanup failed: {}. Retry with: {}",
            removal.failures.join("; "),
            cleanup::recovery_command(&root)
        )));
    }
    if !removal.root_removed {
        warnings.push(removal.describe());
    }
    Ok(Completed {
        url: String::new(),
        warnings,
    })
}

/// An operational error that states plainly that nothing local was removed and
/// names the retry command.
fn kept_credentials(root: &Path, problem: &str) -> AppError {
    let problem = problem.trim_end_matches('.');
    AppError::fail(format!(
        "{problem}. Credentials were kept at {}; retry with: {}",
        root.display(),
        cleanup::recovery_command(root)
    ))
}

/// `None` means only recognized residue exists; no registration needs deleting.
fn delete_plan(root: &Path) -> AppResult<Option<(WrapperMeta, Option<&'static str>)>> {
    match state::classify(root)? {
        Classification::Initialized { meta, .. } => Ok(Some((meta, None))),
        Classification::Resumable { meta } => Ok(Some((
            meta,
            Some("the saved identity had not finished provisioning"),
        ))),
        Classification::Deleting { meta } => Ok(Some((
            meta,
            Some("finishing a deletion whose remote registration is already gone"),
        ))),
        Classification::Corrupt { reason } => Err(AppError::fail(format!(
            "refusing to delete: {reason}. The credentials were preserved for manual recovery"
        ))),
        Classification::Unrelated { reason } => Err(AppError::fail(format!(
            "refusing to delete: {reason}. This program only deletes recognized state"
        ))),
        Classification::FirstRun { residue } if residue.is_empty() => Err(AppError::fail(format!(
            "refusing to delete: {} contains no recognized state for this program to remove",
            root.display(),
        ))),
        Classification::FirstRun { .. } => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::state::{DEFAULT_API, Mode, WrapperMeta, write_wrapper};
    use crate::test_support::TempDir;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    fn state_with_profile(root: &Path, completed: bool) {
        state_at(root, completed, DEFAULT_API);
    }

    fn state_at(root: &Path, completed: bool, api: &str) {
        state::create_private_dir(root).unwrap();
        write_wrapper(
            root,
            &WrapperMeta::new(Mode::Reusable, api.to_owned(), "route".to_owned()),
        )
        .unwrap();
        let profile = layout::profile_dir(root);
        fs::create_dir_all(&profile).unwrap();
        for name in [
            layout::TOKEN,
            layout::PRIVATE_KEY,
            layout::CERTIFICATE,
            layout::CHAIN,
        ] {
            fs::write(profile.join(name), b"secret").unwrap();
        }
        if completed {
            fs::write(
                profile.join(layout::TUNNEL),
                br#"{"id":"abc","hostname":"abc.opentunnel.xyz","certificateExpiry":"2099-01-01T00:00:00.000Z"}"#,
            )
            .unwrap();
        } else {
            fs::write(
                profile.join(layout::PENDING),
                br#"{"id":"abc","hostname":"abc.opentunnel.xyz","csr":"CSR"}"#,
            )
            .unwrap();
        }
    }

    #[test]
    fn refuses_unrelated_directories() {
        let temp = TempDir::new();
        let root = temp.path().join("mine");
        fs::create_dir(&root).unwrap();
        // Reach the ownership check, not the privacy check: the directory is
        // private and ours, but its contents are not a state layout.
        fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
        fs::write(root.join("notes.txt"), b"keep me").unwrap();
        let error = run(&root, None, None, Duration::from_millis(10), 0).unwrap_err();
        assert!(error.message.contains("only deletes"), "{}", error.message);
        assert!(root.join("notes.txt").exists());
        assert_eq!(
            fs::read_dir(&root).unwrap().count(),
            1,
            "no lock was created"
        );
    }

    #[test]
    fn refuses_a_private_directory_with_no_state() {
        let temp = TempDir::new();
        let root = temp.path().join("mine");
        state::create_private_dir(&root).unwrap();

        let error = run(&root, None, None, Duration::from_secs(1), 0).unwrap_err();

        assert!(error.message.contains("no recognized state"), "{error}");
        assert_eq!(fs::read_dir(&root).unwrap().count(), 0);
    }

    #[test]
    fn an_inspection_error_is_not_a_missing_directory() {
        let temp = TempDir::new();
        let file = temp.path().join("file");
        fs::write(&file, b"not a directory").unwrap();
        let root = file.join("state");

        let error = run(&root, None, None, Duration::from_secs(1), 0).unwrap_err();

        assert_eq!(error.code, crate::error::FAILURE);
        assert!(
            error.message.contains(&root.display().to_string()),
            "{error}"
        );
    }

    #[test]
    fn refuses_corrupt_state() {
        let temp = TempDir::new();
        let root = temp.path().join("state");
        state_with_profile(&root, true);
        fs::write(root.join(layout::WRAPPER_FILE), b"{bad").unwrap();
        let error = run(&root, None, None, Duration::from_millis(10), 0).unwrap_err();
        assert!(
            error.message.contains("refusing to delete"),
            "{}",
            error.message
        );
        assert!(layout::profile_dir(&root).join(layout::TOKEN).exists());
    }

    #[test]
    fn refuses_a_live_run() {
        let temp = TempDir::new();
        let root = temp.path().join("state");
        state_with_profile(&root, true);
        let _held = StateLock::acquire(&root, true).unwrap();
        let error = run(&root, None, None, Duration::from_millis(10), 0).unwrap_err();
        assert!(
            error.message.contains("holds the lock"),
            "{}",
            error.message
        );
    }

    #[test]
    fn refuses_a_conflicting_endpoint() {
        let temp = TempDir::new();
        let root = temp.path().join("state");
        state_with_profile(&root, true);
        let error = run(
            &root,
            Some("http://127.0.0.1:9"),
            None,
            Duration::from_millis(10),
            0,
        )
        .unwrap_err();
        assert!(error.message.contains("created for"), "{}", error.message);
    }

    #[test]
    fn missing_directory_is_harmless() {
        let temp = TempDir::new();
        let root = temp.path().join("absent");
        let completed = run(&root, None, None, Duration::from_secs(1), 0).unwrap();
        assert!(
            completed
                .warnings
                .iter()
                .any(|w| w.contains("nothing to delete"))
        );
        assert!(!root.exists());
    }

    #[test]
    fn first_run_state_is_removed_locally() {
        let temp = TempDir::new();
        let root = temp.path().join("state");
        state::create_private_dir(&root).unwrap();
        let residue = "wrapper.json.tmp-1-abc";
        fs::write(root.join(residue), b"{}").unwrap();
        {
            let _failure = cleanup::InjectedUnlinkFailure::arm(residue);
            let error = run(&root, None, None, Duration::from_secs(1), 0).unwrap_err();
            assert!(error.message.contains(residue), "{error}");
            assert!(root.join(residue).exists());
        }
        run(&root, None, None, Duration::from_secs(1), 0).unwrap();
        assert!(!root.exists());
    }

    #[test]
    fn failure_to_remove_the_empty_root_is_reported() {
        if state::current_uid() == 0 {
            eprintln!("skipping rmdir permission check as root");
            return;
        }
        let temp = TempDir::new();
        let root = temp.path().join("state");
        state::create_private_dir(&root).unwrap();
        let meta = WrapperMeta::new(Mode::Reusable, DEFAULT_API.to_owned(), "route".to_owned());
        state::mark_remote_deleted(&root, &meta).unwrap();
        fs::set_permissions(temp.path(), fs::Permissions::from_mode(0o500)).unwrap();

        let result = run(&root, None, None, Duration::ZERO, 0);

        fs::set_permissions(temp.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let error = result.unwrap_err();
        assert_eq!(error.code, crate::error::FAILURE);
        assert!(
            error.message.contains("removing the empty state directory"),
            "{error}"
        );
        assert!(
            error.message.contains(&root.display().to_string()),
            "{error}"
        );
        assert_eq!(fs::read_dir(&root).unwrap().count(), 0);
    }

    #[test]
    fn an_unreachable_api_keeps_the_credentials() {
        let temp = TempDir::new();
        let root = temp.path().join("state");
        // The endpoint must match the recorded one, or `delete` refuses before
        // contacting anything and this would not exercise the remote path.
        state_at(&root, true, "http://127.0.0.1:1");
        let error = run(
            &root,
            Some("http://127.0.0.1:1"),
            None,
            Duration::from_millis(1),
            0,
        )
        .unwrap_err();
        assert_kept_credentials(&root, &error.message);
        assert!(layout::profile_dir(&root).join(layout::TOKEN).exists());
    }

    /// The reviewer's failure: an HTTP error (not just an uncertain outcome)
    /// short-circuited with `?`, so `delete` exited without ever saying that
    /// the credentials were preserved or how to retry. Every remote failure
    /// must produce that recoverable diagnostic.
    ///
    /// The status itself varies with the environment (an outbound proxy may
    /// answer instead of the fixture), so the assertion is on the contract:
    /// the diagnostic exists, names the directory and retry, and leaks nothing.
    #[test]
    fn an_api_error_keeps_the_credentials_and_prints_the_retry() {
        const TOKEN: &str = "token-7c1d9e2b-should-never-be-printed";
        let api = failing_api(500, TOKEN);
        let temp = TempDir::new();
        let root = temp.path().join("state");
        state_at(&root, true, &api);
        fs::write(layout::profile_dir(&root).join(layout::TOKEN), TOKEN).unwrap();

        let error = run(&root, Some(&api), None, Duration::from_secs(5), 0).unwrap_err();

        assert_kept_credentials(&root, &error.message);
        // A fixture (or hostile endpoint) echoing the request must not leak
        // the token into the diagnostic.
        assert!(
            !error.message.contains(TOKEN),
            "the token leaked: {}",
            error.message
        );
        // Nothing local was touched: the identity can still be deleted later.
        assert_eq!(
            fs::read_to_string(layout::profile_dir(&root).join(layout::TOKEN)).unwrap(),
            TOKEN
        );
        assert!(
            layout::profile_dir(&root)
                .join(layout::PRIVATE_KEY)
                .exists()
        );
        let wrapper = fs::read_to_string(root.join(layout::WRAPPER_FILE)).unwrap();
        assert!(
            !wrapper.contains("\"remote_deleted\": true"),
            "a failed remote deletion must not be recorded as done: {wrapper}"
        );
        // And the directory still classifies as a live identity, so the
        // printed retry command is actually usable.
        assert!(matches!(
            state::classify(&root).unwrap(),
            Classification::Initialized { .. }
        ));
    }

    /// A 404 is a settled outcome, not an error: it means the registration is
    /// already gone, so local cleanup may proceed. Asserted on the mapping
    /// itself, because a loopback 404 is not reliably reachable through an
    /// outbound proxy.
    #[test]
    fn a_404_settles_the_deletion() {
        assert_eq!(
            remote::outcome_for_error(404),
            Some(Outcome::AlreadyAbsent),
            "a confirmed 404 must count as an absent registration"
        );
        assert_eq!(remote::outcome_for_error(401), None);
        assert_eq!(remote::outcome_for_error(500), None);
        assert!(Outcome::AlreadyAbsent.may_remove_local());
    }

    fn assert_kept_credentials(root: &Path, message: &str) {
        assert!(
            message.contains("Credentials were kept at"),
            "the diagnostic must say the credentials were kept: {message}"
        );
        assert!(
            message.contains(&root.display().to_string()),
            "the diagnostic must name the state directory: {message}"
        );
        assert!(
            message.contains(&cleanup::recovery_command(root)),
            "the diagnostic must name the retry command: {message}"
        );
    }

    /// A loopback HTTP API that answers every request with `status`, echoing
    /// `echo` in the body (as an untrusted endpoint could). Serves until the
    /// returned handle is dropped.
    fn failing_api(status: u16, echo: &str) -> String {
        use std::io::{Read, Write};
        use std::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let echo = echo.to_owned();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { return };
                let mut buffer = [0u8; 4096];
                let _ = stream.read(&mut buffer);
                let body = format!("{{\"_tag\":\"Error\",\"message\":\"{echo}\"}}");
                let response = format!(
                    "HTTP/1.1 {status} Status\r\ncontent-type: application/json\r\ncontent-length: \
                     {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.write_all(response.as_bytes());
                let _ = stream.flush();
                let _ = stream.shutdown(std::net::Shutdown::Both);
            }
        });
        format!("http://{address}")
    }
}
