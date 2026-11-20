//! Helpers shared by unit tests: temporary directories with owner-only
//! permissions, kept under `TMPDIR` (or `/tmp` when it is unset or empty).

use std::fs;
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};

/// A private temporary directory removed when dropped.
pub struct TempDir {
    path: PathBuf,
}

impl TempDir {
    pub fn new() -> Self {
        let base = std::env::var_os("TMPDIR")
            .filter(|value| !value.is_empty())
            .map_or_else(|| PathBuf::from("/tmp"), PathBuf::from);
        let path = base.join(format!(
            "portable-opentunnel-test-{}-{}",
            std::process::id(),
            crate::random::alphanumeric(16)
        ));
        fs::DirBuilder::new()
            .mode(0o700)
            .create(&path)
            .unwrap_or_else(|error| panic!("creating {}: {error}", path.display()));
        Self { path }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}
