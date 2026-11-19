# portable-opentunnel

[![CI](https://github.com/martin-pfister/portable-opentunnel/actions/workflows/ci.yml/badge.svg)](https://github.com/martin-pfister/portable-opentunnel/actions/workflows/ci.yml)

Expose one local service at a public HTTPS URL through [OpenTunnel](https://opentunnel.xyz).
The client runs in the foreground without root, systemd, or an installed
OpenTunnel CLI. It is designed for environments such as cluster batch jobs.

## Get the binary

From the [latest GitHub Release](https://github.com/martin-pfister/portable-opentunnel/releases/latest),
download the Linux x86-64 `.tar.gz` and its `.tar.gz.sha256` checksum into the
same directory. Every successful push to `main` gets an automated release
named `build-N`. Verify and extract the download:

```sh
sha256sum -c portable-opentunnel-*-x86_64-unknown-linux-musl.tar.gz.sha256 &&
  tar -xzf portable-opentunnel-*-x86_64-unknown-linux-musl.tar.gz
```

The artifact is a static Linux x86-64 binary. CI tests it in AlmaLinux 8.8
userland, not on the original AlmaLinux kernel. To build from source, see the
[build instructions](docs/ci.md#build-from-source).

## Quick start

Start your application first. For a service listening on `127.0.0.1:3000`:

```console
$ ./portable-opentunnel 3000 --directory ./my-tunnel
https://k3f9x2m1q7zt.abcd1234.opentunnel.xyz
```

A bare port uses `127.0.0.1`; `host:port` and `[IPv6]:port` also work. The URL
is printed on stdout once the tunnel attaches. That tells you the tunnel is
connected, not that your application is healthy. Diagnostics go to stderr.
Press Ctrl+C to stop forwarding.
For a throwaway URL, use `./portable-opentunnel 3000 --ephemeral`.

**The URL is public.** Only expose services intended to be reachable from the
internet, and configure application authentication if needed.

## Keep or discard the URL

| Mode | State directory | On exit |
| --- | --- | --- |
| Reusable | Requires a private `--directory DIR` you choose | Keeps the identity and URL, even if the local target changes next time |
| `--ephemeral` | Fresh private directory under `$TMPDIR` (or `/tmp`); cannot be combined with `--directory` | Tries to delete the registration and local state |

Give concurrent jobs separate directories. Only one process can modify a given
state directory at a time, and shared storage must provide reliable cross-node
locking.

`init`, `url`, and `delete` also require `--directory`:

```sh
./portable-opentunnel init   --directory ./my-tunnel  # create an identity, without forwarding
./portable-opentunnel url    --directory ./my-tunnel  # print the saved URL offline
./portable-opentunnel delete --directory ./my-tunnel  # retire the identity and remove its state
```

Use `./portable-opentunnel --help` for flags and exit codes. The startup timeout
is shared by provisioning and attachment.

## Recover after interruption

- **Keep a reusable URL:** restart forwarding with the same state directory.
- **Retire an identity or finish cleanup:** stop any running instance, then run
  `delete` with the saved directory. If deletion could not be confirmed, keep
  the directory and retry. Supply your private CA again if you used one.
- **Plan for node failure:** Ctrl+C and SIGTERM allow cleanup; SIGKILL and node
  failure do not. If node-local storage is lost, you cannot retry deletion
  without the saved credentials. Point `TMPDIR` at durable storage if needed.

## More information

- [Operations guide](docs/operations.md): timeouts, exit status, state, recovery,
  TLS, proxies, and static binary limitations.
- [Slurm example](docs/slurm.md): batch script and signal handling.
- [Build and CI](docs/ci.md): source build, checks, and artifact validation.
- [SDK upgrade checklist](docs/sdk-upgrades.md) for maintainers.

MIT licensed. See [LICENSE](LICENSE).
