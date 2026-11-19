//! Helpers shared by unit tests: temporary directories with owner-only
//! permissions, kept under the platform temporary directory.

use std::fs;
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};

/// A private temporary directory removed when dropped.
pub struct TempDir {
    path: PathBuf,
}

impl TempDir {
    pub fn new() -> Self {
        let mut attempt = 0u32;
        loop {
            let path = std::env::temp_dir().join(format!(
                "portable-opentunnel-test-{}-{}",
                std::process::id(),
                rand_suffix()
            ));
            let mut builder = fs::DirBuilder::new();
            builder.mode(0o700);
            match builder.create(&path) {
                Ok(()) => return Self { path },
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                    attempt += 1;
                    assert!(attempt < 100, "cannot create a temporary directory");
                }
                Err(error) => panic!("creating {}: {error}", path.display()),
            }
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Default for TempDir {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

fn rand_suffix() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos() as u64)
        .unwrap_or_default();
    format!("{nanos:x}-{}", crate::random::alphanumeric(8))
}
