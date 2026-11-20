//! Deleting an identity: the explicit `delete` command, and the same sequence
//! at the end of an ephemeral run.
//!
//! The order never changes: delete the remote registration, durably record
//! that it is gone, and only then remove local files. Any interruption leaves
//! a directory that the printed `delete` command can finish.

mod local;
mod remote;

use std::io;
use std::path::Path;
use std::time::Duration;

use crate::budget::Budget;
use crate::error::{AppError, AppResult};
use crate::output::Log;
use crate::state::{
    self, Classification, StateLock, WrapperMeta, directory, layout, recovery_command,
};

/// `delete`: refuses live, unowned, or unsafe state, then deletes the saved
/// identity. Returns warnings for stderr.
pub fn run(
    directory: &Path,
    api_override: Option<&str>,
    ca_cert: Option<&Path>,
    timeout: Duration,
    log: Log,
) -> AppResult<Vec<String>> {
    let root = directory::absolutize(directory);
    match std::fs::symlink_metadata(&root) {
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Ok(vec![format!(
                "{} does not exist; nothing to delete",
                root.display()
            )]);
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
            // Never write a lock into an unrecognized directory.
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
    // Decide again under the lock: the first look was unprotected.
    let Some((meta, note)) = delete_plan(&root)? else {
        return remove_local(&mut lock).map_err(AppError::fail);
    };
    meta.check_api(api_override)?;

    let mut warnings: Vec<String> = note.into_iter().map(str::to_owned).collect();
    warnings.extend(
        delete_identity(&mut lock, &meta, ca_cert, &Budget::new(timeout), log)
            .map_err(AppError::fail)?,
    );
    Ok(warnings)
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

/// Deletes the identity in a locked state directory within `budget`.
///
/// `Ok` carries notes about a finished deletion. `Err` explains what is left
/// and always names the retry command.
pub fn delete_identity(
    lock: &mut StateLock,
    meta: &WrapperMeta,
    ca_cert: Option<&Path>,
    budget: &Budget,
    log: Log,
) -> Result<Vec<String>, String> {
    // A marked directory must not contact the API again: its credentials may
    // already be partly gone.
    if !meta.remote_deleted {
        delete_remote(lock, meta, ca_cert, budget, log)?;
    }
    finish_locally(lock, meta, ca_cert, budget)
}

fn delete_remote(
    lock: &StateLock,
    meta: &WrapperMeta,
    ca_cert: Option<&Path>,
    budget: &Budget,
    log: Log,
) -> Result<(), String> {
    let root = lock.root();
    let kept = |problem: &str| kept_credentials(root, ca_cert, problem);
    if budget.expired() {
        return Err(kept(&format!(
            "the {} second shutdown budget was exhausted before the registration was deleted",
            budget.timeout().as_secs()
        )));
    }
    lock.validate().map_err(|error| kept(&error.message))?;
    let outcome = remote::delete_saved_identity(remote::Request {
        api: &meta.api,
        state_root: root,
        ca_cert,
        deadline: budget.arm(),
    })
    .map_err(|error| kept(&error.message))?;
    if !outcome.may_remove_local() {
        return Err(kept(outcome.describe()));
    }
    log.info(outcome.describe());
    Ok(())
}

/// After a settled remote deletion: records it, even when no budget is left,
/// then removes the local files if time remains.
fn finish_locally(
    lock: &mut StateLock,
    meta: &WrapperMeta,
    ca_cert: Option<&Path>,
    budget: &Budget,
) -> Result<Vec<String>, String> {
    let root = lock.root().to_path_buf();
    if !meta.remote_deleted {
        // Unrecorded, a retry asks the API again and settles on its 404.
        lock.validate()
            .and_then(|()| state::mark_remote_deleted(&root, meta))
            .map_err(|error| {
                kept_credentials(
                    &root,
                    ca_cert,
                    &format!(
                        "the tunnel registration was deleted, but the deletion could not be \
                         recorded ({})",
                        error.message
                    ),
                )
            })?;
    }
    if budget.expired() {
        return Err(format!(
            "the {} second shutdown budget ran out before local cleanup; the remote \
             registration is gone and the directory is marked for resumption. Finish it with: {}",
            budget.timeout().as_secs(),
            recovery_command(&root, None)
        ));
    }
    remove_local(lock).map_err(|problem| format!("the tunnel registration is gone, but {problem}"))
}

/// Removes the owned local files, reporting anything retained.
fn remove_local(lock: &mut StateLock) -> Result<Vec<String>, String> {
    let root = lock.root().to_path_buf();
    let failed = |problem: String| {
        format!(
            "local cleanup failed: {problem}. Retry with: {}",
            recovery_command(&root, None)
        )
    };
    let removal = local::remove(lock).map_err(|error| failed(error.message))?;
    if !removal.failures.is_empty() {
        return Err(failed(removal.describe()));
    }
    if let Some(problem) = removal.root_error {
        return Err(format!(
            "local cleanup failed: {problem}. Nothing owned is left; remove the empty directory \
             yourself"
        ));
    }
    Ok(if removal.root_removed {
        Vec::new()
    } else {
        vec![removal.describe()]
    })
}

/// A problem that left every credential in place, with the retry command.
fn kept_credentials(root: &Path, ca_cert: Option<&Path>, problem: &str) -> String {
    format!(
        "{}. Credentials were kept at {}; retry with: {}",
        problem.trim_end_matches('.'),
        root.display(),
        recovery_command(root, ca_cert)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    use crate::state::{DEFAULT_API, Mode, write_wrapper};
    use crate::test_support::TempDir;

    const QUIET: Log = Log { verbose: false };
    const SECOND: Duration = Duration::from_secs(1);

    fn delete(root: &Path, api: Option<&str>, timeout: Duration) -> AppResult<Vec<String>> {
        run(root, api, None, timeout, QUIET)
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
        // Private, so the ownership check is reached rather than the privacy one.
        fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
        fs::write(root.join("notes.txt"), b"keep me").unwrap();
        let error = delete(&root, None, SECOND).unwrap_err();
        assert!(error.message.contains("only deletes"), "{error}");
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
        let error = delete(&root, None, SECOND).unwrap_err();
        assert!(error.message.contains("no recognized state"), "{error}");
        assert_eq!(fs::read_dir(&root).unwrap().count(), 0);
    }

    #[test]
    fn an_inspection_error_is_not_a_missing_directory() {
        let temp = TempDir::new();
        let file = temp.path().join("file");
        fs::write(&file, b"not a directory").unwrap();
        let root = file.join("state");
        let error = delete(&root, None, SECOND).unwrap_err();
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
        state_at(&root, true, DEFAULT_API);
        fs::write(root.join(layout::WRAPPER_FILE), b"{bad").unwrap();
        let error = delete(&root, None, SECOND).unwrap_err();
        assert!(error.message.contains("refusing to delete"), "{error}");
        assert!(layout::profile_dir(&root).join(layout::TOKEN).exists());
    }

    #[test]
    fn refuses_a_live_run() {
        let temp = TempDir::new();
        let root = temp.path().join("state");
        state_at(&root, true, DEFAULT_API);
        let _held = StateLock::acquire(&root, true).unwrap();
        let error = delete(&root, None, SECOND).unwrap_err();
        assert!(error.message.contains("holds the lock"), "{error}");
    }

    #[test]
    fn refuses_a_conflicting_endpoint() {
        let temp = TempDir::new();
        let root = temp.path().join("state");
        state_at(&root, true, DEFAULT_API);
        let error = delete(&root, Some("http://127.0.0.1:9"), SECOND).unwrap_err();
        assert!(error.message.contains("created for"), "{error}");
    }

    #[test]
    fn missing_directory_is_harmless() {
        let temp = TempDir::new();
        let root = temp.path().join("absent");
        let warnings = delete(&root, None, SECOND).unwrap();
        assert!(warnings.iter().any(|w| w.contains("nothing to delete")));
        assert!(!root.exists());
    }

    #[test]
    fn first_run_residue_is_removed_locally() {
        let temp = TempDir::new();
        let root = temp.path().join("state");
        state::create_private_dir(&root).unwrap();
        let residue = "wrapper.json.tmp-1-abc";
        fs::write(root.join(residue), b"{}").unwrap();
        {
            let _failure = local::InjectedUnlinkFailure::arm(residue);
            let error = delete(&root, None, SECOND).unwrap_err();
            assert!(error.message.contains(residue), "{error}");
            assert!(root.join(residue).exists());
        }
        delete(&root, None, SECOND).unwrap();
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

        let result = delete(&root, None, SECOND);

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
        assert!(!error.message.contains("delete --dir"), "{error}");
        assert_eq!(fs::read_dir(&root).unwrap().count(), 0);
    }

    #[test]
    fn a_confirmed_deletion_is_recorded_even_without_budget_left() {
        let temp = TempDir::new();
        let root = temp.path().join("state");
        state_at(&root, true, "http://127.0.0.1:1");
        let meta = WrapperMeta::new(Mode::Reusable, "http://127.0.0.1:1".into(), "route".into());

        let mut lock = StateLock::acquire(&root, true).unwrap();
        let problem =
            finish_locally(&mut lock, &meta, None, &Budget::new(Duration::ZERO)).unwrap_err();
        assert!(problem.contains("marked for resumption"), "{problem}");
        assert!(layout::profile_dir(&root).join(layout::TOKEN).exists());
        assert!(matches!(
            state::classify(&root).unwrap(),
            Classification::Deleting { .. }
        ));
        drop(lock);

        delete(&root, None, SECOND).unwrap();
        assert!(!root.exists(), "a marked deletion finishes without network");
    }

    #[test]
    fn a_retry_that_needs_the_api_carries_the_ca() {
        let temp = TempDir::new();
        let root = temp.path().join("state");
        state_at(&root, true, "http://127.0.0.1:1");
        let meta = WrapperMeta::new(Mode::Reusable, "http://127.0.0.1:1".into(), "route".into());
        let ca = Path::new("/etc/my ca.pem");

        let mut lock = StateLock::acquire(&root, true).unwrap();
        let problem = delete_identity(
            &mut lock,
            &meta,
            Some(ca),
            &Budget::new(Duration::ZERO),
            QUIET,
        )
        .unwrap_err();
        assert!(problem.contains("Credentials were kept"), "{problem}");
        assert!(
            problem.contains(&recovery_command(&root, Some(ca))),
            "{problem}"
        );
    }

    #[test]
    fn an_unreachable_api_keeps_the_credentials() {
        let temp = TempDir::new();
        let root = temp.path().join("state");
        state_at(&root, true, "http://127.0.0.1:1");
        let error = delete(&root, None, Duration::from_millis(1)).unwrap_err();
        assert_kept_credentials(&root, &error.message);
        assert!(layout::profile_dir(&root).join(layout::TOKEN).exists());
    }

    /// Every remote failure, an HTTP error included, must say that the
    /// credentials were kept and how to retry, without leaking the token.
    ///
    /// The status itself varies with the environment (an outbound proxy may
    /// answer instead of the fixture), so only that contract is asserted.
    #[test]
    fn an_api_error_keeps_the_credentials_and_prints_the_retry() {
        const TOKEN: &str = "token-7c1d9e2b-should-never-be-printed";
        let api = failing_api(500, TOKEN);
        let temp = TempDir::new();
        let root = temp.path().join("state");
        state_at(&root, true, &api);
        fs::write(layout::profile_dir(&root).join(layout::TOKEN), TOKEN).unwrap();

        let error = delete(&root, None, Duration::from_secs(5)).unwrap_err();

        assert_kept_credentials(&root, &error.message);
        assert!(!error.message.contains(TOKEN), "the token leaked: {error}");
        assert_eq!(
            fs::read_to_string(layout::profile_dir(&root).join(layout::TOKEN)).unwrap(),
            TOKEN
        );
        let wrapper = fs::read_to_string(root.join(layout::WRAPPER_FILE)).unwrap();
        assert!(
            !wrapper.contains("\"remote_deleted\": true"),
            "a failed remote deletion must not be recorded as done: {wrapper}"
        );
        assert!(matches!(
            state::classify(&root).unwrap(),
            Classification::Initialized { .. }
        ));
    }

    fn assert_kept_credentials(root: &Path, message: &str) {
        assert!(message.contains("Credentials were kept at"), "{message}");
        assert!(message.contains(&root.display().to_string()), "{message}");
        assert!(message.contains(&recovery_command(root, None)), "{message}");
    }

    /// A loopback HTTP API that answers every request with `status`, echoing
    /// `echo` in the body as an untrusted endpoint could.
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
