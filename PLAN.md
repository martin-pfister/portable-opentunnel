# portable-opentunnel implementation and test plan

## Goal

Build a small standalone Linux executable that exposes one local service through
Anomaly OpenTunnel, runs in the foreground, and works inside Slurm without
systemd, root access, or an installed OpenTunnel CLI.

Use the official Rust `opentunnel` SDK with explicitly located filesystem
storage. Do not fork OpenTunnel or implement its protocol unless an SDK
limitation makes that necessary and the tradeoff is approved.

The eventual deliverable is a GitHub repository with CI that tests and builds
downloadable binaries on every push to `main`.

Implementation and publication to the confirmed public GitHub repository have
been authorized. Use the user's GitHub no-reply address for commits, never their
real email address. AlmaLinux acceptance tests run in GitHub CI, not a locally
installed container engine.

## Confirmed choices and interface

Confirmed by the user:

- Public GitHub repository: `martin-pfister/portable-opentunnel`.
- Local directory has already been renamed to
  `/home/martin/workdir/portable-opentunnel`.
- License: MIT, with dependency license review.
- Single OS acceptance-test target: AlmaLinux 8.8, x86-64.
- No additional OS acceptance targets.

Agreed interface:

| Topic | Decision |
| --- | --- |
| Default mode | Reuse an identity in `$PWD/.portable-opentunnel`; disconnect but keep it on exit |
| `--ephemeral` mode | Create a fresh identity in private temporary state; delete it remotely on exit |
| Initial architecture | Linux x86-64, as originally requested; other architectures are separate artifacts |
| Local target | Port or explicit `host:port`; a bare port means `127.0.0.1` |
| Route | Generate one random route name per identity, save it with the ID, and reuse it; no `--name` option |
| Concurrency | Lock reusable state and reject a second instance; independent ephemeral directories are allowed |
| Failed disposable cleanup | Retain credentials and print a recovery command |

Use an AlmaLinux 8.8 container for automated runtime/integration acceptance tests.
Pin the image digest and verify its OS version; do not silently replace it with
the moving `8` or `latest` tag. Container tests cover its userland, not its native
kernel, because containers share the CI host kernel. No separate VM/kernel test
matrix is required.

No GitHub credentials are needed in the program. Publishing the repository will
require an appropriately authenticated GitHub account/tool.

## Scope and CLI contract

CLI:

```sh
portable-opentunnel 3000
portable-opentunnel 127.0.0.1:8080 --directory /shared/private/my-tunnel
portable-opentunnel 3000 --ephemeral
portable-opentunnel 3000 --ephemeral --directory ./disposable-tunnel
portable-opentunnel init --directory ./my-tunnel
portable-opentunnel url --directory ./my-tunnel
portable-opentunnel delete
portable-opentunnel delete --directory ./my-tunnel
portable-opentunnel delete --directory /tmp/portable-opentunnel-XXXXXX
portable-opentunnel --help
portable-opentunnel --version
```

- Default mode: `--directory` selects the exact reusable state directory;
  default `$PWD/.portable-opentunnel`. Persist the identity, endpoint, and route
  name, not a live connection. Each invocation establishes a new connection.
- Generate a random route name once per identity and persist both the route and
  SDK tunnel identity/ID. Reuse the route so the complete HTTPS URL remains
  stable even when the target hostname/port changes.
- Do not expose `--name` or automatic route renaming. Persist the random route
  in wrapper metadata before provisioning so interruption cannot leave a
  provisioned identity without its route. Apply these first-run/recovery rules
  after acquiring and validating the directory lock:
  - Missing or empty state (apart from the newly created lock) is a first run
    for forwarding/`init`, and exit 3 for the read-only `url` command.
  - Residue consisting only of recognized wrapper-metadata temporary files and
    the lock, with no SDK state, is an interrupted local first run: after
    ownership checks and locking, discard those incomplete temporary files and
    initialize normally. `url` leaves them untouched and exits 3. No provisioning
    may begin before the final metadata file is committed.
  - Valid wrapper metadata without a completed `tunnel.json` is resumable:
    forwarding/`init` resumes pending provisioning or creates the identity if
    necessary; `url` exits 3.
  - SDK files without wrapper metadata, malformed metadata, or an initialized
    identity without its saved route are corrupt state: exit 1, never silently
    generate a replacement identity/route. Preserve credentials for recovery.
  - Do not initialize in an unrelated nonempty directory. Recognize only the
    documented wrapper/SDK layout; never overwrite unrelated files.
