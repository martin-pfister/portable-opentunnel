//! Choosing and creating the state directory for each mode.
//!
//! * Reusable: `--dir` names the exact directory and is required.
//!   It is created when missing, privately and exclusively.
//! * Ephemeral: always create a unique private directory under `$TMPDIR`
//!   (falling back to `/tmp`).

use std::ffi::OsString;
use std::io;
use std::path::{Path, PathBuf};

use super::Mode;
use super::private::{check_private_dir, create_private_dir, try_create_private_dir};
use crate::error::{AppError, AppResult};

const EPHEMERAL_PREFIX: &str = "portable-opentunnel-";

/// The parent of ephemeral state when `TMPDIR` is unset or empty.
///
/// Not `std::env::temp_dir()`: that returns an empty path for an empty
/// `TMPDIR`, which would put ephemeral state under the working directory.
const PLATFORM_TEMP_DIR: &str = "/tmp";

/// How to choose the state directory for forwarding or init.
#[derive(Clone, Copy)]
pub enum Selection<'a> {
    Reusable(&'a Path),
    Ephemeral,
}

impl Selection<'_> {
    pub fn mode(self) -> Mode {
        match self {
            Self::Reusable(_) => Mode::Reusable,
            Self::Ephemeral => Mode::Ephemeral,
        }
    }
}

/// Makes a user-supplied path absolute against the current directory, without
/// resolving symlinks (the directory itself is checked later).
pub fn absolutize(path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map(|cwd| cwd.join(path))
            .unwrap_or_else(|_| path.to_path_buf())
    }
}

/// Picks the directory for a forwarding or init invocation, creates it if
/// needed, and checks that it is private.
pub fn prepare(selection: Selection<'_>) -> AppResult<PathBuf> {
    prepare_with(selection, std::env::var_os("TMPDIR"))
}

/// [`prepare`] with `TMPDIR` supplied explicitly, so tests need not mutate
/// process-wide environment state.
fn prepare_with(selection: Selection<'_>, tmpdir: Option<OsString>) -> AppResult<PathBuf> {
    let root = match selection {
        Selection::Reusable(path) => {
            let path = absolutize(path);
            match std::fs::symlink_metadata(&path) {
                Ok(_) => {}
                Err(error) if error.kind() == io::ErrorKind::NotFound => {
                    create_private_dir(&path)?;
                }
                Err(error) => {
                    return Err(AppError::io(format!("inspecting {}", path.display()))(
                        error,
                    ));
                }
            }
            path
        }
        Selection::Ephemeral => create_unique(&absolutize(&temporary_parent(tmpdir)))?,
    };
    check_private_dir(&root)?;
    Ok(root)
}

/// A non-empty `$TMPDIR`, otherwise [`PLATFORM_TEMP_DIR`].
fn temporary_parent(tmpdir: Option<OsString>) -> PathBuf {
    tmpdir
        .filter(|value| !value.is_empty())
        .map_or_else(|| PathBuf::from(PLATFORM_TEMP_DIR), PathBuf::from)
}

/// Creates a unique private directory under `parent`, retrying on collision.
fn create_unique(parent: &Path) -> AppResult<PathBuf> {
    for _ in 0..64 {
        let path = parent.join(format!(
            "{EPHEMERAL_PREFIX}{}",
            crate::random::alphanumeric(16)
        ));
        match try_create_private_dir(&path) {
            Ok(()) => return Ok(path),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => {
                return Err(AppError::io(format!(
                    "creating a temporary directory in {}",
                    parent.display()
                ))(error));
            }
        }
    }
    Err(AppError::fail(format!(
        "could not create a unique temporary directory in {}",
        parent.display()
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    use crate::test_support::TempDir;

    fn private(path: &Path) -> bool {
        fs::metadata(path).unwrap().permissions().mode() & 0o777 == 0o700
    }

    #[test]
    fn explicit_reusable_directory_is_created_once() {
        let temp = TempDir::new();
        let path = temp.path().join("state");
        let chosen = prepare(Selection::Reusable(&path)).unwrap();
        assert_eq!(chosen, path);
        assert!(private(&path));
        let again = prepare(Selection::Reusable(&path)).unwrap();
        assert_eq!(again, path);
    }

    #[test]
    fn ephemeral_directories_are_unique_and_private_under_tmpdir() {
        let temp = TempDir::new();
        let tmpdir = Some(temp.path().as_os_str().to_owned());
        let first = prepare_with(Selection::Ephemeral, tmpdir.clone()).unwrap();
        let second = prepare_with(Selection::Ephemeral, tmpdir).unwrap();
        assert_ne!(first, second);
        for chosen in [first, second] {
            assert!(chosen.starts_with(temp.path()));
            assert!(private(&chosen));
            assert!(
                chosen
                    .file_name()
                    .unwrap()
                    .to_string_lossy()
                    .starts_with(EPHEMERAL_PREFIX)
            );
        }
    }

    #[test]
    fn an_unset_or_empty_tmpdir_falls_back_to_tmp() {
        for supplied in [None, Some(OsString::new())] {
            assert_eq!(temporary_parent(supplied), PathBuf::from("/tmp"));
        }
    }

    #[test]
    fn symlinked_directory_is_refused() {
        let temp = TempDir::new();
        let real = temp.path().join("real");
        fs::create_dir(&real).unwrap();
        let link = temp.path().join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        assert!(prepare(Selection::Reusable(&link)).is_err());
    }
}
