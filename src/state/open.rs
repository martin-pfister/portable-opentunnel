//! Opening a state directory for `init` and forwarding.

use std::path::PathBuf;

use super::directory::{self, Selection};
use super::{Classification, DEFAULT_API, Mode, StateLock, WrapperMeta};
use crate::error::{AppError, AppResult};
use crate::output::Log;

/// Length of a generated route label: lowercase alphanumeric, so always valid.
const ROUTE_LENGTH: usize = 12;

/// A state directory opened for modification: private, locked, and carrying
/// committed wrapper metadata.
#[derive(Debug)]
pub struct Prepared {
    pub root: PathBuf,
    pub meta: WrapperMeta,
    pub lock: StateLock,
}

impl Prepared {
    pub fn open(selection: Selection<'_>, api_override: Option<&str>, log: Log) -> AppResult<Self> {
        let mode = selection.mode();
        let root = directory::prepare(selection)?;
        if mode == Mode::Ephemeral {
            log.info(format!(
                "using temporary state directory {}",
                root.display()
            ));
        }

        // Classify only while holding the lock, so a concurrent process cannot
        // change the outcome between the decision and the first write.
        let lock = StateLock::acquire(&root, true)?;
        let (meta, residue) = match super::classify(&root)? {
            Classification::FirstRun { residue } => {
                let api = api_override.unwrap_or(DEFAULT_API).to_owned();
                let route = crate::random::alphanumeric(ROUTE_LENGTH);
                let meta = WrapperMeta::new(mode, api, route);
                // The route is committed before any SDK write, so an
                // interruption can never leave an identity without its route.
                lock.validate()?;
                super::write_wrapper(&root, &meta)?;
                (meta, residue)
            }
            Classification::Resumable { meta } | Classification::Initialized { meta, .. } => {
                if meta.mode != mode {
                    return Err(AppError::fail(format!(
                        "the state directory holds a {} identity; refusing to reuse it as {}",
                        meta.mode.as_str(),
                        mode.as_str()
                    )));
                }
                meta.check_api(api_override)?;
                (meta, Vec::new())
            }
            // The remote registration is already gone, so provisioning again
            // would create a different identity behind the same route.
            Classification::Deleting { .. } => {
                return Err(AppError::fail(format!(
                    "{} holds an identity whose remote registration was already deleted, but \
                     local cleanup did not finish. Finish it with: {}",
                    root.display(),
                    super::recovery_command(&root, None)
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

        // A residue name never proves its writer is gone, so it is discarded
        // only now: holding the lock, with the final metadata in place.
        for path in residue {
            if super::private::safe_to_remove(&path) {
                log.info(format!("discarding incomplete file {}", path.display()));
                let _ = std::fs::remove_file(&path);
            }
        }

        Ok(Self { root, meta, lock })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::path::Path;

    use crate::state::{create_private_dir, layout, mark_remote_deleted, write_wrapper};
    use crate::test_support::TempDir;

    const API: &str = "http://127.0.0.1:1";
    const QUIET: Log = Log { verbose: false };

    fn open(dir: &Path, api: &str) -> AppResult<Prepared> {
        Prepared::open(Selection::Reusable(dir), Some(api), QUIET)
    }

    #[test]
    fn metadata_is_committed_before_provisioning_and_reused() {
        let temp = TempDir::new();
        let dir = temp.path().join("state");
        let prepared = open(&dir, API).unwrap();
        assert!(dir.join(layout::WRAPPER_FILE).exists());
        assert!(opentunnel::protocol::names::is_valid_route(
            &prepared.meta.route
        ));
        let route = prepared.meta.route.clone();
        drop(prepared);
        assert_eq!(open(&dir, API).unwrap().meta.route, route);
    }

    #[test]
    fn mode_cannot_change_for_an_existing_identity() {
        let temp = TempDir::new();
        let dir = temp.path().join("state");
        create_private_dir(&dir).unwrap();
        let meta = WrapperMeta::new(Mode::Ephemeral, API.into(), "abc".into());
        write_wrapper(&dir, &meta).unwrap();
        let error = open(&dir, API).unwrap_err();
        assert!(error.message.contains("refusing to reuse"), "{error}");
    }

    #[test]
    fn a_marked_deletion_is_refused_instead_of_reprovisioned() {
        let temp = TempDir::new();
        let dir = temp.path().join("state");
        let prepared = open(&dir, API).unwrap();
        mark_remote_deleted(&prepared.root, &prepared.meta).unwrap();
        drop(prepared);
        let error = open(&dir, API).unwrap_err();
        assert!(error.message.contains("already deleted"), "{error}");
        assert!(error.message.contains("delete --dir"), "{error}");
    }

    #[test]
    fn unrelated_directory_is_refused() {
        let temp = TempDir::new();
        let dir = temp.path().join("state");
        fs::create_dir(&dir).unwrap();
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).unwrap();
        fs::write(dir.join("notes.txt"), b"mine").unwrap();
        let error = open(&dir, API).unwrap_err();
        assert!(error.message.contains("not empty"), "{error}");
    }

    #[test]
    fn a_concurrent_holder_blocks_the_second_instance() {
        let temp = TempDir::new();
        let dir = temp.path().join("state");
        let _held = open(&dir, API).unwrap();
        let error = open(&dir, API).unwrap_err();
        assert!(error.message.contains("holds the lock"), "{error}");
    }
}