- `url` applies these classification rules without creating/acquiring a writer
  lock or creating a missing directory.
- If saved credentials are invalid or the remote identity was deleted, report an
  actionable error; do not silently replace the identity and change its URL.
- `--ephemeral` without `--directory`: securely create a unique temporary
  directory under `$TMPDIR`, falling back to the system temporary directory.
  Slurm's node-local directory can be selected by setting TMPDIR explicitly.
- With either mode, `--directory` means the exact state directory, not a parent
  for random children. With `--ephemeral`, an explicit directory must not exist:
  create it exclusively and fail if another process created it first. Do not
  accept even an empty existing directory. Never turn persistent state into
  disposable state. Concurrent use of the same explicit directory is rejected.
- `delete` explicitly deletes either reusable state or an abandoned ephemeral
  run and refuses active/locked state. There is no separate `cleanup` command.
- `init` creates/provisions or ensures the saved identity and its fixed random
  route, prints the complete HTTPS route URL to stdout, and never attaches a
  forwarding bridge. It is idempotent and retains state. Reject `--ephemeral`
  with `init`. The URL is an address for future forwarding, not a readiness claim.
- `url`: a local, read-only lookup of the saved full HTTPS route URL. With
  initialized state, print only that URL to stdout and exit 0. With no complete
  identity/route, print \"No tunnel initialized in <directory>.\" to stderr, leave
  stdout empty, and exit 3. Use exit 1 for malformed state, permission errors,
  or other operational failures rather than treating
  corrupt/inaccessible credentials as absent.
- It works without internet and never provisions, renews, connects, deletes,
  creates missing state, or waits for a running tunnel to exit. It makes no
  connectivity claim: the URL may belong to a stopped or reconnecting tunnel.
- For `url`, read the wrapper route and the SDK's `tunnel.json` hostname only.
  The SDK writes `tunnel.json` last by atomic rename; no credential-file reads or
  special synchronization protocol is needed. Missing completion metadata means
  not initialized. Do not infer process liveness from a PID file. Defer a
  separate status command, lock-occupancy reporting, and JSON output.
- Support hostname targets and correctly parsed bracketed IPv6 targets, or
  explicitly document any initial limitation.
- For forwarding invocations, emit the HTTPS URL to stdout only after the bridge
  has successfully attached. `init` prints the same URL after initialization,
  without claiming forwarding is ready; `url` prints the saved URL, if any.
  Send progress, diagnostics, and recovery instructions to stderr.
- Do not implement a ready file in v1; the URL on stdout is the readiness signal
  for forwarding invocations, and `init`/`url` cover saved-address discovery.
- Exit statuses: 0 success; 1 operational/startup/terminal/cleanup failure;
  2 invalid arguments (clap default); 3 no initialized identity for `url`;
  130 after SIGINT; 143 after SIGTERM. Report cleanup errors even when an earlier
  error or signal determines the exit code.
- Expose overall startup and shutdown/cleanup timeouts; default to approximately
  10 minutes for startup (certificate issuance may take minutes) and 15 seconds
  for shutdown. Document that signals/forced termination can defeat cleanup.
- Support SDK proxy and CA configuration without disabling TLS verification.
  Verify and document which SDK options actually honor environment variables.
- Use the default hosted API, with a hidden `--api` override for local tests.
  Pass it explicitly through `ClientOptions`; the Rust SDK does not read
  `OPENTUNNEL_API` (the upstream CLI implements that variable).
  Persist the chosen endpoint before provisioning; subsequent operations use
  the recorded endpoint and reject a conflicting override. Do not read an API
  endpoint environment variable.

