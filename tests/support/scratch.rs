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
/// testing the release binary. Otherwise the binary Cargo built for this test
/// run is used. The path must be absolute: tests run it from their own
/// working directories.
pub fn binary() -> PathBuf {
    match env::var_os("PORTABLE_OPENTUNNEL_TEST_BINARY") {
        Some(value) if !value.is_empty() => {
            let path = PathBuf::from(value);
            assert!(
                path.is_absolute() && path.is_file(),
                "PORTABLE_OPENTUNNEL_TEST_BINARY must name an existing file by absolute path: {}",
                path.display()
            );
            path
        }
        _ => PathBuf::from(env!("CARGO_BIN_EXE_portable-opentunnel")),
    }
}

/// Nothing listens on the discard port of a test machine's loopback.
pub const UNREACHABLE_PROXY: &str = "http://127.0.0.1:9";

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
    ///
    /// Every request to a non-loopback host goes to a proxy that refuses the
    /// connection, so a test that forgets `--api` fails instead of registering
    /// a real tunnel with the hosted service.
    pub fn command(&self, args: &[&str]) -> Command {
        let mut no_proxy = "127.0.0.1,::1,localhost".to_owned();
        if let Some(host) = env::var("PORTABLE_OPENTUNNEL_TEST_HOST")
            .ok()
            .filter(|host| !host.trim().is_empty())
        {
            no_proxy = format!("{no_proxy},{host}");
        }
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
            .env("ALL_PROXY", UNREACHABLE_PROXY)
            .env("NO_PROXY", no_proxy)
            .stdin(Stdio::null());
        command
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}
