# Operating portable-opentunnel

Start with the [README](../README.md) for installation and common commands.
For command-line syntax and defaults, run `./portable-opentunnel --help`.

## Lifecycle and exit status

- **Forwarding** holds the writer lock, provisions or reuses an identity, and
  prints the URL once the bridge attaches. It runs until SIGINT, SIGTERM, or a
  terminal bridge error. It does not start or health-check the target service.
- **`init`** provisions or checks a reusable identity and prints its URL.
  It does not attach a bridge and cannot be combined with `--ephemeral`.
- **`url`** is offline and read-only. It checks for completed state without a
  writer lock; it does not check whether the bridge is running. It prints
  nothing if initialization has not completed or the state is marked for
  deletion. Corrupt or unreadable state is an error, not an absent identity.
- **`delete`** retires the registration and removes recognized local state.
  It refuses a live lock, corrupt state, or unrecognized directories; a
  missing directory is harmless.

`--startup-timeout` (default 600 seconds) sets **one** deadline across
provisioning and attachment. Startup failure can still be followed by
bounded cleanup. `--shutdown-timeout` (default 15 seconds) is shared by any
grace period for saving credentials, closing the tunnel, and deleting the
remote registration. Once remote deletion is confirmed, the program records
it locally even if no time remains to unlink files. Synchronous filesystem
operations are not preempted by either timeout.

| Code | Meaning |
| --- | --- |
| 0 | Success |
| 1 | Operational failure |
| 2 | Invalid or incompatible arguments |
| 3 | `url` found no initialized identity |
| 130 | SIGINT |
| 143 | SIGTERM |

Cleanup problems are reported on stderr, even when an earlier failure or
signal determines the status. A second signal forces immediate exit.

## State and recovery

Reusable forwarding, `init`, `url`, and `delete` require `--directory DIR`.
The argument names the exact directory. Ephemeral forwarding can omit it and
use a fresh directory under `$TMPDIR` (or the system temporary directory).
If you specify a path in ephemeral mode, it must not exist yet. Keep reusable
state out of version control and on a private filesystem with reliable `flock`
semantics, including across nodes if the directory is shared.

```text
<state>/
  .lock          exclusive writer lock
  wrapper.json   mode, API origin, route, and optional remote_deleted marker
  sdk/default/   token, private-key.pem, certificate.pem, chain.pem,
                 pending.json, tunnel.json
```

The wrapper stores the random route before provisioning; the SDK commits
`tunnel.json` last. New state roots use `0700`; credential files use `0600`.
Treat the whole directory as sensitive. These permissions do not control
swap, core dumps, or files and logs written by other programs.

- **To keep the URL:** restart forwarding with the same reusable directory.
  An interrupted provision can resume using saved pending credentials.
- **To retire the URL:** run `delete --directory DIR`. The API must confirm
  deletion or return 404 before local credentials are removed. If a timeout,
  transport error, or other API status prevents confirmation, the credentials
  stay in place. Retry with the same directory and CA settings. A 401/403
  does not confirm removal and retrying with the same rejected credentials
  may not help; contact the service operator if the rejection persists.
- **If deletion was marked but local cleanup stopped:** retry `delete`; it
  can finish without contacting the API. Forwarding and `init` refuse to
  reuse marked state. An interruption before the marker was committed may
  require another API request.
- **If the state is corrupt:** keep the credentials for manual recovery. The
  client does not silently replace the identity or regenerate its route.

Local cleanup removes recognized files and temporary residue by name. It
does not recursively delete directories or remove unrelated entries.
An unconfirmed create request may have registered a tunnel without returning
the token. No local retry can remove that registration without its credentials.
The same limitation applies if node-local storage disappears.

## TLS, proxy, and API settings

The control-plane connections (API and bridge WebSocket) verify TLS using
bundled Mozilla roots, the system store where available, and any extra PEM
certificates. Pass `--ca-cert /path/to/ca.pem` or set `OPENTUNNEL_CA_CERT`;
the flag wins. The PEM is checked before use. `url` ignores this setting
because it does not connect. The visitor-facing tunnel certificate is
separate from control-plane trust.

The SDK supports HTTP and HTTPS proxies via `HTTPS_PROXY` for HTTPS/WSS,
`HTTP_PROXY` for HTTP/WS, `ALL_PROXY` as a fallback, and `NO_PROXY` for
bypasses. Lowercase variants are also supported.

For a private OpenTunnel service, `--api` takes an HTTP(S) origin, such as
`https://tunnel.internal.example`. The origin is saved with the identity;
conflicting overrides are refused. The wrapper does not read `OPENTUNNEL_API`.

## Static binary limitations

The musl executable does not need target glibc, but still needs a compatible
Linux kernel on x86-64. It does not load glibc NSS plugins such as SSSD, so a
name that works in other programs may not resolve. Prefer an `/etc/hosts` or
DNS entry, or pass the address directly (`127.0.0.1:8080` or `[::1]:8080`,
for example). Only Linux x86-64 artifacts are provided.