Non-goals for v1: launching the application itself, managing Slurm allocations,
multiple routes per invocation, systemd integration, a container image, and
strictly memory-only storage.

## Architecture

### 1. Small native wrapper

- Use a Cargo binary project, Tokio, an argument parser, and the public Rust SDK.
- Commit `Cargo.lock` and pin the SDK exactly (`opentunnel = "=0.6.2"` initially).
  Pin a tested Rust toolchain at least 1.89, matching the SDK minimum and providing
  standard-library file locking. Review SDK updates deliberately.
- Keep CLI parsing, lifecycle supervision, storage ownership, and output
  formatting separate. Prefer real child-process tests against the fake API;
  add a lifecycle test double only for a specific otherwise unreachable case.
- Construct the SDK with `ClientOptions` and `Storage::at(identity_directory)`.
  Use a fixed internal profile within either the reusable or disposable state.
- Supply routes directly to `Client::connect`; do not call the OpenTunnel CLI.
- Avoid the CLI daemon entirely: no control sockets, service registration, or
  background service.
- Use `Tunnel::status_watch()` for readiness and terminal-state detection.
  `connect()` returns before attachment; wait for `State::Connected` before
  printing the URL. Do not depend on broadcast events, which can be missed.
- Keep the SDK's reconnection behavior. A bad token, certificate-not-ready error,
  or unusable proxy can be terminal; ordinary network failures keep retrying.
  The wrapper's startup deadline bounds a connection that never becomes ready.

### 2. Explicit lifecycle

```text
validate arguments
  -> create/open private state and acquire exclusive lock
  -> atomically persist wrapper metadata including the random route
  -> load and ensure existing identity, or provision a new identity
  -> connect and await attached state
  -> announce readiness
  -> forward until signal or terminal error
  -> stop forwarding / join active tasks
  -> reusable mode: retain identity, saved route, and recovery metadata
  -> --ephemeral mode: delete remote registration, then remove owned run state
```

- Both modes are foreground-only. Retaining an identity does not retain a
  connection or leave any forwarding process running.
- In reusable mode, interrupted provisioning remains available to resume on the
  next invocation. Do not delete it just because startup or forwarding failed.
- Install SIGINT/SIGTERM handling before provisioning, not only after connection.
- Handle signals during provisioning, attachment, reconnect, and shutdown.
- Stop or join lifecycle tasks before deleting files; no detached task may
  continue provisioning or rewriting credentials after cleanup.
- Do not race `Tunnel::wait()` against signals: its future takes ownership of
  the task handle, and cancellation can detach it. Select on `status_watch()`
  reaching `State::Stopped` and termination signals instead, then always call
  `tunnel.close().await` to join and retrieve any terminal error.
- Use an explicitly owned Tokio current-thread runtime for provisioning and
  forwarding (`Builder::new_current_thread().enable_all()`, then `block_on`),
  rather than a multi-threaded `#[tokio::main]`. The pinned SDK's unjoined
  refresh task saves credentials synchronously within its async task, not on a
  separate storage-writing thread. Once this runtime's `block_on` returns,
  those async tasks are suspended and cannot race local deletion.
  Visitor TLS processing uses one core per process; accept that tradeoff for v1.
- For ephemeral shutdown, attempt `tunnel.close().await` within the shared
  shutdown deadline, drop forwarding-runtime client/resources, then consume
  that runtime with `shutdown_background()`. For an interrupted provision,
  return/drop that future before runtime shutdown too. Background shutdown is
  equivalent to a zero shutdown timeout; do not spend the remote-cleanup budget
  waiting on an unrelated blocking hostname lookup.
