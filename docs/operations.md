# Operating portable-opentunnel

Start with the [README](../README.md) for installation and common commands.
Run `./portable-opentunnel --help` for all flags and their defaults.

## Commands

- **`ephemeral TARGET`** forwards with a new URL and tries to delete it on exit.
- **`up TARGET --dir DIR`** forwards with the URL saved in `DIR`, creating it
  on first use.
- **`init --dir DIR`** creates the saved URL if needed and prints it, without
  forwarding.
- **`url --dir DIR`** prints the saved URL. It works offline and does not check
  whether a tunnel is running. It exits with status 3 if there is no finished
  URL, for example after an interrupted setup or deletion.
- **`delete --dir DIR`** deletes the URL at the service, then removes the local
  state. It refuses a directory that another process is using or that does not
  look like tunnel state. A missing directory counts as already deleted.

`up` and `ephemeral` print the URL on stdout once the tunnel is connected, then
forward until SIGINT, SIGTERM, or a fatal tunnel error. They neither start your
service nor check that it is healthy. Only one process can use a state
directory at a time.

## Timeouts

- `--startup-timeout` (default 600 seconds) is a single deadline for creating
  the URL and connecting, not a separate limit for each step.
- `--shutdown-timeout` (default 15 seconds) is a single deadline for everything
  after a stop: letting an interrupted setup save its credentials, closing the
  tunnel, and, for `ephemeral` and `delete`, deleting the URL at the service.

Local file operations are not interrupted by either timeout.

## Exit status

| Code | Meaning |
| --- | --- |
| 0 | Success |
| 1 | Operational failure |
| 2 | Invalid arguments |
| 3 | `url` found no saved URL |
| 130 | Stopped by SIGINT |
| 143 | Stopped by SIGTERM |

Cleanup problems are reported on stderr even when an earlier failure or signal
determines the status. A second signal exits immediately, skipping cleanup.

## State directory

`--dir DIR` is used exactly as given. `ephemeral` creates a new private
directory under `$TMPDIR` (or `/tmp`); `--verbose` prints its path.

The directory holds the credentials that control the URL. It is created with
mode `0700` and the credential files with `0600`. Keep it out of version
control. If several nodes share it, the filesystem must support `flock` across
nodes.

## Recovery

- **To keep the URL,** run `up` or `init` again with the same directory. An
  interrupted setup resumes where it stopped.
- **To delete the URL,** run `delete --dir DIR`. The local credentials are
  removed only after the service confirms the deletion or reports the URL as
  already gone. Otherwise they are kept, and stderr shows the exact `delete`
  command to retry with, including `--ca-cert` if an extra CA was in use. A 401
  or 403 response means the service rejected the credentials, so retrying is
  unlikely to help; contact the service operator if it persists.
- **If the deletion was recorded but local cleanup stopped,** run the printed
  `delete` command again. It finishes without contacting the service. `up` and
  `init` refuse such a directory.
- **If the state is damaged,** commands stop with an error instead of replacing
  it, so the credentials stay available for manual recovery.

`delete` removes only the files it manages. Anything else you put in the
directory stays, and so does the directory.

A URL cannot be deleted locally if its credentials are gone. This happens when
the service's reply to the create request is lost, or when the state directory
itself is lost, for example on node-local storage.

## TLS, proxies, and private services

Connections to the OpenTunnel service always verify TLS against the bundled
Mozilla roots, the system store (including `SSL_CERT_FILE` and
`SSL_CERT_DIR`), and an optional extra PEM file from `--ca-cert` or
`OPENTUNNEL_CA_CERT`; the flag wins. The file is checked at startup, so a wrong
path fails immediately rather than as a TLS error later. None of this affects
the certificate visitors see on the public URL.

Proxies are read from `HTTPS_PROXY`, `HTTP_PROXY`, `ALL_PROXY`, and
`NO_PROXY`, in upper or lower case.

For a private OpenTunnel service, pass its origin with `--api`, such as
`--api https://tunnel.internal.example`. The origin is saved in the state
directory, so later commands can omit `--api`; a different origin is refused.
`OPENTUNNEL_API` is not read.

## Static binary limitations

The binary is statically linked for Linux x86-64, so it does not need glibc,
but it also cannot use glibc name-service plugins such as SSSD. A hostname that resolves in other
programs may therefore fail here. Use a name from `/etc/hosts` or DNS, or give
the address directly, such as `127.0.0.1:8080` or `[::1]:8080`.
