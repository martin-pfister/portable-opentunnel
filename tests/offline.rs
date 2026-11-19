//! Black-box, offline CLI tests for `portable-opentunnel`.
//!
//! Each test spawns the real executable as a child process with a sanitized
//! environment (cleared proxies, CA variables, and XDG homes) and a private
//! scratch directory, then asserts on exit status and the bytes on each
//! stream. Only paths that cannot reach the network are exercised:
//!
//! * argument validation (exit 2) and `--help` / `--version` (exit 0),
//! * the read-only `url` command over hand-built state directories,
//! * refusals that happen before any SDK client exists (directory permissions,
//!   symlinks, a held writer lock).
//!
//! `init` and forwarding are never run to completion, because that would
//! create a real hosted identity. The only forwarding invocations below are
//! guaranteed to fail during directory validation; they also point `--api` at a
//! closed loopback port so that even a future regression cannot reach the
//! hosted service.
//!
//! The fixtures are written from the documented on-disk layout rather than by
//! calling into `src/`, so these tests stay independent of internal refactors.
//! The same layout is asserted separately by the unit tests in `src/state.rs`
//! and `src/layout.rs`; keeping both means a layout change fails loudly in
//! either place instead of silently desynchronising.

use std::env;
use std::fs::{self, File, OpenOptions};
use std::io::Write as _;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

// ---------------------------------------------------------------------------
// Running the executable
// ---------------------------------------------------------------------------

/// Environment variable naming an already-built executable, for testing a
/// release binary or one built for another target. Falls back to the binary
/// Cargo built for this test run.
fn binary() -> PathBuf {
    match env::var_os("PORTABLE_OPENTUNNEL_TEST_BINARY") {
        Some(value) if !value.is_empty() => PathBuf::from(value),
        _ => PathBuf::from(env!("CARGO_BIN_EXE_portable-opentunnel")),
    }
}

static COUNTER: AtomicU64 = AtomicU64::new(0);

/// A unique private directory holding everything one test needs: the state
/// directories it builds, plus a sanitized `HOME`, `TMPDIR`, and working
/// directory for the child process.
struct Scratch {
    root: PathBuf,
}