- Construct a fresh current-thread cleanup runtime and a NEW SDK `Client` from
  the recorded endpoint/storage and this invocation's resolved CA options or
  environment. For automatic ephemeral cleanup, carry forward the CA option
  values from startup; a later explicit `delete` resolves its own CA settings.
  CA paths are not persisted in wrapper metadata. Do not reuse the original
  reqwest client, connection pool, or runtime-bound I/O. Read saved complete/
  pending credentials, request remote deletion, then remove owned local state.
- Tokio documents that background shutdown can leave blocking work running;
  returning from it is not proof that every task finished. The write-safety
  argument is the current-thread runtime plus a source audit that no credential
  writer escapes it. Audit the ENTIRE pinned crate, including protocol/helpers,
  for `tokio::fs`, `spawn_blocking`, `thread::spawn`, and equivalent thread/task
  dispatch. Review any matches and the actual credential write call paths, not
  just enabled Cargo features or the absence of a single spelling.
  Repeat and record this check on every SDK bump. Unrelated DNS blocking work
  may outlive shutdown. If any credential writer can escape that boundary,
  retain state and fail the safety gate rather than delete potentially active
  files. Regression-test delayed refresh responses explicitly.
- Reusable mode also stops its runtime, but never deletes identity files on exit.
  `delete` runs only a fresh cleanup client/runtime while holding the state lock.
- Keep process-wide signal counting/force-exit handling active across the
  runtime transition and synchronous cleanup (for example via signal-hook);
  register async listeners in each runtime as needed. Tokio-installed handlers
  do not restore the OS default terminate action when a runtime is dropped.
  A second signal must still force exit, leaving credentials if not yet deleted.
- Give cleanup its own deadline, independent of an expired startup deadline.
  Close, runtime shutdown, remote deletion, and local cleanup share that single
  shutdown budget; do not reset it at each phase. If it is exhausted, retain
  remaining recovery state and exit. A second termination request may force
  exit, retaining recoverable state.
- On terminal SDK errors, exit rather than leaving an apparently healthy process
  running without a tunnel. Transient disconnects remain the SDK's responsibility.

### 3. Secure and recoverable state

- Use race-safe random directory creation, 0700 directories, and 0600 secret
  files; never weaken permissions on an existing directory.
- Use one atomic wrapper metadata file for format version, ownership marker,
  mode, endpoint, and random route. Write it before SDK provisioning; do not
  spread those fields across separate marker and route files.
  Do not place tokens in argv, logs, or committed files.
- Account for atomic-write residue after SIGKILL. In the pinned SDK profile,
  allow its narrowly defined regular-file names as well as the final files:
  `token.tmp-<pid>`, `tunnel.tmp-<pid>`, `pending.tmp-<pid>`,
  `private-key.tmp-<pid>`, `certificate.tmp-<pid>`, and `chain.tmp-<pid>`.
  Give wrapper metadata writes one documented narrow temporary-name pattern,
  for example `wrapper.json.tmp-<pid>-<random>`, and create those files
  exclusively with private permissions.
- Treat these recognized temporary files as owned residue only within the
  validated state layout, with matching ownership and no symlinks. They must
  not make recovery/deletion refuse otherwise valid state. Unknown files remain
  unrelated and are preserved. Do not promote incomplete temporary credentials
  to a completed identity or infer that a PID suffix proves a process is dead.
- Refuse unsafe cleanup targets, symlink substitutions, and paths that do not
  identify a wrapper-owned run. Use a lock to prevent cleanup of a live run.
- Hold the same exclusive lock in BOTH reusable and ephemeral modes, and in
  `init`/`delete`, for the entire modifying operation. `delete` refuses a live
  ephemeral run just as it refuses a live persistent run.
- After acquiring a lock, compare the open file's device/inode (`fstat`, exposed
  by Rust `File::metadata`/Unix `MetadataExt`) with the current lock path's
  device/inode. Reject a missing, symlinked, or replaced path; fail/retry before
  reading or writing state if they differ. Validate state-directory identity as
  well. This prevents a process that opened an old, subsequently unlinked lock
  from proceeding concurrently with a process holding a new lock inode.
