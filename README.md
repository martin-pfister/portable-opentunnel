# portable-opentunnel

[![CI](https://github.com/martin-pfister/portable-opentunnel/actions/workflows/ci.yml/badge.svg)](https://github.com/martin-pfister/portable-opentunnel/actions/workflows/ci.yml)

Expose one local service at a public HTTPS URL through [OpenTunnel](https://opentunnel.xyz).
The client runs in the foreground without root, systemd, or an installed
OpenTunnel CLI. It is designed for environments such as cluster batch jobs.

## Get the binary

Get the static Linux x86-64 binary from the
[latest release](https://github.com/martin-pfister/portable-opentunnel/releases/latest)
or [build from source](docs/ci.md#build-from-source).

```sh
url=https://github.com/martin-pfister/portable-opentunnel/releases/latest/download
archive=portable-opentunnel-linux-x86_64.tar.gz
curl -fLO "$url/$archive" && curl -fLO "$url/$archive.sha256" &&
  sha256sum -c "$archive.sha256" && tar -xzf "$archive"
```

## Quick start: temporary URL

Start your application first. For a service listening on `127.0.0.1:3000`:

```console
$ ./portable-opentunnel 3000 --ephemeral
https://k3f9x2m1q7zt.abcd1234.opentunnel.xyz
```

A bare port uses `127.0.0.1`; `host:port` and `[IPv6]:port` also work. The URL
appears on stdout once the tunnel attaches. This confirms the tunnel is
connected, not that your application is healthy. Diagnostics go to stderr.

**The URL is public.** Only expose services intended to be reachable from the
internet, and configure application authentication if needed.

`--ephemeral` creates a new identity in a private directory under `$TMPDIR`
(or `/tmp`). Press Ctrl+C to stop forwarding and attempt cleanup.

## Reuse the same URL

Use `--directory DIR` **instead of** `--ephemeral`. The identity and URL are
saved there. Run again with the same directory to get the same URL, even if
the local port changes.

```sh
./portable-opentunnel 3000 --directory ./my-tunnel
```

Give concurrent jobs separate directories. If you reuse a directory across
nodes, use shared storage with reliable cross-node locking.

`init`, `url`, and `delete` also require `--directory`:

```sh
./portable-opentunnel init   --directory ./my-tunnel  # create an identity, without forwarding
./portable-opentunnel url    --directory ./my-tunnel  # print the saved URL offline
./portable-opentunnel delete --directory ./my-tunnel  # retire the identity and remove its state
```

If cleanup fails, keep the state directory and retry `delete` with the path
shown on stderr. Supply your private CA again if you used one. See the
[recovery guide](docs/operations.md#state-and-recovery) for details.

## More information

- `./portable-opentunnel --help` for flags and exit codes.
- [Operations guide](docs/operations.md): timeouts, exit status, state, recovery,
  TLS, proxies, and static binary limitations.
- [Slurm example](docs/slurm.md): batch script and signal handling.
- [Build and CI](docs/ci.md): source build, checks, and artifact validation.
- [SDK upgrade checklist](docs/sdk-upgrades.md) for maintainers.

MIT licensed. See [LICENSE](LICENSE).
