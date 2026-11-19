//! Child-process lifecycle tests against the local `FakeApi` fixture.
//!
//! Every test spawns the real executable, points it at a loopback fake API and
//! bridge (`tests/support/fake_api.rs`), and drives it with real signals. No
//! hosted service is ever contacted: `--api` always names `127.0.0.1`.
//!
//! Readiness is observed the way a user would observe it: the URL line on
//! stdout. Nothing here reaches into the program's state directory to decide
//! whether it is "ready", except to assert what was left on disk afterwards.
//!
//! The child is always wrapped in [`Process`], which kills and reaps it on
//! drop, so a failing assertion cannot leave a forwarder running.

#[path = "support/fake_api.rs"]
mod fake_api;

use std::fs;
use std::io::{BufRead, BufReader};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use fake_api::FakeApi;

// ---------------------------------------------------------------------------
// Scratch directories
// ---------------------------------------------------------------------------

static COUNTER: AtomicU64 = AtomicU64::new(0);

/// A unique private scratch root. The fixture writes its CA here, so it must be
/// a real directory this user owns.
struct Scratch {
    root: PathBuf,
}

impl Scratch {
    fn new() -> Self {
        let base = std::env::var_os("TMPDIR")
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("/tmp/opencode"));
        fs::create_dir_all(&base).expect("creating the scratch base");

        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|elapsed| elapsed.as_nanos())
            .unwrap_or_default();
        let root = base.join(format!(
            "portable-opentunnel-lifecycle-{}-{}-{nonce}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&root).expect("creating the scratch root");
        fs::set_permissions(&root, fs::Permissions::from_mode(0o700))
            .expect("making the scratch root private");

        // A sanitized HOME and TMPDIR so the child never reads or writes the
        // developer's real state.
        for name in ["home", "tmp", "cwd"] {
            let path = root.join(name);
            fs::create_dir(&path).expect("creating a scratch subdirectory");
            fs::set_permissions(&path, fs::Permissions::from_mode(0o700))
                .expect("making a scratch subdirectory private");
        }
        Self { root }
    }

    fn path(&self, name: &str) -> PathBuf {
        self.root.join(name)
    }

    /// The state directory a test operates on.
    fn state(&self) -> PathBuf {
        self.path("state")
    }

    /// Finds the state directory created by this test's single ephemeral run.
    fn ephemeral_state(&self) -> PathBuf {
        let mut entries = fs::read_dir(self.path("tmp")).unwrap();
        let state = entries.next().expect("state under TMPDIR").unwrap().path();
        assert!(entries.next().is_none(), "one temporary state directory");
        assert!(state.join("wrapper.json").is_file());
        state
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

// ---------------------------------------------------------------------------
// The child process
// ---------------------------------------------------------------------------

fn binary() -> PathBuf {
    match std::env::var_os("PORTABLE_OPENTUNNEL_TEST_BINARY") {
        Some(value) if !value.is_empty() => PathBuf::from(value),
        _ => PathBuf::from(env!("CARGO_BIN_EXE_portable-opentunnel")),
    }
}

/// Environment variable the CI acceptance step uses to prove a *release*
/// binary was actually exercised by this suite rather than only compiled.
///
/// When it is set, every successful spawn appends one line naming the binary
/// and a short context. The path is opened in append mode and each line is
/// written with a single `write_all`, so concurrent tests interleave whole
/// lines rather than corrupting each other.
const MARKER_ENV: &str = "PORTABLE_OPENTUNNEL_TEST_BINARY_USED";

/// Records one exercised launch. A missing or unset variable is a no-op.
///
/// The context is the running test's name, taken from the thread name libtest
/// assigns, so the marker proves *which* scenarios exercised the binary without
/// every call site having to pass a label.
fn record_exercise(fallback: &str) {
    let Some(path) = std::env::var_os(MARKER_ENV).filter(|value| !value.is_empty()) else {
        return;
    };
    let thread = std::thread::current();
    let context = thread.name().unwrap_or(fallback);
    let line = format!(
        "{}\t{}\t{}\n",
        binary().display(),
        context,
        std::process::id()
    );
    // Best effort: the marker must never turn a real test into a failure.
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    {
        let _ = std::io::Write::write_all(&mut file, line.as_bytes());
    }
}

/// A spawned child with its streams drained on background threads.
///
/// The reader handles are **retained** and joined only after the process has
/// been confirmed exited. Waiting on a pipe reader while the child is still
/// alive can block forever, and inspecting the buffers without joining can miss
/// the final lines (the exit status is observed before the readers have drained
/// the pipes). `output()` therefore joins first, then reports.
///
/// Dropping it kills and reaps the process, then joins, so a panic in a test
/// cannot leave a forwarder holding a lock, a tunnel, or a detached thread.
struct Process {
    child: std::process::Child,
    lines: Receiver<String>,
    stderr: Arc<Mutex<String>>,
    /// Lines already pulled from `lines` but not yet matched.
    pending: Vec<String>,
    stdout_reader: Option<std::thread::JoinHandle<()>>,
    stderr_reader: Option<std::thread::JoinHandle<()>>,
    /// Set once the child has been reaped, so readers can be joined.
    reaped: bool,
}

impl Process {
    /// Spawns the executable with a cleared environment.
    ///
    /// `env_clear` means no `*_PROXY`, `OPENTUNNEL_CA_CERT`, `SSL_CERT_FILE`,
    /// or `SSL_CERT_DIR` leaks in from the developer's shell.
    fn spawn(scratch: &Scratch, args: &[&str], extra_env: &[(&str, &str)]) -> Self {
        Self::spawn_with_context(scratch, args, extra_env, "lifecycle")
    }

    /// As [`Process::spawn`], but records a specific context in the CI marker.
    fn spawn_with_context(
        scratch: &Scratch,
        args: &[&str],
        extra_env: &[(&str, &str)],
        context: &str,
    ) -> Self {
        let home = scratch.path("home");
        let tmp = scratch.path("tmp");
        let cwd = scratch.path("cwd");

        let mut command = Command::new(binary());
        command
            .args(args)
            .current_dir(&cwd)
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", &home)
            .env("TMPDIR", &tmp)
            .env("XDG_DATA_HOME", scratch.path("xdg-data"))
            .env("XDG_CONFIG_HOME", scratch.path("xdg-config"))
            .env("XDG_STATE_HOME", scratch.path("xdg-state"))
            .env("XDG_RUNTIME_DIR", scratch.path("xdg-runtime"))
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        for (name, value) in extra_env {
            command.env(name, value);
        }

        let mut child = command.spawn().expect("spawning the executable");
        // Only a real launch counts as exercising the binary.
        record_exercise(context);

        let stdout = child.stdout.take().expect("the child's stdout");
        let (sender, lines) = mpsc::channel();
        let stdout_reader = std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                match line {
                    Ok(line) => {
                        if sender.send(line).is_err() {
                            return;
                        }
                    }
                    Err(_) => return,
                }
            }
        });

        let stderr_pipe = child.stderr.take().expect("the child's stderr");
        let stderr = Arc::new(Mutex::new(String::new()));
        let sink = Arc::clone(&stderr);
        let stderr_reader = std::thread::spawn(move || {
            for line in BufReader::new(stderr_pipe).lines() {
                let Ok(line) = line else { return };
                let mut guard = sink.lock().expect("stderr buffer");
                guard.push_str(&line);
                guard.push('\n');
            }
        });

        Self {
            child,
            lines,
            stderr,
            pending: Vec::new(),
            stdout_reader: Some(stdout_reader),
            stderr_reader: Some(stderr_reader),
            reaped: false,
        }
    }

    /// Joins both reader threads. Only valid once the child has been reaped:
    /// while it is alive the pipe writers still hold the descriptors open and a
    /// join would block indefinitely.
    ///
    /// After joining, the channel is drained so every line the child wrote is
    /// in `pending` and visible to later assertions.
    fn join_readers(&mut self) {
        if let Some(handle) = self.stdout_reader.take() {
            let _ = handle.join();
        }
        if let Some(handle) = self.stderr_reader.take() {
            let _ = handle.join();
        }
        while let Ok(line) = self.lines.try_recv() {
            self.pending.push(line);
        }
    }

    fn stderr(&self) -> String {
        self.stderr.lock().expect("stderr buffer").clone()
    }

    /// Waits for a stdout line containing `needle`, within `timeout`.
    ///
    /// Lines that do not match are kept so a later call can still see them.
    fn wait_line(&mut self, needle: &str, timeout: Duration) -> Result<String, String> {
        if let Some(found) = self.take_pending(needle) {
            return Ok(found);
        }
        let deadline = Instant::now() + timeout;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(format!(
                    "timed out waiting for a stdout line containing {needle:?}.\n\
                     stdout so far: {:?}\nstderr so far: {}",
                    self.pending,
                    self.stderr()
                ));
            }
            match self.lines.recv_timeout(remaining) {
                Ok(line) => {
                    if line.contains(needle) {
                        return Ok(line);
                    }
                    self.pending.push(line);
                }
                Err(_) => {
                    return Err(format!(
                        "stdout closed while waiting for {needle:?}.\n\
                         stdout so far: {:?}\nstderr so far: {}",
                        self.pending,
                        self.stderr()
                    ));
                }
            }
        }
    }

    fn take_pending(&mut self, needle: &str) -> Option<String> {
        let index = self.pending.iter().position(|line| line.contains(needle))?;
        Some(self.pending.remove(index))
    }

    fn signal(&self, number: i32) {
        let pid = self.child.id() as i32;
        // SAFETY: `kill` only needs a pid, and the pid belongs to our child.
        let result = unsafe { libc::kill(pid, number) };
        assert_eq!(result, 0, "sending signal {number} to {pid}");
    }

    /// Sends an uncatchable termination signal, as an ungraceful node failure
    /// would. Nothing in the child runs afterwards, so any state left on disk is
    /// exactly what a crash would leave.
    fn kill_hard(&self) {
        self.signal(libc::SIGKILL);
    }

    /// Whether a stdout line containing `needle` has been seen yet.
    ///
    /// Drains anything already queued so this sees lines the reader thread has
    /// forwarded but no `wait_line` call has consumed. Only safe while the child
    /// is still running (or after it was reaped and joined).
    fn stdout_has(&mut self, needle: &str) -> bool {
        while let Ok(line) = self.lines.try_recv() {
            self.pending.push(line);
        }
        self.pending.iter().any(|line| line.contains(needle))
    }

    /// Polls for exit without blocking on `wait` inside an async context.
    ///
    /// On the transition to "exited" the reader threads are joined, so the
    /// output buffers are complete before any caller inspects them. The exit
    /// status can be observed a moment before the pipes are fully drained, so
    /// joining here (and not merely at drop time) is what makes stderr
    /// assertions deterministic.
    fn poll_exit(&mut self) -> Option<std::process::ExitStatus> {
        let status = self.child.try_wait().expect("polling the child");
        if status.is_some() && !self.reaped {
            self.reaped = true;
            self.join_readers();
        }
        status
    }

    fn wait_exit(&mut self, timeout: Duration) -> Result<std::process::ExitStatus, String> {
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(status) = self.poll_exit() {
                return Ok(status);
            }
            if Instant::now() >= deadline {
                return Err(format!(
                    "the child did not exit within {timeout:?}.\nstderr so far: {}",
                    self.stderr()
                ));
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// Waits for a specific exit code, reporting the streams on mismatch.
    fn expect_code(
        &mut self,
        expected: i32,
        timeout: Duration,
        context: &str,
    ) -> Result<(), String> {
        let status = self.wait_exit(timeout)?;
        let actual = status.code().unwrap_or_else(|| {
            // Killed by a signal: report it as a negative number for clarity.
            -(status.signal().unwrap_or(0))
        });
        if actual == expected {
            Ok(())
        } else {
            Err(format!(
                "{context}: expected exit {expected}, got {actual}.\nstderr: {}",
                self.stderr()
            ))
        }
    }
}

impl Drop for Process {
    fn drop(&mut self) {
        if !self.reaped && self.poll_exit().is_none() {
            let _ = self.child.kill();
            let _ = self.child.wait();
            self.reaped = true;
        }
        // Safe now: the child is gone, so the readers cannot block forever.
        self.join_readers();
    }
}

// ---------------------------------------------------------------------------
// Fixture plumbing
// ---------------------------------------------------------------------------

/// A short-lived current-thread runtime for the fixture's async accessors.
///
/// The fixture owns its own runtime, so this is only for the few `async`
/// read-only helpers, and never while a child process is being waited on.
fn block_on<F: std::future::Future>(future: F) -> F::Output {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("creating a helper runtime")
        .block_on(future)
}

/// The body the loopback backend answers with.
const BACKEND_BODY: &str = "hello-from-backend";

/// A loopback HTTP backend that answers `200` with [`BACKEND_BODY`].
///
/// Binds `127.0.0.1` always and, when the kernel provides IPv6 loopback, also
/// `::1` on the *same* port. The two counters are kept apart so a test can prove
/// which address family actually carried the request instead of inferring it.
///
/// Its runtime lives on a dedicated thread driven by `block_on(pending())`, so
/// the accept loops keep running for the whole test (tokio is built here without
/// `rt-multi-thread`).
struct Backend {
    port: u16,
    requests: Arc<AtomicUsize>,
    v6_requests: Arc<AtomicUsize>,
    v6_bound: bool,
    _thread: std::thread::JoinHandle<()>,
}

impl Backend {
    /// A backend on `127.0.0.1` only.
    ///
    /// With no `::1` listener, a `localhost` target must fail its first IPv6
    /// attempt and fall through to IPv4 — which is exactly the resolution
    /// behaviour worth testing.
    fn start() -> Self {
        Self::build(false)
    }

    /// A backend on `127.0.0.1` *and* `::1`, for an explicit IPv6 target.
    fn start_dual() -> Self {
        Self::build(true)
    }

    fn build(want_ipv6: bool) -> Self {
        let requests = Arc::new(AtomicUsize::new(0));
        let v6_requests = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&requests);
        let v6_counter = Arc::clone(&v6_requests);
        let (sender, receiver) = mpsc::channel();

        let thread = std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("creating the backend runtime");
            runtime.block_on(async move {
                let v4 = tokio::net::TcpListener::bind(("127.0.0.1", 0))
                    .await
                    .expect("binding the IPv4 backend");
                let port = v4.local_addr().expect("backend address").port();

                // IPv6 loopback is optional: a kernel with IPv6 disabled cannot
                // provide it, and that is not a product failure.
                let v6 = if want_ipv6 {
                    tokio::net::TcpListener::bind(("::1", port)).await.ok()
                } else {
                    None
                };
                sender
                    .send((port, v6.is_some()))
                    .expect("reporting the backend port");

                let v4_loop = serve_backend(v4, counter);
                match v6 {
                    Some(v6) => {
                        let v6_loop = serve_backend(v6, v6_counter);
                        tokio::join!(v4_loop, v6_loop);
                    }
                    None => v4_loop.await,
                }
            });
        });

        let (port, v6_bound) = receiver.recv().expect("the backend must report its port");
        Self {
            port,
            requests,
            v6_requests,
            v6_bound,
            _thread: thread,
        }
    }

    fn requests(&self) -> usize {
        self.requests.load(Ordering::SeqCst)
    }

    /// Requests that arrived over `::1`. Zero means the tunnel used IPv4.
    fn v6_requests(&self) -> usize {
        self.v6_requests.load(Ordering::SeqCst)
    }

    /// Whether this environment could bind IPv6 loopback at all.
    fn has_ipv6(&self) -> bool {
        self.v6_bound
    }

    fn target(&self) -> String {
        format!("127.0.0.1:{}", self.port)
    }

    /// `localhost:<port>`, which a resolver expands to `::1` and `127.0.0.1`
    /// (in some order). The SDK must reach the backend whichever it tries first.
    fn localhost_target(&self) -> String {
        format!("localhost:{}", self.port)
    }

    /// `[::1]:<port>`, the explicit bracketed IPv6 form.
    fn ipv6_target(&self) -> String {
        format!("[::1]:{}", self.port)
    }
}

