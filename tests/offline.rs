//! Black-box, offline CLI tests for `portable-opentunnel`.
//!
//! Each test spawns the real executable as a child process with a sanitized
//! environment (cleared CA variables, private XDG homes, and a proxy that
//! blocks non-loopback hosts) and a private scratch directory, then asserts
//! on exit status and the bytes on each stream. Only paths that cannot reach
//! the network are exercised:
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
//! calling into `src/`, so these tests stay independent of internal refactors
//! and a change to a file name on disk fails here.

#[path = "support/scratch.rs"]
mod scratch;

use std::fs::{self, File, OpenOptions};
use std::io::Write as _;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::Path;
use std::process::Output;

use scratch::{Scratch, binary};

// ---------------------------------------------------------------------------
// Running the executable
// ---------------------------------------------------------------------------

impl Scratch {
    fn run(&self, args: &[&str]) -> Output {
        self.command(args)
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
            stdout(&output).contains("EXIT STATUS"),
            "{flag} should document the exit codes: {}",
            stdout(&output)
        );
    }

    let output = scratch.run(&["ephemeral", "--help"]);
    assert_code(&output, 0, "ephemeral --help");
    for option in [
        "--api",
        "--ca-cert",
        "--startup-timeout",
        "--shutdown-timeout",
    ] {
        assert!(
            stdout(&output).contains(option),
            "ephemeral --help should list {option}: {}",
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
        let output = scratch.run(&["ephemeral", target, "--api", FIXTURE_API]);
        assert_code(&output, 2, &format!("target {target:?}"));
        assert!(stdout(&output).is_empty(), "stdout is only for the URL");
        assert!(
            !stderr(&output).is_empty(),
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
        let output = scratch.run(&["ephemeral", target, "--api", FIXTURE_API]);
        assert_code(&output, 2, &format!("target {target:?}"));
    }
    scratch.assert_created_nothing();
}

#[test]
fn incompatible_flag_combinations_are_rejected_with_exit_2() {
    let scratch = Scratch::new();

    let cases: &[(&[&str], &str)] = &[
        (&["init", "3000", "--dir", "./state"], "init with a target"),
        (&["url", "3000", "--dir", "./state"], "url with a target"),
        (
            &["delete", "3000", "--dir", "./state"],
            "delete with a target",
        ),
        (
            &[
                "ephemeral",
                "3000",
                "--startup-timeout",
                "0",
                "--api",
                FIXTURE_API,
            ],
            "zero startup timeout",
        ),
        (
            &[
                "ephemeral",
                "3000",
                "--shutdown-timeout",
                "0",
                "--api",
                FIXTURE_API,
            ],
            "zero shutdown timeout",
        ),
        (
            &["url", "--dir", "./state", "--ca-cert", "ca.pem"],
            "url with a network option",
        ),
        (
            &["delete", "--dir", "./state", "--startup-timeout", "1"],
            "delete with a startup option",
        ),
        (
            &["url", "--directory", "./state"],
            "url with the old long directory option",
        ),
        (
            &["url", "-d", "./state"],
            "url with the old short directory option",
        ),
        (&["3000"], "target without a command"),
        (&["ephemeral", "3000", "--no-such-flag"], "unknown flag"),
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
    // path; accepting one would silently redirect requests.
    for api in [
        "ftp://127.0.0.1:1",
        "not a url",
        "http://127.0.0.1:8080/prefix",
        "http://127.0.0.1:8080/?query=1",
        "file:///tmp/socket",
    ] {
        let output = scratch.run(&[
            "ephemeral",
            "3000",
            "--api",
            api,
            "--startup-timeout",
            "1",
            "--shutdown-timeout",
            "1",
        ]);
        assert_code(&output, 2, &format!("--api {api:?}"));
        assert!(stderr(&output).contains("--api"));
    }
    scratch.assert_created_nothing();
}

#[test]
fn reusable_forwarding_and_subcommands_require_a_directory() {
    let scratch = Scratch::new();
    for args in [
        &["up", "3000", "--api", FIXTURE_API][..],
        &["init", "--api", FIXTURE_API],
        &["url"],
        &["delete", "--api", FIXTURE_API],
    ] {
        let output = scratch.run(args);
        assert_code(&output, 2, args[0]);
        assert!(stdout(&output).is_empty());
        assert!(
            stderr(&output).contains("--dir <DIR>"),
            "{}",
            stderr(&output)
        );
    }
    scratch.assert_created_nothing();
}

#[test]
fn ephemeral_forwarding_rejects_an_explicit_directory() {
    let scratch = Scratch::new();
    let output = scratch.run(&[
        "ephemeral",
        "3000",
        "--dir",
        "./disposable",
        "--api",
        FIXTURE_API,
        "--startup-timeout",
        "1",
        "--shutdown-timeout",
        "1",
    ]);
    assert_code(&output, 2, "ephemeral forwarding with --dir");
    assert!(stdout(&output).is_empty());
    assert!(stderr(&output).contains("--dir"));
    scratch.assert_created_nothing();
}

// ---------------------------------------------------------------------------
// `url`: read-only, offline, exit 3 when nothing is initialized
// ---------------------------------------------------------------------------

#[test]
fn url_missing_directory_exits_3_and_creates_nothing() {
    let scratch = Scratch::new();
    let state = scratch.path("absent");

    let output = scratch.run(&["url", "--dir", state.to_str().unwrap()]);
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

    let output = scratch.run(&["url", "--dir", "./state"]);
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

    let output = scratch.run(&["url", "--dir", state.to_str().unwrap()]);
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

    let output = scratch.run(&["url", "--dir", state.to_str().unwrap()]);
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

    let output = scratch.run(&["url", "--dir", state.to_str().unwrap()]);
    assert_code(&output, 3, "url on metadata-only state");
    assert!(stdout(&output).is_empty());
    assert!(stderr(&output).contains("No tunnel initialized in"));

    // The metadata is untouched: url is read-only.
    assert!(state.join("wrapper.json").exists());
}

#[test]
fn url_on_a_half_finished_deletion_exits_3_with_the_finishing_command() {
    let scratch = Scratch::new();
    let state = scratch.path("state");
    initialized(&state, "abc123", "abc.opentunnel.xyz");
    fs::write(
        state.join("wrapper.json"),
        wrapper_json("abc123", FIXTURE_API).replace('}', r#","remote_deleted":true}"#),
    )
    .unwrap();

    let output = scratch.run(&["url", "--dir", state.to_str().unwrap()]);
    assert_code(&output, 3, "url on a marked deletion");
    assert!(stdout(&output).is_empty(), "{}", stdout(&output));
    let stderr = stderr(&output);
    assert!(stderr.contains("was deleted"), "{stderr}");
    assert!(
        stderr.contains(&format!(
            "portable-opentunnel delete --dir {}",
            state.display()
        )),
        "{stderr}"
    );
    assert!(
        !stderr.contains("--ca-cert"),
        "finishing is offline: {stderr}"
    );
    assert_no_secrets(&output);
}

#[test]
fn url_corrupt_metadata_exits_1() {
    let scratch = Scratch::new();
    let state = scratch.path("state");
    private_dir(&state);
    write_private(&state.join("wrapper.json"), "{not json");

    let output = scratch.run(&["url", "--dir", state.to_str().unwrap()]);
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

    let output = scratch.run(&["url", "--dir", state.to_str().unwrap()]);
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

    let output = scratch.run(&["url", "--dir", link.to_str().unwrap()]);
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

    let output = scratch.run(&["url", "--dir", state.to_str().unwrap()]);
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
    let output = scratch.run(&["url", "--dir", state.to_str().unwrap()]);
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
    let output = scratch.run(&["delete", "--dir", state.to_str().unwrap()]);
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
fn an_unusable_ca_from_the_environment_fails_before_anything_is_created() {
    let scratch = Scratch::new();
    let output = scratch
        .command(&["ephemeral", "3000", "--api", FIXTURE_API])
        .env("OPENTUNNEL_CA_CERT", "./absent.pem")
        .output()
        .expect("running the executable");
    assert_code(&output, 1, "OPENTUNNEL_CA_CERT naming a missing file");
    assert!(
        stderr(&output).contains("CA certificate"),
        "{}",
        stderr(&output)
    );
    assert!(stdout(&output).is_empty());
    scratch.assert_created_nothing();
}

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
        "up",
        "3000",
        "--dir",
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
        "up",
        "3000",
        "--dir",
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
        "up",
        "3000",
        "--dir",
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
        stderr(&output).contains("not empty"),
        "stderr should refuse to initialise there: {}",
        stderr(&output)
    );
    assert!(
        state.join("notes.txt").exists(),
        "unrelated content must be preserved"
    );
    assert!(!state.join("wrapper.json").exists());
}

// ---------------------------------------------------------------------------
// The harness's network guard
// ---------------------------------------------------------------------------

/// Without `--api` the binary targets the hosted service. The harness must
/// route that through its proxy, which never forwards anything; here the
/// proxy is a listener that only records the first request line.
#[test]
fn the_hosted_service_is_only_reachable_through_the_guard_proxy() {
    use std::io::{BufRead as _, BufReader};
    use std::net::TcpListener;
    use std::sync::mpsc;
    use std::time::Duration;

    let listener = TcpListener::bind("127.0.0.1:0").expect("binding the proxy");
    let proxy = format!("http://{}", listener.local_addr().unwrap());
    let (sender, received) = mpsc::channel();
    std::thread::spawn(move || {
        if let Ok((stream, _)) = listener.accept() {
            let mut line = String::new();
            let _ = BufReader::new(stream).read_line(&mut line);
            let _ = sender.send(line);
        }
    });

    let scratch = Scratch::new();
    let mut child = scratch
        .command(&["ephemeral", "3000", "--startup-timeout", "20"])
        .env("ALL_PROXY", &proxy)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("spawning the executable");
    let line = received.recv_timeout(Duration::from_secs(15));
    let _ = child.kill();
    let _ = child.wait();

    let line = line.expect("the default API must be requested through the proxy");
    assert!(
        line.starts_with("CONNECT opentunnel.xyz:443 "),
        "unexpected proxy request: {line:?}"
    );
}
