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
and manual dispatches. Its first three jobs run in parallel:

1. **Quality:** formatting, Clippy with warnings denied, and the committed
   lockfile.
2. **Dependencies:** the `cargo-deny` gate.
3. **Acceptance:** build static musl test executables and the release binary,
   verify their linkage, and run them in the pinned AlmaLinux 8.8 x86-64
   container.
4. **Release:** on pushes to `main` only, once all three pass, publish the
   exact accepted package.

Every job installs the toolchain named in `rust-toolchain.toml`, the only
place the Rust version is pinned. Actions are pinned to commit SHAs. Build and
test jobs inherit `contents: read`; only publishing gets `contents: write`. It
uses the built-in token and does not check out or execute repository code.
There is no `pull_request_target` trigger.

### AlmaLinux acceptance

The binary is static, so the distribution matters little: it reads
`/etc/hosts`, the DNS configuration, and the OS CA store, whose location
differs between distributions. AlmaLinux stands in for the RHEL-family systems
it mostly runs on. The container shares the runner's kernel.

[`container.lock`](../scripts/ci/container.lock) pins the image by its amd64
manifest digest, never a mutable tag.
[`run-ci-container.sh`](../scripts/ci/run-ci-container.sh) runs it:

- as a non-root user, so permission tests are meaningful;
- with `--network none` and a controlled `/etc/hosts` name, so no test reaches
  the internet or external DNS, and no CI job contacts the hosted service;
- with a read-only root and checkout, so a write anywhere but the scratch
  directory, including `/tmp`, fails the test that made it.

[`run-acceptance.sh`](../scripts/ci/run-acceptance.sh) gives each test
executable fresh `HOME`, `TMPDIR` and XDG directories, then checks the release
binary's CLI exit codes and streams.

Integration tests run against the release executable, not a debug build.
These variables control that:

- `PORTABLE_OPENTUNNEL_TEST_BINARY` selects the executable under test. When
  unset, tests use Cargo's `CARGO_BIN_EXE_portable-opentunnel`.
- `PORTABLE_OPENTUNNEL_TEST_BINARY_USED` names a file that each test launching
  that executable appends to. Acceptance fails if it stays empty.
- `PORTABLE_OPENTUNNEL_TEST_HOST` supplies the controlled hostname; it defaults
  to `localhost`.

## Reproducing the container run locally

With Docker, Podman, or nerdctl, plus musl-tools and `jq`, run:

```sh
scripts/ci/run-ci-container.sh
```

Use `--runtime podman` (or `docker` or `nerdctl`) to select the runtime. The
script builds the binaries with
[`build-static.sh`](../scripts/ci/build-static.sh) and runs acceptance inside
the pinned image exactly as CI does.

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
can leave gaps. The published binary reports its release tag in `--version`
(for example, `portable-opentunnel build-42`); local builds report `dev`.
There is no Cargo package version to maintain. Reruns can finish a draft but
do not replace a published release. An older build finishing late does not
replace a newer numbered release as Latest.
