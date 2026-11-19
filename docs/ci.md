# CI, container acceptance tests, and artifacts

This document describes what `.github/workflows/` is configured to run, which
container digest is pinned and why, how to reproduce the container run locally,
and what is deliberately **not** automated.

> **Read this as configuration plus local results.** The jobs described below
> are what CI is set up to do. At the time of writing the AlmaLinux acceptance
> run had not happened in CI yet, so no description of a job here should be read
> as evidence that it passed. The [Honest status](#honest-status) section
> separates what was actually executed locally from what remains unverified.

Nothing here contacts the hosted OpenTunnel service. No workflow requires
credentials, and no workflow produces tunnel identities.

## Triggers

| Workflow | File | Trigger |
| --- | --- | --- |
| CI | `.github/workflows/ci.yml` | every pull request, every push to `main`, manual dispatch |
| Diagnostics | `.github/workflows/diagnostics.yml` | manual dispatch only |

There is no `pull_request_target` anywhere. Pull request code never receives
secrets or a writable token. The workflow declares `contents: read`, and no job
declares its own `permissions:` block, so every job runs with exactly that.

Worth stating explicitly because it is easy to assume otherwise: a job-level
`permissions:` block in GitHub Actions **replaces** the top-level one rather than
being intersected with it. The guarantee here comes from the current
configuration having no job-level blocks, not from the top level being an
enforced ceiling. Any future job must not add a broader block.

## CI jobs

### `quality`

Runs on `ubuntu-24.04`, the orchestrator only.

1. `cargo fmt --all --check`
2. `cargo clippy --locked --all-targets --all-features -- -D warnings`
3. Confirms `Cargo.lock` is committed and `cargo metadata --locked` does not
   want to change it.
4. Fails if `rust-toolchain.toml`'s `channel` and the workflow's
   `RUST_TOOLCHAIN` disagree, so the pin cannot drift silently.

### `dependencies`

Runs `cargo deny --all-features --locked check` with `cargo-deny` 0.21.0, using
`deny.toml`. This covers licenses, RustSec advisories, bans, and sources. A new
dependency with an unlisted license or an advisory fails the build.

### `acceptance`

This job requires both `quality` and `dependencies` to pass. It is the only job
that can upload a release artifact, so a license or advisory failure can never
result in a published binary.

This job is intended to be what makes the artifact trustworthy, and it is the
one worth reading carefully. It runs on `ubuntu-24.04`, but **the runner's
userland is not an acceptance target**. The runner is configured to:

1. installs the pinned toolchain and the musl C toolchain (`musl-tools`), because
   `ring` builds C and assembly;
2. builds test executables for `x86_64-unknown-linux-musl` with
   `cargo test --no-run --locked --target x86_64-unknown-linux-musl`, capturing
   Cargo's JSON messages;
3. turns those messages into a manifest of **test** executables
   (`scripts/ci/test-manifest.py`), never naming tests by hand;
4. builds the release executable, strips it, and checks that it — and every test
   executable — has no dynamic loader and no `DT_NEEDED` entries
   (`scripts/ci/check-static-elf.sh`);
5. pulls the pinned image **by digest** and verifies the digest still matches
   `scripts/ci/container.lock` (`scripts/ci/verify-image.sh`);
6. runs the static executables **inside** the AlmaLinux container.

Inside the container, `scripts/ci/run-acceptance.sh`:

- refuses to proceed unless `/etc/os-release` reports AlmaLinux 8.8 on x86-64
  (`scripts/ci/verify-os-release.sh`) — the gate reads the file, it does not
  trust a tag or an environment variable;
- checks that loopback works, because the container runs with `--network none`
  and every fixture binds `127.0.0.1`. A dead `lo` fails here instead of
  producing a wall of bind errors;
- snapshots the filesystem outside the scratch tree, then diffs it after the
  tests (see [Filesystem boundary](#filesystem-boundary));
- runs each test executable with a fresh `HOME`, `TMPDIR`, `XDG_CACHE_HOME`, and
  `XDG_DATA_HOME`;
- exports `PORTABLE_OPENTUNNEL_TEST_BINARY` (the release executable) and
  `PORTABLE_OPENTUNNEL_TEST_BINARY_USED`, then requires — via
  `--require-release-exercised` — that at least one integration test actually
  launched that binary (see [Release binary exercise](#release-binary-exercise));
- checks the documented CLI contract against the release executable via
  `scripts/ci/check-cli-contract.sh`: `--help`/`--version` succeed with output,
  invalid targets and incompatible flags exit **exactly 2** (not merely
  non-zero), and `url` on a nonexistent directory exits 3, prints nothing on
  stdout, prints a diagnostic on stderr, and creates nothing.

Test executables are static musl binaries, so their libc is their own. Running
them in the container is what proves they *execute* on the target OS, rather than
only compiling for it.

### Unprivileged execution

The container run uses `--user "$(id -u):$(id -g)"`, so tests execute as the
host caller rather than root. This is not cosmetic. Docker's default root
**bypasses filesystem permission checks (DAC)**, so a test that makes a file
unreadable and expects `EACCES` would be silently satisfied as root and would
prove nothing about how the program behaves for the ordinary user it is built
for. Running unprivileged is what gives those assertions meaning.

Consequences, all already satisfied by the current setup:

- The mounted scratch tree is created by the calling user, so it is writable;
  the repository mount stays read-only and merely needs to be readable.
- The image needs no matching user entry, because the runner passes `HOME` and
  the `XDG_*` variables explicitly rather than relying on `passwd`.
- The image's `/etc/os-release` check still runs fine read-only.

### Controlled hostname

One integration test reaches a backend by **name** rather than by address, to
cover hostname resolution without external DNS. The container gets a controlled
`/etc/hosts` entry:

```
--add-host portable-opentunnel-fixture:127.0.0.1
```

and the name is handed to the tests as `PORTABLE_OPENTUNNEL_TEST_HOST`. The
runner defaults that variable to `localhost` when it is unset, so a plain local
`cargo test` uses a name that resolves without any DNS either. This is the
"controlled `/etc/hosts` name" case from the plan; no cluster-specific or
external DNS is involved anywhere in CI.

### Release binary exercise

`--help`, `--version`, and an argument error do not exercise a tunnel. The
integration tests do, and they must run the **shipped** binary, not a separate
dev build, so that the artifact under test is the artifact published.

The contract between the tests and CI:

- `PORTABLE_OPENTUNNEL_TEST_BINARY` — absolute path to the release executable.
  Tests use it when set, and fall back to `CARGO_BIN_EXE_portable-opentunnel`
  for a plain `cargo test` run.
- `PORTABLE_OPENTUNNEL_TEST_BINARY_USED` — a file path. Each test that launches
  the binary appends one line naming the scenario.

`--require-release-exercised` fails the job when that file is empty. That turns
"the release binary actually ran a lifecycle against the local fixture" from a
silent assumption into a condition that is checked and fails loudly. Its limit
is real: the marker is written by the test itself, not by the harness observing
a child process, so a test could append without spawning anything. It is paired
with normal test failures, which do surface, but it is a signal rather than
independent proof.

### Filesystem boundary

Before and after the tests, the runner records regular files (mode, size, mtime,
path) and directories (path) on the filesystem, and fails on any difference.

Scope, stated plainly because a pass depends on it:

- **Monitored:** everything except the paths listed below.
- **Excluded, and why:** `/proc`, `/sys`, `/dev` (pseudo-filesystems, not
  persistent storage); `/run`, `/var/log`, `/var/cache` (container and
  package-manager churn); and the scratch tree, which is where writes are
  expected and permitted.
- **`/tmp` and `/var/tmp` are monitored**, with only the scratch tree inside
  them excluded. They are the most likely place for a tool that ignores
  `TMPDIR` to leak, so excluding them wholesale would hide the very bug this
  check exists to catch.

Limits, restated every run in the output:

- It detects **persistent** changes only. A file created and removed during the
  run leaves no trace, so this is not a syscall-level proof of a write boundary.
- Directories the snapshot could not read are skipped, and their contents are
  unchecked. The count of unreadable paths is printed each run so that this
  blind spot is visible rather than silent.
- Directory mtimes are not compared, since creating a file legitimately bumps
  its parent's mtime.
- A snapshot that silently traversed almost nothing would produce a vacuous
  diff, so several known-populated paths must be present and the entry count
  must be plausible, or the run refuses to continue.

A pass therefore means "no unexpected persistent change was found in the
monitored paths", not "the program provably wrote nowhere else".

### Release artifact on `main`

Only on a push to `main`, after acceptance passes:

- `scripts/ci/package-artifact.sh` produces:
  - `portable-opentunnel-<version>-x86_64-unknown-linux-musl.tar.gz`,
  - a `.tar.gz.sha256` checksum,
  - a `.metadata.json` with version, target triple, commit SHA, run id/attempt,
    and the real byte size;
- `actions/upload-artifact` uploads them as `portable-opentunnel-<sha>` with
  `if-no-files-found: error`.

Packaging re-checks static linkage, so a dynamic binary can never be published
even if an earlier step were bypassed.

Artifacts expire (`retention-days: 30`). There are **no** GitHub Release assets
yet; durable release publishing on version tags is a separate decision.

## Pinned container

| | |
| --- | --- |
| Image | `docker.io/library/almalinux` |
| Tag | `8.8` (mutable; recorded for provenance only) |
| Platform manifest digest (used by CI) | `sha256:59fcdf8090ccdf3f69eae066bb40f71bbf38d724c025abad268d520568a487b9` |
| Multi-arch list digest (provenance) | `sha256:af06ad211fd7a53745bcd0d955dc3fa4d47a42caaf985f761327bdb759248a6a` |
| Compressed size (amd64) | 71,544,896 bytes |
| Queried | 2026-11-19 |

The full reference CI uses:

```
docker.io/library/almalinux@sha256:59fcdf8090ccdf3f69eae066bb40f71bbf38d724c025abad268d520568a487b9
```

Source of these values (registry API, not a third-party mirror):

- <https://hub.docker.com/v2/repositories/library/almalinux/tags/8.8>
- <https://hub.docker.com/v2/repositories/library/almalinux/tags?name=8.8>

The `8.8` tag is a multi-arch manifest list. Its amd64 entry carries the digest
above and is the same image as `8.8-20230718`, the last rebuild of the 8.8
series (pushed 2023-07-18). Because this project ships only x86-64, CI pins the
platform digest directly instead of resolving the list.

`scripts/ci/container.lock` is the single source of truth. `scripts/ci/container-ref.sh`
prints the reference; `scripts/ci/verify-image.sh` re-checks that a pulled image
still matches it and is `linux/amd64`.

Do not replace either digest with `8.8`, `8`, or `latest`. Those names are
mutable, and a silent image change would invalidate the acceptance result.

## Reproducing the container run locally

CI is authoritative, but you can run the same gates locally.

```sh
# docker, podman, or nerdctl on PATH
scripts/ci/run-ci-container.sh

# pick the runtime explicitly
scripts/ci/run-ci-container.sh --runtime podman

# leave the container behind for inspection
scripts/ci/run-ci-container.sh --keep
```

That script builds the static test executables and release binary, mounts the
repository read-only and a scratch directory writable, and runs
`scripts/ci/run-acceptance.sh` inside the pinned image with `--network none`.

If no container runtime is installed, the script stops and says the acceptance
tests did not run. It never pretends to have run them.

To check the image alone:

```sh
image=$(scripts/ci/container-ref.sh)
docker pull "$image"
scripts/ci/verify-image.sh --runtime docker "$image"
docker run --rm --network none "$image" bash -c 'grep -E "^(ID|VERSION_ID)=" /etc/os-release'
```

## Diagnostics workflow

`diagnostics.yml` is manual. It prints the toolchain, resolved dependency count,
`cargo tree --duplicates`, the pinned references, `container.lock`, and — when
requested — probes the container's `/etc/os-release` and loopback stack. It
uploads no artifacts and creates no identities. Use it when a pull request fails
and the reason is not obvious from the log.

## What is deliberately not automated

- **Hosted smoke test.** No CI job creates a real tunnel on the hosted service.
  Doing so on every push would be an unreliable, rate-limited, credential-bearing
  test. If ever needed, it must be a manual, explicitly consented run with a
  tiny creation budget — not a workflow trigger.
- **Local upstream server workflow (MySQL).** PLAN.md describes an optional
  manual workflow against a pinned upstream server with a disposable MySQL for
  extra compatibility coverage. It is **deferred**: it is not implemented, not
  triggered, and MySQL is not a CI dependency. Normal CI covers forwarding with
  local fixtures instead.
- **Native old-kernel / VM testing.** The AlmaLinux container proves that
  userland, not that kernel. Containers share the CI host kernel. A VM matrix is
  out of scope for v1.
- **Multi-arch artifacts.** x86-64 only. ARM64 and macOS would each need their
  own build and test coverage.

Each of these is a known gap, not an oversight; the README documents the
supported environment without claiming more than has been tested.

## Pinned actions

Every action is pinned to a commit SHA, with its tag in a comment for
reviewability:

| Action | Pin | Tag |
| --- | --- | --- |
| `actions/checkout` | `3d3c42e5aac5ba805825da76410c181273ba90b1` | `v7.0.1` |
| `actions/upload-artifact` | `cf430e030ddbb5b0abf93d22962f4752f3646cd9` | `v7.0.2` |
| `dtolnay/rust-toolchain` | `683b588c487a69d01f6c6ddf40f036ff682cc2c8` | `v1` |
| `Swatinem/rust-cache` | `d1527c8f67ef9af357290475d4af4b0fab8f12e0` | `v2.9.3` |
| `taiki-e/install-action` | `75a7c0f87bd666af3cdb75c5b9e9d03ab6044e6f` | `v2.90.6` |

Tool versions are pinned alongside: Rust `1.89.0` (from `rust-toolchain.toml`),
`cargo-deny` `0.21.0`, and the container digest above.

The cache key is derived from the lockfile, toolchain, and target by
`Swatinem/rust-cache`; `cache-on-failure: false` keeps a broken build out of the
cache.

## Scripts

| Script | Purpose |
| --- | --- |
| `scripts/ci/container.lock` | Pinned image identity and provenance |
| `scripts/ci/container-ref.sh` | Print the pinned reference (`--list` for the list digest) |
| `scripts/ci/verify-image.sh` | Check a pulled image's digest and platform against the lock |
| `scripts/ci/verify-os-release.sh` | Gate on `/etc/os-release` inside the container |
| `scripts/ci/test-manifest.py` | Turn `cargo test --no-run` JSON into a test manifest |
| `scripts/ci/check-static-elf.sh` | Prove an ELF has no loader and no `DT_NEEDED` |
| `scripts/ci/musl-env.sh` | Select `musl-gcc` as the linker/C compiler for musl builds |
| `scripts/ci/check-cli-contract.sh` | Check exit statuses and stream separation of the release binary |
| `scripts/ci/run-acceptance.sh` | Order the gates: OS, loopback, snapshot, tests, CLI, diff |
| `scripts/ci/package-artifact.sh` | Archive, checksum, and metadata for a release binary |
| `scripts/ci/run-ci-container.sh` | Reproduce the container job locally |
| `scripts/ci/local-checks.sh` | Format, clippy, tests, deny, and static build locally |

`test-manifest.py` keys on Cargo's `profile.test` flag, not on `target.kind`.
That distinction matters: Cargo reports a library's unit-test harness as
`kind: ["lib"]` and a binary's as `kind: ["bin"]`, so a kind-based filter would
silently skip every unit test. `profile.test` also excludes the plain binary,
which `cargo test --no-run` builds (proving it compiles) but which would start
forwarding a tunnel if executed. The release binary is exercised by the CLI
smoke checks instead.

## Honest status

At the time this document was written, the files above existed and the scripts
that can run without Rust or a container were checked locally:

- `test-manifest.py` — exercised against synthetic Cargo JSON: a lib unit-test
  harness is included, integration tests are included, the plain binary is
  excluded, duplicate names are disambiguated, wrong-target and empty-input runs
  fail, stdin works, and a diagnostic line containing a brace does not break
  parsing;
- `check-static-elf.sh` — passes a static binary, fails a dynamic one and a
  missing file, and reports each failure in a multi-file run;
- `verify-os-release.sh` — passes matching `/etc/os-release`, fails a wrong
  `VERSION_ID`, fails a missing file;
- `check-cli-contract.sh` — passes a binary that matches the documented contract,
  and fails one that returns 1 instead of 2 for bad arguments or that accepts an
  incompatible flag;
- `run-acceptance.sh` — a passing run passes; a failing test fails; an empty
  manifest fails; missing arguments are rejected; a binary that violates the CLI
  contract is caught; a test writing **outside** the scratch tree is detected by
  the filesystem diff (verified with a leak into `/tmp`, which is monitored); a
  scratch-only write is ignored; a snapshot that did not traverse the filesystem
  is refused rather than silently passing; `--require-release-exercised` fails
  when nothing launched the binary and passes when a test did;
- `package-artifact.sh` — produces archive, checksum, and metadata, and the
  checksum verifies; refuses a dynamically linked input;
- `verify-image.sh` — with a stubbed runtime: accepts a matching digest and
  `linux/amd64`, fails a mismatched digest, and its message tells the reader not
  to edit the lock file to follow a moving tag;
- `container-ref.sh` — prints the pinned references recorded in
  `container.lock`;
- `musl-env.sh` — sets and persists the linker variables when `musl-gcc` is
  present, and warns when it is not.

### Dependency check: passed, from pristine sources

`cargo deny --all-features --locked check` passes against the current
`Cargo.lock`:

```
advisories ok, bans ok, licenses ok, sources ok
exit 0
```

Run with `cargo-deny 0.21.0` built from crates.io into an empty `CARGO_HOME`,
with every crate archive unpacked unmodified and all state confined to a
throwaway directory. The only output was three `warning[duplicate]` notes
(`getrandom`, `syn`, `synstructure`), expected under
`[bans] multiple-versions = "warn"` because those duplicates come from the
pinned upstream SDK's tree.

This is the one gate with a real local result. It says the dependency graph is
acceptable; it says nothing about whether this crate builds or its tests pass.

To reproduce:

```sh
root=$(mktemp -d "$PWD/target/test-tmp/deny.XXXXXX")
CARGO_HOME=$root/cargo-home TMPDIR=$root/tmp CARGO_TARGET_DIR=$root/target \
  CARGO_BUILD_JOBS=1 \
  cargo install cargo-deny --locked --version 0.21.0 --root "$root/install"
CARGO_HOME=$root/cargo-home "$root/install/bin/cargo-deny" \
  --all-features --locked check
```

### Source build and own test suite

The crate builds, and the tests pass in a debug build. Locally:

```
library tests:   114 passed; 0 failed
lifecycle tests:  22 passed; 0 failed
fixture tests:     7 passed
```

That run used a `TMPDIR` inside the project, because this development sandbox
blocks `.pem` writes in `/tmp`; the tests create certificate fixtures, so a
`/tmp`-based `TMPDIR` fails with `EACCES` for reasons unrelated to the code. On
an ordinary machine the default `TMPDIR` works.

Two notes on coverage:

- The lifecycle tests pass, including the bracketed-IPv6 target test, which ran
  rather than skipping. It skips only when the kernel cannot bind `::1`, which
  is a limitation of the host rather than of the program.
- Permission tests are meaningful only for an unprivileged user. A test that
  makes a file unreadable and expects the read to fail proves nothing when run
  as root, because root ignores mode bits. The suite detects that case and skips
  with an explicit message instead of reporting a false pass, and CI runs the
  container unprivileged (see [Unprivileged execution](#unprivileged-execution))
  so these assertions actually execute.

A skip is therefore visible, never silent, and never a stand-in for a pass.

### Release executable

The static musl release build was verified directly:

| | |
| --- | --- |
| Size | 5,382,952 bytes |
| SHA-256 | `908b61f2104d8365fbce8db940221f8f881ec0c5353dde297cd351891287ac10` |
| Loader / shared libraries | none: no `PT_INTERP`, no `DT_NEEDED` |

Its offline CLI suite passes 20/20.

The lifecycle suite against the **release** executable passes 20 of 22. The
two remaining failures are `localhost` DNS lookups denied by this development sandbox,
not SDK or program behavior: a minimal C probe shows static musl
`getaddrinfo("localhost")` returning `EAI_SYSTEM`/`EACCES` here while glibc
succeeds, `/etc/hosts` in this environment has no `localhost` mapping, and a
mapped name resolves fine. The hostname assertions remain strict. The HTTPS
fixture uses its certificate's loopback-IP SAN to test CA trust independently
of DNS. CI supplies a real `localhost` mapping and the controlled hostname, so
these tests do not need DNS there; its acceptance result is still pending.

To reproduce:

```sh
cargo test --locked
```

### Not tested here

- **The AlmaLinux 8.8 container run.** No container runtime is available here,
  and the project's constraint is that CI is the only place it runs. Everything
  about the acceptance run is therefore configuration plus script-level tests,
  not an end-to-end result.
- **`run-ci-container.sh`** (needs a real runtime) and **the workflows
  themselves** (need a GitHub runner).

The first push to `main` or the first pull request is what actually executes the
build and container gates.
