//! The executable under test and a private scratch directory per test.

#![allow(dead_code)]

use std::env;
use std::fs;
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

/// `PORTABLE_OPENTUNNEL_TEST_BINARY` names an already-built executable, for
/// testing a release binary or one built for another target. Otherwise the
/// binary Cargo built for this test run is used.
pub fn binary() -> PathBuf {
    match env::var_os("PORTABLE_OPENTUNNEL_TEST_BINARY") {
        Some(value) if !value.is_empty() => PathBuf::from(value),
        _ => PathBuf::from(env!("CARGO_BIN_EXE_portable-opentunnel")),
    }
}

static COUNTER: AtomicU64 = AtomicU64::new(0);

/// A unique private directory holding everything one test needs: the state
/// directories it builds, plus a sanitized `HOME`, `TMPDIR`, and working
/// directory for the child process. Removed when dropped.
pub struct Scratch {
    root: PathBuf,
}

impl Scratch {
    pub fn new() -> Self {
        let base = env::var_os("TMPDIR")
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("/tmp"));
        fs::create_dir_all(&base)
            .unwrap_or_else(|error| panic!("creating {}: {error}", base.display()));

        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|elapsed| elapsed.as_nanos())
            .unwrap_or_default();
        let scratch = Self {
            root: base.join(format!(
                "portable-opentunnel-scratch-{}-{}-{nonce}",
                std::process::id(),
                COUNTER.fetch_add(1, Ordering::Relaxed)
            )),
        };
        for directory in [
            scratch.root.clone(),
            scratch.home(),
            scratch.tmp(),
            scratch.cwd(),
        ] {
            fs::DirBuilder::new()
                .mode(0o700)
                .create(&directory)
                .unwrap_or_else(|error| panic!("creating {}: {error}", directory.display()));
        }
        scratch
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn path(&self, name: &str) -> PathBuf {
        self.root.join(name)
    }

    pub fn home(&self) -> PathBuf {
        self.path("home")
    }

    pub fn tmp(&self) -> PathBuf {
        self.path("tmp")
    }

    /// The child's working directory, also used for relative path arguments.
    pub fn cwd(&self) -> PathBuf {
        self.path("cwd")
    }

    /// The executable with a cleared, reproducible environment.
    ///
    /// `env_clear` means no `*_PROXY`, `OPENTUNNEL_CA_CERT`, `SSL_CERT_FILE`,
    /// or `SSL_CERT_DIR` leaks in from the developer's shell, so a test can
    /// never be steered at a proxy or a private CA.
    pub fn command(&self, args: &[&str]) -> Command {
        let mut command = Command::new(binary());
        command
            .args(args)
            .current_dir(self.cwd())
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", self.home())
            .env("TMPDIR", self.tmp())
            .env("XDG_DATA_HOME", self.path("xdg-data"))
            .env("XDG_CONFIG_HOME", self.path("xdg-config"))
            .env("XDG_STATE_HOME", self.path("xdg-state"))
            .env("XDG_RUNTIME_DIR", self.path("xdg-runtime"))
            .stdin(Stdio::null());
        command
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}