- Ordinary persistent exits release the descriptor without unlinking the lock.
  Final state deletion may remove it only as the last file while still holding
  the lock, with no further state writes afterward. All acquiring commands must
  perform the identity check above. Do not use only a PID file: PIDs collide
  across nodes and may be reused.
- Use `std::fs::File::try_lock` and hold the descriptor for the entire operation.
  Fail clearly if the filesystem reports unsupported locking or another owner.
  Document that reliable locking is required; some shared-filesystem mount
  options do not provide cross-node exclusion. Do not promise automatic detection
  of every unsafe configuration or a network-filesystem test matrix.
- For local deletion, remove verified known SDK files and owned temporary files
  individually, then attempt nonrecursive removal of their profile directories.
  Do not use `storage().remove(profile)`: it recursively deletes the profile,
  including unrelated files. Remove known wrapper metadata/owned temporary files,
  then the lock last while holding it. Finally attempt nonrecursive `remove_dir`
  on the state root.
  Unrelated entries are preserved and prevent directory removal; report that
  the directory was retained. Never recursively delete the root, a disposable
  parent, or an arbitrary user-supplied directory. If another process has begun
  a fresh run after lock unlink, `remove_dir` must fail harmlessly on its files.
- In `--ephemeral`/explicit deletion, remove local state after successful remote
  deletion. On deletion failure or uncertain outcome, retain state and print a
  precise retry command. Default-mode exits never delete the reusable identity.
- Do not rely on a temporary-directory destructor to delete credentials on all
  exits; that would destroy recovery information when remote cleanup fails.
- The explicit `delete --directory DIR` command can recover abandoned runs after
  SIGKILL or node failure. Do not automatically delete old-looking directories.
  Print the exact recovery command if graceful ephemeral deletion fails.
- State the limitation honestly: this controls application-managed writes, not
  OS swap, core dumps, external Slurm logs, or temporary files written by tools
  outside this process.

### 4. Server-side cleanup and responsible usage

- Remote deletion is best-effort and soft-deletes the registration; it does not
  erase the server's database row or reverse certificate-issuance work.
- Handle complete and pending-only identities explicitly. In SDK 0.6.2,
  `Client::remove` skips the server call for pending-only state and then removes
  the profile directory; do not use it on that path.
- For pending-only state, use `client.pending(profile)` to recover ID/token,
  `client.api().remove(&token, &id)` to request deletion, and only after success
  or confirmed 404 remove known SDK and wrapper state through the wrapper's
  nonrecursive cleanup. Preserve credentials on every other failure.
- The reviewed SDK's public `Api::remove` sends once rather than guaranteeing
  retries. A timeout is an uncertain outcome: keep state for another explicit
  deletion attempt, which may correctly return 404.
- Treat a confirmed already-absent registration as cleanup success. Preserve
  credentials when a timeout leaves the remote outcome unknown.
- A lost create response can leave a registration whose token the client never
  received. No wrapper can guarantee cleanup of that case.
- Narrow the avoidable cancellation window: on the first shutdown request
  during create/key/CSR generation, allow the in-flight creation future to run
  until its `RequestingCertificate` progress stage, within the shutdown budget.
  In SDK 0.6.2 the pending ID/token have been saved by that point. Then cancel
  provisioning and clean up through the pending-identity path. Keep the second
  signal/deadline escape hatch; do not start another create request to recover.
- Default CI must not create tunnels on the hosted service. Avoid tight loops
  that create new identities during development or testing.
- Reuse mode is the default to reduce certificate-issuance load. Disposable mode
  is explicit; do not accidentally delete a reusable identity.

## Testing strategy

### A. Unit tests: no network or public service

- Argument validation: ports/targets, invalid ports, bracketed IPv6, timeouts,
  subcommands, and incompatible flag combinations.
- State parent/run creation, permissions, ownership markers, and lock behavior.
- Concurrent disposable runs get independent credentials and cannot remove each
  other's state. Concurrent reusable runs against one directory are rejected.
