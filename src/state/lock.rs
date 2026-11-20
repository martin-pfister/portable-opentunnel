//! The exclusive writer lock on one state directory.
//!
//! A stale lock descriptor, a replaced lock path, or a replaced state root must
//! never authorize a write or a deletion, so the lock is re-validated before
//! each of them.

use std::fs::{self, File, OpenOptions};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

use super::layout;
use super::private::check_private;
use crate::error::{AppError, AppResult};

/// dev/ino pair identifying a directory or file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FileId {
    dev: u64,
    ino: u64,
}

impl FileId {
    fn of(metadata: &fs::Metadata) -> Self {
        Self {
            dev: metadata.dev(),
            ino: metadata.ino(),
        }
    }
}

fn open_dir(root: &Path) -> AppResult<FileId> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(root)
        .map_err(AppError::io(format!("opening {}", root.display())))?;
    let metadata = file
        .metadata()
        .map_err(AppError::io(format!("inspecting {}", root.display())))?;
    // Checked on the descriptor, not the path: a directory swapped in after a
    // path-based check must not be locked and written to.
    check_private(root, &metadata)?;
    Ok(FileId::of(&metadata))
}

/// An exclusive lock on one state directory, held for the whole operation.
///
/// `Drop` releases the OS lock and closes the descriptor. Reusable state keeps
/// the lock file; final deletion removes it while the lock is still held.
pub struct StateLock {
    root: PathBuf,
    /// Held open for the whole operation; closing it releases the lock.
    _file: File,
    path_id: FileId,
    dir_id: FileId,
}

impl std::fmt::Debug for StateLock {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("StateLock")
            .field("root", &self.root)
            .finish_non_exhaustive()
    }
}

impl StateLock {
    /// Opens (creating when `create`) and locks `<root>/.lock`.
    pub fn acquire(root: &Path, create: bool) -> AppResult<Self> {
        let dir_id = open_dir(root)?;
        let path = root.join(layout::LOCK_FILE);

        let mut options = OpenOptions::new();
        options
            .read(true)
            .write(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
        if create {
            options.create(true).truncate(false).mode(0o600);
        }
        let file = options
            .open(&path)
            .map_err(AppError::io(format!("opening {}", path.display())))?;

        file.try_lock().map_err(|error| match error {
            fs::TryLockError::WouldBlock => AppError::fail(format!(
                "another portable-opentunnel process holds the lock in {}",
                root.display()
            )),
            fs::TryLockError::Error(error) => AppError::fail(format!(
                "cannot lock {}: {error}. Reliable exclusive locking is required; \
                 some shared-filesystem mount options do not provide it",
                path.display()
            )),
        })?;

        let metadata = file
            .metadata()
            .map_err(AppError::io(format!("inspecting {}", path.display())))?;
        let lock = Self {
            root: root.to_path_buf(),
            _file: file,
            path_id: FileId::of(&metadata),
            dir_id,
        };
        lock.validate()?;
        Ok(lock)
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Confirms that the opened descriptor still is the file at the lock path
    /// and that the directory is still the one we opened. Called before every
    /// state write and before any deletion.
    pub fn validate(&self) -> AppResult<()> {
        let path = self.root.join(layout::LOCK_FILE);
        let metadata = fs::symlink_metadata(&path).map_err(|error| {
            AppError::fail(format!(
                "the lock at {} is gone or unreadable ({error}); refusing to continue",
                path.display()
            ))
        })?;
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            return Err(AppError::fail(format!(
                "the lock at {} was replaced by an unsafe entry; refusing to continue",
                path.display()
            )));
        }
        if FileId::of(&metadata) != self.path_id {
            return Err(AppError::fail(format!(
                "{} or its lock was replaced by another file; refusing to continue",
                self.root.display()
            )));
        }
        if open_dir(&self.root)? != self.dir_id {
            return Err(AppError::fail(format!(
                "{} was replaced while it was in use; refusing to continue",
                self.root.display()
            )));
        }
        Ok(())
    }

    /// Removes the lock file as the final step of deleting a state directory,
    /// while still holding the lock.
    pub fn remove_lock_file(&mut self) -> AppResult<()> {
        self.validate()?;
        let path = self.root.join(layout::LOCK_FILE);
        fs::remove_file(&path).map_err(AppError::io(format!("removing {}", path.display())))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::state::create_private_dir;
    use crate::test_support::TempDir;

    #[test]
    fn second_acquire_fails() {
        let temp = TempDir::new();
        let root = temp.path().join("state");
        create_private_dir(&root).unwrap();
        let _held = StateLock::acquire(&root, true).unwrap();
        let error = StateLock::acquire(&root, true).unwrap_err();
        assert!(
            error.message.contains("holds the lock"),
            "{}",
            error.message
        );
    }

    #[test]
    fn an_unlinked_or_replaced_lock_never_authorizes_writes() {
        let temp = TempDir::new();
        let root = temp.path().join("state");
        create_private_dir(&root).unwrap();
        let mut lock = StateLock::acquire(&root, true).unwrap();
        fs::remove_file(root.join(layout::LOCK_FILE)).unwrap();
        assert!(lock.validate().is_err());
        assert!(lock.remove_lock_file().is_err());
        fs::write(root.join(layout::LOCK_FILE), b"").unwrap();
        assert!(lock.validate().is_err());
    }

    #[test]
    fn a_directory_opened_to_others_is_not_locked() {
        use std::os::unix::fs::PermissionsExt;

        let temp = TempDir::new();
        let root = temp.path().join("state");
        create_private_dir(&root).unwrap();
        let lock = StateLock::acquire(&root, true).unwrap();
        fs::set_permissions(&root, fs::Permissions::from_mode(0o755)).unwrap();
        assert!(lock.validate().is_err());
        drop(lock);
        let error = StateLock::acquire(&root, false).unwrap_err();
        assert!(error.message.contains("chmod 700"), "{error}");
    }
}
