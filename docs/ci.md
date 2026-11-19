# Build, CI, and releases

See the [CI runs](https://github.com/martin-pfister/portable-opentunnel/actions/workflows/ci.yml)
for current results and [GitHub Releases](https://github.com/martin-pfister/portable-opentunnel/releases)
for tested Linux x86-64 binaries.

## Build from source

Use the Rust toolchain pinned in [`rust-toolchain.toml`](../rust-toolchain.toml).
Install a musl C compiler (`musl-tools` on Debian/Ubuntu) for `ring` and
Binutils for the linkage check.

```bash
rustup target add x86_64-unknown-linux-musl
. scripts/ci/musl-env.sh
cargo build --release --locked --target x86_64-unknown-linux-musl
scripts/ci/check-static-elf.sh target/x86_64-unknown-linux-musl/release/portable-opentunnel
```

The linkage check verifies that the binary has no dynamic loader or
`DT_NEEDED` entries.

## Local checks

```sh
scripts/ci/local-checks.sh           # format, lint, host tests, dependency gate, static build
scripts/ci/local-checks.sh --no-musl # skip the static build
```

The tests use loopback fixtures, not the hosted API. Builds and dependency
checks may need downloads. Neither command runs the
[AlmaLinux acceptance tests](#reproducing-the-container-run-locally).
If a development sandbox denies writes of PEM fixtures to its default temp
directory, set `TMPDIR` to a writable directory that permits those files.

## Dependency policy

Install the same dependency checker as [CI](../.github/workflows/ci.yml):

```sh
cargo install cargo-deny --locked --version 0.21.0
cargo deny --all-features --locked check
```

[`deny.toml`](../deny.toml) enforces allowed licenses, RustSec advisories and
yanks, dependency bans, and crates.io-only sources. Dependencies must be
suitable for redistribution in a static binary. To inspect licenses manually
(not a substitute for the gate):

```sh
cargo metadata --locked --format-version 1 |
  jq -r '.packages[] | "\(.name) \(.version) \(.license // "UNKNOWN")"' |
  sort -u
```

Dependabot opens weekly Cargo and Actions update PRs without auto-merging.
Follow the [SDK upgrade checklist](sdk-upgrades.md) before changing the SDK pin.

## What CI checks

[CI](../.github/workflows/ci.yml) runs on pull requests, pushes to `main`,
and manual dispatches. Its jobs:

1. **Quality:** formatting, Clippy with warnings denied, the committed lockfile,
   and agreement between the Rust pins in the workflow and toolchain file.
2. **Dependencies:** the `cargo-deny` gate.
3. **Acceptance:** after the first two jobs pass, build static musl test
   executables and the release binary, verify their linkage, and run acceptance
   in the pinned AlmaLinux 8.8 x86-64 container.
4. **Release:** on pushes to `main` only, publish the exact accepted package.

Actions are pinned to commit SHAs. Build and test jobs inherit `contents: read`;
only publishing gets `contents: write`. It uses the built-in token and does not
check out or execute repository code. There is no `pull_request_target` trigger.

### AlmaLinux acceptance

[`container.lock`](../scripts/ci/container.lock) records the image provenance
and amd64 manifest digest. Keep it pinned to the platform digest, not a
mutable tag. [`run-acceptance.sh`](../scripts/ci/run-acceptance.sh) verifies the
OS release and loopback, runs tests with fresh `HOME`, `TMPDIR` and XDG
directories, and checks the release binary's CLI exit codes and streams.

Run as a non-root user so permission tests are meaningful. The container has
`--network none`; a controlled `/etc/hosts` name covers hostname resolution
without external DNS. This tests AlmaLinux **userland**, not the distribution's
native kernel. No CI job contacts the hosted OpenTunnel service.

### Release binary exercise

[`test-manifest.py`](../scripts/ci/test-manifest.py) selects test executables
from Cargo JSON using `profile.test`, not `target.kind`, so it includes unit
test harnesses but excludes the plain executable.

- `PORTABLE_OPENTUNNEL_TEST_BINARY` points integration tests to the release
  executable. When unset, tests use Cargo's `CARGO_BIN_EXE_portable-opentunnel`.
- When `PORTABLE_OPENTUNNEL_TEST_BINARY_USED` is set, a test that launches it
  appends one line to that file. Acceptance fails if the file stays empty.
  This guards against testing only the development binary; it is not an
  independent trace of child processes.
- `PORTABLE_OPENTUNNEL_TEST_HOST` supplies the controlled hostname. For a
  normal local test run it defaults to `localhost`.

### Filesystem boundary

Acceptance compares before-and-after snapshots of regular files (mode, size,
mtime, path) and directories (path only). It includes `/tmp` and `/var/tmp`
but excludes the writable scratch tree, `/proc`, `/sys`, `/dev`, `/run`,
`/var/log`, and `/var/cache`. The snapshots must contain known system paths and
a plausible number of entries. They do not catch transient writes or changes
in unreadable directories; the runner reports such omissions.

## Reproducing the container run locally

With Docker, Podman, or nerdctl available, run:

```sh
scripts/ci/run-ci-container.sh
```

Use `--runtime podman` (or `docker` or `nerdctl`) to select the runtime.
The script builds the binaries, mounts the checkout read-only with a separate
writable scratch directory, and runs acceptance inside the pinned image.

## Automatic releases on `main`

On pushes to `main`, acceptance packages the tested binary as
`portable-opentunnel-linux-x86_64.tar.gz`, alongside
`portable-opentunnel-linux-x86_64.tar.gz.sha256`. These two public asset names
stay the same across releases; the build number is in the release tag.

The publishing job downloads the accepted package by artifact ID and verifies
the checksum and internal `portable-opentunnel-linux-x86_64.metadata.json`.
It tags the tested commit as `build-<number>` and publishes after uploading
the archive and checksum. The Actions artifact, including the metadata,
expires after 30 days; the release assets do not share that retention limit.

Build numbers come from `github.run_number`, so failed and non-release runs
can leave gaps. The Cargo version stays unchanged. Reruns can finish a draft
but do not replace a published release. An older build finishing late does not
replace a newer numbered release as Latest.