- Reusable restarts keep their identity and route/URL, including target-port
  changes. Invalid saved identities are not silently replaced.
- A normal reusable exit closes forwarding but does not call remote deletion;
  `--ephemeral` and explicit `delete` use the correct deletion semantics.
- `init` is idempotent, persists both route and identity, prints the same URL
  later used by forwarding, and never attaches or starts a background process.
- Test directory selection in both modes, TMPDIR fallback, and rejection of every
  pre-existing explicit ephemeral directory, including empty ones and symlinks.
- `url` is offline/read-only, prints only the full route URL when initialized,
  and has the documented stdout/stderr and exit-status behavior for missing,
  incomplete, corrupt, inaccessible, or concurrently changing state. It makes no
  connectivity claim and does not create a missing directory.
- Cleanup rejects unowned paths and unsafe symlinks and preserves unrelated files.
- Deterministically test deletion against a contender that has already opened
  the old lock, lock-path replacement, and directory recreation. A stale lock
  descriptor must never authorize writes; ephemeral runs also exclude `delete`.
- Test missing/empty state, metadata-only state, pending state, SDK-only corrupt
  state, and unrelated nonempty directories against the classification rules.
- Test recognized SDK/wrapper temporary-file residue after interrupted atomic
  writes, including an interrupted very first metadata write. Recovery and
  deletion must remain usable; `url` stays read-only. Near-matching unknown
  filenames, unsafe ownership, and symlinks must not be blindly removed.
- The stdout URL is valid, emitted at the right time, and contains no credentials
  beyond the intentionally disclosed route URL.
- Errors, exit status selection, and redaction.
- Use child-process tests for interruption during certificate waiting, after
  readiness, and during cleanup, including repeated signals and deadlines.
  Add narrow unit/test-double cases only where these cannot cover the boundary.

### B. Local integration tests

- Use a small fake HTTP API for create/info/certificate/delete operations and a
  fake bridge WebSocket. Exercise the real SDK for provisioning, readiness,
  signals, deletion, and pending cleanup. Use SDK public protocol types for JSON
  and frames where their serialization APIs support it. For response types
  without serialization, use minimal JSON literals matching the pinned public
  schema rather than introducing extra production abstractions.
- Serve most fixture API traffic on `http://127.0.0.1`, with `ws://` bridge
  connections; retain a separate HTTPS/WSS test using `ClientOptions::ca_cert`.
  The forwarded visitor connection still uses real TLS even when bridge/API
  transport is plain loopback HTTP/WebSocket.
- Generate a local test CA and sign the actual CSR submitted by the SDK (for
  example using rcgen in dev-dependencies). An unrelated pre-generated leaf
  certificate does not match the SDK-generated private key. Return the same
  issued certificate on the immediate post-connect refresh GET unless a test
  deliberately simulates renewal.
- Add one bounded forwarding exchange to the fake bridge: send an `open`
  message, transport TLS bytes using the public connection-ID data frames, and
  make one TLS client HTTP request through the SDK to a loopback backend.
  Assert the backend response arrives through the tunnel. This tests real
  TLS termination, route lookup, target resolution, and bidirectional forwarding
  in normal CI without building a general relay.
- Prefer immediate deterministic errors; use 5xx responses only when testing
  retries because the SDK's external retry delays cannot be shortened.
- Exercise startup timeout, failed certificate issuance, invalid credentials,
  relay disconnect/reconnect, terminal failure, and recovery cleanup.
- Include a regression for signal cancellation without detaching the main task
  and a delayed certificate-refresh response during runtime shutdown/local
  cleanup. Verify the cleanup client is constructed on the new runtime, second
  signals remain effective, and state does not reappear after successful deletion.
- Test cancellation before pending credentials are saved and verify that the
  bounded progress-stage grace permits pending-identity deletion when possible.
- Launch the executable as a child process and send real SIGINT/SIGTERM.
  Include signals before readiness and while provisioning.
