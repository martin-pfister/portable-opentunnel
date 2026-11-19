//! Choosing and creating the state directory for each mode.
//!
//! * Reusable: `--directory` names the exact directory and is required.
//!   It is created when missing, privately and exclusively.
//! * Ephemeral: an explicit `--directory` must not exist at all (not even
//!   empty) and is created exclusively; otherwise a unique directory under
//!   `$TMPDIR` (falling back to the system temporary directory) is used.

use std::ffi::OsString;
use std::fs;
use std::io;
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};

use crate::error::{AppError, AppResult};
use crate::state::{self, Mode};

const EPHEMERAL_PREFIX: &str = "portable-opentunnel-";

pub fn require_explicit(directory: Option<&Path>) -> AppResult<&Path> {
    directory.ok_or_else(|| {
        AppError::new(
            2,
            "--directory DIR is required unless forwarding with --ephemeral",
        )
    })
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
pub fn prepare(directory: Option<&Path>, mode: Mode) -> AppResult<PathBuf> {
    prepare_with(directory, mode, std::env::var_os("TMPDIR"))
}

/// [`prepare`] with the ephemeral parent supplied explicitly, so the decision
/// is testable without touching process-wide environment state.
pub fn prepare_with(
    directory: Option<&Path>,
    mode: Mode,
    tmpdir: Option<OsString>,
) -> AppResult<PathBuf> {
    match (mode, directory) {
        (Mode::Reusable, directory) => {
            let path = absolutize(require_explicit(directory)?);
            create_reusable(&path)?;
            Ok(path)
        }
        (Mode::Ephemeral, Some(path)) => {
            let path = absolutize(path);
            // An explicit ephemeral directory must not exist at all, so a
            // persistent identity can never be turned into a disposable one.
            create_new_private(&path).map_err(|error| {
                if error.kind() == io::ErrorKind::AlreadyExists {
                    AppError::fail(format!(
                        "--ephemeral --directory {} exists; ephemeral state needs a new \
                         directory. Choose another path, or run delete --directory {} to \
                         remove a leftover run",
                        path.display(),
                        path.display()
                    ))
                } else {
                    AppError::io(format!("creating {}", path.display()))(error)
                }
            })?;
            Ok(path)
        }
        (Mode::Ephemeral, None) => create_unique(&choose_temporary_parent(tmpdir)),
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
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            create_new_private(path).map_err(AppError::io(format!("creating {}", path.display())))
        }
        Err(error) => Err(AppError::io(format!("inspecting {}", path.display()))(
            error,
        )),
    }
}

fn create_new_private(path: &Path) -> io::Result<()> {
    let mut builder = fs::DirBuilder::new();
    builder.mode(0o700);
    builder.create(path)?;
    state::fsync_dir(path).map_err(|error| io::Error::other(error.message))
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
        let chosen = prepare(Some(&path), Mode::Reusable).unwrap();
        assert_eq!(chosen, path);
        assert!(private(&path));
        // A second run accepts the existing private directory.
        let again = prepare(Some(&path), Mode::Reusable).unwrap();
        assert_eq!(again, path);
    }

    #[test]
    fn reusable_mode_requires_an_explicit_directory() {
        let error = prepare(None, Mode::Reusable).unwrap_err();
        assert_eq!(error.code, 2);
        assert!(error.message.contains("--directory"));
    }

    #[test]
    fn explicit_ephemeral_directory_must_not_exist() {
        let temp = TempDir::new();
        let path = temp.path().join("run");
        prepare(Some(&path), Mode::Ephemeral).unwrap();
        // Even an empty existing directory is refused.
        let error = prepare(Some(&path), Mode::Ephemeral).unwrap_err();
        assert!(error.message.contains("exists"), "{}", error.message);
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
        assert!(prepare(Some(&link), Mode::Reusable).is_err());
        assert!(prepare(Some(&link), Mode::Ephemeral).is_err());
    }

    #[test]
    fn ephemeral_without_directory_uses_the_given_tmpdir() {
        let temp = TempDir::new();
        let chosen = prepare_with(
            None,
            Mode::Ephemeral,
            Some(temp.path().as_os_str().to_owned()),
        )
        .unwrap();
        assert!(chosen.starts_with(temp.path()));
        assert!(private(&chosen));
    }
}
