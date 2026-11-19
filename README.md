# portable-opentunnel

Expose **one** local service through [OpenTunnel](https://opentunnel.xyz) from a
single foreground process.

It exists for environments where the normal OpenTunnel CLI is not an option:
no systemd, no root, no installed OpenTunnel binary, and no background daemon.
The intended use is a batch job on a cluster, including Slurm, where a compute
node runs your service and this wrapper publishes it over HTTPS for the lifetime
of the job.

```
$ ./portable-opentunnel 3000 --directory /shared/private/my-tunnel
https://k3f9x2m1q7zt.opentunnel.xyz
```

The URL is printed on stdout **after** the tunnel has attached, so a script can
capture it and start using it. Everything else goes to stderr.

- Identity, key, and certificate live in one directory you choose, not in the
  network service and not in the OpenTunnel CLI's own profile area.
- The random subdomain is generated **once per identity** and saved, so the
  public HTTPS URL is stable across restarts even if the local port changes.
- `--ephemeral` creates a throwaway identity and deletes it on exit.
- No systemd unit, no background helper, no root, no `systemctl`, no OpenTunnel
  CLI on the machine. This is a property of the implementation — the program
  spawns no child processes and speaks to the API directly — not something CI
  proves by scanning a binary for strings.

## Contents

- [Build](#build)
- [Usage](#usage)
- [Commands](#commands)
- [CLI reference](#cli-reference)
- [Exit status](#exit-status)
- [State directory](#state-directory)
- [Concurrency and locking](#concurrency-and-locking)
- [Certificate trust and `--ca-cert`](#certificate-trust-and---ca-cert)
- [Proxy support](#proxy-support)
- [Security notes](#security-notes)
- [Recovery](#recovery)
- [Slurm example](#slurm-example)
- [Static musl build limitations](#static-musl-build-limitations)
- [Tests and checks](#tests-and-checks)
- [Dependency license and security checks](#dependency-license-and-security-checks)
- [Platform support](#platform-support)
- [License](#license)

## Build

Prerequisites: Rust **1.89.0** (pinned in `rust-toolchain.toml`) and, for the
static artifact, the `x86_64-unknown-linux-musl` target plus a musl C compiler.
`ring` — pulled in by the SDK — contains C and assembly, so a C compiler is
needed even though the final executable is fully static.

```sh
rustup target add x86_64-unknown-linux-musl
sudo apt-get install -y musl-tools      # Debian/Ubuntu; adjust for your distro
cargo build --release --locked --target x86_64-unknown-linux-musl
```

The resulting executable is at
`target/x86_64-unknown-linux-musl/release/portable-opentunnel`.

Do not assume the target *name* proves full static linkage. Verify it:

```sh
readelf -lW target/x86_64-unknown-linux-musl/release/portable-opentunnel | grep INTERP   # no output
readelf -dW target/x86_64-unknown-linux-musl/release/portable-opentunnel | grep NEEDED   # no output
```

`scripts/ci/check-static-elf.sh <paths...>` performs exactly these checks and
is what CI runs on both the release binary and every test executable.

## Usage

```sh
# Forward the service on 127.0.0.1:3000 using the default state directory
portable-opentunnel 3000

# Forward a different local address, keeping state somewhere explicit
portable-opentunnel 127.0.0.1:8080 --directory /shared/private/my-tunnel

# Create a throwaway identity, forwarding a service, deleted on exit
portable-opentunnel 3000 --ephemeral

# Throwaway identity in a directory you name (must not exist yet)
portable-opentunnel 3000 --ephemeral --directory ./disposable-tunnel

# Prepare the identity and URL without forwarding anything
portable-opentunnel init --directory ./my-tunnel

# Print the saved URL. Offline, read-only, no network access.
portable-opentunnel url --directory ./my-tunnel

# Delete a saved identity and its state
portable-opentunnel delete --directory ./my-tunnel
```

`portable-opentunnel --help` and `--version` are the authoritative reference for
the exact flags.

## Commands

### Forwarding (the default)

`portable-opentunnel <TARGET>` opens the state directory, acquires an exclusive
lock, ensures the identity exists (provisioning it on first run), connects, waits
for the tunnel to attach, prints the URL on stdout, and then forwards until it
receives `SIGINT`/`SIGTERM` or the tunnel stops with a terminal error.

- It is **foreground only**. It never daemonizes.
- It does not start the program you are forwarding to. Start that yourself.
- The URL is printed only after attachment. A printed URL means the bridge is
  connected, not that your application is healthy.
- `--startup-timeout` bounds provisioning and attachment **separately**, so a
  run that spends the full budget on each can take about twice that long before
  failing. Certificate issuance may legitimately take minutes.
- After a normal exit the identity, certificate, and route are **kept**, so the
  next run reuses the same URL.

### `init`

Creates or ensures the identity and the fixed random route, then prints the URL.
It never attaches a forwarding bridge and never starts a background process. It
is idempotent and retains state.

`init` proves that the local identity exists and that the hosted control plane
issued a certificate. It is **not** a claim that forwarding currently works —
nothing is attached. Use a forwarding invocation when you need a live tunnel.

`init` cannot be combined with `--ephemeral` or with a target.

Like forwarding, `init` refuses a directory whose deletion is pending (see
[`delete`](#delete)). Creating a new identity there would put a different
identity behind a route the user believes was removed.

### `url`

Prints the saved full HTTPS URL and nothing else. Strictly local:

- no network access at all,
- no provisioning, renewing, connecting, or deleting,
- never creates a missing directory,
- takes no writer lock.

It reads only the saved route and the SDK's `tunnel.json` hostname. The SDK
writes `tunnel.json` last by atomic rename, so its presence is the completion
marker.

With no complete identity and route, it prints
`No tunnel initialized in <directory>.` to stderr, leaves stdout empty, and
exits **3**. For malformed, corrupt, or unreadable state it exits **1** instead
of pretending nothing is there — corrupt credentials are not "absent".

A directory whose deletion is pending is treated as no longer initialized: the
registration is gone, so the saved URL would be a stale address. `url` exits
**3** and the diagnostic names the recovery command that finishes the local
cleanup. It still writes nothing.

`url` makes no connectivity claim: the URL may belong to a stopped tunnel.

### `delete`

Deletes the saved registration on the server, then removes the local state. It
refuses to act on state that is live (locked by a running instance), not owned
by this program, or outside the documented layout.

The order is deliberate. A fresh client asks the API to delete the registration;
local files are touched only after the server confirms the deletion or confirms
the registration is already gone (404). A timeout or an unknown remote outcome
keeps every credential and prints the exact retry command.

Once the remote result is confirmed, `delete` first records it durably in
`wrapper.json` and only then unlinks anything. That record is a single optional
field, `remote_deleted`, which is omitted entirely while it is false so a live
identity's metadata stays byte-identical.

This makes an interrupted deletion resumable. If the process is killed partway
through removing local files — or the machine fails — the flag is already on
disk, so running `delete` again finishes the local cleanup without contacting
the API. It could not contact it anyway: the credentials the request needs may
already be partly unlinked.

If writing that record fails, nothing is unlinked and the credentials stay
intact. Refusing to proceed is the point: the flag is the only thing that makes
a partial deletion recoverable.

Local cleanup itself is this program's own, name by name: recognized SDK files
and atomic-write residue, then `wrapper.json`, then the lock, then a
non-recursive `remove_dir` of the state root. It does not call the SDK's profile
removal, because that removes the profile directory recursively and would also
delete files this program does not recognize. Anything unknown is preserved and
keeps the directory alive, and the tool says so.

`delete` cannot be combined with `--ephemeral`; name the directory instead.
Deleting a directory that does not exist is not an error: it reports that there
was nothing to delete and exits 0.

## CLI reference

| Flag | Default | Meaning |
| --- | --- | --- |
| `[TARGET]` | required for forwarding | `3000`, `host:port`, or `[IPv6]:port` |
| `-d, --directory DIR` | `$PWD/.portable-opentunnel` | The **exact** state directory, not a parent |
| `--ephemeral` | off | Use a new private temporary directory and delete the identity on exit |
| `--ca-cert PEM` | unset | Extra CA certificate(s) to trust for the API and bridge |
| `--startup-timeout SECONDS` | `600` | Bound on provisioning, and again on attachment |
| `--shutdown-timeout SECONDS` | `15` | One shared budget for close, remote deletion, and local cleanup |
| `--verbose` | off | More diagnostics on stderr. Credentials are never printed |
| `--api URL` | `https://opentunnel.xyz` | Hidden, for local tests. `http`/`https` only |

Targets:

| Input | Resolves to |
| --- | --- |
| `3000` | `127.0.0.1:3000` |
| `127.0.0.1:8080` | as given |
| `localhost:8080` | as given |
| `my_host:51234` | as given |
| `[::1]:8080` | bracketed IPv6, as given |

Ports must be 1–65535. A bare port always means loopback — never a wildcard
bind, and never `0.0.0.0`.

The API endpoint is recorded in the state directory when the identity is
created. A later `--api` that conflicts with the recorded endpoint is rejected
rather than silently ignored, so a saved identity cannot be pointed at a
different service. `portable-opentunnel` reads **no** API endpoint environment
variable; the upstream CLI's `OPENTUNNEL_API` is not consulted.

## Exit status

| Code | Meaning |
| --- | --- |
| 0 | Success |
| 1 | Operational failure: startup, terminal tunnel error, corrupt state, cleanup failure |
| 2 | Invalid arguments (argument-parser convention), including incompatible flags |
| 3 | `url` found no initialized identity |
| 130 | Terminated by `SIGINT` |
| 143 | Terminated by `SIGTERM` |

Signals decide the status even when shutdown was otherwise clean. Cleanup errors
are reported on stderr even when an earlier error or a signal determined the
exit code — the process does not hide a failed cleanup behind a signal status.

A **second** signal forces immediate exit. That escape hatch deliberately leaves
recoverable state behind rather than pretending cleanup finished.

## State directory

One directory holds everything for one identity:

```text
<state>/
  .lock                          exclusive wrapper lock (0600)
  wrapper.json                   format, owner, mode, API endpoint, route,
                                 and an optional remote_deleted flag
  wrapper.json.tmp-<pid>-<rand>  residue of an interrupted metadata write
  sdk/                           storage root handed to the SDK
    default/                     the one fixed profile
      token
      private-key.pem
      certificate.pem
      chain.pem
      pending.json
      tunnel.json                written last; completion marker
      <stem>.tmp-<pid>           residue of an interrupted SDK atomic write
```

- Directories are created `0700`; secret files `0600`. Permissions on an
  existing directory are checked, never loosened.
- `wrapper.json` is written **before** any SDK provisioning, so an interruption
  cannot leave a provisioned identity whose random route is unknown.
- `remote_deleted` is present only after the server confirmed the registration is
  gone. While false it is omitted from the file, so an identity in normal use has
  the same metadata as before this flag existed. See [`delete`](#delete).
- `tunnel.json` is written last by the SDK, so `url` can rely on it.
- Recognized temporary names are treated as owned residue and cleaned up while
  holding the lock. Unknown files are never removed: they are preserved,
  reported, and keep the directory from being removed.

The state directory is the only place this program writes its own state. It says
nothing about swap, core dumps, your shell history, or files written by other
tools — see [Security notes](#security-notes).

## Concurrency and locking

Every modifying command (`forward`, `init`, `delete`) holds an exclusive lock on
`<state>/.lock` for the entire operation. A second instance on the same
directory is rejected with a clear message.

- The lock is validated by device/inode comparison against the path, so a
  process holding an unlinked old lock inode cannot proceed alongside a process
  holding a new one.
- Ordinary exits release the lock descriptor but leave the lock file in place.
  Only final state deletion removes it, as the last file, while still holding it.
- `url` takes no writer lock: it never modifies state.

Practical consequences:

- Several jobs started in the **same working directory** contend for the same
  default directory. Give each its own `--directory`, or use `--ephemeral`.
- Independent `--ephemeral` runs are safe in parallel: each gets a new directory.
- An explicit `--ephemeral --directory` must **not** exist. Not even an empty
  directory is accepted, so persistent state can never be silently turned into
  disposable state.

Reliable `flock`-style locking is required. Some shared-filesystem mount options
do **not** provide cross-node exclusion, and this program cannot detect every
such configuration for you. If you rely on the lock across nodes, use a
filesystem whose locks you trust.

## Certificate trust and `--ca-cert`

TLS verification is **never** disabled. The client trusts:

1. the bundled Mozilla root store compiled into the binary,
2. the operating system's certificate store, which honors `SSL_CERT_FILE` and
   `SSL_CERT_DIR` on the systems where the native certificate loader is used,
3. a PEM file given by `--ca-cert`, or by `OPENTUNNEL_CA_CERT` when the flag is
   absent.

`--ca-cert` takes precedence over `OPENTUNNEL_CA_CERT`. A path given with
`--ca-cert` is validated up front: a missing file or a directory fails
immediately instead of falling back to public roots and failing later with a
confusing certificate error.

Only the **control plane** (API and bridge WebSocket) uses this trust
configuration. Your forwarded traffic is terminated locally with the tunnel's own
certificate; the TLS between a visitor and the tunnel is a separate matter.

For a private or on-premises API with an internal CA:

```sh
portable-opentunnel 3000 --api https://tunnel.internal.example --ca-cert /etc/ssl/internal-ca.pem
```

## Proxy support

The SDK honors the same proxy environment variables for the API and for the
bridge WebSocket, so both connections take the same route:

- `HTTPS_PROXY` / `https_proxy` for `https://`/`wss://`
- `HTTP_PROXY` / `http_proxy` for `http://`/`ws://`
- `ALL_PROXY` / `all_proxy` as a fallback
- `NO_PROXY` / `no_proxy` to bypass

Proxies must be `http://` or `https://`. A proxy that answers `407` produces a
clear error. Proxy URLs may contain credentials; those values are never printed.

The wrapper reads no API endpoint variable and does not read proxy variables
itself — it leaves them to the SDK, whose behavior this README describes.

## Security notes

What this program does:

- Keeps credentials in the state directory with `0600` files in `0700`
  directories.
- Never puts tokens in argv, logs, or committed files. `--verbose` adds
  diagnostics only.
- Never disables TLS verification.
- Refuses symlinked state directories, symlinked lock files, and non-regular
  files inside a state directory.
- Removes files only inside the validated owned layout, one known name at a
  time. It never calls a recursive directory removal, so unrelated files are
  preserved and keep the directory alive instead of being deleted.
- Prints the recovery command instead of deleting when remote deletion is
  uncertain.

What it deliberately does **not** claim:

- It controls writes **it** makes. It does not stop the OS from swapping pages,
  writing a core dump, or logging to an external system (for example a Slurm
  log). It cannot constrain files written by other tools.
- Remote deletion is best-effort. The registration is soft-deleted on the
  server; it does not erase a server database row or reverse certificate work.
- A lost response during identity creation can leave a registration whose token
  this client never received. That case cannot be cleaned up by any local
  process.
- A forced second signal, `SIGKILL`, or a machine failure can defeat cleanup.
  That is exactly why state is left recoverable and `delete --directory` exists.

The hosted service is not contacted by any test or CI job in this repository
unless you deliberately run an opt-in workflow.

## Recovery

Nothing here is automatic. If a run is interrupted, the state directory is left
in a state you can inspect and clean up.

| Situation | What to do |
| --- | --- |
| Forwarding stopped normally | Nothing. The identity is kept; run again for the same URL |
| Killed with `SIGKILL`, or node failed | Run `portable-opentunnel delete --directory <dir>` |
| `delete` interrupted after the server confirmed deletion | Run `delete --directory <dir>` again. It finishes the local cleanup without contacting the API; `init` and forwarding refuse this state until you do |
| `--ephemeral` exited but remote deletion was uncertain | The error names the directory. Run the printed `delete --directory` command |
| `url` says no tunnel initialized | Either it was never initialized, or provisioning did not finish, or a deletion is pending. Run `init` or a forwarding invocation, or finish a pending `delete` |
| Saved credentials rejected | The error says the identity will not be replaced. Run `delete`, then start again for a new identity and URL |

An interrupted first run — only owned temporary files and the lock — is
recovered automatically by the next forwarding or `init` invocation, which
discards that incomplete residue after taking the lock. `url` leaves such
residue untouched and reports "not initialized".

Corrupt state (SDK files with no wrapper metadata, malformed metadata, or an
identity whose saved route is missing) is **never** silently repaired. It exits
1 and preserves the credentials, because generating a replacement identity would
silently change your URL.

## Slurm example

This is the only supported Slurm integration: a batch script that runs the
service and the wrapper in the foreground. There is no Slurm plugin, and Slurm
is not a dependency of this program.

```bash
#!/usr/bin/env bash
#SBATCH --job-name=my-service
#SBATCH --time=08:00:00
#SBATCH --nodes=1
#SBATCH --ntasks=1

set -euo pipefail

# Per-node scratch for ephemeral state. On many clusters /tmp is node-local,
# which is what an ephemeral run wants. Set it explicitly so it is obvious.
export TMPDIR="${TMPDIR:-/tmp}"

# Start the service you want to publish, in the background, and keep its PID.
python3 ./serve.py --port 3000 &
service_pid=$!

cleanup() {
  # Stop the service; the wrapper handles its own deletion on SIGTERM.
  kill -TERM "$service_pid" 2>/dev/null || true
}
trap cleanup EXIT

# `exec` replaces this shell with the wrapper so Slurm's SIGTERM reaches it
# directly instead of a shell that might ignore or mis-forward the signal.
# The URL appears on stdout once the tunnel has attached.
exec ./portable-opentunnel 3000 --ephemeral
```

Notes that matter in practice:

- **Use `exec`.** Without it, Slurm signals a shell that may not forward them,
  so the wrapper never starts bounded cleanup.
- **Termination grace.** Set `--shutdown-timeout` (default 15s) below whatever
  grace period your site gives between `SIGTERM` and `SIGKILL`. An ephemeral
  run needs that time to delete its registration before local state is removed.
- **Temporary storage is node-local.** If you use `TMPDIR` for ephemeral state
  and the node dies, the local credentials are gone. The registration may remain
  on the server; you cannot delete it without the token. Prefer a durable
  `--directory` on shared storage when cleanup after node failure matters.
- **One identity per directory.** Jobs started in the same working directory
  share the default state directory and will contend for the lock. Use separate
  `--directory` values, or `--ephemeral`.
- **`--ephemeral --directory` must not exist**, so do not point it at a
  directory a previous job left behind; clean it up or name a fresh path.

## Static musl build limitations

The release artifact is a statically linked `x86_64-unknown-linux-musl`
executable. That removes a dependency on the target machine's glibc, which is
usually the point on an old cluster image. It does not make the binary
universally portable.

**glibc version on the target is irrelevant.** A fully static musl executable
does not load the target's glibc at all. What still matters:

- **Kernel**: syscalls the binary uses must exist in the target's kernel.
- **CPU**: the binary targets baseline x86-64; it does not require newer
  instructions, but the machine must run x86-64.
- **Hostname resolution**: a static musl binary does **not** load glibc NSS
  plugins. `nsswitch.conf`-driven sources such as LDAP, SSSD, or `myhostname`
  will not be consulted. musl resolves through `/etc/hosts` and DNS as
  configured, and nothing more. If a short cluster hostname does not resolve
  under musl, resolve it yourself and pass the address:

  ```sh
  ip=$(getent hosts my-service.internal | awk '{print $1; exit}')
  ./portable-opentunnel "$ip:8080"
  ```

- **CA store**: trust comes from the bundled Mozilla roots, the OS store where
  available, and `--ca-cert`/`OPENTUNNEL_CA_CERT`. A container or minimal image
  with no CA bundle still trusts the bundled roots and any CA you pass
  explicitly.

If your target genuinely needs glibc-only name resolution, that is a reason to
reconsider the artifact, not a reason to disable TLS verification.

## Tests and checks

Local, no network:

```sh
cargo fmt --all --check
cargo clippy --all-targets --all-features --locked -- -D warnings
cargo test --locked
cargo deny --all-features check          # dependency licenses, advisories, bans, sources
```

Static build and linkage check:

```sh
cargo build --release --locked --target x86_64-unknown-linux-musl
scripts/ci/check-static-elf.sh target/x86_64-unknown-linux-musl/release/portable-opentunnel
```

The current release build measures 5,382,952 bytes with no dynamic loader and
no `DT_NEEDED` entries. Size is measured, not predicted, and will drift as
dependencies change.

`scripts/ci/local-checks.sh` runs the sequence above in one go. It does **not**
run the AlmaLinux acceptance tests; it says so when it finishes.

Integration tests that launch the built program should honor the CI contract
rather than a hardcoded path:

- use `PORTABLE_OPENTUNNEL_TEST_BINARY` when it is set, otherwise
  `CARGO_BIN_EXE_portable-opentunnel`;
- append one line to the file named by `PORTABLE_OPENTUNNEL_TEST_BINARY_USED`
  each time they launch it.

CI sets both variables and fails the acceptance job unless at least one test
reported a launch, so the artifact under test is the release binary and it is
exercised through a real lifecycle, not just `--help`.

The full gate, including the pinned-container acceptance run, is
`.github/workflows/ci.yml`; see [docs/ci.md](docs/ci.md) for what each job does,
which container digest is pinned, and how to reproduce it locally.

## Dependency license and security checks

The policy lives in `deny.toml` and is enforced by
[`cargo-deny`](https://github.com/EmbarkStudios/cargo-deny) 0.21.0, both in CI
and locally:

```sh
cargo install cargo-deny --locked --version 0.21.0
cargo deny --all-features check
```

That single command covers four checks:

- **licenses** — every crate's license must be in the allowlist in `deny.toml`;
- **advisories** — the RustSec advisory database, with `yanked = "deny"`;
- **bans** — no OpenSSL/native-tls TLS stacks, no wildcard versions;
- **sources** — crates.io only, no git dependencies.

If `cargo-deny` is unavailable, the equivalent manual review is:

```sh
# What is actually in the tree, and under which licenses
cargo metadata --locked --format-version 1 \
  | jq -r '.packages[] | "\(.name) \(.version) \(.license // "UNKNOWN")"' \
  | sort -u

# Anything that looks like a copyleft or unrecognized license
cargo metadata --locked --format-version 1 \
  | jq -r '.packages[] | select((.license // "") | test("GPL|AGPL|SSPL|UNKNOWN|^$")) | "\(.name) \(.version) \(.license // "none")"'

# Advisories, if cargo-audit is available
cargo audit
```

These are review aids, not a gate: they print what is present and require a
human to judge it. The enforced check is `cargo deny check`.

That check passes against the current `Cargo.lock` with `cargo-deny 0.21.0`:

```
advisories ok, bans ok, licenses ok, sources ok
```

The only output is three `warning[duplicate]` notes (`getrandom`, `syn`,
`synstructure`) from the pinned upstream SDK's tree; `deny.toml` sets
`multiple-versions = "warn"` deliberately, because those duplicates are not this
project's to deduplicate. Reproduction commands and the list of what has and has
not been tested are in [docs/ci.md](docs/ci.md#honest-status).

The project itself is MIT. Every dependency must be compatible with
redistribution in a static binary; anything unexpected should be added to
`deny.toml` deliberately, with a reason, not by widening the allowlist quietly.

## Platform support

- The only artifact this project builds and ships is **Linux x86-64**,
  statically linked musl.
- The declared acceptance target is **AlmaLinux 8.8 (x86-64)**: CI is configured
  to run the tests inside a digest-pinned container of that image. That covers
  that userland, not that kernel, because containers share the host's kernel, so
  native old-kernel behavior is not covered.
- macOS, ARM64, and glibc artifacts are not provided. They are separate pieces
  of work with their own test requirements.

This is the intended and configured support, not a blanket certification. The
first CI run executed the full 163-test suite inside the pinned AlmaLinux 8.8
container, unprivileged, against the actual static release executable, and all
passed; the release build is 5,382,952 bytes with no dynamic loader or shared
libraries. That run's pipeline still failed on a harness tool gap, so no artifact
has been published yet. [docs/ci.md](docs/ci.md#honest-status) records exactly
what has and has not been executed.

## License

MIT. See [LICENSE](LICENSE).

The implementation plan and design rationale are in [PLAN.md](PLAN.md).