- SIGKILL tests: reusable state can be reopened after the lock is released;
  disposable state can be deleted by a subsequent cleanup invocation.
- Compare the test container's writable filesystem before and after runs, using
  scratch HOME/XDG/temp directories to detect unexpected persistent state.
  Do not add syscall tracing in v1. Document that a filesystem diff does not
  prove the absence of transient writes outside the state directory; the
  application-write boundary also depends on construction and code review.
- Verify no `systemctl`, OpenTunnel CLI, or background helper is executed.

### C. Optional manual end-to-end local server workflow

- Evaluate a pinned upstream server revision using its local issuer/test CA and
  disposable MySQL, following upstream's documented development setup.
- Keep fixtures and services bound to loopback; no hosted-service secrets.
- Run a real create -> certificate -> attach -> HTTPS request cycle, restart the
  reusable identity without new registration, then explicitly delete it.
- Separately verify the disposable create -> forward -> automatic delete cycle.
- Verify pending-identity cleanup and reconnection against real server behavior.
- Run this only through a separate manual workflow, not normal push/PR CI.
  Normal CI's fake bridge covers one real TLS/HTTP forwarding exchange; this
  optional workflow adds compatibility coverage against the actual upstream
  server, without making MySQL or server builds mandatory for every push.
- Avoid maintaining a full alternative server just to test this wrapper.

### D. Opt-in hosted smoke test

- Run manually, with explicit consent and a very small creation budget.
- Test one identity, one local backend, and one public request.
- Verify deletion; retain credentials for later cleanup if deletion fails.
- Never run on every push/PR or publish credentials as CI artifacts.

## GitHub repository and CI

### Repository setup

- Use the confirmed public repository `martin-pfister/portable-opentunnel`, MIT
  license, and the already renamed local directory.
- Add source, tests, README, LICENSE, Cargo files, and this plan.
- Include a short README Slurm batch-script example using `exec`, explaining
  termination grace time and temporary-storage recovery limitations. Slurm is
  not a dependency and requires no dedicated integration or tests.
- Document that persistent jobs started in the same working directory contend
  for one identity/lock; use separate `--directory` paths or ephemeral mode for
  independent simultaneous runs.
- Ignore credentials/run directories and `target/`.
  Review staged files for secrets before the first commit.
- Initialize Git on `main`, create the GitHub remote, and push only after
  authorization. Do not replace an existing remote/repository without approval.

### CI triggers and checks

- Run formatting, Clippy with warnings denied, and locked tests on pull requests
  and every push to `main`.
- Run a release compilation on every push to `main`, not only on version tags.
- Run client runtime/integration acceptance tests in the pinned AlmaLinux 8.8
  x86-64 container, including the actual release executable. GitHub's runner OS
  may provide build/orchestration tools; it is not another compatibility target.
- Build test executables with
  `cargo test --no-run --locked --target x86_64-unknown-linux-musl` and run those
  static executables inside the AlmaLinux container. Do not accidentally test
  only GNU/glibc test binaries on the host runner.
- Add a manual workflow trigger for diagnostics and supported integration tests.
- Use minimal workflow permissions, pinned action revisions, a pinned Rust
  toolchain, and dependency caches keyed by the lockfile/toolchain/target.
- Do not give untrusted PR code secrets or use `pull_request_target` to execute
  it. Normal builds require no hosted-service credentials.

### Binary portability and artifacts

- Primary artifact: a statically linked `x86_64-unknown-linux-musl` executable,
  following upstream OpenTunnel's Linux distribution approach. Keep v1 to one
  Linux x86-64 artifact, without a special glibc build environment or second libc
  variant unless testing demonstrates a concrete need.
- Install the musl target and C compiler (`musl-tools` on the Ubuntu build
  runner); the SDK's `ring` dependency includes native C/assembly.
- Use release optimization and strip debug information from distribution
  binaries. Measure size rather than predicting it from libc choice.
- Verify the final ELF has no required shared libraries or dynamic loader;
  do not infer fully static linkage from the target name alone.
