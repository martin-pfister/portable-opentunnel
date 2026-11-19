//! The read-only `url` command.
//!
//! `url` is strictly local: it never provisions, renews, connects, deletes,
//! creates a missing directory, or takes the writer lock. It reads the wrapper
//! route and the SDK's `tunnel.json` hostname only — no credential file — and
//! prints the saved HTTPS URL. The SDK writes `tunnel.json` last by atomic
//! rename, so its presence is the completion marker and no extra
//! synchronization is needed.

use std::path::Path;

use crate::directory;
use crate::error::{AppError, AppResult};
use crate::state::{self, Classification};

/// Runs `url` and returns the URL to print, or an error carrying the exit
/// status: 3 when nothing is initialized, 1 for anything operational.
pub fn run(directory: &Path) -> AppResult<String> {
    let root = directory::absolutize(directory);
    let classification = state::classify(&root)?;
    match classification {
        Classification::Initialized { meta, hostname } => {
            Ok(state::route_url(&meta.route, &hostname))
        }
        Classification::FirstRun { .. } | Classification::Resumable { .. } => {
            Err(AppError::no_tunnel(not_initialized(&root)))
        }
        // The registration is gone; the saved URL would be a stale address.
        Classification::Deleting { .. } => Err(AppError::no_tunnel(format!(
            "The tunnel in {} was deleted; only local cleanup remains. Finish it with: {}",
            root.display(),
            crate::cleanup::recovery_command(&root)
        ))),
        Classification::Corrupt { reason } => Err(AppError::fail(reason)),
        Classification::Unrelated { reason } => Err(AppError::fail(reason)),
    }
}

fn not_initialized(root: &Path) -> String {
    format!("No tunnel initialized in {}.", root.display())
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::layout;
    use crate::state::{DEFAULT_API, Mode, WrapperMeta, write_wrapper};
    use crate::test_support::TempDir;
    use std::fs;

    fn initialized(root: &Path, route: &str, hostname: &str) {
        state::create_private_dir(root).unwrap();
        write_wrapper(
            root,
            &WrapperMeta::new(Mode::Reusable, DEFAULT_API.to_owned(), route.to_owned()),
        )
        .unwrap();
        let profile = layout::profile_dir(root);
        fs::create_dir_all(&profile).unwrap();
        for name in [layout::TOKEN, layout::PRIVATE_KEY, layout::CHAIN] {
            fs::write(profile.join(name), b"secret").unwrap();
        }
        fs::write(
            profile.join(layout::TUNNEL),
            format!(r#"{{"id":"abc","hostname":"{hostname}","certificateExpiry":"2099-01-01T00:00:00.000Z"}}"#),
        )
        .unwrap();
    }

    #[test]
    fn prints_the_saved_url() {
        let temp = TempDir::new();
        let root = temp.path().join("state");
        initialized(&root, "abc", "abc.opentunnel.xyz");
        assert_eq!(run(&root).unwrap(), "https://abc.abc.opentunnel.xyz");
    }

    #[test]
    fn missing_directory_is_not_initialized_and_is_not_created() {
        let temp = TempDir::new();
        let root = temp.path().join("absent");
        let error = run(&root).unwrap_err();
        assert_eq!(error.code, crate::error::NO_TUNNEL);
        assert!(error.message.starts_with("No tunnel initialized in"));
        assert!(!root.exists(), "url must not create the directory");
    }

    #[test]
    fn empty_directory_is_not_initialized() {
        let temp = TempDir::new();
        let root = temp.path().join("state");
        state::create_private_dir(&root).unwrap();
        assert_eq!(run(&root).unwrap_err().code, crate::error::NO_TUNNEL);
    }

    #[test]
    fn interrupted_first_run_stays_not_initialized_and_untouched() {
        let temp = TempDir::new();
        let root = temp.path().join("state");
        state::create_private_dir(&root).unwrap();
        fs::write(root.join("wrapper.json.tmp-1-abc"), b"{}").unwrap();
        let error = run(&root).unwrap_err();
        assert_eq!(error.code, crate::error::NO_TUNNEL);
        assert!(
            root.join("wrapper.json.tmp-1-abc").exists(),
            "url is read-only"
        );
    }

    #[test]
    fn metadata_without_completion_is_not_initialized() {
        let temp = TempDir::new();
        let root = temp.path().join("state");
        state::create_private_dir(&root).unwrap();
        write_wrapper(
            &root,
            &WrapperMeta::new(Mode::Reusable, DEFAULT_API.to_owned(), "abc".to_owned()),
        )
        .unwrap();
        assert_eq!(run(&root).unwrap_err().code, crate::error::NO_TUNNEL);
    }

    #[test]
    fn corrupt_state_is_an_operational_failure() {
        let temp = TempDir::new();
        let root = temp.path().join("state");
        state::create_private_dir(&root).unwrap();
        fs::write(root.join(layout::WRAPPER_FILE), b"{oops").unwrap();
        assert_eq!(run(&root).unwrap_err().code, crate::error::FAILURE);
    }

    #[test]
    fn sdk_only_state_is_an_operational_failure() {
        let temp = TempDir::new();
        let root = temp.path().join("state");
        let profile = layout::profile_dir(&root);
        fs::create_dir_all(&profile).unwrap();
        fs::write(profile.join(layout::TUNNEL), b"{}").unwrap();
        assert_eq!(run(&root).unwrap_err().code, crate::error::FAILURE);
    }

    #[test]
    fn a_directory_with_a_root_route_prints_the_bare_hostname() {
        let temp = TempDir::new();
        let root = temp.path().join("state");
        initialized(&root, "@", "abc.opentunnel.xyz");
        assert_eq!(run(&root).unwrap(), "https://abc.opentunnel.xyz");
    }
}
