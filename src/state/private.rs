//! Owner-only directories and files.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::Path;

use crate::error::{AppError, AppResult};

pub fn current_uid() -> u32 {
    // SAFETY: geteuid has no preconditions and cannot fail.
    unsafe { libc::geteuid() }
}

/// Verifies that `root` is a real directory owned by this user and not
/// accessible by group or others. Existing permissions are never relaxed.
pub fn check_private_dir(root: &Path) -> AppResult<()> {
    let metadata = fs::symlink_metadata(root)
        .map_err(AppError::io(format!("inspecting {}", root.display())))?;
    if !metadata.is_dir() {
        return Err(AppError::fail(format!(
            "{} is not a real directory",
            root.display()
        )));
    }
    check_private(root, &metadata)
}

/// The owner and permission half of [`check_private_dir`], for metadata the
/// caller already holds, such as that of an opened descriptor.
pub fn check_private(root: &Path, metadata: &fs::Metadata) -> AppResult<()> {
    if metadata.uid() != current_uid() {
        return Err(AppError::fail(format!(
            "{} is owned by uid {}, not the current user (uid {})",
            root.display(),
            metadata.uid(),
            current_uid()
        )));
    }
    let mode = metadata.permissions().mode() & 0o777;
    if mode & 0o077 != 0 {
        return Err(AppError::fail(format!(
            "{} is accessible by group or others (mode {mode:o}); credentials must stay private. \
             Run: chmod 700 {}",
            root.display(),
            root.display()
        )));
    }
    Ok(())
}

/// Creates `root` with mode 0700, failing if anything is already there.
pub fn create_private_dir(root: &Path) -> AppResult<()> {
    try_create_private_dir(root).map_err(AppError::io(format!("creating {}", root.display())))
}

/// [`create_private_dir`] with the raw error, so callers can detect collisions.
pub fn try_create_private_dir(root: &Path) -> io::Result<()> {
    fs::DirBuilder::new().mode(0o700).create(root)?;
    File::open(root)?.sync_all()
}

pub fn fsync_dir(path: &Path) -> AppResult<()> {
    File::open(path)
        .and_then(|file| file.sync_all())
        .map_err(AppError::io(format!("syncing {}", path.display())))
}

/// Creates a new owner-only file with `content`, synced to disk.
pub fn create_new_private(path: &Path, content: &[u8]) -> AppResult<()> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)
        .map_err(AppError::io(format!("creating {}", path.display())))?;
    file.write_all(content)
        .and_then(|()| file.sync_all())
        .map_err(AppError::io(format!("writing {}", path.display())))
}

/// Whether a recognized residue file is safe to remove: a regular file (not a
/// symlink) owned by this user.
pub fn safe_to_remove(path: &Path) -> bool {
    fs::symlink_metadata(path)
        .is_ok_and(|metadata| metadata.is_file() && metadata.uid() == current_uid())
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::test_support::TempDir;

    #[test]
    fn created_directories_are_private() {
        let temp = TempDir::new();
        let root = temp.path().join("state");
        create_private_dir(&root).unwrap();
        check_private_dir(&root).unwrap();
        assert!(create_private_dir(&root).is_err(), "creation is exclusive");
    }

    #[test]
    fn refuses_a_group_accessible_directory() {
        let temp = TempDir::new();
        let root = temp.path().join("state");
        create_private_dir(&root).unwrap();
        fs::set_permissions(&root, fs::Permissions::from_mode(0o755)).unwrap();
        let error = check_private_dir(&root).unwrap_err();
        assert!(error.message.contains("chmod 700"), "{}", error.message);
    }

    #[test]
    fn refuses_a_symlink() {
        let temp = TempDir::new();
        let real = temp.path().join("real");
        create_private_dir(&real).unwrap();
        let link = temp.path().join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        assert!(check_private_dir(&link).is_err());
    }
}