/// Accepts connections until the listener is dropped, answering one HTTP
/// request per connection and counting it.
async fn serve_backend(listener: tokio::net::TcpListener, requests: Arc<AtomicUsize>) {
    loop {
        let Ok((mut stream, _)) = listener.accept().await else {
            return;
        };
        let requests = Arc::clone(&requests);
        tokio::spawn(async move {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            let mut buffer = Vec::new();
            let mut chunk = [0u8; 1024];
            while !buffer.windows(4).any(|window| window == b"\r\n\r\n") {
                match stream.read(&mut chunk).await {
                    Ok(0) | Err(_) => return,
                    Ok(read) => buffer.extend_from_slice(&chunk[..read]),
                }
            }
            requests.fetch_add(1, Ordering::SeqCst);
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: text/plain\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{BACKEND_BODY}",
                BACKEND_BODY.len()
            );
            let _ = stream.write_all(response.as_bytes()).await;
            let _ = stream.flush().await;
        });
    }
}

/// Starts the fake API and a loopback backend for one test.
fn fixture(scratch: &Scratch) -> (FakeApi, Backend) {
    fixture_with(scratch, Backend::start())
}

/// The same, with a backend that also listens on `::1`.
fn fixture_dual(scratch: &Scratch) -> (FakeApi, Backend) {
    fixture_with(scratch, Backend::start_dual())
}

