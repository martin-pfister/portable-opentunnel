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
scripts/ci/local-checks.sh           # format, lint, dependency gate, static build and its tests
scripts/ci/local-checks.sh --no-musl # host tests instead of the static build
```

The first runs the same [tests as CI](#how-the-tests-run). Builds and
dependency checks may need downloads. If a development sandbox denies writes
of PEM fixtures to its default temp directory, set `TMPDIR` to a writable
directory that permits those files.

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
3. **Tests:** build the static release binary, verify its linkage, and run
   [the tests](#how-the-tests-run) against it.
4. **Release:** on pushes to `main` only, once all three pass, publish the
   exact tested package.

Every job installs the toolchain named in `rust-toolchain.toml`, the only
place the Rust version is pinned. Actions are pinned to commit SHAs. Build and
test jobs inherit `contents: read`; only publishing gets `contents: write`. It
uses the built-in token and does not check out or execute repository code.
There is no `pull_request_target` trigger.

## How the tests run

[`test-static.sh`](../scripts/ci/test-static.sh) is the whole test step, in
CI and locally. It builds the static release binary, checks its linkage, runs
the test suite compiled for musl against that binary, and checks the CLI exit
codes and streams. It needs musl-tools and Binutils, but no container.

The integration tests start the binary with a cleared environment and private
`HOME`, `TMPDIR` and XDG directories. They talk only to loopback fixtures:
every other host is routed to a proxy that refuses connections, so a test that
forgets `--api` fails instead of registering a tunnel with the hosted service.

Two variables control the tests:

- `PORTABLE_OPENTUNNEL_TEST_BINARY` is the absolute path of the binary under
  test. When unset, tests use the debug build Cargo made for the test run.
- `PORTABLE_OPENTUNNEL_TEST_HOST` names a host mapped to `127.0.0.1` in
  `/etc/hosts`, to test a target given by a hostname other than `localhost`.
  CI sets it; when unset, that test is skipped.

## Automatic releases on `main`

On pushes to `main`, the test job packages the tested binary as
`portable-opentunnel-linux-x86_64.tar.gz`, alongside
`portable-opentunnel-linux-x86_64.tar.gz.sha256`. These two public asset names
stay the same across releases; the build number is in the release tag.

The publishing job downloads the tested package by artifact ID and verifies
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