- Test the built executable in AlmaLinux 8.8, not just on the GitHub runner.
  Document this tested environment without claiming universal Linux or native
  old-kernel compatibility.
- Document that the installed glibc version is irrelevant to a fully static
  musl executable, but kernel, CPU, DNS, and CA-store compatibility still apply.
- Support both bare ports and `hostname:port`. Keep automated target-resolution
  tests to bare ports, localhost, a controlled `/etc/hosts` name, and bracketed
  IPv6. Do not require external DNS or cluster-specific names in CI.
  Document that glibc NSS plugins are not loaded by static musl and that resolving
  a hostname with system `getent` and passing its IP is a possible workaround.
- Do not add a glibc artifact preemptively. Reconsider only if actual acceptance
  tests reveal a required hostname-resolution or other integration unsupported
  by musl.
- Treat execution in AlmaLinux 8.8 as an early feasibility gate. The SDK includes
  bundled Mozilla roots and adds OS trust, honoring SSL_CERT_FILE/SSL_CERT_DIR and
  OPENTUNNEL_CA_CERT. Exercise local test-CA trust without turning CA discovery
  into a separate portability project; never disable TLS verification.
- Run the compiled artifact's help/version commands and a local smoke test.
- Upload a versioned executable/archive, SHA-256 checksum, commit identifier,
  target, and actual byte size as GitHub Actions artifacts.
- Actions artifacts satisfy \"compile on push\" but expire. Decide separately
  whether to publish durable GitHub Release assets on version tags, and whether
  a mutable main/nightly release is wanted.
- Add ARM64/macOS only after their need and test coverage are agreed.

### Ongoing maintenance

- Automated dependency-update PRs; review SDK behavior changes before merging.
- Dependency/security and license checks with documented exceptions where needed.
- Release notes cover supported platforms and state-format compatibility.
- Preserve compatibility with retained run directories so a new binary can clean
  up credentials left by an older version.

## Suggested implementation order

1. Inspect the pinned SDK release, especially readiness, pending-identity
   deletion, cancellation, refresh-task quiescence, API overrides, and locking.
2. Prove the SDK can compile to the selected Linux target and complete a local
   lifecycle using explicitly located storage.
3. Implement CLI, private state, foreground supervision, signals, and cleanup.
4. Add recovery, deterministic failure tests, and secret/write-boundary checks.
5. Add CI builds/artifacts and documentation; run AlmaLinux 8.8 acceptance tests.
6. Create/publish the approved GitHub repository and verify a push to `main`
   produces a tested downloadable artifact.

## Definition of done

- One executable works without installing OpenTunnel, Rust, Node, Bun, or systemd.
- A fresh run provisions and forwards without a separate initialization command.
- Default mode reuses its saved identity and URL and retains them after exit.
- `--ephemeral` mode uses independent temporary credentials and attempts remote/local
  deletion on exit. Concurrent use of reusable state is prevented by a lock.
- The random route is constant for the saved identity; no custom-name option.
- `init` prepares the identity and route without forwarding; local `url`
  prints its saved URL without contacting or changing the hosted service.
- Readiness is reported only after attachment; normal forwarding stays foreground.
- Application-managed state stays within the selected private state directory,
  checked through construction, code review, and persistent-write tests (not
  claimed to be a syscall-level proof).
- SIGINT/SIGTERM initiate bounded cleanup; interrupted provisioning is covered.
- Forced termination and failed deletion leave documented, usable recovery state.
- No credential files are committed, logged, or uploaded in build artifacts.
- Normal offline/local tests cover lifecycle behavior, failure recovery, lock
  deletion races, and one actual TLS/HTTP forwarding exchange. The upstream
  local-server workflow is optional; hosted tests are opt-in only.
- AlmaLinux 8.8 acceptance tests pass; measured binary size and the container
  test's kernel limitation are documented.
- The approved GitHub repository's CI builds on every push to `main` and makes
  binaries/checksums available as artifacts.