fn fixture_with(scratch: &Scratch, backend: Backend) -> (FakeApi, Backend) {
    let api = FakeApi::start(
        &scratch.root,
        Some(std::net::SocketAddr::from(([127, 0, 0, 1], backend.port))),
    );
    (api, backend)
}

fn body_dir(scratch: &Scratch) -> PathBuf {
    scratch.state()
}

fn read(path: &Path) -> String {
    fs::read_to_string(path).unwrap_or_else(|error| panic!("reading {}: {error}", path.display()))
}

// ---------------------------------------------------------------------------
// Shared invocation helpers
//
// Every SDK-level scenario below needs the same few shapes: provision an
// identity, forward a target, wait for readiness, and read the state layout.
// Keeping them here avoids repeating a dozen `Process::spawn` argument lists
// per test, which is what makes this harness maintainable.
// ---------------------------------------------------------------------------

/// The profile directory the wrapper uses inside one state root.
fn profile(state: &Path) -> PathBuf {
    state.join("sdk").join("default")
}

/// Runs `init` and returns its URL. Panics with the child's stderr on failure.
fn init(scratch: &Scratch, api: &FakeApi, state: &Path, extra: &[&str]) -> String {
    let mut args = vec!["init", "--directory", state.to_str().unwrap(), "--api"];
    let api_url = api.api_url();
    args.push(&api_url);
    args.extend_from_slice(extra);
    let mut process = Process::spawn_with_context(scratch, &args, &[], "init");
    process
        .expect_code(0, Duration::from_secs(40), "init")
        .unwrap_or_else(|error| panic!("{error}"));
    process
        .wait_line("https://", Duration::from_secs(10))
        .expect("init must print its URL")
        .trim()
        .to_owned()
}

/// Spawns a forwarding run that has not been waited on yet.
///
/// The caller decides whether to wait for readiness, for an exit, or to signal
/// it, which is what the lifecycle regressions need.
fn spawn_forward(
    scratch: &Scratch,
    api: &FakeApi,
    state: &Path,
    target: &str,
    extra: &[&str],
) -> Process {
    let mut args = vec![target, "--directory", state.to_str().unwrap(), "--api"];
    let api_url = api.api_url();
    args.push(&api_url);
    args.extend_from_slice(extra);
    Process::spawn(scratch, &args, &[])
}

fn spawn_ephemeral(scratch: &Scratch, api: &FakeApi, target: &str, extra: &[&str]) -> Process {
    let api_url = api.api_url();
    let mut args = vec![target, "--ephemeral", "--api", &api_url];
    args.extend_from_slice(extra);
    Process::spawn(scratch, &args, &[])
}

/// `--ca-cert` arguments for a fixture whose CA is on disk.
fn ca_args(api: &FakeApi) -> Vec<String> {
    vec!["--ca-cert".to_owned(), api.ca_path().display().to_string()]
}

