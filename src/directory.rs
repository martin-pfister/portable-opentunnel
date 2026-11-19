//! Choosing and creating the state directory for each mode.
//!
//! * Reusable: `--dir` names the exact directory and is required.
//!   It is created when missing, privately and exclusively.
//! * Ephemeral: always create a unique private directory under `$TMPDIR`
//!   (falling back to the system temporary directory).

use std::ffi::OsString;
use std::fs;
use std::io;
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};

use crate::error::{AppError, AppResult};
use crate::state::{self, Mode};

const EPHEMERAL_PREFIX: &str = "portable-opentunnel-";

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

/// The platform temporary directory used when `TMPDIR` is unset or empty.
///
/// Deliberately a constant rather than `std::env::temp_dir()`: that helper
/// re-reads `TMPDIR`, so with `TMPDIR` set to the empty string it returns an
/// empty path and ephemeral state would be created relative to the working
/// directory instead of in the system temporary directory. This binary is
/// Linux-only, so `/tmp` is the correct platform default.
pub const PLATFORM_TEMP_DIR: &str = "/tmp";

/// The temporary parent for ephemeral runs: a non-empty `$TMPDIR`, otherwise
/// [`PLATFORM_TEMP_DIR`].
///
/// A pure function of the supplied value: it never reads the environment, so
/// tests can exercise every branch without mutating process-wide state.
pub fn choose_temporary_parent(tmpdir: Option<OsString>) -> PathBuf {
    match tmpdir.filter(|value| !value.is_empty()) {
        Some(dir) => PathBuf::from(dir),
        None => PathBuf::from(PLATFORM_TEMP_DIR),
    }
}

/// Picks and creates the directory for a forwarding or init invocation.
pub fn prepare(selection: Selection<'_>) -> AppResult<PathBuf> {
    prepare_with(selection, std::env::var_os("TMPDIR"))
}

/// [`prepare`] with the ephemeral parent supplied explicitly, so the decision
/// is testable without touching process-wide environment state.
fn prepare_with(selection: Selection<'_>, tmpdir: Option<OsString>) -> AppResult<PathBuf> {
    match selection {
        Selection::Reusable(path) => {
            let path = absolutize(path);
            create_reusable(&path)?;
            Ok(path)
        }
        Selection::Ephemeral => {
            let parent = absolutize(&choose_temporary_parent(tmpdir));
            create_unique(&parent)
        }
    }
}

fn create_reusable(path: &Path) -> AppResult<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() || !metadata.is_dir() {
                return Err(AppError::fail(format!(
                    "{} is not a real directory",
                    path.display()
                )));
            }
            state::check_private_dir(path)
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => state::create_private_dir(path),
        Err(error) => Err(AppError::io(format!("inspecting {}", path.display()))(
            error,
        )),
    }
}

/// Creates a unique private directory under `parent`, retrying on collision.
fn create_unique(parent: &Path) -> AppResult<PathBuf> {
    for _ in 0..64 {
        let path = parent.join(format!("{EPHEMERAL_PREFIX}{}", random_token()));
        let mut builder = fs::DirBuilder::new();
        builder.mode(0o700);
        match builder.create(&path) {
            Ok(()) => {
                state::fsync_dir(&path)?;
                return Ok(path);
            }
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

fn random_token() -> String {
    crate::random::alphanumeric(16)
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::test_support::TempDir;
    use std::os::unix::fs::PermissionsExt;

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
        // A second run accepts the existing private directory.
        let again = prepare(Selection::Reusable(&path)).unwrap();
        assert_eq!(again, path);
    }

    #[test]
    fn ephemeral_directory_is_unique_and_private() {
        let temp = TempDir::new();
        let parent = temp.path();
        let first = create_unique(parent).unwrap();
        let second = create_unique(parent).unwrap();
        assert_ne!(first, second);
        assert!(private(&first) && private(&second));
        assert!(
            first
                .file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with(EPHEMERAL_PREFIX)
        );
    }

    #[test]
    fn temporary_parent_follows_a_nonempty_tmpdir() {
        let temp = TempDir::new();
        let chosen = choose_temporary_parent(Some(temp.path().as_os_str().to_owned()));
        assert_eq!(chosen, temp.path());
    }

    #[test]
    fn empty_tmpdir_falls_back_to_the_platform_directory() {
        // Not `std::env::temp_dir()`: that re-reads `TMPDIR`, so this assertion
        // must be independent of whatever the test environment holds.
        assert_eq!(
            choose_temporary_parent(Some(OsString::new())),
            PathBuf::from(PLATFORM_TEMP_DIR)
        );
    }

    #[test]
    fn absent_tmpdir_falls_back_to_the_platform_directory() {
        assert_eq!(
            choose_temporary_parent(None),
            PathBuf::from(PLATFORM_TEMP_DIR)
        );
    }

    #[test]
    fn the_platform_fallback_is_absolute() {
        // The bug this guards: an empty `TMPDIR` once produced a relative path,
        // silently creating ephemeral state under the working directory.
        for supplied in [None, Some(OsString::new())] {
            let chosen = choose_temporary_parent(supplied);
            assert!(
                chosen.is_absolute(),
                "the fallback must be absolute, got {}",
                chosen.display()
            );
            assert_eq!(chosen, PathBuf::from("/tmp"));
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

    #[test]
    fn ephemeral_uses_the_given_tmpdir() {
        let temp = TempDir::new();
        let chosen = prepare_with(
            Selection::Ephemeral,
            Some(temp.path().as_os_str().to_owned()),
        )
        .unwrap();
        assert!(chosen.starts_with(temp.path()));
        assert!(private(&chosen));
    }
}