impl Scratch {
    fn new() -> Self {
        let base = env::var_os("TMPDIR")
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("/tmp/opencode"));
        fs::create_dir_all(&base)
            .unwrap_or_else(|error| panic!("creating {}: {error}", base.display()));

        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|elapsed| elapsed.as_nanos())
            .unwrap_or_default();
        let root = base.join(format!(
            "portable-opentunnel-cli-{}-{}-{nonce}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        let mut builder = fs::DirBuilder::new();
        builder.mode(0o700);
        builder
            .create(&root)
            .unwrap_or_else(|error| panic!("creating {}: {error}", root.display()));

        let scratch = Self { root };
        for directory in [scratch.home(), scratch.tmp(), scratch.cwd()] {
            fs::DirBuilder::new()
                .mode(0o700)
                .create(&directory)
                .unwrap_or_else(|error| panic!("creating {}: {error}", directory.display()));
        }
        scratch
    }

    fn path(&self, name: &str) -> PathBuf {
        self.root.join(name)
    }

    fn home(&self) -> PathBuf {
        self.path("home")
    }

    fn tmp(&self) -> PathBuf {
        self.path("tmp")
    }

    /// The child's working directory, also used for relative path arguments.
    fn cwd(&self) -> PathBuf {
        self.path("cwd")
    }

    /// Runs the executable with a cleared, reproducible environment.
    ///
    /// `env_clear` also means no `*_PROXY`, `OPENTUNNEL_CA_CERT`,
    /// `SSL_CERT_FILE`, or `SSL_CERT_DIR` leaks in from the developer's shell,
    /// so a test can never be steered at a proxy or a private CA.
    fn run(&self, args: &[&str]) -> Output {
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
            .output()
            .unwrap_or_else(|error| panic!("running {}: {error}", binary().display()))
    }

    /// Asserts that a rejected invocation left the working directory and
    /// `TMPDIR` empty.
    fn assert_created_nothing(&self) {
        for dir in [self.cwd(), self.tmp()] {
            let mut entries = fs::read_dir(&dir).expect("reading the scratch directory");
            assert!(entries.next().is_none(), "{} is not empty", dir.display());
        }
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        // Only this test's own unique directory is removed.
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// Asserts an exit status, showing both streams when it does not match.
#[track_caller]
fn assert_code(output: &Output, expected: i32, context: &str) {
    assert_eq!(
        output.status.code(),
        Some(expected),
        "{context}\n--- stdout ---\n{}\n--- stderr ---\n{}",
        stdout(output),
        stderr(output)
    );
}

// ---------------------------------------------------------------------------
// State fixtures (the documented layout, written directly)
// ---------------------------------------------------------------------------

fn private_dir(path: &Path) {
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(path)
        .unwrap_or_else(|error| panic!("creating {}: {error}", path.display()));
}

/// Writes a file only this user can read, the way the program and the SDK do.
fn write_private(path: &Path, contents: &str) {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .unwrap_or_else(|error| panic!("creating {}: {error}", path.display()));
    file.write_all(contents.as_bytes())
        .unwrap_or_else(|error| panic!("writing {}: {error}", path.display()));
}

fn wrapper_json(route: &str, api: &str) -> String {
    format!(
        r#"{{"format":1,"owner":"portable-opentunnel","mode":"reusable","api":"{api}","route":"{route}"}}"#
    )
}

/// The fixed API used when a fixture names an endpoint. It is never contacted
/// by these tests; it only satisfies the endpoint rules.
const FIXTURE_API: &str = "http://127.0.0.1:1";

/// A state directory with wrapper metadata but no SDK profile: the state left
/// behind when provisioning was interrupted before the pending identity was
/// saved.
fn metadata_only(root: &Path, route: &str) {
    private_dir(root);
    write_private(
        &root.join("wrapper.json"),
        &wrapper_json(route, FIXTURE_API),
    );
}

/// Sentinel secret. `url` must never surface it, because it must never read it.
const SENTINEL_TOKEN: &str = "SENTINEL-TOKEN-MUST-NOT-BE-READ";

/// A state directory as `url` must see it: wrapper metadata plus the SDK's
/// `tunnel.json`, which the SDK writes last and which is therefore the
/// completion marker.
///
/// The token is present and must be ignored. The `private-key.pem`,
/// `certificate.pem`, and `chain.pem` files are deliberately **absent**: `url`
/// may read only the wrapper route and the `tunnel.json` hostname, so an
/// implementation that reached for `Storage::load` would fail on the missing
/// files instead of quietly starting to read credentials. That makes the
/// read-only rule fail closed rather than merely untested.
fn initialized(root: &Path, route: &str, hostname: &str) {
    private_dir(root);
    write_private(
        &root.join("wrapper.json"),
        &wrapper_json(route, FIXTURE_API),
    );

    let profile = root.join("sdk").join("default");
    private_dir(&profile);
    write_private(&profile.join("token"), &format!("{SENTINEL_TOKEN}\n"));
    write_private(
        &profile.join("tunnel.json"),
        &format!(
            r#"{{"id":"abc","hostname":"{hostname}","certificateExpiry":"2099-01-01T00:00:00.000Z"}}"#
        ),
    );
}

/// The URL the program must derive for a route and hostname.
fn expected_url(route: &str, hostname: &str) -> String {
    if route == "@" {
        format!("https://{hostname}")
    } else {
        format!("https://{route}.{hostname}")
    }
}

/// Asserts that the credential sentinel never leaked into either stream.
#[track_caller]
fn assert_no_secrets(output: &Output) {
    for (name, text) in [("stdout", stdout(output)), ("stderr", stderr(output))] {
        assert!(
            !text.contains(SENTINEL_TOKEN),
            "{name} leaked a credential: {text}"
        );
    }
}

// ---------------------------------------------------------------------------
// Argument validation: exit 2, and nothing touched
// ---------------------------------------------------------------------------

#[test]
fn help_and_version_succeed_without_creating_state() {
    let scratch = Scratch::new();

    for flag in ["--help", "-h"] {
        let output = scratch.run(&[flag]);
        assert_code(&output, 0, flag);
        assert!(
            stdout(&output).contains("portable-opentunnel"),
            "{flag} should describe the program: {}",
            stdout(&output)
        );
    }

    for flag in ["--version", "-V"] {
        let output = scratch.run(&[flag]);
        assert_code(&output, 0, flag);
        assert!(
            stdout(&output).contains("portable-opentunnel"),
            "{flag} should print a version: {}",
            stdout(&output)
        );
    }

    scratch.assert_created_nothing();
}

#[test]
fn invalid_ports_are_rejected_with_exit_2() {
    let scratch = Scratch::new();
    for target in ["0", "65536", "99999", "-1", "abc", "3000abc", "  "] {
        let output = scratch.run(&[target]);
        assert_code(&output, 2, &format!("target {target:?}"));
        assert!(
            !stdout(&output).is_empty() || !stderr(&output).is_empty(),
            "target {target:?} should explain the problem"
        );
        assert_no_secrets(&output);
    }
    scratch.assert_created_nothing();
}

#[test]
fn malformed_hosts_and_ipv6_are_rejected_with_exit_2() {
    let scratch = Scratch::new();
    // Bracketed forms must be complete; unbracketed IPv6 and stray colons are
    // ambiguous and must be refused rather than mis-parsed.
    for target in [
        ":3000",
        "host:",
        "host:0",
        "host:notaport",
        "host:3000:4000",
        "::1:8080",
        "[::1]",
        "[::1]8080",
        "[zz]:3000",
        "[::1]:0",
        "bad host:3000",
        "exa/mple:3000",
    ] {
        let output = scratch.run(&[target]);
        assert_code(&output, 2, &format!("target {target:?}"));
    }
    scratch.assert_created_nothing();
}

#[test]
fn incompatible_flag_combinations_are_rejected_with_exit_2() {
    let scratch = Scratch::new();

    let cases: &[(&[&str], &str)] = &[
        (&["init", "--ephemeral"], "init with --ephemeral"),
        (&["url", "--ephemeral"], "url with --ephemeral"),
        (&["delete", "--ephemeral"], "delete with --ephemeral"),
        (&["init", "3000"], "init with a target"),
        (&["url", "3000"], "url with a target"),
        (&["delete", "3000"], "delete with a target"),
        (
            &["3000", "-d", "./state", "--startup-timeout", "0"],
            "zero startup timeout",
        ),
        (
            &["3000", "-d", "./state", "--shutdown-timeout", "0"],
            "zero shutdown timeout",
        ),
        (&["3000", "--no-such-flag"], "unknown flag"),
        (&[], "no arguments"),
    ];

    for (args, description) in cases {
        let output = scratch.run(args);
        assert_code(&output, 2, description);
        assert_no_secrets(&output);
    }

    scratch.assert_created_nothing();
}

#[test]
fn the_api_override_must_be_a_bare_origin() {
    let scratch = Scratch::new();
    // The SDK joins absolute API paths onto the base, which discards a base
    // path; accepting one would silently redirect requests. `url` is used
    // because it validates arguments before reading any state.
    for api in [
        "ftp://example.test",
        "not a url",
        "http://127.0.0.1:8080/prefix",
        "http://127.0.0.1:8080/?query=1",
        "file:///tmp/socket",
    ] {
        let output = scratch.run(&["url", "-d", "./state", "--api", api]);
        assert_code(&output, 2, &format!("--api {api:?}"));
        assert!(stderr(&output).contains("invalid --api"));
    }
    scratch.assert_created_nothing();
}

#[test]
fn reusable_forwarding_and_subcommands_require_a_directory() {
    let scratch = Scratch::new();
    for action in ["3000", "init", "url", "delete"] {
        let output = scratch.run(&[
            action,
            "--api",
            FIXTURE_API,
            "--startup-timeout",
            "1",
            "--shutdown-timeout",
            "1",
        ]);
        assert_code(&output, 2, action);
        assert!(stdout(&output).is_empty());
        assert!(
            stderr(&output).contains("--directory DIR is required"),
            "{}",
            stderr(&output)
        );
    }
    scratch.assert_created_nothing();
}

// ---------------------------------------------------------------------------
// `url`: read-only, offline, exit 3 when nothing is initialized
// ---------------------------------------------------------------------------

#[test]
fn url_missing_directory_exits_3_and_creates_nothing() {
    let scratch = Scratch::new();
    let state = scratch.path("absent");

    let output = scratch.run(&["url", "--directory", state.to_str().unwrap()]);
    assert_code(&output, 3, "url on a missing directory");
    assert!(
        stdout(&output).is_empty(),
        "nothing may be printed to stdout: {}",
        stdout(&output)
    );
    assert!(
        stderr(&output).contains("No tunnel initialized in"),
        "stderr should explain the state: {}",
        stderr(&output)
    );
    assert!(!state.exists(), "url must not create the directory");
    scratch.assert_created_nothing();
}

#[test]
fn url_prints_the_saved_url_without_reading_credentials() {
    let scratch = Scratch::new();
    let state = scratch.cwd().join("state");
    initialized(&state, "abc123", "abc.opentunnel.xyz");

    let output = scratch.run(&["url", "--directory", "./state"]);
    assert_code(&output, 0, "url on an initialized directory");
    assert_eq!(
        stdout(&output).trim_end(),
        expected_url("abc123", "abc.opentunnel.xyz"),
        "url must print exactly the saved URL"
    );
    assert!(
        stderr(&output).is_empty(),
        "a successful url should be quiet on stderr: {}",
        stderr(&output)
    );
    assert_no_secrets(&output);
}

#[test]
fn url_prints_the_bare_hostname_for_the_root_route() {
    let scratch = Scratch::new();
    let state = scratch.path("state");
    initialized(&state, "@", "abc.opentunnel.xyz");

    let output = scratch.run(&["url", "--directory", state.to_str().unwrap()]);
    assert_code(&output, 0, "url with a root route");
    assert_eq!(stdout(&output).trim_end(), "https://abc.opentunnel.xyz");
}

#[test]
fn url_succeeds_when_the_token_is_unreadable() {
    let scratch = Scratch::new();
    let state = scratch.path("state");
    initialized(&state, "abc123", "abc.opentunnel.xyz");

    let token = state.join("sdk").join("default").join("token");
    fs::set_permissions(&token, fs::Permissions::from_mode(0o000))
        .expect("removing the read bit on the token");

    // A privileged user ignores the mode bits, so this would prove nothing.
    if fs::read(&token).is_ok() {
        eprintln!("skipping url_succeeds_when_the_token_is_unreadable: privileged user");
        return;
    }

    let output = scratch.run(&["url", "--directory", state.to_str().unwrap()]);
    assert_code(&output, 0, "url must not read the token");
    assert_eq!(
        stdout(&output).trim_end(),
        expected_url("abc123", "abc.opentunnel.xyz")
    );
    assert_no_secrets(&output);
}

#[test]
fn url_metadata_without_a_completed_identity_exits_3() {
    let scratch = Scratch::new();
    let state = scratch.path("state");
    metadata_only(&state, "abc123");

    let output = scratch.run(&["url", "--directory", state.to_str().unwrap()]);
    assert_code(&output, 3, "url on metadata-only state");
    assert!(stdout(&output).is_empty());
    assert!(stderr(&output).contains("No tunnel initialized in"));

    // The metadata is untouched: url is read-only.
    assert!(state.join("wrapper.json").exists());
}

#[test]
fn url_corrupt_metadata_exits_1() {
    let scratch = Scratch::new();
    let state = scratch.path("state");
    private_dir(&state);
    write_private(&state.join("wrapper.json"), "{not json");

    let output = scratch.run(&["url", "--directory", state.to_str().unwrap()]);
    assert_code(&output, 1, "url on corrupt metadata");
    assert!(
        stdout(&output).is_empty(),
        "corrupt state must not be reported as a URL: {}",
        stdout(&output)
    );
}

#[test]
fn url_ignores_wrapper_temporary_residue_and_stays_read_only() {
    let scratch = Scratch::new();
    let state = scratch.path("state");
    private_dir(&state);
    // Residue of an interrupted very first metadata write: recognized name, no
    // final metadata, no SDK state. That is "not initialized", not corrupt.
    let residue = state.join("wrapper.json.tmp-4242-abcdef");
    write_private(&residue, "{}\n");

    let output = scratch.run(&["url", "--directory", state.to_str().unwrap()]);
    assert_code(&output, 3, "url with only wrapper residue");
    assert!(stdout(&output).is_empty());
    assert!(
        residue.exists(),
        "url must leave residue untouched (it is read-only)"
    );
}

#[test]
fn url_rejects_a_symlinked_state_directory() {
    let scratch = Scratch::new();
    let real = scratch.path("real");
    initialized(&real, "abc123", "abc.opentunnel.xyz");
    let link = scratch.path("link");
    std::os::unix::fs::symlink(&real, &link).expect("creating a symlink");

    let output = scratch.run(&["url", "--directory", link.to_str().unwrap()]);
    assert_code(&output, 1, "url on a symlinked directory");
    assert!(
        stderr(&output).contains("symlink"),
        "stderr should name the symlink: {}",
        stderr(&output)
    );
}

#[test]
fn url_tolerates_unrelated_entries_in_the_state_root() {
    let scratch = Scratch::new();
    let state = scratch.path("state");
    initialized(&state, "abc123", "abc.opentunnel.xyz");
    // Unrelated content is preserved, not adopted, and does not hide a
    // complete identity from a read-only lookup.
    write_private(&state.join("notes.txt"), "mine\n");

    let output = scratch.run(&["url", "--directory", state.to_str().unwrap()]);
    assert_code(&output, 0, "url with an unrelated file present");
    assert_eq!(
        stdout(&output).trim_end(),
        expected_url("abc123", "abc.opentunnel.xyz")
    );
    assert!(
        state.join("notes.txt").exists(),
        "url must not delete anything"
    );
}

// ---------------------------------------------------------------------------
// Locking, seen from the outside
// ---------------------------------------------------------------------------

/// Takes the writer lock the way another live invocation would, so the tests
/// can observe exclusion without a second tunnel process.
fn hold_lock(state: &Path) -> File {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(state.join(".lock"))
        .expect("creating the lock file");
    file.try_lock().expect("taking the lock");
    file
}

#[test]
fn url_works_while_the_writer_lock_is_held() {
    let scratch = Scratch::new();
    let state = scratch.path("state");
    initialized(&state, "abc123", "abc.opentunnel.xyz");
    let _held = hold_lock(&state);

    // `url` is read-only and must not acquire the writer lock, so a running
    // tunnel cannot make it fail.
    let output = scratch.run(&["url", "--directory", state.to_str().unwrap()]);
    assert_code(&output, 0, "url while the lock is held");
    assert_eq!(
        stdout(&output).trim_end(),
        expected_url("abc123", "abc.opentunnel.xyz")
    );
}

#[test]
fn delete_refuses_live_state_before_contacting_the_api() {
    let scratch = Scratch::new();
    let state = scratch.path("state");
    initialized(&state, "abc123", "abc.opentunnel.xyz");
    let _held = hold_lock(&state);

    // Refusal must happen on the lock, before the recorded endpoint is used,
    // so this stays offline even though the fixture names an API.
    let output = scratch.run(&["delete", "--directory", state.to_str().unwrap()]);
    assert_code(&output, 1, "delete while the lock is held");
    assert!(
        stderr(&output).contains("holds the lock"),
        "stderr should report the live run: {}",
        stderr(&output)
    );

    // Credentials are preserved for recovery.
    assert!(state.join("wrapper.json").exists());
    assert!(state.join("sdk").join("default").join("token").exists());
}

// ---------------------------------------------------------------------------
// Forwarding refusals that provably precede any network use
// ---------------------------------------------------------------------------

#[test]
fn forward_refuses_a_group_accessible_state_directory() {
    let scratch = Scratch::new();
    let state = scratch.path("state");
    private_dir(&state);
    fs::set_permissions(&state, fs::Permissions::from_mode(0o755))
        .expect("loosening the directory mode");

    // `--api` points at a closed loopback port so that even an unexpected
    // regression cannot reach the hosted service; the mode check must fail
    // first, leaving no metadata and no SDK files behind.
    let output = scratch.run(&[
        "3000",
        "--directory",
        state.to_str().unwrap(),
        "--api",
        "http://127.0.0.1:1",
        "--startup-timeout",
        "1",
        "--shutdown-timeout",
        "1",
    ]);
    assert_code(&output, 1, "forward with a group-accessible directory");
    assert!(
        stderr(&output).contains("chmod 700"),
        "stderr should say how to fix the mode: {}",
        stderr(&output)
    );
    assert!(
        !state.join("wrapper.json").exists(),
        "no metadata may be written"
    );
    assert!(!state.join("sdk").exists(), "no SDK state may be created");
}

#[test]
fn forward_refuses_a_symlinked_state_directory() {
    let scratch = Scratch::new();
    let real = scratch.path("real");
    private_dir(&real);
    let link = scratch.path("link");
    std::os::unix::fs::symlink(&real, &link).expect("creating a symlink");

    let output = scratch.run(&[
        "3000",
        "--directory",
        link.to_str().unwrap(),
        "--api",
        "http://127.0.0.1:1",
        "--startup-timeout",
        "1",
        "--shutdown-timeout",
        "1",
    ]);
    assert_code(&output, 1, "forward with a symlinked directory");
    assert!(
        stderr(&output).contains("not a real directory"),
        "stderr should name the problem: {}",
        stderr(&output)
    );
    assert!(!real.join("wrapper.json").exists());
    assert!(!real.join("sdk").exists());
}

#[test]
fn forward_refuses_an_unrelated_nonempty_directory() {
    let scratch = Scratch::new();
    let state = scratch.path("state");
    private_dir(&state);
    write_private(&state.join("notes.txt"), "mine\n");

    let output = scratch.run(&[
        "3000",
        "--directory",
        state.to_str().unwrap(),
        "--api",
        "http://127.0.0.1:1",
        "--startup-timeout",
        "1",
        "--shutdown-timeout",
        "1",
    ]);
    assert_code(&output, 1, "forward into an unrelated directory");
    assert!(
        stderr(&output).contains("not a real directory") || stderr(&output).contains("not empty"),
        "stderr should refuse to initialise there: {}",
        stderr(&output)
    );
    assert!(
        state.join("notes.txt").exists(),
        "unrelated content must be preserved"
    );
    assert!(!state.join("wrapper.json").exists());
}