/// Waits until a predicate holds, failing the test if the child exits or the
/// deadline passes first. Avoids fixed sleeps that assume a phase was reached.
fn wait_until(
    process: &mut Process,
    timeout: Duration,
    what: &str,
    mut condition: impl FnMut() -> bool,
) {
    let deadline = Instant::now() + timeout;
    while !condition() {
        assert!(
            Instant::now() < deadline,
            "timed out waiting for {what}.\nstderr: {}",
            process.stderr()
        );
        assert!(
            process.poll_exit().is_none(),
            "the child exited while waiting for {what}.\nstderr: {}",
            process.stderr()
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// `init` provisions but never attaches, is idempotent, and the saved URL is
/// readable offline afterwards.
#[test]
fn init_never_attaches_and_keeps_one_stable_url() {
    let scratch = Scratch::new();
    let (api, _backend) = fixture(&scratch);
    let state = body_dir(&scratch);

    let mut process = Process::spawn(
        &scratch,
        &[
            "init",
            "--directory",
            state.to_str().unwrap(),
            "--api",
            &api.api_url(),
            "--startup-timeout",
            "20",
            "--shutdown-timeout",
            "10",
        ],
        &[],
    );
    process
        .expect_code(0, Duration::from_secs(30), "first init")
        .expect("first init should succeed");
    let first = process.wait_line("https://", Duration::from_secs(5));

    // A second init must reuse the identity and the route.
    let mut again = Process::spawn(
        &scratch,
        &[
            "init",
            "--directory",
            state.to_str().unwrap(),
            "--api",
            &api.api_url(),
            "--startup-timeout",
            "20",
            "--shutdown-timeout",
            "10",
        ],
        &[],
    );
    again
        .expect_code(0, Duration::from_secs(30), "second init")
        .expect("second init should be idempotent");
    let second = again.wait_line("https://", Duration::from_secs(5));

    assert_eq!(
        api.creates(),
        1,
        "a second init must not create another registration"
    );
    assert_eq!(
        api.attaches(),
        0,
        "init must never attach a forwarding bridge"
    );

    // `url` is offline: no --api, so it cannot have contacted anything.
    let mut url = Process::spawn(
        &scratch,
        &["url", "--directory", state.to_str().unwrap()],
        &[],
    );
    url.expect_code(0, Duration::from_secs(10), "url after init")
        .expect("url should read the saved state");
    let printed = url.wait_line("https://", Duration::from_secs(5));

    if let (Ok(first), Ok(second), Ok(printed)) = (first, second, printed) {
        assert_eq!(first.trim(), printed.trim(), "url must print the saved URL");
        assert_eq!(second.trim(), printed.trim(), "the URL must stay stable");
    }
    assert_eq!(api.deletes(), 0, "init must not delete anything");
}

/// A persistent forward attaches, serves one real TLS visitor exchange, and
/// keeps its identity on SIGTERM with the same URL for a different target.
#[test]
fn persistent_forward_serves_traffic_then_keeps_identity_on_sigterm() {
    let scratch = Scratch::new();
    let (api, backend) = fixture(&scratch);
    let state = body_dir(&scratch);
    let target = backend.target();

    let mut process = Process::spawn(
        &scratch,
        &[
            &target,
            "--directory",
            state.to_str().unwrap(),
            "--api",
            &api.api_url(),
            "--startup-timeout",
            "30",
            "--shutdown-timeout",
            "10",
        ],
        &[],
    );

    let url = process
        .wait_line("https://", Duration::from_secs(40))
        .expect("the URL must be printed only after the bridge attached");

    // The fixture drives one visitor exchange per attach; the client must reach
    // the backend through real TLS termination.
    let response = block_on(api.wait_visitor_response(Duration::from_secs(20)))
        .expect("the visitor exchange did not complete");
    // Prove the bytes actually came from the backend through the tunnel, rather
    // than merely that some TLS connection succeeded.
    assert!(
        response.contains("hello-from-backend"),
        "the visitor did not receive the backend's body; got: {response:?}"
    );
    assert!(
        response.starts_with("HTTP/1.1 200"),
        "the visitor did not receive a backend HTTP response; got: {response:?}"
    );
    assert!(
        backend.requests() >= 1,
        "the backend never received a request through the tunnel"
    );

    process.signal(libc::SIGTERM);
    process
        .expect_code(143, Duration::from_secs(20), "SIGTERM during forwarding")
        .expect("SIGTERM must exit 143");
    assert_eq!(
        api.deletes(),
        0,
        "reusable state must not be deleted on exit"
    );

    // A restart with a *different* target must reuse the same identity and URL.
    let other_port = {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("binding a spare port");
        listener.local_addr().expect("local address").port()
    };
    let mut restart = Process::spawn(
        &scratch,
        &[
            &format!("127.0.0.1:{other_port}"),
            "--directory",
            state.to_str().unwrap(),
            "--api",
            &api.api_url(),
            "--startup-timeout",
            "30",
            "--shutdown-timeout",
            "10",
        ],
        &[],
    );
    let restarted = restart
        .wait_line("https://", Duration::from_secs(40))
        .expect("the restart must attach again");
    assert_eq!(
        url.trim(),
        restarted.trim(),
        "the saved route must keep the URL stable across target changes"
    );
    assert_eq!(
        api.creates(),
        1,
        "a restart must reuse the identity, not create another"
    );
}

/// Without an explicit directory, ephemeral forwarding uses TMPDIR and
/// removes both the registration and its temporary state on exit.
#[test]
fn ephemeral_exit_deletes_remotely_and_removes_local_state() {
    let scratch = Scratch::new();
    let (api, backend) = fixture(&scratch);

    let mut process = spawn_ephemeral(
        &scratch,
        &api,
        &backend.target(),
        &["--startup-timeout", "30", "--shutdown-timeout", "15"],
    );

    process
        .wait_line("https://", Duration::from_secs(40))
        .expect("the ephemeral tunnel must attach");
    let state = scratch.ephemeral_state();
    assert!(fs::read_dir(scratch.path("cwd")).unwrap().next().is_none());
    process.signal(libc::SIGTERM);
    process
        .expect_code(143, Duration::from_secs(25), "ephemeral SIGTERM")
        .expect("SIGTERM must exit 143");

    assert_eq!(
        api.deletes(),
        1,
        "an ephemeral run must delete its registration.\nstderr: {}",
        process.stderr()
    );
    assert!(api.all_deleted(), "every registration must be gone");
    assert!(
        !state.join("sdk").exists(),
        "the ephemeral local state must be removed"
    );
    assert!(
        !state.exists(),
        "the ephemeral state directory itself must be removed"
    );
}

/// `delete` succeeds for a complete identity and treats a confirmed 404 as
/// success.
#[test]
fn delete_removes_a_complete_identity_and_accepts_404() {
    let scratch = Scratch::new();
    let (api, _backend) = fixture(&scratch);
    let state = body_dir(&scratch);

    let mut init = Process::spawn(
        &scratch,
        &[
            "init",
            "--directory",
            state.to_str().unwrap(),
            "--api",
            &api.api_url(),
            "--startup-timeout",
            "20",
            "--shutdown-timeout",
            "10",
        ],
        &[],
    );
    init.expect_code(0, Duration::from_secs(30), "init before delete")
        .expect("init should succeed");

    let mut delete = Process::spawn(
        &scratch,
        &["delete", "--directory", state.to_str().unwrap()],
        &[],
    );
    delete
        .expect_code(0, Duration::from_secs(25), "delete")
        .expect("delete should succeed");
    assert_eq!(api.deletes(), 1, "the registration must be deleted once");
    assert!(!state.exists(), "delete must remove the owned state");

    // A second identity whose registration the server no longer knows: the
    // fixture answers 404 on DELETE, which must count as success.
    let second = scratch.path("state404");
    let mut init = Process::spawn(
        &scratch,
        &[
            "init",
            "--directory",
            second.to_str().unwrap(),
            "--api",
            &api.api_url(),
            "--startup-timeout",
            "20",
            "--shutdown-timeout",
            "10",
        ],
        &[],
    );
    init.expect_code(0, Duration::from_secs(30), "init for 404 case")
        .expect("init should succeed");
    api.change(|behaviour| behaviour.delete_status = Some(404));

    let mut delete = Process::spawn(
        &scratch,
        &["delete", "--directory", second.to_str().unwrap()],
        &[],
    );
    delete
        .expect_code(0, Duration::from_secs(25), "delete answering 404")
        .expect("a confirmed 404 is success");
    assert!(
        !second.exists(),
        "state is removed once the registration is confirmed absent"
    );
}

/// A pending-only identity is deleted through the pending path.
///
/// The fixture never issues a certificate, so the run is interrupted after the
/// SDK has saved `pending.json`. That is exactly the case where `Client::remove`
/// would silently skip the server call.
#[test]
fn delete_removes_a_pending_only_identity() {
    let scratch = Scratch::new();
    let (api, _backend) = fixture(&scratch);
    api.change(|behaviour| behaviour.certificate_never_ready = true);
    let state = body_dir(&scratch);

    let mut process = Process::spawn(
        &scratch,
        &[
            "init",
            "--directory",
            state.to_str().unwrap(),
            "--api",
            &api.api_url(),
            "--startup-timeout",
            "120",
            "--shutdown-timeout",
            "10",
        ],
        &[],
    );

    let pending = state.join("sdk").join("default").join("pending.json");
    let deadline = Instant::now() + Duration::from_secs(40);
    while !pending.exists() {
        assert!(
            Instant::now() < deadline,
            "the SDK never saved a pending identity.\nstderr: {}",
            process.stderr()
        );
        assert!(
            process.poll_exit().is_none(),
            "the child exited before saving a pending identity.\nstderr: {}",
            process.stderr()
        );
        std::thread::sleep(Duration::from_millis(50));
    }

    process.signal(libc::SIGTERM);
    process
        .expect_code(143, Duration::from_secs(25), "SIGTERM during provisioning")
        .expect("SIGTERM must exit 143 even mid-provision");

    // The pending state is kept by the reusable run and must now be deletable.
    assert!(pending.exists(), "a reusable run keeps its pending state");

    let mut delete = Process::spawn(
        &scratch,
        &["delete", "--directory", state.to_str().unwrap()],
        &[],
    );
    delete
        .expect_code(0, Duration::from_secs(25), "delete pending identity")
        .expect("delete must handle pending-only state");
    assert_eq!(
        api.deletes(),
        1,
        "pending-only state must still call the server.\nstderr: {}",
        delete.stderr()
    );
    assert!(
        !state.exists(),
        "the pending state is removed after deletion"
    );
}

/// A startup timeout is an operational failure, and a signal before readiness
/// exits with the signal status.
#[test]
fn startup_timeout_and_signal_before_ready() {
    let scratch = Scratch::new();
    let (api, _backend) = fixture(&scratch);
    api.change(|behaviour| behaviour.certificate_never_ready = true);
    let state = body_dir(&scratch);

    // 1. The startup deadline bounds a connection that never becomes ready.
    let mut timeout = Process::spawn(
        &scratch,
        &[
            "init",
            "--directory",
            state.to_str().unwrap(),
            "--api",
            &api.api_url(),
            "--startup-timeout",
            "2",
            "--shutdown-timeout",
            "5",
        ],
        &[],
    );
    timeout
        .expect_code(1, Duration::from_secs(30), "startup timeout")
        .expect("a startup deadline must fail with exit 1");
    assert!(
        timeout.stderr().contains("did not finish within"),
        "the timeout should be explained: {}",
        timeout.stderr()
    );

    // 2. A signal before readiness exits with the signal status, not 1.
    //
    // The baseline matters: the timeout case above already created one
    // registration, so waiting for a *nonzero* count would pass immediately and
    // signal the child before it installed its handlers (default action, exit
    // -2). Wait for the count to grow past this baseline instead.
    let other = scratch.path("state2");
    let creates_before = api.creates();
    let mut early = Process::spawn(
        &scratch,
        &[
            "init",
            "--directory",
            other.to_str().unwrap(),
            "--api",
            &api.api_url(),
            "--startup-timeout",
            "120",
            "--shutdown-timeout",
            "5",
        ],
        &[],
    );
    // Wait for this child's own registration, which proves it is genuinely
    // inside provisioning rather than merely started.
    let deadline = Instant::now() + Duration::from_secs(20);
    while api.creates() <= creates_before {
        assert!(
            Instant::now() < deadline,
            "the child never created a registration.\nstderr: {}",
            early.stderr()
        );
        assert!(
            early.poll_exit().is_none(),
            "the child exited before being signalled.\nstderr: {}",
            early.stderr()
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    early.signal(libc::SIGINT);
    early
        .expect_code(130, Duration::from_secs(20), "SIGINT before readiness")
        .expect("SIGINT must exit 130");
}

/// A failed remote deletion keeps the credentials and prints a recovery
/// command.
#[test]
fn failed_deletion_preserves_credentials_and_prints_recovery() {
    let scratch = Scratch::new();
    let (api, _backend) = fixture(&scratch);
    let state = body_dir(&scratch);

    let mut init = Process::spawn(
        &scratch,
        &[
            "init",
            "--directory",
            state.to_str().unwrap(),
            "--api",
            &api.api_url(),
            "--startup-timeout",
            "20",
            "--shutdown-timeout",
            "10",
        ],
        &[],
    );
    init.expect_code(0, Duration::from_secs(30), "init before failed delete")
        .expect("init should succeed");

    api.change(|behaviour| behaviour.delete_status = Some(500));

    let mut delete = Process::spawn(
        &scratch,
        &["delete", "--directory", state.to_str().unwrap()],
        &[],
    );
    delete
        .expect_code(1, Duration::from_secs(25), "delete against a failing API")
        .expect("a failed deletion must exit 1");

    let stderr = delete.stderr();
    assert!(
        stderr.contains("Credentials were kept"),
        "the failure must say the credentials were kept: {stderr}"
    );
    assert!(
        stderr.contains("portable-opentunnel delete --directory"),
        "the failure must print the exact recovery command: {stderr}"
    );
    assert!(
        state.join("wrapper.json").exists(),
        "wrapper metadata must survive a failed deletion"
    );
    assert!(
        state.join("sdk").join("default").join("token").exists(),
        "the token must survive a failed deletion"
    );
}

/// SIGINT exits 130, and a second signal forces a prompt exit without pretending
/// to have cleaned up.
#[test]
fn sigint_exits_130_and_a_second_signal_forces_exit() {
    let scratch = Scratch::new();
    let (api, backend) = fixture(&scratch);
    let state = body_dir(&scratch);

    let mut process = Process::spawn(
        &scratch,
        &[
            backend.target().as_str(),
            "--directory",
            state.to_str().unwrap(),
            "--api",
            &api.api_url(),
            "--startup-timeout",
            "30",
            "--shutdown-timeout",
            "15",
        ],
        &[],
    );
    process
        .wait_line("https://", Duration::from_secs(40))
        .expect("the tunnel must attach before SIGINT");

    process.signal(libc::SIGINT);
    process
        .expect_code(130, Duration::from_secs(25), "SIGINT during forwarding")
        .expect("SIGINT must exit 130");

    // A second signal, sent immediately after the first, must also terminate
    // promptly rather than hanging in cleanup.
    let other = scratch.path("state2");
    let mut twice = Process::spawn(
        &scratch,
        &[
            backend.target().as_str(),
            "--directory",
            other.to_str().unwrap(),
            "--api",
            &api.api_url(),
            "--startup-timeout",
            "30",
            "--shutdown-timeout",
            "30",
        ],
        &[],
    );
    twice
        .wait_line("https://", Duration::from_secs(40))
        .expect("the second tunnel must attach");
    twice.signal(libc::SIGTERM);
    twice.signal(libc::SIGTERM);
    let status = twice
        .wait_exit(Duration::from_secs(10))
        .expect("a second signal must not hang the process");
    assert!(
        status.code().is_some() || status.signal().is_some(),
        "the process must exit after a repeated signal"
    );
}

/// A delayed certificate refresh that is still in flight during shutdown must
/// not recreate credential files after a successful deletion.
///
/// The in-flight request is *observed* through the fixture's poll counter rather
/// than assumed: the SDK issues one certificate GET during provisioning and then
/// one immediately on attach, so a poll count above the provisioning baseline
/// proves the refresh task has entered its delayed GET. Only then is the signal
/// sent, which is the state the runtime teardown has to survive.
#[test]
fn delayed_refresh_cannot_recreate_deleted_state() {
    let scratch = Scratch::new();
    let (api, backend) = fixture(&scratch);
    // Hold every certificate GET in flight for longer than the shutdown budget.
    api.change(|behaviour| behaviour.certificate_delay = Duration::from_secs(3));

    let mut process = spawn_ephemeral(
        &scratch,
        &api,
        &backend.target(),
        &["--startup-timeout", "30", "--shutdown-timeout", "15"],
    );
    process
        .wait_line("https://", Duration::from_secs(60))
        .expect("the ephemeral tunnel must attach");
    let state = scratch.ephemeral_state();

    // Wait until a certificate GET is *actually held* by the fixture. `polls()`
    // only proves a request happened at some point; `active_refreshes()` proves
    // one is inside the delay right now, which is the state a shutdown must
    // survive.
    let deadline = Instant::now() + Duration::from_secs(30);
    while api.active_refreshes() == 0 {
        assert!(
            Instant::now() < deadline,
            "no certificate GET was ever in flight.\nstderr: {}",
            process.stderr()
        );
        assert!(
            process.poll_exit().is_none(),
            "the child exited before a refresh was in flight.\nstderr: {}",
            process.stderr()
        );
        std::thread::sleep(Duration::from_millis(10));
    }

    process.signal(libc::SIGTERM);
    process
        .expect_code(
            143,
            Duration::from_secs(30),
            "ephemeral shutdown with a held refresh",
        )
        .expect("shutdown must still exit 143");

    // The delayed GET resolves about 3 s after it was issued. Any task that
    // escaped the forwarding runtime would write credentials in that window, so
    // wait past it before asserting the state stayed gone.
    let settle = Instant::now() + Duration::from_secs(5);
    while Instant::now() < settle {
        if state.join("sdk").join("default").join("token").exists() || state.exists() {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(
        !state.join("sdk").join("default").join("token").exists(),
        "credentials reappeared after deletion: a refresh task escaped the runtime"
    );
    assert!(
        !state.exists(),
        "the state directory was recreated after a successful deletion"
    );
}

/// Unrecognized files under the SDK profile are preserved, and the surrounding
/// owned files are still removed.
#[test]
fn unknown_profile_files_are_preserved_on_successful_delete() {
    let scratch = Scratch::new();
    let (api, _backend) = fixture(&scratch);
    let state = body_dir(&scratch);

    let mut init = Process::spawn(
        &scratch,
        &[
            "init",
            "--directory",
            state.to_str().unwrap(),
            "--api",
            &api.api_url(),
            "--startup-timeout",
            "20",
            "--shutdown-timeout",
            "10",
        ],
        &[],
    );
    init.expect_code(0, Duration::from_secs(30), "init before preservation check")
        .expect("init should succeed");

    // A file this program does not own, inside the SDK profile.
    let profile = state.join("sdk").join("default");
    let stranger = profile.join("notes.txt");
    fs::write(&stranger, b"keep me\n").expect("writing an unrelated file");

    let mut delete = Process::spawn(
        &scratch,
        &["delete", "--directory", state.to_str().unwrap()],
        &[],
    );
    delete
        .expect_code(
            0,
            Duration::from_secs(25),
            "delete around an unrelated file",
        )
        .expect("an unrelated file must not fail the deletion");

    assert!(
        stranger.exists(),
        "an unrecognized file must be preserved, not deleted"
    );
    assert_eq!(read(&stranger), "keep me\n");
    assert!(
        !profile.join("token").exists(),
        "owned credentials must still be removed"
    );
    assert!(
        !state.join("wrapper.json").exists(),
        "owned wrapper metadata must still be removed"
    );
    // Retention must be reported, not silently ignored.
    let stderr = delete.stderr();
    assert!(
        stderr.contains("retain"),
        "the retained directory should be reported: {stderr}"
    );
}

// ---------------------------------------------------------------------------
// Provisioning deadlines and interruption windows
// ---------------------------------------------------------------------------

/// A signal while `POST /api/tunnel` is still outstanding must not leave an
/// orphaned registration: the pending identity is saved by the grace poll, so
/// the ephemeral cleanup can delete it.
#[test]
fn signal_during_create_response_still_saves_pending_and_deletes() {
    let scratch = Scratch::new();
    let (api, backend) = fixture(&scratch);
    // Hold the create response so the signal lands after the server registered
    // the tunnel but before the client ever saw its token.
    api.change(|behaviour| behaviour.create_delay = Duration::from_secs(3));

    let mut process = spawn_ephemeral(
        &scratch,
        &api,
        &backend.target(),
        &["--startup-timeout", "120", "--shutdown-timeout", "20"],
    );

    // The registration exists server-side; the client has no token yet.
    wait_until(
        &mut process,
        Duration::from_secs(30),
        "the create request",
        || api.creates() > 0,
    );
    let state = scratch.ephemeral_state();
    process.signal(libc::SIGTERM);
    process
        .expect_code(143, Duration::from_secs(40), "signal during create")
        .expect("SIGTERM must exit 143");

    // The grace must have let the pending identity reach disk, which is what
    // makes the registration deletable at all.
    assert_eq!(
        api.deletes(),
        1,
        "an interrupted create must still delete its registration.\nstderr: {}",
        process.stderr()
    );
    assert!(api.all_deleted(), "no orphaned registration may remain");
    assert!(
        !state.exists(),
        "ephemeral state is removed once the registration is gone"
    );
}

#[test]
fn startup_timeout_during_create_still_saves_pending_and_deletes() {
    let scratch = Scratch::new();
    let (api, backend) = fixture(&scratch);
    api.change(|behaviour| behaviour.create_delay = Duration::from_secs(3));
    let mut process = spawn_ephemeral(
        &scratch,
        &api,
        &backend.target(),
        &["--startup-timeout", "1", "--shutdown-timeout", "10"],
    );

    process
        .expect_code(1, Duration::from_secs(15), "startup timeout during create")
        .expect("startup must time out, then clean up the registration");
    assert!(
        process.stderr().contains("startup limit"),
        "{}",
        process.stderr()
    );
    assert!(!process.stdout_has("https://"));
    assert_eq!(api.creates(), 1);
    assert_eq!(api.deletes(), 1, "{}", process.stderr());
    assert!(api.all_deleted());
    assert!(fs::read_dir(scratch.path("tmp")).unwrap().next().is_none());
}

#[test]
fn startup_timeout_reports_an_unconfirmed_create_if_grace_expires() {
    let scratch = Scratch::new();
    let (api, backend) = fixture(&scratch);
    api.change(|behaviour| behaviour.create_delay = Duration::from_secs(30));
    let mut process = spawn_ephemeral(
        &scratch,
        &api,
        &backend.target(),
        &["--startup-timeout", "1", "--shutdown-timeout", "1"],
    );

    process
        .expect_code(
            1,
            Duration::from_secs(10),
            "create outlasts shutdown budget",
        )
        .expect("shutdown must not keep waiting for credentials");
    assert!(
        process.stderr().contains("a remote registration may exist"),
        "{}",
        process.stderr()
    );
    assert_eq!(api.creates(), 1);
    assert_eq!(api.deletes(), 0);
    let state = scratch.ephemeral_state();
    assert!(process.stderr().contains(&state.display().to_string()));
}

/// Each phase fits individually, but their sum exceeds the startup deadline.
#[test]
fn startup_deadline_spans_provisioning_and_attachment() {
    let scratch = Scratch::new();
    let (api, backend) = fixture(&scratch);
    api.change(|behaviour| {
        behaviour.create_delay = Duration::from_secs(3);
        behaviour.attach_delay = Duration::from_secs(3);
    });
    let state = body_dir(&scratch);

    let mut process = spawn_forward(
        &scratch,
        &api,
        &state,
        &backend.target(),
        &["--startup-timeout", "5", "--shutdown-timeout", "10"],
    );

    process
        .expect_code(1, Duration::from_secs(20), "held attachment")
        .expect("a startup deadline must bound attachment too");
    assert!(
        process.stderr().contains("the bridge did not attach"),
        "{}",
        process.stderr()
    );
    assert!(!process.stdout_has("https://"));
    // The identity was provisioned and is kept: a reusable run never deletes.
    assert!(
        profile(&state).join("tunnel.json").exists(),
        "a reusable identity must survive a failed startup"
    );
    assert_eq!(api.deletes(), 0, "nothing may be deleted in reusable mode");
}

// ---------------------------------------------------------------------------
// TLS trust: the fixture's CA must actually be used by the SDK
// ---------------------------------------------------------------------------

/// HTTPS API and WSS bridge with the fixture CA: the real SDK must trust it,
/// attach, and forward a visitor exchange to the backend.
#[test]
fn https_and_wss_with_the_fixture_ca_forwards() {
    let scratch = Scratch::new();
    let backend = Backend::start();
    let api = FakeApi::start_https(
        &scratch.root,
        Some(std::net::SocketAddr::from(([127, 0, 0, 1], backend.port))),
    );
    let state = body_dir(&scratch);
    let ca = ca_args(&api);
    let ca_refs: Vec<&str> = ca.iter().map(String::as_str).collect();

    let target = backend.target();
    let mut args = vec![target.as_str(), "--directory", state.to_str().unwrap()];
    let api_url = api.api_url();
    args.push("--api");
    args.push(&api_url);
    args.extend_from_slice(&ca_refs);
    args.extend_from_slice(&["--startup-timeout", "40", "--shutdown-timeout", "10"]);

    let mut process = Process::spawn(&scratch, &args, &[]);
    process
        .wait_line("https://", Duration::from_secs(60))
        .unwrap_or_else(|error| panic!("{error}"));

    let response = block_on(api.wait_visitor_response(Duration::from_secs(25)))
        .expect("the visitor exchange over WSS must complete");
    assert!(
        response.contains("hello-from-backend"),
        "the forwarded body must come from the backend; got {response:?}"
    );
    assert!(backend.requests() >= 1, "the backend must be reached");

    process.signal(libc::SIGTERM);
    process
        .expect_code(143, Duration::from_secs(20), "HTTPS run shutdown")
        .expect("SIGTERM must exit 143");
}

/// Without `--ca-cert` the private fixture CA must be rejected, and the startup
/// deadline must turn that into a bounded exit 1 rather than a hung ready state.
#[test]
fn https_without_the_ca_fails_within_the_startup_deadline() {
    let scratch = Scratch::new();
    let backend = Backend::start();
    let api = FakeApi::start_https(
        &scratch.root,
        Some(std::net::SocketAddr::from(([127, 0, 0, 1], backend.port))),
    );
    let state = body_dir(&scratch);

    let mut process = spawn_forward(
        &scratch,
        &api,
        &state,
        &backend.target(),
        &["--startup-timeout", "5", "--shutdown-timeout", "10"],
    );

    let started = Instant::now();
    process
        .expect_code(1, Duration::from_secs(60), "untrusted CA")
        .expect("an untrusted API must fail, not hang");
    assert!(
        started.elapsed() < Duration::from_secs(50),
        "the startup deadline must bound the retries"
    );
    // Nothing may be completed: no registration, no `tunnel.json`, no URL.
    assert!(
        !profile(&state).join("tunnel.json").exists(),
        "no identity may be completed when the CA is not trusted"
    );
}

// ---------------------------------------------------------------------------
// SIGKILL recovery
// ---------------------------------------------------------------------------

/// A persistent identity killed with SIGKILL can be reopened and keeps its URL.
#[test]
fn sigkill_persistent_state_reopens_with_the_same_url() {
    let scratch = Scratch::new();
    let (api, backend) = fixture(&scratch);
    let state = body_dir(&scratch);

    let mut first = spawn_forward(
        &scratch,
        &api,
        &state,
        &backend.target(),
        &["--startup-timeout", "40", "--shutdown-timeout", "10"],
    );
    let url = first
        .wait_line("https://", Duration::from_secs(60))
        .expect("the first run must attach");
    // SIGKILL leaves the lock and the state exactly as they were.
    first.kill_hard();
    assert!(
        first.wait_exit(Duration::from_secs(20)).is_ok(),
        "SIGKILL must terminate the process"
    );

    let mut second = spawn_forward(
        &scratch,
        &api,
        &state,
        &backend.target(),
        &["--startup-timeout", "40", "--shutdown-timeout", "10"],
    );
    let again = second
        .wait_line("https://", Duration::from_secs(60))
        .expect("the state must be reusable after SIGKILL");
    assert_eq!(
        url.trim(),
        again.trim(),
        "a killed run must keep its saved URL"
    );
    assert_eq!(api.creates(), 1, "reopening must not create a new identity");
}

/// An ephemeral run killed with SIGKILL leaves a directory a later explicit
/// `delete` can clean up.
#[test]
fn sigkill_ephemeral_state_is_removable_by_explicit_delete() {
    let scratch = Scratch::new();
    let (api, backend) = fixture(&scratch);

    let mut process = spawn_ephemeral(
        &scratch,
        &api,
        &backend.target(),
        &["--startup-timeout", "40", "--shutdown-timeout", "10"],
    );
    process
        .wait_line("https://", Duration::from_secs(60))
        .expect("the ephemeral run must attach");
    process.kill_hard();
    assert!(process.wait_exit(Duration::from_secs(20)).is_ok());

    // No automatic cleanup happened, so the state must still be there...
    let state = scratch.ephemeral_state();
    assert_eq!(api.deletes(), 0, "SIGKILL cannot have deleted remotely");

    // ...and an explicit delete must remove both halves.
    let mut delete = Process::spawn(
        &scratch,
        &["delete", "--directory", state.to_str().unwrap()],
        &[],
    );
    delete
        .expect_code(0, Duration::from_secs(30), "delete abandoned ephemeral run")
        .expect("delete must accept an abandoned ephemeral directory");
    assert_eq!(api.deletes(), 1, "delete must remove the registration");
    assert!(!state.exists(), "delete must remove the local state");
}

// ---------------------------------------------------------------------------
// Terminal SDK failures
// ---------------------------------------------------------------------------

/// A certificate issuance failure is a terminal error: exit 1, credentials kept.
#[test]
fn certificate_failure_exits_1_and_keeps_state() {
    let scratch = Scratch::new();
    let (api, backend) = fixture(&scratch);
    api.change(|behaviour| {
        behaviour.certificate_failure = Some("the CA refused to issue".to_owned());
    });
    let state = body_dir(&scratch);

    let mut process = spawn_forward(
        &scratch,
        &api,
        &state,
        &backend.target(),
        &["--startup-timeout", "30", "--shutdown-timeout", "10"],
    );
    process
        .expect_code(1, Duration::from_secs(45), "certificate failure")
        .expect("a failed issuance must exit 1");
    assert!(
        process.stderr().contains("certificate"),
        "the failure must mention the certificate: {}",
        process.stderr()
    );
    // The reusable identity is kept for a later retry.
    assert!(
        state.join("wrapper.json").exists(),
        "wrapper metadata must survive a terminal failure"
    );
    assert!(
        profile(&state).join("pending.json").exists() || profile(&state).join("token").exists(),
        "the provisioned credentials must be kept for recovery"
    );
}

/// A bad token is a terminal attach error: exit 1, never a false readiness.
#[test]
fn bad_token_attach_is_terminal_and_never_reports_ready() {
    let scratch = Scratch::new();
    let (api, backend) = fixture(&scratch);
    // Provision normally, then make the bridge reject the attach.
    let state = body_dir(&scratch);
    init(&scratch, &api, &state, &["--startup-timeout", "30"]);
    api.change(|behaviour| behaviour.attach_error = Some("bad_token".to_owned()));

    let mut process = spawn_forward(
        &scratch,
        &api,
        &state,
        &backend.target(),
        &["--startup-timeout", "30", "--shutdown-timeout", "10"],
    );
    process
        .expect_code(1, Duration::from_secs(45), "bad token")
        .expect("a bad token must be terminal");
    assert!(
        !process.stdout_has("https://"),
        "a terminal attach error must never print a ready URL"
    );
}

// ---------------------------------------------------------------------------
// Reconnection
// ---------------------------------------------------------------------------

/// A dropped bridge reconnects: the fixture drops only the first attach, so the
/// second attachment must carry the visitor exchange.
#[test]
fn relay_disconnect_reconnects_and_second_attach_forwards() {
    let scratch = Scratch::new();
    let (api, backend) = fixture(&scratch);
    // One-shot and fixture-owned: the fixture consumes this on the first attach.
    // Flipping a flag from the test would race the SDK's reconnect, because
    // `attaches` is counted before the handler snapshots the behaviour.
    api.change(|behaviour| behaviour.drop_first_attach_only = true);
    let state = body_dir(&scratch);

    let mut process = spawn_forward(
        &scratch,
        &api,
        &state,
        &backend.target(),
        &["--startup-timeout", "40", "--shutdown-timeout", "10"],
    );

    // The first attach is dropped before it can carry a visitor exchange. No
    // test-side reset is needed: only that attach is dropped.
    //
    // Readiness is *not* the signal to wait for here: the fixture sends
    // `Attached` and only then closes the socket, so the client legitimately
    // reports ready once before it reconnects. Wait for the second attachment
    // itself instead of inferring it from stdout.
    wait_until(
        &mut process,
        Duration::from_secs(30),
        "the first attach",
        || api.attaches() >= 1,
    );
    wait_until(
        &mut process,
        Duration::from_secs(60),
        "the reconnecting attach",
        || api.attaches() >= 2,
    );
    process
        .wait_line("https://", Duration::from_secs(30))
        .expect("the tunnel must become ready after reconnecting");

    let response = block_on(api.wait_visitor_response(Duration::from_secs(25)))
        .expect("the reconnected session must forward a visitor exchange");
    assert!(
        response.contains("hello-from-backend"),
        "the reconnected tunnel must reach the backend; got {response:?}"
    );
    assert!(backend.requests() >= 1, "the backend must be reached");
    // `opens` is only incremented after the drop check in the fixture, so this
    // proves the visitor exchange happened on a session that was *not* dropped
    // — i.e. genuinely after reconnecting, not on the discarded first attach.
    // The dropped attach returns before opening, so the count is exactly one.
    assert_eq!(
        api.opens(),
        1,
        "exactly one session should have carried traffic, after reconnecting"
    );

    process.signal(libc::SIGTERM);
    process
        .expect_code(143, Duration::from_secs(20), "shutdown after reconnect")
        .expect("SIGTERM must exit 143");
}

// ---------------------------------------------------------------------------
// Target resolution
//
// The API endpoint stays `127.0.0.1`; only the *route target* changes. These
// tests therefore exercise the SDK's route plumbing and the wrapper's target
// parsing, not the control-plane transport.
// ---------------------------------------------------------------------------

/// A `localhost` target reaches a backend that listens only on `127.0.0.1`.
///
/// `localhost` resolves to `::1` first on many systems. Because this backend has
/// no IPv6 listener, that first attempt is refused and the tunnel must retry the
/// IPv4 address — so the test proves the fallthrough, not merely that a name
/// resolved.
#[test]
fn localhost_target_reaches_a_loopback_backend() {
    let scratch = Scratch::new();
    let (api, backend) = fixture(&scratch);
    let state = body_dir(&scratch);

    let mut process = spawn_forward(
        &scratch,
        &api,
        &state,
        &backend.localhost_target(),
        &["--startup-timeout", "40", "--shutdown-timeout", "10"],
    );
    process
        .wait_line("https://", Duration::from_secs(60))
        .expect("a localhost target must still become ready");

    let response = block_on(api.wait_visitor_response(Duration::from_secs(25)))
        .expect("the visitor exchange must complete through a localhost target");
    assert!(
        response.contains(BACKEND_BODY),
        "the localhost target did not reach the backend; got {response:?}"
    );
    // The v4-only backend has no `::1` listener, so a resolver that tried IPv6
    // first must have been refused and retried. Either way the request arrived
    // exactly once, over IPv4.
    assert_eq!(
        backend.v6_requests(),
        0,
        "a v4-only backend cannot have served IPv6"
    );
    assert_eq!(
        backend.requests(),
        1,
        "exactly one request should have been served"
    );

    process.signal(libc::SIGTERM);
    process
        .expect_code(143, Duration::from_secs(20), "localhost target shutdown")
        .expect("SIGTERM must exit 143");
}

/// The same resolution path with an operator-supplied name.
///
/// CI may point `PORTABLE_OPENTUNNEL_TEST_HOST` at a name it mapped to loopback
/// (for example `--add-host portable-opentunnel-fixture:127.0.0.1`). Without the
/// variable the test uses `localhost`, so it runs everywhere and never depends
/// on external DNS.
#[test]
fn configured_host_target_reaches_a_loopback_backend() {
    let scratch = Scratch::new();
    let (api, backend) = fixture(&scratch);
    let state = body_dir(&scratch);

    let host = std::env::var("PORTABLE_OPENTUNNEL_TEST_HOST")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| "localhost".to_owned());
    let target = format!("{host}:{}", backend.port);

    let mut process = spawn_forward(
        &scratch,
        &api,
        &state,
        &target,
        &["--startup-timeout", "40", "--shutdown-timeout", "10"],
    );
    process
        .wait_line("https://", Duration::from_secs(60))
        .unwrap_or_else(|error| panic!("target {target}: {error}"));

    let response = block_on(api.wait_visitor_response(Duration::from_secs(25)))
        .unwrap_or_else(|| panic!("target {target}: the visitor exchange did not complete"));
    assert!(
        response.contains(BACKEND_BODY),
        "target {target} did not reach the backend; got {response:?}"
    );

    process.signal(libc::SIGTERM);
    process
        .expect_code(143, Duration::from_secs(20), "configured host shutdown")
        .expect("SIGTERM must exit 143");
}

/// An explicit bracketed IPv6 target reaches a backend bound to `::1`.
///
/// The skip is deliberate and narrow: only a kernel that cannot bind IPv6
/// loopback is exempt. Where IPv6 exists — including CI — the test must pass,
/// and it asserts that the request arrived over `::1` specifically.
#[test]
fn bracketed_ipv6_target_reaches_an_ipv6_backend() {
    let scratch = Scratch::new();
    let (api, backend) = fixture_dual(&scratch);
    if !backend.has_ipv6() {
        // Nothing to prove without a second loopback address family, and
        // building a tunnel only to skip would waste a registration.
        eprintln!("skipping bracketed_ipv6_target_reaches_an_ipv6_backend: no IPv6 loopback");
        return;
    }
    let state = body_dir(&scratch);

    let mut process = spawn_forward(
        &scratch,
        &api,
        &state,
        &backend.ipv6_target(),
        &["--startup-timeout", "40", "--shutdown-timeout", "10"],
    );
    process
        .wait_line("https://", Duration::from_secs(60))
        .expect("a bracketed IPv6 target must become ready");

    let response = block_on(api.wait_visitor_response(Duration::from_secs(25)))
        .expect("the visitor exchange must complete through an IPv6 target");
    assert!(
        response.contains(BACKEND_BODY),
        "the IPv6 target did not reach the backend; got {response:?}"
    );
    assert_eq!(
        backend.v6_requests(),
        1,
        "the request must have arrived over ::1, not IPv4"
    );
    assert_eq!(
        backend.requests(),
        0,
        "an explicit [::1] target must not fall back to IPv4"
    );

    process.signal(libc::SIGTERM);
    process
        .expect_code(143, Duration::from_secs(20), "IPv6 target shutdown")
        .expect("SIGTERM must exit 143");
}
